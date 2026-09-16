// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use std::collections::HashMap;
use std::sync::Arc;

use crate::client::A2AClient;
use crate::jsonrpc::JsonRpcTransportFactory;
use crate::middleware::CallInterceptor;
use crate::rest::RestTransportFactory;
use crate::transport::TransportFactory;

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
        self.create_from_card_selecting(card)
            .await
            .map(|(client, _)| client)
    }

    /// Same negotiation as [`Self::create_from_card`], but also returns the
    /// [`AgentInterface`] that was selected — needed by a caller that must
    /// honor that interface's own declared routing `tenant` (A2A §8.3.2) on
    /// every subsequent request, which the interface list alone doesn't
    /// otherwise surface once a client has been built.
    pub async fn create_from_card_with_interface(
        &self,
        card: &AgentCard,
    ) -> Result<(A2AClient<Box<dyn crate::Transport>>, AgentInterface), A2AError> {
        self.create_from_card_selecting(card).await
    }

    async fn create_from_card_selecting(
        &self,
        card: &AgentCard,
    ) -> Result<(A2AClient<Box<dyn crate::Transport>>, AgentInterface), A2AError> {
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
                    let mut client =
                        A2AClient::new(transport).with_interceptors(self.interceptors.clone());
                    // A2A §8.3.2 rule 4: echo the tenant the selected
                    // interface declares on every request. An empty string
                    // is proto3's default for an unset field, so it means
                    // "not declared" rather than a tenant named "".
                    if let Some(tenant) = iface.tenant.as_deref().filter(|t| !t.is_empty()) {
                        client = client.with_tenant(tenant);
                    }
                    return Ok((client, (*iface).clone()));
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
        assert_eq!(factory.factories.len(), 2); // jsonrpc + rest
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

    #[tokio::test]
    async fn test_create_from_card_with_interface_returns_selected_interface() {
        let factory = A2AClientFactory::builder().build();
        let mut iface = AgentInterface::new("http://localhost/jsonrpc", TRANSPORT_PROTOCOL_JSONRPC);
        iface.tenant = Some("tenant-42".to_string());
        let card = AgentCard {
            name: "test".into(),
            description: "test agent".into(),
            version: "1.0".into(),
            supported_interfaces: vec![iface],
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

        let (_client, selected) = factory
            .create_from_card_with_interface(&card)
            .await
            .unwrap();
        assert_eq!(selected.protocol_binding, TRANSPORT_PROTOCOL_JSONRPC);
        assert_eq!(selected.tenant.as_deref(), Some("tenant-42"));
    }

    fn card_with_tenant(tenant: Option<&str>) -> AgentCard {
        let mut iface = AgentInterface::new("http://localhost/jsonrpc", TRANSPORT_PROTOCOL_JSONRPC);
        iface.tenant = tenant.map(str::to_string);
        AgentCard {
            name: "test".into(),
            description: "test agent".into(),
            version: "1.0".into(),
            supported_interfaces: vec![iface],
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

    /// A2A §8.3.2 rule 4: the factory carries the declared tenant onto the
    /// client, so every request echoes it without the caller doing anything.
    #[tokio::test]
    async fn test_create_from_card_applies_the_declared_tenant() {
        let factory = A2AClientFactory::builder().build();
        let client = factory
            .create_from_card(&card_with_tenant(Some("tenant-42")))
            .await
            .unwrap();
        assert_eq!(client.tenant(), Some("tenant-42"));
    }

    /// An entry that declares no tenant leaves the client with none, so the
    /// field stays omitted rather than being defaulted.
    #[tokio::test]
    async fn test_create_from_card_without_a_declared_tenant() {
        let factory = A2AClientFactory::builder().build();
        let client = factory
            .create_from_card(&card_with_tenant(None))
            .await
            .unwrap();
        assert_eq!(client.tenant(), None);
    }

    /// The empty string is proto3's default for an unset field, so it means
    /// "not declared" — not a tenant named "".
    #[tokio::test]
    async fn test_create_from_card_treats_an_empty_tenant_as_undeclared() {
        let factory = A2AClientFactory::builder().build();
        let client = factory
            .create_from_card(&card_with_tenant(Some("")))
            .await
            .unwrap();
        assert_eq!(client.tenant(), None);
    }
}
