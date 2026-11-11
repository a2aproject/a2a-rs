// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! One handler-wide task polls ordered, concurrent webhook HTTP futures.

use super::events::ExecutionEvent;
use super::push_delivery::PUSH_QUEUE_CAPACITY;
use super::task_handle::{TaskHandle, WeakTaskHandle};
use a2a::*;
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::{collections::VecDeque, sync::Arc, task::Poll};
use tokio::sync::{Notify, mpsc};

const MAX_PUSH_DELIVERIES: usize = 16;
type Endpoint = (TaskId, String);

pub(super) struct Notification {
    pub(super) config: TaskPushNotificationConfig,
    pub(super) event: Arc<ExecutionEvent>,
    pub(super) sender: Arc<crate::HttpPushSender>,
    // Retain the source across intake retirement until accepted work drains.
    pub(super) source: TaskHandle,
}

impl Notification {
    fn endpoint(&self) -> Endpoint {
        (self.source.id().clone(), self.config.url.clone())
    }

    fn fail(&self, error: A2AError) {
        tracing::warn!(task_id = %self.config.task_id, config_id = ?self.config.id, %error, "webhook delivery failed");
        signal_failure(&self.sender, &self.event, error);
    }

    async fn deliver(self) -> Endpoint {
        let endpoint = self.endpoint();
        let sender = self.sender.clone();
        let event = self.event.clone();
        if tokio::time::timeout(sender.delivery_timeout(), self.deliver_inner())
            .await
            .is_err()
        {
            let error = A2AError::internal("webhook delivery deadline expired");
            tracing::warn!(task_id = %endpoint.0, %error);
            signal_failure(&sender, &event, error);
        }
        endpoint
    }

    async fn deliver_inner(self) -> Endpoint {
        let endpoint = self.endpoint();
        let gate = self.source.pushes().map(|settings| {
            settings.gate(
                &self.config.task_id,
                self.config.id.as_deref().unwrap_or_default(),
            )
        });
        loop {
            let guard = match &gate {
                Some(gate) => Some(gate.lock().await),
                None => None,
            };
            if let Some(settings) = self.source.pushes() {
                let current = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    settings.store().get(
                        &self.config.task_id,
                        self.config.id.as_deref().unwrap_or_default(),
                    ),
                )
                .await
                .unwrap_or_else(|_| {
                    Err(A2AError::internal("webhook configuration lookup timed out"))
                });
                match current {
                    Ok(current) if current == self.config => {}
                    Ok(_) => return endpoint,
                    Err(error) if error.code == error_code::TASK_NOT_FOUND => return endpoint,
                    Err(error) => {
                        // Let deletion proceed between failed lookups. A store
                        // outage must not hold this registration's gate forever.
                        drop(guard);
                        self.fail(error);
                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                        continue;
                    }
                }
            }
            if let Ok(event) = &self.event.result
                && let Err(error) = self.sender.send_push(&self.config, event.clone()).await
            {
                self.fail(error);
            }
            break;
        }
        // This lease also keeps a successor intake on the same Task source.
        drop(self.source);
        endpoint
    }
}

pub(super) fn signal_failure(
    sender: &crate::HttpPushSender,
    event: &ExecutionEvent,
    error: A2AError,
) {
    if sender.fail_on_error()
        && let Some(signal) = &event.failure
    {
        signal.send_replace(Some(error));
    }
}

/// Bounded notification storage for one task, shared by its concurrent producers.
pub(super) struct TaskPushQueue {
    sender: mpsc::Sender<Notification>,
}

impl TaskPushQueue {
    pub(super) fn is_closed(&self) -> bool {
        self.sender.is_closed()
    }
    pub(super) fn is_empty(&self) -> bool {
        self.sender.capacity() == self.sender.max_capacity()
    }
}

struct TaskQueue {
    id: TaskId,
    source: WeakTaskHandle,
    queue: Arc<TaskPushQueue>,
    receiver: mpsc::Receiver<Notification>,
    pending: VecDeque<Notification>,
}

