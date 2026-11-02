// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Ordered per-endpoint delivery, with a handler-wide HTTP concurrency bound.

use super::events::ExecutionEvent;
use super::protocol::is_at_least_1_1;
use super::push_delivery::{NotificationSettings, PUSH_QUEUE_CAPACITY};
use a2a::*;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::mpsc;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

pub(super) fn signal_failure(
    settings: &NotificationSettings,
    event: &ExecutionEvent,
    error: A2AError,
) {
    if settings.sender.fail_on_error()
        && let Some(signal) = &event.failure
    {
        signal.send_replace(Some(error));
    }
}

pub(super) async fn deliver_notifications(
    settings: NotificationSettings,
    mut events: mpsc::Receiver<ExecutionEvent>,
    previous: Option<CancellationToken>,
    done: CancellationToken,
) {
    let _done = done.drop_guard();
    if let Some(previous) = previous {
        previous.cancelled().await;
    }
    let mut endpoints =
        HashMap::<String, mpsc::Sender<(TaskPushNotificationConfig, Arc<ExecutionEvent>)>>::new();
    let mut workers = JoinSet::new();
    while let Some(event) = events.recv().await {
        while let Some(result) = workers.try_join_next() {
            if let Err(error) = result {
                tracing::error!(task_id = %settings.task_id, %error, "webhook worker failed");
            }
        }
        let configs = match settings.store.list(&settings.task_id).await {
            Ok(configs) => configs,
            Err(error) => {
                signal_failure(&settings, &event, error.clone());
                tracing::warn!(task_id = %settings.task_id, %error, "could not load webhook configurations");
                continue;
            }
        };
        // Keep one queued payload even when many endpoints subscribe to it.
        let event = Arc::new(event);
        for config in configs {
            if matches!(event.result, Ok(StreamResponse::MessageUpdate(_)))
                && !is_at_least_1_1(config.protocol_version.as_deref().unwrap_or("1.0"))
            {
                continue;
            }
            let tx = endpoints.entry(config.url.clone()).or_insert_with(|| {
                let (tx, rx) = mpsc::channel(PUSH_QUEUE_CAPACITY);
                workers.spawn(deliver_endpoint(settings.clone(), rx));
                tx
            });
            if let Err(error) = tx.try_send((config, event.clone())) {
                tracing::warn!(task_id = %settings.task_id, %error, "webhook endpoint queue unavailable; dropping a notification");
                signal_failure(
                    &settings,
                    &event,
                    A2AError::internal("webhook endpoint queue unavailable"),
                );
            }
        }
    }
    drop(endpoints);
    while let Some(result) = workers.join_next().await {
        if let Err(error) = result {
            tracing::error!(task_id = %settings.task_id, %error, "webhook worker failed");
        }
    }
}

async fn deliver_endpoint(
    settings: NotificationSettings,
    mut jobs: mpsc::Receiver<(TaskPushNotificationConfig, Arc<ExecutionEvent>)>,
) {
    while let Some((config, event)) = jobs.recv().await {
        // One queue per Task/URL preserves ordering across configuration IDs.
        // Await permits here, never in broadcast intake or task mutation.
        let Ok(_permit) = settings.permits.acquire().await else {
            return;
        };
        if let Ok(item) = &event.result
            && let Err(error) = settings.sender.send_push(&config, item.clone()).await
        {
            tracing::warn!(task_id = %settings.task_id, config_id = ?config.id, %error, "webhook delivery failed");
            signal_failure(&settings, &event, error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PushConfigStore;
    use crate::handler::events::EventKind;
    use crate::handler::test_support::*;
    use axum::Json;

    #[tokio::test]
    async fn slow_endpoints_do_not_block_others_and_each_endpoint_keeps_order() {
        install_crypto_provider();
        let (slow_tx, mut slow_rx) = mpsc::unbounded_channel();
        let (fast_tx, mut fast_rx) = mpsc::unbounded_channel();
        let app = Router::new()
            .route(
                "/slow",
                post(move |Json(event): Json<StreamResponse>| {
                    let tx = slow_tx.clone();
                    async move {
                        let StreamResponse::Task(task) = event else {
                            panic!("expected Task");
                        };
                        let (release, wait) = tokio::sync::oneshot::channel();
                        tx.send((task.generation, release)).unwrap();
                        let _ = wait.await;
                        StatusCode::OK
                    }
                }),
            )
            .route(
                "/fast",
                post(move |Json(event): Json<StreamResponse>| {
                    let tx = fast_tx.clone();
                    async move {
                        let StreamResponse::Task(task) = event else {
                            panic!("expected Task");
                        };
                        tx.send(task.generation).unwrap();
                        StatusCode::OK
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let store = Arc::new(InMemoryPushConfigStore::new());
        for name in ["slow", "fast"] {
            let mut config = push_config("task");
            config.id = Some(name.into());
            config.url = format!("{url}/{name}");
            store.save(config).await.unwrap();
        }
        let settings = NotificationSettings {
            task_id: "task".into(),
            store,
            sender: Arc::new(crate::HttpPushSender::new(Some(
                crate::HttpPushSenderConfig {
                    validate_urls: false,
                    timeout: Duration::from_secs(5),
                    fail_on_error: true,
                },
            ))),
            permits: Arc::new(tokio::sync::Semaphore::new(2)),
        };
        let (tx, rx) = mpsc::channel(4);
        let done = CancellationToken::new();
        let previous = CancellationToken::new();
        let mut delivery = Box::pin(deliver_notifications(
            settings,
            rx,
            Some(previous.clone()),
            done.clone(),
        ));
        for generation in [1, 2] {
            let mut task = task_with_history("task", "context", 0);
            task.generation = generation;
            tx.send(ExecutionEvent {
                sequence: generation as u64,
                result: Ok(StreamResponse::Task(task)),
                kind: EventKind::Normal,
                failure: None,
            })
            .await
            .unwrap();
        }
        drop(tx);
        assert!(futures::poll!(delivery.as_mut()).is_pending());
        assert!(slow_rx.try_recv().is_err());
        assert!(
            fast_rx.try_recv().is_err(),
            "successor work overtook its predecessor"
        );
        previous.cancel();
        let delivery = tokio::spawn(delivery);
        let (first, release) = tokio::time::timeout(Duration::from_secs(2), slow_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first, 1);
        for expected in [1, 2] {
            assert_eq!(
                tokio::time::timeout(Duration::from_secs(2), fast_rx.recv())
                    .await
                    .unwrap(),
                Some(expected)
            );
        }
        assert!(
            slow_rx.try_recv().is_err(),
            "a later delivery overtook the blocked first request"
        );
        assert!(
            !done.is_cancelled(),
            "retirement must wait for accepted HTTP deliveries"
        );
        release.send(()).unwrap();
        let (second, release) = tokio::time::timeout(Duration::from_secs(2), slow_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(second, 2);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), delivery)
            .await
            .unwrap()
            .unwrap();
        assert!(done.is_cancelled());
        server.abort();
    }
}
