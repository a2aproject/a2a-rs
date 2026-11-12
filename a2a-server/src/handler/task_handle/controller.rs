// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Private task execution decisions, serialized by TaskHandle’s operation queue.

use super::super::cancellation::CancellationStart;
use super::super::delivery::{AuthorizedMessage, Delivery};
use super::super::events::ActiveExecution;
use super::super::execution::{
    self, ExecutionEnd, ExecutionState, Invocation, reject_unread_inputs, release_execution,
    save_pending_push_config,
};
use super::super::protocol::{
    self, EventKind, ExecutionFailure, OutputKind, ProtocolError, Rejection, StartMode,
    classify_output, prepare_agent_reply, request_task_id,
};
use super::super::{DefaultRequestHandler, admission, cancellation};
use crate::User;
use crate::middleware::ServiceParams;
use a2a::*;
use std::sync::Arc;

#[derive(Default)]
pub(in crate::handler) struct TaskController {
    pub(in crate::handler) execution: ExecutionState,
}

impl TaskController {
    pub(in crate::handler) async fn admit(
        &mut self,
        handler: &DefaultRequestHandler,
        message: AuthorizedMessage,
        mode: StartMode,
        reserved_id: TaskId,
    ) -> Result<Delivery, Rejection> {
        if let (Some(task_id), ExecutionState::Running(invocation)) =
            (request_task_id(&message.request), &mut self.execution)
        {
            let active = invocation.active.clone();
            if !active
                .stream_ended
                .load(std::sync::atomic::Ordering::Acquire)
            {
                match admission::inject_input(
                    handler,
                    &message,
                    &task_id,
                    &active,
                    &mut invocation.bookkeeping,
                )
                .await
                {
                    Err(Rejection::ExecutionEnded) => {} // No input was persisted.
                    delivery => return delivery,
                }
            }
            // EOF may precede the driver's queued Finished command. Complete
            // it here, then admit this fresh request exactly once.
            self.finish_execution(handler, &active, &task_id).await;
        }
        if matches!(self.execution, ExecutionState::Canceling { .. }) {
            return Err(ProtocolError::TaskTerminated(&reserved_id).into());
        }
        self.start(handler, message, mode, reserved_id).await
    }

    /// Recover an existing snapshot without replaying a client message or changing its status.
    pub(in crate::handler) async fn restart(
        &mut self,
        handler: &DefaultRequestHandler,
        task_id: &str,
    ) -> Result<Option<tokio::sync::oneshot::Receiver<bool>>, A2AError> {
        if !matches!(self.execution, ExecutionState::Idle) {
            return Ok(None);
        }
        let Some(task) = handler.tasks.get(task_id).await? else {
            return Ok(None);
        };
        if task.status.state.is_terminal() {
            return Ok(None);
        }
        let (active, inputs) = self.reserve(handler, task_id).await?;
        active.set_snapshot(task.clone());
        handler.attach_pushes(&active.task);
        let mut context =
            crate::ExecutorContext::new(task.id.clone(), task.context_id.clone(), inputs)
                .with_stored_task(task);
        context.execution_id = active.execution_id.clone();
        let (accepted, started) = tokio::sync::oneshot::channel();
        handler.tasks.spawn_execution(
            handler.clone(),
            active,
            execution::ExecutionRequest::Restart { context, accepted },
        );
        Ok(Some(started))
    }

    pub(in crate::handler) async fn reserve(
        &mut self,
        handler: &DefaultRequestHandler,
        task_id: &str,
    ) -> Result<(Arc<ActiveExecution>, crate::ClientInputs), A2AError> {
        if !matches!(self.execution, ExecutionState::Idle) {
            return Err(ProtocolError::ExecutionInProgress.into());
        }
        let task = handler.tasks.task(task_id);
        let (active, inputs, bookkeeping) = ActiveExecution::with_task(task.clone());
        task.register(&active)?;
        self.execution = ExecutionState::Running(Invocation {
            active: active.clone(),
            bookkeeping,
        });
        Ok((active, inputs))
    }

    pub(in crate::handler) fn release(&mut self, active: &Arc<ActiveExecution>) {
        self.execution.release(active);
        active.task.unregister(active);
    }

