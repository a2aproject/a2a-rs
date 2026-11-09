// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Cancellation progress owned by the task runtime, never shared by hooks.

use super::DefaultRequestHandler;
use super::events::ActiveExecution;
use super::protocol::{self, CancelEntry, CancellationNotification, is_terminal_event};
use super::task_handle::{TaskCommand, TaskOperation};
use crate::middleware::ServiceParams;
use crate::{ClientInputs, User};
use a2a::*;
use futures::StreamExt;
use std::sync::Arc;

/// Cancellation hooks run outside the mailbox, after the store transition.
pub(super) enum CancellationStart {
    Unchanged(Box<Task>),
    Started(Box<CancellationHook>),
}

pub(super) struct CancellationHook {
    pub(super) active: Option<Arc<super::events::ActiveExecution>>,
    pub(super) context: crate::ExecutorContext,
}

pub(super) struct Cancellation {
    pub(super) active: Arc<ActiveExecution>,
    entry: CancelEntry,
    signal_failure: bool,
}

impl Cancellation {
    pub(super) async fn begin(
        handler: &DefaultRequestHandler,
        params: &ServiceParams,
        req: CancelTaskRequest,
        user: Option<User>,
        active_execution: Option<Arc<ActiveExecution>>,
        slot: &mut Option<Self>,
    ) -> Result<CancellationStart, A2AError> {
        let handle = handler.tasks.task(&req.id);
        // State transition and snapshot publication are serialized with input
        // and executor output. The hook is scheduled only after this command.
        let task = handler.tasks.load(&req.id).await?;
        let generation_before = task.generation;

        if !protocol::cancel_requires_transition(&task)? {
            return Ok(CancellationStart::Unchanged(Box::new(task)));
        }

        // Atomically transition the task to CANCELED so two concurrent cancel
        // requests cannot both pass the check-then-act window.
        let task = match handler.tasks.mark_canceled(&req.id).await {
            Ok(task) => task,
            Err(error) if error.code == error_code::TASK_NOT_CANCELABLE => {
                // Raced with another cancel or with task completion. Keep the
                // same semantics as the upfront check: a task another cancel
                // already moved to CANCELED is returned (idempotent); any
                // other terminal state is still not cancelable.
                let raced = handler.tasks.load(&req.id).await?;
                if matches!(protocol::cancel_requires_transition(&raced), Ok(false)) {
                    return Ok(CancellationStart::Unchanged(Box::new(raced)));
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };
        if let Some(active) = &active_execution {
            // Wake the driver. Its completion command returns unread inputs
            // before the hook publishes the cancellation event.
            active.set_snapshot(task.clone());
            active.inputs.close();
        }
        // The task changed under every long-poll waiting on it.
        handle.notify();

        let cancel_inputs = ClientInputs::closed();
        let exec_ctx = crate::ExecutorContext {
            execution_id: active_execution.as_ref().map_or_else(
                || uuid::Uuid::now_v7().to_string(),
                |active| active.execution_id.clone(),
            ),
            message: None,
            task_id: req.id.clone(),
            stored_task: Some(task.clone()),
            context_id: task.context_id.clone(),
            metadata: req.metadata,
            user,
            service_params: params.clone(),
            tenant: req.tenant,
            inputs: cancel_inputs,
            new_task: false,
            task_snapshot: None,
        };
        // A store that advanced the generation recorded the transition ithandler.
        let recorded_by_store = task.generation > generation_before;

        let control = match &active_execution {
            Some(active) => active.clone(),
            None => {
                let (active, _inputs, _state) = ActiveExecution::with_task(handle);
                active.set_snapshot(task.clone());
                active
            }
        };
        handler.attach_pushes(&control.task);
        *slot = Some(Self::new(
            control,
            task,
            recorded_by_store,
            active_execution.is_some(),
        ));
        Ok(CancellationStart::Started(Box::new(CancellationHook {
            active: active_execution,
            context: exec_ctx,
        })))
    }

    pub(super) fn new(
        active: Arc<ActiveExecution>,
        canceled: Task,
        recorded_by_store: bool,
        signal_failure: bool,
    ) -> Self {
        let entry = CancelEntry {
            recorded_by_store,
            generation: canceled.generation,
            announced: false,
        };
        Self {
            active,
            entry,
            signal_failure,
        }
    }

    /// Hook results are validated acknowledgements, never task mutations.
    pub(super) fn acknowledge(&self, mut event: StreamResponse) -> Result<(), A2AError> {
        let base = self
            .active
            .snapshot()
            .ok_or_else(|| A2AError::internal("cancellation has no task"))?;
        protocol::validate_cancel_acknowledgement(&base, &mut event)
    }

    pub(super) async fn finish(
        mut self,
        handler: &DefaultRequestHandler,
        failure: Option<A2AError>,
    ) -> Result<(), A2AError> {
        let recorded = self.record_if_missing(handler).await;
        if let Some(error) = failure {
            if self.signal_failure {
                self.active.publish(Err(&error), None).await;
            }
            return Err(error);
        }
        recorded
    }

    pub(super) async fn record_if_missing(
        &mut self,
        handler: &DefaultRequestHandler,
    ) -> Result<(), A2AError> {
        match self.entry.notification(self.active.snapshot()) {
            CancellationNotification::None => {}
            CancellationNotification::Publish(event) => self.active.publish(Ok(&event), None).await,
            CancellationNotification::Persist { task, mut event } => {
                handler
                    .tasks
                    .apply_and_publish(&self.active, Some(*task), &mut event)
                    .await?;
            }
        }

        self.entry.announced = true;
        Ok(())
    }
}

/// The cancellation worker owns the workflow; it never polls a hook inside
/// a task operation, so input cleanup and other operations can progress.
pub(super) async fn cancel(
    handler: DefaultRequestHandler,
    params: ServiceParams,
    request: CancelTaskRequest,
    user: Option<User>,
) -> Result<Task, A2AError> {
    let task_id = request.id.clone();
    let plan = match handler
        .tasks
        .task(&task_id)
        .request(TaskOperation::Cancel, |reply| TaskCommand::BeginCancel {
            handler: handler.clone(),
            params,
            request,
            user,
            reply,
        })
        .await??
    {
        CancellationStart::Unchanged(task) => return Ok(*task),
        CancellationStart::Started(plan) => *plan,
    };
    if let Some(active) = &plan.active {
        active.inputs_drained.cancelled().await;
    }
    run(handler.clone(), plan.context).await?;
    handler.tasks.load(&task_id).await
}

pub(super) async fn run(
    handler: DefaultRequestHandler,
    context: crate::ExecutorContext,
) -> Result<(), A2AError> {
    use futures::FutureExt;
    let deadline = tokio::time::Instant::now() + handler.cancel_hook_timeout;
    let task_id = context.task_id.clone();
    let hook = async {
        let mut stream = handler.tasks.executor.cancel(context);
        loop {
            let event = match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(event)) => event?,
                Ok(None) => return Ok(()),
                Err(_) => return Err(A2AError::internal("agent cancellation hook timed out")),
            };
            let terminal = is_terminal_event(&event);
            handler
                .tasks
                .task(&task_id)
                .request(TaskOperation::Cancel, |reply| TaskCommand::CancelOutput {
                    event: Box::new(event),
                    reply,
                })
                .await??;
            if terminal {
                return Ok(());
            }
        }
    };
    // Even a panicking hook must leave a recorded cancellation transition.
    let failure = match std::panic::AssertUnwindSafe(hook).catch_unwind().await {
        Ok(result) => result.err(),
        Err(_) => Some(A2AError::internal("agent cancellation hook panicked")),
    };
    handler
        .tasks
        .task(&task_id)
        .request(TaskOperation::Cancel, |reply| TaskCommand::CancelFinished {
            handler: handler.clone(),
            failure,
            reply,
        })
        .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    struct PanickingHook;
    impl crate::AgentExecutor for PanickingHook {
        fn execute(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            panic!("private agent failure")
        }
    }

