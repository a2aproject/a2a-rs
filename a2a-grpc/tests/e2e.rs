// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;
use std::time::Duration;

use a2a::event::{StreamResponse, TaskStatusUpdateEvent};
use a2a::*;
use a2a_client::{Transport, TransportFactory};
use a2a_grpc::{GrpcHandler, GrpcTransport, GrpcTransportFactory};
use a2a_pb::proto::a2a_service_server::A2aServiceServer;
use a2a_server::{DefaultRequestHandler, InMemoryTaskStore, RequestHandler, ServiceParams};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

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
            authentication: None,
        },
    }
}

fn sample_agent_card() -> AgentCard {
    AgentCard {
        name: "gRPC Test Agent".to_string(),
        description: "Integration test agent".to_string(),
        version: VERSION.to_string(),
        supported_interfaces: vec![AgentInterface::new(
            "http://localhost:50051",
            TRANSPORT_PROTOCOL_GRPC,
        )],
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

struct StoredTaskExecutor;

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

impl a2a_server::AgentExecutor for StoredTaskExecutor {
    fn execute(
        &self,
        ctx: a2a_server::ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let response = StreamResponse::Task(Task {
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: Some(Message {
                    message_id: "stored-task-response".to_string(),
                    context_id: Some(ctx.context_id.clone()),
                    task_id: Some(ctx.task_id.clone()),
                    role: Role::Agent,
                    parts: vec![Part::text("stored-task-done")],
                    metadata: None,
                    extensions: None,
                    reference_task_ids: None,
                }),
                timestamp: None,
            },
            artifacts: None,
            history: ctx.message.clone().map(|message| vec![message]),
            metadata: None,
        });

        Box::pin(stream::once(async move { Ok(response) }))
    }

    fn cancel(
        &self,
        ctx: a2a_server::ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let response = StreamResponse::Task(Task {
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
        });

        Box::pin(stream::once(async move { Ok(response) }))
    }
}

async fn spawn_grpc_server() -> (String, tokio::task::JoinHandle<()>) {
    let handler = Arc::new(TestHandler);
    let service = A2aServiceServer::new(GrpcHandler::new(handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = TcpListenerStream::new(listener);
    let handle = tokio::spawn(async move {
        Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), handle)
}

async fn spawn_default_handler_grpc_server() -> (String, tokio::task::JoinHandle<()>) {
    let handler = Arc::new(DefaultRequestHandler::new(
        StoredTaskExecutor,
        InMemoryTaskStore::new(),
    ));
    let service = A2aServiceServer::new(GrpcHandler::new(handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = TcpListenerStream::new(listener);
    let handle = tokio::spawn(async move {
        Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
            .unwrap();
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    (format!("http://{addr}"), handle)
}

fn endpoint_without_scheme(endpoint: &str) -> String {
    endpoint
        .split_once("://")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_else(|| endpoint.to_string())
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
async fn grpc_transport_end_to_end() {
    let (endpoint, handle) = spawn_grpc_server().await;
    let transport = GrpcTransport::connect(endpoint).await.unwrap();

    let send_resp = transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await;
    assert!(matches!(send_resp.unwrap(), SendMessageResponse::Task(_)));

    let task = transport
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "task-1".to_string(),
                history_length: Some(1),
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
                page_size: Some(1),
                page_token: Some("token-1".to_string()),
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
    assert_eq!(created.config.id.as_deref(), Some("cfg-1"));

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
                page_size: Some(10),
                page_token: Some("token-1".to_string()),
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

    let card = transport
        .get_extended_agent_card(
            &ServiceParams::new(),
            &GetExtendedAgentCardRequest { tenant: None },
        )
        .await
        .unwrap();
    assert_eq!(card.name, "gRPC Test Agent");

    transport.destroy().await.unwrap();
    handle.abort();
}

#[tokio::test]
async fn grpc_transport_streaming_and_error_paths() {
    let (endpoint, handle) = spawn_grpc_server().await;
    let transport = GrpcTransport::connect(endpoint.clone()).await.unwrap();

    let stream = transport
        .send_streaming_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();
    let events: Vec<_> = stream.collect().await;
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0].as_ref().unwrap(),
        StreamResponse::StatusUpdate(_)
    ));

    let subscribe_stream = transport
        .subscribe_to_task(
            &ServiceParams::new(),
            &SubscribeToTaskRequest {
                id: "task-1".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();
    let subscribe_events: Vec<_> = subscribe_stream.collect().await;
    assert_eq!(subscribe_events.len(), 1);

    let err = transport
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
    assert_eq!(err.code, error_code::TASK_NOT_FOUND);

    let err = transport
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
    assert_eq!(err.code, error_code::TASK_NOT_FOUND);

    let factory = GrpcTransportFactory;
    assert_eq!(factory.protocol(), TRANSPORT_PROTOCOL_GRPC);
    let transport = factory
        .create(
            &sample_agent_card(),
            &AgentInterface::new(endpoint_without_scheme(&endpoint), TRANSPORT_PROTOCOL_GRPC),
        )
        .await
        .unwrap();
    transport.destroy().await.unwrap();

    handle.abort();
}

#[tokio::test]
async fn grpc_transport_accepts_bare_host_port_endpoints() {
    let (endpoint, handle) = spawn_grpc_server().await;
    let bare_endpoint = endpoint_without_scheme(&endpoint);

    let transport = GrpcTransport::connect(bare_endpoint.clone()).await.unwrap();
    let task = transport
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "task-1".to_string(),
                history_length: Some(1),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(task.id, "task-1");

    let factory = GrpcTransportFactory;
    let transport = factory
        .create(
            &sample_agent_card(),
            &AgentInterface::new(bare_endpoint, TRANSPORT_PROTOCOL_GRPC),
        )
        .await
        .unwrap();
    let response = transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();
    assert!(matches!(response, SendMessageResponse::Task(_)));

    transport.destroy().await.unwrap();
    handle.abort();
}

#[tokio::test]
async fn grpc_transport_treats_zero_page_size_as_unset() {
    let (endpoint, handle) = spawn_default_handler_grpc_server().await;
    let transport = GrpcTransport::connect(endpoint).await.unwrap();

    let sent = transport
        .send_message(
            &ServiceParams::new(),
            &SendMessageRequest {
                message: Message::new(Role::User, vec![Part::text("hello")]),
                configuration: None,
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();

    let task = match sent {
        SendMessageResponse::Task(task) => task,
        SendMessageResponse::Message(_) => panic!("expected task response"),
    };

    let listed = transport
        .list_tasks(
            &ServiceParams::new(),
            &ListTasksRequest {
                context_id: Some(task.context_id.clone()),
                status: None,
                page_size: Some(0),
                page_token: None,
                history_length: None,
                status_timestamp_after: None,
                include_artifacts: Some(false),
                tenant: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(listed.tasks.len(), 1);
    assert_eq!(listed.tasks[0].id, task.id);
    assert_eq!(listed.page_size, 50);

    transport.destroy().await.unwrap();
    handle.abort();
}
