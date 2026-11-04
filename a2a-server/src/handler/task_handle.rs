// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! In-memory channels and execution routing owned by one task handle.

use super::events::{ActiveExecution, EventKind, ExecutionEvent, ExecutionReceiver, FailureSignal};
use a2a::*;
use parking_lot::RwLock;
use std::sync::{Arc, Weak};
use tokio::sync::{broadcast, watch};

mod controller;
mod prepared_execution;

pub(super) use controller::{OutputAction, TaskController};

const TASK_QUEUE_CAPACITY: usize = 32;
type TaskTransition =
    Box<dyn for<'a> FnOnce(&'a mut TaskController) -> futures::future::BoxFuture<'a, ()> + Send>;
pub(super) type TaskSender = tokio::sync::mpsc::Sender<TaskTransition>;

#[derive(Clone, Copy, Debug)]
pub(super) enum TaskOperation {
    Input,
    Subscribe,
    Output,
    Finished,
    Cancel,
}

#[derive(Default)]
struct State {
    // The worker and driver own these lifetimes. Strong back-references here
    // would keep an idle worker or finished execution alive indefinitely.
    operations: Weak<TaskSender>,
    execution: Weak<ActiveExecution>,
    sequence: u64,
    snapshot: Option<Task>,
    last_error: Option<(u64, A2AError)>,
}

/// Persisted Task data stays in TaskStore. This handle owns its process-local
/// channels; receivers and active workers retain it across executor invocations.
#[derive(Clone)]
pub(super) struct TaskHandle {
    inner: Arc<TaskInner>,
}

/// Weak routing reference that does not retain a task's channels or snapshot.
pub(super) struct WeakTaskHandle {
    inner: Weak<TaskInner>,
}

impl WeakTaskHandle {
    pub(super) fn upgrade(&self) -> Option<TaskHandle> {
        self.inner.upgrade().map(|inner| TaskHandle { inner })
    }

    pub(super) fn is_alive(&self) -> bool {
        self.inner.strong_count() > 0
    }
}

struct TaskInner {
    id: TaskId,
    background_tasks: Arc<super::background_tasks::BackgroundTasks>,
    changes: watch::Sender<u64>,
    sender: broadcast::Sender<ExecutionEvent>,
    state: RwLock<State>,
    notifications: std::sync::OnceLock<Arc<super::push_delivery::PushService>>,
    webhook_subscribed: std::sync::Mutex<super::push_delivery::SubscriptionState>,
}

impl TaskHandle {
    #[cfg(test)]
    pub(super) fn new(id: TaskId, snapshot: Option<Task>) -> Self {
        Self::with_background_tasks(id, snapshot, Arc::default())
    }

    pub(super) fn with_background_tasks(
        id: TaskId,
        snapshot: Option<Task>,
        background_tasks: Arc<super::background_tasks::BackgroundTasks>,
    ) -> Self {
        Self {
            inner: Arc::new(TaskInner {
                id,
                background_tasks,
                changes: watch::channel(0).0,
                sender: broadcast::channel(super::EXECUTION_BUFFER_CAPACITY).0,
                state: RwLock::new(State {
                    snapshot,
                    ..State::default()
                }),
                notifications: std::sync::OnceLock::new(),
                webhook_subscribed: std::sync::Mutex::default(),
            }),
        }
    }

    pub(super) fn downgrade(&self) -> WeakTaskHandle {
        WeakTaskHandle {
            inner: Arc::downgrade(&self.inner),
        }
    }

    #[cfg(test)]
    pub(super) fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    pub(super) fn id(&self) -> &TaskId {
        &self.inner.id
    }

    pub(super) fn background_tasks(&self) -> &Arc<super::background_tasks::BackgroundTasks> {
        &self.inner.background_tasks
    }

    pub(super) fn sender(&self) -> &broadcast::Sender<ExecutionEvent> {
        &self.inner.sender
    }

    pub(super) fn pushes(&self) -> Option<&Arc<super::push_delivery::PushService>> {
        self.inner.notifications.get()
    }

