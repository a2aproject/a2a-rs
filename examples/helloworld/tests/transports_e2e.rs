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
use a2a_server::{RequestHandler, ServiceParams, WELL_KNOWN_AGENT_CARD_PATH};
use async_trait::async_trait;
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use reqwest::Client;
use tokio::net::TcpListener;

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

    let resolver = AgentCardResolver::new(Client::new());
    let card = resolver
        .resolve(format!("{base_url}{WELL_KNOWN_AGENT_CARD_PATH}"))
        .await
        .unwrap();
    assert_eq!(card.name, "Test Agent");

    let err = resolver
        .resolve(format!("{base_url}/bad/.well-known/agent-card.json"))
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::INTERNAL_ERROR);

    let err = resolver
        .resolve(format!("{base_url}/missing/.well-known/agent-card.json"))
        .await
        .unwrap_err();
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

    let subscribe_missing = transport
        .subscribe_to_task(
            &ServiceParams::new(),
            &SubscribeToTaskRequest {
                id: "missing".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(subscribe_missing.code, error_code::TASK_NOT_FOUND);

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
