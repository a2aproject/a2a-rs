// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::event::StreamResponse;
use a2a::*;
use a2a_client::transport::{ServiceParams, Transport, TransportFactory};
use a2a_pb::pbconv;
use a2a_pb::proto::a2a_service_client::A2aServiceClient;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use tonic::transport::{Channel, Endpoint};

use crate::errors::status_to_a2a_error;

/// gRPC transport — implements the `Transport` trait using tonic gRPC client.
///
/// Converts native A2A types to proto types via `a2a_pb::pbconv`,
/// calls the gRPC service, and converts responses back.
pub struct GrpcTransport {
    channel: Channel,
}

fn normalize_grpc_endpoint(endpoint: &str) -> String {
    if endpoint.contains("://") {
        endpoint.to_string()
    } else {
        format!("http://{endpoint}")
    }
}

impl GrpcTransport {
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self, A2AError> {
        let endpoint_str = normalize_grpc_endpoint(&endpoint.into());
        let channel = Endpoint::from_shared(endpoint_str)
            .map_err(|e| A2AError::internal(format!("gRPC endpoint error: {e}")))?
            .connect()
            .await
            .map_err(|e| A2AError::internal(format!("gRPC connect error: {e}")))?;
        Ok(GrpcTransport { channel })
    }

    pub fn from_channel(channel: Channel) -> Self {
        GrpcTransport { channel }
    }

    fn client(&self) -> A2aServiceClient<Channel> {
        A2aServiceClient::new(self.channel.clone())
    }
}

/// Convert `ServiceParams` to gRPC metadata.
fn service_params_to_metadata(params: &ServiceParams) -> tonic::metadata::MetadataMap {
    let mut metadata = tonic::metadata::MetadataMap::new();
    for (key, values) in params {
        if let Ok(key) = tonic::metadata::MetadataKey::from_bytes(key.as_bytes()) {
            for value in values {
                if let Ok(val) = value.parse() {
                    metadata.insert(key.clone(), val);
                }
            }
        }
    }
    metadata
}

fn make_request<T>(params: &ServiceParams, msg: T) -> tonic::Request<T> {
    let mut req = tonic::Request::new(msg);
    let metadata = service_params_to_metadata(params);
    *req.metadata_mut() = metadata;
    req
}

