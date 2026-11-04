// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Subscribe to the task broadcast without awaiting webhook network I/O.

use super::DefaultRequestHandler;
use super::events::{EventKind, ExecutionEvent};
use super::push_worker::{Notification, PushDispatcher, signal_failure};
use super::task_handle::TaskHandle;
use a2a::*;
use std::{
    sync::{Arc, MutexGuard},
    time::Duration,
};
use tokio::sync::broadcast;

pub(super) const PUSH_QUEUE_CAPACITY: usize = 1024;
const PUSH_WORKER_IDLE: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub(super) struct NotificationSettings {
    pub(super) task_id: TaskId,
    pub(super) store: Arc<dyn crate::PushConfigStore>,
    pub(super) sender: Arc<crate::HttpPushSender>,
    pub(super) dispatcher: Arc<std::sync::OnceLock<PushDispatcher>>,
}

#[derive(Default)]
pub(super) struct SubscriptionState {
    running: bool,
}

#[derive(Default)]
pub(super) struct PushSubscriptions {
    dispatcher: Arc<std::sync::OnceLock<PushDispatcher>>,
}

impl PushSubscriptions {
    pub(super) fn attach(
        &self,
        handler: &DefaultRequestHandler,
        task_id: &str,
        events: &TaskHandle,
    ) {
        if let (Some(store), Some(sender)) = (&handler.push_config_store, &handler.push_sender) {
            let _ = events.notifications.set(NotificationSettings {
                task_id: task_id.into(),
                store: store.clone(),
                sender: sender.clone(),
                dispatcher: self.dispatcher.clone(),
            });
        }
    }
}

/// Called immediately before broadcast send. Holding the returned guard closes
/// the retirement race: the receiver is registered until this send completes.
pub(super) fn subscribe(events: &Arc<TaskHandle>) -> MutexGuard<'_, SubscriptionState> {
    let mut state = events
        .webhook_subscribed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if events.notifications.get().is_some() && !state.running {
        let receiver = events.sender.subscribe();
        state.running = true;
        tokio::spawn(receive_events(events.clone(), receiver));
    }
    state
}

