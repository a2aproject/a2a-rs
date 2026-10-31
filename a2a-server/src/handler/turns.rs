// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Close execution turns and transfer unread inputs to the next turn.

use super::events::{ActiveExecution, EventKind};
use super::execution::drive_execution_guarded;
use super::registry::{ExecutionManager, ExecutionRuntime};
use super::updates::save_task_with;
use a2a::*;
use std::sync::Arc;

/// Ends a turn whose executor has returned, under the task's writer.
///
/// An executor that ended without saying anything still has a task: the one
/// the request prepared. Messages the executor never read, while the task goes
/// on, are not turned away: they were accepted and recorded, so the next turn
/// is theirs. It starts here, under the same writer, with the first as its
/// message and the rest queued. A message is accepted only if some turn will
/// read it.
pub(super) async fn end_turn(
    runtime: &ExecutionRuntime,
    active: &Arc<ActiveExecution>,
    task_id: &TaskId,
) {
    let writer = Arc::clone(&active.writer).lock_owned().await;
    if active
        .uncreated
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        if let Some(task) = active.snapshot().await {
            match save_task_with(runtime.task_store.as_ref(), task, true).await {
                Ok(_) => save_pending_push_config(active, runtime).await,
                Err(error) => {
                    tracing::warn!(task_id = %task_id, %error, "could not create the task");
                }
            }
        }
    }
    let snapshot = active.snapshot().await;
    let goes_on = snapshot
        .as_ref()
        .is_some_and(|task| !task.status.state.is_terminal());
    let unread = if goes_on {
        active.drain_inputs().await
    } else {
        Vec::new()
    };
    let (Some(snapshot), false) = (snapshot, unread.is_empty()) else {
        release_execution_locked(&runtime.execution_manager, active, task_id).await;
        return;
    };
    runtime.execution_manager.finish(task_id, active).await;
    if let Err(unread) = start_next_turn(runtime, task_id, snapshot, unread, writer).await {
        // Nothing could take them over: they are answered, as at any end.
        reject_inputs(active, task_id, unread).await;
    }
}

/// Starts the turn that takes over `unread` messages, under the task's
/// writer. Gives them back if it cannot.
async fn start_next_turn(
    runtime: &ExecutionRuntime,
    task_id: &TaskId,
    snapshot: Task,
    mut unread: Vec<crate::executor::ClientInput>,
    writer: tokio::sync::OwnedMutexGuard<()>,
) -> Result<(), Vec<crate::executor::ClientInput>> {
    let Ok((next, inputs, guard)) = runtime
        .execution_manager
        .reserve_held(task_id, writer)
        .await
    else {
        return Err(unread);
    };
    next.set_snapshot(snapshot.clone()).await;
    let first = unread.remove(0);
    for input in unread {
        if let Ok(permit) = next.inputs.sender.clone().try_reserve_owned() {
            permit.send(next.track_input(input).await);
        }
    }

    drop(guard);
    let runtime = runtime.clone();
    let task_id = task_id.clone();
    tokio::spawn(async move {
        let exec_ctx = crate::ExecutorContext {
            execution_id: next.execution_id.clone(),
            message: Some(first.message),
            task_id: task_id.clone(),
            stored_task: Some(snapshot.clone()),
            context_id: snapshot.context_id.clone(),
            metadata: first.metadata,
            user: first.user,
            service_params: first.service_params,
            tenant: first.tenant,
            inputs,
            new_task: false,
        };
        run_execution(runtime, task_id, next, exec_ctx).await;
    });
    Ok(())
}

/// [`drive_execution_guarded`] as a boxed future: an execution can start the
/// next one as it ends, which a plain `async fn` cannot prove `Send` for.
fn run_execution(
    runtime: ExecutionRuntime,
    task_id: TaskId,
    active: Arc<ActiveExecution>,
    exec_ctx: crate::ExecutorContext,
) -> futures::future::BoxFuture<'static, ()> {
    Box::pin(drive_execution_guarded(runtime, task_id, active, exec_ctx))
}

/// Ends an execution's hold on its task in one step, under the task's writer:
/// the task-input queue is closed (what the executor never read is answered)
/// and the execution deregistered together. A message delivered to the task
/// decides under the same writer, so it finds either a running execution that
/// is taking input, or none.
pub(super) async fn release_execution(
    manager: &ExecutionManager,
    active: &Arc<ActiveExecution>,
    task_id: &str,
) {
    let _writer = active.writer.lock().await;
    release_execution_locked(manager, active, task_id).await;
}

/// [`release_execution`] for a caller that already holds the task's writer.
/// Every place that closes the queue while the task goes on does so through
/// this, so a closed queue is never found on a registered execution.
pub(super) async fn release_execution_locked(
    manager: &ExecutionManager,
    active: &Arc<ActiveExecution>,
    task_id: &str,
) {
    reject_unread_inputs_locked(active, task_id).await;
    manager.finish(task_id, active).await;
}

/// Saves the push configuration the request that started the execution
/// carried, now that the task it is for exists.
pub(super) async fn save_pending_push_config(active: &ActiveExecution, runtime: &ExecutionRuntime) {
    let pending = active
        .pending_push
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    if let (Some(config), Some(store)) = (pending, runtime.push_config_store.as_ref()) {
        if let Err(error) = store.save(config).await {
            tracing::warn!(%error, "could not save the push configuration of a new task");
        }
    }
}

/// Closes the task-input queue and tells the exchanges about each
/// message the executor never read. Those messages were accepted, recorded and
/// announced; the executor's turn ended without handling them, which the
/// client that sent one is told by a `StreamError` naming it, rather than by
/// silence.
pub(super) async fn reject_unread_inputs(active: &ActiveExecution, task_id: &str) {
    // Under the writer: no message can be queued once the queue is closed.
    let _writer = active.writer.lock().await;
    reject_unread_inputs_locked(active, task_id).await;
}

/// [`reject_unread_inputs`] for a caller that already holds the task's writer.
/// Called before a terminal event is published, so the errors come first: a
/// stream ends on the terminal event and would never see them after it.
pub(super) async fn reject_unread_inputs_locked(active: &ActiveExecution, task_id: &str) {
    reject_inputs(active, task_id, active.drain_inputs().await).await;
}

/// Answers each of `inputs` with a `StreamError` naming it. The caller holds
/// the task's writer.
pub(super) async fn reject_inputs(
    active: &ActiveExecution,
    task_id: &str,
    inputs: Vec<crate::executor::ClientInput>,
) {
    for input in inputs {
        let event = StreamResponse::Error(StreamError {
            task_id: task_id.to_string(),
            code: "UNAVAILABLE".to_string(),
            message: "the agent finished its turn before reading this message".to_string(),
            reference_id: Some(input.message.message_id.clone()).filter(|id| !id.is_empty()),
            metadata: None,
        });
        active
            .publish_kind(Ok(&event), None, EventKind::LiveOnly(input.response_id))
            .await;
    }
}
