// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

pub(super) use super::updates::apply_event_to_task;
pub(super) use super::{DefaultRequestHandler, RequestHandler, ServiceParams};
pub(super) use crate::executor::ExecutorContext;
pub(super) use crate::pagination::{DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE};
pub(super) use crate::push::{InMemoryPushConfigStore, PushConfigStore};
pub(super) use crate::task_store::{InMemoryTaskStore, TaskStore, TaskVersion};
pub(super) use a2a::*;
pub(super) use async_trait::async_trait;
pub(super) use futures::stream::BoxStream;

pub(super) use crate::test_util::install_crypto_provider;
pub(super) use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    routing::post,
};
pub(super) use futures::stream;
pub(super) use std::sync::Arc;
pub(super) use std::sync::atomic::{AtomicI32, Ordering};
pub(super) use tokio::{
    net::TcpListener,
    sync::{Notify, mpsc, oneshot},
    time::{Duration, timeout},
};

pub(super) struct EchoExecutor;

pub(super) struct PushEventExecutor;

pub(super) struct ResumableExecutor {
    pub(super) release: Arc<Notify>,
}

#[derive(Debug)]
pub(super) struct CapturedPush {
    pub(super) authorization: Option<String>,
    pub(super) notification_token: Option<String>,
    pub(super) event: StreamResponse,
}

impl crate::AgentExecutor for EchoExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let task = Task {
            generation: 0,
            timeline: None,
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: ctx.message,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let task = Task {
            generation: 0,
            timeline: None,
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Canceled,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
    }
}

impl crate::AgentExecutor for PushEventExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let working = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            generation: 0,
            task_id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        let completed = StreamResponse::Task(Task {
            generation: 0,
            timeline: None,
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: ctx.message,
                timestamp: None,
            },
            artifacts: None,
            history: ctx.stored_task.and_then(|task| task.history),
            metadata: None,
        });

        Box::pin(stream::iter(vec![Ok(working), Ok(completed)]))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let task = Task {
            generation: 0,
            timeline: None,
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Canceled,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
    }
}

impl crate::AgentExecutor for ResumableExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let release = self.release.clone();
        let working_task = Task {
            generation: 0,
            timeline: None,
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: ctx.stored_task.and_then(|task| task.history),
            metadata: None,
        };
        let completed_task = Task {
            generation: 0,
            timeline: None,
            id: working_task.id.clone(),
            context_id: working_task.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: working_task.history.clone(),
            metadata: None,
        };

        Box::pin(stream::unfold(
            (Some(working_task), Some((release, completed_task))),
            |(working_task, completion)| async move {
                if let Some(task) = working_task {
                    return Some((Ok(StreamResponse::Task(task)), (None, completion)));
                }

                if let Some((release, task)) = completion {
                    release.notified().await;
                    return Some((Ok(StreamResponse::Task(task)), (None, None)));
                }

                None
            },
        ))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let task = Task {
            generation: 0,
            timeline: None,
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Canceled,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
    }
}

/// Mirrors itk's own `tck-multi-turn` ACTS behavior: only ever emits a
/// `StatusUpdate`, never a full `Task`, so a test against it exercises
/// exactly the path a real executor that never touches `history` relies
/// on the server to handle.
pub(super) struct MultiTurnExecutor;

impl crate::AgentExecutor for MultiTurnExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let done = ctx
            .message
            .as_ref()
            .and_then(Message::text)
            .is_some_and(|text| text.trim().eq_ignore_ascii_case("done"));
        let state = if done {
            TaskState::Completed
        } else {
            TaskState::InputRequired
        };
        let update = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            generation: 0,
            task_id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        Box::pin(stream::once(async move { Ok(update) }))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let task = Task {
            generation: 0,
            timeline: None,
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Canceled,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
    }
}

/// A `TaskStore` that behaves exactly like `InMemoryTaskStore` except
/// for `list`, which it drives from the fields below.
///
/// `TaskStore` is public, so nothing obliges a third-party
/// implementation to bound its own response. Only a decorator can model
/// that; everything else delegates, so a cap test cannot accidentally
/// depend on stub behaviour.
pub(super) struct PagingTaskStore {
    pub(super) inner: InMemoryTaskStore,
    pub(super) count: usize,
    /// `true` models a well-behaved store, bounded by the handler
    /// clamping the request; `false` models one that ignores
    /// `page_size`, bounded only by the handler truncating the response.
    pub(super) honour_page_size: bool,
    /// The `page_size` the handler actually asked for. Recorded because
    /// the response bound alone cannot distinguish a clamped request
    /// from an unclamped one -- both end up 100 items wide.
    pub(super) seen_page_size: Arc<AtomicI32>,
    /// Continuation token the store reports, so truncation's handling of
    /// it can be observed.
    pub(super) next_page_token: String,
}