    pub(super) fn configure_pushes(
        &self,
        service: Arc<super::push_delivery::PushService>,
    ) -> Result<(), Arc<super::push_delivery::PushService>> {
        self.inner.notifications.set(service)
    }

    pub(super) fn webhook_subscription(
        &self,
    ) -> std::sync::MutexGuard<'_, super::push_delivery::SubscriptionState> {
        self.inner
            .webhook_subscribed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The worker holds this handle, but the handle keeps only a weak sender.
    /// Once callers and drivers release the sender, the idle worker can retire.
    pub(super) fn operations(&self) -> Arc<TaskSender> {
        let mut state = self.inner.state.write();
        if let Some(queue) = state.operations.upgrade() {
            return queue;
        }
        let queue = self.spawn_controller();
        state.operations = Arc::downgrade(&queue);
        queue
    }

    fn spawn_controller(&self) -> Arc<TaskSender> {
        let task = self.clone();
        let (sender, mut commands) =
            tokio::sync::mpsc::channel::<TaskTransition>(TASK_QUEUE_CAPACITY);
        let background_tasks = task.background_tasks().clone();
        background_tasks.spawn(async move {
            let mut state = TaskController::default();
            while let Some(transition) = commands.recv().await {
                tracing::trace!(task_id = %task.id(), "processing task operation");
                transition(&mut state).await;
            }
        });
        Arc::new(sender)
    }

