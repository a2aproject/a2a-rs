// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Shared task operations over any user-provided persistence implementation.
//!
//! The service owns coordination and subscriptions, so TaskStore implementors
//! need only provide persistence. Mutations run inside the task's mailbox;
//! executor, response, and webhook I/O stay outside it.

use super::DefaultRequestHandler;
use super::cancellation::CancellationStart;
use super::delivery::{AuthorizedMessage, Delivery, StartMode, request_task_id};
use super::events::ActiveExecution;
use super::execution::{ExecutionRequest, InvocationState, TurnEnd};
use super::protocol::Rejection;
use super::push_delivery::PushService;
use super::response_preparation::SubscriptionAttachment;
use super::runtime::{self, OutputAction, TaskOperation, TaskRuntime, TaskSender};
use super::task_handle::TaskHandle;
use super::updates;
use super::workers::Workers;
use crate::middleware::{ServiceParams, User};
use a2a::*;
use futures::future::BoxFuture;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::Duration,
};

pub(super) struct TaskService {
    pub(super) executor: Arc<dyn crate::AgentExecutor>,
    store: Arc<dyn crate::TaskStore>,
    handles: RwLock<HashMap<TaskId, Weak<TaskHandle>>>,
    pub(super) workers: Arc<Workers>,
}

impl TaskService {
    pub(super) fn new(executor: impl crate::AgentExecutor, store: impl crate::TaskStore) -> Self {
        let workers = Arc::new(Workers::default());
        Self {
            executor: Arc::new(executor),
            store: Arc::new(store),
            handles: RwLock::default(),
            workers,
        }
    }

    /// Test fixtures may seed storage directly; production code uses task operations.
    #[cfg(test)]
    pub(super) fn store(&self) -> &dyn crate::TaskStore {
        self.store.as_ref()
    }

    /// Get the shared runtime handle without creating or loading a stored Task.
    pub(super) fn task(&self, task_id: &str) -> Arc<TaskHandle> {
        if let Some(task) = self.handles.read().get(task_id).and_then(Weak::upgrade) {
            return task;
        }
        let mut tasks = self.handles.write();
        // Another request may have installed a handle while we acquired the writer.
        if let Some(task) = tasks.get(task_id).and_then(Weak::upgrade) {
            return task;
        }
        tasks.retain(|_, task| task.strong_count() > 0);
        let task = TaskHandle::with_workers(task_id.to_owned(), None, self.workers.clone());
        tasks.insert(task_id.to_owned(), Arc::downgrade(&task));
        task
    }

    fn queue(&self, task_id: &str) -> Arc<TaskSender> {
        self.task(task_id).operations()
    }

    #[cfg(test)]
    pub(super) fn watcher_count(&self) -> usize {
        self.handles
            .read()
            .values()
            .filter_map(Weak::upgrade)
            .filter(|task| task.is_watched())
            .count()
    }

