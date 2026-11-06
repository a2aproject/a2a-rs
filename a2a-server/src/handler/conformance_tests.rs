// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use crate::*;
use a2a::*;
use futures::{StreamExt, stream, stream::BoxStream};
use std::time::Duration;

struct Executor;
impl AgentExecutor for Executor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        if ctx.message.as_ref().and_then(Message::text) == Some("reply") {
            return stream::once(async {
                Ok(StreamResponse::Message(Message::new(
                    Role::Agent,
                    vec![Part::text("answer")],
                )))
            })
            .boxed();
        }
        stream::once(async move {
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: ctx.task_id,
                context_id: ctx.context_id,
                status: TaskStatus {
                    state: TaskState::InputRequired,
                    message: None,
                    timestamp: None,
                },
                generation: 0,
                metadata: None,
            }))
        })
        .boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}
fn params() -> ServiceParams {
    ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])])
}
fn config() -> SendMessageConfiguration {
    serde_json::from_value(serde_json::json!({})).unwrap()
}
fn request(text: &str) -> SendMessageRequest {
    SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text(text)]),
        configuration: None,
        metadata: None,
        tenant: None,
        artifact_updates: vec![],
    }
}
fn handler() -> DefaultRequestHandler {
    DefaultRequestHandler::new(Executor, InMemoryTaskStore::new()).with_capabilities(
        AgentCapabilities {
            streaming: Some(true),
            user_artifacts: Some(true),
            ..Default::default()
        },
    )
}
fn crypto() {
    crate::test_util::install_crypto_provider();
}

// Regression tests for the handler findings from the 1.1 RC review.
#[tokio::test]
async fn live_opening_respects_zero_history_and_timeline_limits() {
    let h = handler();
    let mut req = request("wait");
    let mut c = config();
    c.history_length = Some(0);
    c.timeline_length = Some(0);
    req.configuration = Some(c);
    let first = StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::SendMessage(req),
    };
    let mut down = h
        .send_live_message(
            &params(),
            stream::once(async move { first })
                .chain(stream::pending())
                .boxed(),
        )
        .await
        .unwrap();
    let StreamResponse::Task(task) = tokio::time::timeout(Duration::from_secs(2), down.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
    else {
        panic!("task opening")
    };
    assert!(task.history.unwrap_or_default().is_empty());
    assert!(task.timeline.unwrap_or_default().is_empty());
    drop(down);
    h.shutdown().await;
}
#[tokio::test]
async fn empty_client_artifact_is_rejected_before_persistence() {
    let h = handler();
    let mut req = request("wait");
    req.artifact_updates.push(serde_json::from_value(serde_json::json!({"taskId":"","contextId":"","artifact":{"artifactId":"empty","parts":[]}})).unwrap());
    assert_eq!(
        h.send_message(&params(), req).await.unwrap_err().code,
        error_code::INVALID_PARAMS
    );
    h.shutdown().await;
}
#[tokio::test]
async fn unknown_task_push_list_and_delete_return_not_found() {
    crypto();
    let h = handler().with_push_config_store(InMemoryPushConfigStore::new());
    let list = h
        .list_push_configs(
            &params(),
            ListTaskPushNotificationConfigsRequest {
                tenant: None,
                task_id: "does-not-exist".into(),
                page_size: None,
                page_token: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(list.code, error_code::TASK_NOT_FOUND);
    let error = h
        .delete_push_config(
            &params(),
            DeleteTaskPushNotificationConfigRequest {
                tenant: None,
                task_id: "does-not-exist".into(),
                id: "missing".into(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, error_code::TASK_NOT_FOUND);
    h.shutdown().await;
}
#[tokio::test]
async fn server_message_inherits_admitted_context() {
    let h = handler();
    let SendMessageResponse::Message(message) =
        h.send_message(&params(), request("reply")).await.unwrap()
    else {
        panic!("message")
    };
    assert!(message.context_id.is_some_and(|id| !id.is_empty()));
    h.shutdown().await;
}
#[tokio::test]
async fn unsupported_future_minor_version_is_rejected() {
    let h = handler();
    let params = ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.999".into()])]);
    assert_eq!(
        h.send_message(&params, request("reply"))
            .await
            .unwrap_err()
            .code,
        error_code::VERSION_NOT_SUPPORTED
    );
    h.shutdown().await;
}
#[tokio::test]
async fn delayed_message_reply_survives_later_turns_and_broadcast_lag() {
    let store = InMemoryTaskStore::new();
    store
        .create(
            serde_json::from_value(serde_json::json!({
                "id":"shared", "contextId":"context", "generation":"1",
                "status":{"state":"TASK_STATE_WORKING"}
            }))
            .unwrap(),
        )
        .await
        .unwrap();
    let h = DefaultRequestHandler::new(Executor, store).with_capabilities(AgentCapabilities {
        streaming: Some(true),
        ..Default::default()
    });
    let mut req = request("reply");
    req.message.task_id = Some("shared".into());
    let mut delayed = h.send_streaming_message(&params(), req).await.unwrap();
    // The mailbox barrier runs after the first reply's output command.
    let barrier = h
        .subscribe_to_task(
            &params(),
            SubscribeToTaskRequest {
                id: "shared".into(),
                tenant: None,
                last_seen_generation: None,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(10)).await;
    for _ in 0..super::EXECUTION_BUFFER_CAPACITY + 8 {
        let mut req = request("tick");
        req.message.task_id = Some("shared".into());
        h.send_message(&params(), req).await.unwrap();
    }
    let observed = tokio::time::timeout(Duration::from_secs(2), delayed.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(observed, StreamResponse::Message(_)),
        "the original Message must survive lag: {observed:?}"
    );
    drop((delayed, barrier));
    h.shutdown().await;
}

#[derive(Clone)]
struct Configs {
    inner: std::sync::Arc<InMemoryPushConfigStore>,
    lists: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}
#[async_trait::async_trait]
impl PushConfigStore for Configs {
    async fn save(
        &self,
        c: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.inner.save(c).await
    }
    async fn get(&self, t: &str, id: &str) -> Result<TaskPushNotificationConfig, A2AError> {
        self.inner.get(t, id).await
    }
    async fn list(&self, t: &str) -> Result<Vec<TaskPushNotificationConfig>, A2AError> {
        let configs = self.inner.list(t).await?;
        self.lists.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(configs)
    }
    async fn delete(&self, t: &str, id: &str) -> Result<(), A2AError> {
        self.inner.delete(t, id).await
    }
    async fn delete_all(&self, t: &str) -> Result<(), A2AError> {
        self.inner.delete_all(t).await
    }
}
struct Controlled(std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<StreamResponse>>>);
impl AgentExecutor for Controlled {
    fn execute(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let rx = self.0.lock().unwrap().take().unwrap();
        stream::unfold(rx, |mut rx| async move { Some((Ok(rx.recv().await?), rx)) }).boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}
async fn wait_count(count: &std::sync::atomic::AtomicUsize, minimum: usize) {
    tokio::time::timeout(Duration::from_secs(3), async {
        while count.load(std::sync::atomic::Ordering::SeqCst) < minimum {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn queued_webhook_cannot_start_after_configuration_deletion() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    crypto();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let server = tokio::spawn({
        let calls = calls.clone();
        let release = release.clone();
        async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let n = calls.fetch_add(1, Ordering::SeqCst);
                let release = release.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = socket.read(&mut buf).await;
                    if n == 0 {
                        release.notified().await;
                    }
                    let _=socket.write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                });
            }
        }
    });
    let configs = Configs {
        inner: Arc::new(InMemoryPushConfigStore::new()),
        lists: Arc::new(AtomicUsize::new(0)),
    };
    configs
        .save(TaskPushNotificationConfig {
            task_id: "task".into(),
            id: Some("cfg".into()),
            url,
            token: None,
            authentication: None,
            tenant: None,
            protocol_version: Some("1.0".into()),
        })
        .await
        .unwrap();
    let store = InMemoryTaskStore::new();
    store.create(serde_json::from_value(serde_json::json!({"id":"task","contextId":"ctx","generation":"1","status":{"state":"TASK_STATE_WORKING"}})).unwrap()).await.unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let h = DefaultRequestHandler::new(Controlled(std::sync::Mutex::new(Some(rx))), store)
        .with_capabilities(AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        })
        .with_push_notifications(
            configs.clone(),
            HttpPushSender::new(Some(HttpPushSenderConfig {
                validate_urls: false,
                fail_on_error: true,
                timeout: Duration::from_secs(3),
            })),
        );
    let mut req = request("start");
    req.message.task_id = Some("task".into());
    let down = h.send_streaming_message(&params(), req).await.unwrap();
    let event = || {
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "task".into(),
            context_id: "ctx".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            generation: 0,
            metadata: None,
        })
    };
    tx.send(event()).await.unwrap();
    wait_count(&calls, 1).await;
    let previous = configs.lists.load(Ordering::SeqCst);
    tx.send(event()).await.unwrap();
    wait_count(&configs.lists, previous + 1).await;
    let deleting = tokio::spawn({
        let h = h.clone();
        async move {
            h.delete_push_config(
                &params(),
                DeleteTaskPushNotificationConfigRequest {
                    tenant: None,
                    task_id: "task".into(),
                    id: "cfg".into(),
                },
            )
            .await
            .unwrap();
        }
    });
    tokio::task::yield_now().await;
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), deleting)
        .await
        .unwrap()
        .unwrap();
    assert!(configs.inner.list("task").await.unwrap().is_empty());
    tokio::time::sleep(Duration::from_millis(50)).await;
    let after_deletion = calls.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(calls.load(Ordering::SeqCst), after_deletion);
    assert_eq!(after_deletion, 1);
    drop((down, tx));
    h.shutdown().await;
    server.abort();
}