impl PagingTaskStore {
    pub(super) fn new(count: usize, honour_page_size: bool) -> Self {
        Self {
            inner: InMemoryTaskStore::new(),
            count,
            honour_page_size,
            seen_page_size: Arc::new(AtomicI32::new(-1)),
            next_page_token: String::new(),
        }
    }

    pub(super) fn with_next_page_token(mut self, token: &str) -> Self {
        self.next_page_token = token.to_string();
        self
    }
}

#[async_trait]
impl TaskStore for PagingTaskStore {
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.inner.create(task).await
    }

    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.inner.update(task).await
    }

    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        self.inner.get(task_id).await
    }

    async fn begin_cancel(&self, task_id: &str) -> Result<Task, A2AError> {
        self.inner.begin_cancel(task_id).await
    }

    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.seen_page_size
            .store(req.page_size.unwrap_or(-1), Ordering::SeqCst);
        let served = if self.honour_page_size {
            req.page_size.unwrap_or(i32::MAX).max(0) as usize
        } else {
            self.count
        }
        .min(self.count);
        let tasks = (0..served)
            .map(|i| Task {
                generation: 0,
                timeline: None,
                id: format!("t-{i:04}"),
                context_id: "c1".into(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            })
            .collect::<Vec<_>>();
        let total = tasks.len() as i32;
        Ok(ListTasksResponse {
            tasks,
            next_page_token: self.next_page_token.clone(),
            page_size: total,
            total_size: total,
        })
    }
}

/// The push-config equivalent, and the only way to reach the handler's
/// page cap on that path: `InMemoryPushConfigStore` caps a task at
/// `MAX_PUSH_CONFIGS_PER_TASK` (50), below the page cap of 100, so
/// nothing routed through it can make the clamp bind. Everything but
/// `list` delegates.
pub(super) struct PagingPushConfigStore {
    pub(super) inner: InMemoryPushConfigStore,
    pub(super) count: usize,
}

impl PagingPushConfigStore {
    pub(super) fn new(count: usize) -> Self {
        Self {
            inner: InMemoryPushConfigStore::new(),
            count,
        }
    }
}

#[async_trait]
impl crate::PushConfigStore for PagingPushConfigStore {
    async fn save(
        &self,
        config: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.inner.save(config).await
    }

    async fn get(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.inner.get(task_id, config_id).await
    }

    async fn list(&self, task_id: &str) -> Result<Vec<TaskPushNotificationConfig>, A2AError> {
        Ok((0..self.count)
            .map(|i| TaskPushNotificationConfig {
                protocol_version: None,
                task_id: task_id.to_string(),
                url: format!("https://example.com/hook/{i}"),
                id: Some(format!("cfg-{i:04}")),
                token: None,
                authentication: None,
                tenant: None,
            })
            .collect())
    }

    async fn delete(&self, task_id: &str, config_id: &str) -> Result<(), A2AError> {
        self.inner.delete(task_id, config_id).await
    }

    async fn delete_all(&self, task_id: &str) -> Result<(), A2AError> {
        self.inner.delete_all(task_id).await
    }
}

/// Pre-create a task so a message may legitimately reference it: A2A
/// §3.4.2 forbids client-provided taskIds for *creating* tasks.
pub(super) async fn seed_task(handler: &DefaultRequestHandler, task_id: &str, context_id: &str) {
    handler
        .task_store
        .create(Task {
            generation: 0,
            timeline: None,
            id: task_id.into(),
            context_id: context_id.into(),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        })
        .await
        .unwrap();
}

pub(super) fn make_handler() -> DefaultRequestHandler {
    DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
}

pub(super) fn make_handler_with_push_configs() -> DefaultRequestHandler {
    DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
        .with_push_config_store(InMemoryPushConfigStore::new())
}