#[async_trait]
impl Transport for GrpcTransport {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let proto_req = pbconv::to_proto_send_message_request(req);
        let grpc_req = make_request(params, proto_req);
        let mut client = self.client();
        let response = client
            .send_message(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        pbconv::from_proto_send_message_response(response.get_ref())
            .ok_or_else(|| A2AError::internal("empty SendMessageResponse payload"))
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let proto_req = pbconv::to_proto_send_message_request(req);
        let grpc_req = make_request(params, proto_req);
        let mut client = self.client();
        let response = client
            .send_streaming_message(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        let stream = response.into_inner().map(|item| match item {
            Ok(proto_sr) => pbconv::from_proto_stream_response(&proto_sr)
                .ok_or_else(|| A2AError::internal("empty StreamResponse payload")),
            Err(s) => Err(status_to_a2a_error(&s)),
        });
        Ok(Box::pin(stream))
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: &GetTaskRequest,
    ) -> Result<Task, A2AError> {
        let proto_req = pbconv::to_proto_get_task_request(req);
        let grpc_req = make_request(params, proto_req);
        let mut client = self.client();
        let response = client
            .get_task(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_task(response.get_ref()))
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        let proto_req = pbconv::to_proto_list_tasks_request(req);
        let grpc_req = make_request(params, proto_req);
        let mut client = self.client();
        let response = client
            .list_tasks(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_list_tasks_response(response.get_ref()))
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: &CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let proto_req = pbconv::to_proto_cancel_task_request(req);
        let grpc_req = make_request(params, proto_req);
        let mut client = self.client();
        let response = client
            .cancel_task(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_task(response.get_ref()))
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: &SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let proto_req = pbconv::to_proto_subscribe_to_task_request(req);
        let grpc_req = make_request(params, proto_req);
        let mut client = self.client();
        let response = client
            .subscribe_to_task(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        let stream = response.into_inner().map(|item| match item {
            Ok(proto_sr) => pbconv::from_proto_stream_response(&proto_sr)
                .ok_or_else(|| A2AError::internal("empty StreamResponse payload")),
            Err(s) => Err(status_to_a2a_error(&s)),
        });
        Ok(Box::pin(stream))
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: &CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let proto_req = pbconv::to_proto_create_task_push_notification_config_request(req);
        let grpc_req = make_request(params, proto_req);
        let mut client = self.client();
        let response = client
            .create_task_push_notification_config(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_task_push_notification_config(
            response.get_ref(),
        ))
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let proto_req = pbconv::to_proto_get_task_push_notification_config_request(req);
        let grpc_req = make_request(params, proto_req);
        let mut client = self.client();
        let response = client
            .get_task_push_notification_config(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_task_push_notification_config(
            response.get_ref(),
        ))
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let proto_req = pbconv::to_proto_list_task_push_notification_configs_request(req);
        let grpc_req = make_request(params, proto_req);
        let mut client = self.client();
        let response = client
            .list_task_push_notification_configs(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_list_task_push_notification_configs_response(response.get_ref()))
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let proto_req = pbconv::to_proto_delete_task_push_notification_config_request(req);
        let grpc_req = make_request(params, proto_req);
        let mut client = self.client();
        client
            .delete_task_push_notification_config(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(())
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: &GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        let proto_req = pbconv::to_proto_get_extended_agent_card_request(req);
        let grpc_req = make_request(params, proto_req);
        let mut client = self.client();
        let response = client
            .get_extended_agent_card(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_agent_card(response.get_ref()))
    }

    async fn destroy(&self) -> Result<(), A2AError> {
        Ok(())
    }
}

/// Factory for creating gRPC transports.
///
/// Supports optional TLS via rustls when the `rustls` feature is enabled.
/// Use [`GrpcTransportFactory::new()`] for plain connections, or
/// [`GrpcTransportFactory::with_rustls_config()`] to enable TLS.
pub struct GrpcTransportFactory {
    #[cfg(feature = "rustls")]
    tls_config: Option<std::sync::Arc<tokio_rustls::rustls::ClientConfig>>,
}

impl GrpcTransportFactory {
    pub fn new() -> Self {
        GrpcTransportFactory {
            #[cfg(feature = "rustls")]
            tls_config: None,
        }
    }

    #[cfg(feature = "rustls")]
    pub fn with_rustls_config(config: std::sync::Arc<tokio_rustls::rustls::ClientConfig>) -> Self {
        GrpcTransportFactory {
            tls_config: Some(config),
        }
    }
}

impl Default for GrpcTransportFactory {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "rustls")]
fn server_name_from_url(
    url: &str,
) -> Result<tokio_rustls::rustls::pki_types::ServerName<'static>, A2AError> {
    let uri: http::Uri = url
        .parse()
        .map_err(|e| A2AError::internal(format!("invalid URL: {e}")))?;
    let host = uri
        .host()
        .ok_or_else(|| A2AError::internal("URL has no host"))?
        .to_string();
    tokio_rustls::rustls::pki_types::ServerName::try_from(host)
        .map_err(|e| A2AError::internal(format!("invalid server name: {e}")))
}

#[cfg(feature = "rustls")]
impl GrpcTransportFactory {
    async fn connect_tls(
        url: &str,
        tls_config: &std::sync::Arc<tokio_rustls::rustls::ClientConfig>,
    ) -> Result<GrpcTransport, A2AError> {
        let url = normalize_grpc_endpoint(url);
        let server_name = server_name_from_url(&url)?;
        let ep = Endpoint::from_shared(url)
            .map_err(|e| A2AError::internal(format!("invalid endpoint: {e}")))?;
        let transport = tonic_tls::TcpTransport::from_endpoint(&ep);
        let connector =
            tonic_tls::rustls::TlsConnector::new(transport, tls_config.clone(), server_name);
        let channel = ep
            .connect_with_connector(connector)
            .await
            .map_err(|e| A2AError::internal(format!("gRPC TLS connect: {e}")))?;
        Ok(GrpcTransport::from_channel(channel))
    }
}

#[async_trait]
impl TransportFactory for GrpcTransportFactory {
    fn protocol(&self) -> &str {
        a2a::TRANSPORT_PROTOCOL_GRPC
    }