struct CancelArtifacts;
impl AgentExecutor for CancelArtifacts {
    fn execute(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        assert_eq!(
            ctx.stored_task.as_ref().unwrap().status.state,
            TaskState::Canceled
        );
        stream::once(async move {
            Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: ctx.task_id,
                context_id: ctx.context_id,
                artifact: serde_json::from_value(
                    serde_json::json!({"artifactId":"after-cancel","parts":[{"text":"late"}]}),
                )
                .unwrap(),
                generation: 0,
                append: None,
                last_chunk: Some(true),
                metadata: None,
            }))
        })
        .boxed()
    }
}
#[tokio::test]
async fn cancellation_hook_cannot_mutate_a_terminal_task() {
    let store = InMemoryTaskStore::new();
    store.create(serde_json::from_value(serde_json::json!({"id":"task","contextId":"ctx","generation":"1","status":{"state":"TASK_STATE_WORKING"}})).unwrap()).await.unwrap();
    let h = DefaultRequestHandler::new(CancelArtifacts, store);
    let error = h
        .cancel_task(
            &params(),
            CancelTaskRequest {
                id: "task".into(),
                tenant: None,
                metadata: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, error_code::INVALID_AGENT_RESPONSE);
    let task = h
        .get_task(
            &params(),
            GetTaskRequest {
                id: "task".into(),
                tenant: None,
                history_length: None,
                timeline_length: None,
                current_generation: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(task.status.state, TaskState::Canceled);
    assert!(task.artifacts.unwrap_or_default().is_empty());
    h.shutdown().await;
}

struct RejectConfig;
#[async_trait::async_trait]
impl PushConfigStore for RejectConfig {
    async fn save(
        &self,
        _: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Err(A2AError::internal("store save failed"))
    }
    async fn get(&self, _: &str, _: &str) -> Result<TaskPushNotificationConfig, A2AError> {
        Err(A2AError::task_not_found("cfg"))
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
#[tokio::test]
async fn deferred_webhook_save_failure_reaches_the_caller() {
    crypto();
    let h = handler().with_push_config_store(RejectConfig);
    let mut req = request("wait");
    let mut c = config();
    c.task_push_notification_config = Some(TaskPushNotificationConfig {
        task_id: String::new(),
        id: None,
        url: "https://example.org/hook".into(),
        token: None,
        authentication: None,
        tenant: None,
        protocol_version: None,
    });
    req.configuration = Some(c);
    assert_eq!(
        h.send_message(&params(), req).await.unwrap_err().message,
        "store save failed"
    );
    h.shutdown().await;
}

struct QuietExecutor;
impl AgentExecutor for QuietExecutor {
    fn execute(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

#[tokio::test]
async fn empty_turn_webhook_save_failure_reaches_the_caller() {
    crypto();
    let h = DefaultRequestHandler::new(QuietExecutor, InMemoryTaskStore::new())
        .with_push_config_store(RejectConfig);
    let mut req = request("wait");
    let mut c = config();
    c.task_push_notification_config = Some(TaskPushNotificationConfig {
        task_id: String::new(),
        id: None,
        url: "https://example.org/hook".into(),
        token: None,
        authentication: None,
        tenant: None,
        protocol_version: None,
    });
    req.configuration = Some(c);
    assert_eq!(
        h.send_message(&params(), req).await.unwrap_err().message,
        "store save failed"
    );
    h.shutdown().await;
}

#[tokio::test]
async fn artifact_only_inputs_enforce_declared_media_types() {
    let h = handler().with_default_input_modes(vec!["text/plain".into()]);
    let mut req = request("wait");
    req.message.message_id.clear();
    req.message.parts.clear();
    req.message.task_id = Some("uncreated".into());
    req.artifact_updates.push(serde_json::from_value(serde_json::json!({
        "taskId":"uncreated", "contextId":"", "artifact":{ "artifactId":"unsupported", "parts":[{"text":"payload", "mediaType":"application/octet-stream"}] }
    })).unwrap());
    assert_eq!(
        h.send_message(&params(), req).await.unwrap_err().code,
        error_code::CONTENT_TYPE_NOT_SUPPORTED
    );
    h.shutdown().await;
}

#[tokio::test]
async fn http_live_acknowledgement_applies_its_own_response_limits() {
    let h = handler();
    let LiveResponse::Stream(mut events) = h
        .send_live_request(
            &params(),
            StreamRequest {
                tenant: None,
                payload: StreamRequestPayload::SendMessage(request("wait")),
            },
        )
        .await
        .unwrap()
    else {
        panic!("opening stream");
    };
    let StreamResponse::Task(task) = events.next().await.unwrap().unwrap() else {
        panic!("opening task");
    };
    let mut followup = request("again");
    followup.message.task_id = Some(task.id);
    let mut limits = config();
    limits.history_length = Some(0);
    limits.timeline_length = Some(0);
    followup.configuration = Some(limits);
    let LiveResponse::Ack(task) = tokio::time::timeout(
        Duration::from_secs(2),
        h.send_live_request(
            &params(),
            StreamRequest {
                tenant: None,
                payload: StreamRequestPayload::SendMessage(followup),
            },
        ),
    )
    .await
    .unwrap()
    .unwrap() else {
        panic!("followup acknowledgement");
    };
    assert!(task.history.unwrap_or_default().is_empty());
    assert!(task.timeline.unwrap_or_default().is_empty());
    drop(events);
    h.shutdown().await;
}

struct PendingExecutor;
impl AgentExecutor for PendingExecutor {
    fn execute(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::pending().boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

#[tokio::test]
async fn immediate_unary_returns_a_persisted_task_without_executor_output() {
    for existing in [false, true] {
        let h = DefaultRequestHandler::new(PendingExecutor, InMemoryTaskStore::new());
        if existing {
            super::test_support::seed_task(&h, "task", "ctx").await;
        }
        let mut req = request("pending");
        req.message.task_id = existing.then(|| "task".to_owned());
        let mut options = config();
        options.return_immediately = Some(true);
        req.configuration = Some(options);
        let SendMessageResponse::Task(task) =
            tokio::time::timeout(Duration::from_secs(1), h.send_message(&params(), req))
                .await
                .expect("must not wait for executor output")
                .unwrap()
        else {
            panic!("immediate task");
        };
        assert_eq!(h.tasks.load(&task.id).await.unwrap(), task);
        assert!(!task.history.unwrap_or_default().is_empty());
        h.shutdown().await;
    }
}

struct NamedReplyExecutor {
    foreign: bool,
}
impl AgentExecutor for NamedReplyExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let mut message = Message::new(Role::Agent, vec![Part::text("reply")]);
        message.task_id = Some(if self.foreign {
            "foreign".into()
        } else {
            ctx.task_id
        });
        stream::once(async move { Ok(StreamResponse::Message(message)) }).boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

#[tokio::test]
async fn direct_replies_only_name_tasks_that_exist() {
    for existing in [false, true] {
        for streaming in [false, true] {
            let h = DefaultRequestHandler::new(
                NamedReplyExecutor { foreign: false },
                InMemoryTaskStore::new(),
            )
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            });
            if existing {
                super::test_support::seed_task(&h, "task", "ctx").await;
            }
            let mut req = request("reply");
            req.message.task_id = existing.then(|| "task".into());
            let message = if streaming {
                let mut events = h.send_streaming_message(&params(), req).await.unwrap();
                let StreamResponse::Message(message) = events.next().await.unwrap().unwrap() else {
                    panic!("direct message");
                };
                assert!(events.next().await.is_none());
                message
            } else {
                let SendMessageResponse::Message(message) =
                    h.send_message(&params(), req).await.unwrap()
                else {
                    panic!("direct message");
                };
                message
            };
            assert_eq!(message.task_id.as_deref(), existing.then_some("task"));
            assert!(message.context_id.is_some());
            h.shutdown().await;
        }
    }
}

#[tokio::test]
async fn foreign_task_ids_are_rejected_before_reply_normalization() {
    let h = DefaultRequestHandler::new(
        NamedReplyExecutor { foreign: true },
        InMemoryTaskStore::new(),
    );
    assert_eq!(
        h.send_message(&params(), request("reply"))
            .await
            .unwrap_err()
            .code,
        error_code::INVALID_AGENT_RESPONSE
    );
    h.shutdown().await;
}

struct InvalidArtifactExecutor {
    snapshot: bool,
    empty_id: bool,
}
impl AgentExecutor for InvalidArtifactExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let artifact: Artifact = serde_json::from_value(serde_json::json!({
            "artifactId": if self.empty_id { "" } else { "artifact" },
            "parts": if self.empty_id { vec![serde_json::json!({"text":"content"})] } else { vec![] }
        })).unwrap();
        let event = if self.snapshot {
            let mut task = ctx.stored_task.unwrap();
            task.artifacts = Some(vec![artifact]);
            StreamResponse::Task(task)
        } else {
            StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: ctx.task_id,
                context_id: ctx.context_id,
                artifact,
                generation: 0,
                append: None,
                last_chunk: Some(true),
                metadata: None,
            })
        };
        stream::once(async move { Ok(event) }).boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

#[tokio::test]
async fn executor_artifacts_are_validated_before_persistence() {
    for snapshot in [false, true] {
        for empty_id in [false, true] {
            let h = DefaultRequestHandler::new(
                InvalidArtifactExecutor { snapshot, empty_id },
                InMemoryTaskStore::new(),
            );
            assert_eq!(
                h.send_message(&params(), request("artifact"))
                    .await
                    .unwrap_err()
                    .code,
                error_code::INVALID_AGENT_RESPONSE
            );
            let tasks = h
                .tasks
                .list(
                    &serde_json::from_value(serde_json::json!({})).unwrap(),
                    None,
                )
                .await
                .unwrap();
            assert!(tasks.tasks.iter().all(|task| {
                task.artifacts
                    .as_ref()
                    .is_none_or(|artifacts| artifacts.is_empty())
            }));
            h.shutdown().await;
        }
    }
}

#[tokio::test]
async fn unary_response_keeps_the_completion_snapshot_after_later_task_updates() {
    use super::protocol::{ResponsePolicy, StartMode};
    use super::response::{task_response, unary_response};
    let h = handler();
    super::test_support::seed_task(&h, "task", "ctx").await;
    let initial = h.tasks.load("task").await.unwrap();
    let source = h.tasks.task("task");
    let (active, _, _) = source.reserve().unwrap();
    active.set_snapshot(initial.clone());
    let receiver = active.subscribe();
    let policy = ResponsePolicy::message(StartMode::Blocking, &params(), &request("wait"));
    let mut completed = initial;
    completed.status.state = TaskState::InputRequired;
    completed.generation += 1;
    h.tasks.store().update(completed.clone()).await.unwrap();
    active
        .publish(
            Ok(&StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: "task".into(),
                context_id: "ctx".into(),
                status: completed.status.clone(),
                generation: completed.generation,
                metadata: None,
            })),
            Some(completed.clone()),
        )
        .await;
    // A later turn advances both storage and the live cache before this caller polls.
    let mut later = completed.clone();
    later.status.state = TaskState::Working;
    later.generation += 1;
    h.tasks.store().update(later.clone()).await.unwrap();
    active.set_snapshot(later);
    let inner = super::events::subscription_stream(receiver, None, 0, policy.event_filter());
    let events = task_response(h.clone(), source.clone(), policy, None, Some(inner), 0);
    let SendMessageResponse::Task(reply) = unary_response(h.tasks.clone(), source, events)
        .await
        .unwrap()
    else {
        panic!("task reply");
    };
    assert_eq!(reply, completed);
    assert_eq!(
        h.tasks.load("task").await.unwrap().status.state,
        TaskState::Working
    );
    h.shutdown().await;
}

struct StatusMessageExecutor {
    snapshot: bool,
    message: Message,
}

impl StatusMessageExecutor {
    fn event(&self, ctx: ExecutorContext, state: TaskState) -> StreamResponse {
        let status = TaskStatus {
            state,
            message: Some(self.message.clone()),
            timestamp: None,
        };
        if self.snapshot {
            StreamResponse::Task(Task {
                id: ctx.task_id,
                context_id: ctx.context_id,
                status,
                generation: 0,
                history: None,
                timeline: None,
                artifacts: None,
                metadata: None,
            })
        } else {
            StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: ctx.task_id,
                context_id: ctx.context_id,
                status,
                generation: 0,
                metadata: None,
            })
        }
    }
}

impl AgentExecutor for StatusMessageExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let event = self.event(ctx, TaskState::InputRequired);
        stream::once(async move { Ok(event) }).boxed()
    }
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let event = self.event(ctx, TaskState::Canceled);
        stream::once(async move { Ok(event) }).boxed()
    }
}

