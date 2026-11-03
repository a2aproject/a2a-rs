// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Task admission and lifecycle decisions, owned by one task queue.

use super::cancellation::CancellationStart;
use super::delivery::{AuthorizedMessage, Delivery, StartMode, is_artifact_only, request_task_id};
use super::events::{ActiveExecution, EventKind};
use super::execution::{
    self, ExecutionState, Invocation, TurnEnd, reject_unread_inputs, release_execution,
    save_pending_push_config,
};
use super::protocol::Rejection;
use super::updates::validate_event_identity;
use super::{DefaultRequestHandler, admission, cancellation};
use crate::User;
use crate::middleware::ServiceParams;
use a2a::*;
use std::sync::Arc;

const TASK_QUEUE_CAPACITY: usize = 32;
pub(super) type TaskTransition =
    Box<dyn for<'a> FnOnce(&'a mut TaskRuntime) -> futures::future::BoxFuture<'a, ()> + Send>;
pub(super) type TaskSender = tokio::sync::mpsc::Sender<TaskTransition>;

#[derive(Clone, Copy, Debug)]
pub(super) enum TaskOperation {
    Input,
    Subscribe,
    Output,
    Finished,
    Cancel,
}

pub(super) fn spawn_task(task_id: TaskId) -> Arc<TaskSender> {
    let (sender, mut commands) = tokio::sync::mpsc::channel::<TaskTransition>(TASK_QUEUE_CAPACITY);
    tokio::spawn(async move {
        let mut state = TaskRuntime::default();
        while let Some(transition) = commands.recv().await {
            tracing::trace!(%task_id, "processing task operation");
            transition(&mut state).await;
        }
    });
    Arc::new(sender)
}

/// Once enqueued, the operation owns its reply and queue lease independently of its caller.
pub(super) async fn submit<T, F>(
    task: Arc<TaskSender>,
    operation: TaskOperation,
    transition: F,
) -> Result<T, A2AError>
where
    T: Send + 'static,
    F: for<'a> FnOnce(&'a mut TaskRuntime) -> futures::future::BoxFuture<'a, T> + Send + 'static,
{
    use futures::FutureExt;
    let (reply, result) = tokio::sync::oneshot::channel();
    let owner = task.clone();
    let work: TaskTransition = Box::new(move |state| {
        Box::pin(async move {
            let result = std::panic::AssertUnwindSafe(async { transition(state).await })
                .catch_unwind()
                .await;
            if result.is_err() {
                state.recover_preparation();
            }
            let result = result.map_err(|_| {
                A2AError::internal(match operation {
                    TaskOperation::Input => "message preparation failed unexpectedly",
                    _ => "task transition failed unexpectedly",
                })
            });
            let _ = reply.send(result);
            drop(owner);
        })
    });
    task.send(work)
        .await
        .map_err(|_| A2AError::internal("task operation queue closed"))?;
    result
        .await
        .map_err(|_| A2AError::internal("task operation lost its response"))?
}

#[derive(Default)]
pub(super) struct TaskRuntime {
    pub(super) execution: ExecutionState,
}

impl TaskRuntime {
    pub(super) async fn admit(
        &mut self,
        handler: &DefaultRequestHandler,
        message: AuthorizedMessage,
        mode: StartMode,
        reserved_id: TaskId,
    ) -> Result<Delivery, Rejection> {
        if let (Some(task_id), ExecutionState::Running(invocation)) =
            (request_task_id(&message.request), &mut self.execution)
        {
            return admission::inject_input(
                handler,
                &message,
                &task_id,
                &invocation.active,
                &mut invocation.bookkeeping,
            )
            .await;
        }
        if matches!(self.execution, ExecutionState::Canceling { .. }) {
            return Err(A2AError::unsupported_operation(format!(
                "task {reserved_id} has already reached a terminal state"
            ))
            .into());
        }
        self.start(handler, message, mode, reserved_id).await
    }

