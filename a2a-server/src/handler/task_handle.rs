// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! In-memory channels and execution routing owned by one task handle.

use super::events::{ActiveExecution, ExecutionEvent, FailureSignal};
use super::protocol::EventKind;
use super::push_worker::{PushDispatcher, TaskPushQueue};
use a2a::*;
use parking_lot::RwLock;
use std::sync::{Arc, Weak};
use tokio::sync::{broadcast, watch};

mod admission_queue;
mod controller;

pub(super) use controller::{OutputAction, TaskCommand, TaskController};

tokio::task_local! {
    // A publication can join only the batch of the controller executing it.
    static CONTROLLER: TaskHandle;
}

const TASK_QUEUE_CAPACITY: usize = 32;
pub(super) type TaskSender = tokio::sync::mpsc::Sender<QueuedCommand>;

pub(super) struct QueuedCommand {
    command: TaskCommand,
    // Keep this mailbox registered until its admitted operation finishes.
    _lease: Arc<TaskSender>,
    publication: Option<super::push_delivery::PublicationPermit>,
    operation: TaskOperation,
}

/// A typed reply also reports interruption when unwinding drops its command.
pub(super) struct CommandReply<T> {
    sender: Option<tokio::sync::oneshot::Sender<Result<T, A2AError>>>,
    operation: TaskOperation,
}

impl<T> CommandReply<T> {
    pub(super) fn complete(mut self, value: T) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(Ok(value));
        }
    }
}

impl<T> Drop for CommandReply<T> {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let message = match self.operation {
                TaskOperation::Input => "message preparation failed unexpectedly",
                _ => "task transition failed unexpectedly",
            };
            let _ = sender.send(Err(A2AError::internal(message)));
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) enum TaskOperation {
    Input,
    Subscribe,
    Output,
    Publication,
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
    interrupted_input_generation: i64,
    completed_input: Option<(u64, i64)>,
    notifications: Option<Vec<ExecutionEvent>>,
    push_queue: Weak<TaskPushQueue>,
    // Outstanding receivers retain their direct reply or private input rejection.
    // No task-wide reply history survives after those receivers are dropped.
    replies: Vec<Weak<ReplyRetention>>,
}

pub(super) struct ReplyRetention {
    opened_at: u64,
    pub(super) response_id: Option<uuid::Uuid>,
    pub(super) reply: std::sync::OnceLock<ExecutionEvent>,
    pub(super) rejection: std::sync::OnceLock<ExecutionEvent>,
}

/// Persisted Task data stays in TaskStore. This handle owns its process-local
/// channels; receivers and active workers retain it across executor invocations.
#[derive(Clone)]
pub(super) struct TaskHandle {
    inner: Arc<TaskInner>,
}

/// Weak routing reference that does not retain a task's channels or snapshot.
pub(crate) struct WeakTaskHandle {
    inner: Weak<TaskInner>,
}

impl WeakTaskHandle {
    pub(crate) fn snapshot(&self) -> Option<Task> {
        self.upgrade()?.snapshot()
    }

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
    input_activity: Arc<tokio::sync::Notify>,
    sender: broadcast::Sender<ExecutionEvent>,
    state: RwLock<State>,
    notifications: std::sync::OnceLock<Arc<super::push_delivery::PushService>>,
    publications: std::sync::OnceLock<super::push_delivery::PublicationQueue>,
}

impl TaskHandle {
    pub(super) fn publications(&self) -> &super::push_delivery::PublicationQueue {
        self.inner
            .publications
            .get_or_init(|| super::push_delivery::PublicationQueue::new(self))
    }

    fn in_controller(&self) -> bool {
        CONTROLLER
            .try_with(|task| Arc::ptr_eq(&task.inner, &self.inner))
            .unwrap_or(false)
    }

    pub(super) fn buffer_notification(&self, event: ExecutionEvent) -> bool {
        if !self.in_controller() {
            return false;
        }
        let mut state = self.inner.state.write();
        if let Some(events) = &mut state.notifications {
            events.push(event);
            true
        } else {
            false
        }
    }

