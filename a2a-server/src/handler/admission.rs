// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Validate stored Task identity and commit accepted client input.

use super::DefaultRequestHandler;
use super::delivery::{AuthorizedMessage, Delivery};
use super::protocol::{
    ProtocolError, RecordedInput, Rejection, is_artifact_only, record_client_input,
    validate_existing_task,
};
use super::{events::ActiveExecution, execution::InvocationState};
use crate::{ClientInput, executor::QueuedInput};
use a2a::*;
use std::sync::Arc;
use tokio::sync::mpsc;

pub(super) async fn prepare(
    handler: &DefaultRequestHandler,
    req: &SendMessageRequest,
    task_id: &str,
) -> Result<(Task, bool, Vec<StreamResponse>), A2AError> {
    // A2A §3.4.2: a client-provided taskId MUST reference an existing task,
    // and client-provided ids for *creating* tasks are not supported. Only
    // an absent taskId may mint a new one.
    let stored = handler.tasks.get(task_id).await?;
    super::protocol::validate_task_reference(stored.as_ref(), req, task_id)?;
    let context_id = stored
        .as_ref()
        .map(|task| task.context_id.clone())
        .unwrap_or_else(|| {
            req.message
                .as_ref()
                .and_then(|message| message.context_id.clone())
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

    Ok((task, existed, events))
}

pub(super) async fn inject_input(
    handler: &DefaultRequestHandler,
    message: &AuthorizedMessage,
    task_id: &str,
    active: &Arc<ActiveExecution>,
    invocation: &mut InvocationState,
) -> Result<Delivery, Rejection> {
    // Secure delivery capacity before recording anything in the store.
    let (task, permit) = validate_and_reserve_input(active, &message.request, task_id)?;

    // Configuration must be accepted before the input is committed.
    let (task, recorded) = persist_input(handler, message, task_id, task).await?;

    // Publish the complete batch before handing the message to user code.
    let delivery = publish_input(
        active,
        task,
        &recorded.events,
        recorded.message_generation,
        message.response_id,
    )
    .await;
    deliver_input(permit, recorded.message_generation, message, invocation);
    Ok(delivery)
}

/// The reader has claimed this offer. Commit and publish the real input before
/// releasing it to executor code; a failed write releases the reader as well.
pub(super) async fn commit_claimed(
    handler: &DefaultRequestHandler,
    message: &AuthorizedMessage,
    task_id: &str,
    active: &Arc<ActiveExecution>,
    offer: crate::executor::InputOffer,
    permit: InputPermit,
    invocation: &mut InvocationState,
) -> Result<Delivery, Rejection> {
    let task = active
        .snapshot()
        .ok_or_else(|| A2AError::internal("a running execution has no task"))?;
    validate_existing_task(&task, &message.request)?;
    let (task, recorded) = persist_input(handler, message, task_id, task).await?;
    let input = recorded
        .message_generation
        .and_then(|generation| {
            ClientInput::received(
                &message.request,
                generation,
                message.user.clone(),
                &message.params,
                message.response_id,
            )
        })
        .ok_or_else(|| A2AError::internal("claimed input has no message generation"))?;
    // Persistence is not consumption. Track the normal unread receipt before
    // publication so EOF can reject even a reader lost during this write.
    let input = invocation.track_input(input);
    let delivery = publish_input(
        active,
        task,
        &recorded.events,
        recorded.message_generation,
        message.response_id,
    )
    .await;
    permit.send(input);
    offer.complete();
    Ok(delivery)
}

pub(super) type InputPermit = mpsc::OwnedPermit<QueuedInput>;

pub(super) fn validate_and_reserve_input(
    active: &Arc<ActiveExecution>,
    request: &SendMessageRequest,
    task_id: &str,
) -> Result<(Task, Option<InputPermit>), Rejection> {
    if active
        .stream_ended
        .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(Rejection::ExecutionEnded);
    }
    if active.inputs.is_revoked() {
        if !active.inputs.closed.is_cancelled() {
            return Err(Rejection::input_busy()); // Last output has not committed yet.
        }
        return Err(Rejection::Failed(A2AError::internal(
            "the input queue of a registered execution is closed",
        )));
    }
    if active.uncreated.load(std::sync::atomic::Ordering::SeqCst) {
        return Err(ProtocolError::TaskNotFound(task_id).into());
    }
    let task = active
        .snapshot()
        .ok_or_else(|| A2AError::internal("a running execution has no task"))?;
    validate_existing_task(&task, request)?;
    if is_artifact_only(request) {
        return Ok((task, None));
    }
    let permit =
        active
            .inputs
            .sender
            .clone()
            .try_reserve_owned()
            .map_err(|refusal| match refusal {
                mpsc::error::TrySendError::Full(_) => Rejection::input_busy(),
                // Closing and unregistering are serialized in the same task queue.
                mpsc::error::TrySendError::Closed(_) => Rejection::Failed(A2AError::internal(
                    "the input queue of a registered execution is closed",
                )),
            })?;
    Ok((task, Some(permit)))
}

async fn persist_input(
    handler: &DefaultRequestHandler,
    message: &AuthorizedMessage,
    task_id: &str,
    mut task: Task,
) -> Result<(Task, RecordedInput), A2AError> {
    let recorded = record_client_input(&mut task, &message.request, false)?;
    handler
        .save_request_push_config(&message.params, task_id, &message.request)
        .await?;
    let task = handler.tasks.save(task, false).await?;
    if let Some(receipt) = &message.receipt {
        receipt.committed(&task);
    }
    Ok((task, recorded))
}

async fn publish_input(
    active: &Arc<ActiveExecution>,
    task: Task,
    events: &[StreamResponse],
    input_generation: Option<i64>,
    response_id: Option<uuid::Uuid>,
) -> Delivery {
    let sequence_before = active.sequence();
    // Bind before publication/delivery: the caller may not poll again before
    // execution finishes and rejects its input.
    let receiver = active.subscribe_for_response(response_id);
    active.publish_input(task.clone(), events).await;
    Delivery::Injected {
        task,
        receiver,
        sequence_before,
        sequence_after: active.sequence(),
        input_generation,
    }
}

fn deliver_input(
    permit: Option<InputPermit>,
    message_generation: Option<i64>,
    message: &AuthorizedMessage,
    invocation: &mut InvocationState,
) {
    let Some(permit) = permit else {
        return;
    };
    if let Some(input) = message_generation.and_then(|generation| {
        ClientInput::received(
            &message.request,
            generation,
            message.user.clone(),
            &message.params,
            message.response_id,
        )
    }) {
        permit.send(invocation.track_input(input));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;
    use crate::handler::{
        delivery::AuthorizedMessage, events::ActiveExecution, protocol::Rejection,
    };

    #[tokio::test]
    async fn a_late_offer_rejects_ended_revoked_or_lost_readers_before_writing() {
        let handler = make_handler();
        seed_task(&handler, "task", "context").await;
        let stored = handler.tasks.load("task").await.unwrap();
        let mut request = send_request();
        request.message.as_mut().unwrap().task_id = Some("task".into());
        for lost in 0..3 {
            let (active, inputs, invocation) =
                ActiveExecution::with_task(handler.tasks.task("task"));
            active.set_snapshot(stored.clone());
            match lost {
                0 => active.mark_stream_ended(),
                1 => active.inputs.revoke_reads(),
                _ => {}
            }
            drop(invocation);
            if lost == 2 {
                drop(inputs);
            }
            let rejection = validate_and_reserve_input(&active, &request, "task")
                .expect_err("a late offer cannot enter an unavailable inbox");
            let StreamResponse::Error(error) =
                rejection.into_stream_error("task", Some("input".into()))
            else {
                panic!("input rejection must be a stream error");
            };
            assert_eq!(
                error.code,
                if lost == 2 {
                    "INTERNAL_ERROR"
                } else {
                    "UNAVAILABLE"
                }
            );
            assert_eq!(error.reference_id.as_deref(), Some("input"));
            if lost == 0 {
                let unary = validate_and_reserve_input(&active, &request, "task")
                    .err()
                    .unwrap()
                    .into_error();
                assert_eq!(unary.code, error_code::INVALID_REQUEST);
            }
            assert_eq!(handler.tasks.load("task").await.unwrap(), stored);
        }
        handler.shutdown().await;
    }

    #[tokio::test]
    async fn a_missing_snapshot_or_closed_inbox_rejects_input_before_persistence() {
        let handler = make_handler();
        seed_task(&handler, "task", "context").await;
        let before = handler.tasks.load("task").await.unwrap();
        let (active, inputs, mut invocation) =
            ActiveExecution::with_task(handler.tasks.task("task"));
        let mut inputs = Some(inputs);
        let mut request = send_request();
        request.message.as_mut().unwrap().task_id = Some("task".into());
        let message = AuthorizedMessage {
            receipt: None,
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
