// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use std::collections::HashMap;
use std::sync::Arc;

use crate::client::A2AClient;
use crate::jsonrpc::JsonRpcTransportFactory;
use crate::middleware::CallInterceptor;
use crate::rest::RestTransportFactory;
use crate::transport::TransportFactory;
use crate::websocket::WebSocketTransportFactory;

/// Key for looking up transport factories.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
struct TransportKey {
    protocol: String,
    major_version: u64,
}

impl TransportKey {
    fn from_interface(iface: &AgentInterface) -> Self {
        let major = iface
            .protocol_version
            .split('.')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1);
        TransportKey {
            protocol: iface.protocol_binding.clone(),
            major_version: major,
        }
    }

    fn from_protocol(protocol: &str, version: &str) -> Self {
        let major = version
            .split('.')
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1);
        TransportKey {
            protocol: protocol.to_string(),
            major_version: major,
        }
    }
}

/// Factory for creating [`A2AClient`] instances with automatic protocol negotiation.
///
/// Maintains a registry of [`TransportFactory`] implementations indexed by protocol
/// and major version. When creating a client from an [`AgentCard`], it selects the
/// best matching transport based on the agent's declared interfaces and the client's
/// preferred bindings.
pub struct A2AClientFactory {
    factories: HashMap<TransportKey, Arc<dyn TransportFactory>>,
    preferred_bindings: Vec<String>,
    interceptors: Vec<Arc<dyn CallInterceptor>>,
}

impl A2AClientFactory {
    /// Create a builder for configuring the factory.
    pub fn builder() -> A2AClientFactoryBuilder {
        A2AClientFactoryBuilder::new()
    }

    /// Create a client by negotiating the best transport with the agent card.
    ///
    /// Selection algorithm (mirrors Go SDK):
    /// 1. For each interface in the agent card's `supported_interfaces`
    /// 2. Look up matching factory by (protocol, major_version)
    /// 3. Rank candidates: client preference order, then newest version
    /// 4. Try connecting in rank order; first success wins
    pub async fn create_from_card(
        &self,
        card: &AgentCard,
    ) -> Result<A2AClient<Box<dyn crate::Transport>>, A2AError> {
        let mut candidates: Vec<(usize, &AgentInterface, &Arc<dyn TransportFactory>)> = Vec::new();

        for iface in &card.supported_interfaces {
            let key = TransportKey::from_interface(iface);
            if let Some(factory) = self.factories.get(&key) {
                let priority = self
                    .preferred_bindings
                    .iter()
                    .position(|b| b == &iface.protocol_binding)
                    .unwrap_or(usize::MAX);
                candidates.push((priority, iface, factory));
            }
        }

        if candidates.is_empty() {
            return Err(A2AError::unsupported_operation(
                "no compatible transport found for agent card interfaces",
            ));
        }

        // Sort by preference (lower is better)
        candidates.sort_by_key(|(prio, _, _)| *prio);

        let mut last_err = None;
        for (_prio, iface, factory) in &candidates {
            match factory.create(card, iface).await {
                Ok(transport) => {
                    return Ok(
                        A2AClient::new(transport).with_interceptors(self.interceptors.clone())
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        protocol = %iface.protocol_binding,
                        url = %iface.url,
                        error = %e,
                        "transport creation failed, trying next"
                    );
                    last_err = Some(e);
                }
            }
        }

        Err(last_err.unwrap_or_else(|| A2AError::internal("failed to create transport")))
    }
}

/// Builder for [`A2AClientFactory`].
pub struct A2AClientFactoryBuilder {
    factories: HashMap<TransportKey, Arc<dyn TransportFactory>>,
    preferred_bindings: Vec<String>,
    interceptors: Vec<Arc<dyn CallInterceptor>>,
    include_defaults: bool,
}

impl A2AClientFactoryBuilder {
    fn new() -> Self {
        A2AClientFactoryBuilder {
            factories: HashMap::new(),
            preferred_bindings: vec![
                TRANSPORT_PROTOCOL_JSONRPC.to_string(),
                TRANSPORT_PROTOCOL_HTTP_JSON.to_string(),
            ],
            interceptors: Vec::new(),
            include_defaults: true,
        }
    }

