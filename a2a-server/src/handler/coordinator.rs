// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Owned per-task transitions. No executor, response, or webhook I/O runs here.

use super::cancellation::CancellationStart;
use a2a::*;
use futures::{FutureExt, future::BoxFuture};
use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::{mpsc, oneshot};

const COMMAND_CAPACITY: usize = 32;

type Transition = Box<dyn for<'a> FnOnce(&'a mut TaskRuntime) -> BoxFuture<'a, ()> + Send>;

pub(super) use super::runtime::TaskRuntime;

/// The operation being serialized, independent of transport or response mode.
#[derive(Clone, Copy, Debug)]
pub(super) enum TaskOperation {
    Input,
    Subscribe,
    Output,
    Finished,
    Cancel,
}

struct Command {
    operation: TaskOperation,
    transition: Transition,
}

/// A handle, not a mutex guard: dropping a caller never cancels admitted work.
pub(super) struct TaskCoordinator {
    task_id: TaskId,
    sender: mpsc::Sender<Command>,
}

impl TaskCoordinator {
    pub(super) async fn call<T: Send + 'static>(
        self: &Arc<Self>,
        operation: TaskOperation,
        transition: impl Future<Output = T> + Send + 'static,
    ) -> Result<T, A2AError> {
        self.update(operation, move |_| Box::pin(transition)).await
    }

    pub(super) async fn update<T, F>(
        self: &Arc<Self>,
        operation: TaskOperation,
        transition: F,
    ) -> Result<T, A2AError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut TaskRuntime) -> BoxFuture<'a, T> + Send + 'static,
    {
        let (reply, result) = oneshot::channel();
        let owner = self.clone();
        let transition: Transition = Box::new(move |state| {
            Box::pin(async move {
                let result = std::panic::AssertUnwindSafe(transition(state))
                    .catch_unwind()
                    .await;
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
        self.sender
            .send(Command {
                operation,
                transition,
            })
            .await
            .map_err(|_| A2AError::internal("task coordinator stopped"))?;
        result
            .await
            .map_err(|_| A2AError::internal("task transition lost its response"))?
    }

    pub(super) async fn admit(
        self: &Arc<Self>,
        handler: super::DefaultRequestHandler,
        message: super::delivery::AuthorizedMessage,
        mode: super::delivery::StartMode,
    ) -> Result<super::delivery::Delivery, super::protocol::Rejection> {
        let id = self.task_id.clone();
        self.update(TaskOperation::Input, move |state| {
            Box::pin(async move { state.admit(&handler, message, mode, id).await })
        })
        .await
        .map_err(super::protocol::Rejection::Interrupted)?
    }

    pub(super) async fn subscribe(
        self: &Arc<Self>,
        handler: super::DefaultRequestHandler,
        request: SubscribeToTaskRequest,
    ) -> Result<super::subscription::SubscriptionAttachment, A2AError> {
        self.call(TaskOperation::Subscribe, async move {
            super::subscription::SubscriptionAttachment::capture(&handler, request).await
        })
        .await?
    }

    pub(super) async fn begin_cancel(
        self: &Arc<Self>,
        handler: super::DefaultRequestHandler,
        params: crate::middleware::ServiceParams,
        request: CancelTaskRequest,
        user: Option<crate::User>,
    ) -> Result<CancellationStart, A2AError> {
        self.update(TaskOperation::Cancel, move |state| {
            Box::pin(async move { state.begin_cancel(&handler, &params, request, user).await })
        })
        .await?
    }

    pub(super) async fn cancel_update(
        self: &Arc<Self>,
        handler: super::DefaultRequestHandler,
        event: StreamResponse,
    ) -> Result<(), A2AError> {
        self.update(TaskOperation::Cancel, move |state| {
            Box::pin(async move {
                state
                    .cancellation
                    .as_mut()
                    .ok_or_else(|| A2AError::internal("no cancellation in progress"))?
                    .apply(&handler, event)
                    .await
                    .map(|_| ())
            })
        })
        .await?
    }

    pub(super) async fn cancel_finished(
        self: &Arc<Self>,
        handler: super::DefaultRequestHandler,
        failure: Option<A2AError>,
    ) -> Result<(), A2AError> {
        let id = self.task_id.clone();
        self.update(TaskOperation::Cancel, move |state| {
            Box::pin(async move {
                let cancellation = state
                    .cancellation
                    .take()
                    .ok_or_else(|| A2AError::internal("no cancellation in progress"))?;
                let active = cancellation.active.clone();
                let result = cancellation.finish(&handler, failure).await;
                handler.execution_manager.finish(&id, &active).await;
                result
            })
        })
        .await?
    }

    pub(super) async fn output(
        self: &Arc<Self>,
        handler: super::DefaultRequestHandler,
        active: Arc<super::events::ActiveExecution>,
        event: StreamResponse,
    ) -> Result<super::runtime::OutputAction, A2AError> {
        let id = self.task_id.clone();
        self.update(TaskOperation::Output, move |state| {
            Box::pin(async move { state.output(&handler, id, active, event).await })
        })
        .await?
    }

    pub(super) async fn finished(
        self: &Arc<Self>,
        handler: super::DefaultRequestHandler,
        active: Arc<super::events::ActiveExecution>,
        end: super::execution::TurnEnd,
    ) -> Result<(), A2AError> {
        let id = self.task_id.clone();
        self.update(TaskOperation::Finished, move |state| {
            Box::pin(async move { state.finished(&handler, id, active, end).await })
        })
        .await
    }

    pub(super) async fn panicked(
        self: &Arc<Self>,
        handler: super::DefaultRequestHandler,
        active: Arc<super::events::ActiveExecution>,
        id: TaskId,
    ) -> Result<(), A2AError> {
        self.update(TaskOperation::Finished, move |state| {
            Box::pin(async move { state.panicked(&handler, active, id).await })
        })
        .await
    }
}

/// Only routing is synchronized. Mailboxes have weak entries and retire when
/// no request or executor retains a handle; idle stored Tasks need no worker.
#[derive(Default)]
pub(super) struct TaskCoordinators {
    tasks: Mutex<HashMap<TaskId, Weak<TaskCoordinator>>>,
}

impl TaskCoordinators {
    pub(super) fn for_task(&self, task_id: &str) -> Arc<TaskCoordinator> {
        let mut tasks = self
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(task) = tasks.get(task_id).and_then(Weak::upgrade) {
            return task;
        }
        tasks.retain(|_, task| task.strong_count() > 0);
        let (sender, mut commands) = mpsc::channel::<Command>(COMMAND_CAPACITY);
        let task = Arc::new(TaskCoordinator {
            task_id: task_id.to_owned(),
            sender,
        });
        tasks.insert(task_id.to_owned(), Arc::downgrade(&task));
        let task_id = task_id.to_owned();
        tokio::spawn(async move {
            let mut state = TaskRuntime::default();
            while let Some(command) = commands.recv().await {
                tracing::trace!(%task_id, operation = ?command.operation, "processing task transition");
                (command.transition)(&mut state).await;
            }
        });
        task
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn admitted_work_finishes_after_its_caller_disconnects_and_precedes_later_commands() {
        let registry = TaskCoordinators::default();
        let task = registry.for_task("task");
        let value = Arc::new(AtomicUsize::new(0));
        let (entered, began) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let caller = tokio::spawn({
            let task = task.clone();
            let value = value.clone();
            async move {
                task.call(TaskOperation::Input, async move {
                    let _ = entered.send(());
                    let _ = released.await;
                    value.store(1, Ordering::SeqCst);
                })
                .await
                .unwrap();
            }
        });
        began.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert!(Arc::ptr_eq(&task, &registry.for_task("task")));
        let mut subscriber = tokio::spawn({
            let task = task.clone();
            let value = value.clone();
            async move {
                task.call(TaskOperation::Subscribe, async move {
                    value.load(Ordering::SeqCst)
                })
                .await
                .unwrap()
            }
        });
        assert!(
            timeout(Duration::from_millis(10), &mut subscriber)
                .await
                .is_err()
        );
        release.send(()).unwrap();
        assert_eq!(
            timeout(Duration::from_secs(2), subscriber)
                .await
                .unwrap()
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn one_tasks_persistence_does_not_block_another_task_and_panics_do_not_kill_the_mailbox()
    {
        let registry = TaskCoordinators::default();
        let slow = registry.for_task("slow");
        let fast = registry.for_task("fast");
        let (entered, began) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let pending = tokio::spawn({
            let slow = slow.clone();
            async move {
                slow.call(TaskOperation::Input, async move {
                    entered.send(()).unwrap();
                    released.await.unwrap();
                })
                .await
                .unwrap()
            }
        });
        began.await.unwrap();
        assert_eq!(
            timeout(
                Duration::from_secs(1),
                fast.call(TaskOperation::Input, async { 7 })
            )
            .await
            .unwrap()
            .unwrap(),
            7
        );
        let error = fast
            .call(TaskOperation::Output, async { panic!("store panicked") })
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::INTERNAL_ERROR);
        assert_eq!(
            fast.call(TaskOperation::Subscribe, async { 8 })
                .await
                .unwrap(),
            8
        );
        release.send(()).unwrap();
        pending.await.unwrap();
    }

    #[tokio::test]
    async fn idle_mailboxes_retire_and_the_registry_recovers_from_poison() {
        let registry = TaskCoordinators::default();
        let task = registry.for_task("task");
        let weak = Arc::downgrade(&task);
        drop(task);
        assert!(weak.upgrade().is_none());
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _tasks = registry.tasks.lock().unwrap();
                panic!("interrupted routing");
            }))
            .is_err()
        );
        let next = registry.for_task("next");
        assert_eq!(
            next.call(TaskOperation::Input, async { 9 }).await.unwrap(),
            9
        );
        let tasks = registry
            .tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!tasks.contains_key("task"));
    }
    #[tokio::test]
    async fn completing_an_invocation_releases_its_coordinator_lease() {
        use crate::handler::test_support::*;
        let (handler, release) = make_resumable_handler();
        let mut events = handler
            .send_streaming_message(&ServiceParams::new(), send_req(None, None))
            .await
            .unwrap();
        let Some(Ok(StreamResponse::Task(task))) = events.next().await else {
            panic!("the executor announces its Task");
        };
        let coordinator = handler.execution_manager.coordinators.for_task(&task.id);
        let weak = Arc::downgrade(&coordinator);
        drop(coordinator);
        assert!(
            weak.upgrade().is_some(),
            "the running invocation retains its mailbox"
        );
        release.notify_one();
        let output = timeout(Duration::from_secs(2), events.collect::<Vec<_>>())
            .await
            .unwrap();
        assert!(output.iter().any(|event| matches!(
            event, Ok(StreamResponse::Task(task)) if task.status.state == TaskState::Completed
        )));
        timeout(Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("finished invocations must not retain a coordinator cycle");
    }
}