    pub(super) async fn reserve(
        &mut self,
        handler: &DefaultRequestHandler,
        task_id: &str,
    ) -> Result<(Arc<ActiveExecution>, crate::ClientInputs), A2AError> {
        if !matches!(self.execution, ExecutionState::Idle) {
            return Err(A2AError::invalid_request(
                "task execution is already in progress",
            ));
        }
        let (active, inputs, bookkeeping) =
            ActiveExecution::with_events(handler.tasks.registry.events_for(task_id));
        handler.tasks.registry.register(task_id, &active).await?;
        self.execution = ExecutionState::Running(Invocation {
            active: active.clone(),
            bookkeeping,
        });
        Ok((active, inputs))
    }

    pub(super) async fn release(
        &mut self,
        handler: &DefaultRequestHandler,
        task_id: &str,
        active: &Arc<ActiveExecution>,
    ) {
        self.execution.release(active);
        handler.tasks.registry.unregister(task_id, active).await;
    }

    /// A panicking preparation has not started a driver to clean up its inbox.
    pub(super) fn recover_preparation(&mut self) {
        if let ExecutionState::Running(invocation) = &self.execution
            && !invocation.active.is_prepared()
        {
            invocation.active.inputs.close();
            self.execution = ExecutionState::Idle;
        }
    }

    async fn start(
        &mut self,
        handler: &DefaultRequestHandler,
        message: AuthorizedMessage,
        mode: StartMode,
        reserved_id: TaskId,
    ) -> Result<Delivery, Rejection> {
        let req = &message.request;
        let include_task_snapshot_in_context = mode == StartMode::Blocking;
        // A new task is created by the executor: only a live exchange, which is
        // bound to its task from the first request, creates it up front.
        let lazy = mode != StartMode::Bidi;
        let artifact_only = is_artifact_only(req);
        let task_id = reserved_id;

        // The mailbox owns preparation through completion or panic recovery.
        let (active, inputs) = self.reserve(handler, &task_id).await?;
        let prepared = async {
            let input = admission::prepare(handler, req, &task_id).await?;
            handler
                .tasks
                .commit_input(
                    &input,
                    handler,
                    &message,
                    lazy,
                    self.execution
                        .bookkeeping(&active)
                        .expect("reserved invocation"),
                )
                .await?;
            active.set_snapshot(input.task.clone()).await;
            active
                .uncreated
                .store(lazy && !input.existed, std::sync::atomic::Ordering::SeqCst);
            Ok::<_, A2AError>(input)
        }
        .await;
        let admission::PreparedInput {
            task,
            existed,
            events: input_events,
        } = match prepared {
            Ok(input) => input,
            Err(error) => {
                self.release(handler, &task_id, &active).await;
                return Err(error.into());
            }
        };
        let AuthorizedMessage {
            request: req,
            params,
            user,
            ..
        } = message;
        let params = &params;
        handler
            .tasks
            .pushes
            .attach(handler, &task_id, &active.events);
        if existed || !lazy {
            // Existing subscribers and webhooks observe persisted client input
            // through the same publication path as executor output. The new
            // response subscribes below and announces its input via its opening
            // snapshot (or recorded follow-up), avoiding duplicate generations.
            for event in &input_events {
                active.publish(Ok(event), None).await;
            }
        }
        handler.tasks.changes.notify(&task_id);
        // Attach before acknowledging admission, so output and cancellation
        // commands queued immediately afterwards cannot be missed.
        let receiver = active.subscribe().await;

        let stored_task = (existed || include_task_snapshot_in_context).then(|| task.clone());
        let service_params = params.clone();

        let exec_ctx = crate::ExecutorContext {
            execution_id: active.execution_id.clone(),
            message: (!artifact_only).then_some(req.message),
            task_id: task_id.clone(),
            // The task as it stands once the request is applied. A task this
            // request created is withheld unless the caller asked for it
            // (`send_message` does, streaming does not): executors use its
            // absence to recognise a new task.
            stored_task,
            context_id: task.context_id.clone(),
            metadata: req.metadata,
            user,
            service_params,
            tenant: req.tenant,
            inputs,
            new_task: !existed,
        };
        handler
            .tasks
            .spawn_execution(handler.clone(), task_id, active, exec_ctx);

        Ok(Delivery::Started {
            task,
            receiver,
            recorded: input_events,
        })
    }