    #[tokio::test]
    async fn a_panicking_hook_still_records_cancellation_and_returns_a_sanitized_error() {
        let handler = DefaultRequestHandler::new(PanickingHook, InMemoryTaskStore::new());
        seed_task(&handler, "task", "context").await;
        // Cancellation also works after an invocation returns without completing its Task.
        let mut request = send_request();
        request.message.task_id = Some("task".into());
        request.configuration = Some(SendMessageConfiguration {
            return_immediately: Some(true),
            ..serde_json::from_value(serde_json::json!({})).unwrap()
        });
        let response = handler
            .send_message(&ServiceParams::new(), request)
            .await
            .unwrap();
        assert!(matches!(response, SendMessageResponse::Task(_)));
        timeout(Duration::from_secs(2), async {
            while handler.tasks.task("task").active().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let error = timeout(
            Duration::from_secs(2),
            handler.cancel_task(
                &ServiceParams::new(),
                CancelTaskRequest {
                    id: "task".into(),
                    metadata: None,
                    tenant: None,
                },
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error.code, error_code::INTERNAL_ERROR);
        assert_eq!(error.message, "agent cancellation hook panicked");
        let task = handler.tasks.store().get("task").await.unwrap().unwrap();
        assert_eq!(task.status.state, TaskState::Canceled);
        assert!(task.timeline.unwrap().iter().any(|entry| matches!(
            &entry.content, TimelineContent::Status(status) if status.state == TaskState::Canceled
        )));
        assert!(handler.tasks.task("task").active().is_none());
    }

    struct GatedHook {
        entered: Arc<Notify>,
        release: Arc<Notify>,
    }
    impl crate::AgentExecutor for GatedHook {
        fn execute(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let entered = self.entered.clone();
            let release = self.release.clone();
            stream::once(async move {
                entered.notify_one();
                release.notified().await;
                stream::empty::<Result<StreamResponse, A2AError>>()
            })
            .flatten()
            .boxed()
        }
    }

    #[tokio::test]
    async fn a_waiting_cancel_hook_does_not_occupy_its_tasks_mailbox() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let handler = DefaultRequestHandler::new(
            GatedHook {
                entered: entered.clone(),
                release: release.clone(),
            },
            InMemoryTaskStore::new(),
        );
        seed_task(&handler, "task", "context").await;
        // Cancellation also works after an invocation returns without completing its Task.
        let mut request = send_request();
        request.message.task_id = Some("task".into());
        request.configuration = Some(SendMessageConfiguration {
            return_immediately: Some(true),
            ..serde_json::from_value(serde_json::json!({})).unwrap()
        });
        let response = handler
            .send_message(&ServiceParams::new(), request)
            .await
            .unwrap();
        assert!(matches!(response, SendMessageResponse::Task(_)));
        timeout(Duration::from_secs(2), async {
            while handler.tasks.task("task").active().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        let cancel = || CancelTaskRequest {
            id: "task".into(),
            metadata: None,
            tenant: None,
        };
        let first = tokio::spawn({
            let handler = handler.clone();
            let request = cancel();
            async move { handler.cancel_task(&ServiceParams::new(), request).await }
        });
        timeout(Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        let second = timeout(
            Duration::from_secs(1),
            handler.cancel_task(&ServiceParams::new(), cancel()),
        )
        .await
        .expect("the first hook must not block a later command")
        .unwrap();
        assert_eq!(second.status.state, TaskState::Canceled);
        release.notify_one();
        let first = timeout(Duration::from_secs(2), first)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first, second);
    }
}