    /// A failed task operation cannot leave an invocation registered without
    /// a driver that can complete it. Storage remains authoritative on retry.
    pub(in crate::handler) fn recover_panic(&mut self) {
        let active = match std::mem::take(&mut self.execution) {
            ExecutionState::Running(invocation) => Some(invocation.active),
            ExecutionState::Canceling { progress, .. } => Some(progress.active.clone()),
            ExecutionState::Idle => None,
        };
        if let Some(active) = active {
            active.inputs.revoke();
            active.task.unregister(&active);
            active.inputs_drained.cancel();
            active.task.notify();
        }
    }

    async fn start(
        &mut self,
        handler: &DefaultRequestHandler,
        message: AuthorizedMessage,
        mode: StartMode,
        reserved_id: TaskId,
    ) -> Result<Delivery, Rejection> {
        let (active, inputs) = self.reserve(handler, &reserved_id).await?;
        let result = async {
            // Validate and persist input under this task's mailbox reservation.
            let (task, existed, events) =
                admission::prepare(handler, &message.request, &reserved_id).await?;
            let policy = protocol::ResponsePolicy::message(mode, &message.params, &message.request);
            let lazy = policy.defer_task_creation();
            let invocation = self
                .execution
                .bookkeeping(&active)
                .expect("reserved invocation");
            handler
                .tasks
                .commit_input(&task, existed, handler, &message, lazy, invocation)
                .await?;
            active.set_snapshot(task.clone());
            active
                .uncreated
                .store(lazy && !existed, std::sync::atomic::Ordering::SeqCst);

            // Attach delivery before the driver can publish its first output.
            handler.attach_pushes(&active.task);
            if existed || !lazy {
                active.publish_input(task.clone(), &events).await;
            } else {
                active.task.notify();
            }
            let delivery = Delivery::Started {
                task: task.clone(),
                receiver: active.subscribe_for_response(message.response_id),
            };

            // The invocation owns its inbox and caller context independently of this request.
            active.inputs.received(task.generation);
            let context = crate::ExecutorContext {
                execution_id: active.execution_id.clone(),
                message: message.request.message,
                task_id: task.id.clone(),
                context_id: task.context_id.clone(),
                stored_task: Some(task),
                metadata: message.request.metadata,
                user: message.user,
                service_params: message.params,
                tenant: message.request.tenant,
                inputs,
                opening_input_generation: 0,
                new_task: !existed,
                task_snapshot: None,
            };
            handler
                .tasks
                .spawn_execution(handler.clone(), active.clone(), context);
            Ok(delivery)
        }
        .await;
        if result.is_err() {
            self.release(&active);
        }
        result
    }

    pub(in crate::handler) async fn begin_cancel(
        &mut self,
        handler: &DefaultRequestHandler,
        params: &ServiceParams,
        request: CancelTaskRequest,
        user: Option<User>,
    ) -> Result<CancellationStart, A2AError> {
        let active = self
            .execution
            .invocation()
            .map(|invocation| invocation.active.clone());
        let mut progress = None;
        let plan = cancellation::Cancellation::begin(
            handler,
            params,
            request,
            user,
            active,
            &mut progress,
        )
        .await?;
        if let Some(progress) = progress {
            let previous = std::mem::take(&mut self.execution);
            let invocation = match previous {
                ExecutionState::Running(invocation) => Some(invocation),
                ExecutionState::Idle => None,
                ExecutionState::Canceling { .. } => {
                    unreachable!("a committed cancellation returns unchanged")
                }
            };
            self.execution = ExecutionState::Canceling {
                invocation,
                progress: Box::new(progress),
            };
        }
        Ok(plan)
    }

    pub(in crate::handler) fn cancel_output(&self, event: StreamResponse) -> Result<(), A2AError> {
        let ExecutionState::Canceling { progress, .. } = &self.execution else {
            return Err(A2AError::internal("no cancellation in progress"));
        };
        progress.acknowledge(event)
    }

    pub(in crate::handler) async fn cancel_finished(
        &mut self,
        handler: &DefaultRequestHandler,
        failure: Option<A2AError>,
    ) -> Result<(), A2AError> {
        if !matches!(self.execution, ExecutionState::Canceling { .. }) {
            return Err(A2AError::internal("no cancellation in progress"));
        }
        let ExecutionState::Canceling { progress, .. } = std::mem::take(&mut self.execution) else {
            unreachable!()
        };
        let active = progress.active.clone();
        let result = progress.finish(handler, failure).await;
        active.task.unregister(&active);
        result
    }

