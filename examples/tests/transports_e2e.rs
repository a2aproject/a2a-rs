// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;
use std::time::Duration;

use a2a::event::{StreamResponse, TaskStatusUpdateEvent};
use a2a::*;
use a2a_client::Transport;
use a2a_client::agent_card::AgentCardResolver;
use a2a_client::jsonrpc::JsonRpcTransport;
use a2a_client::rest::RestTransport;
use a2a_server::jsonrpc::jsonrpc_router;
use a2a_server::rest::rest_router;
use a2a_server::{
    DefaultRequestHandler, ExecutorContext, HttpPushSender, InMemoryPushConfigStore,
    InMemoryTaskStore, RequestHandler, ServiceParams, WELL_KNOWN_AGENT_CARD_PATH,
};
use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use reqwest::Client;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::time::timeout;

fn sample_message(role: Role, text: &str) -> Message {
    Message {
        message_id: format!("msg-{text}"),
        context_id: Some("ctx-1".to_string()),
        task_id: Some("task-1".to_string()),
        role,
        parts: vec![Part::text(text)],
        metadata: None,
        extensions: None,
        reference_task_ids: None,
    }
}

fn sample_task(id: &str, state: TaskState) -> Task {
    Task {
        id: id.to_string(),
        context_id: "ctx-1".to_string(),
        status: TaskStatus {
            state,
            message: Some(sample_message(Role::Agent, "done")),
            timestamp: None,
        },
        artifacts: None,
        history: None,
        metadata: None,
    }
}

fn sample_push_config(id: &str) -> TaskPushNotificationConfig {
    TaskPushNotificationConfig {
        task_id: "task-1".to_string(),
        tenant: None,
        config: PushNotificationConfig {
            url: "https://example.com/callback".to_string(),
            id: Some(id.to_string()),
            token: Some("tok-1".to_string()),
            authentication: Some(AuthenticationInfo {
                scheme: "Bearer".to_string(),
                credentials: Some("secret".to_string()),
            }),
        },
    }
}

fn sample_agent_card() -> AgentCard {
    AgentCard {
        name: "Test Agent".to_string(),
        description: "Integration test agent".to_string(),
        version: VERSION.to_string(),
        supported_interfaces: vec![
            AgentInterface::new("http://localhost/rest", TRANSPORT_PROTOCOL_HTTP_JSON),
            AgentInterface::new("http://localhost/rpc", TRANSPORT_PROTOCOL_JSONRPC),
        ],
        capabilities: AgentCapabilities::default(),
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
        skills: vec![],
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes: None,
        security_requirements: None,
        signatures: None,
    }
}

struct TestHandler;

struct PushTransportExecutor;

#[derive(Debug)]
struct CapturedPush {
    authorization: Option<String>,
    notification_token: Option<String>,
    event: StreamResponse,
}

#[async_trait]
impl RequestHandler for TestHandler {
    async fn send_message(
        &self,
        _params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let task_id = req.message.task_id.unwrap_or_else(|| "task-1".to_string());
        Ok(SendMessageResponse::Task(sample_task(
            &task_id,
            TaskState::Completed,
        )))
    }

