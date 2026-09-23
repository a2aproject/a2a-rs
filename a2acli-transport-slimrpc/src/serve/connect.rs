// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! SLIM gateway connection and gRPC-proxy serve loop.
//!
//! `TransportHandler`'s dispatch and token-gating logic is generic over the
//! `Transport` it wraps, so it's unit-tested here against a fake transport
//! (see the `tests` module below) without needing a live SLIM connection.
//! `run()` itself -- the actual SLIM gateway connect, TLS/TCP bind, and
//! serve loop -- has no fake-able seam and is instead exercised end-to-end
//! via the itk/csit integration suites.

use std::io::{self, BufRead};
use std::net::SocketAddr;
use std::sync::Arc;

use a2a::*;
use a2a_client::Transport;
use a2a_client::transport::ServiceParams;
use a2a_grpc::GrpcHandler;
use a2a_pb::proto::a2a_service_server::A2aServiceServer;
use a2a_server::RequestHandler;
use a2a_slimrpc::{SlimRpcTransport, parse_slimrpc_target};
use async_trait::async_trait;
use futures::stream::BoxStream;
use slim_service::service::{Service, ServiceBuilder};
use tonic::transport::server::TcpIncoming;
use tonic_tls::rustls::TlsIncoming;
use uuid::Uuid;

use crate::error::PluginError;
use crate::tls::generate_loopback_tls;

use super::{
    EndpointPayload, Handshake, PluginConfig, check_token, forward_params, load_config,
    parse_proto_name, write_handshake,
};

// ── Transport → RequestHandler adapter ────────────────────────────────────────

/// Adapts a client [`Transport`] into a server [`RequestHandler`], forwarding
/// all calls while enforcing the per-launch plugin token.
///
/// Generic over `T` (rather than naming `SlimRpcTransport` directly) so the
/// dispatch/token-gating logic below is unit-testable against a fake
/// transport, without requiring a live SLIM connection.
struct TransportHandler<T: Transport + 'static> {
    transport: T,
    token: String,
}

impl<T: Transport + 'static> TransportHandler<T> {
    fn new(transport: T, token: String) -> Self {
        Self { transport, token }
    }
}

#[async_trait]
impl<T: Transport + 'static> RequestHandler for TransportHandler<T> {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        check_token(&self.token, params)?;
        self.transport
            .send_message(&forward_params(params), &req)
            .await
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        check_token(&self.token, params)?;
        self.transport
            .send_streaming_message(&forward_params(params), &req)
            .await
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        check_token(&self.token, params)?;
        self.transport.get_task(&forward_params(params), &req).await
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        check_token(&self.token, params)?;
        self.transport
            .list_tasks(&forward_params(params), &req)
            .await
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        check_token(&self.token, params)?;
        self.transport
            .cancel_task(&forward_params(params), &req)
            .await
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        check_token(&self.token, params)?;
        self.transport
            .subscribe_to_task(&forward_params(params), &req)
            .await
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        check_token(&self.token, params)?;
        self.transport
            .create_push_config(&forward_params(params), &req)
            .await
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        check_token(&self.token, params)?;
        self.transport
            .get_push_config(&forward_params(params), &req)
            .await
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        check_token(&self.token, params)?;
        self.transport
            .list_push_configs(&forward_params(params), &req)
            .await
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        check_token(&self.token, params)?;
        self.transport
            .delete_push_config(&forward_params(params), &req)
            .await
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        check_token(&self.token, params)?;
        self.transport
            .get_extended_agent_card(&forward_params(params), &req)
            .await
    }
}

// ── Serve entry point ──────────────────────────────────────────────────────────

pub async fn run(endpoint: &str) -> Result<(), PluginError> {
    // 1. Load config
    let config_path =
        std::env::var("A2A_SLIMRPC_PLUGIN_CONFIG").map_err(|_| PluginError::ConfigEnvMissing)?;
    let config = load_config(&config_path)?;
    run_with_config(endpoint, config).await
}