#[tokio::test]
async fn status_messages_are_normalized_before_storage_timeline_and_stream_delivery() {
    for snapshot in [false, true] {
        for streaming in [false, true] {
            for role in [Role::Agent, Role::User] {
                let message = Message::new(role.clone(), vec![Part::text("status message")]);
                let h = DefaultRequestHandler::new(
                    StatusMessageExecutor {
                        snapshot,
                        message: message.clone(),
                    },
                    InMemoryTaskStore::new(),
                )
                .with_capabilities(AgentCapabilities {
                    streaming: Some(true),
                    ..Default::default()
                });
                let (task, delivered_message) = if streaming {
                    let mut events = h
                        .send_streaming_message(&params(), request("status"))
                        .await
                        .unwrap();
                    let result = tokio::time::timeout(Duration::from_secs(1), async {
                        loop {
                            match events.next().await.unwrap().unwrap() {
                                StreamResponse::Task(task)
                                    if task.status.state == TaskState::InputRequired =>
                                {
                                    let message = task.status.message.clone().unwrap();
                                    break (task, message);
                                }
                                StreamResponse::StatusUpdate(update)
                                    if update.status.state == TaskState::InputRequired =>
                                {
                                    break (
                                        h.tasks.load(&update.task_id).await.unwrap(),
                                        update.status.message.unwrap(),
                                    );
                                }
                                _ => {}
                            }
                        }
                    })
                    .await
                    .unwrap();
                    drop(events);
                    result
                } else {
                    let SendMessageResponse::Task(task) =
                        h.send_message(&params(), request("status")).await.unwrap()
                    else {
                        panic!("task");
                    };
                    let message = task.status.message.clone().unwrap();
                    (task, message)
                };
                let mut expected = message;
                if role == Role::Agent {
                    expected.context_id = Some(task.context_id.clone());
                }
                assert_eq!(delivered_message, expected);
                assert_eq!(task.status.message.as_ref(), Some(&expected));
                assert_eq!(
                    h.tasks
                        .load(&task.id)
                        .await
                        .unwrap()
                        .status
                        .message
                        .as_ref(),
                    Some(&expected)
                );
                let TimelineContent::Status(status) =
                    &task.timeline.as_ref().unwrap().last().unwrap().content
                else {
                    panic!("status timeline entry");
                };
                assert_eq!(status.message.as_ref(), Some(&expected));
                h.shutdown().await;
            }
        }
    }
}

