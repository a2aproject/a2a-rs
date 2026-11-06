// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Drive executors, own invocation bookkeeping, and finish or hand off execution.

use super::DefaultRequestHandler;
use super::cancellation::Cancellation;
use super::events::ActiveExecution;
use super::protocol::{
    self, EventKind, ExecutionFailure, ExecutionProtocol, OutputKind, Rejection,
};
use super::task_handle::{TaskCommand, TaskController, TaskOperation};
use crate::executor::{ClientInput, ClientInputSender, QueuedInput, ReturnedInputs};
use a2a::*;
use futures::StreamExt;
use std::sync::Arc;

/// Mutable bookkeeping held in TaskController, never behind a shared mutex.
#[derive(Default)]
pub(super) struct InvocationState {
    pending_inputs: Vec<QueuedInput>,
    returned_inputs: Option<ReturnedInputs>,
    pub(super) pending_push: Option<TaskPushNotificationConfig>,
}

impl InvocationState {
    pub(super) fn new(returned_inputs: ReturnedInputs) -> Self {
        Self {
            returned_inputs: Some(returned_inputs),
            ..Default::default()
        }
    }

    pub(super) fn track_input(&mut self, input: ClientInput) -> QueuedInput {
        let input = QueuedInput::new(input);
        self.pending_inputs.retain(QueuedInput::pending);
        self.pending_inputs.push(input.clone());
        input
    }

    /// Revoke even a receiver moved into user work before reclaiming receipts.
    pub(super) fn drain_inputs(&mut self, inputs: &ClientInputSender) -> Vec<ClientInput> {
        inputs.revoke();
        self.returned_inputs.take();
        self.pending_inputs
            .drain(..)
            .filter_map(QueuedInput::reclaim)
            .collect()
    }
}

pub(super) struct Invocation {
    pub(super) active: Arc<ActiveExecution>,
    pub(super) bookkeeping: InvocationState,
}

#[derive(Default)]
pub(super) enum ExecutionState {
    #[default]
    Idle,
    Running(Invocation),
    Canceling {
        invocation: Option<Invocation>,
        progress: Box<Cancellation>,
    },
}

impl ExecutionState {
    pub(super) fn invocation(&self) -> Option<&Invocation> {
        match self {
            Self::Running(invocation) => Some(invocation),
            Self::Canceling { invocation, .. } => invocation.as_ref(),
            Self::Idle => None,
        }
    }

    pub(super) fn bookkeeping(&mut self, active: &ActiveExecution) -> Option<&mut InvocationState> {
        let invocation = match self {
            Self::Running(invocation) => invocation,
            Self::Canceling { invocation, .. } => invocation.as_mut()?,
            Self::Idle => return None,
        };
        (invocation.active.execution_id == active.execution_id)
            .then_some(&mut invocation.bookkeeping)
    }

    pub(super) fn is_current(&self, active: &Arc<ActiveExecution>) -> bool {
        self.invocation()
            .is_some_and(|invocation| Arc::ptr_eq(&invocation.active, active))
    }

    pub(super) fn release(&mut self, active: &Arc<ActiveExecution>) {
        if !self.is_current(active) {
            return;
        }
        match self {
            Self::Running(_) => *self = Self::Idle,
            Self::Canceling { invocation, .. } => *invocation = None,
            Self::Idle => {}
        }
    }
}

pub(super) enum ExecutionRequest {
    Message(crate::ExecutorContext),
    Restart {
        context: crate::ExecutorContext,
        accepted: tokio::sync::oneshot::Sender<bool>,
    },
}

impl From<crate::ExecutorContext> for ExecutionRequest {
    fn from(context: crate::ExecutorContext) -> Self {
        Self::Message(context)
    }
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
    active: Arc<ActiveExecution>,
    request: ExecutionRequest,
) {
    let end = poll_executor(&handler, &active, request).await;
    active
        .task
        .clone()
        .request(TaskOperation::Finished, |reply| TaskCommand::Finished {
            handler: handler.clone(),
            active,
            end,
            reply,
        })
        .await
        .expect("execution completion must reach the task service");
}

