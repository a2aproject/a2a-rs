// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Task admission and lifecycle decisions, owned by one coordinator mailbox.

use super::cancellation::CancellationStart;
use super::delivery::{AuthorizedMessage, Delivery, StartMode, is_artifact_only, request_task_id};
use super::events::{ActiveExecution, EventKind};
use super::execution::TurnEnd;
use super::protocol::Rejection;
use super::registry::ReservationGuard;
use super::turns::{self, reject_unread_inputs, release_execution, save_pending_push_config};
use super::updates::{
    apply_and_publish, backfill_status_timestamp, persist_event, save_task_with,
    validate_event_identity,
};
use super::{DefaultRequestHandler, admission, cancellation, execution};
use crate::User;
use crate::middleware::ServiceParams;
use a2a::*;
use std::{collections::HashMap, sync::Arc};

#[derive(Default)]
pub(super) struct TaskRuntime {
    pub(super) cancellation: Option<cancellation::Cancellation>,
    pub(super) invocations: HashMap<String, super::events::InvocationState>,
}

impl TaskRuntime {
    pub(super) async fn admit(
        &mut self,
        handler: &DefaultRequestHandler,
        message: AuthorizedMessage,
        mode: StartMode,
        reserved_id: TaskId,
    ) -> Result<Delivery, Rejection> {
        // No task named: the message creates one, and there is nothing to join.
        let Some(task_id) = request_task_id(&message.request) else {
            return self.start(handler, message, mode, reserved_id).await;
        };
        // An entry with no snapshot is a request that was dropped before it
        // began; it is no execution.
        let running = match handler.execution_manager.get(&task_id).await {
            Some(active) if active.snapshot().await.is_some() => Some(active),
            _ => None,
        };
        match running {
            Some(active) => {
                admission::inject(
                    handler,
                    &message,
                    &task_id,
                    &active,
                    self.invocations
                        .entry(active.execution_id.clone())
                        .or_default(),
                )
                .await
            }
            None => self.start(handler, message, mode, reserved_id).await,
        }
    }

    async fn start(
        &mut self,
        handler: &DefaultRequestHandler,
        message: AuthorizedMessage,
        mode: StartMode,
        reserved_id: TaskId,
    ) -> Result<Delivery, Rejection> {
        let AuthorizedMessage {
            request: req,
            params,
            user,
            ..
        } = message;
        let params = &params;
        let include_task_snapshot_in_context = mode == StartMode::Blocking;
        // A new task is created by the executor: only a live exchange, which is
        // bound to its task from the first request, creates it up front.
        let lazy = mode != StartMode::Bidi;
        let artifact_only = is_artifact_only(&req);
        let task_id = reserved_id;

        // Claim before reading storage. The coordinator completes this command
        // even after a caller disconnects; the guard cleans up a panic or error.
        let (active, inputs, invocation) = handler.execution_manager.reserve(&task_id).await?;
        self.invocations
            .insert(active.execution_id.clone(), invocation);
        let reservation = ReservationGuard::new(
            Arc::clone(&handler.execution_manager),
            task_id.clone(),
            Arc::clone(&active),
        );
        let prepared = async {
            let input = admission::prepare(handler, &req, &task_id).await?;
            input
                .commit(
                    handler,
                    params,
                    &req,
                    lazy,
                    self.invocations
                        .get_mut(&active.execution_id)
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
                self.invocations.remove(&active.execution_id);
                reservation.release().await;
                return Err(error.into());
            }
        };
        handler
            .execution_manager
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
        handler.changes.notify(&task_id);
        // Attach before acknowledging admission, so output and cancellation
        // commands queued immediately afterwards cannot be missed.
        let receiver = active.subscribe().await;

        let stored_task = (existed || include_task_snapshot_in_context).then(|| task.clone());
        let service_params = params.clone();

        reservation.disarm();
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
        execution::spawn(handler.clone(), task_id, active, exec_ctx);

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
        cancellation::Cancellation::begin(handler, params, request, user, &mut self.cancellation)
            .await
    }