#[tokio::test]
async fn invalid_status_messages_are_rejected_before_persistence() {
    for snapshot in [false, true] {
        for invalid in ["id", "parts", "role", "task", "context"] {
            let mut message = Message::new(Role::Agent, vec![Part::text("invalid status payload")]);
            match invalid {
                "id" => message.message_id.clear(),
                "parts" => message.parts.clear(),
                "role" => message.role = Role::Unspecified,
                "task" => message.task_id = Some("foreign-task".into()),
                "context" => message.context_id = Some("foreign-context".into()),
                _ => unreachable!(),
            }
            let h = DefaultRequestHandler::new(
                StatusMessageExecutor { snapshot, message },
                InMemoryTaskStore::new(),
            );
            assert_eq!(
                h.send_message(&params(), request("status"))
                    .await
                    .unwrap_err()
                    .code,
                error_code::INVALID_AGENT_RESPONSE,
                "{invalid}, snapshot={snapshot}"
            );
            let tasks = h
                .tasks
                .list(
                    &serde_json::from_value(serde_json::json!({})).unwrap(),
                    None,
                )
                .await
                .unwrap();
            assert!(
                tasks
                    .tasks
                    .iter()
                    .all(|task| task.status.message.as_ref().and_then(Message::text)
                        != Some("invalid status payload"))
            );
            h.shutdown().await;
        }
    }
}

