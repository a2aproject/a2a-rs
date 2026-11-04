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
    pub(super) source: Arc<TaskHandle>,
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
        if let Ok(event) = &self.event.result
            && let Err(error) = self.sender.send_push(&self.config, event.clone()).await
        {
            self.fail(error);
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
    pub(super) fn spawn(workers: &super::workers::Workers) -> Self {
        let (queue, receiver) = mpsc::channel(PUSH_QUEUE_CAPACITY);
        workers.spawn(dispatch(receiver, MAX_PUSH_DELIVERIES, PUSH_QUEUE_CAPACITY));
        Self { queue }
    }

    pub(super) fn submit(&self, notification: Notification) -> bool {
        match self.queue.try_send(notification) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(job)) => {
                job.fail(A2AError::internal("webhook queue full"));
                true
            }
            Err(mpsc::error::TrySendError::Closed(job)) => {
                job.fail(A2AError::internal("webhook queue unavailable"));
                false
            }
        }
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
            next = queue.recv(), if !closed => {
                match next {
                    Some(job) if pending.len() < capacity => pending.push_back(job),
                    Some(job) => job.fail(A2AError::internal("webhook pending queue full")),
                    None => closed = true,
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::events::EventKind;
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
        source: &Arc<TaskHandle>,
    ) -> Notification {
        let mut task = task_with_history(task_id, "context", 0);
        task.generation = generation;
        let mut config = push_config(task_id);
        config.url = url.into();
        Notification {
            config,
            event: Arc::new(ExecutionEvent {
                sequence: generation as u64,
                kind: EventKind::Normal,
                result: Ok(StreamResponse::Task(task)),
                failure: None,
            }),
            sender: sender.clone(),
            source: source.clone(),
        }
    }

    #[tokio::test]
    async fn shutdown_cancels_in_flight_http_and_releases_queued_task_handles() {
        let workers = Arc::new(super::super::workers::Workers::default());
        let sender = sender();
        let (url, mut seen, server) = endpoint(true).await;
        let source = TaskHandle::with_workers("task".into(), None, workers.clone());
        let weak = Arc::downgrade(&source);
        let dispatcher = PushDispatcher::spawn(&workers);
        assert!(dispatcher.submit(notification("task", 1, &url, &sender, &source)));
        assert!(dispatcher.submit(notification("task", 2, &url, &sender, &source)));
        let (_, _, release) = timeout(Duration::from_secs(2), seen.recv())
            .await
            .unwrap()
            .unwrap();
        drop(source);
        assert!(
            weak.upgrade().is_some(),
            "in-flight and queued notifications retain their Task"
        );
        timeout(Duration::from_secs(1), workers.shutdown())
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
        let retained = Arc::downgrade(&source);
        let (queue, receiver) = PushDispatcher::channel(8);
        for (task, generation, path) in [
            ("a", 1, "/slow"),
            ("a", 2, "/slow"),
            ("b", 3, "/slow"),
            ("a", 4, "/fast"),
        ] {
            assert!(queue.submit(notification(
                task,
                generation,
                &format!("{url}{path}"),
                &sender,
                &source
            )));
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
            queue.submit(notification(task, 1, &url, &sender, &source));
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
    async fn pending_work_is_bounded_and_overflow_signals_the_execution() {
        let sender = sender();
        let (url, mut seen, server) = endpoint(true).await;
        let source = TaskHandle::new("task".into(), None);
        let (failure, mut observed) = tokio::sync::watch::channel(None);
        let (queue, receiver) = PushDispatcher::channel(8);
        for generation in 1..=3 {
            let mut job = notification("task", generation, &url, &sender, &source);
            Arc::get_mut(&mut job.event).unwrap().failure = Some(failure.clone());
            queue.submit(job);
        }
        drop(queue);
        let delivery = tokio::spawn(dispatch(receiver, 1, 1));
        timeout(Duration::from_secs(2), observed.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(
            observed
                .borrow()
                .as_ref()
                .unwrap()
                .message
                .contains("pending queue full")
        );
        for generation in [1, 2] {
            let (_, received, release) = timeout(Duration::from_secs(2), seen.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(received, generation);
            release.unwrap().send(()).unwrap();
        }
        timeout(Duration::from_secs(2), delivery)
            .await
            .unwrap()
            .unwrap();
        assert!(
            seen.try_recv().is_err(),
            "the rejected notification was never sent"
        );
        server.abort();
    }
}
