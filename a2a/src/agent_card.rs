// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use std::collections::HashMap;

use crate::types::{ProtocolVersion, TRANSPORT_PROTOCOL_GRPC, TransportProtocol};

// ---------------------------------------------------------------------------
// AgentCard
// ---------------------------------------------------------------------------

/// Self-describing manifest for an agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    pub name: String,
    pub description: String,
    pub version: String,
    pub supported_interfaces: Vec<AgentInterface>,
    pub capabilities: AgentCapabilities,
    pub default_input_modes: Vec<String>,
    pub default_output_modes: Vec<String>,
    #[serde(default, deserialize_with = "deserialize_vec_null_as_default")]
    pub skills: Vec<AgentSkill>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<AgentProvider>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_url: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_schemes: Option<HashMap<String, SecurityScheme>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_requirements: Option<Vec<SecurityRequirement>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signatures: Option<Vec<AgentCardSignature>>,
}

fn deserialize_vec_null_as_default<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

// ---------------------------------------------------------------------------
// AgentInterface
// ---------------------------------------------------------------------------

/// A URL + protocol binding combination for reaching the agent.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentInterface {
    pub url: String,
    pub protocol_binding: TransportProtocol,
    pub protocol_version: ProtocolVersion,
    pub tenant: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentInterfaceSerde {
    url: String,
    protocol_binding: TransportProtocol,
    protocol_version: ProtocolVersion,
    #[serde(default)]
    tenant: Option<String>,
}

fn normalize_agent_interface_url(url: String, protocol_binding: &str) -> String {
    if protocol_binding.eq_ignore_ascii_case(TRANSPORT_PROTOCOL_GRPC) {
        if let Some(stripped) = url.strip_prefix("http://") {
            return stripped.to_string();
        }
    }

    url
}

impl AgentInterface {
    pub fn new(url: impl Into<String>, protocol_binding: impl Into<String>) -> Self {
        let protocol_binding = protocol_binding.into();
        AgentInterface {
            url: normalize_agent_interface_url(url.into(), &protocol_binding),
            protocol_binding,
            protocol_version: crate::VERSION.to_string(),
            tenant: None,
        }
    }

    pub fn wire_url(&self) -> String {
        normalize_agent_interface_url(self.url.clone(), &self.protocol_binding)
    }
}

impl Serialize for AgentInterface {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;

        let mut state = serializer
            .serialize_struct("AgentInterface", if self.tenant.is_some() { 4 } else { 3 })?;
        state.serialize_field("url", &self.wire_url())?;
        state.serialize_field("protocolBinding", &self.protocol_binding)?;
        state.serialize_field("protocolVersion", &self.protocol_version)?;
        if let Some(tenant) = &self.tenant {
            state.serialize_field("tenant", tenant)?;
        }
        state.end()
    }
}

impl<'de> Deserialize<'de> for AgentInterface {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = AgentInterfaceSerde::deserialize(deserializer)?;

        Ok(Self {
            url: normalize_agent_interface_url(raw.url, &raw.protocol_binding),
            protocol_binding: raw.protocol_binding,
            protocol_version: raw.protocol_version,
            tenant: raw.tenant,
        })
    }
}

// ---------------------------------------------------------------------------
// AgentProvider
// ---------------------------------------------------------------------------

/// Information about the agent's service provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProvider {
    pub organization: String,
    pub url: String,
}

// ---------------------------------------------------------------------------
// AgentCapabilities
// ---------------------------------------------------------------------------

/// Optional capabilities supported by an agent.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub streaming: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_notifications: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<AgentExtension>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extended_agent_card: Option<bool>,
}

// ---------------------------------------------------------------------------
// AgentExtension
// ---------------------------------------------------------------------------

/// A protocol extension supported by the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentExtension {
    pub uri: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<HashMap<String, Value>>,
}

// ---------------------------------------------------------------------------
// AgentSkill
// ---------------------------------------------------------------------------

/// A distinct capability or function that an agent can perform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub tags: Vec<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub examples: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_modes: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_modes: Option<Vec<String>>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_requirements: Option<Vec<SecurityRequirement>>,
}

// ---------------------------------------------------------------------------
// SecurityScheme (field-presence union)
// ---------------------------------------------------------------------------

/// A security scheme for authorizing requests, following OpenAPI 3.0.
#[derive(Debug, Clone, PartialEq)]
pub enum SecurityScheme {
    ApiKey(ApiKeySecurityScheme),
    HttpAuth(HttpAuthSecurityScheme),
    OAuth2(OAuth2SecurityScheme),
    OpenIdConnect(OpenIdConnectSecurityScheme),
    MutualTls(MutualTlsSecurityScheme),
}