/// Cancellation drops the owned output stream; input receipts can be reclaimed
/// even when an executor moved its receiver into a separate worker task.
async fn poll_executor(
    handler: &DefaultRequestHandler,
    active: &Arc<ActiveExecution>,
    request: impl Into<ExecutionRequest> + Send,
) -> TurnEnd {
    let mut stream = match request.into() {
        ExecutionRequest::Message(context) => handler.tasks.executor.execute(context),
        ExecutionRequest::Restart { context, accepted } => {
            let stream = handler.tasks.executor.restart(context);
            let _ = accepted.send(stream.is_some());
            match stream {
                Some(stream) => stream,
                None => return TurnEnd::Finished,
            }
        }
    };
    let mut push_failure = active.push_failure.subscribe();
    let mut output_protocol = ExecutionProtocol::default();
    loop {
        // Cancellation and terminal state take precedence over a late delivery error.
        if active.inputs.closed.is_cancelled() {
            return TurnEnd::Stopped;
        }
        let failure = push_failure.borrow_and_update().clone();
        if let Some(error) = failure {
            if active.snapshot_is_terminal() {
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
        active.emitted_output();
        if active.snapshot_is_terminal() {
            return TurnEnd::Stopped;
        }
        match result {
            Ok(event) => {
                match output_protocol.accept(&event) {
                    Ok(OutputKind::Terminal) => return TurnEnd::Terminal(Box::new(event)),
                    Err(error) => return TurnEnd::Failed(error),
                    _ => {}
                }
                let action = active
                    .task
                    .request(TaskOperation::Output, |reply| TaskCommand::Output {
                        handler: handler.clone(),
                        active: active.clone(),
                        event: Box::new(event),
                        reply,
                    })
                    .await
                    .and_then(std::convert::identity);
                match action {
                    Ok(super::task_handle::OutputAction::Continue) => {}
                    Ok(super::task_handle::OutputAction::Stopped) => return TurnEnd::Stopped,
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
    active: Arc<ActiveExecution>,
    request: impl Into<ExecutionRequest> + Send,
) {
    use futures::FutureExt;

    let _drained = active.inputs_drained.clone().drop_guard();

    let recovery = handler.clone();
    handler.attach_pushes(&active.task);
    let outcome =
        std::panic::AssertUnwindSafe(drive_execution(handler, active.clone(), request.into()))
            .catch_unwind()
            .await;
    if outcome.is_ok() {
        return;
    }

    let _ = active
        .task
        .clone()
        .request(TaskOperation::Finished, |reply| TaskCommand::Panicked {
            handler: recovery,
            active,
            reply,
        })
        .await;
}

/// Starts the invocation that takes over `unread` inside the completion
/// command. Gives the messages back if reservation fails.
pub(super) async fn start_next_turn(
    handler: &DefaultRequestHandler,
    task_id: &TaskId,
    snapshot: Task,
    mut unread: Vec<crate::executor::ClientInput>,
    state: &mut TaskController,
) -> Result<(), Vec<crate::executor::ClientInput>> {
    let Ok((next, inputs)) = state.reserve(handler, task_id).await else {
        return Err(unread);
    };
    next.set_snapshot(snapshot.clone());
    let first = unread.remove(0);
    next.inputs.received(first.generation);
    for input in unread {
        if let Ok(permit) = next.inputs.sender.clone().try_reserve_owned() {
            permit.send(
                state
                    .execution
                    .bookkeeping(&next)
                    .unwrap()
                    .track_input(input),
            );
        }
    }

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
    handler
        .tasks
        .spawn_execution(handler.clone(), next, exec_ctx);
    Ok(())
}

/// Close input delivery and unregister in one task operation. Later
/// input commands either join a successor or start a new invocation.
pub(super) async fn release_execution(
    active: &Arc<ActiveExecution>,
    task_id: &str,
    state: &mut TaskController,
) {
    reject_unread_inputs(active, task_id, state).await;
    state.release(active);
}

/// Saves the push configuration the request that started the execution
/// carried, now that the task it is for exists.
pub(super) async fn save_pending_push_config(
    active: &ActiveExecution,
    handler: &DefaultRequestHandler,
    state: &mut TaskController,
) -> Result<(), A2AError> {
    if let (Some(invocation), Some(pushes)) = (state.execution.bookkeeping(active), &handler.pushes)
    {
        pushes.save_pending(&mut invocation.pending_push).await?;
    }
    Ok(())
}

/// Closes the task-input queue and tells the exchanges about each
/// message the executor never read. Those messages were accepted, recorded and
/// announced; the executor's turn ended without handling them, which the
/// client that sent one is told by a `StreamError` naming it, rather than by
/// silence.
pub(super) async fn reject_unread_inputs(
    active: &ActiveExecution,
    task_id: &str,
    state: &mut TaskController,
) {
    reject_inputs(
        active,
        task_id,
        state
            .execution
            .bookkeeping(active)
            .map(|invocation| invocation.drain_inputs(&active.inputs))
            .unwrap_or_default(),
    )
    .await;
}

/// Publishes unread-input errors inside the task completion operation.
pub(super) async fn reject_inputs(
    active: &ActiveExecution,
    task_id: &str,
    inputs: Vec<crate::executor::ClientInput>,
) {
    for input in inputs {
        let event = Rejection::unread_input()
            .into_stream_error(task_id, Some(input.message.message_id.clone()));
        active
            .publish_kind(Ok(&event), None, EventKind::LiveOnly(input.response_id))
            .await;
    }
}

/// Called only inside the task runtime. A failed lazy start leaves no Task.
pub(super) async fn persist_failure(
    handler: &DefaultRequestHandler,
    active: &ActiveExecution,
    task_id: &str,
    error: &A2AError,
) -> Option<Task> {
    if active.snapshot_is_terminal() {
        return None;
    }
    let task = match handler.tasks.get(task_id).await {
        Ok(Some(task)) if !task.status.state.is_terminal() => task,
        Ok(_) => return None,
        Err(cause) => {
            tracing::warn!(%task_id, %cause, "could not load the failed Task");
            return None;
        }
    };
    let mut event = protocol::failure_event(&task, ExecutionFailure::Error(error));
    match handler.tasks.persist(Some(task), &mut event, false).await {
        Ok((snapshot, _)) => snapshot,
        Err(cause) => {
            tracing::warn!(%task_id, %cause, "could not persist the failed Task");
            None
        }
    }
}

/// Report any failed output through the same persisted-task failure path.
pub(super) async fn publish_failure(
    handler: &DefaultRequestHandler,
    active: &ActiveExecution,
    task_id: &str,
    error: &A2AError,
) {
    let failed = persist_failure(handler, active, task_id, error).await;
    active.publish(Err(error), failed.clone()).await;
    if let Some(task) = failed {
        let event = protocol::current_status_event(task);
        active.publish(Ok(&event), None).await;
    }
}

#[cfg(test)]
mod driver_tests {
    use super::super::protocol::EventKind;
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
        let configs = handler.pushes.as_ref().unwrap().store();
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
        Arc<super::super::task_handle::TaskSender>,
    ) {
        let lease = runtime.tasks.test_lease(&task.id);
        let handler = runtime.clone();
        let task_id = task.id.clone();
        let (active, inputs) = runtime
            .tasks
            .test_update(&task.id, move |state| {
                Box::pin(async move { state.reserve(&handler, &task_id).await })
            })
            .await
            .unwrap()
            .unwrap();
        active.set_snapshot(task.clone());
        let context = ExecutorContext::new(task.id.clone(), task.context_id.clone(), inputs)
            .with_stored_task(task);
        (active, context, lease)
    }

    #[tokio::test]
    async fn a_failed_terminal_save_releases_the_execution_without_overwriting_cancellation() {
        let runtime = make_handler();
        let mut task = task_with_history("save-fails", "context", 0);
        task.status.state = TaskState::Canceled;
        runtime.tasks.store().create(task.clone()).await.unwrap();
        task.status.state = TaskState::Working;
        let (active, context, _lease) = registered(&runtime, task.clone()).await;
        let mut events = active.subscribe();
        timeout(
            Duration::from_secs(1),
            drive_execution_guarded(runtime.clone(), active.clone(), context),
        )
        .await
        .unwrap();
        assert_eq!(
            events.recv().await.unwrap().result.unwrap_err().code,
            error_code::INVALID_REQUEST
        );
        assert!(runtime.tasks.task(&task.id).active().is_none());
        assert!(active.inputs_drained.is_cancelled());
        assert_eq!(
            runtime
                .tasks
                .store()
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
                poll_executor(&runtime, &active, context).await,
                TurnEnd::Stopped
            ));
            assert!(active.snapshot().is_some());
        }
    }

    #[tokio::test]
    async fn late_executor_output_cannot_overwrite_a_terminal_snapshot() {
        let (runtime, release) = make_resumable_handler();
        let mut task = task_with_history("late-output", "context", 0);
        task.status.state = TaskState::Submitted;
        runtime.tasks.store().create(task.clone()).await.unwrap();
        let (active, context, _lease) = registered(&runtime, task.clone()).await;
        let mut events = active.subscribe();
        let runner = tokio::spawn({
            let runtime = runtime.clone();
            let active = active.clone();
            let lease = _lease.clone();
            async move {
                let _lease = lease;
                poll_executor(&runtime, &active, context).await
            }
        });
        events.recv().await.unwrap().result.unwrap();
        let canceled = runtime.tasks.store().begin_cancel(&task.id).await.unwrap();
        active.set_snapshot(canceled);
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
                .tasks
                .store()
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
        runtime.tasks.store().create(task.clone()).await.unwrap();
        task.status.state = TaskState::Working;
        let (active, context, _lease) = registered(&runtime, task.clone()).await;
        let mut events = active.subscribe();
        timeout(
            Duration::from_secs(1),
            drive_execution_guarded(runtime.clone(), active.clone(), context),
        )
        .await
        .unwrap();
        assert_eq!(
            events.recv().await.unwrap().result.unwrap_err().code,
            error_code::INVALID_REQUEST
        );
        assert!(active.inputs_drained.is_cancelled());
        assert!(runtime.tasks.task(&task.id).active().is_none());
        assert_eq!(
            runtime
                .tasks
                .store()
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
    async fn direct_reply_drops_a_nonterminating_stream_and_releases_its_queue() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        struct Reply(Arc<AtomicBool>);
        impl crate::AgentExecutor for Reply {
            fn execute(
                &self,
                _: ExecutorContext,
            ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
                let guard = DropFlag(self.0.clone());
                Box::pin(async_stream::stream! {
                    let _guard = guard;
                    yield Ok(StreamResponse::Message(Message::new(Role::Agent, vec![Part::text("done")])));
                    std::future::pending::<()>().await;
                })
            }
            fn cancel(
                &self,
                _: ExecutorContext,
            ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
                stream::empty().boxed()
            }
        }
        for mode in [
            super::super::protocol::StartMode::Blocking,
            super::super::protocol::StartMode::Streaming,
            super::super::protocol::StartMode::Bidi,
        ] {
            let dropped = Arc::new(AtomicBool::new(false));
            let handler =
                DefaultRequestHandler::new(Reply(dropped.clone()), InMemoryTaskStore::new())
                    .with_capabilities(AgentCapabilities {
                        streaming: Some(true),
                        ..Default::default()
                    });
            let service = Arc::downgrade(&handler.tasks);
            let request = send_req(None, None);
            if mode == super::super::protocol::StartMode::Blocking {
                let response = timeout(
                    Duration::from_secs(1),
                    handler.send_message(&ServiceParams::new(), request),
                )
                .await
                .unwrap()
                .unwrap();
                assert!(matches!(response, SendMessageResponse::Message(_)));
            } else {
                let mut events = if mode == super::super::protocol::StartMode::Streaming {
                    handler
                        .send_streaming_message(&ServiceParams::new(), request)
                        .await
                        .unwrap()
                } else {
                    handler
                        .send_live_message(
                            &ServiceParams::new(),
                            stream::once(async move {
                                StreamRequest {
                                    tenant: None,
                                    payload: StreamRequestPayload::SendMessage(request),
                                }
                            })
                            .chain(stream::pending())
                            .boxed(),
                        )
                        .await
                        .unwrap()
                };
                timeout(Duration::from_secs(1), async {
                    while let Some(event) = events.next().await {
                        if matches!(event.unwrap(), StreamResponse::Message(_)) {
                            return;
                        }
                    }
                    panic!("expected direct reply");
                })
                .await
                .unwrap();
                assert!(
                    timeout(Duration::from_secs(1), events.next())
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            drop(handler);
            timeout(Duration::from_secs(1), async {
                while service.upgrade().is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("a completed direct reply must not retain the handler");
            assert!(dropped.load(Ordering::SeqCst));
        }
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
            let mut events = active.subscribe();
            drive_execution_guarded(runtime.clone(), active.clone(), context).await;
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
            assert!(runtime.tasks.store().get(&task.id).await.unwrap().is_none());
            assert!(runtime.tasks.task(&task.id).active().is_none());
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
                runtime.tasks.store().create(task.clone()).await.unwrap();
            }
            let (active, context, _lease) = registered(&runtime, task.clone()).await;
            active
                .uncreated
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let _lease = runtime.tasks.test_lease(&task.id);
            let control = active.clone();
            let config = push_config(&task.id);
            runtime
                .tasks
                .test_update(&task.id, move |state| {
                    Box::pin(async move {
                        state.execution.bookkeeping(&control).unwrap().pending_push = Some(config);
                    })
                })
                .await
                .unwrap();
            drive_execution_guarded(runtime.clone(), active.clone(), context).await;
            assert_eq!(
                runtime
                    .tasks
                    .store()
                    .get(&task.id)
                    .await
                    .unwrap()
                    .unwrap()
                    .id,
                task.id
            );
            assert!(runtime.tasks.task(&task.id).active().is_none());
            let configs = runtime
                .pushes
                .as_ref()
                .unwrap()
                .store()
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
        let mut events = active.subscribe();
        drive_execution_guarded(runtime.clone(), active, context).await;
        assert_eq!(
            events.recv().await.unwrap().result.unwrap_err().code,
            error_code::INTERNAL_ERROR
        );
        assert!(runtime.tasks.store().get(&task.id).await.unwrap().is_none());
        assert!(runtime.tasks.task(&task.id).active().is_none());
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

#[cfg(test)]
mod turn_tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn a_busy_handoff_returns_input_with_its_caller_identity_intact() {
        let runtime = make_handler();
        let task = task_with_history("busy-handoff", "context", 0);
        let (active, _inputs, _invocation) = runtime.tasks.task(&task.id).reserve().unwrap();
        active.set_snapshot(task.clone());
        let mut input = crate::ClientInput::new(make_message(), 7);
        input.user = Some(crate::User::authenticated("alice"));
        input.tenant = Some("tenant-a".into());
        input.metadata = Some(std::collections::HashMap::from([(
            "trace".into(),
            serde_json::json!(42),
        )]));
        let returned = start_next_turn(
            &runtime,
            &task.id,
            task.clone(),
            vec![input.clone()],
            &mut TaskController::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(returned.len(), 1);
        assert_eq!(returned[0].message, input.message);
        assert_eq!(returned[0].user, input.user);
        assert_eq!(returned[0].metadata, input.metadata);
        assert_eq!(returned[0].tenant, input.tenant);
        assert!(runtime.tasks.task(&task.id).active().is_some());
    }

    #[tokio::test]
    async fn rejected_pending_push_configuration_does_not_replace_existing_endpoints() {
        let runtime = make_handler_with_push_configs();
        let config = |id: usize| TaskPushNotificationConfig {
            task_id: "configured".into(),
            id: Some(id.to_string()),
            url: "https://example.com/push".into(),
            token: None,
            authentication: None,
            tenant: None,
            protocol_version: Some("1.1".into()),
        };
        let store = runtime.pushes.as_ref().unwrap().store();
        for id in 0..50 {
            store.save(config(id)).await.unwrap();
        }
        let active = ActiveExecution::new(None);
        let mut state = TaskController::default();
        let invocation = InvocationState {
            pending_push: Some(config(50)),
            ..Default::default()
        };
        state.execution = ExecutionState::Running(Invocation {
            active: active.clone(),
            bookkeeping: invocation,
        });
        assert!(
            save_pending_push_config(&active, &runtime, &mut state)
                .await
                .is_err()
        );
        assert!(
            state
                .execution
                .bookkeeping(&active)
                .unwrap()
                .pending_push
                .is_some()
        );
        assert_eq!(store.list("configured").await.unwrap().len(), 50);
        assert!(store.get("configured", "50").await.is_err());
        assert!(
            save_pending_push_config(&active, &runtime, &mut state)
                .await
                .is_err()
        );
        assert_eq!(store.list("configured").await.unwrap().len(), 50);
    }

    #[tokio::test]
    async fn artifact_only_unread_input_has_no_empty_message_reference() {
        let active = ActiveExecution::new(None);
        let mut events = active.subscribe();
        let mut message = make_message();
        message.message_id.clear();
        reject_inputs(&active, "task", vec![crate::ClientInput::new(message, 3)]).await;
        let event = events.recv().await.unwrap();
        assert!(matches!(event.kind, EventKind::LiveOnly(None)));
        let StreamResponse::Error(error) = event.result.unwrap() else {
            panic!("expected a rejection")
        };
        assert_eq!(error.task_id, "task");
        assert_eq!(error.code, "UNAVAILABLE");
        assert_eq!(error.reference_id, None);
    }
    struct CaptureTurn(
        tokio::sync::mpsc::UnboundedSender<(crate::ExecutorContext, Vec<crate::ClientInput>)>,
    );

    impl crate::AgentExecutor for CaptureTurn {
        fn execute(
            &self,
            mut context: crate::ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let queued = std::iter::from_fn(|| context.inputs.try_recv()).collect();
            let mut task = context.stored_task.clone().unwrap();
            task.status.state = TaskState::Completed;
            self.0
                .send((context, queued))
                .unwrap_or_else(|_| panic!("observer dropped"));
            Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
        }
        fn cancel(
            &self,
            context: crate::ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            crate::AgentExecutor::cancel(&EchoExecutor, context)
        }
    }

    #[tokio::test]
    async fn a_handoff_preserves_the_trigger_and_queues_remaining_inputs_in_order() {
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let runtime = DefaultRequestHandler::new(CaptureTurn(sent), InMemoryTaskStore::new());
        let mut task = task_with_history("next-turn", "context", 0);
        task.status.state = TaskState::Working;
        runtime.tasks.store().create(task.clone()).await.unwrap();
        let inputs: Vec<_> = ["first", "second", "third"]
            .into_iter()
            .enumerate()
            .map(|(index, id)| {
                let mut message = make_message();
                message.message_id = id.into();
                let mut input = crate::ClientInput::new(message, index as i64 + 1);
                input.user = Some(crate::User::authenticated("alice"));
                input.tenant = Some("tenant-a".into());
                input.metadata = Some(std::collections::HashMap::from([(
                    "trace".into(),
                    serde_json::json!(id),
                )]));
                input
            })
            .collect();
        let _lease = runtime.tasks.test_lease(&task.id);
        let handler = runtime.clone();
        let snapshot = task.clone();
        let accepted = inputs.clone();
        let started = runtime
            .tasks
            .test_update(&task.id, move |state| {
                Box::pin(async move {
                    start_next_turn(&handler, &snapshot.id, snapshot.clone(), accepted, state).await
                })
            })
            .await
            .unwrap();
        assert!(started.is_ok());
        let (context, queued) = timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(context.message, Some(inputs[0].message.clone()));
        assert_eq!(context.metadata, inputs[0].metadata);
        assert_eq!(context.user, inputs[0].user);
        assert_eq!(context.tenant, inputs[0].tenant);
        assert_eq!(
            queued
                .iter()
                .map(|input| input.message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["second", "third"]
        );
        assert_eq!(
            queued
                .iter()
                .map(|input| input.generation)
                .collect::<Vec<_>>(),
            [2, 3]
        );
        timeout(Duration::from_secs(1), async {
            while runtime.tasks.task(&task.id).active().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            runtime
                .tasks
                .store()
                .get(&task.id)
                .await
                .unwrap()
                .unwrap()
                .status
                .state,
            TaskState::Completed
        );
    }
}

#[cfg(test)]
mod failure_tests {
    use super::*;
    use crate::handler::test_support::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Clone, Default)]
    struct UnavailableStore {
        inner: Arc<InMemoryTaskStore>,
        fail_read: Arc<AtomicBool>,
        fail_write: Arc<AtomicBool>,
    }

    #[async_trait]
    impl TaskStore for UnavailableStore {
        async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.inner.create(task).await
        }
        async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
            if self.fail_write.load(Ordering::SeqCst) {
                return Err(A2AError::internal("storage write unavailable"));
            }
            self.inner.update(task).await
        }
        async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
            if self.fail_read.load(Ordering::SeqCst) {
                return Err(A2AError::internal("storage read unavailable"));
            }
            self.inner.get(id).await
        }
        async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
            self.inner.list(req).await
        }
        async fn begin_cancel(&self, id: &str) -> Result<Task, A2AError> {
            self.inner.begin_cancel(id).await
        }
    }

    #[tokio::test]
    async fn storage_outages_do_not_report_an_uncommitted_failure_or_corrupt_the_task() {
        let store = UnavailableStore::default();
        let handler = DefaultRequestHandler::new(EchoExecutor, store.clone());
        seed_task(&handler, "task", "context").await;
        let before = handler.tasks.load("task").await.unwrap();
        let active = ActiveExecution::new(None);
        active.set_snapshot(before.clone());
        let error = A2AError::internal("private executor detail");

        store.fail_read.store(true, Ordering::SeqCst);
        assert!(
            persist_failure(&handler, &active, "task", &error)
                .await
                .is_none()
        );
        store.fail_read.store(false, Ordering::SeqCst);
        store.fail_write.store(true, Ordering::SeqCst);
        assert!(
            persist_failure(&handler, &active, "task", &error)
                .await
                .is_none()
        );
        assert_eq!(handler.tasks.load("task").await.unwrap(), before);
        assert_eq!(active.snapshot().unwrap(), before);

        store.fail_write.store(false, Ordering::SeqCst);
        let failed = persist_failure(&handler, &active, "task", &error)
            .await
            .unwrap();
        assert_eq!(failed.status.state, TaskState::Failed);
        assert_eq!(
            failed.status.message.unwrap().parts,
            vec![Part::text("Internal error")]
        );
        let listed = store
            .list(&serde_json::from_value(serde_json::json!({})).unwrap())
            .await
            .unwrap();
        assert_eq!(listed.tasks.len(), 1);
        assert_eq!(listed.tasks[0].status.state, TaskState::Failed);
        assert_eq!(
            store.begin_cancel("task").await.unwrap_err().code,
            error_code::TASK_NOT_CANCELABLE
        );

        active.set_snapshot(handler.tasks.load("task").await.unwrap());
        assert!(
            persist_failure(&handler, &active, "task", &error)
                .await
                .is_none()
        );
        assert_eq!(handler.tasks.load("task").await.unwrap(), listed.tasks[0]);
    }
    #[tokio::test]
    async fn an_executor_failure_releases_its_owner_even_when_failure_persistence_is_down() {
        use crate::handler::{execution::TurnEnd, task_handle::TaskController};
        for read_outage in [true, false] {
            let store = UnavailableStore::default();
            let handler = DefaultRequestHandler::new(EchoExecutor, store.clone());
            seed_task(&handler, "task", "context").await;
            let before = handler.tasks.load("task").await.unwrap();
            let mut runtime = TaskController::default();
            let (active, _inputs) = runtime.reserve(&handler, "task").await.unwrap();
            active.set_snapshot(before.clone());
            let mut events = active.subscribe();
            store.fail_read.store(read_outage, Ordering::SeqCst);
            store.fail_write.store(!read_outage, Ordering::SeqCst);
            runtime
                .finished(
                    &handler,
                    "task".into(),
                    active.clone(),
                    TurnEnd::Failed(A2AError::internal("agent failed")),
                )
                .await;
            assert!(events.recv().await.unwrap().result.is_err());
            assert!(handler.tasks.task("task").active().is_none());
            assert!(runtime.execution.invocation().is_none());
            assert_eq!(active.snapshot().unwrap(), before);
            store.fail_read.store(false, Ordering::SeqCst);
            assert_eq!(handler.tasks.load("task").await.unwrap(), before);
            let (successor, _inputs) = runtime.reserve(&handler, "task").await.unwrap();
            assert_ne!(successor.execution_id, active.execution_id);
            runtime.release(&successor);
        }
    }
}