async fn receive_events(
    events: Arc<TaskHandle>,
    mut receiver: broadcast::Receiver<ExecutionEvent>,
) {
    'events: loop {
        let result = match tokio::time::timeout(PUSH_WORKER_IDLE, receiver.recv()).await {
            Ok(result) => result,
            Err(_) => {
                let mut state = events
                    .webhook_subscribed
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                match receiver.try_recv() {
                    Ok(event) => Ok(event),
                    Err(broadcast::error::TryRecvError::Lagged(skipped)) => {
                        Err(broadcast::error::RecvError::Lagged(skipped))
                    }
                    Err(_) => {
                        state.running = false;
                        return;
                    }
                }
            }
        };
        match result {
            Ok(event)
                if event.kind == EventKind::Normal
                    && event
                        .result
                        .as_ref()
                        .is_ok_and(|event| super::protocol::task_id_for_event(event).is_some()) =>
            {
                let settings = events.notifications.get().unwrap();
                let configs = match settings.store.list(&settings.task_id).await {
                    Ok(configs) => configs,
                    Err(error) => {
                        signal_failure(&settings.sender, &event, error.clone());
                        tracing::warn!(task_id = %settings.task_id, %error, "could not load webhook configurations");
                        continue;
                    }
                };
                let event = Arc::new(event);
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
                        .get_or_init(PushDispatcher::spawn)
                        .submit(Notification {
                            config,
                            event: event.clone(),
                            sender: settings.sender.clone(),
                            source: events.clone(),
                        })
                    {
                        events
                            .webhook_subscribed
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .running = false;
                        break 'events;
                    }
                }
            }
            Ok(_) => {}
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(task_id = %events.notifications.get().unwrap().task_id, skipped, "webhook subscriber lagged; notifications were lost");
            }
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    async fn captured_source(
        capacity: usize,
        fail_on_error: bool,
    ) -> (Arc<TaskHandle>, mpsc::Receiver<Notification>) {
        let handler = make_push_delivery_handler();
        let source = handler.tasks.task("task");
        handler.tasks.pushes.attach(&handler, "task", &source);
        let mut settings = source.notifications.get().unwrap().clone();
        settings.store.save(push_config("task")).await.unwrap();
        settings.sender = Arc::new(crate::HttpPushSender::new(Some(
            crate::HttpPushSenderConfig {
                fail_on_error,
                validate_urls: false,
                ..Default::default()
            },
        )));
        let (dispatcher, receiver) = PushDispatcher::channel(capacity);
        settings.dispatcher = Arc::new(std::sync::OnceLock::from(dispatcher));
        let source = TaskHandle::new("task".into(), None);
        assert!(source.notifications.set(settings).is_ok());
        (source, receiver)
    }

    #[tokio::test]
    async fn direct_replies_and_errors_do_not_start_webhook_workers() {
        let handler = make_push_delivery_handler();
        let source = handler.tasks.task("uncreated");
        handler.tasks.pushes.attach(&handler, "uncreated", &source);
        source.publish(
            Ok(&StreamResponse::Message(make_message())),
            None,
            EventKind::Reply,
            None,
        );
        source.publish(
            Err(&A2AError::internal("no task created")),
            None,
            EventKind::Normal,
            None,
        );
        assert!(!source.webhook_subscribed.lock().unwrap().running);
        assert_eq!(source.sender.receiver_count(), 0);
    }

    #[tokio::test]
    async fn webhook_intake_filters_private_repeated_and_non_task_events() {
        let (source, mut rx) = captured_source(8, false).await;
        let receiver = source.sender.subscribe();
        let intake = tokio::spawn(receive_events(source.clone(), receiver));
        let task = task_with_history("task", "context", 0);
        let mut events = vec![
            (EventKind::Repeated, Ok(StreamResponse::Task(task.clone()))),
            (
                EventKind::LiveOnly(None),
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
        for (index, (kind, result)) in events.into_iter().enumerate() {
            source
                .sender
                .send(ExecutionEvent {
                    sequence: index as u64 + 1,
                    result,
                    kind,
                    failure: None,
                })
                .unwrap_or_else(|_| panic!("intake must remain subscribed"));
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
        intake.abort();
    }

    #[tokio::test]
    async fn full_intake_queue_signals_failure_without_blocking_publication() {
        let (source, mut rx) = captured_source(1, true).await;
        let receiver = source.sender.subscribe();
        let intake = tokio::spawn(receive_events(source.clone(), receiver));
        let (failure, mut observed) = tokio::sync::watch::channel(None);
        for sequence in [1, 2] {
            source
                .sender
                .send(ExecutionEvent {
                    sequence,
                    result: Ok(StreamResponse::Task(task_with_history(
                        "task", "context", 0,
                    ))),
                    kind: EventKind::Normal,
                    failure: Some(failure.clone()),
                })
                .unwrap_or_else(|_| panic!("intake must remain subscribed"));
        }
        timeout(Duration::from_secs(1), observed.changed())
            .await
            .unwrap()
            .unwrap();
        assert!(
            observed
                .borrow()
                .as_ref()
                .unwrap()
                .message
                .contains("queue full")
        );
        assert_eq!(rx.recv().await.unwrap().event.sequence, 1);
        intake.abort();
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
                    message,
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
                    message,
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

    #[tokio::test(start_paused = true)]
    async fn idle_webhook_subscribers_retire_and_restart_without_changing_task_source() {
        let handler = make_push_delivery_handler();
        let source = handler.tasks.task("task");
        handler.tasks.pushes.attach(&handler, "task", &source);
        let event = StreamResponse::Task(task_with_history("task", "context", 0));
        source.publish(Ok(&event), None, EventKind::Normal, None);
        while !source.sender.is_empty() {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(PUSH_WORKER_IDLE + Duration::from_secs(1)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(!source.webhook_subscribed.lock().unwrap().running);
        source.publish(Ok(&event), None, EventKind::Normal, None);
        assert!(source.webhook_subscribed.lock().unwrap().running);
        assert!(Arc::ptr_eq(&source, &handler.tasks.task("task")));
    }

    #[tokio::test]
    async fn a_lagging_webhook_subscriber_continues_and_a_closed_dispatcher_retires_it() {
        for closed in [false, true] {
            let (source, mut rx) = captured_source(2, false).await;
            source.webhook_subscribed.lock().unwrap().running = true;
            let receiver = source.sender.subscribe();
            for sequence in 0..super::super::EXECUTION_BUFFER_CAPACITY as u64 + 2 {
                assert!(
                    source
                        .sender
                        .send(ExecutionEvent {
                            sequence,
                            kind: EventKind::Repeated,
                            result: Ok(StreamResponse::Task(task_with_history(
                                "task", "context", 0
                            ))),
                            failure: None
                        })
                        .is_ok()
                );
            }
            assert!(
                source
                    .sender
                    .send(ExecutionEvent {
                        sequence: 999,
                        kind: EventKind::Normal,
                        result: Ok(StreamResponse::Task(task_with_history(
                            "task", "context", 0
                        ))),
                        failure: None
                    })
                    .is_ok()
            );
            if closed {
                rx.close();
            }
            let intake = tokio::spawn(receive_events(source.clone(), receiver));
            if closed {
                timeout(Duration::from_secs(1), intake)
                    .await
                    .unwrap()
                    .unwrap();
                assert!(!source.webhook_subscribed.lock().unwrap().running);
            } else {
                assert_eq!(
                    timeout(Duration::from_secs(1), rx.recv())
                        .await
                        .unwrap()
                        .unwrap()
                        .event
                        .sequence,
                    999
                );
                intake.abort();
            }
        }
    }
}