    async fn send_streaming_message(
        &self,
        _params: &ServiceParams,
        _req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        Ok(Box::pin(stream::iter(vec![
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: "task-1".to_string(),
                context_id: "ctx-1".to_string(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            })),
            Ok(StreamResponse::Task(sample_task(
                "task-1",
                TaskState::Completed,
            ))),
        ])))
    }

    async fn get_task(
        &self,
        _params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        if req.id == "missing" {
            return Err(A2AError::task_not_found(&req.id));
        }
        Ok(sample_task(&req.id, TaskState::Completed))
    }

    async fn list_tasks(
        &self,
        _params: &ServiceParams,
        _req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        Ok(ListTasksResponse {
            tasks: vec![sample_task("task-1", TaskState::Completed)],
            next_page_token: "next-page".to_string(),
            page_size: 1,
            total_size: 1,
        })
    }

    async fn cancel_task(
        &self,
        _params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        if req.id == "missing" {
            return Err(A2AError::task_not_found(&req.id));
        }
        Ok(sample_task(&req.id, TaskState::Canceled))
    }

    async fn subscribe_to_task(
        &self,
        _params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        if req.id == "missing" {
            return Err(A2AError::task_not_found(&req.id));
        }
        Ok(Box::pin(stream::once(async move {
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: req.id,
                context_id: "ctx-1".to_string(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            }))
        })))
    }

    async fn create_push_config(
        &self,
        _params: &ServiceParams,
        req: CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        if req.task_id == "missing" {
            return Err(A2AError::task_not_found(&req.task_id));
        }
        Ok(TaskPushNotificationConfig {
            task_id: req.task_id,
            config: req.config,
            tenant: req.tenant,
        })
    }

    async fn get_push_config(
        &self,
        _params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        if req.id == "missing" {
            return Err(A2AError::task_not_found(&req.id));
        }
        Ok(TaskPushNotificationConfig {
            task_id: req.task_id,
            tenant: req.tenant,
            config: PushNotificationConfig {
                url: "https://example.com/callback".to_string(),
                id: Some(req.id),
                token: Some("tok-1".to_string()),
                authentication: None,
            },
        })
    }

    async fn list_push_configs(
        &self,
        _params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        Ok(ListTaskPushNotificationConfigsResponse {
            configs: vec![TaskPushNotificationConfig {
                task_id: req.task_id,
                tenant: req.tenant,
                ..sample_push_config("cfg-1")
            }],
            next_page_token: Some("next".to_string()),
        })
    }

    async fn delete_push_config(
        &self,
        _params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        if req.id == "missing" {
            return Err(A2AError::task_not_found(&req.id));
        }
        Ok(())
    }

    async fn get_extended_agent_card(
        &self,
        _params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        Ok(sample_agent_card())
    }
}

impl a2a_server::AgentExecutor for PushTransportExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let working = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
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
            id: ctx.task_id,
            context_id: ctx.context_id,
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
        Box::pin(stream::once(async move {
            Ok(StreamResponse::Task(Task {
                id: ctx.task_id,
                context_id: ctx.context_id,
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            }))
        }))
    }
}

async fn spawn_http_server() -> (String, tokio::task::JoinHandle<()>) {
    let handler = Arc::new(TestHandler);
    let app = Router::new()
        .nest("/rest", rest_router(handler.clone()))
        .nest("/rpc", jsonrpc_router(handler))
        .route(
            WELL_KNOWN_AGENT_CARD_PATH,
            get(|| async { Json(sample_agent_card()) }),
        )
        .route(
            "/bad/.well-known/agent-card.json",
            get(|| async { "not-json" }),
        )
        .route(
            "/missing/.well-known/agent-card.json",
            get(|| async { StatusCode::INTERNAL_SERVER_ERROR }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), handle)
}

async fn spawn_push_http_server() -> (String, tokio::task::JoinHandle<()>) {
    let handler = Arc::new(
        DefaultRequestHandler::new(PushTransportExecutor, InMemoryTaskStore::new())
            .with_push_notifications(InMemoryPushConfigStore::new(), HttpPushSender::new(None)),
    );
    let app = Router::new()
        .nest("/rest", rest_router(handler.clone()))
        .nest("/rpc", jsonrpc_router(handler))
        .route(
            WELL_KNOWN_AGENT_CARD_PATH,
            get(|| async { Json(sample_agent_card()) }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), handle)
}

async fn capture_push(
    State(sender): State<mpsc::UnboundedSender<CapturedPush>>,
    headers: HeaderMap,
    body: Bytes,
) -> StatusCode {
    sender
        .send(CapturedPush {
            authorization: headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned),
            notification_token: headers
                .get("A2A-Notification-Token")
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned),
            event: serde_json::from_slice(&body).unwrap(),
        })
        .unwrap();
    StatusCode::ACCEPTED
}

async fn spawn_webhook_server() -> (
    String,
    mpsc::UnboundedReceiver<CapturedPush>,
    tokio::task::JoinHandle<()>,
) {
    let (sender, receiver) = mpsc::unbounded_channel();
    let app = Router::new()
        .route("/", post(capture_push))
        .with_state(sender);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}/"), receiver, handle)
}