    pub(in crate::handler) async fn output(
        &mut self,
        handler: &DefaultRequestHandler,
        active: Arc<ActiveExecution>,
        mut event: StreamResponse,
    ) -> Result<OutputAction, A2AError> {
        if !self.execution.is_current(&active)
            || matches!(self.execution, ExecutionState::Canceling { .. })
            || active.snapshot_is_terminal()
        {
            return Ok(OutputAction::Stopped);
        }
        if classify_output(&event) == OutputKind::Reply {
            if let Some(task) = active.snapshot() {
                prepare_agent_reply(
                    &task,
                    &mut event,
                    !active.uncreated.load(std::sync::atomic::Ordering::SeqCst),
                )?;
            }
            if active.uncreated.load(std::sync::atomic::Ordering::SeqCst) {
                active
                    .publish_kind(Ok(&event), None, EventKind::Reply)
                    .await;
            } else {
                // A reply closes response streams. Reject unread input first,
                // then finish this invocation even if its Task is nonterminal.
                reject_unread_inputs(&active, active.task.id(), self).await;
                self.apply_output(handler, &active, &mut event).await?;
            }
            self.release(&active);
            active.task.notify();
            return Ok(OutputAction::Stopped);
        }
        self.apply_output(handler, &active, &mut event).await?;
        Ok(OutputAction::Continue)
    }

    async fn apply_output(
        &mut self,
        handler: &DefaultRequestHandler,
        active: &Arc<ActiveExecution>,
        event: &mut StreamResponse,
    ) -> Result<(), A2AError> {
        let invocation = self
            .execution
            .bookkeeping(active)
            .expect("current invocation");
        handler
            .tasks
            .update(active, event, invocation, handler.pushes.as_deref())
            .await
    }

    pub(in crate::handler) async fn finished(
        &mut self,
        handler: &DefaultRequestHandler,
        task_id: TaskId,
        active: Arc<ActiveExecution>,
        end: ExecutionEnd,
    ) {
        if !self.execution.is_current(&active) {
            return;
        }
        if matches!(self.execution, ExecutionState::Canceling { .. }) {
            release_execution(&active, &task_id, self).await;
            active.task.notify();
            return;
        }
        match end {
            ExecutionEnd::Finished => {
                self.finish_execution(handler, &active, &task_id).await;
            }
            ExecutionEnd::Terminal(mut event) => {
                reject_unread_inputs(&active, &task_id, self).await;
                if !active.snapshot_is_terminal() {
                    if let Err(error) = self.apply_output(handler, &active, &mut event).await {
                        execution::publish_failure(handler, &active, &task_id, &error).await;
                    }
                }
                self.release(&active);
            }
            ExecutionEnd::Failed(error) => {
                reject_unread_inputs(&active, &task_id, self).await;
                execution::publish_failure(handler, &active, &task_id, &error).await;
                release_execution(&active, &task_id, self).await;
            }
            ExecutionEnd::Stopped => {
                release_execution(&active, &task_id, self).await;
            }
        }

        active.task.notify();
    }

    /// Finish this execution without replaying its inbox into another call.
    async fn finish_execution(
        &mut self,
        handler: &DefaultRequestHandler,
        active: &Arc<ActiveExecution>,
        task_id: &TaskId,
    ) {
        // A queued admission may finalize EOF before the driver's Failed
        // command arrives. Preserve an already signaled notification failure.
        let failure = active.push_failure.borrow().clone();
        if let Some(error) = failure {
            reject_unread_inputs(active, task_id, self).await;
            execution::publish_failure(handler, active, task_id, &error).await;
            self.release(active);
            return;
        }
        if let Err(error) = self.persist_deferred_task(handler, active).await {
            reject_unread_inputs(active, task_id, self).await;
            execution::publish_failure(handler, active, task_id, &error).await;
            self.release(active);
            return;
        }

        // Give every unread input an outcome before completing sending calls.
        reject_unread_inputs(active, task_id, self).await;
        if let Some(snapshot) = active
            .snapshot()
            .filter(|task| !task.status.state.is_terminal())
        {
            active.emitted_output();
            active
                .publish_kind(
                    Ok(&StreamResponse::Task(snapshot)),
                    None,
                    EventKind::InvocationEnded,
                )
                .await;
        }
        self.release(active);
    }

