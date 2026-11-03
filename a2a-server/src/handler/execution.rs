// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Poll agent code and submit output and completion to the Task coordinator.

use super::DefaultRequestHandler;
use super::coordinator::TaskCoordinator;
use super::events::ActiveExecution;
use super::protocol::is_terminal_event;
use a2a::*;
use futures::StreamExt;
use std::sync::Arc;

/// Starting an invocation has the same cleanup contract for initial input and handoff.
pub(super) fn spawn(
    handler: DefaultRequestHandler,
    task_id: TaskId,
    active: Arc<ActiveExecution>,
    context: crate::ExecutorContext,
) {
    // The driver retains its mailbox before it is polled. Keeping this lease
    // outside the actor-owned invocation avoids a reference cycle.
    let coordinator = handler.execution_manager.coordinators.for_task(&task_id);
    let work: futures::future::BoxFuture<'static, ()> = Box::pin(async move {
        let _lease = coordinator;
        drive_execution_guarded(handler, task_id, active, context).await;
    });
    tokio::spawn(work);
}

/// The output stream has been dropped before these reach the runner. Input
/// receipts let the server recover unread messages from an owned inbox.
pub(super) enum TurnEnd {
    Finished,
    Terminal(Box<StreamResponse>),
    Failed(A2AError),
    Stopped,
}

async fn drive_execution(
    handler: DefaultRequestHandler,
    task_id: TaskId,
    active: Arc<ActiveExecution>,
    exec_ctx: crate::ExecutorContext,
    coordinator: &Arc<TaskCoordinator>,
) {
    let end = poll_executor(&handler, &task_id, &active, exec_ctx, coordinator).await;
    coordinator
        .finished(handler, active, end)
        .await
        .expect("execution completion must reach its coordinator");
}

/// Cancellation drops the owned output stream; input receipts can be reclaimed
/// even when an executor moved its receiver into a separate worker task.
async fn poll_executor(
    handler: &DefaultRequestHandler,
    task_id: &TaskId,
    active: &Arc<ActiveExecution>,
    exec_ctx: crate::ExecutorContext,
    coordinator: &Arc<TaskCoordinator>,
) -> TurnEnd {
    let mut stream = handler.executor.execute(exec_ctx);
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
            Ok(event) if is_terminal_event(&event) => return TurnEnd::Terminal(Box::new(event)),
            Ok(event) => {
                let action = coordinator
                    .output(handler.clone(), active.clone(), event)
                    .await;
                match action {
                    Ok(super::runtime::OutputAction::Continue) => {}
                    Ok(super::runtime::OutputAction::Stopped) => return TurnEnd::Stopped,
                    Ok(super::runtime::OutputAction::Drain) => {
                        while let Some(rest) = stream.next().await {
                            if rest.is_ok() {
                                tracing::warn!(%task_id, "the executor emitted an event after a direct reply; ignored");
                            }
                        }
                        return TurnEnd::Stopped;
                    }
                    Err(error) => return TurnEnd::Failed(error),
                }
            }
            Err(error) => return TurnEnd::Failed(error),
        }
    }
}

/// Runs the execution and, if the executor panicked, fails the task: the
/// subscribers get a terminal `FAILED` event instead of a stream that just
/// stops, and the stored task does not stay `WORKING` for good.
pub(super) async fn drive_execution_guarded(
    handler: DefaultRequestHandler,
    task_id: TaskId,
    active: Arc<ActiveExecution>,
    exec_ctx: crate::ExecutorContext,
) {
    use futures::FutureExt;

    let _drained = active.inputs_drained.clone().drop_guard();
    let coordinator = handler.execution_manager.coordinators.for_task(&task_id);

    let recovery = handler.clone();
    handler
        .execution_manager
        .pushes
        .attach(&handler, &task_id, &active.events);
    let outcome = std::panic::AssertUnwindSafe(drive_execution(
        handler,
        task_id.clone(),
        active.clone(),
        exec_ctx,
        &coordinator,
    ))
    .catch_unwind()
    .await;
    if outcome.is_ok() {
        return;
    }

    let _ = coordinator.panicked(recovery, active, task_id).await;
}

#[cfg(test)]
mod tests {
    use super::super::coordinator::TaskOperation;
    use super::super::events::EventKind;
    use super::*;
    use crate::handler::test_support::*;

    struct GatedPushStore {
        inner: InMemoryPushConfigStore,
        saving: Arc<Notify>,
        release: Arc<Notify>,
    }

    #[async_trait]
    impl PushConfigStore for GatedPushStore {
        async fn save(
            &self,
            config: TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            self.saving.notify_one();
            self.release.notified().await;
            self.inner.save(config).await
        }
        async fn get(&self, task: &str, id: &str) -> Result<TaskPushNotificationConfig, A2AError> {
            self.inner.get(task, id).await
        }
        async fn list(&self, task: &str) -> Result<Vec<TaskPushNotificationConfig>, A2AError> {
            self.inner.list(task).await
        }
        async fn delete(&self, task: &str, id: &str) -> Result<(), A2AError> {
            self.inner.delete(task, id).await
        }
        async fn delete_all(&self, task: &str) -> Result<(), A2AError> {
            self.inner.delete_all(task).await
        }
    }

