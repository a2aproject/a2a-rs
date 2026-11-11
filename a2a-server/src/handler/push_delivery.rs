// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Queue bounded webhook batches without blocking the task controller.

use super::events::ExecutionEvent;
use super::protocol::EventKind;
use super::push_worker::{Notification, PushDispatcher, signal_failure};
use super::task_handle::TaskHandle;
use a2a::*;
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
    time::Duration,
};
use tokio::sync::Mutex;

pub(super) const PUSH_QUEUE_CAPACITY: usize = 1024;
const PUSH_CONFIG_TIMEOUT: Duration = Duration::from_secs(30);

type DeliveryGates = parking_lot::Mutex<HashMap<(TaskId, String), Weak<Mutex<()>>>>;

/// One configured webhook service, shared by handler clones and task handles.
#[derive(Clone)]
pub(super) struct PushService {
    store: Arc<dyn crate::PushConfigStore>,
    sender: Arc<crate::HttpPushSender>,
    dispatcher: Arc<std::sync::OnceLock<PushDispatcher>>,
    gates: Arc<DeliveryGates>,
}

impl PushService {
    pub(super) fn new(
        store: impl crate::PushConfigStore,
        sender: crate::HttpPushSender,
    ) -> Arc<Self> {
        Arc::new(Self {
            store: Arc::new(store),
            sender: Arc::new(sender),
            dispatcher: Arc::default(),
            gates: Arc::default(),
        })
    }

    pub(super) fn sender(&self) -> &crate::HttpPushSender {
        &self.sender
    }

    pub(super) fn store(&self) -> &dyn crate::PushConfigStore {
        self.store.as_ref()
    }

    pub(super) fn validate_config(
        &self,
        config: &TaskPushNotificationConfig,
    ) -> Result<(), A2AError> {
        self.sender.validate_config(config)
    }

    pub(super) async fn save_pending(
        &self,
        pending: &mut Option<TaskPushNotificationConfig>,
    ) -> Result<(), A2AError> {
        if let Some(config) = pending.as_ref() {
            self.store.save(config.clone()).await?;
            *pending = None;
        }
        Ok(())
    }

    pub(super) fn gate(&self, task: &str, id: &str) -> Arc<Mutex<()>> {
        let mut gates = self.gates.lock();
        let key = (task.to_owned(), id.to_owned());
        if let Some(gate) = gates.get(&key).and_then(Weak::upgrade) {
            return gate;
        }
        gates.retain(|_, gate| gate.strong_count() > 0);
        let gate = Arc::new(Mutex::new(()));
        gates.insert(key, Arc::downgrade(&gate));
        gate
    }

    pub(super) async fn delete(&self, task: &str, id: &str) -> Result<(), A2AError> {
        // Wait for this registration's in-flight request; queued jobs recheck
        // storage under the same gate before starting any HTTP request.
        let gate = self.gate(task, id);
        let _guard = gate.lock().await;
        self.store.delete(task, id).await
    }

    pub(super) fn attach(self: &Arc<Self>, task: &TaskHandle) {
        let _ = task.configure_pushes(self.clone());
    }
}

/// Bounded batches reserved before task mutations enter the controller.
/// Two finalization slots let a committed terminal state bypass ordinary load.
pub(super) struct PublicationQueue {
    sender: tokio::sync::mpsc::UnboundedSender<PublicationBatch>,
    regular: Arc<tokio::sync::Semaphore>,
    terminal: Arc<tokio::sync::Semaphore>,
}

pub(super) struct PublicationBatch {
    source: TaskHandle,
    events: Vec<ExecutionEvent>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

pub(super) type PublicationPermit = tokio::sync::OwnedSemaphorePermit;

impl PublicationQueue {
    pub(super) fn new(source: &TaskHandle) -> Self {
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        source.background_tasks().spawn(receive_batches(receiver));
        Self {
            sender,
            regular: Arc::new(tokio::sync::Semaphore::new(
                super::EXECUTION_BUFFER_CAPACITY,
            )),
            terminal: Arc::new(tokio::sync::Semaphore::new(2)),
        }
    }

    pub(super) fn try_reserve(&self) -> Option<PublicationPermit> {
        self.regular.clone().try_acquire_owned().ok()
    }