    /// Once enqueued, the operation owns its reply and queue lease independently of its caller.
    pub(super) async fn submit<T, F>(
        &self,
        operation: TaskOperation,
        transition: F,
    ) -> Result<T, A2AError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut TaskController) -> futures::future::BoxFuture<'a, T>
            + Send
            + 'static,
    {
        use futures::FutureExt;
        self.background_tasks().check_running()?;
        let task = self.operations();
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

    pub(super) fn active(&self) -> Option<Arc<ActiveExecution>> {
        self.inner.state.read().execution.upgrade()
    }

    /// The controller owns the invocation; routing must not keep it alive.
    pub(super) fn register(&self, active: &Arc<ActiveExecution>) -> Result<(), A2AError> {
        let mut state = self.inner.state.write();
        if state
            .execution
            .upgrade()
            .is_some_and(|current| current.is_prepared())
        {
            return Err(A2AError::invalid_request(format!(
                "task execution is already in progress: {}",
                self.inner.id
            )));
        }
        state.execution = Arc::downgrade(active);
        Ok(())
    }

    pub(super) fn unregister(&self, active: &Arc<ActiveExecution>) {
        let mut state = self.inner.state.write();
        if state
            .execution
            .upgrade()
            .is_some_and(|current| Arc::ptr_eq(&current, active))
        {
            state.execution = Weak::new();
        }
    }

    pub(super) fn resubscribe_active(&self) -> Option<(ExecutionReceiver, Option<Task>, u64)> {
        Some(self.active()?.resubscribe())
    }

    #[cfg(test)]
    pub(super) fn reserve(
        &self,
    ) -> Result<
        (
            Arc<ActiveExecution>,
            crate::ClientInputs,
            super::execution::InvocationState,
        ),
        A2AError,
    > {
        let (active, inputs, state) = ActiveExecution::with_task(self.clone());
        self.register(&active)?;
        Ok((active, inputs, state))
    }

    pub(super) fn watch(&self) -> TaskWatch {
        TaskWatch {
            changes: self.inner.changes.subscribe(),
            _task: self.clone(),
        }
    }

    pub(super) fn notify(&self) {
        self.inner.changes.send_modify(|version| *version += 1);
    }

    #[cfg(test)]
    pub(super) fn is_watched(&self) -> bool {
        self.inner.changes.receiver_count() > 0
    }

    pub(super) fn snapshot(&self) -> Option<Task> {
        self.inner.state.read().snapshot.clone()
    }

    pub(super) fn set_snapshot(&self, task: Task) {
        self.inner.state.write().snapshot = Some(task);
    }

    pub(super) fn sequence(&self) -> u64 {
        self.inner.state.read().sequence
    }

    pub(super) fn subscribe(&self) -> (broadcast::Receiver<ExecutionEvent>, u64) {
        let receiver = self.inner.sender.subscribe();
        let sequence = self.inner.state.read().sequence;
        (receiver, sequence)
    }

    pub(super) fn resubscribe(&self) -> (broadcast::Receiver<ExecutionEvent>, Option<Task>, u64) {
        let receiver = self.inner.sender.subscribe();
        let state = self.inner.state.read();
        (receiver, state.snapshot.clone(), state.sequence)
    }

    /// A lagging response must still observe an execution error published since
    /// its cursor, even after a later turn has overwritten the broadcast buffer.
    pub(super) fn resynchronize(
        &self,
        after: u64,
    ) -> Result<(broadcast::Receiver<ExecutionEvent>, Option<Task>, u64), A2AError> {
        let receiver = self.inner.sender.subscribe();
        let state = self.inner.state.read();
        if let Some((sequence, error)) = &state.last_error
            && *sequence > after
        {
            return Err(error.clone());
        }
        Ok((receiver, state.snapshot.clone(), state.sequence))
    }

    /// Cache and broadcast a persisted update before waking task readers.
    pub(super) fn publish(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot: Option<Task>,
        kind: EventKind,
        failure: Option<FailureSignal>,
    ) {
        let changed = snapshot.is_some()
            || (kind == EventKind::Normal
                && result.is_ok_and(|event| super::protocol::task_id_for_event(event).is_some()));
        self.publish_event(result, snapshot, kind, failure);
        if changed {
            self.notify();
        }
    }

    /// Input is persisted as one batch. Reattachment must see its final cursor
    /// and snapshot, so wake readers only after all its events are available.
    pub(super) fn publish_input(
        &self,
        snapshot: Task,
        events: &[StreamResponse],
        failure: FailureSignal,
    ) {
        if events.is_empty() {
            self.set_snapshot(snapshot);
        } else {
            for (index, event) in events.iter().enumerate() {
                let snapshot = (index + 1 == events.len()).then(|| snapshot.clone());
                self.publish_event(
                    Ok(event),
                    snapshot,
                    EventKind::Normal,
                    Some(failure.clone()),
                );
            }
        }
        self.notify();
    }

    fn publish_event(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot: Option<Task>,
        kind: EventKind,
        failure: Option<FailureSignal>,
    ) {
        let sequence = {
            let mut state = self.inner.state.write();
            if let Some(task) = snapshot {
                state.snapshot = Some(task);
            }
            state.sequence += 1;
            if let Err(error) = result {
                state.last_error = Some((state.sequence, error.clone()));
            }
            state.sequence
        };
        let notifies = kind == EventKind::Normal
            && result.is_ok_and(|event| super::protocol::task_id_for_event(event).is_some());
        // Subscription startup/retirement and send share this short lock; an
        // idle subscriber cannot disappear between admission and publication.
        let _subscription = if notifies {
            self.inner
                .notifications
                .get()
                .map(|_| super::push_delivery::subscribe(self))
        } else {
            None
        };
        if self.inner.sender.receiver_count() > 0 {
            let _ = self.inner.sender.send(ExecutionEvent {
                sequence,
                result: result.cloned().map_err(Clone::clone),
                kind,
                failure,
            });
        }
    }
}

/// Holds the task handle while waiting, so updates use the same channel.
pub(super) struct TaskWatch {
    _task: TaskHandle,
    changes: watch::Receiver<u64>,
}