/// The bulk of `run`, taking an already-loaded config directly. Split out so
/// tests can exercise it without touching the process-global
/// `A2A_SLIMRPC_PLUGIN_CONFIG` env var (see `load_config`'s own doc comment).
async fn run_with_config(endpoint: &str, config: PluginConfig) -> Result<(), PluginError> {
    // 2. Parse SLIMRPC remote target
    let remote = parse_slimrpc_target(endpoint)
        .map_err(|e| PluginError::InvalidEndpoint(format!("{endpoint}: {}", e.message)))?;

    // 3. Build auth provider + verifier from app config
    let provider = config.app.identity_provider.build_auth_provider()?;
    let verifier = config.app.identity_verifier.build_auth_verifier()?;

    // 4. Build SLIM Service + connect to gateway
    let kind = ServiceBuilder::kind();
    let id = slim_config::component::id::ID::new_with_name(kind, &config.app.name)
        .map_err(|e| PluginError::Slim(format!("invalid service ID: {e}")))?;
    let service = Service::new(id);

    let conn_id = service
        .connect(&config.client)
        .await
        .map_err(|e| PluginError::Slim(format!("SLIM gateway connect failed: {e}")))?;

    // 5. Build SlimApp + transport
    // Parse the app name (org/namespace/agent) into SLIM's ProtoName components.
    let app_name = parse_proto_name(&config.app.name)?;
    let (slim_app, _notifications) = service
        .create_app(&app_name, provider, verifier)
        .map_err(|e| PluginError::Slim(format!("create_app failed: {e}")))?;
    let slim_app = Arc::new(slim_app);

    let transport = SlimRpcTransport::new_with_connection(slim_app, remote, Some(conn_id));

    // 6. Generate per-launch token + TLS cert
    let token = Uuid::new_v4().to_string();
    let tls = generate_loopback_tls()?;

    // 7. Bind loopback TCP + wrap in TLS
    // Bind on :0 to get a random port, then record the assigned address.
    let addr: SocketAddr = "127.0.0.1:0".parse().expect("valid addr");
    let std_listener = std::net::TcpListener::bind(addr).map_err(PluginError::Bind)?;
    let local_addr = std_listener.local_addr().map_err(PluginError::Bind)?;
    std_listener
        .set_nonblocking(true)
        .map_err(PluginError::Bind)?;
    let tokio_listener =
        tokio::net::TcpListener::from_std(std_listener).map_err(PluginError::Bind)?;

    let tcp_incoming = TcpIncoming::from(tokio_listener);
    let tls_incoming = TlsIncoming::new(tcp_incoming, tls.server_config);

    // 8. Build gRPC service
    let handler = Arc::new(TransportHandler::new(transport, token.clone()));
    let grpc_service = A2aServiceServer::new(GrpcHandler::new(handler));

    // 9. Print handshake
    let hs = Handshake {
        success: true,
        error: None,
        endpoint: Some(EndpointPayload {
            address: local_addr.to_string(),
            binding: TRANSPORT_PROTOCOL_GRPC,
            protocol: VERSION,
            token,
            cert_pem: tls.cert_pem,
        }),
    };
    write_handshake(&hs)?;

    // 10. Serve until stdin closes (CLI parent exit signal)
    let serve_fut = tonic::transport::Server::builder()
        .add_service(grpc_service)
        .serve_with_incoming(tls_incoming);

    tokio::select! {
        result = serve_fut => {
            if let Err(e) = result {
                tracing::error!(error = %e, "gRPC proxy server exited with error");
            }
        }
        _ = wait_stdin_close() => {
            tracing::debug!("stdin closed, shutting down");
        }
    }

    Ok(())
}

