// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
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
    let json = serde_json::to_string(hs).map_err(|e| PluginError::Handshake(e.to_string()))?;
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
}

/// Rejects a call unless it carries exactly one `a2a-plugin-token` header
/// matching `token`. Free function (not a method) so it's testable without
/// constructing a `TransportHandler`, which needs a live SLIM connection.
fn check_token(token: &str, params: &ServiceParams) -> Result<(), A2AError> {
    let provided = params.get(TOKEN_HEADER);
    match provided {
        Some(values) if values.len() == 1 && values[0] == token => Ok(()),
        _ => Err(A2AError::new(
            error_code::INVALID_REQUEST,
            "invalid or missing plugin token",
        )),
    }
}

/// Strips the plugin token before forwarding a call upstream.
fn forward_params(params: &ServiceParams) -> ServiceParams {
    let mut fwd = ServiceParams::new();
    for (k, v) in params.iter() {
        if k.eq_ignore_ascii_case(TOKEN_HEADER) {
            continue;
        }
        fwd.insert(k.clone(), v.clone());
    }
    fwd
}

#[async_trait]
impl RequestHandler for TransportHandler {
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

// ── Helpers ────────────────────────────────────────────────────────────────────

/// Parse an app name of the form "org/namespace/agent" into a SLIM `ProtoName`.
fn parse_proto_name(name: &str) -> Result<ProtoName, PluginError> {
    let parts: Vec<&str> = name.splitn(3, '/').collect();
    match parts.as_slice() {
        [org, namespace, agent]
            if !org.is_empty() && !namespace.is_empty() && !agent.is_empty() =>
        {
            Ok(ProtoName::from_strings([*org, *namespace, *agent]))
        }
        _ => Err(PluginError::InvalidEndpoint(format!(
            "app.name '{name}' must be 'org/namespace/agent'"
        ))),
    }
}

// ── Serve entry point ──────────────────────────────────────────────────────────

/// Reads and parses the plugin config at `path`. Split out from `run` so the
/// file-read and YAML-parse failure paths are testable with a tempfile,
/// without touching the process-global `A2A_SLIMRPC_PLUGIN_CONFIG` env var.
fn load_config(path: &str) -> Result<PluginConfig, PluginError> {
    let config_bytes = std::fs::read(path).map_err(|e| PluginError::ConfigRead {
        path: path.to_string(),
        source: e,
    })?;
    serde_yaml::from_slice(&config_bytes).map_err(|e| PluginError::ConfigParse {
        path: path.to_string(),
        source: e,
    })
}

pub async fn run(endpoint: &str) -> Result<(), PluginError> {
    // 1. Load config
    let config_path =
        std::env::var("A2A_SLIMRPC_PLUGIN_CONFIG").map_err(|_| PluginError::ConfigEnvMissing)?;
    let config = load_config(&config_path)?;

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
    use super::*;

    const VALID_CONFIG_YAML: &str = r#"
client:
  endpoint: "grpc://slim-gateway:46357"
app:
  name: "org/namespace/agent"
  identity_provider:
    type: shared_secret
    id: "my-id"
    data: "secret"
  identity_verifier:
    type: shared_secret
    id: "my-id"
    data: "secret"
"#;

    /// A scratch file under the OS temp dir, removed on drop. Avoids adding
    /// a `tempfile` dependency for what's otherwise a one-line write+read.
    struct ScratchFile(std::path::PathBuf);

    impl ScratchFile {
        fn new(name: &str, contents: &str) -> Self {
            let path = std::env::temp_dir().join(name);
            std::fs::write(&path, contents).unwrap();
            Self(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().unwrap()
        }
    }

    impl Drop for ScratchFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn test_load_config_parses_the_documented_example() {
        let file = ScratchFile::new("a2acli-slimrpc-test-valid.yaml", VALID_CONFIG_YAML);
        let config = load_config(file.path()).unwrap();
        assert_eq!(config.app.name, "org/namespace/agent");
    }

    #[test]
    fn test_load_config_reports_a_missing_file() {
        let err = load_config("/nonexistent/path/to/config.yaml").unwrap_err();
        assert!(matches!(err, PluginError::ConfigRead { .. }));
    }

    #[test]
    fn test_load_config_reports_malformed_yaml() {
        let file = ScratchFile::new(
            "a2acli-slimrpc-test-malformed.yaml",
            "not: [valid, this: is: broken",
        );
        let err = load_config(file.path()).unwrap_err();
        assert!(matches!(err, PluginError::ConfigParse { .. }));
    }

    #[test]
    fn test_load_config_reports_a_schema_mismatch() {
        // Valid YAML, but missing the required `app` section.
        let file = ScratchFile::new(
            "a2acli-slimrpc-test-schema-mismatch.yaml",
            "client:\n  endpoint: \"grpc://slim-gateway:46357\"\n",
        );
        let err = load_config(file.path()).unwrap_err();
        assert!(matches!(err, PluginError::ConfigParse { .. }));
    }

    #[test]
    fn test_parse_proto_name_accepts_org_namespace_agent() {
        let name = parse_proto_name("acme/billing/invoicer").unwrap();
        assert_eq!(
            name,
            ProtoName::from_strings(["acme", "billing", "invoicer"])
        );
    }

    #[test]
    fn test_parse_proto_name_rejects_too_few_segments() {
        assert!(parse_proto_name("acme/billing").is_err());
        assert!(parse_proto_name("acme").is_err());
        assert!(parse_proto_name("").is_err());
    }

    #[test]
    fn test_parse_proto_name_rejects_an_empty_segment() {
        assert!(parse_proto_name("acme//invoicer").is_err());
        assert!(parse_proto_name("/billing/invoicer").is_err());
    }

    #[test]
    fn test_parse_proto_name_keeps_a_slash_inside_the_third_segment() {
        // splitn(3, '/') -- the agent segment may itself contain '/'.
        let name = parse_proto_name("acme/billing/invoicer/v2").unwrap();
        assert_eq!(
            name,
            ProtoName::from_strings(["acme", "billing", "invoicer/v2"])
        );
    }

    fn params(pairs: &[(&str, &str)]) -> ServiceParams {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), vec![v.to_string()]))
            .collect()
    }

    #[test]
    fn test_check_token_accepts_the_matching_token() {
        assert!(check_token("secret", &params(&[(TOKEN_HEADER, "secret")])).is_ok());
    }

    #[test]
    fn test_check_token_rejects_a_wrong_token() {
        let err = check_token("secret", &params(&[(TOKEN_HEADER, "wrong")])).unwrap_err();
        assert_eq!(err.code, error_code::INVALID_REQUEST);
    }

    #[test]
    fn test_check_token_rejects_a_missing_header() {
        assert!(check_token("secret", &ServiceParams::new()).is_err());
    }

    #[test]
    fn test_check_token_rejects_a_duplicated_header() {
        let mut p = ServiceParams::new();
        p.insert(
            TOKEN_HEADER.to_string(),
            vec!["secret".into(), "secret".into()],
        );
        assert!(check_token("secret", &p).is_err());
    }

    #[test]
    fn test_forward_params_strips_the_token_header_case_insensitively() {
        let p = params(&[("A2A-Plugin-Token", "secret"), ("x-tenant-id", "acme")]);
        let forwarded = forward_params(&p);
        assert_eq!(forwarded.len(), 1);
        assert_eq!(
            forwarded.get("x-tenant-id"),
            Some(&vec!["acme".to_string()])
        );
    }

    #[test]
    fn test_forward_params_keeps_everything_when_no_token_is_present() {
        let p = params(&[("x-tenant-id", "acme")]);
        assert_eq!(forward_params(&p), p);
    }

    #[test]
    fn test_handshake_success_omits_error_and_renames_payload_fields() {
        let hs = Handshake {
            success: true,
            error: None,
            endpoint: Some(EndpointPayload {
                address: "127.0.0.1:5555".into(),
                binding: TRANSPORT_PROTOCOL_GRPC,
                protocol: VERSION,
                token: "tok".into(),
                cert_pem: "-----BEGIN CERTIFICATE-----".into(),
            }),
        };
        let json: serde_json::Value = serde_json::to_value(&hs).unwrap();
        assert_eq!(json.get("error"), None, "error must be omitted, not null");
        assert_eq!(json["payload"]["certPem"], "-----BEGIN CERTIFICATE-----");
        assert_eq!(json["payload"]["token"], "tok");
    }

    #[test]
    fn test_handshake_failure_omits_the_payload_field() {
        let hs = Handshake {
            success: false,
            error: Some("SLIM gateway connect failed: timed out".into()),
            endpoint: None,
        };
        let json: serde_json::Value = serde_json::to_value(&hs).unwrap();
        assert_eq!(
            json.get("payload"),
            None,
            "payload must be omitted on failure"
        );
        assert_eq!(json["error"], "SLIM gateway connect failed: timed out");
    }
}
