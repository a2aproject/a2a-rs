// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;
use std::time::Duration;

use a2a::event::{StreamResponse, TaskStatusUpdateEvent};
use a2a::*;
use a2a_client::Transport;
use a2a_client::transport::{ServiceParams, TransportFactory};
use a2a_server::RequestHandler;
use a2a_slimrpc::{SlimRpcHandler, SlimRpcTransport, SlimRpcTransportFactory};
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use prost::Message as _;
use slim_bindings::{
    App, Channel, Direction, IdentityProviderConfig, IdentityVerifierConfig, Name, RpcError,
    Server, initialize_with_defaults,
};

const A2A_SERVICE_NAME: &str = "lf.a2a.v1.A2AService";
const GET_TASK_METHOD: &str = "GetTask";
const SEND_MESSAGE_METHOD: &str = "SendMessage";
const SEND_STREAMING_MESSAGE_METHOD: &str = "SendStreamingMessage";
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
        config: PushNotificationConfig {
            url: "https://example.com/callback".to_string(),
            id: Some(id.to_string()),
            token: Some("tok-1".to_string()),
            authentication: None,
        },
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

fn provider_config(id: &str) -> IdentityProviderConfig {
    IdentityProviderConfig::SharedSecret {
        id: id.to_string(),
        data: SHARED_SECRET.to_string(),
    }
}

fn verifier_config(id: &str) -> IdentityVerifierConfig {
    IdentityVerifierConfig::SharedSecret {
        id: id.to_string(),
        data: SHARED_SECRET.to_string(),
    }
}

struct TestEnv {
    test_name: String,
    server: Arc<Server>,
    _server_app: Arc<App>,
    server_name: Arc<Name>,
}

impl TestEnv {
    async fn new(test_name: &str) -> Self {
        initialize_with_defaults();

        let server_name = Arc::new(Name::new(
            "org".to_string(),
            "test".to_string(),
            test_name.to_string(),
        ));
        let server_app = App::new_with_direction_async(
            server_name.clone(),
            provider_config("test-provider"),
            verifier_config("test-verifier"),
            Direction::Bidirectional,
        )
        .await
        .unwrap();
        let server = Arc::new(Server::new(&server_app, server_app.name().clone()));

        Self {
            test_name: test_name.to_string(),
            server,
            _server_app: server_app,
            server_name,
        }
    }

    async fn start(&self) {
        let server = self.server.clone();
        tokio::spawn(async move {
            let _ = server.serve_async().await;
        });
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    async fn shutdown(&self) {
        self.server.shutdown_async().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    async fn new_client_app(&self, suffix: &str) -> Arc<App> {
        let name = Arc::new(Name::new(
            "org".to_string(),
            "test".to_string(),
            format!("{}-{suffix}", self.test_name),
        ));

        App::new_with_direction_async(
            name,
            provider_config("test-provider-client"),
            verifier_config("test-verifier-client"),
            Direction::Bidirectional,
        )
        .await
        .unwrap()
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
                &CreateTaskPushNotificationConfigRequest {
                    task_id: "missing".to_string(),
                    config: sample_push_config("cfg-1").config,
                    tenant: None,
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

    env.server.register_unary_unary_internal(
        A2A_SERVICE_NAME,
        GET_TASK_METHOD,
        |_request: Vec<u8>, _context| async move { Ok(vec![0xff]) },
    );
    env.server.register_unary_unary_internal(
        A2A_SERVICE_NAME,
        SEND_MESSAGE_METHOD,
        |_request: Vec<u8>, _context| async move {
            Ok(a2a_pb::proto::SendMessageResponse::default().encode_to_vec())
        },
    );
    env.server.register_unary_stream_internal(
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