#[tokio::test]
async fn cancellation_acknowledgements_are_validated_without_rewriting_status() {
    for foreign_context in [false, true] {
        let mut message = Message::new(Role::Agent, vec![Part::text("canceled")]);
        if foreign_context {
            message.context_id = Some("foreign-context".into());
        }
        let h = DefaultRequestHandler::new(
            StatusMessageExecutor {
                snapshot: false,
                message,
            },
            InMemoryTaskStore::new(),
        );
        super::test_support::seed_task(&h, "task", "ctx").await;
        let result = h
            .cancel_task(
                &params(),
                CancelTaskRequest {
                    id: "task".into(),
                    tenant: None,
                    metadata: None,
                },
            )
            .await;
        if foreign_context {
            assert_eq!(result.unwrap_err().code, error_code::INVALID_AGENT_RESPONSE);
            assert!(h.tasks.load("task").await.unwrap().status.message.is_none());
        } else {
            let task = result.unwrap();
            assert!(
                task.status.message.is_none(),
                "cleanup annotations cannot amend the terminal status"
            );
            let TimelineContent::Status(status) =
                &task.timeline.as_ref().unwrap().last().unwrap().content
            else {
                panic!("cancellation status");
            };
            assert_eq!(status.message, task.status.message);
        }
        h.shutdown().await;
    }
}

#[derive(Clone)]
enum SnapshotMutation {
    Timestamp,
    History(Vec<Message>),
    Artifacts(Vec<Artifact>),
    Metadata,
}

struct SnapshotMutationExecutor {
    mutation: SnapshotMutation,
    before: std::sync::Arc<std::sync::Mutex<Option<Task>>>,
}

impl AgentExecutor for SnapshotMutationExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let mut task = ctx.stored_task.unwrap_or(Task {
            id: ctx.task_id,
            context_id: ctx.context_id,
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: None,
            },
            generation: 0,
            history: None,
            timeline: None,
            artifacts: None,
            metadata: None,
        });
        *self.before.lock().unwrap() = Some(task.clone());
        match &self.mutation {
            SnapshotMutation::Timestamp => {
                task.status.timestamp = Some("2030-01-01T00:00:00Z".parse().unwrap())
            }
            SnapshotMutation::History(messages) => task.history = Some(messages.clone()),
            SnapshotMutation::Artifacts(artifacts) => task.artifacts = Some(artifacts.clone()),
            SnapshotMutation::Metadata => {
                task.metadata = Some([("phase".into(), serde_json::json!("updated"))].into())
            }
        }
        task.status.state = TaskState::InputRequired;
        stream::once(async move { Ok(StreamResponse::Task(task)) }).boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

#[tokio::test]
async fn snapshot_history_uses_shared_message_validation_before_storage_and_delivery() {
    for role in [Role::Agent, Role::User] {
        for streaming in [false, true] {
            let message = Message::new(role.clone(), vec![Part::text("history message")]);
            let h = DefaultRequestHandler::new(
                SnapshotMutationExecutor {
                    mutation: SnapshotMutation::History(vec![message.clone()]),
                    before: std::sync::Arc::default(),
                },
                InMemoryTaskStore::new(),
            )
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            });
            let task = if streaming {
                let mut events = h
                    .send_streaming_message(&params(), request("history"))
                    .await
                    .unwrap();
                let StreamResponse::Task(task) = events.next().await.unwrap().unwrap() else {
                    panic!("snapshot");
                };
                task
            } else {
                let SendMessageResponse::Task(task) =
                    h.send_message(&params(), request("history")).await.unwrap()
                else {
                    panic!("snapshot");
                };
                task
            };
            let mut expected = message;
            if role == Role::Agent {
                expected.context_id = Some(task.context_id.clone());
            }
            let stored = h.tasks.load(&task.id).await.unwrap();
            for task in [&task, &stored] {
                let supplied = task
                    .history
                    .as_ref()
                    .unwrap()
                    .iter()
                    .find(|message| message.message_id == expected.message_id)
                    .unwrap();
                assert_eq!(supplied, &expected);
                assert!(
                    task.history
                        .as_ref()
                        .unwrap()
                        .iter()
                        .any(|message| message.text() == Some("history")),
                    "snapshot must preserve the admitted client message"
                );
            }
            h.shutdown().await;
        }
    }
}