async fn wait_stdin_close() {
    tokio::task::spawn_blocking(|| {
        let stdin = io::stdin();
        let mut reader = stdin.lock();
        let mut buf = Vec::new();
        let _ = reader.read_until(0, &mut buf);
    })
    .await
    .ok();
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use futures::stream;
    use futures::stream::StreamExt;

    use super::super::TOKEN_HEADER;
    use super::*;

    /// A [`Transport`] test double: records the params of the last call it
    /// received (after `TransportHandler` has already applied `check_token`
    /// and `forward_params`) and returns canned responses. Lets the
    /// dispatch/token-gating logic in `TransportHandler` be exercised without
    /// a live SLIM connection.
    #[derive(Default)]
    struct FakeTransport {
        last_params: Mutex<Option<ServiceParams>>,
    }

    impl FakeTransport {
        fn last_params(&self) -> Option<ServiceParams> {
            self.last_params.lock().unwrap().clone()
        }

        fn record(&self, params: &ServiceParams) {
            *self.last_params.lock().unwrap() = Some(params.clone());
        }
    }

    fn fake_task() -> Task {
        Task {
            id: "task-1".into(),
            context_id: "ctx-1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    fn fake_message() -> Message {
        Message {
            message_id: "msg-1".into(),
            context_id: None,
            task_id: None,
            role: Role::User,
            parts: vec![],
            metadata: None,
            extensions: None,
            reference_task_ids: None,
        }
    }

    fn fake_push_config() -> TaskPushNotificationConfig {
        TaskPushNotificationConfig {
            url: "https://example.test/callback".into(),
            id: Some("cfg-1".into()),
            task_id: "task-1".into(),
            token: None,
            authentication: None,
            tenant: None,
        }
    }

    #[async_trait]
    impl Transport for FakeTransport {
        async fn send_message(
            &self,
            params: &ServiceParams,
            _req: &SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            self.record(params);
            Ok(SendMessageResponse::Task(fake_task()))
        }

        async fn send_streaming_message(
            &self,
            params: &ServiceParams,
            _req: &SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            self.record(params);
            Ok(stream::iter(vec![Ok(StreamResponse::Task(fake_task()))]).boxed())
        }

        async fn get_task(
            &self,
            params: &ServiceParams,
            _req: &GetTaskRequest,
        ) -> Result<Task, A2AError> {
            self.record(params);
            Ok(fake_task())
        }

        async fn list_tasks(
            &self,
            params: &ServiceParams,
            _req: &ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            self.record(params);
            Ok(ListTasksResponse {
                tasks: vec![fake_task()],
                next_page_token: String::new(),
                page_size: 1,
                total_size: 1,
            })
        }

        async fn cancel_task(
            &self,
            params: &ServiceParams,
            _req: &CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            self.record(params);
            Ok(fake_task())
        }

        async fn subscribe_to_task(
            &self,
            params: &ServiceParams,
            _req: &SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            self.record(params);
            Ok(stream::iter(vec![Ok(StreamResponse::Message(fake_message()))]).boxed())
        }

        async fn create_push_config(
            &self,
            params: &ServiceParams,
            _req: &TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            self.record(params);
            Ok(fake_push_config())
        }

        async fn get_push_config(
            &self,
            params: &ServiceParams,
            _req: &GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            self.record(params);
            Ok(fake_push_config())
        }

        async fn list_push_configs(
            &self,
            params: &ServiceParams,
            _req: &ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            self.record(params);
            Ok(ListTaskPushNotificationConfigsResponse {
                configs: vec![fake_push_config()],
                next_page_token: None,
            })
        }

        async fn delete_push_config(
            &self,
            params: &ServiceParams,
            _req: &DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            self.record(params);
            Ok(())
        }

        async fn get_extended_agent_card(
            &self,
            params: &ServiceParams,
            _req: &GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            self.record(params);
            Ok(AgentCard {
                name: "fake-agent".into(),
                description: "fake".into(),
                version: "0.0.0".into(),
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
            })
        }

        async fn destroy(&self) -> Result<(), A2AError> {
            Ok(())
        }
    }

    const TOKEN: &str = "secret";

    fn handler() -> TransportHandler<FakeTransport> {
        TransportHandler::new(FakeTransport::default(), TOKEN.to_string())
    }

    #[tokio::test]
    async fn test_fake_transport_destroy_is_a_noop() {
        // Exercises the `Transport::destroy` arm of the test double itself;
        // `TransportHandler` never calls it (it's outside `RequestHandler`).
        assert!(handler().transport.destroy().await.is_ok());
    }

    fn authorized_params() -> ServiceParams {
        let mut p = ServiceParams::new();
        p.insert(TOKEN_HEADER.to_string(), vec![TOKEN.to_string()]);
        p.insert("x-tenant-id".to_string(), vec!["acme".to_string()]);
        p
    }

    fn unauthorized_params() -> ServiceParams {
        let mut p = ServiceParams::new();
        p.insert(TOKEN_HEADER.to_string(), vec!["wrong".to_string()]);
        p
    }

    #[tokio::test]
    async fn test_send_message_rejects_an_invalid_token_without_reaching_the_transport() {
        let h = handler();
        let req = SendMessageRequest {
            message: fake_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let err = h
            .send_message(&unauthorized_params(), req)
            .await
            .unwrap_err();
        assert_eq!(err.code, error_code::INVALID_REQUEST);
        assert_eq!(
            h.transport.last_params(),
            None,
            "transport must not be called"
        );
    }

    #[tokio::test]
    async fn test_send_message_forwards_stripped_params_and_returns_the_transport_response() {
        let h = handler();
        let req = SendMessageRequest {
            message: fake_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let resp = h.send_message(&authorized_params(), req).await.unwrap();
        assert!(matches!(resp, SendMessageResponse::Task(t) if t.id == "task-1"));

        let seen = h.transport.last_params().expect("transport was called");
        assert!(!seen.contains_key(TOKEN_HEADER), "token must be stripped");
        assert_eq!(seen.get("x-tenant-id"), Some(&vec!["acme".to_string()]));
    }

    #[tokio::test]
    async fn test_send_streaming_message_rejects_an_invalid_token() {
        let h = handler();
        let req = SendMessageRequest {
            message: fake_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        };
        assert!(
            h.send_streaming_message(&unauthorized_params(), req)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_send_streaming_message_forwards_and_streams_the_transport_response() {
        let h = handler();
        let req = SendMessageRequest {
            message: fake_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let mut stream = h
            .send_streaming_message(&authorized_params(), req)
            .await
            .unwrap();
        let first = stream.next().await.unwrap().unwrap();
        assert!(matches!(first, StreamResponse::Task(t) if t.id == "task-1"));
    }

    #[tokio::test]
    async fn test_get_task_rejects_an_invalid_token() {
        let h = handler();
        let req = GetTaskRequest {
            id: "task-1".into(),
            history_length: None,
            tenant: None,
        };
        assert!(h.get_task(&unauthorized_params(), req).await.is_err());
    }

    #[tokio::test]
    async fn test_get_task_forwards_and_returns_the_transport_response() {
        let h = handler();
        let req = GetTaskRequest {
            id: "task-1".into(),
            history_length: None,
            tenant: None,
        };
        let task = h.get_task(&authorized_params(), req).await.unwrap();
        assert_eq!(task.id, "task-1");
        assert!(
            !h.transport
                .last_params()
                .unwrap()
                .contains_key(TOKEN_HEADER)
        );
    }

    #[tokio::test]
    async fn test_list_tasks_rejects_an_invalid_token() {
        let h = handler();
        let req = ListTasksRequest {
            context_id: None,
            status: None,
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        assert!(h.list_tasks(&unauthorized_params(), req).await.is_err());
    }

    #[tokio::test]
    async fn test_list_tasks_forwards_and_returns_the_transport_response() {
        let h = handler();
        let req = ListTasksRequest {
            context_id: None,
            status: None,
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = h.list_tasks(&authorized_params(), req).await.unwrap();
        assert_eq!(resp.total_size, 1);
    }

    #[tokio::test]
    async fn test_cancel_task_rejects_an_invalid_token() {
        let h = handler();
        let req = CancelTaskRequest {
            id: "task-1".into(),
            metadata: None,
            tenant: None,
        };
        assert!(h.cancel_task(&unauthorized_params(), req).await.is_err());
    }

    #[tokio::test]
    async fn test_cancel_task_forwards_and_returns_the_transport_response() {
        let h = handler();
        let req = CancelTaskRequest {
            id: "task-1".into(),
            metadata: None,
            tenant: None,
        };
        let task = h.cancel_task(&authorized_params(), req).await.unwrap();
        assert_eq!(task.id, "task-1");
    }

    #[tokio::test]
    async fn test_subscribe_to_task_rejects_an_invalid_token() {
        let h = handler();
        let req = SubscribeToTaskRequest {
            id: "task-1".into(),
            tenant: None,
        };
        assert!(
            h.subscribe_to_task(&unauthorized_params(), req)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_subscribe_to_task_forwards_and_streams_the_transport_response() {
        let h = handler();
        let req = SubscribeToTaskRequest {
            id: "task-1".into(),
            tenant: None,
        };
        let mut stream = h
            .subscribe_to_task(&authorized_params(), req)
            .await
            .unwrap();
        let first = stream.next().await.unwrap().unwrap();
        assert!(matches!(first, StreamResponse::Message(_)));
    }

    #[tokio::test]
    async fn test_create_push_config_rejects_an_invalid_token() {
        let h = handler();
        assert!(
            h.create_push_config(&unauthorized_params(), fake_push_config())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_create_push_config_forwards_and_returns_the_transport_response() {
        let h = handler();
        let cfg = h
            .create_push_config(&authorized_params(), fake_push_config())
            .await
            .unwrap();
        assert_eq!(cfg.id, Some("cfg-1".to_string()));
    }

    #[tokio::test]
    async fn test_get_push_config_rejects_an_invalid_token() {
        let h = handler();
        let req = GetTaskPushNotificationConfigRequest {
            task_id: "task-1".into(),
            id: "cfg-1".into(),
            tenant: None,
        };
        assert!(
            h.get_push_config(&unauthorized_params(), req)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_get_push_config_forwards_and_returns_the_transport_response() {
        let h = handler();
        let req = GetTaskPushNotificationConfigRequest {
            task_id: "task-1".into(),
            id: "cfg-1".into(),
            tenant: None,
        };
        let cfg = h.get_push_config(&authorized_params(), req).await.unwrap();
        assert_eq!(cfg.id, Some("cfg-1".to_string()));
    }

    #[tokio::test]
    async fn test_list_push_configs_rejects_an_invalid_token() {
        let h = handler();
        let req = ListTaskPushNotificationConfigsRequest {
            task_id: "task-1".into(),
            page_size: None,
            page_token: None,
            tenant: None,
        };
        assert!(
            h.list_push_configs(&unauthorized_params(), req)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_list_push_configs_forwards_and_returns_the_transport_response() {
        let h = handler();
        let req = ListTaskPushNotificationConfigsRequest {
            task_id: "task-1".into(),
            page_size: None,
            page_token: None,
            tenant: None,
        };
        let resp = h
            .list_push_configs(&authorized_params(), req)
            .await
            .unwrap();
        assert_eq!(resp.configs.len(), 1);
    }

    #[tokio::test]
    async fn test_delete_push_config_rejects_an_invalid_token() {
        let h = handler();
        let req = DeleteTaskPushNotificationConfigRequest {
            task_id: "task-1".into(),
            id: "cfg-1".into(),
            tenant: None,
        };
        assert!(
            h.delete_push_config(&unauthorized_params(), req)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_delete_push_config_forwards_and_returns_the_transport_response() {
        let h = handler();
        let req = DeleteTaskPushNotificationConfigRequest {
            task_id: "task-1".into(),
            id: "cfg-1".into(),
            tenant: None,
        };
        assert!(
            h.delete_push_config(&authorized_params(), req)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_get_extended_agent_card_rejects_an_invalid_token() {
        let h = handler();
        let req = GetExtendedAgentCardRequest { tenant: None };
        assert!(
            h.get_extended_agent_card(&unauthorized_params(), req)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_get_extended_agent_card_forwards_and_returns_the_transport_response() {
        let h = handler();
        let req = GetExtendedAgentCardRequest { tenant: None };
        let card = h
            .get_extended_agent_card(&authorized_params(), req)
            .await
            .unwrap();
        assert_eq!(card.name, "fake-agent");
    }

    // ── run_with_config: fail-fast paths ────────────────────────────────────
    //
    // These exercise real (not mocked) error handling in `run_with_config`
    // itself, up to the point where a live SLIM gateway would be required.
    // No live gateway is needed for either: `parse_slimrpc_target` is pure
    // string parsing, and connecting to a closed local port fails immediately
    // with "connection refused" rather than needing a real peer.

    fn minimal_config(client_endpoint: &str) -> PluginConfig {
        let yaml = format!(
            r#"
client:
  endpoint: "{client_endpoint}"
app:
  name: "org/namespace/agent"
  identity_provider:
    type: shared_secret
    id: "my-id"
    data: "shared-secret-value-0123456789abcdef"
  identity_verifier:
    type: shared_secret
    id: "my-id"
    data: "shared-secret-value-0123456789abcdef"
"#
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    /// Binds an ephemeral loopback port and immediately drops the listener,
    /// so connecting to it fails fast with "connection refused" instead of
    /// hanging or needing a real SLIM gateway.
    fn closed_local_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    #[tokio::test]
    async fn test_run_with_config_rejects_an_invalid_endpoint() {
        let config = minimal_config("http://127.0.0.1:1");
        let err = run_with_config("not-a-valid-target", config)
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::InvalidEndpoint(_)));
    }

    #[tokio::test]
    async fn test_run_with_config_reports_a_connect_failure_for_an_unreachable_gateway() {
        let port = closed_local_port();
        let config = minimal_config(&format!("http://127.0.0.1:{port}"));
        let err = run_with_config("org/namespace/agent", config)
            .await
            .unwrap_err();
        assert!(matches!(err, PluginError::Slim(_)));
    }
}
