// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

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
use serde::{Deserialize, Serialize};
use slim_config::auth::identity::{IdentityProviderConfig, IdentityVerifierConfig};
use slim_config::client::ClientConfig;
use slim_datapath::api::ProtoName;
use slim_service::service::{Service, ServiceBuilder};
use tonic::transport::server::TcpIncoming;
use tonic_tls::rustls::TlsIncoming;
use uuid::Uuid;

use crate::error::PluginError;
use crate::tls::generate_loopback_tls;

const TOKEN_HEADER: &str = "a2a-plugin-token";

// ── Config file schema ─────────────────────────────────────────────────────────
//
// Example:
//   client:
//     endpoint: "grpc://slim-gateway:46357"
//     # optional: tls, auth, backoff, etc. (slim_config::ClientConfig)
//   app:
//     name: "org/namespace/agent"
//     identity_provider:
//       type: shared_secret
//       id: "my-id"
//       data: "secret"
//     identity_verifier:
//       type: shared_secret
//       id: "my-id"
//       data: "secret"

#[derive(Debug, Deserialize)]
pub struct PluginConfig {
    pub client: ClientConfig,
    pub app: AppConfig,
}

#[derive(Debug, Deserialize)]
pub struct AppConfig {
    /// SLIM app name in the form "org/namespace/agent".
    pub name: String,
    pub identity_provider: IdentityProviderConfig,
    pub identity_verifier: IdentityVerifierConfig,
}

// ── Handshake wire types ───────────────────────────────────────────────────────

#[derive(Serialize)]
struct Handshake {
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", rename = "payload")]
    endpoint: Option<EndpointPayload>,
}

#[derive(Serialize)]
struct EndpointPayload {
    address: String,
    binding: &'static str,
    protocol: &'static str,
    token: String,
    #[serde(rename = "certPem")]
    cert_pem: String,
}

fn write_handshake(hs: &Handshake) -> Result<(), PluginError> {
    let json = serde_json::to_string(hs)
        .map_err(|e| PluginError::Handshake(e.to_string()))?;
    println!("{json}");
    Ok(())
}

// ── Transport → RequestHandler adapter ────────────────────────────────────────

/// Adapts SlimRpcTransport (a client Transport) into a server RequestHandler,
/// forwarding all calls while enforcing the per-launch plugin token.
struct TransportHandler {
    transport: SlimRpcTransport,
    token: String,
}

impl TransportHandler {
    fn new(transport: SlimRpcTransport, token: String) -> Self {
        Self { transport, token }
    }

    fn check_token(&self, params: &ServiceParams) -> Result<(), A2AError> {
        let provided = params.get(TOKEN_HEADER);
        match provided {
            Some(values) if values.len() == 1 && values[0] == self.token => Ok(()),
            _ => Err(A2AError::new(
                error_code::INVALID_REQUEST,
                "invalid or missing plugin token",
            )),
        }
    }

    fn forward_params(&self, params: &ServiceParams) -> ServiceParams {
        // Strip the plugin token before forwarding upstream
        let mut fwd = ServiceParams::new();
        for (k, v) in params.iter() {
            if k.eq_ignore_ascii_case(TOKEN_HEADER) {
                continue;
            }
            fwd.insert(k.clone(), v.clone());
        }
        fwd
    }
}

#[async_trait]
impl RequestHandler for TransportHandler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        self.check_token(params)?;
        self.transport
            .send_message(&self.forward_params(params), &req)
            .await
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.check_token(params)?;
        self.transport
            .send_streaming_message(&self.forward_params(params), &req)
            .await
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        self.check_token(params)?;
        self.transport
            .get_task(&self.forward_params(params), &req)
            .await
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        self.check_token(params)?;
        self.transport
            .list_tasks(&self.forward_params(params), &req)
            .await
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        self.check_token(params)?;
        self.transport
            .cancel_task(&self.forward_params(params), &req)
            .await
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.check_token(params)?;
        self.transport
            .subscribe_to_task(&self.forward_params(params), &req)
            .await
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.check_token(params)?;
        self.transport
            .create_push_config(&self.forward_params(params), &req)
            .await
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.check_token(params)?;
        self.transport
            .get_push_config(&self.forward_params(params), &req)
            .await
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        self.check_token(params)?;
        self.transport
            .list_push_configs(&self.forward_params(params), &req)
            .await
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        self.check_token(params)?;
        self.transport
            .delete_push_config(&self.forward_params(params), &req)
            .await
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        self.check_token(params)?;
        self.transport
            .get_extended_agent_card(&self.forward_params(params), &req)
            .await
    }
}

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Parse an app name of the form "org/namespace/agent" into a SLIM `ProtoName`.
fn parse_proto_name(name: &str) -> Result<ProtoName, PluginError> {
    let parts: Vec<&str> = name.splitn(3, '/').collect();
    match parts.as_slice() {
        [org, namespace, agent] if !org.is_empty() && !namespace.is_empty() && !agent.is_empty() => {
            Ok(ProtoName::from_strings([*org, *namespace, *agent]))
        }
        _ => Err(PluginError::InvalidEndpoint(format!(
            "app.name '{name}' must be 'org/namespace/agent'"
        ))),
    }
}

// ── Serve entry point ──────────────────────────────────────────────────────────

pub async fn run(endpoint: &str) -> Result<(), PluginError> {
    // 1. Load config
    let config_path = std::env::var("A2A_SLIMRPC_PLUGIN_CONFIG")
        .map_err(|_| PluginError::ConfigEnvMissing)?;
    let config_bytes = std::fs::read(&config_path).map_err(|e| PluginError::ConfigRead {
        path: config_path.clone(),
        source: e,
    })?;
    let config: PluginConfig = serde_yaml::from_slice(&config_bytes).map_err(|e| {
        PluginError::ConfigParse {
            path: config_path.clone(),
            source: e,
        }
    })?;

    // 2. Parse SLIMRPC remote target
    let remote = parse_slimrpc_target(endpoint).map_err(|e| {
        PluginError::InvalidEndpoint(format!("{endpoint}: {}", e.message))
    })?;

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
    std_listener.set_nonblocking(true).map_err(PluginError::Bind)?;
    let tokio_listener = tokio::net::TcpListener::from_std(std_listener).map_err(PluginError::Bind)?;

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