#[tokio::test]
async fn malformed_history_messages_are_rejected_before_persistence() {
    for invalid in ["id", "parts", "role", "task", "context"] {
        let mut message = Message::new(Role::Agent, vec![Part::text("invalid history")]);
        match invalid {
            "id" => message.message_id.clear(),
            "parts" => message.parts.clear(),
            "role" => message.role = Role::Unspecified,
            "task" => message.task_id = Some("foreign-task".into()),
            "context" => message.context_id = Some("foreign-context".into()),
            _ => unreachable!(),
        }
        let h = DefaultRequestHandler::new(
            SnapshotMutationExecutor {
                mutation: SnapshotMutation::History(vec![message]),
                before: std::sync::Arc::default(),
            },
            InMemoryTaskStore::new(),
        );
        assert_eq!(
            h.send_message(&params(), request("history"))
                .await
                .unwrap_err()
                .code,
            error_code::INVALID_AGENT_RESPONSE,
            "{invalid}"
        );
        let tasks = h
            .tasks
            .list(
                &serde_json::from_value(serde_json::json!({})).unwrap(),
                None,
            )
            .await
            .unwrap();
        assert!(
            tasks
                .tasks
                .iter()
                .flat_map(|task| task.history.iter().flatten())
                .all(|message| message.text() != Some("invalid history"))
        );
        h.shutdown().await;
    }
}

#[tokio::test]
async fn duplicate_snapshot_artifact_ids_are_rejected_without_changing_user_artifacts() {
    for existing in [false, true] {
        let store = InMemoryTaskStore::new();
        let user_artifact: Artifact = serde_json::from_value(serde_json::json!({"artifactId":"shared", "parts":[{"text":"client content"}], "role":"ROLE_USER"})).unwrap();
        if existing {
            let mut task = super::test_support::task_with_history("task", "ctx", 0);
            task.status.state = TaskState::InputRequired;
            task.artifacts = Some(vec![user_artifact.clone()]);
            store.create(task).await.unwrap();
        }
        let artifact: Artifact = serde_json::from_value(serde_json::json!({"artifactId":"shared", "parts":[{"text":"agent content"}], "role":"ROLE_AGENT"})).unwrap();
        let h = DefaultRequestHandler::new(
            SnapshotMutationExecutor {
                mutation: SnapshotMutation::Artifacts(vec![artifact.clone(), artifact]),
                before: std::sync::Arc::default(),
            },
            store,
        );
        let mut req = request("artifacts");
        req.message.task_id = existing.then(|| "task".into());
        assert_eq!(
            h.send_message(&params(), req).await.unwrap_err().code,
            error_code::INVALID_AGENT_RESPONSE
        );
        if existing {
            let stored = h.tasks.load("task").await.unwrap();
            let artifacts = stored.artifacts.unwrap();
            assert_eq!(artifacts.len(), 1);
            assert_eq!(artifacts[0].artifact_id, user_artifact.artifact_id);
            assert_eq!(artifacts[0].role, user_artifact.role);
            assert_eq!(artifacts[0].parts, user_artifact.parts);
        }
        h.shutdown().await;
    }
}

#[tokio::test]
async fn history_only_snapshot_changes_advance_generation_but_replayed_messages_do_not() {
    let store = InMemoryTaskStore::new();
    let mut initial = super::test_support::task_with_history("task", "ctx", 0);
    initial.status.state = TaskState::InputRequired;
    store.create(initial).await.unwrap();
    let message = Message::new(Role::Agent, vec![Part::text("history addition")]);
    let before = std::sync::Arc::new(std::sync::Mutex::new(None));
    let h = DefaultRequestHandler::new(
        SnapshotMutationExecutor {
            mutation: SnapshotMutation::History(vec![message.clone()]),
            before: before.clone(),
        },
        store,
    );
    for first in [true, false] {
        let mut req = request("input");
        req.message.task_id = Some("task".into());
        let SendMessageResponse::Task(reply) = h.send_message(&params(), req).await.unwrap() else {
            panic!("snapshot");
        };
        let previous = before.lock().unwrap().clone().unwrap();
        assert_eq!(reply.generation, previous.generation + i64::from(first));
        assert_eq!(
            reply
                .history
                .as_ref()
                .unwrap()
                .iter()
                .filter(|entry| entry.message_id == message.message_id)
                .count(),
            1
        );
        assert!(
            previous.history.as_ref().unwrap().iter().all(|entry| reply
                .history
                .as_ref()
                .unwrap()
                .contains(entry)),
            "a stale supplied history must preserve recorded input"
        );
    }
    h.shutdown().await;
}

#[tokio::test]
async fn metadata_only_snapshot_changes_advance_generation_once() {
    let store = InMemoryTaskStore::new();
    let mut initial = super::test_support::task_with_history("task", "ctx", 0);
    initial.status.state = TaskState::InputRequired;
    store.create(initial).await.unwrap();
    let before = std::sync::Arc::new(std::sync::Mutex::new(None));
    let h = DefaultRequestHandler::new(
        SnapshotMutationExecutor {
            mutation: SnapshotMutation::Metadata,
            before: before.clone(),
        },
        store,
    );
    for first in [true, false] {
        let mut req = request("input");
        req.message.task_id = Some("task".into());
        let SendMessageResponse::Task(reply) = h.send_message(&params(), req).await.unwrap() else {
            panic!("snapshot");
        };
        let previous = before.lock().unwrap().clone().unwrap();
        assert_eq!(reply.generation, previous.generation + i64::from(first));
        assert_eq!(reply.metadata.as_ref().unwrap()["phase"], "updated");
    }
    h.shutdown().await;
}

struct ArtifactFieldsExecutor {
    claimed_role: Role,
    snapshot: bool,
    terminal: bool,
    append: bool,
    last_chunk: bool,
}