    pub(super) async fn reserve(
        &self,
        source: &TaskHandle,
        operation: super::task_handle::TaskOperation,
    ) -> Result<Option<PublicationPermit>, A2AError> {
        use super::task_handle::TaskOperation;
        let capacity = match operation {
            TaskOperation::Subscribe | TaskOperation::Cancel => return Ok(None),
            TaskOperation::Input => {
                return self
                    .regular
                    .clone()
                    .try_acquire_owned()
                    .map(Some)
                    .map_err(|_| {
                        A2AError::invalid_request(
                            "webhook publication queue is full; input was not admitted",
                        )
                    });
            }
            TaskOperation::Output => self.regular.clone(),
            TaskOperation::Finished => self.terminal.clone(),
        };
        let execution = (matches!(operation, TaskOperation::Output))
            .then(|| source.active().map(|active| active.inputs.closed.clone()))
            .flatten();
        tokio::select! {
            biased;
            () = source.background_tasks().cancelled() => Err(A2AError::internal("handler is shutting down")),
            () = async { match execution { Some(token) => token.cancelled().await, None => std::future::pending().await } } => Err(A2AError::internal("execution stopped while waiting for publication capacity")),
            permit = capacity.acquire_owned() => permit.map(Some).map_err(|_| A2AError::internal("webhook publication queue closed")),
        }
    }