    async fn with_task<T, F>(
        &self,
        task: Arc<TaskHandle>,
        operation: TaskOperation,
        transition: F,
    ) -> Result<T, A2AError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut TaskRuntime) -> BoxFuture<'a, T> + Send + 'static,
    {
        self.workers.check_running()?;
        runtime::submit(task.operations(), operation, transition).await
    }

    #[cfg(test)]
    pub(super) fn test_lease(&self, task_id: &str) -> Arc<TaskSender> {
        self.queue(task_id)
    }

    #[cfg(test)]
    pub(super) async fn test_update<T, F>(
        &self,
        task_id: &str,
        transition: F,
    ) -> Result<T, A2AError>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut TaskRuntime) -> BoxFuture<'a, T> + Send + 'static,
    {
        self.with_task(self.task(task_id), TaskOperation::Input, transition)
            .await
    }

    /// Retain task state before spawning, without a reference from that state back to its queue.
    pub(super) fn spawn_execution(
        &self,
        handler: DefaultRequestHandler,
        active: Arc<ActiveExecution>,
        request: impl Into<ExecutionRequest>,
    ) {
        let request = request.into();
        let lease = active.task.operations();
        self.workers.spawn(async move {
            let _lease = lease;
            super::execution::drive_execution_guarded(handler, active, request).await;
        });
    }

    /// Administrative startup scan: ordinary request authorization is not involved.
    pub(super) async fn restart_tasks(
        &self,
        handler: DefaultRequestHandler,
    ) -> Result<usize, A2AError> {
        self.workers.check_running()?;
        let mut token = None;
        let mut pages = std::collections::HashSet::new();
        let mut seen = std::collections::HashSet::new();
        let mut restarted = 0;
        loop {
            let request = ListTasksRequest {
                page_size: Some(100),
                page_token: token,
                context_id: None,
                status: None,
                history_length: None,
                timeline_length: None,
                status_timestamp_after: None,
                include_artifacts: Some(false),
                tenant: None,
            };
            let page = self.store.list(&request).await?;
            for task in page.tasks {
                if task.status.state.is_terminal() || !seen.insert(task.id.clone()) {
                    continue;
                }
                let id = task.id.clone();
                // Re-read and reserve in one task operation, skipping already active tasks.
                let started = self
                    .with_task(self.task(&id), TaskOperation::Input, {
                        let handler = handler.clone();
                        move |state| {
                            Box::pin(async move { state.restart(&handler, &task.id).await })
                        }
                    })
                    .await??;
                if let Some(started) = started {
                    if started.await.map_err(|_| {
                        A2AError::internal("executor restart failed before accepting the task")
                    })? {
                        restarted += 1;
                    }
                }
            }
            if page.next_page_token.is_empty() {
                return Ok(restarted);
            }
            if !pages.insert(page.next_page_token.clone()) {
                return Err(A2AError::internal(
                    "task store repeated a recovery page token",
                ));
            }
            token = Some(page.next_page_token);
        }
    }

    /// Every send mode admits input through the same owned task operation.
    /// Admission persists before delivery and rejects a full inbox before writing.
    pub(super) async fn append_input(
        &self,
        handler: DefaultRequestHandler,
        message: AuthorizedMessage,
        mode: StartMode,
    ) -> Result<Delivery, Rejection> {
        let task_id = request_task_id(&message.request).unwrap_or_else(new_task_id);
        let id = task_id.clone();
        self.with_task(self.task(&task_id), TaskOperation::Input, move |state| {
            Box::pin(async move { state.admit(&handler, message, mode, id).await })
        })
        .await
        .map_err(Rejection::Interrupted)?
    }

    /// Capture the snapshot and event cursor in the same mailbox as mutations.
    /// Subscribing never creates a stored task or starts an executor.
    pub(super) async fn subscribe(
        &self,
        handler: DefaultRequestHandler,
        request: SubscribeToTaskRequest,
    ) -> Result<SubscriptionAttachment, A2AError> {
        let task = self.task(&request.id);
        self.with_task(task.clone(), TaskOperation::Subscribe, move |_| {
            Box::pin(async move { handler.tasks.capture_subscription(task, request).await })
        })
        .await?
    }

    /// Dropping the request stops waiting, not the admitted cancellation.
    pub(super) async fn cancel(
        &self,
        handler: DefaultRequestHandler,
        params: ServiceParams,
        request: CancelTaskRequest,
        user: Option<User>,
    ) -> Result<Task, A2AError> {
        let task_id = request.id.clone();
        let lease = self.queue(&task_id);
        self.workers
            .spawn(async move {
                let _lease = lease;
                super::cancellation::cancel(handler, params, request, user).await
            })
            .ok_or_else(|| A2AError::internal("handler is shutting down"))?
            .await
            .map_err(|error| {
                tracing::error!(%task_id, %error, "cancellation operation failed");
                A2AError::internal("cancellation operation failed unexpectedly")
            })?
            .ok_or_else(|| A2AError::internal("handler is shutting down"))?
    }

    pub(super) async fn begin_cancel(
        &self,
        handler: DefaultRequestHandler,
        params: ServiceParams,
        request: CancelTaskRequest,
        user: Option<User>,
    ) -> Result<CancellationStart, A2AError> {
        self.with_task(
            self.task(&request.id),
            TaskOperation::Cancel,
            move |state| {
                Box::pin(async move { state.begin_cancel(&handler, &params, request, user).await })
            },
        )
        .await?
    }

    pub(super) async fn cancel_update(
        &self,
        handler: DefaultRequestHandler,
        task_id: &str,
        event: StreamResponse,
    ) -> Result<(), A2AError> {
        self.with_task(self.task(task_id), TaskOperation::Cancel, move |state| {
            Box::pin(async move { state.cancel_update(&handler, event).await })
        })
        .await?
    }

    pub(super) async fn cancel_finished(
        &self,
        handler: DefaultRequestHandler,
        task_id: &str,
        failure: Option<A2AError>,
    ) -> Result<(), A2AError> {
        self.with_task(self.task(task_id), TaskOperation::Cancel, move |state| {
            Box::pin(async move { state.cancel_finished(&handler, failure).await })
        })
        .await?
    }

    pub(super) async fn output(
        &self,
        handler: DefaultRequestHandler,
        active: Arc<ActiveExecution>,
        event: StreamResponse,
    ) -> Result<OutputAction, A2AError> {
        self.with_task(active.task.clone(), TaskOperation::Output, move |state| {
            Box::pin(async move { state.output(&handler, active, event).await })
        })
        .await?
    }

    pub(super) async fn finished(
        &self,
        handler: DefaultRequestHandler,
        active: Arc<ActiveExecution>,
        end: TurnEnd,
    ) -> Result<(), A2AError> {
        let id = active.task.id.clone();
        self.with_task(active.task.clone(), TaskOperation::Finished, move |state| {
            Box::pin(async move { state.finished(&handler, id, active, end).await })
        })
        .await
    }

    pub(super) async fn panicked(
        &self,
        handler: DefaultRequestHandler,
        active: Arc<ActiveExecution>,
    ) -> Result<(), A2AError> {
        let id = active.task.id.clone();
        self.with_task(active.task.clone(), TaskOperation::Finished, move |state| {
            Box::pin(async move { state.panicked(&handler, active, id).await })
        })
        .await
    }

    pub(super) async fn load(&self, task_id: &str) -> Result<Task, A2AError> {
        self.store
            .get(task_id)
            .await?
            .ok_or_else(|| A2AError::task_not_found(task_id))
    }

    /// Register the watch before reading, so an intervening write is observed.
    /// Periodic reads also notice writes made by another server process.
    pub(super) async fn wait_for_generation(
        &self,
        task_id: &str,
        current: i64,
        timeout: Duration,
    ) -> Result<Task, A2AError> {
        let deadline = tokio::time::Instant::now() + timeout;
        let mut watch = self.task(task_id).watch();
        loop {
            let task = self.load(task_id).await?;
            if task.generation > current || task.status.state.is_terminal() {
                return Ok(task);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(task);
            }
            tokio::select! {
                () = self.workers.cancelled() => {
                    return Err(A2AError::internal("handler is shutting down"));
                }
                () = watch.changed() => {}
                _ = tokio::time::sleep((deadline - now).min(Duration::from_secs(1))) => {}
            }
        }
    }

    /// Low-level writes below are used only inside a task operation.
    /// Publishing may be deferred until pending push configuration is installed.
    pub(super) async fn persist(
        &self,
        base: Option<Task>,
        event: &mut StreamResponse,
        creating: bool,
    ) -> Result<(Option<Task>, bool), A2AError> {
        updates::persist_event(self.store.as_ref(), base, event, creating).await
    }

    pub(super) async fn save(&self, task: Task, creating: bool) -> Result<Task, A2AError> {
        updates::save_task_with(self.store.as_ref(), task, creating).await
    }

    pub(super) async fn apply_and_publish(
        &self,
        active: &ActiveExecution,
        base: Option<Task>,
        event: &mut StreamResponse,
    ) -> Result<bool, A2AError> {
        let (snapshot, repeated) = self.persist(base, event, false).await?;
        active.publish_marked(Ok(event), snapshot, repeated);
        Ok(!repeated)
    }
    /// Persist executor output before the task broadcast and long-poll wakeup.
    /// A newly created task's webhook configuration is installed before publication.
    pub(super) async fn update(
        &self,
        active: &ActiveExecution,
        event: &mut StreamResponse,
        invocation: &mut InvocationState,
        pushes: Option<&PushService>,
    ) -> Result<(), A2AError> {
        updates::backfill_status_timestamp(event);
        let base = active.snapshot();
        let creating = active.uncreated.load(std::sync::atomic::Ordering::SeqCst);
        let applied = self.persist(base, event, creating).await;
        active
            .uncreated
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let (snapshot, repeated) = applied?;
        if creating {
            if let Some(pushes) = pushes {
                pushes.save_pending(&mut invocation.pending_push).await;
            }
        }
        active.publish_marked(Ok(event), snapshot, repeated);
        Ok(())
    }

    pub(super) async fn capture_subscription(
        &self,
        handle: Arc<TaskHandle>,
        req: SubscribeToTaskRequest,
    ) -> Result<SubscriptionAttachment, A2AError> {
        // Earlier admission commands have completed before this lookup. An
        // unpublished lazy reservation is still private to its opening caller.
        if let Some(active) = handle.active() {
            if active.uncreated.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(A2AError::task_not_found(&req.id));
            }
        }
        let mut resubscribed = handle.resubscribe_active();
        // An abandoned reservation with no snapshot has no subscribable Task.
        if matches!(resubscribed, Some((_, None, _))) {
            resubscribed = None;
        }
        if let Some((receiver, snapshot_task, sequence)) = resubscribed {
            if snapshot_task
                .as_ref()
                .is_some_and(|task| task.status.state.is_terminal())
            {
                return Err(A2AError::unsupported_operation(format!(
                    "task {} has already reached a terminal state",
                    req.id
                )));
            }
            // A client that already holds the task's current generation has
            // missed nothing, so it resumes without a snapshot. One that is
            // behind cannot be replayed to -- events are not retained -- and
            // falls back to a snapshot, which the spec allows.
            let snapshot_task = match (req.last_seen_generation, snapshot_task) {
                (Some(last_seen), Some(task))
                    if task.generation == last_seen && !task.status.state.is_terminal() =>
                {
                    None
                }
                (_, snapshot) => snapshot,
            };
            return Ok(SubscriptionAttachment {
                task: handle,
                receiver: Some(receiver),
                opening: snapshot_task,
                sequence,
                seen_generation: req.last_seen_generation.unwrap_or(0),
            });
        }

        // A registry miss also happens once a task finishes and its
        // execution is cleaned up, so it doesn't by itself mean the task
        // never existed -- check the store to tell the two apart before
        // reporting TaskNotFoundError.
        if let Some(task) = self.store.get(&req.id).await? {
            if task.status.state.is_terminal() {
                return Err(A2AError::unsupported_operation(format!(
                    "task {} has already reached a terminal state",
                    req.id
                )));
            }
            let current = req.last_seen_generation == Some(task.generation);
            return Ok(SubscriptionAttachment {
                task: handle,
                receiver: None,
                opening: (!current).then_some(task),
                sequence: 0,
                seen_generation: req.last_seen_generation.unwrap_or(0),
            });
        }
        Err(A2AError::task_not_found(&req.id))
    }
    pub(super) async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        self.store.get(task_id).await
    }

    pub(super) async fn mark_canceled(&self, task_id: &str) -> Result<Task, A2AError> {
        self.store.begin_cancel(task_id).await
    }

    pub(super) async fn list(
        &self,
        request: &ListTasksRequest,
        visible: Option<&(dyn for<'id> Fn(&'id str) -> bool + Send + Sync)>,
    ) -> Result<ListTasksResponse, A2AError> {
        match visible {
            Some(visible) => self.store.list_scoped(request, visible).await,
            None => self.store.list(request).await,
        }
    }

    pub(super) async fn commit_input(
        &self,
        input: &super::admission::PreparedInput,
        handler: &DefaultRequestHandler,
        message: &AuthorizedMessage,
        lazy: bool,
        invocation: &mut InvocationState,
    ) -> Result<(), A2AError> {
        let (params, request) = (&message.params, &message.request);
        if lazy && !input.existed {
            invocation.pending_push =
                handler.request_push_config(params, &input.task.id, request)?;
        } else {
            handler
                .save_request_push_config(params, &input.task.id, request)
                .await?;
        }
        // Configuration must be accepted before input becomes committed.
        if input.existed {
            self.store.update(input.task.clone()).await?;
        } else if !lazy {
            self.store.create(input.task.clone()).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod store_compatibility_tests {
    use super::*;
    use crate::handler::test_support::*;

    // An existing persistence-only implementation: no event or subscription API.
    struct LegacyStore(InMemoryTaskStore);

    #[async_trait]
    impl TaskStore for LegacyStore {
        async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.0.create(task).await
        }
        async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.0.update(task).await
        }
        async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
            self.0.get(id).await
        }
        async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
            self.0.list(req).await
        }
        async fn begin_cancel(&self, id: &str) -> Result<Task, A2AError> {
            self.0.begin_cancel(id).await
        }
    }

    #[tokio::test]
    async fn persistence_only_stores_get_shared_subscriptions_for_every_send_mode() {
        for mode in [StartMode::Blocking, StartMode::Streaming, StartMode::Bidi] {
            let release = Arc::new(Notify::new());
            let handler = DefaultRequestHandler::new(
                ResumableExecutor {
                    release: release.clone(),
                },
                LegacyStore(InMemoryTaskStore::new()),
            )
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            });
            let other = handler.clone();
            assert!(Arc::ptr_eq(&handler.tasks, &other.tasks));
            let params = ServiceParams::new();
            let subscription = || SubscribeToTaskRequest {
                id: "task".into(),
                last_seen_generation: None,
                tenant: None,
            };
            let missing = other
                .subscribe_to_task(&params, subscription())
                .await
                .err()
                .unwrap();
            assert_eq!(missing.code, error_code::TASK_NOT_FOUND);
            assert!(handler.tasks.get("task").await.unwrap().is_none());

            seed_task(&handler, "task", "context").await;
            let mut updates = other
                .subscribe_to_task(&params, subscription())
                .await
                .unwrap();
            let StreamResponse::Task(initial) = updates.next().await.unwrap().unwrap() else {
                panic!("subscription starts with the stored task");
            };
            assert_eq!(initial.status.state, TaskState::Submitted);
            assert!(handler.tasks.task("task").active().is_none());
            let mut request = send_request();
            let message_id = request.message.message_id.clone();
            request.message.task_id = Some("task".into());
            // Keep transport output alive independently of the task subscriber.
            let _output = match mode {
                StartMode::Blocking => {
                    request.configuration = Some(SendMessageConfiguration {
                        return_immediately: Some(true),
                        ..serde_json::from_value(serde_json::json!({})).unwrap()
                    });
                    assert!(matches!(
                        handler.send_message(&params, request).await.unwrap(),
                        SendMessageResponse::Task(_)
                    ));
                    None
                }
                StartMode::Streaming => Some(
                    handler
                        .send_streaming_message(&params, request)
                        .await
                        .unwrap(),
                ),
                StartMode::Bidi => {
                    let uplink = stream::once(async move {
                        StreamRequest {
                            tenant: None,
                            payload: StreamRequestPayload::SendMessage(request),
                        }
                    })
                    .chain(stream::pending())
                    .boxed();
                    Some(handler.send_live_message(&params, uplink).await.unwrap())
                }
            };
            timeout(Duration::from_secs(2), async {
                loop {
                    let event = updates.next().await.unwrap().unwrap();
                    if matches!(event, StreamResponse::Task(ref task) if task.status.state == TaskState::Working) {
                        break;
                    }
                }
            }).await.unwrap();
            let working = other.tasks.load("task").await.unwrap();
            assert!(
                working
                    .history
                    .as_ref()
                    .unwrap()
                    .iter()
                    .any(|m| m.message_id == message_id)
            );

            let expected = if mode == StartMode::Blocking {
                let canceled = other
                    .cancel_task(
                        &params,
                        CancelTaskRequest {
                            id: "task".into(),
                            metadata: None,
                            tenant: None,
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(canceled.status.state, TaskState::Canceled);
                TaskState::Canceled
            } else {
                release.notify_one();
                TaskState::Completed
            };
            timeout(Duration::from_secs(2), async {
                while updates.next().await.is_some() {}
            })
            .await
            .unwrap();
            let stored = handler.tasks.load("task").await.unwrap();
            assert_eq!(stored.status.state, expected);
            assert!(stored.generation > working.generation);
            let listed = handler
                .tasks
                .list(
                    &serde_json::from_value(serde_json::json!({})).unwrap(),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(listed.tasks.len(), 1);
            assert_eq!(listed.tasks[0].generation, stored.generation);
        }
    }
}