pub(super) fn make_push_delivery_handler() -> DefaultRequestHandler {
    // URL validation is disabled because the test webhook runs on
    // 127.0.0.1; the default HttpPushSender rejects loopback targets.
    DefaultRequestHandler::new(PushEventExecutor, InMemoryTaskStore::new()).with_push_notifications(
        InMemoryPushConfigStore::new(),
        crate::HttpPushSender::new(Some(crate::HttpPushSenderConfig {
            validate_urls: false,
            ..Default::default()
        })),
    )
}

pub(super) fn make_resumable_handler() -> (DefaultRequestHandler, Arc<Notify>) {
    let release = Arc::new(Notify::new());
    (
        DefaultRequestHandler::new(
            ResumableExecutor {
                release: release.clone(),
            },
            InMemoryTaskStore::new(),
        ),
        release,
    )
}

pub(super) fn make_message() -> Message {
    Message::new(Role::User, vec![Part::text("hello")])
}

pub(super) async fn capture_push(
    State(sender): State<mpsc::UnboundedSender<CapturedPush>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    let captured = CapturedPush {
        authorization: headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned),
        notification_token: headers
            .get("A2A-Notification-Token")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned),
        event: serde_json::from_slice(&body).unwrap(),
    };
    sender.send(captured).unwrap();
    StatusCode::ACCEPTED
}

pub(super) async fn start_push_webhook() -> (
    String,
    mpsc::UnboundedReceiver<CapturedPush>,
    oneshot::Sender<()>,
    tokio::task::JoinHandle<()>,
) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let app = Router::new()
        .route("/", post(capture_push))
        .with_state(sender);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });

    (format!("http://{address}/"), receiver, shutdown_tx, server)
}

pub(super) async fn next_push(
    receiver: &mut mpsc::UnboundedReceiver<CapturedPush>,
) -> CapturedPush {
    timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

// A2A §3.4.2 / §3.4.3: taskId and contextId rules on send_message (#248).

pub(super) fn send_req(task_id: Option<&str>, context_id: Option<&str>) -> SendMessageRequest {
    let mut msg = make_message();
    msg.task_id = task_id.map(Into::into);
    msg.context_id = context_id.map(Into::into);
    SendMessageRequest {
        artifact_updates: Vec::new(),
        message: msg,
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

pub(super) fn task_with_history(id: &str, context_id: &str, n: usize) -> Task {
    Task {
        generation: 0,
        timeline: None,
        id: id.into(),
        context_id: context_id.into(),
        status: TaskStatus {
            state: TaskState::Completed,
            message: None,
            timestamp: None,
        },
        artifacts: None,
        history: Some(
            (0..n)
                .map(|i| Message::new(Role::User, vec![Part::text(format!("m{i}"))]))
                .collect(),
        ),
        metadata: None,
    }
}

pub(super) fn reject_unauthenticated(
    _params: &ServiceParams,
    _task_id: Option<&str>,
) -> Result<(), A2AError> {
    Err(A2AError::invalid_request("authentication required"))
}

// §13.3 (#203): a capability the card does not declare is not served.

pub(super) fn caps(streaming: Option<bool>, push: Option<bool>) -> AgentCapabilities {
    AgentCapabilities {
        user_artifacts: None,
        streaming,
        push_notifications: push,
        extensions: None,
        extended_agent_card: None,
    }
}

pub(super) fn send_request() -> SendMessageRequest {
    SendMessageRequest {
        artifact_updates: Vec::new(),
        message: Message::new(Role::User, vec![Part::text("hi")]),
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

pub(super) fn push_config(task_id: &str) -> TaskPushNotificationConfig {
    TaskPushNotificationConfig {
        protocol_version: None,
        task_id: task_id.into(),
        url: "https://example.com/callback".into(),
        id: Some("cfg-1".into()),
        token: None,
        authentication: None,
        tenant: None,
    }
}

// §12.4 / §10.1 (#204): the extended Agent Card.

pub(super) fn extended_card(name: &str) -> AgentCard {
    AgentCard {
        name: name.into(),
        description: "extended".into(),
        version: "1.0".into(),
        supported_interfaces: vec![],
        capabilities: AgentCapabilities::default(),
        default_input_modes: vec![],
        default_output_modes: vec![],
        skills: vec![],
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes: None,
        security_requirements: None,
        signatures: None,
    }
}

pub(super) fn extended_request() -> GetExtendedAgentCardRequest {
    GetExtendedAgentCardRequest { tenant: None }
}