impl AgentExecutor for ArtifactFieldsExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let artifact: Artifact = serde_json::from_value(serde_json::json!({
            "artifactId":"artifact", "parts":[{"text":"agent chunk"}],
            "role": if self.claimed_role == Role::User { "ROLE_USER" } else { "ROLE_AGENT" }, "startGeneration":"999999", "endGeneration":"999999"
        }))
        .unwrap();
        let status = TaskStatus {
            state: if self.terminal {
                TaskState::Completed
            } else {
                TaskState::InputRequired
            },
            message: None,
            timestamp: None,
        };
        let events = if self.snapshot {
            vec![StreamResponse::Task(Task {
                id: ctx.task_id,
                context_id: ctx.context_id,
                status,
                artifacts: Some(vec![artifact]),
                history: None,
                timeline: None,
                metadata: None,
                generation: 0,
            })]
        } else {
            vec![
                StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                    task_id: ctx.task_id.clone(),
                    context_id: ctx.context_id.clone(),
                    artifact,
                    append: Some(self.append),
                    last_chunk: Some(self.last_chunk),
                    generation: 999999,
                    metadata: None,
                }),
                StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: ctx.task_id,
                    context_id: ctx.context_id,
                    status,
                    generation: 0,
                    metadata: None,
                }),
            ]
        };
        stream::iter(events.into_iter().map(Ok)).boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

async fn seed_owned_artifact(store: &InMemoryTaskStore, role: Role) {
    let mut task = super::test_support::task_with_history("task", "ctx", 0);
    task.status.state = TaskState::InputRequired;
    task.generation = 1;
    let mut artifact: Artifact = serde_json::from_value(
        serde_json::json!({"artifactId":"artifact","parts":[{"text":"original"}]}),
    )
    .unwrap();
    artifact.role = Some(role);
    artifact.start_generation = Some(1);
    artifact.end_generation = Some(1);
    task.artifacts = Some(vec![artifact]);
    store.create(task).await.unwrap();
}

#[tokio::test]
async fn snapshot_artifact_stamps_and_ownership_come_from_server_state() {
    for existing in [false, true] {
        for terminal in [false, true] {
            let store = InMemoryTaskStore::new();
            if existing {
                seed_owned_artifact(&store, Role::Agent).await;
            }
            let h = DefaultRequestHandler::new(
                ArtifactFieldsExecutor {
                    claimed_role: Role::User,
                    snapshot: true,
                    terminal,
                    append: false,
                    last_chunk: false,
                },
                store,
            );
            let mut req = request("artifact");
            req.message.task_id = existing.then(|| "task".into());
            let SendMessageResponse::Task(task) = h.send_message(&params(), req).await.unwrap()
            else {
                panic!("task");
            };
            let artifact = &task.artifacts.as_ref().unwrap()[0];
            assert_eq!(
                artifact.start_generation,
                Some(if existing { 1 } else { task.generation })
            );
            assert_eq!(artifact.end_generation, terminal.then_some(task.generation));
            assert_eq!(artifact.role, Some(Role::Agent));
            assert_eq!(h.tasks.load(&task.id).await.unwrap(), task);
            h.shutdown().await;
        }
    }
}

#[tokio::test]
async fn outgoing_artifact_chunks_have_authoritative_stamps_and_keep_chunk_parts() {
    for append in [false, true] {
        for last_chunk in [false, true] {
            let store = InMemoryTaskStore::new();
            if append {
                seed_owned_artifact(&store, Role::Agent).await;
            }
            let h = DefaultRequestHandler::new(
                ArtifactFieldsExecutor {
                    claimed_role: Role::User,
                    snapshot: false,
                    terminal: false,
                    append,
                    last_chunk,
                },
                store,
            )
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            });
            let mut req = request("artifact");
            req.message.task_id = append.then(|| "task".into());
            let mut events = h.send_streaming_message(&params(), req).await.unwrap();
            let update = tokio::time::timeout(Duration::from_secs(1), async {
                loop {
                    if let StreamResponse::ArtifactUpdate(update) =
                        events.next().await.unwrap().unwrap()
                    {
                        break update;
                    }
                }
            })
            .await
            .unwrap();
            assert_ne!(update.generation, 999999);
            assert_eq!(
                update.artifact.start_generation,
                Some(if append { 1 } else { update.generation })
            );
            assert_eq!(
                update.artifact.end_generation,
                last_chunk.then_some(update.generation)
            );
            assert_eq!(update.artifact.role, Some(Role::Agent));
            assert_eq!(update.artifact.parts, vec![Part::text("agent chunk")]);
            let task = h.tasks.load(&update.task_id).await.unwrap();
            let stored = &task.artifacts.as_ref().unwrap()[0];
            assert_eq!(stored.start_generation, update.artifact.start_generation);
            assert_eq!(stored.end_generation, update.artifact.end_generation);
            assert_eq!(stored.parts.len(), if append { 2 } else { 1 });
            drop(events);
            h.shutdown().await;
        }
    }
}

#[tokio::test]
async fn executor_updates_preserve_client_artifact_ownership_even_if_they_claim_agent_role() {
    for append in [false, true] {
        let store = InMemoryTaskStore::new();
        seed_owned_artifact(&store, Role::User).await;
        let h = DefaultRequestHandler::new(
            ArtifactFieldsExecutor {
                claimed_role: Role::Agent,
                snapshot: false,
                terminal: false,
                append,
                last_chunk: true,
            },
            store,
        );
        let mut req = request("artifact");
        req.message.task_id = Some("task".into());
        h.send_message(&params(), req).await.unwrap();
        let stored = h.tasks.load("task").await.unwrap();
        let artifact = &stored.artifacts.as_ref().unwrap()[0];
        assert_eq!(artifact.role, Some(Role::User));
        assert_eq!(
            artifact.parts,
            if append {
                vec![Part::text("original"), Part::text("agent chunk")]
            } else {
                vec![Part::text("agent chunk")]
            }
        );
        h.shutdown().await;
    }
}