#[cfg(test)]
mod serialization_tests {
    use super::*;
    use crate::handler::test_support::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn admitted_work_finishes_after_its_caller_disconnects_and_precedes_later_commands() {
        let tasks = make_handler().tasks;
        let lease = tasks.queue("task");
        let value = Arc::new(AtomicUsize::new(0));
        let (entered, began) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let caller = tokio::spawn({
            let tasks = tasks.clone();
            let value = value.clone();
            async move {
                tasks
                    .with_task(tasks.task("task"), TaskOperation::Input, move |_| {
                        Box::pin(async move {
                            let _ = entered.send(());
                            let _ = released.await;
                            value.store(1, Ordering::SeqCst);
                        })
                    })
                    .await
                    .unwrap();
            }
        });
        began.await.unwrap();
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        assert!(Arc::ptr_eq(&lease, &tasks.queue("task")));
        let mut subscriber = tokio::spawn({
            let tasks = tasks.clone();
            let value = value.clone();
            async move {
                tasks
                    .with_task(tasks.task("task"), TaskOperation::Subscribe, move |_| {
                        Box::pin(async move { value.load(Ordering::SeqCst) })
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
        let tasks = make_handler().tasks;
        let _slow = tasks.queue("slow");
        let _fast = tasks.queue("fast");
        let (entered, began) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let pending = tokio::spawn({
            let tasks = tasks.clone();
            async move {
                tasks
                    .with_task(tasks.task("slow"), TaskOperation::Input, move |_| {
                        Box::pin(async move {
                            entered.send(()).unwrap();
                            released.await.unwrap();
                        })
                    })
                    .await
                    .unwrap()
            }
        });
        began.await.unwrap();
        assert_eq!(
            timeout(
                Duration::from_secs(1),
                tasks.with_task(tasks.task("fast"), TaskOperation::Input, |_| Box::pin(
                    async { 7 }
                ))
            )
            .await
            .unwrap()
            .unwrap(),
            7
        );
        let error = tasks
            .with_task(tasks.task("fast"), TaskOperation::Output, |_| {
                Box::pin(async { panic!("store panicked") })
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::INTERNAL_ERROR);
        let error = tasks
            .with_task::<(), _>(tasks.task("fast"), TaskOperation::Output, |_| {
                panic!("transition panicked before returning its future")
            })
            .await
            .unwrap_err();
        assert_eq!(error.message, "task transition failed unexpectedly");

        assert_eq!(
            tasks
                .with_task(tasks.task("fast"), TaskOperation::Subscribe, |_| Box::pin(
                    async { 8 }
                ))
                .await
                .unwrap(),
            8
        );
        release.send(()).unwrap();
        pending.await.unwrap();
    }

    #[tokio::test]
    async fn idle_mailboxes_retire_without_discarding_a_subscribed_task() {
        let tasks = make_handler().tasks;
        let task = tasks.task("task");
        let mut watch = task.watch();
        let mut events = task.sender.subscribe();
        let snapshot = task_with_history("task", "context", 0);
        task.set_snapshot(snapshot.clone());
        let queue = task.operations();
        let weak = Arc::downgrade(&queue);
        drop(queue);
        assert!(weak.upgrade().is_none());
        assert!(Arc::ptr_eq(&task, &tasks.task("task")));
        assert_eq!(
            tasks
                .with_task(tasks.task("task"), TaskOperation::Input, |_| Box::pin(
                    async { 9 }
                ))
                .await
                .unwrap(),
            9
        );
        let retained = tasks.task("task");
        assert_eq!(retained.snapshot(), Some(snapshot.clone()));
        retained.publish(
            Ok(&StreamResponse::Task(snapshot.clone())),
            None,
            super::super::events::EventKind::Normal,
            None,
        );
        assert!(matches!(events.recv().await.unwrap().result,
            Ok(StreamResponse::Task(task)) if task == snapshot));
        retained.notify();
        timeout(Duration::from_secs(1), watch.changed())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn completing_an_invocation_releases_its_task_queue_lease() {
        let (handler, release) = make_resumable_handler();
        let mut events = handler
            .send_streaming_message(&ServiceParams::new(), send_req(None, None))
            .await
            .unwrap();
        let Some(Ok(StreamResponse::Task(task))) = events.next().await else {
            panic!("the executor announces its Task");
        };
        let queue = handler.tasks.queue(&task.id);
        let weak = Arc::downgrade(&queue);
        drop(queue);
        assert!(
            weak.upgrade().is_some(),
            "the running invocation retains its state"
        );
        release.notify_one();
        let output = timeout(Duration::from_secs(2), events.collect::<Vec<_>>())
            .await
            .unwrap();
        assert!(output.iter().any(|event| matches!(event,
            Ok(StreamResponse::Task(task)) if task.status.state == TaskState::Completed)));
        timeout(Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("finished invocations must not retain a queue cycle");
    }
}

#[cfg(test)]
mod routing_tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_requests_share_the_task_and_operation_queue() {
        let registry = make_handler().tasks;
        let barrier = Arc::new(tokio::sync::Barrier::new(32));
        let mut requests = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let registry = registry.clone();
            let barrier = barrier.clone();
            requests.spawn(async move {
                barrier.wait().await;
                let task = registry.task("shared");
                let operations = task.operations();
                (task, operations)
            });
        }
        let (task, operations) = requests.join_next().await.unwrap().unwrap();
        while let Some(result) = requests.join_next().await {
            let (other_task, other_operations) = result.unwrap();
            assert!(Arc::ptr_eq(&task, &other_task));
            assert!(Arc::ptr_eq(&operations, &other_operations));
        }
        let (active, _inputs, _state) = task.reserve().unwrap();
        assert!(Arc::ptr_eq(&active.task, &task));
        assert!(Arc::ptr_eq(
            &registry.task("shared").active().unwrap(),
            &active
        ));
        assert!(!Arc::ptr_eq(&task, &registry.task("different")));
    }

    #[tokio::test]
    async fn releasing_all_task_users_retires_its_worker_and_handle() {
        let registry = make_handler().tasks;
        let task = registry.task("retired");
        let weak = Arc::downgrade(&task);
        let watch = task.watch();
        let operations = task.operations();
        super::super::runtime::submit(
            operations.clone(),
            super::super::runtime::TaskOperation::Subscribe,
            |_| Box::pin(async {}),
        )
        .await
        .unwrap();
        drop(operations);
        drop(task);
        assert!(weak.upgrade().is_some(), "the watcher retains the task");
        drop(watch);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the handle and worker must not form a reference cycle");
        let _next = registry.task("next");
        assert!(!registry.handles.read().contains_key("retired"));
    }

    /// A reservation whose request was dropped (its release still on its way)
    /// is replaced by the next request instead of being mistaken for a running
    /// execution.
    #[test]
    fn test_reserve_replaces_a_dropped_requests_leftover() {
        let manager = make_handler().tasks;
        let (stale, _inputs, _invocation) = manager.task("t-stale").reserve().unwrap();

        let (fresh, _inputs, _invocation) = manager
            .task("t-stale")
            .reserve()
            .expect("the leftover is not a running execution");

        assert!(!Arc::ptr_eq(&stale, &fresh));
        let registered = manager.task("t-stale").active().unwrap();
        assert!(Arc::ptr_eq(&registered, &fresh));
        // A stale routing handle must not remove its successor.
        manager.task("t-stale").unregister(&stale);
        assert!(manager.task("t-stale").active().is_some());
    }

    #[tokio::test]
    async fn test_stream_disconnect_preserves_execution_for_resubscription() {
        use futures::StreamExt;

        let (handler, release) = make_resumable_handler();
        let params = ServiceParams::new();
        let mut message = make_message();
        message.task_id = Some("t-disconnect".into());
        message.context_id = Some("c-disconnect".into());
        seed_task(&handler, "t-disconnect", "c-disconnect").await;

        let mut stream = handler
            .send_streaming_message(
                &params,
                SendMessageRequest {
                    artifact_updates: Vec::new(),
                    message,
                    configuration: None,
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let initial = stream.next().await.unwrap().unwrap();
        match initial {
            StreamResponse::Task(task) => assert_eq!(task.status.state, TaskState::Working),
            _ => panic!("expected initial working task"),
        }
        drop(stream);

        let mut resumed = handler
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    last_seen_generation: None,
                    id: "t-disconnect".into(),
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let snapshot = resumed.next().await.unwrap().unwrap();
        match snapshot {
            StreamResponse::Task(task) => assert_eq!(task.status.state, TaskState::Working),
            _ => panic!("expected working task snapshot"),
        }

        release.notify_waiters();

        let terminal = resumed.next().await.unwrap().unwrap();
        match terminal {
            StreamResponse::Task(task) => {
                assert_eq!(task.status.state, TaskState::Completed)
            }
            _ => panic!("expected terminal task"),
        }
        assert!(resumed.next().await.is_none());
    }

    #[tokio::test]
    async fn a_prepared_execution_cannot_be_reserved_twice() {
        let manager = make_handler().tasks;
        let (active, _inputs, _invocation) = manager.task("task").reserve().unwrap();
        let task = task_with_history("task", "context", 0);
        active.set_snapshot(task);
        assert!(manager.task("task").reserve().is_err());
        assert!(Arc::ptr_eq(
            &manager.task("task").active().unwrap(),
            &active
        ));
    }

    #[test]
    fn task_routing_remains_usable_after_panic_and_prunes_unused_handles() {
        let manager = make_handler().tasks;
        let held = manager.task("held");
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _sources = manager.handles.write();
                panic!("interrupted registry owner");
            }))
            .is_err()
        );
        for id in 0..256 {
            drop(manager.task(&format!("abandoned-{id}")));
        }
        assert!(Arc::ptr_eq(&held, &manager.task("held")));
        let sources = manager.handles.write();
        assert!(!sources.contains_key("abandoned-0"));
        assert!(sources.contains_key("held"));
    }

    #[test]
    fn dropping_the_owner_expires_the_route_even_while_the_registry_is_locked() {
        let manager = make_handler().tasks;
        let (active, _inputs, _invocation) = manager.task("dropped-owner").reserve().unwrap();
        let task = active.task.clone();
        let registry = manager.handles.read();
        drop(active);
        assert!(
            registry
                .get("dropped-owner")
                .unwrap()
                .upgrade()
                .unwrap()
                .active()
                .is_none()
        );
        drop(registry);
        assert!(task.active().is_none());
    }
}
