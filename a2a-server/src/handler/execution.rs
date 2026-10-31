// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Drive executor output, persist updates, and handle execution failures.

use super::events::{ActiveExecution, EventKind};
use super::protocol::is_terminal_event;
use super::registry::{ExecutionManager, ExecutionRuntime};
use super::turns::{
    end_turn, reject_unread_inputs, reject_unread_inputs_locked, release_execution,
    release_execution_locked, save_pending_push_config,
};
use super::updates::{
    apply_and_publish, apply_and_publish_with, backfill_status_timestamp, validate_event_identity,
};
use a2a::*;
use futures::StreamExt;
use std::sync::Arc;

/// The output stream has been dropped before these reach the runner. Input
/// receipts let the server recover unread messages from an owned inbox.
enum TurnEnd {
    Finished,
    Terminal(Box<StreamResponse>),
    Failed(A2AError),
    Stopped,
}

async fn drive_execution(
    runtime: ExecutionRuntime,
    task_id: TaskId,
    active: Arc<ActiveExecution>,
    exec_ctx: crate::ExecutorContext,
) {
    match poll_executor(&runtime, &task_id, &active, exec_ctx).await {
        TurnEnd::Finished => {
            end_turn(&runtime, &active, &task_id).await;
        }
        TurnEnd::Terminal(mut event) => {
            let _writer = active.writer.lock().await;
            reject_unread_inputs_locked(&active, &task_id).await;
            if !active.snapshot_is_terminal().await {
                if let Err(error) =
                    apply_executor_event_locked(&runtime, &active, &task_id, &mut event).await
                {
                    active.publish(Err(&error), None).await;
                }
            }
            runtime.execution_manager.finish(&task_id, &active).await;
        }
        TurnEnd::Failed(error) => {
            fail_execution(&runtime.execution_manager, &active, &task_id, &error).await;
        }
        TurnEnd::Stopped => {
            release_execution(&runtime.execution_manager, &active, &task_id).await;
        }
    }
    runtime.changes.notify(&task_id);
}

/// Cancellation drops the owned output stream; input receipts can be reclaimed
/// even when an executor moved its receiver into a separate worker task.
async fn poll_executor(
    runtime: &ExecutionRuntime,
    task_id: &TaskId,
    active: &Arc<ActiveExecution>,
    exec_ctx: crate::ExecutorContext,
) -> TurnEnd {
    let mut stream = runtime.executor.execute(exec_ctx);
    let mut push_failure = active.push_failure.subscribe();
    loop {
        // Cancellation and terminal state take precedence over a late delivery error.
        if active.inputs.closed.is_cancelled() {
            return TurnEnd::Stopped;
        }
        let failure = push_failure.borrow_and_update().clone();
        if let Some(error) = failure {
            if active.snapshot_is_terminal().await {
                return TurnEnd::Stopped;
            }
            return TurnEnd::Failed(error);
        }
        let result = tokio::select! {
            biased;
            _ = active.inputs.closed.cancelled() => return TurnEnd::Stopped,
            _ = push_failure.changed() => continue,
            result = stream.next() => result,
        };
        let Some(result) = result else {
            return TurnEnd::Finished;
        };
        if active.snapshot_is_terminal().await {
            return TurnEnd::Stopped;
        }
        match result {
            Ok(event)
                if active.uncreated.load(std::sync::atomic::Ordering::SeqCst)
                    && matches!(event, StreamResponse::Message(_)) =>
            {
                // A direct reply has no task to receive follow-ups. Release
                // its reservation now, while allowing the executor to finish
                // its work; events after the reply are still ignored.
                {
                    let _writer = active.writer.lock().await;
                    if let Some(task) = active.snapshot().await
                        && let Err(error) = validate_event_identity(&task, &event)
                    {
                        return TurnEnd::Failed(error);
                    }
                    active
                        .publish_kind(Ok(&event), None, EventKind::Reply)
                        .await;
                    runtime.execution_manager.finish(task_id, active).await;
                }
                runtime.changes.notify(task_id);
                while let Some(rest) = stream.next().await {
                    if rest.is_ok() {
                        tracing::warn!(%task_id, "the executor emitted an event after a direct reply; ignored");
                    }
                }
                return TurnEnd::Stopped;
            }
            Ok(event) if is_terminal_event(&event) => return TurnEnd::Terminal(Box::new(event)),
            Ok(mut event) => {
                let _writer = active.writer.lock().await;
                if active.snapshot_is_terminal().await {
                    return TurnEnd::Stopped;
                }
                if let Err(error) =
                    apply_executor_event_locked(runtime, active, task_id, &mut event).await
                {
                    return TurnEnd::Failed(error);
                }
            }
            Err(error) => return TurnEnd::Failed(error),
        }
    }
}

