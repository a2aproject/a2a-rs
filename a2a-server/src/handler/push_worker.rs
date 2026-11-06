// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! One handler-wide task polls ordered, concurrent webhook HTTP futures.

use super::events::ExecutionEvent;
use super::push_delivery::PUSH_QUEUE_CAPACITY;
use super::task_handle::TaskHandle;
use a2a::*;
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use std::{collections::VecDeque, sync::Arc};
use tokio::sync::mpsc;

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
        (self.config.task_id.clone(), self.config.url.clone())
    }

    fn fail(&self, error: A2AError) {
        tracing::warn!(task_id = %self.config.task_id, config_id = ?self.config.id, %error, "webhook delivery failed");
        signal_failure(&self.sender, &self.event, error);
    }

    async fn deliver(self) -> Endpoint {
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

pub(super) struct PushDispatcher {
    queue: mpsc::Sender<Notification>,
}

impl PushDispatcher {
    pub(super) fn spawn(background_tasks: &super::background_tasks::BackgroundTasks) -> Self {
        let (queue, receiver) = mpsc::channel(PUSH_QUEUE_CAPACITY);
        background_tasks.spawn(dispatch(receiver, MAX_PUSH_DELIVERIES, PUSH_QUEUE_CAPACITY));
        Self { queue }
    }

    pub(super) async fn submit(&self, notification: Notification) -> bool {
        if let Err(error) = self.queue.send(notification).await {
            error
                .0
                .fail(A2AError::internal("webhook queue unavailable"));
            return false;
        }
        true
    }

    #[cfg(test)]
    pub(super) fn channel(capacity: usize) -> (Self, mpsc::Receiver<Notification>) {
        let (queue, receiver) = mpsc::channel(capacity);
        (Self { queue }, receiver)
    }
}

async fn dispatch(mut queue: mpsc::Receiver<Notification>, concurrency: usize, capacity: usize) {
    let mut pending = VecDeque::<Notification>::new();
    // At most `concurrency` keys are busy; scanning them borrows URL strings.
    let mut busy = Vec::<Endpoint>::new();
    let mut requests = FuturesUnordered::<BoxFuture<'static, Endpoint>>::new();
    let mut closed = false;
    loop {
        // Start the oldest eligible job. A slow endpoint never holds up other
        // keys, and only one HTTP request per Task/URL can run at once.
        while requests.len() < concurrency {
            let Some(index) = pending.iter().position(|job| {
                !busy
                    .iter()
                    .any(|(task, url)| task == &job.config.task_id && url == &job.config.url)
            }) else {
                break;
            };
            let job = pending
                .remove(index)
                .expect("index came from the pending queue");
            busy.push(job.endpoint());
            requests.push(Box::pin(job.deliver()));
        }
        if closed && requests.is_empty() {
            break;
        }
        tokio::select! {
            completed = requests.next(), if !requests.is_empty() => {
                let completed = completed.expect("nonempty request set");
                busy.retain(|endpoint| endpoint != &completed);
            }
            next = queue.recv(), if !closed && pending.len() < capacity => {
                match next {
                    Some(job) => pending.push_back(job),
                    None => closed = true,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::protocol::EventKind;
    use crate::handler::test_support::*;
    use axum::Json;

    type Seen = (TaskId, i64, Option<oneshot::Sender<()>>);

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
        assert!(dispatcher.queue.is_closed());
        drop(release);
        server.abort();
    }

    #[tokio::test]
    async fn one_dispatcher_preserves_task_url_order_while_other_keys_progress() {
        let sender = sender();
        let (url, mut seen, server) = endpoint(false).await;
        let source = TaskHandle::new("task".into(), None);
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
                        &source
                    ))
                    .await
            );
        }
        drop(source);
        drop(queue);
        let delivery = tokio::spawn(dispatch(receiver, 2, 8));
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
        let source = TaskHandle::new("task".into(), None);
        let (queue, receiver) = PushDispatcher::channel(8);
        for task in ["a", "b", "c", "d"] {
            queue
                .submit(notification(task, 1, &url, &sender, &source))
                .await;
        }
        drop(queue);
        let delivery = tokio::spawn(dispatch(receiver, 2, 8));
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
        let delivery = tokio::spawn(dispatch(receiver, 1, 1));
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
}