    pub(super) async fn output(
        &mut self,
        handler: &DefaultRequestHandler,
        task_id: TaskId,
        active: Arc<ActiveExecution>,
        mut event: StreamResponse,
    ) -> Result<OutputAction, A2AError> {
        if active.snapshot_is_terminal().await {
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
            handler.execution_manager.finish(&task_id, &active).await;
            handler.changes.notify(&task_id);
            return Ok(OutputAction::Drain);
        }
        self.apply_output(handler, &active, &task_id, &mut event)
            .await?;
        Ok(OutputAction::Continue)
    }

    async fn apply_output(
        &mut self,
        handler: &DefaultRequestHandler,
        active: &Arc<ActiveExecution>,
        task_id: &TaskId,
        event: &mut StreamResponse,
    ) -> Result<(), A2AError> {
        backfill_status_timestamp(event);
        let base = active.snapshot().await;
        let creating = active.uncreated.load(std::sync::atomic::Ordering::SeqCst);
        let applied = persist_event(handler.task_store.as_ref(), base, event, creating).await;
        active
            .uncreated
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let (snapshot, repeated) = applied?;
        if creating {
            save_pending_push_config(active, handler, self).await;
        }
        active.publish_marked(Ok(event), snapshot, repeated).await;
        handler.changes.notify(task_id);
        Ok(())
    }

    pub(super) async fn finished(
        &mut self,
        handler: &DefaultRequestHandler,
        task_id: TaskId,
        active: Arc<ActiveExecution>,
        end: TurnEnd,
    ) {
        match end {
            TurnEnd::Finished => {
                self.end_turn(handler, &active, &task_id).await;
            }
            TurnEnd::Terminal(mut event) => {
                reject_unread_inputs(&active, &task_id, self).await;
                if !active.snapshot_is_terminal().await {
                    if let Err(error) = self
                        .apply_output(handler, &active, &task_id, &mut event)
                        .await
                    {
                        active.publish(Err(&error), None).await;
                    }
                }
                handler.execution_manager.finish(&task_id, &active).await;
            }
            TurnEnd::Failed(error) => {
                active
                    .uncreated
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                reject_unread_inputs(&active, &task_id, self).await;
                let failed = super::failure::persist_failure(handler, &active, &task_id, &error).await;
                active.publish(Err(&error), failed.clone()).await;
                if let Some(task) = failed { active.publish(Ok(&StreamResponse::Task(task)), None).await; }
                release_execution(&handler.execution_manager, &active, &task_id, self).await;
            }
            TurnEnd::Stopped => {
                release_execution(&handler.execution_manager, &active, &task_id, self).await;
            }
        }
        self.invocations.remove(&active.execution_id);
        handler.changes.notify(&task_id);
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
                match save_task_with(handler.task_store.as_ref(), task, true).await {
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
            self.invocations
                .entry(active.execution_id.clone())
                .or_default()
                .drain_inputs(&active.inputs)
        } else {
            Vec::new()
        };
        let (Some(snapshot), false) = (snapshot, unread.is_empty()) else {
            release_execution(&handler.execution_manager, active, task_id, self).await;
            return;
        };
        handler.execution_manager.finish(task_id, active).await;
        if let Err(unread) = turns::start_next_turn(handler, task_id, snapshot, unread, self).await
        {
            // Nothing could take them over: they are answered, as at any end.
            turns::reject_inputs(active, task_id, unread).await;
        }
    }

    pub(super) async fn panicked(
        &mut self,
        handler: &DefaultRequestHandler,
        active: Arc<ActiveExecution>,
        task_id: TaskId,
    ) {
        tracing::error!(task_id = %task_id, "the agent executor panicked; failing the task");
        let registered = handler
            .execution_manager
            .get(&task_id)
            .await
            .is_some_and(|current| Arc::ptr_eq(&current, &active));
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
            if let Err(error) =
                apply_and_publish(handler.task_store.as_ref(), &active, Some(base), &mut event)
                    .await
            {
                active.publish(Err(&error), None).await;
            }
        }
        // Identity-checked cleanup cannot unregister a successor invocation.
        release_execution(&handler.execution_manager, &active, &task_id, self).await;
        handler.changes.notify(&task_id);
    }
}

pub(super) enum OutputAction {
    Continue,
    Drain,
    Stopped,
}