    #[tokio::test]
    async fn lazy_creation_saves_push_configuration_before_publishing_its_first_event() {
        install_crypto_provider();
        let (url, mut pushes, shutdown, server) = start_push_webhook().await;
        let saving = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let handler = DefaultRequestHandler::new(PushEventExecutor, InMemoryTaskStore::new())
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            })
            .with_push_notifications(
                GatedPushStore {
                    inner: InMemoryPushConfigStore::new(),
                    saving: saving.clone(),
                    release: release.clone(),
                },
                crate::HttpPushSender::new(Some(crate::HttpPushSenderConfig {
                    validate_urls: false,
                    ..Default::default()
                })),
            );
        let mut request = send_req(None, None);
        let mut config = push_config("");
        config.url = url;
        request.configuration = Some(SendMessageConfiguration {
            task_push_notification_config: Some(config),
            accepted_output_modes: None,
            history_length: None,
            timeline_length: None,
            return_immediately: None,
            if_generation_match: None,
        });
        let mut stream = handler
            .send_streaming_message(&ServiceParams::new(), request)
            .await
            .unwrap();
        timeout(Duration::from_secs(2), saving.notified())
            .await
            .unwrap();
        assert!(
            timeout(Duration::from_millis(50), stream.next())
                .await
                .is_err(),
            "the first output became visible before its webhook configuration was saved"
        );
        release.notify_one();
        assert!(matches!(
            timeout(Duration::from_secs(2), stream.next())
                .await
                .unwrap(),
            Some(Ok(_))
        ));
        assert!(matches!(
            next_push(&mut pushes).await.event,
            StreamResponse::StatusUpdate(_)
        ));
        let StreamResponse::Task(task) = next_push(&mut pushes).await.event else {
            panic!("expected the final task notification");
        };
        // Deferred creation must leave an ordinary, manageable push configuration.
        let configs = handler.push_config_store.as_ref().unwrap();
        let saved = configs.list(&task.id).await.unwrap();
        assert_eq!(saved.len(), 1);
        let id = saved[0].id.as_deref().unwrap();
        assert_eq!(configs.get(&task.id, id).await.unwrap(), saved[0]);
        handler
            .delete_push_config(
                &ServiceParams::new(),
                DeleteTaskPushNotificationConfigRequest {
                    task_id: task.id.clone(),
                    id: id.into(),
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert!(configs.get(&task.id, id).await.is_err());
        assert!(configs.list(&task.id).await.unwrap().is_empty());
        let _ = shutdown.send(());
        server.await.unwrap();
    }

    async fn registered(
        runtime: &DefaultRequestHandler,
        task: Task,
    ) -> (
        Arc<ActiveExecution>,
        ExecutorContext,
        Arc<super::super::coordinator::TaskCoordinator>,
    ) {
        let coordinator = runtime.execution_manager.coordinators.for_task(&task.id);
        let handler = runtime.clone();
        let task_id = task.id.clone();
        let (active, inputs) = coordinator
            .update(TaskOperation::Input, move |state| {
                Box::pin(async move { state.reserve(&handler, &task_id).await })
            })
            .await
            .unwrap()
            .unwrap();
        active.set_snapshot(task.clone()).await;
        let context = ExecutorContext::new(task.id.clone(), task.context_id.clone(), inputs)
            .with_stored_task(task);
        (active, context, coordinator)
    }

    #[tokio::test]
    async fn a_failed_terminal_save_releases_the_execution_without_overwriting_cancellation() {
        let runtime = make_handler();
        let mut task = task_with_history("save-fails", "context", 0);
        task.status.state = TaskState::Canceled;
        runtime.task_store.create(task.clone()).await.unwrap();
        task.status.state = TaskState::Working;
        let (active, context, _lease) = registered(&runtime, task.clone()).await;
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
            let (active, context, _lease) = registered(&runtime, task.clone()).await;
            active
                .push_failure
                .send_replace(Some(A2AError::internal("late delivery failure")));
            if canceled {
                active.inputs.revoke();
            }
            assert!(matches!(
                poll_executor(&runtime, &task.id, &active, context, &_lease).await,
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
        let (active, context, _lease) = registered(&runtime, task.clone()).await;
        let mut events = active.subscribe().await;
        let runner = tokio::spawn({
            let runtime = runtime.clone();
            let active = active.clone();
            let task_id = task.id.clone();
            let coordinator = _lease.clone();
            async move { poll_executor(&runtime, &task_id, &active, context, &coordinator).await }
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
        let (active, context, _lease) = registered(&runtime, task.clone()).await;
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
            let (active, mut context, _lease) = registered(&runtime, task.clone()).await;
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
            let (active, context, _lease) = registered(&runtime, task.clone()).await;
            active
                .uncreated
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let coordinator = runtime.execution_manager.coordinators.for_task(&task.id);
            let control = active.clone();
            let config = push_config(&task.id);
            coordinator
                .update(TaskOperation::Input, move |state| {
                    Box::pin(async move {
                        state.execution.bookkeeping(&control).unwrap().pending_push = Some(config);
                    })
                })
                .await
                .unwrap();
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
        let (active, context, _lease) = registered(&runtime, task.clone()).await;
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
