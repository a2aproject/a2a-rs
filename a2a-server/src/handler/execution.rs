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
            fail_execution(&runtime, &active, &task_id, &error).await;
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
    let (mut answered, mut tracked) = (false, false);
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
        if let Ok(event) = &result {
            let message = matches!(event, StreamResponse::Message(_));
            if answered || (message && tracked) {
                return TurnEnd::Failed(A2AError::invalid_agent_response());
            }
            answered = message;
            tracked |= !message;
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
    applied?;
    if creating {
        save_pending_push_config(active, runtime).await;
    }
    runtime.changes.notify(task_id);
    Ok(())
}

/// Ends an execution with `error`: what the executor never read is answered
/// first, then the error reaches the subscribers (which it ends).
async fn fail_execution(
    runtime: &ExecutionRuntime,
    active: &Arc<ActiveExecution>,
    task_id: &str,
    error: &A2AError,
) {
    let _writer = active.writer.lock().await;
    // An executor that fails before it has said anything created no task.
    let creating = active
        .uncreated
        .swap(false, std::sync::atomic::Ordering::SeqCst);
    let failed = if creating {
        None
    } else {
        fail_task(runtime.task_store.as_ref(), active.snapshot().await, error).await
    };
    release_execution_locked(&runtime.execution_manager, active, task_id).await;
    active.publish(Err(error), failed.clone()).await;
    if let Some(task) = failed {
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            generation: task.generation,
            task_id: task.id,
            context_id: task.context_id,
            status: task.status,
            metadata: None,
        });
        active.publish(Ok(&event), None).await;
    }
}

/// Publishes an error to the subscribers in order with the events around it:
/// two unlocked publishers could broadcast out of sequence, and a subscriber
/// drops what arrives behind a later sequence number.
pub(super) async fn publish_error(active: &ActiveExecution, error: &A2AError) {
    let _writer = active.writer.lock().await;
    active.publish(Err(error), None).await;
}