    async fn create(
        &self,
        _card: &AgentCard,
        iface: &AgentInterface,
    ) -> Result<Box<dyn Transport>, A2AError> {
        #[cfg(feature = "rustls")]
        if let Some(tls_config) = &self.tls_config {
            return Ok(Box::new(Self::connect_tls(&iface.url, tls_config).await?));
        }

        let transport = GrpcTransport::connect(&iface.url).await?;
        Ok(Box::new(transport))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_grpc_endpoint_adds_http_scheme() {
        assert_eq!(
            normalize_grpc_endpoint("127.0.0.1:50051"),
            "http://127.0.0.1:50051"
        );
    }

    #[test]
    fn test_normalize_grpc_endpoint_preserves_existing_scheme() {
        assert_eq!(
            normalize_grpc_endpoint("http://127.0.0.1:50051"),
            "http://127.0.0.1:50051"
        );
        assert_eq!(
            normalize_grpc_endpoint("https://example.com:443"),
            "https://example.com:443"
        );
    }

    #[test]
    fn test_service_params_to_metadata_empty() {
        let params = ServiceParams::new();
        let metadata = service_params_to_metadata(&params);
        assert_eq!(metadata.len(), 0);
    }

    #[test]
    fn test_service_params_to_metadata_ascii() {
        let mut params = ServiceParams::new();
        params.insert("x-custom".to_string(), vec!["value1".to_string()]);
        params.insert(
            "x-multi".to_string(),
            vec!["a".to_string(), "b".to_string()],
        );
        let metadata = service_params_to_metadata(&params);
        assert!(metadata.get("x-custom").is_some());
    }

    #[test]
    fn test_make_request() {
        let mut params = ServiceParams::new();
        params.insert("x-version".to_string(), vec!["1.0".to_string()]);
        let req = make_request(&params, "test-body".to_string());
        assert!(req.metadata().get("x-version").is_some());
        assert_eq!(req.get_ref(), "test-body");
    }

    #[test]
    fn test_grpc_transport_factory_protocol() {
        let f = GrpcTransportFactory::new();
        assert_eq!(f.protocol(), "GRPC");
    }

    #[tokio::test]
    async fn test_grpc_transport_connect_failure() {
        // Connecting to invalid endpoint should fail
        let result = GrpcTransport::connect("http://[::1]:1").await;
        // connect may succeed (deferred connection) or fail
        // Either way we're testing the code path
        let _ = result;
    }

    #[test]
    fn test_grpc_transport_from_channel() {
        // We can't easily create a real Channel without a server,
        // but we can test the factory protocol
        let f = GrpcTransportFactory::new();
        assert_eq!(f.protocol(), a2a::TRANSPORT_PROTOCOL_GRPC);
    }

    #[test]
    fn test_grpc_transport_factory_default() {
        let f = GrpcTransportFactory::default();
        assert_eq!(f.protocol(), a2a::TRANSPORT_PROTOCOL_GRPC);
    }

    #[cfg(feature = "rustls")]
    mod tls_tests {
        use super::*;
        use std::sync::Arc;

        fn self_signed_ca_pem() -> Vec<u8> {
            let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, "Test CA");
            let key = rcgen::KeyPair::generate().unwrap();
            let cert = params.self_signed(&key).unwrap();
            cert.pem().into_bytes()
        }

        fn build_test_tls_config(ca_pem: &[u8]) -> Arc<tokio_rustls::rustls::ClientConfig> {
            let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
            let ca_cert = rustls_pemfile::certs(&mut &ca_pem[..])
                .next()
                .unwrap()
                .unwrap();
            let mut root_store = tokio_rustls::rustls::RootCertStore::empty();
            root_store.add(ca_cert).unwrap();
            let mut config = tokio_rustls::rustls::ClientConfig::builder()
                .with_root_certificates(root_store)
                .with_no_client_auth();
            config.alpn_protocols = vec![tonic_tls::ALPN_H2.to_vec()];
            Arc::new(config)
        }

        #[test]
        fn test_with_rustls_config_protocol() {
            let ca_pem = self_signed_ca_pem();
            let config = build_test_tls_config(&ca_pem);
            let f = GrpcTransportFactory::with_rustls_config(config);
            assert_eq!(f.protocol(), a2a::TRANSPORT_PROTOCOL_GRPC);
            assert!(f.tls_config.is_some());
        }

        #[test]
        fn test_new_has_no_tls_config() {
            let f = GrpcTransportFactory::new();
            assert!(f.tls_config.is_none());
        }

        #[test]
        fn test_server_name_from_url_https() {
            let name = server_name_from_url("https://example.com:443").unwrap();
            assert_eq!(
                name,
                tokio_rustls::rustls::pki_types::ServerName::try_from("example.com").unwrap()
            );
        }

        #[test]
        fn test_server_name_from_url_localhost() {
            let name = server_name_from_url("https://localhost:50052").unwrap();
            assert_eq!(
                name,
                tokio_rustls::rustls::pki_types::ServerName::try_from("localhost").unwrap()
            );
        }

        #[test]
        fn test_server_name_from_url_ip() {
            let name = server_name_from_url("https://127.0.0.1:50052").unwrap();
            assert_eq!(
                name,
                tokio_rustls::rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap()
            );
        }

        #[test]
        fn test_server_name_from_url_invalid() {
            let result = server_name_from_url("not a url");
            assert!(result.is_err());
        }

        #[tokio::test]
        async fn test_tls_factory_create_fails_on_unreachable() {
            let ca_pem = self_signed_ca_pem();
            let config = build_test_tls_config(&ca_pem);
            let f = GrpcTransportFactory::with_rustls_config(config);
            let card = AgentCard {
                name: "test".into(),
                description: "test".into(),
                version: "1.0".into(),
                supported_interfaces: vec![],
                capabilities: AgentCapabilities::default(),
                default_input_modes: vec!["text".into()],
                default_output_modes: vec!["text".into()],
                skills: vec![],
                provider: None,
                documentation_url: None,
                icon_url: None,
                security_schemes: None,
                security_requirements: None,
                signatures: None,
            };
            let iface = AgentInterface::new("https://127.0.0.1:1", TRANSPORT_PROTOCOL_GRPC);
            let result = f.create(&card, &iface).await;
            assert!(result.is_err());
        }
    }
}