    pub(super) fn submit(
        &self,
        source: TaskHandle,
        events: Vec<ExecutionEvent>,
        permit: PublicationPermit,
    ) {
        if let Err(error) = self.sender.send(PublicationBatch {
            source,
            events,
            _permit: permit,
        }) {
            let batch = error.0;
            if let Some(settings) = batch.source.pushes() {
                for event in &batch.events {
                    signal_failure(
                        &settings.sender,
                        event,
                        A2AError::internal("webhook intake unavailable"),
                    );
                }
            }
        }
    }
}

pub(super) fn needs_notification(event: &ExecutionEvent) -> bool {
    event.kind == EventKind::Normal
        && event
            .result
            .as_ref()
            .is_ok_and(|event| super::protocol::task_id_for_event(event).is_some())
}

/// Clients observe committed events immediately. Webhooks own reserved queue
/// capacity and never await configuration or HTTP inside the task controller.
pub(super) async fn broadcast(
    source: &TaskHandle,
    settings: &Arc<PushService>,
    event: ExecutionEvent,
) {
    let _ = source.sender().send(event.clone());
    if !needs_notification(&event) {
        return;
    }
    if source.buffer_notification(event.clone()) {
        return;
    }
    // Direct publishers (outside a task operation) may backpressure their own
    // future. The controller reserves capacity before executing its operation.
    match source
        .publications()
        .reserve(source, super::task_handle::TaskOperation::Output)
        .await
    {
        Ok(Some(permit)) => source
            .publications()
            .submit(source.clone(), vec![event], permit),
        Ok(None) => unreachable!("output publication reserves capacity"),
        Err(error) => signal_failure(&settings.sender, &event, error),
    }
}

async fn receive_batches(mut batches: tokio::sync::mpsc::UnboundedReceiver<PublicationBatch>) {
    while let Some(batch) = batches.recv().await {
        let Some(settings) = batch.source.pushes() else {
            continue;
        };
        for event in &batch.events {
            // A failed lookup affects this event, not the remaining queue.
            // Every accepted event is either submitted or explicitly failed.
            let configs = tokio::time::timeout(PUSH_CONFIG_TIMEOUT, async {
                loop {
                    match settings.store.list(batch.source.id()).await {
                        Ok(configs) => break configs,
                        Err(error) => {
                            signal_failure(&settings.sender, event, error.clone());
                            tracing::warn!(task_id = %batch.source.id(), %error, "could not load webhook configurations; retrying");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            }).await;
            let configs = match configs {
                Ok(configs) => configs,
                Err(_) => {
                    signal_failure(
                        &settings.sender,
                        event,
                        A2AError::internal("webhook configuration lookup timed out"),
                    );
                    continue;
                }
            };
            let event = Arc::new(event.clone());
            for config in configs {
                if matches!(event.result, Ok(StreamResponse::MessageUpdate(_)))
                    && !super::protocol::is_at_least_1_1(
                        config.protocol_version.as_deref().unwrap_or("1.0"),
                    )
                {
                    continue;
                }
                if !settings
                    .dispatcher
                    .get_or_init(|| PushDispatcher::spawn(batch.source.background_tasks()))
                    .submit(Notification {
                        config,
                        event: event.clone(),
                        sender: settings.sender.clone(),
                        source: batch.source.clone(),
                    })
                    .await
                {
                    signal_failure(
                        &settings.sender,
                        &event,
                        A2AError::internal("webhook dispatcher unavailable"),
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn a_closed_publication_queue_reports_failure_for_each_accepted_event() {
        let handler = make_handler().with_push_notifications(
            InMemoryPushConfigStore::new(),
            crate::HttpPushSender::new(Some(crate::HttpPushSenderConfig {
                fail_on_error: true,
                ..Default::default()
            })),
        );
        let source = handler.tasks.task("task");
        handler.attach_pushes(&source);
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        drop(receiver);
        let queue = PublicationQueue {
            sender,
            regular: Arc::new(tokio::sync::Semaphore::new(1)),
            terminal: Arc::new(tokio::sync::Semaphore::new(2)),
        };
        let mut signals = Vec::new();
        let events = (0..2)
            .map(|sequence| {
                let (failure, receiver) = tokio::sync::watch::channel(None);
                signals.push(receiver);
                ExecutionEvent {
                    sequence,
                    input_generation: 0,
                    result: Ok(StreamResponse::Task(task_with_history(
                        "task", "context", 0,
                    ))),
                    kind: EventKind::Normal,
                    failure: Some(failure),
                    response_task: None,
                }
            })
            .collect();
        queue.submit(source.clone(), events, queue.try_reserve().unwrap());
        for signal in signals {
            assert!(
                signal
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .message
                    .contains("intake unavailable")
            );
        }
        // A closed capacity semaphore is surfaced to direct publishers too.
        source.publications().regular.close();
        let (failure, signal) = tokio::sync::watch::channel(None);
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
        assert!(
            signal
                .borrow()
                .as_ref()
                .unwrap()
                .message
                .contains("queue closed")
        );
        handler.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_and_subscriptions_bypass_a_full_webhook_queue() {
        install_crypto_provider();
        let entered = Arc::new(Notify::new());
        let handler = make_handler().with_push_notifications(
            HangingStore {
                entered: entered.clone(),
                dropped: Arc::default(),
            },
            crate::HttpPushSender::new(Some(crate::HttpPushSenderConfig {
                validate_urls: false,
                ..Default::default()
            })),
        );
        seed_task(&handler, "task", "context").await;
        let source = handler.tasks.task("task");
        let task = handler.tasks.load("task").await.unwrap();
        source.set_snapshot(task.clone());
        handler.attach_pushes(&source);
        for _ in 0..super::super::EXECUTION_BUFFER_CAPACITY {
            source
                .publish(
                    Ok(&StreamResponse::Task(task.clone())),
                    None,
                    EventKind::Normal,
                    None,
                    0,
                )
                .await;
        }
        entered.notified().await;
        let mut observed = source.sender().subscribe();
        let before = handler.tasks.load("task").await.unwrap();
        let error = handler
            .send_message(&ServiceParams::new(), send_req(Some("task"), None))
            .await
            .unwrap_err();
        assert!(error.message.contains("not admitted"));
        assert_eq!(handler.tasks.load("task").await.unwrap(), before);
        let mut subscription = timeout(
            Duration::from_secs(1),
            handler.subscribe_to_task(
                &ServiceParams::new(),
                SubscribeToTaskRequest {
                    id: "task".into(),
                    tenant: None,
                    last_seen_generation: None,
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(matches!(
            subscription.next().await,
            Some(Ok(StreamResponse::Task(_)))
        ));
        let canceled = timeout(
            Duration::from_secs(1),
            handler.cancel_task(
                &ServiceParams::new(),
                CancelTaskRequest {
                    id: "task".into(),
                    tenant: None,
                    metadata: None,
                },
            ),
        )
        .await
        .expect("cancel is independent of webhook delivery")
        .unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);
        let notification = observed.recv().await.unwrap();
        assert!(
            notification
                .result
                .as_ref()
                .is_ok_and(super::super::protocol::is_terminal_event)
        );
        handler.shutdown().await;
    }

    struct LookupGuard(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for LookupGuard {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    struct HangingStore {
        entered: Arc<Notify>,
        dropped: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait]
    impl PushConfigStore for HangingStore {
        async fn save(
            &self,
            config: TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            Ok(config)
        }
        async fn get(&self, _: &str, _: &str) -> Result<TaskPushNotificationConfig, A2AError> {
            Err(A2AError::internal("unused"))
        }
        async fn list(&self, _: &str) -> Result<Vec<TaskPushNotificationConfig>, A2AError> {
            let _guard = LookupGuard(self.dropped.clone());
            self.entered.notify_one();
            std::future::pending().await
        }
        async fn delete(&self, _: &str, _: &str) -> Result<(), A2AError> {
            Ok(())
        }
        async fn delete_all(&self, _: &str) -> Result<(), A2AError> {
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn hanging_configuration_lookups_expire_and_shutdown_drops_them_immediately() {
        for shutdown in [false, true] {
            let (configured, _queue) = captured_source(8, true).await;
            let mut settings = (**configured.pushes().unwrap()).clone();
            let entered = Arc::new(Notify::new());
            let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
            settings.store = Arc::new(HangingStore {
                entered: entered.clone(),
                dropped: dropped.clone(),
            });
            let source = TaskHandle::new("task".into(), None);
            assert!(source.configure_pushes(Arc::new(settings)).is_ok());
            let (failure, mut observed) = tokio::sync::watch::channel(None);
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
            entered.notified().await;
            if shutdown {
                source.background_tasks().shutdown().await;
            } else {
                tokio::time::advance(PUSH_CONFIG_TIMEOUT).await;
                observed.changed().await.unwrap();
                assert_eq!(
                    observed.borrow().as_ref().unwrap().message,
                    "webhook configuration lookup timed out"
                );
                tokio::task::yield_now().await;
                for _ in 0..8 {
                    tokio::task::yield_now().await;
                }
            }
            source.background_tasks().shutdown().await;
            assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
            assert_eq!(source.sender().receiver_count(), 0);
            let weak = source.downgrade();
            drop(source);
            assert!(
                weak.upgrade().is_none(),
                "neither the hanging lookup nor its worker retains the task"
            );
        }
    }

    async fn captured_source(
        capacity: usize,
        fail_on_error: bool,
    ) -> (TaskHandle, super::super::push_worker::CapturedNotifications) {
        let handler = make_push_delivery_handler();
        let source = handler.tasks.task("task");
        handler.attach_pushes(&source);
        let mut settings = (**source.pushes().unwrap()).clone();
        settings.store.save(push_config("task")).await.unwrap();
        settings.sender = Arc::new(crate::HttpPushSender::new(Some(
            crate::HttpPushSenderConfig {
                fail_on_error,
                validate_urls: false,
                ..Default::default()
            },
        )));
        let (dispatcher, receiver) = PushDispatcher::capture(capacity);
        settings.dispatcher = Arc::new(std::sync::OnceLock::from(dispatcher));
        let source = TaskHandle::new("task".into(), None);
        assert!(source.configure_pushes(Arc::new(settings)).is_ok());
        (source, receiver)
    }

    struct RecoveringStore {
        inner: Arc<dyn PushConfigStore>,
        failed: std::sync::atomic::AtomicBool,
        get_error: Option<Arc<Notify>>,
    }
    #[async_trait]
    impl PushConfigStore for RecoveringStore {
        async fn save(
            &self,
            config: TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            self.inner.save(config).await
        }
        async fn get(&self, task: &str, id: &str) -> Result<TaskPushNotificationConfig, A2AError> {
            if let Some(entered) = &self.get_error {
                entered.notify_one();
                return Err(A2AError::internal("temporary get failure"));
            }
            self.inner.get(task, id).await
        }
        async fn list(&self, task: &str) -> Result<Vec<TaskPushNotificationConfig>, A2AError> {
            if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(A2AError::internal("temporary store outage"));
            }
            self.inner.list(task).await
        }
        async fn delete(&self, task: &str, id: &str) -> Result<(), A2AError> {
            self.inner.delete(task, id).await
        }
        async fn delete_all(&self, task: &str) -> Result<(), A2AError> {
            self.inner.delete_all(task).await
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_update_survives_a_transient_configuration_lookup_failure() {
        let (configured, mut queue) = captured_source(8, false).await;
        let mut settings = (**configured.pushes().unwrap()).clone();
        settings.store = Arc::new(RecoveringStore {
            inner: settings.store.clone(),
            failed: false.into(),
            get_error: None,
        });
        let source = TaskHandle::new("task".into(), None);
        assert!(source.configure_pushes(Arc::new(settings)).is_ok());
        source
            .publish(
                Ok(&StreamResponse::Task(task_with_history(
                    "task", "context", 0,
                ))),
                None,
                EventKind::Normal,
                None,
                0,
            )
            .await;
        let notification = timeout(Duration::from_secs(2), queue.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(notification.event.sequence, 1);
        assert!(queue.try_recv().is_err());
        source.background_tasks().shutdown().await;
    }

    #[tokio::test]
    async fn configuration_lookup_retries_do_not_prevent_deletion() {
        let (configured, _queue) = captured_source(8, false).await;
        let mut settings = (**configured.pushes().unwrap()).clone();
        let entered = Arc::new(Notify::new());
        settings.store = Arc::new(RecoveringStore {
            inner: settings.store.clone(),
            failed: true.into(),
            get_error: Some(entered.clone()),
        });
        let config = settings.store.list("task").await.unwrap().remove(0);
        let settings = Arc::new(settings);
        let source = TaskHandle::new("task".into(), None);
        assert!(source.configure_pushes(settings.clone()).is_ok());
        let dispatcher = PushDispatcher::spawn(source.background_tasks());
        assert!(
            dispatcher
                .submit(Notification {
                    config: config.clone(),
                    event: Arc::new(ExecutionEvent {
                        input_generation: 0,
                        sequence: 1,
                        result: Ok(StreamResponse::Task(task_with_history(
                            "task", "context", 0
                        ))),
                        kind: EventKind::Normal,
                        failure: None,
                        response_task: None,
                    }),
                    sender: settings.sender.clone(),
                    source: source.clone(),
                })
                .await
        );
        entered.notified().await;
        timeout(
            Duration::from_millis(200),
            settings.delete("task", config.id.as_deref().unwrap()),
        )
        .await
        .unwrap()
        .unwrap();
        source.background_tasks().shutdown().await;
    }

    #[tokio::test]
    async fn direct_replies_and_errors_do_not_start_webhook_workers() {
        let handler = make_push_delivery_handler();
        let source = handler.tasks.task("uncreated");
        handler.attach_pushes(&source);
        source
            .publish(
                Ok(&StreamResponse::Message(make_message())),
                None,
                EventKind::Reply,
                None,
                0,
            )
            .await;
        source
            .publish(
                Err(&A2AError::internal("no task created")),
                None,
                EventKind::Normal,
                None,
                0,
            )
            .await;
        assert_eq!(source.background_tasks().count(), 0);
        assert_eq!(source.sender().receiver_count(), 0);
    }

    #[tokio::test]
    async fn webhook_intake_filters_private_repeated_and_non_task_events() {
        let (source, mut rx) = captured_source(8, false).await;

        let task = task_with_history("task", "context", 0);
        let mut events = vec![
            (EventKind::Repeated, Ok(StreamResponse::Task(task.clone()))),
            (
                EventKind::ResponseOnly(None),
                Ok(StreamResponse::Task(task.clone())),
            ),
            (
                EventKind::Reply,
                Ok(StreamResponse::Message(make_message())),
            ),
            (
                EventKind::Normal,
                Err(A2AError::internal("execution failed")),
            ),
            (
                EventKind::Normal,
                Ok(StreamResponse::Error(StreamError {
                    task_id: "task".into(),
                    code: "UNAVAILABLE".into(),
                    message: "private rejection".into(),
                    reference_id: None,
                    metadata: None,
                })),
            ),
        ];
        events.push((EventKind::Normal, Ok(StreamResponse::Task(task))));
        for (kind, result) in events {
            source.publish(result.as_ref(), None, kind, None, 0).await;
        }
        assert_eq!(
            timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .unwrap()
                .event
                .sequence,
            6
        );
        assert!(rx.try_recv().is_err());
        source.background_tasks().shutdown().await;
    }

    #[tokio::test]
    async fn shared_broadcast_backpressures_without_losing_webhook_updates() {
        let (source, mut rx) = captured_source(1, true).await;
        let total = super::super::EXECUTION_BUFFER_CAPACITY * 3 + 4;
        let (mut ordinary, _, _) = source.resubscribe();
        let producer_source = source.clone();
        let mut producer = tokio::spawn(async move {
            for index in 0..total {
                producer_source
                    .publish(
                        Ok(&StreamResponse::Task(task_with_history(
                            "task", "context", 0,
                        ))),
                        None,
                        if index % 3 == 0 {
                            EventKind::Normal
                        } else {
                            EventKind::Repeated
                        },
                        None,
                        0,
                    )
                    .await;
            }
        });
        assert!(
            timeout(Duration::from_millis(30), &mut producer)
                .await
                .is_err()
        );
        for sequence in (1..=total).step_by(3) {
            let notification = timeout(Duration::from_secs(2), rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(notification.event.sequence, sequence as u64);
        }
        producer.await.unwrap();
        assert!(
            matches!(
                ordinary.try_recv(),
                Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
            ),
            "ordinary subscribers can lag without holding up publication"
        );
        source.background_tasks().shutdown().await;
    }

    #[tokio::test]
    async fn webhook_failure_wakes_an_executor_waiting_for_more_input() {
        install_crypto_provider();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let app = Router::new().route("/", post(|| async { StatusCode::INTERNAL_SERVER_ERROR }));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let handler = DefaultRequestHandler::new(
            ResumableExecutor {
                release: Arc::new(Notify::new()),
            },
            InMemoryTaskStore::new(),
        )
        .with_capabilities(AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        })
        .with_push_notifications(
            InMemoryPushConfigStore::new(),
            crate::HttpPushSender::new(Some(crate::HttpPushSenderConfig {
                timeout: Duration::from_secs(1),
                fail_on_error: true,
                validate_urls: false,
            })),
        );
        seed_task(&handler, "push-failure", "context").await;
        handler
            .create_push_config(
                &ServiceParams::new(),
                TaskPushNotificationConfig {
                    protocol_version: Some("1.1".into()),
                    task_id: "push-failure".into(),
                    url,
                    id: None,
                    token: None,
                    authentication: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        let mut events = handler
            .send_streaming_message(&ServiceParams::new(), send_req(Some("push-failure"), None))
            .await
            .unwrap();
        let error = timeout(Duration::from_secs(3), async {
            loop {
                match events.next().await {
                    Some(Ok(_)) => {}
                    Some(Err(error)) => break error,
                    None => panic!("the webhook failure must be reported"),
                }
            }
        })
        .await
        .expect("webhook failure must wake the driver without another executor event");
        assert_eq!(error.code, error_code::INTERNAL_ERROR);
        assert!(handler.tasks.task("push-failure").active().is_none());
        server.abort();
    }

    #[tokio::test]
    async fn test_push_config_not_supported_without_store() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let result = handler
            .create_push_config(
                &params,
                TaskPushNotificationConfig {
                    protocol_version: None,
                    task_id: "t1".into(),
                    url: "http://example.com".into(),
                    id: None,
                    token: None,
                    authentication: None,
                    tenant: None,
                },
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_push_configs_invalid_page_token_errors() {
        install_crypto_provider();
        let handler = make_handler_with_push_configs();
        seed_task(&handler, "t1", "c1").await;
        let params = ServiceParams::new();
        handler
            .create_push_config(
                &params,
                TaskPushNotificationConfig {
                    protocol_version: None,
                    task_id: "t1".into(),
                    url: "https://example.com/callback".into(),
                    id: Some("cfg-1".into()),
                    token: None,
                    authentication: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let result = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: None,
                    page_token: Some("not-a-number".into()),
                    tenant: None,
                },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn test_list_push_configs_page_size_cap_bounds_a_custom_store() {
        install_crypto_provider();
        // InMemoryPushConfigStore caps a task at 50 configs, below the page
        // cap of 100, so it can never reach the clamp. A store without that
        // limit can, and is what the clamp exists for.
        let handler = DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
            .with_push_config_store(PagingPushConfigStore::new(MAX_PAGE_SIZE * 3));
        seed_task(&handler, "t1", "c1").await;

        let resp = handler
            .list_push_configs(
                &ServiceParams::new(),
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: Some(1_000_000),
                    page_token: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(resp.configs.len(), MAX_PAGE_SIZE);
        // The remainder must stay reachable, or the cap silently truncates.
        assert_eq!(resp.next_page_token.as_deref(), Some("100"));
    }

    #[tokio::test]
    async fn test_list_push_configs_page_size_is_capped() {
        install_crypto_provider();
        let handler = make_handler_with_push_configs();
        seed_task(&handler, "t1", "c1").await;
        let params = ServiceParams::new();
        // Keep the count below InMemoryPushConfigStore's per-task cap
        // (MAX_PUSH_CONFIGS_PER_TASK), which rejects the 51st config.
        for i in 0..40 {
            handler
                .create_push_config(
                    &params,
                    TaskPushNotificationConfig {
                        protocol_version: None,
                        task_id: "t1".into(),
                        url: format!("https://example.com/callback/{i}"),
                        id: Some(format!("cfg-{i:03}")),
                        token: None,
                        authentication: None,
                        tenant: None,
                    },
                )
                .await
                .unwrap();
        }

        // Requesting a huge page size must not error and returns everything
        // available. This does NOT exercise the cap -- 40 < MAX_PAGE_SIZE, so
        // a capped and an uncapped implementation agree here. The cap itself
        // is pinned by test_list_push_configs_page_size_cap_bounds_a_custom_store,
        // which is the only route that can reach it.
        let resp = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: Some(1000),
                    page_token: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(resp.configs.len(), 40);
    }

    #[tokio::test]
    async fn test_push_config_crud_with_store() {
        install_crypto_provider();
        let handler = make_handler_with_push_configs();
        seed_task(&handler, "t1", "c1").await;
        let params = ServiceParams::new();

        let created = handler
            .create_push_config(
                &params,
                TaskPushNotificationConfig {
                    protocol_version: None,
                    task_id: "t1".into(),
                    url: "https://example.com/first".into(),
                    id: Some("cfg-1".into()),
                    token: None,
                    authentication: None,
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(created.id.as_deref(), Some("cfg-1"));

        handler
            .create_push_config(
                &params,
                TaskPushNotificationConfig {
                    protocol_version: None,
                    task_id: "t1".into(),
                    url: "https://example.com/second".into(),
                    id: Some("cfg-2".into()),
                    token: None,
                    authentication: None,
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();

        let fetched = handler
            .get_push_config(
                &params,
                GetTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    id: "cfg-1".into(),
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(fetched.url, "https://example.com/first");

        let first_page = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: Some(1),
                    page_token: Some("0".into()),
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(first_page.configs.len(), 1);
        assert_eq!(first_page.configs[0].id.as_deref(), Some("cfg-1"));
        assert_eq!(first_page.next_page_token.as_deref(), Some("1"));

        let default_page = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: Some(0),
                    page_token: None,
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(default_page.configs.len(), 2);
        assert!(default_page.next_page_token.is_none());

        handler
            .delete_push_config(
                &params,
                DeleteTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    id: "cfg-1".into(),
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();

        let remaining = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: None,
                    page_token: None,
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(remaining.configs.len(), 1);
        assert_eq!(remaining.configs[0].id.as_deref(), Some("cfg-2"));
    }

    #[tokio::test]
    async fn test_send_message_request_push_config_delivers_webhooks() {
        install_crypto_provider();
        let (url, mut receiver, shutdown_tx, server) = start_push_webhook().await;
        let handler = make_push_delivery_handler();
        let params = ServiceParams::new();
        let mut message = make_message();
        message.task_id = Some("t-push-request".into());
        message.context_id = Some("c-push-request".into());
        seed_task(&handler, "t-push-request", "c-push-request").await;

        let response = handler
            .send_message(
                &params,
                SendMessageRequest {
                    artifact_updates: Vec::new(),
                    message: Some(message),
                    configuration: Some(SendMessageConfiguration {
                        if_generation_match: None,
                        timeline_length: None,
                        accepted_output_modes: None,
                        task_push_notification_config: Some(TaskPushNotificationConfig {
                            protocol_version: None,
                            task_id: String::new(),
                            url: url.clone(),
                            id: Some("cfg-request".into()),
                            token: Some("notify-token".into()),
                            authentication: Some(AuthenticationInfo {
                                scheme: "bearer".into(),
                                credentials: Some("secret-token".into()),
                            }),
                            tenant: None,
                        }),
                        history_length: None,
                        return_immediately: None,
                    }),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        match response {
            SendMessageResponse::Task(task) => assert_eq!(task.status.state, TaskState::Completed),
            _ => panic!("expected Task response"),
        }

        let saved = handler
            .get_push_config(
                &params,
                GetTaskPushNotificationConfigRequest {
                    task_id: "t-push-request".into(),
                    id: "cfg-request".into(),
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(saved.url, url);

        let first = next_push(&mut receiver).await;
        assert_eq!(first.authorization.as_deref(), Some("Bearer secret-token"));
        assert_eq!(first.notification_token.as_deref(), Some("notify-token"));
        match first.event {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.task_id, "t-push-request");
                assert_eq!(update.status.state, TaskState::Working);
            }
            _ => panic!("expected status update push"),
        }

        let second = next_push(&mut receiver).await;
        assert_eq!(second.authorization.as_deref(), Some("Bearer secret-token"));
        assert_eq!(second.notification_token.as_deref(), Some("notify-token"));
        match second.event {
            StreamResponse::Task(task) => {
                assert_eq!(task.id, "t-push-request");
                assert_eq!(task.status.state, TaskState::Completed);
            }
            _ => panic!("expected final task push"),
        }

        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_send_message_stored_push_config_delivers_webhooks() {
        install_crypto_provider();
        let (url, mut receiver, shutdown_tx, server) = start_push_webhook().await;
        let handler = make_push_delivery_handler();
        let params = ServiceParams::new();

        seed_task(&handler, "t-push-stored", "c-push-stored").await;
        handler
            .create_push_config(
                &params,
                TaskPushNotificationConfig {
                    protocol_version: None,
                    task_id: "t-push-stored".into(),
                    url: url.clone(),
                    id: Some("cfg-stored".into()),
                    token: Some("stored-token".into()),
                    authentication: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let mut message = make_message();
        message.task_id = Some("t-push-stored".into());
        message.context_id = Some("c-push-stored".into());
        let response = handler
            .send_message(
                &params,
                SendMessageRequest {
                    artifact_updates: Vec::new(),
                    message: Some(message),
                    configuration: None,
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        match response {
            SendMessageResponse::Task(task) => assert_eq!(task.status.state, TaskState::Completed),
            _ => panic!("expected Task response"),
        }

        let first = next_push(&mut receiver).await;
        assert_eq!(first.notification_token.as_deref(), Some("stored-token"));
        match first.event {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.task_id, "t-push-stored");
                assert_eq!(update.status.state, TaskState::Working);
            }
            _ => panic!("expected status update push"),
        }

        let second = next_push(&mut receiver).await;
        assert_eq!(second.notification_token.as_deref(), Some("stored-token"));
        match second.event {
            StreamResponse::Task(task) => {
                assert_eq!(task.id, "t-push-stored");
                assert_eq!(task.status.state, TaskState::Completed);
            }
            _ => panic!("expected final task push"),
        }

        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[test]
    fn test_with_push_config_store_enables_push_capability() {
        install_crypto_provider();
        let handler = make_handler_with_push_configs();
        assert_eq!(handler.capabilities.push_notifications, Some(true));
    }

    #[tokio::test]
    async fn test_push_refused_when_the_card_declares_it_absent() {
        // A store *is* configured; the card declaring `false` is what refuses.
        // Before #203 this combination served push regardless.
        let handler = make_handler_with_push_configs().with_capabilities(caps(None, Some(false)));
        let params = ServiceParams::new();

        let error = handler
            .create_push_config(&params, push_config("t1"))
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);

        let error = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: None,
                    page_token: None,
                    tenant: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);
    }

    /// `with_push_notifications` defaults the capability the same way as
    /// `with_push_config_store`, and likewise must not override an explicit
    /// `false`. It had no unit test before #203 — only an example — so the
    /// two builders could have drifted apart unnoticed.
    #[tokio::test]
    async fn test_with_push_notifications_defaults_the_capability() {
        let params = ServiceParams::new();

        let handler = make_handler().with_push_notifications(
            InMemoryPushConfigStore::new(),
            crate::HttpPushSender::new(None),
        );
        seed_task(&handler, "t1", "c1").await;
        assert_eq!(handler.capabilities.push_notifications, Some(true));
        assert!(
            handler
                .create_push_config(&params, push_config("t1"))
                .await
                .is_ok()
        );

        let handler = make_handler()
            .with_capabilities(caps(None, Some(false)))
            .with_push_notifications(
                InMemoryPushConfigStore::new(),
                crate::HttpPushSender::new(None),
            );
        assert_eq!(handler.capabilities.push_notifications, Some(false));
        let error = handler
            .create_push_config(&params, push_config("t1"))
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);
    }

    /// A store that was never supplied still reports the same code — that is
    /// the deployment state, distinct from the card's declaration.
    #[tokio::test]
    async fn test_missing_store_still_reports_push_unsupported() {
        let handler = make_handler();
        let error = handler
            .create_push_config(&ServiceParams::new(), push_config("t1"))
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);
    }

    #[tokio::test]
    async fn webhook_intake_does_not_retain_an_idle_task_handle() {
        let (source, mut notifications) = captured_source(2, false).await;
        let weak = source.downgrade();
        let background = source.background_tasks().clone();
        source
            .publish(
                Ok(&StreamResponse::Task(task_with_history(
                    "task", "context", 0,
                ))),
                None,
                EventKind::Normal,
                None,
                0,
            )
            .await;
        drop(notifications.recv().await.unwrap());
        drop(source);
        drop(notifications);
        timeout(Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        background.shutdown().await;
    }

    #[tokio::test]
    async fn a_closed_dispatcher_reports_every_accepted_notification() {
        let (source, receiver) = captured_source(2, true).await;
        drop(receiver);
        let (failure, mut observed) = tokio::sync::watch::channel(None);
        for generation in 1..=2 {
            source
                .publish(
                    Ok(&StreamResponse::Task(task_with_history(
                        "task", "context", generation,
                    ))),
                    None,
                    EventKind::Normal,
                    Some(failure.clone()),
                    0,
                )
                .await;
            timeout(Duration::from_secs(2), observed.changed())
                .await
                .unwrap()
                .unwrap();
            assert!(observed.borrow().is_some());
        }
        source.background_tasks().shutdown().await;
    }
}