async fn fail_task(
    task_store: &dyn crate::TaskStore,
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
    super::updates::save_task(task_store, task).await.ok()
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
    runtime
        .execution_manager
        .pushes
        .attach(&runtime, &task_id, &active.events);
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
                    Ok(_) => {}
                    Err(error) => {
                        release_execution_locked(&manager, &active, &task_id).await;
                        active.publish(Err(&error), None).await;
                    }
                }
            }
        }
    }
    reject_unread_inputs(&active, &task_id).await;
    release_execution(&manager, &active, &task_id).await;
    changes.notify(&task_id);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    async fn registered(
        runtime: &ExecutionRuntime,
        task: Task,
    ) -> (Arc<ActiveExecution>, ExecutorContext) {
        let (active, inputs, writer) = runtime.execution_manager.reserve(&task.id).await.unwrap();
        active.set_snapshot(task.clone()).await;
        drop(writer);
        let context = ExecutorContext::new(task.id.clone(), task.context_id.clone(), inputs)
            .with_stored_task(task);
        (active, context)
    }

    #[tokio::test]
    async fn a_failed_terminal_save_releases_the_execution_without_overwriting_cancellation() {
        let runtime = make_handler();
        let mut task = task_with_history("save-fails", "context", 0);
        task.status.state = TaskState::Canceled;
        runtime.task_store.create(task.clone()).await.unwrap();
        task.status.state = TaskState::Working;
        let (active, context) = registered(&runtime, task.clone()).await;
        let mut events = active.subscribe().await;
        timeout(
            Duration::from_secs(1),
            drive_execution_guarded(runtime.clone(), task.id.clone(), active.clone(), context),
        )
        .await
        .unwrap();
        assert_eq!(
            events.recv().await.unwrap().result.unwrap_err().code,
            error_code::INVALID_REQUEST
        );
        assert!(runtime.execution_manager.get(&task.id).await.is_none());
        assert!(active.inputs_drained.is_cancelled());
        assert_eq!(
            runtime
                .task_store
                .get(&task.id)
                .await
                .unwrap()
                .unwrap()
                .status
                .state,
            TaskState::Canceled
        );
    }

    #[tokio::test]
    async fn terminal_state_and_cancellation_take_precedence_over_push_failure() {
        for canceled in [false, true] {
            let runtime = make_handler();
            let mut task = task_with_history("already-stopped", "context", 0);
            task.status.state = if canceled {
                TaskState::Working
            } else {
                TaskState::Canceled
            };
            let (active, context) = registered(&runtime, task.clone()).await;
            active
                .push_failure
                .send_replace(Some(A2AError::internal("late delivery failure")));
            if canceled {
                active.inputs.revoke();
            }
            assert!(matches!(
                poll_executor(&runtime, &task.id, &active, context).await,
                TurnEnd::Stopped
            ));
            assert!(active.snapshot().await.is_some());
        }
    }

    #[tokio::test]
    async fn late_executor_output_cannot_overwrite_a_terminal_snapshot() {
        let (runtime, release) = make_resumable_handler();
        let mut task = task_with_history("late-output", "context", 0);
        task.status.state = TaskState::Submitted;
        runtime.task_store.create(task.clone()).await.unwrap();
        let (active, context) = registered(&runtime, task.clone()).await;
        let mut events = active.subscribe().await;
        let runner = tokio::spawn({
            let runtime = runtime.clone();
            let active = active.clone();
            let task_id = task.id.clone();
            async move { poll_executor(&runtime, &task_id, &active, context).await }
        });
        events.recv().await.unwrap().result.unwrap();
        let canceled = runtime.task_store.begin_cancel(&task.id).await.unwrap();
        active.set_snapshot(canceled).await;
        release.notify_one();
        assert!(matches!(
            timeout(Duration::from_secs(1), runner)
                .await
                .unwrap()
                .unwrap(),
            TurnEnd::Stopped
        ));
        assert_eq!(
            runtime
                .task_store
                .get(&task.id)
                .await
                .unwrap()
                .unwrap()
                .status
                .state,
            TaskState::Canceled
        );
        assert!(
            events.try_recv().is_err(),
            "late completion must not be published"
        );
    }

    struct Panics;
    impl crate::AgentExecutor for Panics {
        fn execute(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            panic!("executor panicked before returning its stream");
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            Box::pin(stream::empty())
        }
    }

    #[tokio::test]
    async fn failure_to_persist_a_panic_still_notifies_and_releases_subscribers() {
        let runtime = DefaultRequestHandler::new(Panics, InMemoryTaskStore::new());
        let mut task = task_with_history("panic-save-fails", "context", 0);
        task.status.state = TaskState::Canceled;
        runtime.task_store.create(task.clone()).await.unwrap();
        task.status.state = TaskState::Working;
        let (active, context) = registered(&runtime, task.clone()).await;
        let mut events = active.subscribe().await;
        timeout(
            Duration::from_secs(1),
            drive_execution_guarded(runtime.clone(), task.id.clone(), active.clone(), context),
        )
        .await
        .unwrap();
        assert_eq!(
            events.recv().await.unwrap().result.unwrap_err().code,
            error_code::INVALID_REQUEST
        );
        assert!(active.inputs_drained.is_cancelled());
        assert!(runtime.execution_manager.get(&task.id).await.is_none());
        assert_eq!(
            runtime
                .task_store
                .get(&task.id)
                .await
                .unwrap()
                .unwrap()
                .status
                .state,
            TaskState::Canceled
        );
    }

    struct Scripted(Vec<Result<StreamResponse, A2AError>>);
    impl crate::AgentExecutor for Scripted {
        fn execute(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            Box::pin(stream::iter(self.0.clone()))
        }
        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            self.execute(ctx)
        }
    }

    #[tokio::test]
    async fn uncreated_runs_reply_without_a_task_and_reject_foreign_reply_ids() {
        for foreign in [false, true] {
            let mut task = task_with_history("reply", "context", 0);
            task.status.state = TaskState::Submitted;
            let mut reply = Message::new(Role::Agent, vec![Part::text("direct reply")]);
            if foreign {
                reply.task_id = Some("foreign-task".into());
            }
            let runtime = DefaultRequestHandler::new(
                Scripted(vec![
                    Ok(StreamResponse::Message(reply)),
                    Ok(StreamResponse::Task(task.clone())),
                ]),
                InMemoryTaskStore::new(),
            );
            let (active, mut context) = registered(&runtime, task.clone()).await;
            active
                .uncreated
                .store(true, std::sync::atomic::Ordering::SeqCst);
            context.new_task = true;
            let mut events = active.subscribe().await;
            drive_execution_guarded(runtime.clone(), task.id.clone(), active.clone(), context)
                .await;
            let event = events.recv().await.unwrap();
            if foreign {
                assert_eq!(
                    event.result.unwrap_err().code,
                    error_code::INVALID_AGENT_RESPONSE
                );
            } else {
                assert!(event.kind == EventKind::Reply);
                assert!(matches!(event.result, Ok(StreamResponse::Message(_))));
            }
            assert!(runtime.task_store.get(&task.id).await.unwrap().is_none());
            assert!(runtime.execution_manager.get(&task.id).await.is_none());
            assert!(
                events.try_recv().is_err(),
                "outputs following a direct reply must be ignored"
            );
        }
    }

    #[tokio::test]
    async fn empty_uncreated_runs_save_pending_configuration_and_handle_create_collisions() {
        for collision in [false, true] {
            let runtime =
                DefaultRequestHandler::new(Scripted(Vec::new()), InMemoryTaskStore::new())
                    .with_push_config_store(InMemoryPushConfigStore::new());
            let mut task = task_with_history("empty", "context", 0);
            task.status.state = TaskState::Submitted;
            if collision {
                runtime.task_store.create(task.clone()).await.unwrap();
            }
            let (active, context) = registered(&runtime, task.clone()).await;
            active
                .uncreated
                .store(true, std::sync::atomic::Ordering::SeqCst);
            *active.pending_push.lock().unwrap() = Some(push_config(&task.id));
            drive_execution_guarded(runtime.clone(), task.id.clone(), active.clone(), context)
                .await;
            assert_eq!(
                runtime.task_store.get(&task.id).await.unwrap().unwrap().id,
                task.id
            );
            assert!(runtime.execution_manager.get(&task.id).await.is_none());
            let configs = runtime
                .push_config_store
                .as_ref()
                .unwrap()
                .list(&task.id)
                .await
                .unwrap();
            assert_eq!(configs.len(), usize::from(!collision));
        }
    }

    #[tokio::test]
    async fn a_panic_before_task_creation_does_not_leave_an_orphan_task() {
        let runtime = DefaultRequestHandler::new(Panics, InMemoryTaskStore::new());
        let mut task = task_with_history("panic-before-create", "context", 0);
        task.status.state = TaskState::Submitted;
        let (active, context) = registered(&runtime, task.clone()).await;
        active
            .uncreated
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut events = active.subscribe().await;
        drive_execution_guarded(runtime.clone(), task.id.clone(), active, context).await;
        assert_eq!(
            events.recv().await.unwrap().result.unwrap_err().code,
            error_code::INTERNAL_ERROR
        );
        assert!(runtime.task_store.get(&task.id).await.unwrap().is_none());
        assert!(runtime.execution_manager.get(&task.id).await.is_none());
    }

    #[tokio::test]
    async fn empty_cancellation_hooks_still_finalize_idle_tasks() {
        let handlers = [
            DefaultRequestHandler::new(Panics, InMemoryTaskStore::new()),
            DefaultRequestHandler::new(Scripted(Vec::new()), InMemoryTaskStore::new()),
        ];
        for handler in handlers {
            seed_task(&handler, "idle", "context").await;
            let task = handler
                .cancel_task(
                    &ServiceParams::new(),
                    CancelTaskRequest {
                        id: "idle".into(),
                        metadata: None,
                        tenant: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(task.status.state, TaskState::Canceled);
        }
    }
}
