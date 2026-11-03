// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Validate stored Task identity and commit accepted client input.

use super::DefaultRequestHandler;
use super::delivery::{
    AuthorizedMessage, Delivery, is_artifact_only, record_client_input, validate_existing_task,
};
use super::events::{ActiveExecution, InvocationState};
use super::protocol::Rejection;
use super::updates::save_task;
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

impl PreparedInput {
    pub(super) async fn commit(
        &self,
        handler: &DefaultRequestHandler,
        params: &crate::middleware::ServiceParams,
        request: &SendMessageRequest,
        lazy: bool,
        invocation: &mut InvocationState,
    ) -> Result<(), A2AError> {
        if lazy && !self.existed {
            invocation.pending_push =
                handler.request_push_config(params, &self.task.id, request)?;
        } else {
            handler
                .save_request_push_config(params, &self.task.id, request)
                .await?;
        }
        // Configuration must be accepted before input becomes committed.
        if self.existed {
            handler.task_store.update(self.task.clone()).await?;
        } else if !lazy {
            handler.task_store.create(self.task.clone()).await?;
        }
        Ok(())
    }
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
    let stored = handler.task_store.get(task_id).await?;
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

pub(super) async fn inject(
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
    let Some(base) = active.snapshot().await else {
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
                    // Closing and unregistering are one coordinator transition.
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
    let task = save_task(handler.task_store.as_ref(), task).await?;

    let sequence_before = active.sequence().await;
    let receiver = active.subscribe().await;
    let last = events.len().saturating_sub(1);
    for (index, event) in events.iter().enumerate() {
        let snapshot = (index == last).then(|| task.clone());
        active.publish_marked(Ok(event), snapshot, false).await;
    }
    if events.is_empty() {
        active.set_snapshot(task.clone()).await;
    }
    let sequence_after = active.sequence().await;
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
    handler.changes.notify(task_id);

    Ok(Delivery::Injected {
        task,
        receiver,
        sequence_before,
        sequence_after,
    })
}