    async fn persist_deferred_task(
        &mut self,
        handler: &DefaultRequestHandler,
        active: &Arc<ActiveExecution>,
    ) -> Result<(), A2AError> {
        if !active.uncreated.load(std::sync::atomic::Ordering::SeqCst) {
            return Ok(());
        }
        let task = active
            .snapshot()
            .ok_or_else(|| A2AError::internal("a deferred task has no snapshot"))?;
        let task = handler.tasks.save(task, true).await?;
        active
            .uncreated
            .store(false, std::sync::atomic::Ordering::SeqCst);
        active.set_snapshot(task.clone());
        save_pending_push_config(active, handler, self).await?;
        let event = StreamResponse::Task(task.clone());
        active.publish(Ok(&event), Some(task)).await;
        Ok(())
    }

    pub(in crate::handler) async fn panicked(
        &mut self,
        handler: &DefaultRequestHandler,
        active: Arc<ActiveExecution>,
        task_id: TaskId,
    ) {
        tracing::error!(task_id = %task_id, "the agent executor panicked; failing the task");
        let registered = self.execution.is_current(&active);
        if registered && active.uncreated.load(std::sync::atomic::Ordering::SeqCst) {
            active
                .publish(
                    Err(&A2AError::internal("the agent failed unexpectedly")),
                    None,
                )
                .await;
        } else if registered
            && let Some(base) = active.snapshot()
            && !base.status.state.is_terminal()
        {
            // Unread input errors precede the terminal status which closes streams.
            reject_unread_inputs(&active, &task_id, self).await;
            let mut event = protocol::failure_event(&base, ExecutionFailure::Panic);
            if let Err(error) = handler
                .tasks
                .apply_and_publish(&active, Some(base), &mut event)
                .await
            {
                active.publish(Err(&error), None).await;
            }
        }
        // Identity-checked cleanup cannot unregister a successor invocation.
        release_execution(&active, &task_id, self).await;
        active.task.notify();
    }
}

pub(in crate::handler) enum OutputAction {
    Continue,
    Stopped,
}

#[cfg(test)]
type TestTransition =
    Box<dyn for<'a> FnOnce(&'a mut TaskController) -> futures::future::BoxFuture<'a, ()> + Send>;

/// The operations allowed to mutate one task; user execution and HTTP I/O run elsewhere.
pub(in crate::handler) enum TaskCommand {
    Admit {
        handler: DefaultRequestHandler,
        message: Box<AuthorizedMessage>,
        mode: StartMode,
        id: TaskId,
        reply: super::CommandReply<Result<Delivery, Rejection>>,
    },
    AdmitClaimed {
        handler: DefaultRequestHandler,
        message: Box<AuthorizedMessage>,
        mode: StartMode,
        id: TaskId,
        active: Arc<ActiveExecution>,
        offer: crate::executor::InputOffer,
        reply: super::CommandReply<Result<Delivery, Rejection>>,
    },
    Subscribe {
        handler: DefaultRequestHandler,
        task: super::TaskHandle,
        request: SubscribeToTaskRequest,
        policy: protocol::ResponsePolicy,
        reply: super::CommandReply<Result<super::super::response::TaskEvents, A2AError>>,
    },
    Restart {
        handler: DefaultRequestHandler,
        id: TaskId,
        reply: super::CommandReply<Result<Option<tokio::sync::oneshot::Receiver<bool>>, A2AError>>,
    },
    BeginCancel {
        handler: DefaultRequestHandler,
        params: ServiceParams,
        request: CancelTaskRequest,
        user: Option<User>,
        reply: super::CommandReply<Result<CancellationStart, A2AError>>,
    },
    CancelOutput {
        event: Box<StreamResponse>,
        reply: super::CommandReply<Result<(), A2AError>>,
    },
    CancelFinished {
        handler: DefaultRequestHandler,
        failure: Option<A2AError>,
        reply: super::CommandReply<Result<(), A2AError>>,
    },
    Output {
        handler: DefaultRequestHandler,
        active: Arc<ActiveExecution>,
        event: Box<StreamResponse>,
        last_output: bool,
        reply: super::CommandReply<Result<OutputAction, A2AError>>,
    },
    Publish {
        source: super::TaskHandle,
        event: Box<Result<StreamResponse, A2AError>>,
        snapshot: Option<Box<Task>>,
        kind: protocol::EventKind,
        failure: Option<super::FailureSignal>,
        input_generation: Option<i64>,
        published: Arc<std::sync::atomic::AtomicU64>,
        reply: super::CommandReply<()>,
    },
    Finished {
        handler: DefaultRequestHandler,
        active: Arc<ActiveExecution>,
        end: ExecutionEnd,
        reply: super::CommandReply<()>,
    },
    Panicked {
        handler: DefaultRequestHandler,
        active: Arc<ActiveExecution>,
        reply: super::CommandReply<()>,
    },
    #[cfg(test)]
    Test(TestTransition),
}

