// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;
use std::time::Duration;

use a2a::event::{StreamResponse, TaskStatusUpdateEvent};
use a2a::*;
use a2a_client::Transport;
use a2a_client::transport::{ServiceParams, TransportFactory};
use a2a_pb::proto;
use a2a_server::RequestHandler;
use a2a_slimrpc::{
    SLIM_PEER_ISSUER_METADATA_KEY, SLIM_PEER_SUBJECT_METADATA_KEY, SLIM_SRC_METADATA_KEY, SlimApp,
    SlimRpcHandler, SlimRpcTransport, SlimRpcTransportFactory,
};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use prost::Message as _;
use slim_auth::auth_provider::{AuthProvider, AuthVerifier};
use slim_auth::shared_secret::SharedSecret;
use slim_config::component::id::{ID, Kind};
use slim_datapath::api::ProtoName as Name;
use slim_rpc::{Channel, RpcCode, RpcError, Server};
use slim_service::service::Service;

const A2A_SERVICE_NAME: &str = "lf.a2a.v1.A2AService";
const GET_TASK_METHOD: &str = "GetTask";
const SEND_MESSAGE_METHOD: &str = "SendMessage";
const SEND_STREAMING_MESSAGE_METHOD: &str = "SendStreamingMessage";
const COLLABORATE_SERVICE_NAME: &str =
    "experimental.slimrpc.collaborative_channel.v1.CollaborativeChannelService";
const COLLABORATE_METHOD: &str = "Collaborate";
const NO_PART_CONTENT: &str = "must have one of: text, raw, url, data";
const SHARED_SECRET: &str = "test-secret-with-sufficient-length-for-hmac-key";

fn sample_message(role: Role, text: &str, task_id: &str) -> Message {
    Message {
        message_id: format!("msg-{text}"),
        context_id: Some("ctx-1".to_string()),
        task_id: Some(task_id.to_string()),
        role,
        parts: vec![Part::text(text)],
        metadata: None,
        extensions: None,
        reference_task_ids: None,
    }
}

/// A message whose only part carries no content, which the native types
/// cannot represent.
fn empty_part_message() -> proto::Message {
    proto::Message {
        message_id: "msg-empty".to_string(),
        role: proto::Role::Agent as i32,
        parts: vec![proto::Part::default()],
        ..Default::default()
    }
}

fn empty_part_task() -> proto::Task {
    proto::Task {
        id: "task-1".to_string(),
        history: vec![empty_part_message()],
        ..Default::default()
    }
}

