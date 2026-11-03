// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

use super::{
    events::ActiveExecution,
    protocol::is_terminal_event,
    push_delivery::send_push_notifications,
    registry::ExecutionManager,
    updates::{apply_and_publish, backfill_status_timestamp},
};
use a2a::*;
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::Notify;

pub(super) struct ExecutionRuntime {
    pub(super) executor: Arc<dyn crate::AgentExecutor>,
    pub(super) task_store: Arc<dyn crate::TaskStore>,
    pub(super) push_config_store: Option<Arc<dyn crate::PushConfigStore>>,
    pub(super) push_sender: Option<Arc<crate::HttpPushSender>>,
    pub(super) execution_manager: Arc<ExecutionManager>,
    pub(super) task_changed: Arc<Notify>,
}

pub(super) async fn drive_execution(
    runtime: ExecutionRuntime,
    task_id: TaskId,
    active: Arc<ActiveExecution>,
    exec_ctx: crate::ExecutorContext,
) {
    let mut stream = runtime.executor.execute(exec_ctx);
    let (mut answered, mut tracked) = (false, false);

    while let Some(result) = stream.next().await {
        if active.snapshot_is_terminal().await {
            break;
        }

        match result {
            Ok(mut event) => {
                let message = matches!(event, StreamResponse::Message(_));
                if answered || (message && tracked) {
                    fail_and_publish(&runtime, &active, &A2AError::invalid_agent_response()).await;
                    break;
                }
                answered = message;
                tracked |= !message;
                backfill_status_timestamp(&mut event);
                // One writer at a time: the snapshot is read, changed, saved
                // and published under the lock, so a cancel that lands
                // meanwhile is seen here instead of being overwritten. The
                // push below runs after the lock is released: a slow webhook
                // must not hold up a cancel.
                let published = {
                    let _writer = active.writer.lock().await;
                    let base = active.snapshot().await;
                    if base
                        .as_ref()
                        .is_some_and(|task| task.status.state.is_terminal())
                    {
                        None
                    } else {
                        match apply_and_publish(
                            runtime.task_store.as_ref(),
                            &active,
                            base,
                            &mut event,
                        )
                        .await
                        {
                            Ok(published) => {
                                runtime.task_changed.notify_waiters();
                                Some(published)
                            }
                            Err(error) => {
                                let failed =
                                    fail_task(&runtime, active.snapshot().await, &error).await;
                                active.publish(Err(&error), failed).await;
                                None
                            }
                        }
                    }
                };
                let Some(published) = published else {
                    break;
                };
                if !published {
                    continue;
                }
                if let Err(error) = send_push_notifications(
                    &task_id,
                    runtime.push_config_store.as_deref(),
                    runtime.push_sender.as_deref(),
                    &event,
                )
                .await
                {
                    fail_and_publish(&runtime, &active, &error).await;
                    break;
                }
                if is_terminal_event(&event) {
                    break;
                }
            }
            Err(error) => {
                fail_and_publish(&runtime, &active, &error).await;
                break;
            }
        }
    }

    runtime.execution_manager.finish(&task_id, &active).await;
    runtime.task_changed.notify_waiters();
}

/// Publishes an error to the subscribers in order with the events around it:
/// two unlocked publishers could broadcast out of sequence, and a subscriber
/// drops what arrives behind a later sequence number.
pub(super) async fn fail_and_publish(
    runtime: &ExecutionRuntime,
    active: &ActiveExecution,
    error: &A2AError,
) {
    let _writer = active.writer.lock().await;
    let failed = fail_task(runtime, active.snapshot().await, error).await;
    active.publish(Err(error), failed).await;
}

async fn fail_task(
    runtime: &ExecutionRuntime,
    task: Option<Task>,
    error: &A2AError,
) -> Option<Task> {
    let mut task = task.filter(|task| !task.status.state.is_terminal())?;
    let text = crate::sanitized_internal_error(&error.message).message;
    task.status = TaskStatus {
        state: TaskState::Failed,
        message: Some(Message {
            task_id: Some(task.id.clone()),
            context_id: Some(task.context_id.clone()),
            ..Message::new(Role::Agent, vec![Part::text(text)])
        }),
        timestamp: Some(chrono::Utc::now()),
    };
    let status = task.status.clone();
    super::updates::record_status(&mut task, status);
    let task = super::updates::save_task(runtime.task_store.as_ref(), task)
        .await
        .ok()?;
    let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        generation: task.generation,
        task_id: task.id.clone(),
        context_id: task.context_id.clone(),
        status: task.status.clone(),
        metadata: None,
    });
    let _ = send_push_notifications(
        &task.id,
        runtime.push_config_store.as_deref(),
        runtime.push_sender.as_deref(),
        &event,
    )
    .await;
    Some(task)
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

    let task_store = Arc::clone(&runtime.task_store);
    let manager = Arc::clone(&runtime.execution_manager);
    let task_changed = Arc::clone(&runtime.task_changed);
    let push_config_store = runtime.push_config_store.clone();
    let push_sender = runtime.push_sender.clone();
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
        if let Some(base) = active.snapshot().await {
            if !base.status.state.is_terminal() {
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
                    Err(error) => active.publish(Err(&error), None).await,
                }
            }
        }
    }
    if let Some(event) = failed_event {
        if let Err(error) = send_push_notifications(
            &task_id,
            push_config_store.as_deref(),
            push_sender.as_deref(),
            &event,
        )
        .await
        {
            tracing::warn!(task_id = %task_id, %error, "could not push the failure");
        }
    }
    manager.finish(&task_id, &active).await;
    task_changed.notify_waiters();
}

pub(super) async fn publish_error(active: &ActiveExecution, error: &A2AError) {
    let _writer = active.writer.lock().await;
    active.publish(Err(error), None).await;
}