#[tokio::test]
async fn forged_snapshot_ownership_does_not_allow_client_writes_to_agent_artifacts() {
    let store = InMemoryTaskStore::new();
    seed_owned_artifact(&store, Role::Agent).await;
    let h = DefaultRequestHandler::new(
        ArtifactFieldsExecutor {
            claimed_role: Role::User,
            snapshot: true,
            terminal: false,
            append: false,
            last_chunk: false,
        },
        store,
    )
    .with_capabilities(AgentCapabilities {
        user_artifacts: Some(true),
        ..Default::default()
    });
    let mut req = request("artifact");
    req.message.task_id = Some("task".into());
    h.send_message(&params(), req).await.unwrap();
    let mut client = request("overwrite");
    client.message.task_id = Some("task".into());
    client.artifact_updates.push(serde_json::from_value(serde_json::json!({
        "taskId":"task", "contextId":"ctx", "artifact":{"artifactId":"artifact","parts":[{"text":"client overwrite"}]}
    })).unwrap());
    assert_eq!(
        h.send_message(&params(), client).await.unwrap_err().code,
        error_code::INVALID_PARAMS
    );
    let stored = h.tasks.load("task").await.unwrap();
    assert_eq!(
        stored.artifacts.as_ref().unwrap()[0].role,
        Some(Role::Agent)
    );
    assert_eq!(
        stored.artifacts.as_ref().unwrap()[0].parts,
        vec![Part::text("agent chunk")]
    );
    h.shutdown().await;
}

#[tokio::test]
async fn restated_status_snapshots_preserve_the_committed_timestamp_and_generation() {
    let store = InMemoryTaskStore::new();
    let mut task = super::test_support::task_with_history("task", "ctx", 0);
    task.status.state = TaskState::InputRequired;
    task.status.timestamp = Some("2020-01-01T00:00:00Z".parse().unwrap());
    store.create(task).await.unwrap();
    let before = std::sync::Arc::new(std::sync::Mutex::new(None));
    let h = DefaultRequestHandler::new(
        SnapshotMutationExecutor {
            mutation: SnapshotMutation::Timestamp,
            before: before.clone(),
        },
        store,
    );
    let mut req = request("timestamp");
    req.message.task_id = Some("task".into());
    let SendMessageResponse::Task(reply) = h.send_message(&params(), req).await.unwrap() else {
        panic!("task");
    };
    let previous = before.lock().unwrap().clone().unwrap();
    assert_eq!(reply.generation, previous.generation);
    assert_eq!(reply.status.timestamp, previous.status.timestamp);
    assert_eq!(h.tasks.load("task").await.unwrap(), reply);
    h.shutdown().await;
}

struct GatedCancellationAcknowledgement {
    entered: std::sync::Arc<tokio::sync::Notify>,
    release: std::sync::Arc<tokio::sync::Notify>,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    snapshot: bool,
}

impl AgentExecutor for GatedCancellationAcknowledgement {
    fn execute(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::pending().boxed()
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let entered = self.entered.clone();
        let release = self.release.clone();
        let snapshot = self.snapshot;
        stream::once(async move {
            entered.notify_one();
            release.notified().await;
            // Valid cleanup annotations are accepted but cannot rewrite the
            // CANCELED status, timestamp, timeline, or generation already read.
            let status = TaskStatus {
                state: TaskState::Canceled,
                message: Some(Message::new(
                    Role::Agent,
                    vec![Part::text("cleanup finished")],
                )),
                timestamp: Some("2030-01-01T00:00:00Z".parse().unwrap()),
            };
            Ok(if snapshot {
                let mut task = ctx.stored_task.unwrap();
                task.status = status;
                task.generation = 999999;
                StreamResponse::Task(task)
            } else {
                StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: ctx.task_id,
                    context_id: ctx.context_id,
                    status,
                    generation: 999999,
                    metadata: None,
                })
            })
        })
        .boxed()
    }
}

#[tokio::test]
async fn cancellation_cleanup_preserves_the_snapshot_observed_during_the_hook() {
    for running in [false, true] {
        for snapshot in [false, true] {
            let entered = std::sync::Arc::new(tokio::sync::Notify::new());
            let release = std::sync::Arc::new(tokio::sync::Notify::new());
            let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let h = DefaultRequestHandler::new(
                GatedCancellationAcknowledgement {
                    entered: entered.clone(),
                    release: release.clone(),
                    calls: calls.clone(),
                    snapshot,
                },
                InMemoryTaskStore::new(),
            )
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            });
            super::test_support::seed_task(&h, "task", "ctx").await;
            if running {
                let mut req = request("pending");
                req.message.task_id = Some("task".into());
                let mut options = config();
                options.return_immediately = Some(true);
                req.configuration = Some(options);
                h.send_message(&params(), req).await.unwrap();
            }
            let mut events = h
                .subscribe_to_task(
                    &params(),
                    SubscribeToTaskRequest {
                        id: "task".into(),
                        tenant: None,
                        last_seen_generation: None,
                    },
                )
                .await
                .unwrap();
            let StreamResponse::Task(initial) = events.next().await.unwrap().unwrap() else {
                panic!("opening task");
            };
            let cancel_request = || CancelTaskRequest {
                id: "task".into(),
                tenant: None,
                metadata: None,
            };
            let canceling = tokio::spawn({
                let h = h.clone();
                let req = cancel_request();
                async move { h.cancel_task(&params(), req).await }
            });
            tokio::time::timeout(Duration::from_secs(1), entered.notified())
                .await
                .unwrap();
            let observed = h
                .get_task(
                    &params(),
                    GetTaskRequest {
                        id: "task".into(),
                        tenant: None,
                        history_length: None,
                        timeline_length: None,
                        current_generation: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(observed.status.state, TaskState::Canceled);
            assert_eq!(observed.generation, initial.generation + 1);
            assert!(observed.status.message.is_none());
            let concurrent = h.cancel_task(&params(), cancel_request()).await.unwrap();
            assert_eq!(concurrent, observed);
            release.notify_one();
            let returned = tokio::time::timeout(Duration::from_secs(1), canceling)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(returned, observed);
            assert_eq!(h.tasks.load("task").await.unwrap(), observed);
            let updates: Vec<_> =
                tokio::time::timeout(Duration::from_secs(1), events.collect::<Vec<_>>())
                    .await
                    .unwrap();
            assert_eq!(updates.len(), 1, "cancellation must be announced once");
            let StreamResponse::StatusUpdate(update) = updates[0].as_ref().unwrap() else {
                panic!("cancellation status");
            };
            assert_eq!(update.generation, observed.generation);
            assert_eq!(update.status, observed.status);
            assert_eq!(
                h.cancel_task(&params(), cancel_request()).await.unwrap(),
                observed
            );
            assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            h.shutdown().await;
        }
    }
}