impl Serialize for SecurityScheme {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            SecurityScheme::ApiKey(s) => map.serialize_entry("apiKeySecurityScheme", s)?,
            SecurityScheme::HttpAuth(s) => map.serialize_entry("httpAuthSecurityScheme", s)?,
            SecurityScheme::OAuth2(s) => map.serialize_entry("oauth2SecurityScheme", s)?,
            SecurityScheme::OpenIdConnect(s) => {
                map.serialize_entry("openIdConnectSecurityScheme", s)?
            }
            SecurityScheme::MutualTls(s) => map.serialize_entry("mtlsSecurityScheme", s)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for SecurityScheme {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw: HashMap<String, Value> = HashMap::deserialize(deserializer)?;
        if let Some(v) = raw.get("apiKeySecurityScheme") {
            Ok(SecurityScheme::ApiKey(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("httpAuthSecurityScheme") {
            Ok(SecurityScheme::HttpAuth(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("oauth2SecurityScheme") {
            Ok(SecurityScheme::OAuth2(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("openIdConnectSecurityScheme") {
            Ok(SecurityScheme::OpenIdConnect(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("mtlsSecurityScheme") {
            Ok(SecurityScheme::MutualTls(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else {
            Err(serde::de::Error::custom("unknown security scheme variant"))
        }
    }
}

// ---------------------------------------------------------------------------
// Security scheme types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApiKeySecurityScheme {
    pub location: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpAuthSecurityScheme {
    pub scheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bearer_format: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuth2SecurityScheme {
    pub flows: OAuthFlows,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth2_metadata_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenIdConnectSecurityScheme {
    pub open_id_connect_url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MutualTlsSecurityScheme {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

// ---------------------------------------------------------------------------
// OAuth flows (field-presence union)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum OAuthFlows {
    AuthorizationCode(AuthorizationCodeOAuthFlow),
    ClientCredentials(ClientCredentialsOAuthFlow),
    DeviceCode(DeviceCodeOAuthFlow),
    Implicit(ImplicitOAuthFlow),
    Password(PasswordOAuthFlow),
}

impl Serialize for OAuthFlows {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            OAuthFlows::AuthorizationCode(f) => map.serialize_entry("authorizationCode", f)?,
            OAuthFlows::ClientCredentials(f) => map.serialize_entry("clientCredentials", f)?,
            OAuthFlows::DeviceCode(f) => map.serialize_entry("deviceCode", f)?,
            OAuthFlows::Implicit(f) => map.serialize_entry("implicit", f)?,
            OAuthFlows::Password(f) => map.serialize_entry("password", f)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for OAuthFlows {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw: HashMap<String, Value> = HashMap::deserialize(deserializer)?;
        if let Some(v) = raw.get("authorizationCode") {
            Ok(OAuthFlows::AuthorizationCode(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("clientCredentials") {
            Ok(OAuthFlows::ClientCredentials(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("deviceCode") {
            Ok(OAuthFlows::DeviceCode(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("implicit") {
            Ok(OAuthFlows::Implicit(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("password") {
            Ok(OAuthFlows::Password(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else {
            Err(serde::de::Error::custom("unknown OAuth flow variant"))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorizationCodeOAuthFlow {
    pub authorization_url: String,
    pub token_url: String,
    pub scopes: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pkce_required: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClientCredentialsOAuthFlow {
    pub token_url: String,
    pub scopes: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceCodeOAuthFlow {
    pub device_authorization_url: String,
    pub token_url: String,
    pub scopes: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImplicitOAuthFlow {
    pub authorization_url: String,
    pub scopes: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PasswordOAuthFlow {
    pub token_url: String,
    pub scopes: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
}

// ---------------------------------------------------------------------------
// Security requirement
// ---------------------------------------------------------------------------

/// A security requirement: map of scheme name → required scopes.
pub type SecurityRequirement = HashMap<String, Vec<String>>;

// ---------------------------------------------------------------------------
// AgentCardSignature
// ---------------------------------------------------------------------------

/// JWS signature for an AgentCard (RFC 7515).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCardSignature {
    pub protected: String,
    pub signature: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<HashMap<String, Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_card_serde() {
        let card = AgentCard {
            name: "Test Agent".to_string(),
            description: "A test agent".to_string(),
            version: "1.0.0".to_string(),
            supported_interfaces: vec![AgentInterface::new("http://localhost:3000", "JSONRPC")],
            capabilities: AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(false),
                extensions: None,
                extended_agent_card: None,
            },
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: vec![AgentSkill {
                id: "echo".to_string(),
                name: "Echo".to_string(),
                description: "Echoes input".to_string(),
                tags: vec!["test".to_string()],
                examples: Some(vec!["hello".to_string()]),
                input_modes: None,
                output_modes: None,
                security_requirements: None,
            }],
            provider: Some(AgentProvider {
                organization: "Test Corp".to_string(),
                url: "https://test.com".to_string(),
            }),
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        };

        let json = serde_json::to_string(&card).unwrap();
        let back: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(card, back);
    }

    #[test]
    fn test_agent_interface_new() {
        let iface = AgentInterface::new("http://localhost:3000", "JSONRPC");
        assert_eq!(iface.url, "http://localhost:3000");
        assert_eq!(iface.protocol_binding, "JSONRPC");
        assert!(!iface.protocol_version.is_empty());
    }

    #[test]
    fn test_agent_interface_new_normalizes_grpc_http_scheme() {
        let iface = AgentInterface::new("http://localhost:50051", TRANSPORT_PROTOCOL_GRPC);
        assert_eq!(iface.url, "localhost:50051");
        assert_eq!(iface.protocol_binding, TRANSPORT_PROTOCOL_GRPC);
    }

    #[test]
    fn test_agent_interface_new_preserves_grpc_https_scheme() {
        let iface = AgentInterface::new("https://localhost:50051", TRANSPORT_PROTOCOL_GRPC);
        assert_eq!(iface.url, "https://localhost:50051");
        assert_eq!(iface.protocol_binding, TRANSPORT_PROTOCOL_GRPC);
    }

    #[test]
    fn test_agent_interface_serde_normalizes_grpc_http_scheme() {
        let iface = AgentInterface {
            url: "http://localhost:50051".to_string(),
            protocol_binding: TRANSPORT_PROTOCOL_GRPC.to_string(),
            protocol_version: crate::VERSION.to_string(),
            tenant: Some("tenant-a".to_string()),
        };

        let json = serde_json::to_string(&iface).unwrap();
        assert!(json.contains("\"url\":\"localhost:50051\""));

        let back: AgentInterface = serde_json::from_str(&json).unwrap();
        assert_eq!(back.url, "localhost:50051");
        assert_eq!(back.protocol_binding, TRANSPORT_PROTOCOL_GRPC);
        assert_eq!(back.tenant.as_deref(), Some("tenant-a"));
    }

    #[test]
    fn test_agent_card_deserialize_null_skills_as_default() {
        let json = serde_json::json!({
            "name": "Test Agent",
            "description": "A test agent",
            "version": "1.0.0",
            "supportedInterfaces": [
                {
                    "url": "http://localhost:3000",
                    "protocolBinding": "JSONRPC",
                    "protocolVersion": crate::VERSION
                }
            ],
            "capabilities": {},
            "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"],
            "skills": null
        });

        let card: AgentCard = serde_json::from_value(json).unwrap();
        assert!(card.skills.is_empty());
    }

    #[test]
    fn test_security_scheme_deserialize_unknown_variant_errors() {
        let err = serde_json::from_value::<SecurityScheme>(serde_json::json!({
            "unknown": {"value": true}
        }))
        .unwrap_err();

        assert!(err.to_string().contains("unknown security scheme variant"));
    }

    #[test]
    fn test_oauth_flows_deserialize_unknown_variant_errors() {
        let err = serde_json::from_value::<OAuthFlows>(serde_json::json!({
            "unknown": {"tokenUrl": "https://example.com/token"}
        }))
        .unwrap_err();

        assert!(err.to_string().contains("unknown OAuth flow variant"));
    }

    #[test]
    fn test_security_scheme_apikey_serde() {
        let ss = SecurityScheme::ApiKey(ApiKeySecurityScheme {
            location: "header".to_string(),
            name: "X-API-Key".to_string(),
            description: None,
        });
        let json = serde_json::to_string(&ss).unwrap();
        assert!(json.contains("apiKeySecurityScheme"));
        let back: SecurityScheme = serde_json::from_str(&json).unwrap();
        assert_eq!(ss, back);
    }

    #[test]
    fn test_security_scheme_httpauth_serde() {
        let ss = SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
            scheme: "Bearer".to_string(),
            description: None,
            bearer_format: Some("JWT".to_string()),
        });
        let json = serde_json::to_string(&ss).unwrap();
        assert!(json.contains("httpAuthSecurityScheme"));
        let back: SecurityScheme = serde_json::from_str(&json).unwrap();
        assert_eq!(ss, back);
    }

    #[test]
    fn test_security_scheme_oauth2_serde() {
        let ss = SecurityScheme::OAuth2(OAuth2SecurityScheme {
            flows: OAuthFlows::ClientCredentials(ClientCredentialsOAuthFlow {
                token_url: "https://auth.example.com/token".to_string(),
                scopes: [("read".to_string(), "Read access".to_string())]
                    .into_iter()
                    .collect(),
                refresh_url: None,
            }),
            description: None,
            oauth2_metadata_url: None,
        });
        let json = serde_json::to_string(&ss).unwrap();
        let back: SecurityScheme = serde_json::from_str(&json).unwrap();
        assert_eq!(ss, back);
    }

    #[test]
    fn test_security_scheme_openidconnect_serde() {
        let ss = SecurityScheme::OpenIdConnect(OpenIdConnectSecurityScheme {
            open_id_connect_url: "https://example.com/.well-known/openid-configuration".to_string(),
            description: None,
        });
        let json = serde_json::to_string(&ss).unwrap();
        let back: SecurityScheme = serde_json::from_str(&json).unwrap();
        assert_eq!(ss, back);
    }

    #[test]
    fn test_security_scheme_mtls_serde() {
        let ss = SecurityScheme::MutualTls(MutualTlsSecurityScheme {
            description: Some("mTLS auth".to_string()),
        });
        let json = serde_json::to_string(&ss).unwrap();
        let back: SecurityScheme = serde_json::from_str(&json).unwrap();
        assert_eq!(ss, back);
    }

    #[test]
    fn test_oauth_flows_all_variants() {
        let flows = [
            OAuthFlows::AuthorizationCode(AuthorizationCodeOAuthFlow {
                authorization_url: "https://auth.example.com/authorize".to_string(),
                token_url: "https://auth.example.com/token".to_string(),
                scopes: HashMap::new(),
                refresh_url: None,
                pkce_required: Some(true),
            }),
            OAuthFlows::DeviceCode(DeviceCodeOAuthFlow {
                device_authorization_url: "https://auth.example.com/device".to_string(),
                token_url: "https://auth.example.com/token".to_string(),
                scopes: HashMap::new(),
                refresh_url: None,
            }),
            OAuthFlows::Implicit(ImplicitOAuthFlow {
                authorization_url: "https://auth.example.com/authorize".to_string(),
                scopes: HashMap::new(),
                refresh_url: None,
            }),
            OAuthFlows::Password(PasswordOAuthFlow {
                token_url: "https://auth.example.com/token".to_string(),
                scopes: HashMap::new(),
                refresh_url: None,
            }),
        ];
        for flow in flows {
            let json = serde_json::to_string(&flow).unwrap();
            let back: OAuthFlows = serde_json::from_str(&json).unwrap();
            assert_eq!(flow, back);
        }
    }

    #[test]
    fn test_agent_capabilities_default() {
        let caps = AgentCapabilities::default();
        assert_eq!(caps.streaming, None);
        assert_eq!(caps.push_notifications, None);
        assert_eq!(caps.extensions, None);
        assert_eq!(caps.extended_agent_card, None);
    }

    #[test]
    fn test_agent_card_with_security_schemes() {
        let mut schemes = HashMap::new();
        schemes.insert(
            "bearer".to_string(),
            SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
                scheme: "Bearer".to_string(),
                description: None,
                bearer_format: None,
            }),
        );
        let card = AgentCard {
            name: "Secure Agent".to_string(),
            description: "Agent with auth".to_string(),
            version: "1.0.0".to_string(),
            supported_interfaces: vec![],
            capabilities: AgentCapabilities::default(),
            default_input_modes: vec![],
            default_output_modes: vec![],
            skills: vec![],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: Some(schemes),
            security_requirements: Some(vec![
                [("bearer".to_string(), vec![])].into_iter().collect(),
            ]),
            signatures: None,
        };
        let json = serde_json::to_string(&card).unwrap();
        let back: AgentCard = serde_json::from_str(&json).unwrap();
        assert_eq!(card, back);
    }

    #[test]
    fn test_agent_card_deserializes_null_skills_as_empty() {
        let card: AgentCard = serde_json::from_str(
            r#"{
                "name": "Test Agent",
                "description": "A test agent",
                "version": "1.0.0",
                "supportedInterfaces": [
                    {
                        "url": "http://localhost:3000",
                        "protocolBinding": "JSONRPC",
                        "protocolVersion": "1.0"
                    }
                ],
                "capabilities": {
                    "streaming": true
                },
                "defaultInputModes": ["text/plain"],
                "defaultOutputModes": ["text/plain"],
                "skills": null
            }"#,
        )
        .unwrap();

        assert!(card.skills.is_empty());
    }

    #[test]
    fn test_agent_card_deserializes_missing_skills_as_empty() {
        let card: AgentCard = serde_json::from_str(
            r#"{
                "name": "Test Agent",
                "description": "A test agent",
                "version": "1.0.0",
                "supportedInterfaces": [
                    {
                        "url": "http://localhost:3000",
                        "protocolBinding": "JSONRPC",
                        "protocolVersion": "1.0"
                    }
                ],
                "capabilities": {
                    "streaming": true
                },
                "defaultInputModes": ["text/plain"],
                "defaultOutputModes": ["text/plain"]
            }"#,
        )
        .unwrap();

        assert!(card.skills.is_empty());
    }
}
