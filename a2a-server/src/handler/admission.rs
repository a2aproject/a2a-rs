// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Validate stored Task identity and commit accepted client input.

use super::DefaultRequestHandler;
use super::delivery::{
    AuthorizedMessage, Delivery, is_artifact_only, record_client_input, validate_existing_task,
};
use super::{events::ActiveExecution, execution::InvocationState, protocol::Rejection};
use crate::ClientInput;
use a2a::*;
use std::sync::Arc;
use tokio::sync::mpsc;

/// The prepared input snapshot and the events it adds. Context identity
/// comes from the Task rather than a second, independently carried value.
pub(super) struct PreparedInput {
    pub(super) task: Task,
    pub(super) existed: bool,
    pub(super) events: Vec<StreamResponse>,
}

pub(super) async fn prepare(
    handler: &DefaultRequestHandler,
    req: &SendMessageRequest,
    task_id: &str,
) -> Result<PreparedInput, A2AError> {
    let artifact_only = is_artifact_only(req);
    // A2A §3.4.2: a client-provided taskId MUST reference an existing task,
    // and client-provided ids for *creating* tasks are not supported. Only
    // an absent taskId may mint a new one.
    let stored = handler.tasks.get(task_id).await?;
    if stored.is_none() && (req.message.task_id.is_some() || artifact_only) {
        return Err(A2AError::task_not_found(task_id));
    }
    if let Some(task) = &stored {
        validate_existing_task(task, req)?;
    }
    let context_id = stored
        .as_ref()
        .map(|task| task.context_id.clone())
        .unwrap_or_else(|| {
            req.message
                .context_id
                .clone()
                .unwrap_or_else(new_context_id)
        });

    let existed = stored.is_some();
    let events;
    let task = if let Some(mut existing) = stored {
        events = record_client_input(&mut existing, req, false)?.events;
        existing
    } else {
        let mut task = Task {
            generation: 0,
            timeline: None,
            id: task_id.to_string(),
            context_id: context_id.clone(),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: Some(chrono::Utc::now()),
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        events = record_client_input(&mut task, req, true)?.events;
        task
    };

    Ok(PreparedInput {
        task,
        existed,
        events,
    })
}

pub(super) async fn inject_input(
    handler: &DefaultRequestHandler,
    message: &AuthorizedMessage,
    task_id: &str,
    active: &Arc<ActiveExecution>,
    invocation: &mut InvocationState,
) -> Result<Delivery, Rejection> {
    let (params, request) = (&message.params, &message.request);
    // A task the executor has not created yet is not one a client can know.
    if active.uncreated.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(Rejection::Failed(A2AError::task_not_found(task_id)));
    }
    let Some(base) = active.snapshot() else {
        return Err(Rejection::Failed(A2AError::internal(
            "a running execution has no task",
        )));
    };
    validate_existing_task(&base, request)?;
    let artifact_only = is_artifact_only(request);
    // Room for the message is secured before anything is changed, so a
    // message the executor cannot take is declined with nothing recorded.
    let permit =
        if artifact_only {
            None
        } else {
            Some(active.inputs.sender.clone().try_reserve_owned().map_err(
                |refusal| match refusal {
                    mpsc::error::TrySendError::Full(_) => {
                        Rejection::Declined("the agent is busy with earlier input".to_string())
                    }
                    // Closing and unregistering are one task operation.
                    mpsc::error::TrySendError::Closed(_) => Rejection::Failed(A2AError::internal(
                        "the input queue of a registered execution is closed",
                    )),
                },
            )?)
        };

    let mut task = base;
    let recorded = record_client_input(&mut task, request, false)?;
    let events = recorded.events;
    // Admit configuration before committing input: a rejected configuration
    // must not leave a recorded message that no executor will receive.
    handler
        .save_request_push_config(params, task_id, request)
        .await?;
    let task = handler.tasks.save(task, false).await?;

    let sequence_before = active.sequence();
    let receiver = active.subscribe();
    active.publish_input(task.clone(), &events);
    let sequence_after = active.sequence();
    if let Some(permit) = permit {
        let input = ClientInput::received(
            request,
            recorded
                .message_generation
                .expect("a reserved input has a message"),
            message.user.clone(),
            params,
            message.response_id,
        );
        permit.send(invocation.track_input(input));
    }

    Ok(Delivery::Injected {
        task,
        receiver,
        sequence_before,
        sequence_after,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;
    use crate::handler::{
        delivery::AuthorizedMessage, events::ActiveExecution, protocol::Rejection,
    };

    #[tokio::test]
    async fn a_missing_snapshot_or_closed_inbox_rejects_input_before_persistence() {
        let handler = make_handler();
        seed_task(&handler, "task", "context").await;
        let before = handler.tasks.load("task").await.unwrap();
        let (active, inputs, mut invocation) =
            ActiveExecution::with_task(handler.tasks.task("task"));
        let mut inputs = Some(inputs);
        let mut request = send_request();
        request.message.task_id = Some("task".into());
        let message = AuthorizedMessage {
            request,
            params: ServiceParams::new(),
            user: None,
            response_id: None,
        };
        for missing_snapshot in [true, false] {
            if !missing_snapshot {
                active.set_snapshot(before.clone());
                assert!(inputs.as_mut().unwrap().close_and_drain().is_empty());
                drop(inputs.take());
            }
            let result = inject_input(&handler, &message, "task", &active, &mut invocation).await;
            let Err(Rejection::Failed(error)) = result else {
                panic!("an unavailable inbox must reject the input");
            };
            assert_eq!(error.code, error_code::INTERNAL_ERROR);
            assert!(error.message.contains(if missing_snapshot {
                "has no task"
            } else {
                "is closed"
            }));
            assert_eq!(handler.tasks.load("task").await.unwrap(), before);
        }
    }
}