fn sample_task(id: &str, state: TaskState) -> Task {
    Task {
        id: id.to_string(),
        context_id: "ctx-1".to_string(),
        status: TaskStatus {
            state,
            message: Some(sample_message(Role::Agent, "done", id)),
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

fn sample_agent_card(target: &str) -> AgentCard {
    AgentCard {
        name: "SLIMRPC Test Agent".to_string(),
        description: "Integration test agent".to_string(),
        version: VERSION.to_string(),
        supported_interfaces: vec![AgentInterface::new(target, TRANSPORT_PROTOCOL_SLIMRPC)],
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

fn send_message_request(task_id: &str) -> SendMessageRequest {
    SendMessageRequest {
        message: sample_message(Role::User, "hello", task_id),
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

/// Build a shared-secret auth provider/verifier pair for the given identity id.
fn auth(id: &str) -> (AuthProvider, AuthVerifier) {
    let secret = SharedSecret::new(id, SHARED_SECRET).unwrap();
    (
        AuthProvider::shared_secret(secret.clone()),
        AuthVerifier::shared_secret(secret),
    )
}

struct TestEnv {
    test_name: String,
    service: Arc<Service>,
    server: Arc<Server>,
    _server_app: Arc<SlimApp>,
    server_name: Arc<Name>,
}

impl TestEnv {
    async fn new(test_name: &str) -> Self {
        let id = ID::new_with_name(Kind::new("slim").unwrap(), test_name).unwrap();
        let service = Arc::new(Service::new(id));

        let server_name = Name::from_strings(["org", "test", test_name]);
        let (provider, verifier) = auth("test-provider");
        let (server_app, server_notifications) = service
            .create_app(&server_name, provider, verifier)
            .unwrap();
        let server_app = Arc::new(server_app);
        let server = Arc::new(Server::new(
            server_app.clone(),
            server_app.app_name().clone(),
            server_notifications,
        ));

        Self {
            test_name: test_name.to_string(),
            service,
            server,
            _server_app: server_app,
            server_name: Arc::new(server_name),
        }
    }

    async fn start(&self) {
        let server = self.server.clone();
        tokio::spawn(async move {
            let _ = server.serve().await;
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    async fn shutdown(&self) {
        self.server.shutdown().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    async fn new_client_app(&self, suffix: &str) -> Arc<SlimApp> {
        let name = Name::from_strings([
            "org".to_string(),
            "test".to_string(),
            format!("{}-{suffix}", self.test_name),
        ]);
        let (provider, verifier) = auth("test-provider-client");
        let (app, _) = self.service.create_app(&name, provider, verifier).unwrap();
        Arc::new(app)
    }

    async fn transport(&self, suffix: &str) -> SlimRpcTransport {
        let client_app = self.new_client_app(suffix).await;
        SlimRpcTransport::new(client_app, self.server_name.clone())
    }

    async fn transport_with_connection(&self, suffix: &str) -> SlimRpcTransport {
        let client_app = self.new_client_app(suffix).await;
        SlimRpcTransport::new_with_connection(client_app, self.server_name.clone(), None)
    }

    async fn transport_from_channel(&self, suffix: &str) -> SlimRpcTransport {
        let client_app = self.new_client_app(suffix).await;
        let channel = Channel::new(client_app, self.server_name.clone());
        SlimRpcTransport::from_channel(channel)
    }

    async fn factory(&self, suffix: &str) -> SlimRpcTransportFactory {
        let client_app = self.new_client_app(suffix).await;
        SlimRpcTransportFactory::new(client_app)
    }

    async fn factory_with_connection(&self, suffix: &str) -> SlimRpcTransportFactory {
        let client_app = self.new_client_app(suffix).await;
        SlimRpcTransportFactory::new_with_connection(client_app, None)
    }

    fn target(&self) -> String {
        format!("org/test/{}", self.test_name)
    }
}

struct TestHandler {
    target: String,
}

#[async_trait]
impl RequestHandler for TestHandler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        if req.message.task_id.as_deref() == Some("metadata-required")
            && params.get("x-trace") != Some(&vec!["alpha, beta".to_string()])
        {
            return Err(A2AError::invalid_params("missing x-trace metadata"));
        }

        if req.message.task_id.as_deref() == Some("identity-required") {
            // The caller's identity comes from the session: its SLIM name and
            // the verified subject of its shared-secret token (id plus a
            // per-instance suffix), never the spoofed values it sent.
            let single = |key: &str| match params.get(key).map(Vec::as_slice) {
                Some([value]) => Some(value.clone()),
                _ => None,
            };
            let slim_src = single(SLIM_SRC_METADATA_KEY).unwrap_or_default();
            let subject = single(SLIM_PEER_SUBJECT_METADATA_KEY).unwrap_or_default();
            if !slim_src.starts_with(&format!("{}-direct/", self.target))
                || !subject.starts_with("test-provider-client")
                || params.contains_key(SLIM_PEER_ISSUER_METADATA_KEY)
            {
                return Err(A2AError::invalid_params(format!(
                    "unexpected caller identity: {params:?}"
                )));
            }
        }

        if req.message.task_id.as_deref() == Some("message-response") {
            return Ok(SendMessageResponse::Message(sample_message(
                Role::Agent,
                "pong",
                "message-response",
            )));
        }

        let task_id = req
            .message
            .task_id
            .clone()
            .unwrap_or_else(|| "task-1".to_string());

        Ok(SendMessageResponse::Task(sample_task(
            &task_id,
            TaskState::Completed,
        )))
    }

    async fn send_streaming_message(
        &self,
        _params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        if req.message.task_id.as_deref() == Some("stream-error") {
            return Ok(Box::pin(stream::iter(vec![
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
                Err(A2AError::task_not_found("stream-error")),
            ])));
        }

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
        Ok(sample_agent_card(&format!("slimrpc://{}", self.target)))
    }
}

#[tokio::test]
async fn slimrpc_transport_end_to_end() {
    let env = TestEnv::new("transport-success").await;
    let handler = Arc::new(TestHandler {
        target: env.target(),
    });
    SlimRpcHandler::new(handler).register(&env.server);
    env.start().await;

    let transport = env.transport("direct").await;
    let mut params = ServiceParams::new();
    params.insert(
        "x-trace".to_string(),
        vec!["alpha".to_string(), "beta".to_string()],
    );

    let send_resp = transport
        .send_message(&params, &send_message_request("metadata-required"))
        .await
        .unwrap();
    assert!(matches!(send_resp, SendMessageResponse::Task(_)));

    let spoofed = ServiceParams::from([
        (
            SLIM_SRC_METADATA_KEY.to_string(),
            vec!["org/evil/app".to_string()],
        ),
        (
            SLIM_PEER_SUBJECT_METADATA_KEY.to_string(),
            vec!["did:key:spoofed".to_string()],
        ),
        (
            SLIM_PEER_ISSUER_METADATA_KEY.to_string(),
            vec!["https://evil.example".to_string()],
        ),
    ]);
    let send_resp = transport
        .send_message(&spoofed, &send_message_request("identity-required"))
        .await
        .unwrap();
    assert!(matches!(send_resp, SendMessageResponse::Task(_)));

    let send_resp = transport
        .send_message(
            &ServiceParams::new(),
            &send_message_request("message-response"),
        )
        .await
        .unwrap();
    assert!(matches!(send_resp, SendMessageResponse::Message(_)));

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
        .create_push_config(&ServiceParams::new(), &sample_push_config("cfg-1"))
        .await
        .unwrap();
    assert_eq!(created.id.as_deref(), Some("cfg-1"));

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
    assert_eq!(fetched.id.as_deref(), Some("cfg-1"));

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
    assert_eq!(card.name, "SLIMRPC Test Agent");

    transport.destroy().await.unwrap();
    env.shutdown().await;
}

#[tokio::test]
async fn slimrpc_transport_streaming_and_factory_paths() {
    let env = TestEnv::new("transport-streams").await;
    let handler = Arc::new(TestHandler {
        target: env.target(),
    });
    SlimRpcHandler::new(handler).register(&env.server);
    env.start().await;

    let transport = env.transport_from_channel("channel").await;

    let stream = transport
        .send_streaming_message(&ServiceParams::new(), &send_message_request("task-1"))
        .await
        .unwrap();
    let events: Vec<_> = stream.collect().await;
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0].as_ref().unwrap(),
        StreamResponse::StatusUpdate(_)
    ));

    let stream = transport
        .send_streaming_message(&ServiceParams::new(), &send_message_request("stream-error"))
        .await
        .unwrap();
    let events: Vec<_> = stream.collect().await;
    assert_eq!(events.len(), 2);
    assert_eq!(
        events[1].as_ref().unwrap_err().code,
        error_code::TASK_NOT_FOUND
    );

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

    for error in [
        transport
            .get_task(
                &ServiceParams::new(),
                &GetTaskRequest {
                    id: "missing".to_string(),
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap_err(),
        transport
            .cancel_task(
                &ServiceParams::new(),
                &CancelTaskRequest {
                    id: "missing".to_string(),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap_err(),
        transport
            .create_push_config(
                &ServiceParams::new(),
                &TaskPushNotificationConfig {
                    task_id: "missing".to_string(),
                    ..sample_push_config("cfg-1")
                },
            )
            .await
            .unwrap_err(),
        transport
            .get_push_config(
                &ServiceParams::new(),
                &GetTaskPushNotificationConfigRequest {
                    task_id: "task-1".to_string(),
                    id: "missing".to_string(),
                    tenant: None,
                },
            )
            .await
            .unwrap_err(),
        transport
            .delete_push_config(
                &ServiceParams::new(),
                &DeleteTaskPushNotificationConfigRequest {
                    task_id: "task-1".to_string(),
                    id: "missing".to_string(),
                    tenant: None,
                },
            )
            .await
            .unwrap_err(),
    ] {
        assert_eq!(error.code, error_code::TASK_NOT_FOUND);
    }

    let subscribe_stream = transport
        .subscribe_to_task(
            &ServiceParams::new(),
            &SubscribeToTaskRequest {
                id: "missing".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();
    let subscribe_events: Vec<_> = subscribe_stream.collect().await;
    assert_eq!(subscribe_events.len(), 1);
    assert_eq!(
        subscribe_events[0].as_ref().unwrap_err().code,
        error_code::TASK_NOT_FOUND,
    );

    let factory = env.factory("factory").await;
    assert_eq!(factory.protocol(), TRANSPORT_PROTOCOL_SLIMRPC);
    let transport = factory
        .create(
            &sample_agent_card(&format!("slimrpc://{}", env.target())),
            &AgentInterface::new(
                format!("slim://{}", env.target()),
                TRANSPORT_PROTOCOL_SLIMRPC,
            ),
        )
        .await
        .unwrap();
    transport.destroy().await.unwrap();

    let factory = env.factory_with_connection("factory-connection").await;
    let transport = factory
        .create(
            &sample_agent_card(&env.target()),
            &AgentInterface::new(env.target(), TRANSPORT_PROTOCOL_SLIMRPC),
        )
        .await
        .unwrap();
    transport.destroy().await.unwrap();

    let error = factory
        .create(
            &sample_agent_card("bad-target"),
            &AgentInterface::new("bad-target", TRANSPORT_PROTOCOL_SLIMRPC),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, error_code::INVALID_PARAMS);

    env.shutdown().await;
}

#[tokio::test]
async fn slimrpc_transport_reports_malformed_payloads() {
    let env = TestEnv::new("transport-malformed").await;

    env.server.register_unary_unary(
        A2A_SERVICE_NAME,
        GET_TASK_METHOD,
        |_request: Vec<u8>, _context| async move { Ok(vec![0xff]) },
    );
    env.server.register_unary_unary(
        A2A_SERVICE_NAME,
        SEND_MESSAGE_METHOD,
        |_request: Vec<u8>, _context| async move {
            Ok(a2a_pb::proto::SendMessageResponse::default().encode_to_vec())
        },
    );
    env.server.register_unary_stream(
        A2A_SERVICE_NAME,
        SEND_STREAMING_MESSAGE_METHOD,
        |_request: Vec<u8>, _context| async move {
            Ok(stream::iter(vec![Ok::<Vec<u8>, RpcError>(vec![0xff])]))
        },
    );
    env.start().await;

    let transport = env.transport_with_connection("malformed").await;

    let error = transport
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "task-1".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, error_code::INTERNAL_ERROR);
    assert!(error.message.contains("invalid Task payload"));

    let error = transport
        .send_message(
            &ServiceParams::new(),
            &send_message_request("empty-response"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, error_code::INTERNAL_ERROR);
    assert!(
        error.message.contains("empty SendMessageResponse payload")
            || error.message.contains("No response received")
    );

    let stream = transport
        .send_streaming_message(&ServiceParams::new(), &send_message_request("empty-stream"))
        .await
        .unwrap();
    let events: Vec<_> = stream.collect().await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.code, error_code::INTERNAL_ERROR);
    assert!(error.message.contains("invalid StreamResponse payload"));

    env.shutdown().await;
}

/// `Part.content` is a required oneof and `message` a REQUIRED field in
/// a2a.proto: a request without either is INVALID_ARGUMENT naming the field,
/// on SendMessage and SendStreamingMessage alike.
#[tokio::test]
async fn slimrpc_server_rejects_a_missing_message_and_a_part_without_content() {
    let env = TestEnv::new("server-invalid-request").await;
    let handler = Arc::new(TestHandler {
        target: env.target(),
    });
    SlimRpcHandler::new(handler).register(&env.server);
    env.start().await;
    let channel = Channel::new(env.new_client_app("raw").await, env.server_name.clone());

    let cases = [
        (
            Some(empty_part_message()),
            format!("message.parts[0]: {NO_PART_CONTENT}"),
        ),
        (None, "message: is required".to_string()),
    ];
    for (message, expected) in cases {
        let request = proto::SendMessageRequest {
            message,
            ..Default::default()
        }
        .encode_to_vec();

        let error = channel
            .unary::<Vec<u8>, Vec<u8>>(
                A2A_SERVICE_NAME,
                SEND_MESSAGE_METHOD,
                request.clone(),
                None,
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code(), RpcCode::InvalidArgument, "{error:?}");
        assert!(error.message().contains(&expected), "{error:?}");

        let events = channel.unary_stream::<Vec<u8>, Vec<u8>>(
            A2A_SERVICE_NAME,
            SEND_STREAMING_MESSAGE_METHOD,
            request,
            None,
            None,
        );
        futures::pin_mut!(events);
        let error = events.next().await.expect("an error item").unwrap_err();
        assert_eq!(error.code(), RpcCode::InvalidArgument, "{error:?}");
        assert!(error.message().contains(&expected), "{error:?}");
    }

    env.shutdown().await;
}

/// A response that carries a part without content is the peer's fault: the
/// client reports INTERNAL_ERROR naming the part, as the JSON-RPC client does,
/// not INVALID_PARAMS, which would blame the caller's request.
#[tokio::test]
async fn slimrpc_transport_reports_a_part_without_content_in_a_response_as_internal() {
    let env = TestEnv::new("transport-empty-part").await;

    for method in [GET_TASK_METHOD, "CancelTask"] {
        env.server.register_unary_unary(
            A2A_SERVICE_NAME,
            method,
            |_request: Vec<u8>, _context| async move { Ok(empty_part_task().encode_to_vec()) },
        );
    }
    env.server.register_unary_unary(
        A2A_SERVICE_NAME,
        "ListTasks",
        |_request: Vec<u8>, _context| async move {
            Ok(proto::ListTasksResponse {
                tasks: vec![proto::Task::default(), empty_part_task()],
                ..Default::default()
            }
            .encode_to_vec())
        },
    );
    env.server.register_unary_unary(
        A2A_SERVICE_NAME,
        SEND_MESSAGE_METHOD,
        |_request: Vec<u8>, _context| async move {
            Ok(proto::SendMessageResponse {
                payload: Some(proto::send_message_response::Payload::Task(
                    empty_part_task(),
                )),
            }
            .encode_to_vec())
        },
    );
    for method in [SEND_STREAMING_MESSAGE_METHOD, "SubscribeToTask"] {
        env.server.register_unary_stream(
            A2A_SERVICE_NAME,
            method,
            |_request: Vec<u8>, _context| async move {
                let event = proto::StreamResponse {
                    payload: Some(proto::stream_response::Payload::Task(empty_part_task())),
                };
                Ok(stream::iter(vec![Ok::<Vec<u8>, RpcError>(
                    event.encode_to_vec(),
                )]))
            },
        );
    }
    env.start().await;

    let transport = env.transport_with_connection("empty-part").await;
    let params = ServiceParams::new();
    let in_task = format!("history[0].parts[0]: {NO_PART_CONTENT}");
    let internal = |error: A2AError, path: &str| {
        assert_eq!(error.code, error_code::INTERNAL_ERROR, "{error:?}");
        assert_eq!(error.message, format!("invalid response: {path}{in_task}"));
    };

    internal(
        transport
            .send_message(&params, &send_message_request("empty-part"))
            .await
            .unwrap_err(),
        "task.",
    );
    internal(
        transport
            .get_task(
                &params,
                &GetTaskRequest {
                    id: "task-1".to_string(),
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap_err(),
        "",
    );
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
        .send_streaming_message(&params, &send_message_request("empty-part"))
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
    for events in [sent, subscribed] {
        let events: Vec<_> = events.collect().await;
        assert_eq!(events.len(), 1);
        internal(events.into_iter().next().unwrap().unwrap_err(), "task.");
    }

    env.shutdown().await;
}

/// Group `Collaborate`: every member independently broadcasts one intro `Message`
/// to the others over its own group channel, and every other member's listen-only
/// `register_collaborate` handler receives it — proving the SLIMRPC collaborative
/// channel extension's many-to-many delivery end to end (not just moderator-to-
/// members fan-out).
#[tokio::test]
async fn slimrpc_collaborate_broadcasts_to_every_other_member() {
    use std::sync::Mutex;

    const MEMBERS: [&str; 3] = ["avatar", "agent-a", "agent-b"];

    let service = Arc::new(Service::new(
        ID::new_with_name(Kind::new("slim").unwrap(), "collab-group").unwrap(),
    ));

    struct Member {
        name: Arc<Name>,
        app: Arc<SlimApp>,
        received: Arc<Mutex<Vec<Message>>>,
    }

    let mut members = Vec::new();
    for suffix in MEMBERS {
        let name = Arc::new(Name::from_strings([
            "org",
            "test",
            &format!("collab-{suffix}"),
        ]));
        let (provider, verifier) = auth(&format!("collab-provider-{suffix}"));
        let (app, notifications) = service.create_app(&name, provider, verifier).unwrap();
        let app = Arc::new(app);

        let server = Arc::new(Server::new(
            app.clone(),
            app.app_name().clone(),
            notifications,
        ));
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_for_handler = received.clone();
        a2a_slimrpc::register_collaborate(&server, move |message| {
            received_for_handler.lock().unwrap().push(message);
        });
        tokio::spawn(async move {
            let _ = server.serve().await;
        });

        members.push(Member {
            name,
            app,
            received,
        });
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Every member independently broadcasts its own intro to the other two.
    let mut senders = Vec::new();
    for (i, member) in members.iter().enumerate() {
        let others: Vec<Arc<Name>> = members
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, m)| m.name.clone())
            .collect();
        let transport = SlimRpcTransport::new_group(member.app.clone(), others).unwrap();
        let intro = sample_message(
            Role::Agent,
            &format!("hi, I am {}", MEMBERS[i]),
            "collaborate",
        );
        let outbound = stream::once(async move { intro });
        senders.push(tokio::spawn(async move {
            let replies = transport.collaborate(outbound, Some(Duration::from_secs(5)));
            futures::pin_mut!(replies);
            // Everyone else is listen-only, so this drains to nothing; the point
            // is proving the broadcast send doesn't error.
            while let Some(reply) = replies.next().await {
                reply.expect("collaborate reply stream should not error");
            }
        }));
    }
    for sender in senders {
        sender.await.expect("collaborate sender task panicked");
    }

    tokio::time::sleep(Duration::from_millis(300)).await;

    for (i, member) in members.iter().enumerate() {
        let received = member.received.lock().unwrap();
        for (j, other) in MEMBERS.iter().enumerate() {
            if i == j {
                continue;
            }
            let expected = format!("hi, I am {other}");
            assert!(
                received.iter().any(|m| m.text() == Some(expected.as_str())),
                "{} did not receive {other}'s intro; got {:?}",
                MEMBERS[i],
                received.iter().map(|m| m.text()).collect::<Vec<_>>()
            );
        }
    }
}

/// A reply on a `Collaborate` session comes back to the sender as a `Message`
/// attributed to the member that sent it, and a reply whose part carries no
/// content as INTERNAL_ERROR: the member sent it, not the caller.
#[tokio::test]
async fn slimrpc_collaborate_replies_are_attributed_to_their_sender() {
    let service = Arc::new(Service::new(
        ID::new_with_name(Kind::new("slim").unwrap(), "collab-echo").unwrap(),
    ));
    let mut apps = Vec::new();
    for suffix in ["echo", "asker"] {
        let name = Arc::new(Name::from_strings([
            "org",
            "test",
            &format!("collab-{suffix}"),
        ]));
        let (provider, verifier) = auth(&format!("collab-provider-{suffix}"));
        let (app, notifications) = service.create_app(&name, provider, verifier).unwrap();
        apps.push((name, Arc::new(app), notifications));
    }
    let (echo_name, echo_app, notifications) = apps.remove(0);
    let (_, asker_app, _) = apps.remove(0);

    // The echo member answers every broadcast with the same bytes, then with
    // a message whose part carries no content.
    let server = Arc::new(Server::new(
        echo_app.clone(),
        echo_app.app_name().clone(),
        notifications,
    ));
    server.register_stream_stream::<_, Vec<u8>, Vec<u8>, _, _>(
        COLLABORATE_SERVICE_NAME,
        COLLABORATE_METHOD,
        |inbound: slim_rpc::DecodedStream<Vec<u8>>, _context: slim_rpc::Context| async move {
            Ok(inbound
                .flat_map(|echo| stream::iter([echo, Ok(empty_part_message().encode_to_vec())])))
        },
    );
    tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let transport = SlimRpcTransport::new_group(asker_app, vec![echo_name.clone()]).unwrap();
    let outbound = stream::once(async { sample_message(Role::Agent, "hello", "collaborate") });
    let replies = transport.collaborate(outbound, Some(Duration::from_secs(5)));
    futures::pin_mut!(replies);
    let reply = replies.next().await.expect("one reply").unwrap();
    assert_eq!(reply.text(), Some("hello"));
    let source = reply.metadata.as_ref().unwrap()[SLIM_SRC_METADATA_KEY]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        source.starts_with("org/test/collab-echo/"),
        "{source} is not the echo member"
    );

    let error = replies.next().await.expect("a second reply").unwrap_err();
    assert_eq!(error.code, error_code::INTERNAL_ERROR);
    assert_eq!(
        error.message,
        format!("invalid response: parts[0]: {NO_PART_CONTENT}")
    );
}

/// A listen-only member drops a broadcast whose part carries no content and
/// keeps receiving the ones after it.
#[tokio::test]
async fn slimrpc_collaborate_drops_a_message_with_a_part_without_content() {
    use std::sync::Mutex;

    let service = Arc::new(Service::new(
        ID::new_with_name(Kind::new("slim").unwrap(), "collab-invalid").unwrap(),
    ));
    let mut apps = Vec::new();
    for suffix in ["listener", "sender"] {
        let name = Arc::new(Name::from_strings([
            "org",
            "test",
            &format!("collab-invalid-{suffix}"),
        ]));
        let (provider, verifier) = auth(&format!("collab-invalid-provider-{suffix}"));
        let (app, notifications) = service.create_app(&name, provider, verifier).unwrap();
        apps.push((name, Arc::new(app), notifications));
    }
    let (listener_name, listener_app, notifications) = apps.remove(0);
    let (_, sender_app, _) = apps.remove(0);

    let server = Arc::new(Server::new(
        listener_app.clone(),
        listener_app.app_name().clone(),
        notifications,
    ));
    let received = Arc::new(Mutex::new(Vec::new()));
    let sink = received.clone();
    a2a_slimrpc::register_collaborate(&server, move |message| {
        sink.lock().unwrap().push(message);
    });
    tokio::spawn(async move {
        let _ = server.serve().await;
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    let channel = Channel::new_group(sender_app, vec![listener_name]).unwrap();
    let valid =
        a2a_pb::pbconv::to_proto_message(&sample_message(Role::Agent, "after", "collaborate"));
    let outbound = stream::iter([empty_part_message().encode_to_vec(), valid.encode_to_vec()]);
    let replies = channel.multicast_stream_stream::<Vec<u8>, Vec<u8>>(
        COLLABORATE_SERVICE_NAME,
        COLLABORATE_METHOD,
        outbound,
        Some(Duration::from_secs(5)),
        None,
    );
    futures::pin_mut!(replies);
    while replies.next().await.is_some() {}
    tokio::time::sleep(Duration::from_millis(300)).await;

    let received = received.lock().unwrap();
    assert_eq!(
        received.iter().map(|m| m.text()).collect::<Vec<_>>(),
        [Some("after")]
    );
}