    fn finish_notifications(&self, permit: Option<super::push_delivery::PublicationPermit>) {
        let events = self
            .inner
            .state
            .write()
            .notifications
            .take()
            .unwrap_or_default();
        if events.is_empty() {
            return;
        }
        let permit = permit.or_else(|| self.publications().try_reserve());
        match permit {
            Some(permit) => self.publications().submit(self.clone(), events, permit),
            None => {
                if let Some(settings) = self.pushes() {
                    for event in events {
                        super::push_worker::signal_failure(
                            settings.sender(),
                            &event,
                            A2AError::internal("webhook publication capacity was not reserved"),
                        );
                    }
                }
            }
        }
    }

    /// A bounded checkpoint lets blocking senders recover a missed local
    /// invocation-completion event without retaining another task snapshot.
    pub(super) fn completed_input(&self, required: i64, after: u64) -> Option<ExecutionEvent> {
        if required <= 0 {
            return None;
        }
        let state = self.inner.state.read();
        let (sequence, input_generation) = state.completed_input?;
        if sequence <= after || input_generation < required {
            return None;
        }
        Some(ExecutionEvent {
            sequence,
            input_generation,
            kind: EventKind::InvocationEnded,
            result: Ok(StreamResponse::Task(state.snapshot.clone()?)),
            failure: None,
            response_task: None,
        })
    }
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
                input_activity: Arc::default(),
                sender: broadcast::channel(super::EXECUTION_BUFFER_CAPACITY).0,
                state: RwLock::new(State {
                    snapshot,
                    ..State::default()
                }),
                notifications: std::sync::OnceLock::new(),
                publications: std::sync::OnceLock::new(),
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

    #[cfg(test)]
    pub(super) fn sender(&self) -> &broadcast::Sender<ExecutionEvent> {
        &self.inner.sender
    }

    pub(super) fn pushes(&self) -> Option<&Arc<super::push_delivery::PushService>> {
        self.inner.notifications.get()
    }

    pub(super) fn push_queue(&self, dispatcher: &PushDispatcher) -> Option<Arc<TaskPushQueue>> {
        let mut state = self.inner.state.write();
        if let Some(queue) = state.push_queue.upgrade()
            && !queue.is_closed()
        {
            return Some(queue);
        }
        let queue = dispatcher.register(self)?;
        state.push_queue = Arc::downgrade(&queue);
        Some(queue)
    }

    /// Synchronize retirement with producer lookup. A producer either retains
    /// this queue before the check or registers a new queue after it is detached.
    pub(super) fn retire_push_queue(&self, queue: &Arc<TaskPushQueue>) -> bool {
        let mut state = self.inner.state.write();
        if Arc::strong_count(queue) != 1 || !queue.is_empty() {
            return false;
        }
        if Weak::ptr_eq(&state.push_queue, &Arc::downgrade(queue)) {
            state.push_queue = Weak::new();
        }
        true
    }

    pub(super) fn configure_pushes(
        &self,
        service: Arc<super::push_delivery::PushService>,
    ) -> Result<(), Arc<super::push_delivery::PushService>> {
        self.inner.notifications.set(service)
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
            tokio::sync::mpsc::channel::<QueuedCommand>(TASK_QUEUE_CAPACITY);
        let background_tasks = task.background_tasks().clone();
        background_tasks.spawn(CONTROLLER.scope(task.clone(), async move {
            use futures::FutureExt;
            let mut state = TaskController::default();
            let mut pending =
                admission_queue::PendingAdmissions::new(task.inner.input_activity.clone());
            while let Some(work) = pending.next(&mut commands, &state).await {
                tracing::trace!(task_id = %task.id(), "processing task operation");
                task.inner.state.write().notifications = Some(Vec::new());
                if std::panic::AssertUnwindSafe(work.command.run(&mut state))
                    .catch_unwind()
                    .await
                    .is_err()
                {
                    tracing::error!(task_id = %task.id(), "task controller operation panicked");
                    if !matches!(work.operation, TaskOperation::Subscribe) {
                        state.recover_panic();
                        let error = A2AError::internal("task transition failed unexpectedly");
                        task.publish(Err(&error), None, EventKind::Normal, None, 0)
                            .await;
                        task.notify();
                    }
                }
                task.finish_notifications(work.publication);
                drop(work._lease);
            }
        }));
        Arc::new(sender)
    }