pub(super) struct PushDispatcher {
    // This channel registers queues, not notification payloads. A task handle
    // registers at most one live queue; payload capacity is bounded per task.
    registrations: mpsc::UnboundedSender<TaskQueue>,
    released: Arc<Notify>,
    capacity: usize,
}

struct WakeOnRelease(Arc<Notify>);

impl Drop for WakeOnRelease {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

impl PushDispatcher {
    pub(super) fn spawn(background_tasks: &super::background_tasks::BackgroundTasks) -> Self {
        let (dispatcher, receiver) = Self::channel(PUSH_QUEUE_CAPACITY);
        background_tasks.spawn(dispatch(
            receiver,
            dispatcher.released.clone(),
            MAX_PUSH_DELIVERIES,
        ));
        dispatcher
    }

    pub(super) async fn submit(&self, notification: Notification) -> bool {
        // Locals drop in reverse order, including when this future is canceled:
        // release the producer's queue Arc before waking the retirement check.
        let _wake = WakeOnRelease(self.released.clone());
        let Some(queue) = notification.source.push_queue(self) else {
            notification.fail(A2AError::internal("webhook queue unavailable"));
            return false;
        };
        if let Err(error) = queue.sender.send(notification).await {
            error
                .0
                .fail(A2AError::internal("webhook queue unavailable"));
            return false;
        }
        true
    }

    fn channel(capacity: usize) -> (Self, mpsc::UnboundedReceiver<TaskQueue>) {
        let (registrations, receiver) = mpsc::unbounded_channel();
        (
            Self {
                registrations,
                released: Arc::new(Notify::new()),
                capacity,
            },
            receiver,
        )
    }

    pub(super) fn register(&self, source: &TaskHandle) -> Option<Arc<TaskPushQueue>> {
        let (sender, receiver) = mpsc::channel(self.capacity);
        let queue = Arc::new(TaskPushQueue { sender });
        self.registrations
            .send(TaskQueue {
                id: source.id().clone(),
                source: source.downgrade(),
                queue: queue.clone(),
                receiver,
                pending: VecDeque::new(),
            })
            .ok()?;
        Some(queue)
    }