    /// Register a transport factory for a protocol binding.
    pub fn register(mut self, factory: Arc<dyn TransportFactory>) -> Self {
        let key = TransportKey::from_protocol(factory.protocol(), VERSION);
        self.factories.insert(key, factory);
        self
    }

    /// Set preferred binding order. First is most preferred.
    pub fn preferred_bindings(mut self, bindings: Vec<String>) -> Self {
        self.preferred_bindings = bindings;
        self
    }

    /// Add a call interceptor.
    pub fn with_interceptor(mut self, interceptor: Arc<dyn CallInterceptor>) -> Self {
        self.interceptors.push(interceptor);
        self
    }

    /// Disable default JSON-RPC and REST transport factories.
    pub fn no_defaults(mut self) -> Self {
        self.include_defaults = false;
        self
    }

    /// Build the factory.
    pub fn build(mut self) -> A2AClientFactory {
        if self.include_defaults {
            let jsonrpc_key = TransportKey::from_protocol(TRANSPORT_PROTOCOL_JSONRPC, VERSION);
            self.factories
                .entry(jsonrpc_key)
                .or_insert_with(|| Arc::new(JsonRpcTransportFactory::new(None)));

            let rest_key = TransportKey::from_protocol(TRANSPORT_PROTOCOL_HTTP_JSON, VERSION);
            self.factories
                .entry(rest_key)
                .or_insert_with(|| Arc::new(RestTransportFactory::new(None)));

            let ws_key = TransportKey::from_protocol(TRANSPORT_PROTOCOL_WEBSOCKET, VERSION);
            self.factories
                .entry(ws_key)
                .or_insert_with(|| Arc::new(WebSocketTransportFactory::new()));
        }

        A2AClientFactory {
            factories: self.factories,
            preferred_bindings: self.preferred_bindings,
            interceptors: self.interceptors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use futures::stream;

    #[test]
    fn test_transport_key_from_interface() {
        let iface = AgentInterface::new("http://localhost", "jsonrpc");
        let key = TransportKey::from_interface(&iface);
        assert_eq!(key.protocol, "jsonrpc");
    }

    #[test]
    fn test_transport_key_from_interface_no_version() {
        let mut iface = AgentInterface::new("http://localhost", "rest");
        iface.protocol_version = "bad".to_string();
        let key = TransportKey::from_interface(&iface);
        assert_eq!(key.major_version, 1); // defaults to 1
    }

    #[test]
    fn test_transport_key_from_protocol() {
        let key = TransportKey::from_protocol("jsonrpc", "2.3.4");
        assert_eq!(key.protocol, "jsonrpc");
        assert_eq!(key.major_version, 2);
    }

    #[test]
    fn test_builder_defaults() {
        let factory = A2AClientFactory::builder().build();
        assert_eq!(factory.factories.len(), 3); // jsonrpc + rest + websocket
        assert_eq!(factory.preferred_bindings.len(), 2);
    }

    #[test]
    fn test_builder_no_defaults() {
        let factory = A2AClientFactory::builder().no_defaults().build();
        assert!(factory.factories.is_empty());
    }

    #[test]
    fn test_builder_preferred_bindings() {
        let factory = A2AClientFactory::builder()
            .preferred_bindings(vec!["grpc".to_string()])
            .build();
        assert_eq!(factory.preferred_bindings, vec!["grpc"]);
    }

    #[test]
    fn test_builder_with_interceptor() {
        use crate::middleware::LoggingInterceptor;
        let factory = A2AClientFactory::builder()
            .with_interceptor(Arc::new(LoggingInterceptor))
            .build();
        assert_eq!(factory.interceptors.len(), 1);
    }

    #[tokio::test]
    async fn test_create_from_card_no_matching_transport() {
        let factory = A2AClientFactory::builder().no_defaults().build();
        let card = AgentCard {
            name: "test".into(),
            description: "test agent".into(),
            version: "1.0".into(),
            supported_interfaces: vec![AgentInterface::new("http://localhost", "unknown")],
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
        let result = factory.create_from_card(&card).await;
        assert!(result.is_err());
    }

    struct DummyTransport;

    #[async_trait]
    impl crate::Transport for DummyTransport {
        async fn send_message(
            &self,
            _params: &crate::ServiceParams,
            _req: &SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            unimplemented!()
        }

        async fn send_streaming_message(
            &self,
            _params: &crate::ServiceParams,
            _req: &SendMessageRequest,
        ) -> Result<crate::BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            Ok(Box::pin(stream::empty()))
        }

        async fn get_task(
            &self,
            _params: &crate::ServiceParams,
            _req: &GetTaskRequest,
        ) -> Result<Task, A2AError> {
            unimplemented!()
        }

        async fn list_tasks(
            &self,
            _params: &crate::ServiceParams,
            _req: &ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            unimplemented!()
        }

        async fn cancel_task(
            &self,
            _params: &crate::ServiceParams,
            _req: &CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            unimplemented!()
        }

        async fn subscribe_to_task(
            &self,
            _params: &crate::ServiceParams,
            _req: &SubscribeToTaskRequest,
        ) -> Result<crate::BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            Ok(Box::pin(stream::empty()))
        }

        async fn create_push_config(
            &self,
            _params: &crate::ServiceParams,
            _req: &CreateTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            unimplemented!()
        }

        async fn get_push_config(
            &self,
            _params: &crate::ServiceParams,
            _req: &GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            unimplemented!()
        }

        async fn list_push_configs(
            &self,
            _params: &crate::ServiceParams,
            _req: &ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            unimplemented!()
        }

        async fn delete_push_config(
            &self,
            _params: &crate::ServiceParams,
            _req: &DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            unimplemented!()
        }

        async fn get_extended_agent_card(
            &self,
            _params: &crate::ServiceParams,
            _req: &GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            unimplemented!()
        }

        async fn destroy(&self) -> Result<(), A2AError> {
            Ok(())
        }
    }

    struct DummyFactory {
        protocol: &'static str,
    }

    #[async_trait]
    impl TransportFactory for DummyFactory {
        fn protocol(&self) -> &str {
            self.protocol
        }

        async fn create(
            &self,
            _card: &AgentCard,
            _iface: &AgentInterface,
        ) -> Result<Box<dyn crate::Transport>, A2AError> {
            Ok(Box::new(DummyTransport))
        }
    }

    fn card_with_interfaces(supported_interfaces: Vec<AgentInterface>) -> AgentCard {
        AgentCard {
            name: "test".into(),
            description: "test agent".into(),
            version: "1.0".into(),
            supported_interfaces,
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
        }
    }

    #[tokio::test]
    async fn test_create_from_card_selects_websocket_when_only_match() {
        let factory = A2AClientFactory::builder()
            .no_defaults()
            .register(Arc::new(DummyFactory {
                protocol: TRANSPORT_PROTOCOL_WEBSOCKET,
            }))
            .build();
        let card = card_with_interfaces(vec![AgentInterface::new(
            "ws://localhost/ws",
            TRANSPORT_PROTOCOL_WEBSOCKET,
        )]);

        let result = factory.create_from_card(&card).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_create_from_card_honors_websocket_preference() {
        let factory = A2AClientFactory::builder()
            .no_defaults()
            .register(Arc::new(DummyFactory {
                protocol: TRANSPORT_PROTOCOL_JSONRPC,
            }))
            .register(Arc::new(DummyFactory {
                protocol: TRANSPORT_PROTOCOL_WEBSOCKET,
            }))
            .preferred_bindings(vec![TRANSPORT_PROTOCOL_WEBSOCKET.to_string()])
            .build();
        let card = card_with_interfaces(vec![
            AgentInterface::new("http://localhost/rpc", TRANSPORT_PROTOCOL_JSONRPC),
            AgentInterface::new("ws://localhost/ws", TRANSPORT_PROTOCOL_WEBSOCKET),
        ]);

        let result = factory.create_from_card(&card).await;
        assert!(result.is_ok());
    }
}