    /// Once enqueued, a command and its typed reply outlive the requesting future.
    pub(super) async fn request<T: Send + 'static>(
        &self,
        operation: TaskOperation,
        command: impl FnOnce(CommandReply<T>) -> TaskCommand,
    ) -> Result<T, A2AError> {
        self.background_tasks().check_running()?;
        let publication = if self.pushes().is_some() {
            self.publications().reserve(self, operation).await?
        } else {
            None
        };
        let queue = self.operations();
        let (sender, result) = tokio::sync::oneshot::channel();
        let work = QueuedCommand {
            command: command(CommandReply {
                sender: Some(sender),
                operation,
            }),
            _lease: queue.clone(),
            publication,
            operation,
        };
        queue
            .send(work)
            .await
            .map_err(|_| A2AError::internal("task operation queue closed"))?;
        result
            .await
            .map_err(|_| A2AError::internal("task operation lost its response"))?
    }

    /// Tests can install gates and failures without adding production commands.
    #[cfg(test)]
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
        self.request(operation, |reply| {
            TaskCommand::Test(Box::new(move |state| {
                Box::pin(async move {
                    reply.complete(transition(state).await);
                })
            }))
        })
        .await
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

    pub(super) fn wake_inputs(&self) {
        self.inner.input_activity.notify_waiters();
    }

    pub(super) fn notify(&self) {
        self.inner.changes.send_modify(|version| *version += 1);
    }

    #[cfg(test)]
    pub(super) fn is_watched(&self) -> bool {
        self.inner.changes.receiver_count() > 0 || self.inner.sender.receiver_count() > 0
    }

    pub(super) fn is_terminal(&self) -> bool {
        self.inner
            .state
            .read()
            .snapshot
            .as_ref()
            .is_some_and(|task| task.status.state.is_terminal())
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

    #[cfg(test)]
    pub(super) fn resubscribe(&self) -> (broadcast::Receiver<ExecutionEvent>, Option<Task>, u64) {
        let receiver = self.inner.sender.subscribe();
        let state = self.inner.state.read();
        (receiver, state.snapshot.clone(), state.sequence)
    }

    pub(super) fn response_subscription(
        &self,
        response_id: Option<uuid::Uuid>,
    ) -> (
        broadcast::Receiver<ExecutionEvent>,
        Option<Task>,
        u64,
        Arc<ReplyRetention>,
    ) {
        let mut state = self.inner.state.write();
        let receiver = self.inner.sender.subscribe();
        let reply = Arc::new(ReplyRetention {
            opened_at: state.sequence,
            response_id,
            reply: std::sync::OnceLock::new(),
            rejection: std::sync::OnceLock::new(),
        });
        state.replies.retain(|reply| reply.strong_count() > 0);
        state.replies.push(Arc::downgrade(&reply));
        (receiver, state.snapshot.clone(), state.sequence, reply)
    }

    /// A lagging response must still observe an execution error published since
    /// its cursor, even after a later turn has overwritten the broadcast buffer.
    pub(super) fn resynchronize(
        &self,
        after: u64,
    ) -> Result<(broadcast::Receiver<ExecutionEvent>, Option<Task>, u64, i64), A2AError> {
        let receiver = self.inner.sender.subscribe();
        let state = self.inner.state.read();
        if let Some((sequence, error)) = &state.last_error
            && *sequence > after
        {
            return Err(error.clone());
        }
        Ok((
            receiver,
            state.snapshot.clone(),
            state.sequence,
            state.interrupted_input_generation,
        ))
    }

    /// Cache and broadcast a persisted update before waking task readers.
    pub(super) async fn publish(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot: Option<Task>,
        kind: EventKind,
        failure: Option<FailureSignal>,
        input_generation: i64,
    ) {
        let changed = snapshot.is_some()
            || (kind == EventKind::Normal
                && result.is_ok_and(|event| super::protocol::task_id_for_event(event).is_some()));
        self.publish_event(
            result,
            snapshot,
            kind,
            failure,
            Some(input_generation),
            None,
        )
        .await;
        if changed {
            self.notify();
        }
    }

    /// Input is persisted as one batch. Reattachment must see its final cursor
    /// and snapshot, so wake readers only after all its events are available.
    pub(super) async fn publish_input(
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
                    None,
                    None,
                )
                .await;
            }
        }
        self.notify();
    }

    /// Finalization must not publish an old invocation's failure into a newly
    /// started one. Check identity and allocate the error cursor in one lock.
    pub(super) async fn execution_failed(&self, active: &Arc<ActiveExecution>, error: &A2AError) {
        self.publish_event(
            Err(error),
            None,
            EventKind::Normal,
            None,
            None,
            Some(active),
        )
        .await;
        self.notify();
    }

    async fn publish_event(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot: Option<Task>,
        kind: EventKind,
        failure: Option<FailureSignal>,
        input_generation: Option<i64>,
        execution: Option<&Arc<ActiveExecution>>,
    ) {
        // Reserve capacity before assigning a cursor. Direct publishers use the
        // same mailbox as controller batches, preserving client/webhook order.
        if !self.in_controller()
            && self.pushes().is_some()
            && execution.is_none()
            && kind == EventKind::Normal
            && result.is_ok_and(|event| super::protocol::task_id_for_event(event).is_some())
        {
            let fallback_snapshot = snapshot.clone();
            let published = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let publication = self
                .request(TaskOperation::Publication, |reply| TaskCommand::Publish {
                    source: self.clone(),
                    event: Box::new(result.cloned().map_err(Clone::clone)),
                    snapshot: snapshot.map(Box::new),
                    kind,
                    failure: failure.clone(),
                    input_generation,
                    published: published.clone(),
                    reply,
                })
                .await;
            if let Err(error) = publication {
                let sequence = published.load(std::sync::atomic::Ordering::Acquire);
                let event = if sequence == 0 {
                    // A failed webhook admission must not erase the committed
                    // client update. No unordered webhook fallback is queued.
                    self.record_event(
                        result,
                        fallback_snapshot,
                        kind,
                        failure,
                        input_generation,
                        None,
                    )
                } else {
                    Some(ExecutionEvent {
                        sequence,
                        input_generation: input_generation.unwrap_or(0),
                        result: result.cloned().map_err(Clone::clone),
                        kind,
                        failure,
                        response_task: None,
                    })
                };
                if let Some(event) = event {
                    super::push_worker::signal_failure(
                        self.pushes().unwrap().sender(),
                        &event,
                        error,
                    );
                }
                self.notify();
            }
            return;
        }
        let _ =
            self.publish_event_inner(result, snapshot, kind, failure, input_generation, execution);
    }

    fn publish_event_inner(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot: Option<Task>,
        kind: EventKind,
        failure: Option<FailureSignal>,
        input_generation: Option<i64>,
        execution: Option<&Arc<ActiveExecution>>,
    ) -> Option<u64> {
        let event =
            self.record_event(result, snapshot, kind, failure, input_generation, execution)?;
        let sequence = event.sequence;
        if let Some(pushes) = self.pushes() {
            super::push_delivery::notify(self, pushes, event);
        }
        Some(sequence)
    }

    fn record_event(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot: Option<Task>,
        kind: EventKind,
        failure: Option<FailureSignal>,
        input_generation: Option<i64>,
        execution: Option<&Arc<ActiveExecution>>,
    ) -> Option<ExecutionEvent> {
        let event = {
            let mut state = self.inner.state.write();
            if let Some(execution) = execution {
                if !state
                    .execution
                    .upgrade()
                    .is_some_and(|current| Arc::ptr_eq(&current, execution))
                {
                    return None;
                }
                state.execution = Weak::new();
            }
            if let Some(task) = snapshot
                && state
                    .snapshot
                    .as_ref()
                    .is_none_or(|cached| task.generation >= cached.generation)
            {
                state.snapshot = Some(task);
            }
            if result.is_ok_and(|event| {
                super::protocol::should_interrupt_non_streaming(false, event).is_some()
            }) && let Some(input_generation) = input_generation
            {
                state.interrupted_input_generation = input_generation;
            }
            state.sequence += 1;
            if let Err(error) = result {
                state.last_error = Some((state.sequence, error.clone()));
            }
            let event = ExecutionEvent {
                sequence: state.sequence,
                input_generation: input_generation.unwrap_or(0),
                result: result.cloned().map_err(Clone::clone),
                kind,
                failure,
                response_task: if result.is_ok_and(|event| {
                    matches!(event, StreamResponse::StatusUpdate(_))
                        && (super::protocol::is_terminal_event(event)
                            || super::protocol::should_interrupt_non_streaming(false, event)
                                .is_some())
                }) {
                    state
                        .snapshot
                        .as_ref()
                        .filter(|task| {
                            result.ok().and_then(super::protocol::generation_of)
                                == Some(task.generation)
                        })
                        .cloned()
                        .map(Arc::new)
                } else {
                    None
                },
            };
            if kind == EventKind::InvocationEnded {
                state.completed_input = Some((event.sequence, event.input_generation));
            }
            state.replies.retain(|reply| {
                let Some(reply) = reply.upgrade() else {
                    return false;
                };
                let private = matches!(event.kind, EventKind::ResponseOnly(Some(id)) if reply.response_id == Some(id));
                if event.sequence > reply.opened_at {
                    if matches!(event.result, Ok(StreamResponse::Message(_))) {
                        let _ = reply.reply.set(event.clone());
                    }
                    if private {
                        let _ = reply.rejection.set(event.clone());
                    }
                }
                true
            });
            // Cursor allocation and client delivery share this short lock,
            // including failure fallback outside the controller.
            let _ = self.inner.sender.send(event.clone());
            event
        };
        Some(event)
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
    async fn a_retiring_controller_cannot_steal_an_answer_read_claim() {
        use crate::handler::test_support::{seed_task, send_req};
        use crate::{
            AgentExecutor, ExecutorContext, InMemoryTaskStore, RequestHandler, ServiceParams,
        };
        use futures::{FutureExt, stream::BoxStream};
        use tokio::{
            sync::Notify,
            time::{Duration, timeout},
        };

        struct WaitingWorker(Arc<Notify>);
        impl AgentExecutor for WaitingWorker {
            fn execute(
                &self,
                ctx: ExecutorContext,
            ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
                let release = self.0.clone();
                Box::pin(ctx.spawn_worker(1, move |mut ctx, output| async move {
                    let event = |state| {
                        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                            task_id: ctx.task_id.clone(),
                            context_id: ctx.context_id.clone(),
                            generation: 0,
                            status: TaskStatus {
                                state,
                                message: None,
                                timestamp: None,
                            },
                            metadata: None,
                        })
                    };
                    output.send(event(TaskState::InputRequired)).await?;
                    release.notified().await;
                    let input = ctx
                        .inputs
                        .recv()
                        .await
                        .expect("the answer must reach the reader");
                    output.send_for(&input, event(TaskState::Completed)).await?;
                    Ok(())
                }))
            }

            fn cancel(
                &self,
                _: ExecutorContext,
            ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
                Box::pin(futures::stream::empty())
            }
        }

        crate::test_util::install_crypto_provider();
        let release = Arc::new(Notify::new());
        let handler = super::super::DefaultRequestHandler::new(
            WaitingWorker(release.clone()),
            InMemoryTaskStore::new(),
        );
        seed_task(&handler, "task", "context").await;
        let task = handler.tasks.task("task");
        // Retiring mailboxes can outlive their last sender long enough for a
        // replacement to start. Keep their earlier notification waiter registered.
        let retired = task.inner.input_activity.notified();
        tokio::pin!(retired);
        retired.as_mut().enable();
        let params = ServiceParams::new();
        handler
            .send_message(&params, send_req(Some("task"), Some("context")))
            .await
            .unwrap();
        let active = task.active().unwrap();
        let capacity = active.inputs.sender.capacity();
        let before = handler.tasks.load("task").await.unwrap();
        let answer = send_req(Some("task"), Some("context"));
        let message_id = answer.message.as_ref().unwrap().message_id.clone();
        let waiting = tokio::spawn({
            let handler = handler.clone();
            async move { handler.send_message(&params, answer).await }
        });
        timeout(Duration::from_secs(1), async {
            while active.inputs.sender.capacity() == capacity {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the answer must be offered before the reader is released");
        assert_eq!(handler.tasks.load("task").await.unwrap(), before);
        release.notify_one();
        let result = timeout(Duration::from_secs(1), waiting)
            .await
            .expect("the current mailbox must receive the read-claim wakeup")
            .unwrap()
            .unwrap();
        assert!(
            matches!(result, SendMessageResponse::Task(task) if task.status.state == TaskState::Completed)
        );
        assert!(retired.as_mut().now_or_never().is_some());
        let stored = handler.tasks.load("task").await.unwrap();
        assert_eq!(
            stored
                .history
                .iter()
                .flatten()
                .filter(|message| message.message_id == message_id)
                .count(),
            1
        );
        handler.shutdown().await;
        assert_eq!(handler.tasks.background_tasks.count(), 0);
    }

    #[tokio::test]
    async fn a_controller_panic_revokes_execution_and_notifies_waiters() {
        let handler = make_handler();
        let task = handler.tasks.task("task");
        // A real execution driver retains this sender between operations.
        let _lease = task.operations();
        let active = task
            .submit(TaskOperation::Input, {
                let handler = handler.clone();
                move |state| {
                    Box::pin(async move { state.reserve(&handler, "task").await.unwrap().0 })
                }
            })
            .await
            .unwrap();
        let mut events = active.subscribe();
        let read_failure = task
            .submit::<(), _>(TaskOperation::Subscribe, |_| {
                Box::pin(async { panic!("failed subscription") })
            })
            .await
            .unwrap_err();
        assert!(read_failure.message.contains("failed unexpectedly"));
        assert!(
            task.active().is_some(),
            "a read panic must not cancel an invocation"
        );
        let write_failure = task
            .submit::<(), _>(TaskOperation::Finished, |_| {
                Box::pin(async { panic!("failed finalization") })
            })
            .await
            .unwrap_err();
        assert!(write_failure.message.contains("failed unexpectedly"));
        let event = tokio::time::timeout(std::time::Duration::from_secs(1), events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            event
                .result
                .unwrap_err()
                .message
                .contains("failed unexpectedly")
        );
        assert!(active.inputs.closed.is_cancelled());
        assert!(active.inputs_drained.is_cancelled());
        assert!(task.active().is_none());
        // The recovered controller can reserve a new invocation.
        let next = task
            .submit(TaskOperation::Input, {
                let handler = handler.clone();
                move |state| Box::pin(async move { state.reserve(&handler, "task").await.is_ok() })
            })
            .await
            .unwrap();
        assert!(next);
        handler.shutdown().await;
    }

    #[tokio::test]
    async fn failed_direct_publication_still_delivers_the_client_event_exactly_once() {
        for shutdown in [false, true] {
            let handler = make_handler().with_push_notifications(
                crate::InMemoryPushConfigStore::new(),
                crate::HttpPushSender::new(Some(crate::HttpPushSenderConfig {
                    fail_on_error: true,
                    ..Default::default()
                })),
            );
            let task = handler.tasks.task("task");
            handler.attach_pushes(&task);
            let mut client = task.sender().subscribe();
            let mut changed = task.watch();
            let _closed_queue = if shutdown {
                handler.shutdown().await;
                None
            } else {
                let (queue, receiver) = tokio::sync::mpsc::channel::<QueuedCommand>(1);
                drop(receiver);
                let queue = Arc::new(queue);
                task.inner.state.write().operations = Arc::downgrade(&queue);
                Some(queue)
            };
            let mut snapshot = task_with_history("task", "context", 0);
            snapshot.generation = 1;
            let output = StreamResponse::Task(snapshot.clone());
            let (failure, signal) = watch::channel(None);
            task.publish(
                Ok(&output),
                Some(snapshot.clone()),
                EventKind::Normal,
                Some(failure),
                1,
            )
            .await;
            let event = tokio::time::timeout(std::time::Duration::from_secs(1), client.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(event.sequence, 1);
            assert_eq!(event.result.unwrap(), output);
            assert!(client.try_recv().is_err());
            assert_eq!(task.snapshot(), Some(snapshot));
            assert!(
                signal.borrow().is_some(),
                "webhook failure remains observable"
            );
            tokio::time::timeout(std::time::Duration::from_secs(1), changed.changed())
                .await
                .unwrap();
            handler.shutdown().await;
        }
    }

    #[tokio::test]
    async fn failed_direct_publication_does_not_roll_back_a_newer_snapshot() {
        let handler = make_handler().with_push_notifications(
            crate::InMemoryPushConfigStore::new(),
            crate::HttpPushSender::new(None),
        );
        let task = handler.tasks.task("task");
        handler.attach_pushes(&task);
        let mut newer = task_with_history("task", "context", 0);
        newer.generation = 7;
        task.set_snapshot(newer.clone());
        let mut events = task.sender().subscribe();
        handler.shutdown().await;
        let mut older = newer.clone();
        older.generation = 3;
        let event = StreamResponse::Task(older.clone());
        task.publish(Ok(&event), Some(older), EventKind::Normal, None, 0)
            .await;
        assert_eq!(task.snapshot(), Some(newer));
        assert_eq!(events.recv().await.unwrap().result.unwrap(), event);
        assert!(events.try_recv().is_err());
    }

    #[tokio::test]
    async fn an_unreserved_webhook_batch_reports_failure_instead_of_disappearing() {
        let handler = make_handler().with_push_notifications(
            crate::InMemoryPushConfigStore::new(),
            crate::HttpPushSender::new(Some(crate::HttpPushSenderConfig {
                fail_on_error: true,
                ..Default::default()
            })),
        );
        let task = handler.tasks.task("task");
        handler.attach_pushes(&task);
        let permits: Vec<_> = (0..super::super::push_delivery::PUSH_OUTPUT_BATCHES)
            .map(|_| task.publications().try_reserve().unwrap())
            .collect();
        let (failure, mut signal) = watch::channel(None);
        let source = task.clone();
        task.submit(TaskOperation::Subscribe, move |_| {
            Box::pin(async move {
                source
                    .publish(
                        Ok(&StreamResponse::Task(task_with_history(
                            "task", "context", 0,
                        ))),
                        None,
                        EventKind::Normal,
                        Some(failure),
                        0,
                    )
                    .await;
            })
        })
        .await
        .unwrap();
        signal.changed().await.unwrap();
        assert!(
            signal
                .borrow()
                .as_ref()
                .unwrap()
                .message
                .contains("capacity was not reserved")
        );
        drop(permits);
        handler.shutdown().await;
    }

    #[tokio::test]
    async fn input_batch_exposes_its_complete_snapshot_and_events_before_waking_readers() {
        for count in [0, 3] {
            let task = TaskHandle::new("task".into(), None);
            let mut watch = task.watch();
            let mut events = task.sender().subscribe();
            let snapshot = task_with_history("task", "context", 0);
            let updates = vec![StreamResponse::Task(snapshot.clone()); count];
            task.publish_input(
                snapshot.clone(),
                &updates,
                tokio::sync::watch::channel(None).0,
            )
            .await;
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
    }

    #[tokio::test]
    async fn completion_checkpoints_cannot_answer_newer_inputs_or_older_cursors() {
        let mut snapshot = task_with_history("task", "context", 0);
        snapshot.generation = 7;
        let task = TaskHandle::new("task".into(), Some(snapshot.clone()));
        task.publish(
            Ok(&StreamResponse::Task(snapshot.clone())),
            None,
            EventKind::InvocationEnded,
            None,
            7,
        )
        .await;
        assert!(task.completed_input(8, 0).is_none());
        assert!(task.completed_input(7, task.sequence()).is_none());
        let recovered = task.completed_input(7, 0).unwrap();
        assert_eq!(recovered.input_generation, 7);
        assert_eq!(recovered.result.unwrap(), StreamResponse::Task(snapshot));
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
            0,
        )
        .await;
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
        task.publish(Ok(&rejection), None, EventKind::ResponseOnly(None), None, 0)
            .await;
        assert!(!watch.changes.has_changed().unwrap());
        let mut failed = task_with_history("task", "context", 0);
        failed.status.state = TaskState::Failed;
        let error = A2AError::internal("execution failed");
        task.publish(
            Err(&error),
            Some(failed.clone()),
            EventKind::Normal,
            None,
            0,
        )
        .await;
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
        first
            .publish(Ok(&StreamResponse::Task(task.clone())), Some(task.clone()))
            .await;
        manager.task("task").unregister(&first);
        drop(first);

        let (second, _inputs, _invocation) = manager.task("task").reserve().unwrap();
        assert!(source.ptr_eq(&second.task));
        assert!(
            second.snapshot().is_none(),
            "a new reservation must wait for preparation"
        );
        second.set_snapshot(task.clone());
        second.publish(Ok(&StreamResponse::Task(task)), None).await;
        assert_eq!(subscriber.recv().await.unwrap().sequence, 1);
        assert_eq!(subscriber.recv().await.unwrap().sequence, 2);
    }

    #[tokio::test]
    async fn dropping_a_response_releases_its_retained_reply() {
        let source = TaskHandle::new("task".into(), None);
        let (receiver, _, _, retained) = source.response_subscription(None);
        let weak = Arc::downgrade(&retained);
        source
            .publish(
                Ok(&StreamResponse::Message(Message::new(
                    Role::Agent,
                    vec![Part::text("reply")],
                ))),
                None,
                EventKind::Reply,
                None,
                0,
            )
            .await;
        assert!(retained.reply.get().is_some());
        drop((receiver, retained));
        assert!(weak.upgrade().is_none());
        drop(source);
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
