// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Invalid stored webhooks must respect failure policy and not starve other endpoints.
use a2a::*;
use a2a_server::*;
use async_trait::async_trait;
use futures::{StreamExt, stream, stream::BoxStream};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;
struct Waiting;
impl AgentExecutor for Waiting {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::iter([Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: ctx.task_id,
            context_id: ctx.context_id,
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            generation: 0,
            metadata: None,
        }))])
        .chain(stream::pending())
        .boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}
async fn store() -> InMemoryTaskStore {
    let store = InMemoryTaskStore::new();
    store
        .create(Task {
            id: "task".into(),
            context_id: "context".into(),
            generation: 0,
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            timeline: None,
            metadata: None,
        })
        .await
        .unwrap();
    store
}
fn config(url: &str) -> TaskPushNotificationConfig {
    TaskPushNotificationConfig {
        task_id: "task".into(),
        url: url.into(),
        id: None,
        tenant: None,
        protocol_version: None,
        token: None,
        authentication: None,
    }
}
fn request() -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text("work")]);
    message.task_id = Some("task".into());
    SendMessageRequest {
        message,
        tenant: None,
        configuration: None,
        metadata: None,
        artifact_updates: vec![],
    }
}
async fn working(
    handler: &DefaultRequestHandler,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    let mut stream = handler
        .send_streaming_message(&ServiceParams::new(), request())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                stream.next().await.unwrap().unwrap(),
                StreamResponse::StatusUpdate(_)
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    stream
}
#[tokio::test]
async fn invalid_legacy_webhook_only_stops_execution_when_configured_to_fail() {
    for fail_on_error in [false, true] {
        // Simulate configuration written before admission validation existed.
        let listed = Arc::new(Notify::new());
        let configs = OrderedConfigs {
            inner: InMemoryPushConfigStore::new(),
            listed: listed.clone(),
        };
        configs.save(config("http://127.0.0.1/hook")).await.unwrap();
        let handler = DefaultRequestHandler::new(Waiting, store().await)
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            })
            .with_push_notifications(
                configs,
                HttpPushSender::new(Some(HttpPushSenderConfig {
                    fail_on_error,
                    ..Default::default()
                })),
            );
        let mut stream = working(&handler).await;
        tokio::time::timeout(Duration::from_secs(5), listed.notified())
            .await
            .unwrap();
        let wait = if fail_on_error {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(200)
        };
        let next = tokio::time::timeout(wait, stream.next()).await;
        if fail_on_error {
            assert!(next.unwrap().unwrap().is_err());
        } else {
            assert!(
                next.is_err(),
                "invalid webhook must not end an execution by default"
            );
        }
        handler
            .cancel_task(
                &ServiceParams::new(),
                CancelTaskRequest {
                    id: "task".into(),
                    tenant: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn invalid_new_configuration_is_rejected_before_it_is_registered() {
    let handler = DefaultRequestHandler::new(Waiting, store().await)
        .with_push_config_store(InMemoryPushConfigStore::new());
    let mut credentials = config("https://example.com/hook");
    credentials.token = Some("invalid\r\ntoken".into());
    for invalid in [config("http://127.0.0.1/hook"), credentials] {
        assert_eq!(
            handler
                .create_push_config(&ServiceParams::new(), invalid)
                .await
                .unwrap_err()
                .code,
            error_code::INVALID_PARAMS
        );
    }
    let listed = handler
        .list_push_configs(
            &ServiceParams::new(),
            ListTaskPushNotificationConfigsRequest {
                task_id: "task".into(),
                tenant: None,
                page_size: None,
                page_token: None,
            },
        )
        .await
        .unwrap();
    assert!(listed.configs.is_empty());
}

struct OrderedConfigs {
    inner: InMemoryPushConfigStore,
    listed: Arc<Notify>,
}
#[async_trait]
impl PushConfigStore for OrderedConfigs {
    async fn save(
        &self,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.inner.save(config).await
    }
    async fn get(&self, task: &str, id: &str) -> Result<TaskPushNotificationConfig, A2AError> {
        self.inner.get(task, id).await
    }
    async fn list(&self, task: &str) -> Result<Vec<TaskPushNotificationConfig>, A2AError> {
        let mut configs = self.inner.list(task).await?;
        configs.sort_by(|a, b| a.id.cmp(&b.id));
        self.listed.notify_one();
        Ok(configs)
    }
    async fn delete(&self, task: &str, id: &str) -> Result<(), A2AError> {
        self.inner.delete(task, id).await
    }
    async fn delete_all(&self, task: &str) -> Result<(), A2AError> {
        self.inner.delete_all(task).await
    }
}
#[tokio::test]
async fn invalid_configuration_does_not_skip_a_later_healthy_webhook() {
    for fail_on_error in [false, true] {
        let received = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let respond = release.clone();
        let observed = received.clone();
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move || {
                let observed = observed.clone();
                let respond = respond.clone();
                async move {
                    observed.notify_one();
                    respond.notified().await;
                    axum::http::StatusCode::NO_CONTENT
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let configs = OrderedConfigs {
            inner: InMemoryPushConfigStore::new(),
            listed: Arc::new(Notify::new()),
        };
        let mut bad = config(&url);
        bad.id = Some("0-invalid".into());
        bad.token = Some("invalid\r\nvalue".into());
        configs.save(bad).await.unwrap();
        let mut healthy = config(&url);
        healthy.id = Some("1-healthy".into());
        configs.save(healthy).await.unwrap();
        let handler = DefaultRequestHandler::new(Waiting, store().await)
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            })
            .with_push_notifications(
                configs,
                HttpPushSender::new(Some(HttpPushSenderConfig {
                    // The test endpoint is deliberately loopback; credential validation remains on.
                    validate_urls: false,
                    fail_on_error,
                    ..Default::default()
                })),
            );
        let mut downlink = working(&handler).await;
        tokio::time::timeout(Duration::from_secs(5), received.notified())
            .await
            .unwrap();
        if fail_on_error {
            assert!(
                tokio::time::timeout(Duration::from_secs(5), downlink.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .is_err(),
                "failure must be signalled while the next endpoint is still waiting"
            );
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(200), downlink.next())
                    .await
                    .is_err()
            );
        }
        release.notify_one();
        handler
            .cancel_task(
                &ServiceParams::new(),
                CancelTaskRequest {
                    id: "task".into(),
                    tenant: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();
        server.abort();
    }
}