    #[cfg(test)]
    pub(super) fn capture(capacity: usize) -> (Self, CapturedNotifications) {
        let (dispatcher, registrations) = Self::channel(capacity);
        (
            dispatcher,
            CapturedNotifications {
                registrations,
                queues: VecDeque::new(),
            },
        )
    }
}

async fn next_notification(queues: &mut VecDeque<TaskQueue>) {
    futures::future::poll_fn(|cx| {
        for index in 0..queues.len() {
            let queue = &mut queues[index];
            if queue.pending.len() >= queue.queue.sender.max_capacity() {
                continue;
            }
            if let Poll::Ready(Some(notification)) = queue.receiver.poll_recv(cx) {
                queue.pending.push_back(notification);
                // A continuously ready producer must not monopolize intake.
                queues.rotate_left(index + 1);
                return Poll::Ready(());
            }
        }
        Poll::Pending
    })
    .await
}

async fn dispatch(
    mut registrations: mpsc::UnboundedReceiver<TaskQueue>,
    released: Arc<Notify>,
    concurrency: usize,
) {
    let mut queues = VecDeque::<TaskQueue>::new();
    // At most `concurrency` keys are busy; scanning them borrows URL strings.
    let mut busy = Vec::<Endpoint>::new();
    let mut requests = FuturesUnordered::<BoxFuture<'static, Endpoint>>::new();
    let mut closed = false;
    // A fan-out on one task cannot occupy the entire handler pool. Keep at
    // least two URLs concurrent for a task when the pool can support them.
    let per_task = (concurrency / 2).max(2).min(concurrency);
    loop {
        // Rotate tasks after each opportunity. Within a task, skip busy URLs
        // without letting later work overtake an earlier job for the same URL.
        let mut scheduled = false;
        for _ in 0..queues.len() {
            if requests.len() >= concurrency {
                break;
            }
            let mut queue = queues.pop_front().unwrap();
            let index = queue.pending.iter().position(|job| {
                busy.iter()
                    .filter(|(task, _)| task == job.source.id())
                    .count()
                    < per_task
                    && !busy
                        .iter()
                        .any(|(task, url)| task == job.source.id() && url == &job.config.url)
            });
            if let Some(index) = index {
                let job = queue.pending.remove(index).unwrap();
                busy.push(job.endpoint());
                requests.push(Box::pin(job.deliver()));
                scheduled = true;
            }
            queues.push_back(queue);
        }
        if scheduled && requests.len() < concurrency {
            continue;
        }
        queues.retain(|queue| {
            if !queue.pending.is_empty() || busy.iter().any(|(task, _)| task == &queue.id) {
                return true;
            }
            queue
                .source
                .upgrade()
                .is_some_and(|source| !source.retire_push_queue(&queue.queue))
        });
        if closed && requests.is_empty() && queues.is_empty() {
            break;
        }
        tokio::select! {
            completed = requests.next(), if !requests.is_empty() => {
                let completed = completed.expect("nonempty request set");
                busy.retain(|endpoint| endpoint != &completed);
            }
            next = registrations.recv(), if !closed => {
                match next {
                    Some(queue) => queues.push_back(queue),
                    None => closed = true,
                }
            }
            () = next_notification(&mut queues) => {},
            () = released.notified() => {},
        }
    }
}

/// Test intake directly through the real task queues without starting HTTP work.
#[cfg(test)]
pub(super) struct CapturedNotifications {
    registrations: mpsc::UnboundedReceiver<TaskQueue>,
    queues: VecDeque<TaskQueue>,
}

#[cfg(test)]
impl CapturedNotifications {
    pub(super) async fn recv(&mut self) -> Option<Notification> {
        futures::future::poll_fn(|cx| {
            while let Poll::Ready(Some(queue)) = self.registrations.poll_recv(cx) {
                self.queues.push_back(queue);
            }
            for queue in &mut self.queues {
                if let Poll::Ready(Some(notification)) = queue.receiver.poll_recv(cx) {
                    return Poll::Ready(Some(notification));
                }
            }
            if self.registrations.is_closed() {
                Poll::Ready(None)
            } else {
                Poll::Pending
            }
        })
        .await
    }

