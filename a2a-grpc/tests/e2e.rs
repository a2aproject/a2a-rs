// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;
use std::time::Duration;

use a2a::event::{StreamResponse, TaskStatusUpdateEvent};
use a2a::*;
use a2a_client::{Transport, TransportFactory};
use a2a_grpc::{GrpcHandler, GrpcTransport, GrpcTransportFactory};
use a2a_pb::proto;
use a2a_pb::proto::a2a_service_client::A2aServiceClient;
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
        url: "https://example.com/callback".to_string(),
        id: Some(id.to_string()),
        token: Some("tok-1".to_string()),
        authentication: None,
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
        if req.id == "invalid" {
            return Err(
                A2AError::invalid_params("invalid task id").with_details(vec![
                    TypedDetail::bad_request(vec![FieldViolation {
                        field: "id".to_string(),
                        description: "must be a UUID".to_string(),
                    }]),
                ]),
            );
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
        req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        if req.task_id == "missing" {
            return Err(A2AError::task_not_found(&req.task_id));
        }
        Ok(req)
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
            url: "https://example.com/callback".to_string(),
            id: Some(req.id),
            token: Some("tok-1".to_string()),
            authentication: None,
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

/// A2A-Version a real client sends (§3.6.1): the server rejects a request
/// without one (§3.6.2).
fn versioned_params() -> ServiceParams {
    ServiceParams::from([(SVC_PARAM_VERSION.to_string(), vec![VERSION.to_string()])])
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
        .send_message(&versioned_params(), &send_message_request())
        .await;
    assert!(matches!(send_resp.unwrap(), SendMessageResponse::Task(_)));

    let task = transport
        .get_task(
            &versioned_params(),
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
            &versioned_params(),
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
            &versioned_params(),
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
        .create_push_config(&versioned_params(), &sample_push_config("cfg-1"))
        .await
        .unwrap();
    assert_eq!(created.id.as_deref(), Some("cfg-1"));

    let fetched = transport
        .get_push_config(
            &versioned_params(),
            &GetTaskPushNotificationConfigRequest {
                task_id: "task-1".to_string(),
                id: "cfg-1".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(fetched.id.as_deref(), Some("cfg-1"));

    let configs = transport
        .list_push_configs(
            &versioned_params(),
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
            &versioned_params(),
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
            &versioned_params(),
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
        .send_streaming_message(&versioned_params(), &send_message_request())
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
            &versioned_params(),
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
            &versioned_params(),
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
            &versioned_params(),
            &DeleteTaskPushNotificationConfigRequest {
                task_id: "task-1".to_string(),
                id: "missing".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::TASK_NOT_FOUND);

    let factory = GrpcTransportFactory::new();
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
            &versioned_params(),
            &GetTaskRequest {
                id: "task-1".to_string(),
                history_length: Some(1),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(task.id, "task-1");

    let factory = GrpcTransportFactory::new();
    let transport = factory
        .create(
            &sample_agent_card(),
            &AgentInterface::new(bare_endpoint, TRANSPORT_PROTOCOL_GRPC),
        )
        .await
        .unwrap();
    let response = transport
        .send_message(&versioned_params(), &send_message_request())
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
            &versioned_params(),
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
            &versioned_params(),
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

#[tokio::test]
async fn grpc_transport_rejects_unsupported_a2a_version() {
    let (endpoint, handle) = spawn_grpc_server().await;
    let transport = GrpcTransport::connect(endpoint).await.unwrap();

    // §3.6.2: 99.0 is unsupported, and an absent version is "0.3".
    let unsupported =
        ServiceParams::from([(SVC_PARAM_VERSION.to_string(), vec!["99.0".to_string()])]);
    for params in [unsupported, ServiceParams::new()] {
        let err = transport
            .send_message(&params, &send_message_request())
            .await
            .unwrap_err();
        assert_eq!(err.code, error_code::VERSION_NOT_SUPPORTED, "{params:?}");

        let err = match transport
            .send_streaming_message(&params, &send_message_request())
            .await
        {
            Ok(_) => panic!("streaming accepted {params:?}"),
            Err(err) => err,
        };
        assert_eq!(err.code, error_code::VERSION_NOT_SUPPORTED, "{params:?}");
    }

    transport.destroy().await.unwrap();
    handle.abort();
}

#[tokio::test]
async fn grpc_transport_decodes_error_details_from_a_real_server() {
    let (endpoint, handle) = spawn_grpc_server().await;
    let transport = GrpcTransport::connect(endpoint).await.unwrap();

    let err = transport
        .get_task(
            &versioned_params(),
            &GetTaskRequest {
                id: "invalid".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .unwrap_err();

    assert_eq!(err.code, error_code::INVALID_PARAMS);
    assert_eq!(err.message, "invalid task id: id: must be a UUID");
    let details = err.details.expect("details should reach the client");
    assert_eq!(details.len(), 2);
    assert_eq!(details[0].type_url, errordetails::ERROR_INFO_TYPE);
    assert_eq!(details[0].value["reason"], "INVALID_PARAMS");
    assert_eq!(
        details[1],
        TypedDetail::bad_request(vec![FieldViolation {
            field: "id".to_string(),
            description: "must be a UUID".to_string(),
        }])
    );

    transport.destroy().await.unwrap();
    handle.abort();
}

/// `Part.content` is a required oneof and `message` a REQUIRED field in
/// a2a.proto: a request without either is INVALID_ARGUMENT naming the field,
/// as a2a-go answers it, not an empty text part or an empty message.
#[tokio::test]
async fn grpc_server_rejects_a_missing_message_and_a_part_without_content() {
    let (endpoint, handle) = spawn_grpc_server().await;
    let mut client = A2aServiceClient::connect(endpoint).await.unwrap();

    let cases = [
        (
            Some(proto::Message {
                message_id: "msg-empty".to_string(),
                role: proto::Role::User as i32,
                parts: vec![proto::Part::default()],
                ..Default::default()
            }),
            "message.parts[0]: must have one of: text, raw, url, data",
        ),
        (None, "message: is required"),
    ];
    for (message, expected) in cases {
        let request = || {
            let mut request = tonic::Request::new(proto::SendMessageRequest {
                message: message.clone(),
                ..Default::default()
            });
            request
                .metadata_mut()
                .insert("a2a-version", VERSION.parse().unwrap());
            request
        };
        let status = client.send_message(request()).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{status:?}");
        assert_eq!(status.message(), expected);
        let status = client.send_streaming_message(request()).await.unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{status:?}");
        assert_eq!(status.message(), expected);
    }

    handle.abort();
}

/// A peer whose every response carries a part without content, which the
/// native types cannot represent.
struct EmptyPartService;

fn empty_part_task() -> proto::Task {
    proto::Task {
        id: "task-1".to_string(),
        history: vec![proto::Message {
            message_id: "msg-empty".to_string(),
            role: proto::Role::Agent as i32,
            parts: vec![proto::Part::default()],
            ..Default::default()
        }],
        ..Default::default()
    }
}

type EmptyPartStream = BoxStream<'static, Result<proto::StreamResponse, tonic::Status>>;

fn empty_part_stream() -> tonic::Response<EmptyPartStream> {
    let event = proto::StreamResponse {
        payload: Some(proto::stream_response::Payload::Task(empty_part_task())),
    };
    tonic::Response::new(Box::pin(stream::once(async move { Ok(event) })))
}

#[tonic::async_trait]
impl proto::a2a_service_server::A2aService for EmptyPartService {
    async fn send_message(
        &self,
        _: tonic::Request<proto::SendMessageRequest>,
    ) -> Result<tonic::Response<proto::SendMessageResponse>, tonic::Status> {
        Ok(tonic::Response::new(proto::SendMessageResponse {
            payload: Some(proto::send_message_response::Payload::Task(
                empty_part_task(),
            )),
        }))
    }

    type SendStreamingMessageStream = EmptyPartStream;

    async fn send_streaming_message(
        &self,
        _: tonic::Request<proto::SendMessageRequest>,
    ) -> Result<tonic::Response<EmptyPartStream>, tonic::Status> {
        Ok(empty_part_stream())
    }

    async fn get_task(
        &self,
        _: tonic::Request<proto::GetTaskRequest>,
    ) -> Result<tonic::Response<proto::Task>, tonic::Status> {
        Ok(tonic::Response::new(empty_part_task()))
    }

    async fn list_tasks(
        &self,
        _: tonic::Request<proto::ListTasksRequest>,
    ) -> Result<tonic::Response<proto::ListTasksResponse>, tonic::Status> {
        Ok(tonic::Response::new(proto::ListTasksResponse {
            tasks: vec![proto::Task::default(), empty_part_task()],
            ..Default::default()
        }))
    }

    async fn cancel_task(
        &self,
        _: tonic::Request<proto::CancelTaskRequest>,
    ) -> Result<tonic::Response<proto::Task>, tonic::Status> {
        Ok(tonic::Response::new(empty_part_task()))
    }

    type SubscribeToTaskStream = EmptyPartStream;

    async fn subscribe_to_task(
        &self,
        _: tonic::Request<proto::SubscribeToTaskRequest>,
    ) -> Result<tonic::Response<EmptyPartStream>, tonic::Status> {
        Ok(empty_part_stream())
    }

    async fn create_task_push_notification_config(
        &self,
        _: tonic::Request<proto::TaskPushNotificationConfig>,
    ) -> Result<tonic::Response<proto::TaskPushNotificationConfig>, tonic::Status> {
        Err(tonic::Status::unimplemented("not used"))
    }

    async fn get_task_push_notification_config(
        &self,
        _: tonic::Request<proto::GetTaskPushNotificationConfigRequest>,
    ) -> Result<tonic::Response<proto::TaskPushNotificationConfig>, tonic::Status> {
        Err(tonic::Status::unimplemented("not used"))
    }

    async fn list_task_push_notification_configs(
        &self,
        _: tonic::Request<proto::ListTaskPushNotificationConfigsRequest>,
    ) -> Result<tonic::Response<proto::ListTaskPushNotificationConfigsResponse>, tonic::Status>
    {
        Err(tonic::Status::unimplemented("not used"))
    }

    async fn get_extended_agent_card(
        &self,
        _: tonic::Request<proto::GetExtendedAgentCardRequest>,
    ) -> Result<tonic::Response<proto::AgentCard>, tonic::Status> {
        Err(tonic::Status::unimplemented("not used"))
    }

    async fn delete_task_push_notification_config(
        &self,
        _: tonic::Request<proto::DeleteTaskPushNotificationConfigRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        Err(tonic::Status::unimplemented("not used"))
    }
}

/// A response that carries a part without content is the peer's fault: the
/// client reports INTERNAL_ERROR naming the part, as the JSON-RPC client does,
/// not INVALID_PARAMS, which would blame the caller's request.
#[tokio::test]
async fn grpc_transport_reports_a_part_without_content_in_a_response_as_internal() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let handle = tokio::spawn(
        Server::builder()
            .add_service(A2aServiceServer::new(EmptyPartService))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    tokio::time::sleep(Duration::from_millis(20)).await;
    let transport = GrpcTransport::connect(endpoint).await.unwrap();
    let params = versioned_params();
    let get = GetTaskRequest {
        id: "task-1".to_string(),
        history_length: None,
        tenant: None,
    };
    let in_task = "history[0].parts[0]: must have one of: text, raw, url, data";
    let internal = |error: A2AError, path: &str| {
        assert_eq!(error.code, error_code::INTERNAL_ERROR, "{error:?}");
        assert_eq!(error.message, format!("invalid response: {path}{in_task}"));
    };

    internal(
        transport
            .send_message(&params, &send_message_request())
            .await
            .unwrap_err(),
        "task.",
    );
    internal(transport.get_task(&params, &get).await.unwrap_err(), "");
    internal(
        transport
            .list_tasks(
                &params,
                &ListTasksRequest {
                    context_id: None,
                    status: None,
                    page_size: None,
                    page_token: None,
                    history_length: None,
                    status_timestamp_after: None,
                    include_artifacts: None,
                    tenant: None,
                },
            )
            .await
            .unwrap_err(),
        "tasks[1].",
    );
    internal(
        transport
            .cancel_task(
                &params,
                &CancelTaskRequest {
                    id: "task-1".to_string(),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap_err(),
        "",
    );
    let sent = transport
        .send_streaming_message(&params, &send_message_request())
        .await
        .unwrap();
    let subscribed = transport
        .subscribe_to_task(
            &params,
            &SubscribeToTaskRequest {
                id: "task-1".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();
    for stream in [sent, subscribed] {
        let events: Vec<_> = stream.collect().await;
        assert_eq!(events.len(), 1);
        internal(events.into_iter().next().unwrap().unwrap_err(), "task.");
    }

    transport.destroy().await.unwrap();
    handle.abort();
}