/// The caller holds the task writer. Terminal events reach here only after
/// the executor stream has been dropped and unread messages answered.
async fn apply_executor_event_locked(
    runtime: &ExecutionRuntime,
    active: &Arc<ActiveExecution>,
    task_id: &TaskId,
    event: &mut StreamResponse,
) -> Result<(), A2AError> {
    backfill_status_timestamp(event);
    let base = active.snapshot().await;
    let creating = active.uncreated.load(std::sync::atomic::Ordering::SeqCst);
    let applied =
        apply_and_publish_with(runtime.task_store.as_ref(), active, base, event, creating).await;
    active
        .uncreated
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let published = applied?;
    if creating {
        save_pending_push_config(active, runtime).await;
    }
    if published {
        runtime.execution_manager.pushes.push(
            runtime,
            task_id,
            Some(active.push_failure.clone()),
            event,
        );
    }
    runtime.changes.notify(task_id);
    Ok(())
}

/// Ends an execution with `error`: what the executor never read is answered
/// first, then the error reaches the subscribers (which it ends).
async fn fail_execution(
    manager: &ExecutionManager,
    active: &Arc<ActiveExecution>,
    task_id: &str,
    error: &A2AError,
) {
    let _writer = active.writer.lock().await;
    // An executor that fails before it has said anything created no task.
    active
        .uncreated
        .store(false, std::sync::atomic::Ordering::SeqCst);
    release_execution_locked(manager, active, task_id).await;
    active.publish(Err(error), None).await;
}

/// Publishes an error to the subscribers in order with the events around it:
/// two unlocked publishers could broadcast out of sequence, and a subscriber
/// drops what arrives behind a later sequence number.
pub(super) async fn publish_error(active: &ActiveExecution, error: &A2AError) {
    let _writer = active.writer.lock().await;
    active.publish(Err(error), None).await;
}

/// Runs the execution and, if the executor panicked, fails the task: the
/// subscribers get a terminal `FAILED` event instead of a stream that just
/// stops, and the stored task does not stay `WORKING` for good.
pub(super) async fn drive_execution_guarded(
    runtime: ExecutionRuntime,
    task_id: TaskId,
    active: Arc<ActiveExecution>,
    exec_ctx: crate::ExecutorContext,
) {
    use futures::FutureExt;

    let _drained = active.inputs_drained.clone().drop_guard();

    let task_store = Arc::clone(&runtime.task_store);
    let manager = Arc::clone(&runtime.execution_manager);
    let changes = Arc::clone(&runtime.changes);
    let notification_handler = runtime.clone();
    let outcome = std::panic::AssertUnwindSafe(drive_execution(
        runtime,
        task_id.clone(),
        active.clone(),
        exec_ctx,
    ))
    .catch_unwind()
    .await;
    if outcome.is_ok() {
        return;
    }

    tracing::error!(task_id = %task_id, "the agent executor panicked; failing the task");
    let mut failed_event = None;
    {
        let _writer = active.writer.lock().await;
        // A task the executor never created, or answered with a message alone,
        // stays uncreated: the panic is reported on the streams, and there is
        // no task to fail.
        let registered = manager
            .get(&task_id)
            .await
            .is_some_and(|current| Arc::ptr_eq(&current, &active));
        if !registered {
            // The execution was released before the panic (it came from
            // dropping the executor's stream): a later turn may hold the task
            // now, and this one has nothing left to fail.
        } else if active.uncreated.load(std::sync::atomic::Ordering::SeqCst) {
            active
                .publish(
                    Err(&A2AError::internal("the agent failed unexpectedly")),
                    None,
                )
                .await;
        } else if let Some(base) = active.snapshot().await {
            if !base.status.state.is_terminal() {
                reject_unread_inputs_locked(&active, &task_id).await;
                let mut event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: task_id.clone(),
                    context_id: base.context_id.clone(),
                    status: TaskStatus {
                        state: TaskState::Failed,
                        message: Some(Message::new(
                            Role::Agent,
                            vec![Part::text("The agent failed unexpectedly.")],
                        )),
                        timestamp: Some(chrono::Utc::now()),
                    },
                    metadata: None,
                    generation: 0,
                });
                match apply_and_publish(task_store.as_ref(), &active, Some(base), &mut event).await
                {
                    Ok(_) => failed_event = Some(event),
                    Err(error) => {
                        release_execution_locked(&manager, &active, &task_id).await;
                        active.publish(Err(&error), None).await;
                    }
                }
            }
        }
    }
    reject_unread_inputs(&active, &task_id).await;
    if let Some(event) = failed_event {
        manager
            .pushes
            .push(&notification_handler, &task_id, None, &event);
    }
    release_execution(&manager, &active, &task_id).await;
    changes.notify(&task_id);
}