    pub(super) async fn begin_cancel(
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

    pub(super) async fn cancel_update(
        &mut self,
        handler: &DefaultRequestHandler,
        event: StreamResponse,
    ) -> Result<(), A2AError> {
        let ExecutionState::Canceling { progress, .. } = &mut self.execution else {
            return Err(A2AError::internal("no cancellation in progress"));
        };
        progress.apply(handler, event).await.map(|_| ())
    }

    pub(super) async fn cancel_finished(
        &mut self,
        handler: &DefaultRequestHandler,
        task_id: &str,
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
        handler.tasks.registry.unregister(task_id, &active).await;
        result
    }

    pub(super) async fn output(
        &mut self,
        handler: &DefaultRequestHandler,
        task_id: TaskId,
        active: Arc<ActiveExecution>,
        mut event: StreamResponse,
    ) -> Result<OutputAction, A2AError> {
        if !self.execution.is_current(&active)
            || matches!(self.execution, ExecutionState::Canceling { .. })
            || active.snapshot_is_terminal().await
        {
            return Ok(OutputAction::Stopped);
        }
        if active.uncreated.load(std::sync::atomic::Ordering::SeqCst)
            && matches!(event, StreamResponse::Message(_))
        {
            if let Some(task) = active.snapshot().await {
                validate_event_identity(&task, &event)?;
            }
            active
                .publish_kind(Ok(&event), None, EventKind::Reply)
                .await;
            self.release(handler, &task_id, &active).await;
            handler.tasks.changes.notify(&task_id);
            return Ok(OutputAction::Drain);
        }
        self.apply_output(handler, &task_id, &active, &mut event)
            .await?;
        Ok(OutputAction::Continue)
    }

    async fn apply_output(
        &mut self,
        handler: &DefaultRequestHandler,
        task_id: &str,
        active: &Arc<ActiveExecution>,
        event: &mut StreamResponse,
    ) -> Result<(), A2AError> {
        let invocation = self
            .execution
            .bookkeeping(active)
            .expect("current invocation");
        handler
            .tasks
            .update(
                task_id,
                active,
                event,
                invocation,
                handler.push_config_store.as_deref(),
            )
            .await
    }

    pub(super) async fn finished(
        &mut self,
        handler: &DefaultRequestHandler,
        task_id: TaskId,
        active: Arc<ActiveExecution>,
        end: TurnEnd,
    ) {
        if !self.execution.is_current(&active) {
            return;
        }
        if matches!(self.execution, ExecutionState::Canceling { .. }) {
            release_execution(handler, &active, &task_id, self).await;
            handler.tasks.changes.notify(&task_id);
            return;
        }
        match end {
            TurnEnd::Finished => {
                self.end_turn(handler, &active, &task_id).await;
            }
            TurnEnd::Terminal(mut event) => {
                reject_unread_inputs(&active, &task_id, self).await;
                if !active.snapshot_is_terminal().await {
                    if let Err(error) = self
                        .apply_output(handler, &task_id, &active, &mut event)
                        .await
                    {
                        active.publish(Err(&error), None).await;
                    }
                }
                self.release(handler, &task_id, &active).await;
            }
            TurnEnd::Failed(error) => {
                active
                    .uncreated
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                reject_unread_inputs(&active, &task_id, self).await;
                let failed =
                    super::execution::persist_failure(handler, &active, &task_id, &error).await;
                active.publish(Err(&error), failed.clone()).await;
                if let Some(task) = failed {
                    active.publish(Ok(&StreamResponse::Task(task)), None).await;
                }
                release_execution(handler, &active, &task_id, self).await;
            }
            TurnEnd::Stopped => {
                release_execution(handler, &active, &task_id, self).await;
            }
        }

        handler.tasks.changes.notify(&task_id);
    }

    /// Keep a nonterminal Task and hand unread accepted input to a new invocation.
    async fn end_turn(
        &mut self,
        handler: &DefaultRequestHandler,
        active: &Arc<ActiveExecution>,
        task_id: &TaskId,
    ) {
        if active
            .uncreated
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            if let Some(task) = active.snapshot().await {
                match handler.tasks.save(task, true).await {
                    Ok(_) => save_pending_push_config(active, handler, self).await,
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
            self.execution
                .bookkeeping(active)
                .map(|invocation| invocation.drain_inputs(&active.inputs))
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        let (Some(snapshot), false) = (snapshot, unread.is_empty()) else {
            release_execution(handler, active, task_id, self).await;
            return;
        };
        self.release(handler, task_id, active).await;
        if let Err(unread) =
            execution::start_next_turn(handler, task_id, snapshot, unread, self).await
        {
            // Nothing could take them over: they are answered, as at any end.
            execution::reject_inputs(active, task_id, unread).await;
        }
    }

    pub(super) async fn panicked(
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
            && let Some(base) = active.snapshot().await
            && !base.status.state.is_terminal()
        {
            // Unread input errors precede the terminal status which closes streams.
            reject_unread_inputs(&active, &task_id, self).await;
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
            if let Err(error) = handler
                .tasks
                .apply_and_publish(&active, Some(base), &mut event)
                .await
            {
                active.publish(Err(&error), None).await;
            }
        }
        // Identity-checked cleanup cannot unregister a successor invocation.
        release_execution(handler, &active, &task_id, self).await;
        handler.tasks.changes.notify(&task_id);
    }
}

pub(super) enum OutputAction {
    Continue,
    Drain,
    Stopped,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn late_driver_commands_cannot_release_or_publish_into_a_successor() {
        let handler = make_handler();
        let mut runtime = TaskRuntime::default();
        let (old, _inputs) = runtime.reserve(&handler, "task").await.unwrap();
        runtime.release(&handler, "task", &old).await;
        let (next, _inputs) = runtime.reserve(&handler, "task").await.unwrap();
        let sequence = next.sequence().await;
        runtime
            .finished(&handler, "task".into(), old.clone(), TurnEnd::Stopped)
            .await;
        let action = runtime
            .output(
                &handler,
                "task".into(),
                old,
                StreamResponse::Task(task_with_history("task", "context", 0)),
            )
            .await
            .unwrap();
        assert!(matches!(action, OutputAction::Stopped));
        assert!(runtime.execution.is_current(&next));
        assert!(Arc::ptr_eq(
            &handler.tasks.registry.get("task").await.unwrap(),
            &next
        ));
        assert_eq!(next.sequence().await, sequence);
        assert!(handler.tasks.store().get("task").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn driver_completion_preserves_cancellation_until_the_hook_finishes() {
        let handler = make_handler();
        seed_task(&handler, "task", "context").await;
        let mut runtime = TaskRuntime::default();
        let (active, _inputs) = runtime.reserve(&handler, "task").await.unwrap();
        active
            .set_snapshot(handler.tasks.load("task").await.unwrap())
            .await;
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
            .finished(&handler, "task".into(), active, TurnEnd::Stopped)
            .await;
        assert!(matches!(
            runtime.execution,
            ExecutionState::Canceling {
                invocation: None,
                ..
            }
        ));
        assert!(handler.tasks.registry.get("task").await.is_none());
        runtime
            .cancel_finished(&handler, "task", None)
            .await
            .unwrap();
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
    async fn duplicate_reservation_and_unstarted_cancellation_leave_the_owner_intact() {
        let handler = make_handler();
        let mut runtime = TaskRuntime::default();
        let (active, _inputs) = runtime.reserve(&handler, "task").await.unwrap();
        let error = runtime.reserve(&handler, "task").await.err().unwrap();
        assert_eq!(error.code, error_code::INVALID_REQUEST);
        let event = StreamResponse::Task(task_with_history("task", "context", 0));
        assert_eq!(
            runtime
                .cancel_update(&handler, event)
                .await
                .unwrap_err()
                .message,
            "no cancellation in progress"
        );
        assert_eq!(
            runtime
                .cancel_finished(&handler, "task", None)
                .await
                .unwrap_err()
                .message,
            "no cancellation in progress"
        );
        assert!(runtime.execution.is_current(&active));
        assert!(Arc::ptr_eq(
            &handler.tasks.registry.get("task").await.unwrap(),
            &active
        ));
        runtime.release(&handler, "task", &active).await;
        assert!(handler.tasks.registry.get("task").await.is_none());
    }
}