    pub(super) fn try_recv(&mut self) -> Result<Notification, mpsc::error::TryRecvError> {
        while let Ok(queue) = self.registrations.try_recv() {
            self.queues.push_back(queue);
        }
        for queue in &mut self.queues {
            if let Ok(notification) = queue.receiver.try_recv() {
                return Ok(notification);
            }
        }
        Err(if self.registrations.is_closed() {
            mpsc::error::TryRecvError::Disconnected
        } else {
            mpsc::error::TryRecvError::Empty
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::protocol::EventKind;
    use crate::handler::test_support::*;
    use axum::Json;

    type Seen = (TaskId, i64, Option<oneshot::Sender<()>>);

    #[tokio::test(start_paused = true)]
    async fn a_permanent_config_lookup_failure_releases_its_http_slot() {
        struct FailingGet;
        #[async_trait::async_trait]
        impl crate::PushConfigStore for FailingGet {
            async fn get(&self, _: &str, _: &str) -> Result<TaskPushNotificationConfig, A2AError> {
                Err(A2AError::internal("store unavailable"))
            }
            async fn save(
                &self,
                value: TaskPushNotificationConfig,
            ) -> Result<TaskPushNotificationConfig, A2AError> {
                Ok(value)
            }
            async fn list(&self, _: &str) -> Result<Vec<TaskPushNotificationConfig>, A2AError> {
                Ok(vec![])
            }
            async fn delete(&self, _: &str, _: &str) -> Result<(), A2AError> {
                Ok(())
            }
            async fn delete_all(&self, _: &str) -> Result<(), A2AError> {
                Ok(())
            }
        }
        install_crypto_provider();
        let sender = Arc::new(crate::HttpPushSender::new(Some(
            crate::HttpPushSenderConfig {
                timeout: Duration::from_secs(1),
                validate_urls: false,
                ..Default::default()
            },
        )));
        let source = TaskHandle::new("task".into(), None);
        source
            .configure_pushes(super::super::push_delivery::PushService::new(
                FailingGet,
                crate::HttpPushSender::new(Some(crate::HttpPushSenderConfig {
                    validate_urls: false,
                    ..Default::default()
                })),
            ))
            .ok()
            .unwrap();
        let job = notification("task", 1, "https://example.com/hook", &sender, &source);
        let done = timeout(Duration::from_secs(2), job.deliver())
            .await
            .expect("an unavailable config store cannot hold a delivery slot forever");
        assert_eq!(done.0, "task");
    }

    #[tokio::test]
    async fn one_tasks_slow_fanout_cannot_fill_all_http_slots() {
        install_crypto_provider();
        let sender = Arc::new(crate::HttpPushSender::new(Some(
            crate::HttpPushSenderConfig {
                timeout: Duration::from_secs(10),
                validate_urls: false,
                ..Default::default()
            },
        )));
        let (url, mut seen, server) = endpoint(true).await;
        let source = TaskHandle::new("a".into(), None);
        let other = TaskHandle::new("b".into(), None);
        let (queue, receiver) = PushDispatcher::channel(32);
        for generation in 1..=16 {
            queue
                .submit(notification(
                    "a",
                    generation,
                    &format!("{url}/{generation}"),
                    &sender,
                    &source,
                ))
                .await;
        }
        let delivery = tokio::spawn(dispatch(receiver, queue.released.clone(), 16));
        let mut releases = Vec::new();
        for _ in 0..8 {
            releases.push(
                timeout(Duration::from_secs(2), seen.recv())
                    .await
                    .unwrap()
                    .unwrap()
                    .2,
            );
        }
        queue
            .submit(notification("b", 1, &format!("{url}/b"), &sender, &other))
            .await;
        let (task, _, release) = timeout(Duration::from_secs(2), seen.recv())
            .await
            .expect("another task must get a slot before the slow fan-out completes")
            .unwrap();
        assert_eq!(task, "b");
        drop(release);
        drop(releases);
        for _ in 0..8 {
            let (_, _, release) = timeout(Duration::from_secs(2), seen.recv())
                .await
                .unwrap()
                .unwrap();
            drop(release);
        }
        drop(queue);
        timeout(Duration::from_secs(2), delivery)
            .await
            .unwrap()
            .unwrap();
        server.abort();
    }

    async fn endpoint(
        gate_all: bool,
    ) -> (
        String,
        mpsc::UnboundedReceiver<Seen>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sent, received) = mpsc::unbounded_channel();
        let app = Router::new().fallback(post(move |Json(event): Json<StreamResponse>| {
            let sent = sent.clone();
            async move {
                let StreamResponse::Task(task) = event else {
                    panic!("expected Task")
                };
                if gate_all || (task.id == "a" && task.generation == 1) {
                    let (release, wait) = oneshot::channel();
                    sent.send((task.id, task.generation, Some(release)))
                        .unwrap();
                    let _ = wait.await;
                } else {
                    sent.send((task.id, task.generation, None)).unwrap();
                }
                StatusCode::NO_CONTENT
            }
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, received, server)
    }

    fn sender() -> Arc<crate::HttpPushSender> {
        install_crypto_provider();
        Arc::new(crate::HttpPushSender::new(Some(
            crate::HttpPushSenderConfig {
                validate_urls: false,
                fail_on_error: true,
                timeout: Duration::from_secs(5),
            },
        )))
    }

    fn notification(
        task_id: &str,
        generation: i64,
        url: &str,
        sender: &Arc<crate::HttpPushSender>,
        source: &TaskHandle,
    ) -> Notification {
        let mut task = task_with_history(task_id, "context", 0);
        task.generation = generation;
        let mut config = push_config(task_id);
        config.url = url.into();
        Notification {
            config,
            event: Arc::new(ExecutionEvent {
                input_generation: 0,
                sequence: generation as u64,
                kind: EventKind::Normal,
                result: Ok(StreamResponse::Task(task)),
                failure: None,
                response_task: None,
            }),
            sender: sender.clone(),
            source: source.clone(),
        }
    }

    #[tokio::test]
    async fn shutdown_cancels_in_flight_http_and_releases_queued_task_handles() {
        let background_tasks = Arc::new(super::super::background_tasks::BackgroundTasks::default());
        let sender = sender();
        let (url, mut seen, server) = endpoint(true).await;
        let source =
            TaskHandle::with_background_tasks("task".into(), None, background_tasks.clone());
        let weak = source.downgrade();
        let dispatcher = PushDispatcher::spawn(&background_tasks);
        assert!(
            dispatcher
                .submit(notification("task", 1, &url, &sender, &source))
                .await
        );
        assert!(
            dispatcher
                .submit(notification("task", 2, &url, &sender, &source))
                .await
        );
        let (_, _, release) = timeout(Duration::from_secs(2), seen.recv())
            .await
            .unwrap()
            .unwrap();
        drop(source);
        assert!(
            weak.upgrade().is_some(),
            "in-flight and queued notifications retain their Task"
        );
        timeout(Duration::from_secs(1), background_tasks.shutdown())
            .await
            .expect("shutdown must not wait for the HTTP request timeout");
        assert!(
            weak.upgrade().is_none(),
            "shutdown drops both HTTP futures and queued notifications"
        );
        assert!(dispatcher.registrations.is_closed());
        drop(release);
        server.abort();
    }

    #[tokio::test]
    async fn one_dispatcher_preserves_task_url_order_while_other_keys_progress() {
        let sender = sender();
        let (url, mut seen, server) = endpoint(false).await;
        let source = TaskHandle::new("a".into(), None);
        let other = TaskHandle::new("b".into(), None);
        let retained = source.downgrade();
        let (queue, receiver) = PushDispatcher::channel(8);
        for (task, generation, path) in [
            ("a", 1, "/slow"),
            ("a", 2, "/slow"),
            ("b", 3, "/slow"),
            ("a", 4, "/fast"),
        ] {
            assert!(
                queue
                    .submit(notification(
                        task,
                        generation,
                        &format!("{url}{path}"),
                        &sender,
                        if task == "a" { &source } else { &other }
                    ))
                    .await
            );
        }
        drop(source);
        let delivery = tokio::spawn(dispatch(receiver, queue.released.clone(), 2));
        drop(queue);
        let mut release_first = None;
        let mut independent = Vec::new();
        for _ in 0..3 {
            let (task, generation, release) = timeout(Duration::from_secs(2), seen.recv())
                .await
                .unwrap()
                .unwrap();
            if generation == 1 {
                release_first = release;
            } else {
                independent.push((task, generation));
            }
        }
        independent.sort();
        assert_eq!(independent, [("a".into(), 4), ("b".into(), 3)]);
        assert!(
            seen.try_recv().is_err(),
            "the second a/slow delivery cannot overtake its first"
        );
        assert!(
            retained.upgrade().is_some(),
            "accepted work retains its Task source"
        );
        release_first.unwrap().send(()).unwrap();
        let (task, generation, _) = timeout(Duration::from_secs(2), seen.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((task.as_str(), generation), ("a", 2));
        timeout(Duration::from_secs(2), delivery)
            .await
            .unwrap()
            .unwrap();
        assert!(
            retained.upgrade().is_none(),
            "drained work releases the source"
        );
        server.abort();
    }

    #[tokio::test]
    async fn the_http_concurrency_limit_applies_across_tasks_and_drains_on_close() {
        let sender = sender();
        let (url, mut seen, server) = endpoint(true).await;
        let (queue, receiver) = PushDispatcher::channel(8);
        for task in ["a", "b", "c", "d"] {
            let source = TaskHandle::new(task.into(), None);
            queue
                .submit(notification(task, 1, &url, &sender, &source))
                .await;
        }
        let delivery = tokio::spawn(dispatch(receiver, queue.released.clone(), 2));
        drop(queue);
        let (_, _, first) = timeout(Duration::from_secs(2), seen.recv())
            .await
            .unwrap()
            .unwrap();
        let (_, _, second) = timeout(Duration::from_secs(2), seen.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            timeout(Duration::from_millis(50), seen.recv())
                .await
                .is_err(),
            "no third HTTP request may start while two are in flight"
        );
        first.unwrap().send(()).unwrap();
        let (_, _, third) = timeout(Duration::from_secs(2), seen.recv())
            .await
            .unwrap()
            .unwrap();
        second.unwrap().send(()).unwrap();
        let (_, _, fourth) = timeout(Duration::from_secs(2), seen.recv())
            .await
            .unwrap()
            .unwrap();
        third.unwrap().send(()).unwrap();
        fourth.unwrap().send(()).unwrap();
        timeout(Duration::from_secs(2), delivery)
            .await
            .unwrap()
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn pending_work_backpressures_and_every_notification_is_attempted() {
        let sender = sender();
        let (url, mut seen, server) = endpoint(true).await;
        let source = TaskHandle::new("task".into(), None);
        let (queue, receiver) = PushDispatcher::channel(1);
        let delivery = tokio::spawn(dispatch(receiver, queue.released.clone(), 1));
        let producer = tokio::spawn(async move {
            for generation in 1..=5 {
                assert!(
                    queue
                        .submit(notification("task", generation, &url, &sender, &source))
                        .await
                );
            }
        });
        for generation in 1..=5 {
            let (_, received, release) = timeout(Duration::from_secs(2), seen.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(received, generation);
            release.unwrap().send(()).unwrap();
        }
        producer.await.unwrap();
        timeout(Duration::from_secs(2), delivery)
            .await
            .unwrap()
            .unwrap();
        server.abort();
    }

    #[tokio::test]
    async fn a_saturated_task_does_not_block_another_tasks_queue_or_delivery() {
        let sender = sender();
        let (url, mut seen, server) = endpoint(false).await;
        let source = TaskHandle::new("a".into(), None);
        let other = TaskHandle::new("b".into(), None);
        let retained = source.downgrade();
        let (dispatcher, receiver) = PushDispatcher::channel(1);
        let dispatcher = Arc::new(dispatcher);
        let delivery = tokio::spawn(dispatch(receiver, dispatcher.released.clone(), 2));
        let (saturated, waiting) = oneshot::channel();
        let producer = tokio::spawn({
            let dispatcher = dispatcher.clone();
            let sender = sender.clone();
            let url = url.clone();
            async move {
                let mut saturated = Some(saturated);
                for generation in 1..=6 {
                    // One job is in HTTP, one pending and one in ingress. The
                    // fourth send cannot finish until the first HTTP completes.
                    if generation == 4 {
                        saturated.take().unwrap().send(()).unwrap();
                    }
                    assert!(
                        dispatcher
                            .submit(notification("a", generation, &url, &sender, &source))
                            .await
                    );
                }
            }
        });
        let (task, generation, release) = timeout(Duration::from_secs(2), seen.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!((task.as_str(), generation), ("a", 1));
        timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(!producer.is_finished(), "task a must remain backpressured");
        assert!(
            timeout(
                Duration::from_secs(2),
                dispatcher.submit(notification("b", 1, &url, &sender, &other))
            )
            .await
            .expect("task b admission must not wait for task a")
        );
        let (task, generation, _) = timeout(Duration::from_secs(2), seen.recv())
            .await
            .expect("task b delivery must not wait for task a")
            .unwrap();
        assert_eq!((task.as_str(), generation), ("b", 1));
        assert!(!producer.is_finished());
        release.unwrap().send(()).unwrap();
        for generation in 2..=6 {
            let (task, received, _) = timeout(Duration::from_secs(2), seen.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!((task.as_str(), received), ("a", generation));
        }
        producer.await.unwrap();
        drop(dispatcher);
        timeout(Duration::from_secs(2), delivery)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.upgrade().is_none());
        server.abort();
    }

    #[tokio::test]
    async fn drained_queues_retire_and_later_submissions_register_again() {
        let sender = sender();
        let (url, mut seen, server) = endpoint(false).await;
        let source = TaskHandle::new("task".into(), None);
        let retained = source.downgrade();
        let (dispatcher, receiver) = PushDispatcher::channel(1);
        let delivery = tokio::spawn(dispatch(receiver, dispatcher.released.clone(), 1));
        for generation in 1..=20 {
            let queue = source.push_queue(&dispatcher).unwrap();
            let retired = Arc::downgrade(&queue);
            drop(queue);
            assert!(
                dispatcher
                    .submit(notification("task", generation, &url, &sender, &source))
                    .await
            );
            let (_, received, _) = timeout(Duration::from_secs(2), seen.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(received, generation);
            timeout(Duration::from_secs(2), async {
                while retired.upgrade().is_some() {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("drained queues must retire while the task remains alive");
        }
        drop(source);
        drop(dispatcher);
        timeout(Duration::from_secs(2), delivery)
            .await
            .unwrap()
            .unwrap();
        assert!(retained.upgrade().is_none());
        server.abort();
    }

    #[tokio::test]
    async fn releasing_the_last_producer_wakes_an_idle_retirement_check() {
        let source = TaskHandle::new("task".into(), None);
        let (dispatcher, receiver) = PushDispatcher::channel(1);
        let release = WakeOnRelease(dispatcher.released.clone());
        let queue = source.push_queue(&dispatcher).unwrap();
        let retired = Arc::downgrade(&queue);
        let mut delivery = Box::pin(dispatch(receiver, dispatcher.released.clone(), 1));
        // Force the worker to check this empty queue while its producer still
        // holds an Arc, then park with no HTTP or notification activity.
        assert!(futures::poll!(delivery.as_mut()).is_pending());
        assert!(retired.upgrade().is_some());
        drop(queue);
        drop(release);
        assert!(futures::poll!(delivery.as_mut()).is_pending());
        assert!(retired.upgrade().is_none());
        drop(dispatcher);
        assert!(futures::poll!(delivery.as_mut()).is_ready());
    }

    #[tokio::test]
    async fn shutdown_releases_a_producer_waiting_on_a_full_task_queue() {
        let background_tasks = Arc::new(super::super::background_tasks::BackgroundTasks::default());
        let sender = sender();
        let (url, mut seen, server) = endpoint(true).await;
        let source =
            TaskHandle::with_background_tasks("task".into(), None, background_tasks.clone());
        let retained = source.downgrade();
        let (dispatcher, receiver) = PushDispatcher::channel(1);
        let dispatcher = Arc::new(dispatcher);
        background_tasks.spawn(dispatch(receiver, dispatcher.released.clone(), 1));
        let (saturated, waiting) = oneshot::channel();
        let producer = tokio::spawn({
            let dispatcher = dispatcher.clone();
            async move {
                for generation in 1..=3 {
                    assert!(
                        dispatcher
                            .submit(notification("task", generation, &url, &sender, &source))
                            .await
                    );
                }
                saturated.send(()).unwrap();
                dispatcher
                    .submit(notification("task", 4, &url, &sender, &source))
                    .await
            }
        });
        let (_, _, release) = timeout(Duration::from_secs(2), seen.recv())
            .await
            .unwrap()
            .unwrap();
        timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(!producer.is_finished());
        timeout(Duration::from_secs(1), background_tasks.shutdown())
            .await
            .expect("shutdown must cancel queued and in-flight work");
        assert!(
            !timeout(Duration::from_secs(1), producer)
                .await
                .expect("shutdown must release blocked producers")
                .unwrap()
        );
        assert!(retained.upgrade().is_none());
        assert!(dispatcher.registrations.is_closed());
        drop(release);
        server.abort();
    }
}