async fn recv_push(receiver: &mut mpsc::UnboundedReceiver<CapturedPush>) -> CapturedPush {
    timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

fn send_message_request() -> SendMessageRequest {
    SendMessageRequest {
        message: sample_message(Role::User, "hello"),
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

#[tokio::test]
async fn rest_transport_end_to_end() {
    let (base_url, handle) = spawn_http_server().await;
    let transport = RestTransport::new(Client::new(), format!("{base_url}/rest"));

    let send_resp = transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await;
    assert!(matches!(send_resp.unwrap(), SendMessageResponse::Task(_)));

    let task = transport
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "task-1".to_string(),
                history_length: Some(2),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(task.id, "task-1");

    let not_found = transport
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "missing".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(not_found.code, error_code::TASK_NOT_FOUND);

    let list = transport
        .list_tasks(
            &ServiceParams::new(),
            &ListTasksRequest {
                context_id: Some("ctx-1".to_string()),
                status: Some(TaskState::Completed),
                page_size: Some(10),
                page_token: Some("page-1".to_string()),
                history_length: Some(1),
                status_timestamp_after: None,
                include_artifacts: Some(true),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(list.tasks.len(), 1);

    let canceled = transport
        .cancel_task(
            &ServiceParams::new(),
            &CancelTaskRequest {
                id: "task-1".to_string(),
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(canceled.status.state, TaskState::Canceled);

    let cancel_missing = transport
        .cancel_task(
            &ServiceParams::new(),
            &CancelTaskRequest {
                id: "missing".to_string(),
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(cancel_missing.code, error_code::TASK_NOT_FOUND);

    let subscribed = transport
        .subscribe_to_task(
            &ServiceParams::new(),
            &SubscribeToTaskRequest {
                id: "task-1".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();
    let events: Vec<_> = subscribed.collect().await;
    assert_eq!(events.len(), 1);

    let created = transport
        .create_push_config(
            &ServiceParams::new(),
            &CreateTaskPushNotificationConfigRequest {
                task_id: "task-1".to_string(),
                config: sample_push_config("cfg-1").config,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(created.task_id, "task-1");

    let created_missing = transport
        .create_push_config(
            &ServiceParams::new(),
            &CreateTaskPushNotificationConfigRequest {
                task_id: "missing".to_string(),
                config: sample_push_config("cfg-1").config,
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(created_missing.code, error_code::TASK_NOT_FOUND);

    let fetched = transport
        .get_push_config(
            &ServiceParams::new(),
            &GetTaskPushNotificationConfigRequest {
                task_id: "task-1".to_string(),
                id: "cfg-1".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(fetched.config.id.as_deref(), Some("cfg-1"));

    let fetched_missing = transport
        .get_push_config(
            &ServiceParams::new(),
            &GetTaskPushNotificationConfigRequest {
                task_id: "task-1".to_string(),
                id: "missing".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(fetched_missing.code, error_code::TASK_NOT_FOUND);

    let configs = transport
        .list_push_configs(
            &ServiceParams::new(),
            &ListTaskPushNotificationConfigsRequest {
                task_id: "task-1".to_string(),
                page_size: Some(5),
                page_token: Some("page-1".to_string()),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(configs.configs.len(), 1);

    transport
        .delete_push_config(
            &ServiceParams::new(),
            &DeleteTaskPushNotificationConfigRequest {
                task_id: "task-1".to_string(),
                id: "cfg-1".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();

    let delete_missing = transport
        .delete_push_config(
            &ServiceParams::new(),
            &DeleteTaskPushNotificationConfigRequest {
                task_id: "task-1".to_string(),
                id: "missing".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(delete_missing.code, error_code::TASK_NOT_FOUND);

    let resolver = AgentCardResolver::new(Some(Client::new()));
    let card = resolver.resolve(&base_url).await.unwrap();
    assert_eq!(card.name, "Test Agent");

    let bad_card_url = format!("{base_url}/bad");
    let err = resolver.resolve(&bad_card_url).await.unwrap_err();
    assert_eq!(err.code, error_code::INTERNAL_ERROR);

    let missing_card_url = format!("{base_url}/missing");
    let err = resolver.resolve(&missing_card_url).await.unwrap_err();
    assert_eq!(err.code, error_code::INTERNAL_ERROR);

    handle.abort();
}

#[tokio::test]
async fn jsonrpc_transport_end_to_end() {
    let (base_url, handle) = spawn_http_server().await;
    let transport = JsonRpcTransport::new(Client::new(), format!("{base_url}/rpc"));

    let send_resp = transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await;
    assert!(matches!(send_resp.unwrap(), SendMessageResponse::Task(_)));

    let stream = transport
        .send_streaming_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();
    let items: Vec<_> = stream.collect().await;
    assert_eq!(items.len(), 2);

    let task = transport
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "task-1".to_string(),
                history_length: Some(2),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(task.id, "task-1");

    let not_found = transport
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "missing".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(not_found.code, error_code::TASK_NOT_FOUND);

    let list = transport
        .list_tasks(
            &ServiceParams::new(),
            &ListTasksRequest {
                context_id: Some("ctx-1".to_string()),
                status: Some(TaskState::Completed),
                page_size: Some(10),
                page_token: Some("page-1".to_string()),
                history_length: Some(1),
                status_timestamp_after: None,
                include_artifacts: Some(true),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(list.tasks.len(), 1);

    let canceled = transport
        .cancel_task(
            &ServiceParams::new(),
            &CancelTaskRequest {
                id: "task-1".to_string(),
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(canceled.status.state, TaskState::Canceled);

    let cancel_missing = transport
        .cancel_task(
            &ServiceParams::new(),
            &CancelTaskRequest {
                id: "missing".to_string(),
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(cancel_missing.code, error_code::TASK_NOT_FOUND);

    let subscribed = transport
        .subscribe_to_task(
            &ServiceParams::new(),
            &SubscribeToTaskRequest {
                id: "task-1".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();
    let events: Vec<_> = subscribed.collect().await;
    assert_eq!(events.len(), 1);

    let created = transport
        .create_push_config(
            &ServiceParams::new(),
            &CreateTaskPushNotificationConfigRequest {
                task_id: "task-1".to_string(),
                config: sample_push_config("cfg-1").config,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(created.task_id, "task-1");

    let created_missing = transport
        .create_push_config(
            &ServiceParams::new(),
            &CreateTaskPushNotificationConfigRequest {
                task_id: "missing".to_string(),
                config: sample_push_config("cfg-1").config,
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(created_missing.code, error_code::TASK_NOT_FOUND);

    let fetched = transport
        .get_push_config(
            &ServiceParams::new(),
            &GetTaskPushNotificationConfigRequest {
                task_id: "task-1".to_string(),
                id: "cfg-1".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(fetched.config.id.as_deref(), Some("cfg-1"));

    let fetched_missing = transport
        .get_push_config(
            &ServiceParams::new(),
            &GetTaskPushNotificationConfigRequest {
                task_id: "task-1".to_string(),
                id: "missing".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(fetched_missing.code, error_code::TASK_NOT_FOUND);

    let listed = transport
        .list_push_configs(
            &ServiceParams::new(),
            &ListTaskPushNotificationConfigsRequest {
                task_id: "task-1".to_string(),
                page_size: Some(5),
                page_token: Some("page-1".to_string()),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(listed.configs.len(), 1);

    transport
        .delete_push_config(
            &ServiceParams::new(),
            &DeleteTaskPushNotificationConfigRequest {
                task_id: "task-1".to_string(),
                id: "cfg-1".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();

    let delete_missing = transport
        .delete_push_config(
            &ServiceParams::new(),
            &DeleteTaskPushNotificationConfigRequest {
                task_id: "task-1".to_string(),
                id: "missing".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(delete_missing.code, error_code::TASK_NOT_FOUND);

    let card = transport
        .get_extended_agent_card(
            &ServiceParams::new(),
            &GetExtendedAgentCardRequest { tenant: None },
        )
        .await
        .unwrap();
    assert_eq!(card.name, "Test Agent");

    handle.abort();
}

#[tokio::test]
async fn rest_transport_push_delivery_end_to_end() {
    let (base_url, server_handle) = spawn_push_http_server().await;
    let (webhook_url, mut receiver, webhook_handle) = spawn_webhook_server().await;
    let transport = RestTransport::new(Client::new(), format!("{base_url}/rest"));

    transport
        .create_push_config(
            &ServiceParams::new(),
            &CreateTaskPushNotificationConfigRequest {
                task_id: "task-rest-push".to_string(),
                config: PushNotificationConfig {
                    url: webhook_url,
                    id: Some("cfg-rest".to_string()),
                    token: Some("rest-token".to_string()),
                    authentication: Some(AuthenticationInfo {
                        scheme: "Basic".to_string(),
                        credentials: Some("dGVzdDpzZWNyZXQ=".to_string()),
                    }),
                },
                tenant: None,
            },
        )
        .await
        .unwrap();

    let mut request = send_message_request();
    request.message.task_id = Some("task-rest-push".to_string());
    request.message.context_id = Some("ctx-rest-push".to_string());

    let response = transport
        .send_message(&ServiceParams::new(), &request)
        .await
        .unwrap();
    assert!(matches!(response, SendMessageResponse::Task(_)));

    let first = recv_push(&mut receiver).await;
    assert_eq!(
        first.authorization.as_deref(),
        Some("Basic dGVzdDpzZWNyZXQ=")
    );
    assert_eq!(first.notification_token.as_deref(), Some("rest-token"));
    match first.event {
        StreamResponse::StatusUpdate(update) => {
            assert_eq!(update.task_id, "task-rest-push");
            assert_eq!(update.status.state, TaskState::Working);
        }
        _ => panic!("expected status update push"),
    }

    let second = recv_push(&mut receiver).await;
    assert_eq!(
        second.authorization.as_deref(),
        Some("Basic dGVzdDpzZWNyZXQ=")
    );
    assert_eq!(second.notification_token.as_deref(), Some("rest-token"));
    match second.event {
        StreamResponse::Task(task) => {
            assert_eq!(task.id, "task-rest-push");
            assert_eq!(task.status.state, TaskState::Completed);
        }
        _ => panic!("expected final task push"),
    }

    server_handle.abort();
    webhook_handle.abort();
}

#[tokio::test]
async fn jsonrpc_transport_push_delivery_end_to_end() {
    let (base_url, server_handle) = spawn_push_http_server().await;
    let (webhook_url, mut receiver, webhook_handle) = spawn_webhook_server().await;
    let transport = JsonRpcTransport::new(Client::new(), format!("{base_url}/rpc"));

    let mut request = send_message_request();
    request.message.task_id = Some("task-rpc-push".to_string());
    request.message.context_id = Some("ctx-rpc-push".to_string());
    request.configuration = Some(SendMessageConfiguration {
        accepted_output_modes: None,
        push_notification_config: Some(PushNotificationConfig {
            url: webhook_url.clone(),
            id: Some("cfg-rpc".to_string()),
            token: Some("rpc-token".to_string()),
            authentication: Some(AuthenticationInfo {
                scheme: "Bearer".to_string(),
                credentials: Some("rpc-secret".to_string()),
            }),
        }),
        history_length: None,
        return_immediately: None,
    });

    let response = transport
        .send_message(&ServiceParams::new(), &request)
        .await
        .unwrap();
    assert!(matches!(response, SendMessageResponse::Task(_)));

    let saved = transport
        .get_push_config(
            &ServiceParams::new(),
            &GetTaskPushNotificationConfigRequest {
                task_id: "task-rpc-push".to_string(),
                id: "cfg-rpc".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(saved.config.url, webhook_url);

    let first = recv_push(&mut receiver).await;
    assert_eq!(first.authorization.as_deref(), Some("Bearer rpc-secret"));
    assert_eq!(first.notification_token.as_deref(), Some("rpc-token"));
    match first.event {
        StreamResponse::StatusUpdate(update) => {
            assert_eq!(update.task_id, "task-rpc-push");
            assert_eq!(update.status.state, TaskState::Working);
        }
        _ => panic!("expected status update push"),
    }

    let second = recv_push(&mut receiver).await;
    assert_eq!(second.authorization.as_deref(), Some("Bearer rpc-secret"));
    assert_eq!(second.notification_token.as_deref(), Some("rpc-token"));
    match second.event {
        StreamResponse::Task(task) => {
            assert_eq!(task.id, "task-rpc-push");
            assert_eq!(task.status.state, TaskState::Completed);
        }
        _ => panic!("expected final task push"),
    }

    server_handle.abort();
    webhook_handle.abort();
}