impl TaskCommand {
    pub(super) async fn run(self, state: &mut TaskController) {
        match self {
            Self::Admit {
                handler,
                message,
                mode,
                id,
                reply,
            } => reply.complete(state.admit(&handler, *message, mode, id).await),
            Self::AdmitClaimed {
                handler,
                message,
                mode,
                id,
                active,
                offer,
                reply,
            } => {
                let outcome = if state.execution.is_current(&active)
                    && !active
                        .stream_ended
                        .load(std::sync::atomic::Ordering::Acquire)
                    && !active.inputs.is_revoked()
                {
                    admission::commit_claimed(&handler, &message, &id, &active, offer).await
                } else {
                    drop(offer);
                    state.admit(&handler, *message, mode, id).await
                };
                reply.complete(outcome);
            }
            Self::Subscribe {
                handler,
                task,
                request,
                policy,
                reply,
            } => reply.complete(
                handler
                    .tasks
                    .capture_subscription(task, request, policy)
                    .await,
            ),
            Self::Restart { handler, id, reply } => {
                reply.complete(state.restart(&handler, &id).await)
            }
            Self::BeginCancel {
                handler,
                params,
                request,
                user,
                reply,
            } => reply.complete(state.begin_cancel(&handler, &params, request, user).await),
            Self::CancelOutput { event, reply } => reply.complete(state.cancel_output(*event)),
            Self::CancelFinished {
                handler,
                failure,
                reply,
            } => reply.complete(state.cancel_finished(&handler, failure).await),
            Self::Output {
                handler,
                active,
                event,
                last_output,
                reply,
            } => {
                let outcome = state.output(&handler, active.clone(), *event).await;
                // Admission runs in this mailbox too: mark EOF only once its
                // final output was applied, before the next request can run.
                if last_output && outcome.is_ok() {
                    active.mark_stream_ended();
                }
                reply.complete(outcome);
            }
            Self::Publish {
                source,
                event,
                snapshot,
                kind,
                failure,
                input_generation,
                published,
                reply,
            } => {
                let sequence = source
                    .publish_event_inner(
                        event.as_ref().as_ref(),
                        snapshot.map(|task| *task),
                        kind,
                        failure,
                        input_generation,
                        None,
                    )
                    .unwrap_or(0);
                published.store(sequence, std::sync::atomic::Ordering::Release);
                reply.complete(());
            }
            Self::Finished {
                handler,
                active,
                end,
                reply,
            } => {
                state
                    .finished(&handler, active.task.id().clone(), active, end)
                    .await;
                reply.complete(());
            }
            Self::Panicked {
                handler,
                active,
                reply,
            } => {
                state
                    .panicked(&handler, active.clone(), active.task.id().clone())
                    .await;
                reply.complete(());
            }
            #[cfg(test)]
            Self::Test(transition) => transition(state).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn terminal_turn_cleanup_rejects_unread_input_even_after_creation_fails() {
        for deferred in [false, true] {
            let handler = make_handler();
            let mut task = task_with_history("terminal", "context", 0);
            task.status.state = TaskState::Canceled;
            handler.tasks.store().create(task.clone()).await.unwrap();
            let mut controller = TaskController::default();
            let (active, _inbox) = controller.reserve(&handler, &task.id).await.unwrap();
            active.set_snapshot(task.clone());
            active
                .uncreated
                .store(deferred, std::sync::atomic::Ordering::SeqCst);
            let input = crate::ClientInput::new(make_message(), 1);
            let reference = input.message.message_id.clone();
            let queued = controller
                .execution
                .bookkeeping(&active)
                .unwrap()
                .track_input(input);
            active.inputs.sender.try_send(queued).unwrap();
            let mut events = active.subscribe();
            controller
                .finish_execution(&handler, &active, &task.id)
                .await;
            let event = events.recv().await.unwrap().result.unwrap();
            assert!(matches!(event, StreamResponse::Error(ref error)
                if error.code == "UNAVAILABLE" && error.reference_id.as_deref() == Some(reference.as_str())));
            assert!(active.inputs.closed.is_cancelled());
            assert!(matches!(controller.execution, ExecutionState::Idle));
            assert!(handler.tasks.task(&task.id).active().is_none());
            assert_eq!(
                handler.tasks.store().get(&task.id).await.unwrap().unwrap(),
                task
            );
            handler.shutdown().await;
        }
    }

    #[tokio::test]
    async fn late_driver_commands_cannot_release_or_publish_into_a_successor() {
        let handler = make_handler();
        let mut runtime = TaskController::default();
        let (old, _inputs) = runtime.reserve(&handler, "task").await.unwrap();
        runtime.release(&old);
        let (next, _inputs) = runtime.reserve(&handler, "task").await.unwrap();
        let sequence = next.sequence();
        runtime
            .finished(&handler, "task".into(), old.clone(), ExecutionEnd::Stopped)
            .await;
        let action = runtime
            .output(
                &handler,
                old,
                StreamResponse::Task(task_with_history("task", "context", 0)),
            )
            .await
            .unwrap();
        assert!(matches!(action, OutputAction::Stopped));
        assert!(runtime.execution.is_current(&next));
        assert!(Arc::ptr_eq(
            &handler.tasks.task("task").active().unwrap(),
            &next
        ));
        assert_eq!(next.sequence(), sequence);
        assert!(handler.tasks.store().get("task").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn driver_completion_preserves_cancellation_until_the_hook_finishes() {
        let handler = make_handler();
        seed_task(&handler, "task", "context").await;
        let mut runtime = TaskController::default();
        let (active, _inputs) = runtime.reserve(&handler, "task").await.unwrap();
        active.set_snapshot(handler.tasks.load("task").await.unwrap());
        let plan = runtime
            .begin_cancel(
                &handler,
                &ServiceParams::new(),
                CancelTaskRequest {
                    id: "task".into(),
                    metadata: None,
                    tenant: None,
                },
                None,
            )
            .await
            .unwrap();
        assert!(matches!(plan, CancellationStart::Started(_)));
        runtime
            .finished(&handler, "task".into(), active, ExecutionEnd::Stopped)
            .await;
        assert!(matches!(
            runtime.execution,
            ExecutionState::Canceling {
                invocation: None,
                ..
            }
        ));
        assert!(handler.tasks.task("task").active().is_none());
        runtime.cancel_finished(&handler, None).await.unwrap();
        assert!(matches!(runtime.execution, ExecutionState::Idle));
        assert_eq!(
            handler.tasks.load("task").await.unwrap().status.state,
            TaskState::Canceled
        );
    }
}

#[cfg(test)]
mod invalid_lifecycle_tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn failed_preparation_releases_the_reservation_without_starting_execution() {
        let handler = make_handler();
        let task = handler.tasks.task("missing");
        let mut controller = TaskController::default();
        let message = AuthorizedMessage {
            receipt: None,
            request: send_req(Some("missing"), None),
            params: ServiceParams::new(),
            user: None,
            response_id: None,
        };
        let error = controller
            .start(&handler, message, StartMode::Blocking, "missing".into())
            .await
            .err()
            .expect("an unknown task must reject the input");
        assert_eq!(error.into_error().code, error_code::TASK_NOT_FOUND);
        assert!(matches!(controller.execution, ExecutionState::Idle));
        assert!(task.active().is_none());
        assert!(
            handler
                .tasks
                .store()
                .get("missing")
                .await
                .unwrap()
                .is_none()
        );

        let (next, _inputs) = controller.reserve(&handler, "missing").await.unwrap();
        controller.release(&next);
    }

    #[tokio::test]
    async fn duplicate_reservation_and_unstarted_cancellation_leave_the_owner_intact() {
        let handler = make_handler();
        let mut runtime = TaskController::default();
        let (active, _inputs) = runtime.reserve(&handler, "task").await.unwrap();
        let error = runtime.reserve(&handler, "task").await.err().unwrap();
        assert_eq!(error.code, error_code::INVALID_REQUEST);
        let event = StreamResponse::Task(task_with_history("task", "context", 0));
        assert_eq!(
            runtime.cancel_output(event).unwrap_err().message,
            "no cancellation in progress"
        );
        assert_eq!(
            runtime
                .cancel_finished(&handler, None)
                .await
                .unwrap_err()
                .message,
            "no cancellation in progress"
        );
        assert!(runtime.execution.is_current(&active));
        assert!(Arc::ptr_eq(
            &handler.tasks.task("task").active().unwrap(),
            &active
        ));
        runtime.release(&active);
        assert!(handler.tasks.task("task").active().is_none());
    }
}