impl TaskWatch {
    pub(super) async fn changed(&mut self) {
        let _ = self.changes.changed().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::make_handler;
    use crate::handler::test_support::task_with_history;

    #[tokio::test]
    async fn input_batch_exposes_its_complete_snapshot_and_events_before_waking_readers() {
        let task = TaskHandle::new("task".into(), None);
        let mut watch = task.watch();
        let mut events = task.sender().subscribe();
        let snapshot = task_with_history("task", "context", 0);
        let updates = vec![StreamResponse::Task(snapshot.clone()); 3];
        task.publish_input(
            snapshot.clone(),
            &updates,
            tokio::sync::watch::channel(None).0,
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), watch.changed())
            .await
            .unwrap();
        let (_, cached, cursor) = task.resubscribe();
        assert_eq!(cached, Some(snapshot));
        assert_eq!(cursor, updates.len() as u64);
        for (index, expected) in updates.into_iter().enumerate() {
            let event = events.try_recv().unwrap();
            assert_eq!(event.sequence, index as u64 + 1);
            assert_eq!(event.result.unwrap(), expected);
        }
        assert!(events.try_recv().is_err());
        // A batch is one state change even when it broadcasts several events.
        assert_eq!(*watch.changes.borrow(), 1);
    }

    #[tokio::test]
    async fn publication_wakes_readers_after_the_snapshot_and_event_are_available() {
        let task = TaskHandle::new("task".into(), None);
        let mut watch = task.watch();
        let mut events = task.sender().subscribe();
        let snapshot = task_with_history("task", "context", 0);
        let waiter = tokio::spawn({
            let task = task.clone();
            async move {
                watch.changed().await;
                (task.snapshot(), events.try_recv().unwrap())
            }
        });
        task.publish(
            Ok(&StreamResponse::Task(snapshot.clone())),
            Some(snapshot.clone()),
            EventKind::Normal,
            None,
        );
        let (cached, event) = tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cached, Some(snapshot.clone()));
        assert!(matches!(event.result, Ok(StreamResponse::Task(task)) if task == snapshot));
    }

    #[tokio::test]
    async fn committed_failures_wake_readers_but_private_rejections_do_not() {
        let task = TaskHandle::new("task".into(), None);
        let mut watch = task.watch();
        let rejection = StreamResponse::Error(StreamError {
            task_id: "task".into(),
            code: "UNAVAILABLE".into(),
            message: "busy".into(),
            reference_id: None,
            metadata: None,
        });
        task.publish(Ok(&rejection), None, EventKind::LiveOnly(None), None);
        assert!(!watch.changes.has_changed().unwrap());
        let mut failed = task_with_history("task", "context", 0);
        failed.status.state = TaskState::Failed;
        let error = A2AError::internal("execution failed");
        task.publish(Err(&error), Some(failed.clone()), EventKind::Normal, None);
        tokio::time::timeout(std::time::Duration::from_secs(1), watch.changed())
            .await
            .unwrap();
        assert_eq!(task.snapshot(), Some(failed));
        assert_eq!(task.resynchronize(0).err().unwrap().message, error.message);
    }

    #[tokio::test]
    async fn task_subscription_and_sequence_survive_executor_handoff() {
        let manager = make_handler().tasks;
        let (first, _inputs, _invocation) = manager.task("task").reserve().unwrap();
        let source = first.task.clone();
        let mut subscriber = source.sender().subscribe();
        let task = task_with_history("task", "context", 0);
        first.set_snapshot(task.clone());
        first.publish(Ok(&StreamResponse::Task(task.clone())), Some(task.clone()));
        manager.task("task").unregister(&first);
        drop(first);

        let (second, _inputs, _invocation) = manager.task("task").reserve().unwrap();
        assert!(source.ptr_eq(&second.task));
        assert!(
            second.snapshot().is_none(),
            "a new reservation must wait for preparation"
        );
        second.set_snapshot(task.clone());
        second.publish(Ok(&StreamResponse::Task(task)), None);
        assert_eq!(subscriber.recv().await.unwrap().sequence, 1);
        assert_eq!(subscriber.recv().await.unwrap().sequence, 2);
    }

    #[tokio::test]
    async fn registry_does_not_retain_unused_task_sources() {
        let manager = make_handler().tasks;
        let source = manager.task("unused");
        let weak = source.downgrade();
        drop(source);
        assert!(weak.upgrade().is_none());
    }
}
