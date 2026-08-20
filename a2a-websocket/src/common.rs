// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use a2a::jsonrpc::{JsonRpcError, JsonRpcId};

/// Transport protocol identifier used in `AgentInterface::protocol_binding`.
///
/// Matches the `protocolBinding` value (`WEBSOCKET`) defined in the A2A
/// WebSocket binding specification (Section 1). The binding transports
/// JSON-RPC 2.0 messages, but the identifier itself is simply `WEBSOCKET`.
pub use a2a::TRANSPORT_PROTOCOL_WEBSOCKET;

/// WebSocket sub-protocol negotiated via `Sec-WebSocket-Protocol` (spec Section
/// 1 / 2.1).
pub const SUBPROTOCOL: &str = "a2a.v1";

/// The JSON-RPC protocol version string. Every frame carries this value in its
/// `jsonrpc` member (spec Section 3).
pub const JSONRPC_VERSION: &str = "2.0";

/// Recommended maximum frame size (1 MiB) per the binding spec (Section 3.6).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1 << 20;

/// Implementation-defined JSON-RPC server-error code used for binding-specific
/// server errors that occur *after* the handshake — for example an expired or
/// revoked authentication token (spec Section 7 and Section 9.3).
pub const SERVER_ERROR_CODE: i32 = -32000;

/// Name of the server-originated control frame requesting reauthentication
/// (spec Section 9.3.2).
pub const CONTROL_REAUTH_REQUIRED: &str = "ReauthenticationRequired";

fn default_jsonrpc() -> String {
    JSONRPC_VERSION.to_string()
}

/// A2A method names supported by this binding (PascalCase, matching JSON-RPC
/// and gRPC).
pub mod methods {
    /// Binding-specific in-band token refresh method (spec Section 9.3.3).
    ///
    /// This is **not** part of the core A2A method inventory and is therefore
    /// intentionally excluded from [`is_known`]; it is handled separately by
    /// the connection loop only when the server advertises support for it.
    pub const AUTHENTICATE: &str = "Authenticate";

    pub const SEND_MESSAGE: &str = "SendMessage";
    pub const SEND_STREAMING_MESSAGE: &str = "SendStreamingMessage";
    pub const GET_TASK: &str = "GetTask";
    pub const LIST_TASKS: &str = "ListTasks";
    pub const CANCEL_TASK: &str = "CancelTask";
    pub const SUBSCRIBE_TO_TASK: &str = "SubscribeToTask";
    pub const CREATE_PUSH_CONFIG: &str = "CreateTaskPushNotificationConfig";
    pub const GET_PUSH_CONFIG: &str = "GetTaskPushNotificationConfig";
    pub const LIST_PUSH_CONFIGS: &str = "ListTaskPushNotificationConfigs";
    pub const DELETE_PUSH_CONFIG: &str = "DeleteTaskPushNotificationConfig";
    pub const GET_EXTENDED_AGENT_CARD: &str = "GetExtendedAgentCard";

    pub fn is_streaming(method: &str) -> bool {
        matches!(method, SEND_STREAMING_MESSAGE | SUBSCRIBE_TO_TASK)
    }

    pub fn is_known(method: &str) -> bool {
        matches!(
            method,
            SEND_MESSAGE
                | SEND_STREAMING_MESSAGE
                | GET_TASK
                | LIST_TASKS
                | CANCEL_TASK
                | SUBSCRIBE_TO_TASK
                | CREATE_PUSH_CONFIG
                | GET_PUSH_CONFIG
                | LIST_PUSH_CONFIGS
                | DELETE_PUSH_CONFIG
                | GET_EXTENDED_AGENT_CARD
        )
    }
}

/// WebSocket close codes used by this binding (spec Section 2.3).
pub mod close_codes {
    pub const NORMAL_CLOSURE: u16 = 1000;
    pub const GOING_AWAY: u16 = 1001;
    pub const PROTOCOL_ERROR: u16 = 1002;
    pub const UNSUPPORTED_DATA: u16 = 1003;
    pub const POLICY_VIOLATION: u16 = 1008;
    pub const MESSAGE_TOO_BIG: u16 = 1009;
    pub const INTERNAL_ERROR: u16 = 1011;
    pub const TRY_AGAIN_LATER: u16 = 1013;
    pub const A2A_PROTOCOL_ERROR: u16 = 4000;
    pub const AUTHENTICATION_REQUIRED: u16 = 4001;
    pub const VERSION_NOT_SUPPORTED: u16 = 4002;
}

/// Canonical `data[].reason` strings emitted in JSON-RPC error objects,
/// mirroring the A2A error type names in the specification (Section 7.2).
///
/// These are informational reason strings carried in `error.data`; the
/// authoritative machine-readable value is the numeric `error.code`.
pub mod error_types {
    pub const JSON_PARSE: &str = "JSONParseError";
    pub const INVALID_REQUEST: &str = "InvalidRequestError";
    pub const METHOD_NOT_FOUND: &str = "MethodNotFoundError";
    pub const INVALID_PARAMS: &str = "InvalidParamsError";
    pub const INTERNAL: &str = "InternalError";
    pub const TASK_NOT_FOUND: &str = "TaskNotFoundError";
    pub const TASK_NOT_CANCELABLE: &str = "TaskNotCancelableError";
    pub const PUSH_NOTIFICATION_NOT_SUPPORTED: &str = "PushNotificationNotSupportedError";
    pub const UNSUPPORTED_OPERATION: &str = "UnsupportedOperationError";
    pub const CONTENT_TYPE_NOT_SUPPORTED: &str = "ContentTypeNotSupportedError";
    pub const INVALID_AGENT_RESPONSE: &str = "InvalidAgentResponseError";
    pub const EXTENDED_CARD_NOT_CONFIGURED: &str = "ExtendedAgentCardNotConfiguredError";
    pub const EXTENSION_SUPPORT_REQUIRED: &str = "ExtensionSupportRequiredError";
    pub const VERSION_NOT_SUPPORTED: &str = "VersionNotSupportedError";
}

/// Build a stable string key for a [`JsonRpcId`], used to correlate responses
/// to requests in the per-connection registries. The key preserves the
/// string/number distinction so that string `"1"` and number `1` never
/// collide.
pub fn id_key(id: &JsonRpcId) -> String {
    match id {
        JsonRpcId::String(s) => format!("s:{s}"),
        JsonRpcId::Number(n) => format!("n:{n}"),
        JsonRpcId::Null => "null".to_string(),
    }
}

/// A2A WebSocket request envelope — a JSON-RPC 2.0 request object plus the A2A
/// extension members (`serviceParams`, `cancelStream`). Each client text frame
/// carries exactly one of these (spec Sections 3.1, 4.2, 8.5).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsRequestEnvelope {
    /// **MUST** be exactly `"2.0"` (spec Section 3).
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,

    /// A unique request identifier (string or number). Absent only for
    /// malformed frames; the binding does not use JSON-RPC notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonRpcId>,

    /// The A2A method name. Optional only for control messages such as a
    /// `cancelStream` frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,

    /// Method-specific request parameters serialized as ProtoJSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,

    /// **A2A extension.** Per-request service parameters (flat string map).
    #[serde(
        default,
        rename = "serviceParams",
        skip_serializing_if = "Option::is_none"
    )]
    pub service_params: Option<HashMap<String, String>>,

    /// **A2A extension.** Stream cancellation control flag (spec Section 8.5).
    #[serde(
        default,
        rename = "cancelStream",
        skip_serializing_if = "Option::is_none"
    )]
    pub cancel_stream: Option<bool>,
}

impl Default for WsRequestEnvelope {
    fn default() -> Self {
        WsRequestEnvelope {
            jsonrpc: default_jsonrpc(),
            id: None,
            method: None,
            params: None,
            service_params: None,
            cancel_stream: None,
        }
    }
}

/// A2A WebSocket response envelope — a JSON-RPC 2.0 response object plus the
/// A2A extension members. Each server text frame is one of: a unary/stream
/// `result`, an `error`, the `streamEnd` sentinel, or a server-originated
/// control frame (spec Sections 3.2–3.4, 8, 9.3.2).
///
/// `id` is `Some(JsonRpcId::Null)` when the request id could not be parsed
/// (Section 3.3) and `None` only for server-originated control frames that
/// carry no request id (Section 9.3.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsResponseEnvelope {
    /// **MUST** be exactly `"2.0"` (spec Section 3).
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonRpcId>,

    /// Unary result **or** a single streaming chunk (`StreamResponse`); both
    /// use `result` per Section 3.4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,

    /// **A2A extension.** Stream terminator (spec Section 3.4).
    #[serde(default, rename = "streamEnd", skip_serializing_if = "Option::is_none")]
    pub stream_end: Option<bool>,

    /// **A2A extension.** Server-originated control frame name, e.g.
    /// `ReauthenticationRequired` (spec Section 9.3.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control: Option<String>,

    /// Human-readable reason accompanying a control frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Suggested client back-off before reconnecting, in milliseconds.
    #[serde(
        default,
        rename = "retryAfterMs",
        skip_serializing_if = "Option::is_none"
    )]
    pub retry_after_ms: Option<u64>,
}

impl Default for WsResponseEnvelope {
    fn default() -> Self {
        WsResponseEnvelope {
            jsonrpc: default_jsonrpc(),
            id: None,
            result: None,
            error: None,
            stream_end: None,
            control: None,
            reason: None,
            retry_after_ms: None,
        }
    }
}

impl WsResponseEnvelope {
    /// Build a successful unary response (spec Section 3.2).
    pub fn result(id: JsonRpcId, value: Value) -> Self {
        WsResponseEnvelope {
            id: Some(id),
            result: Some(value),
            ..Default::default()
        }
    }

    /// Build a single streaming `result` chunk (spec Section 3.4). This is the
    /// same shape as a unary result; the client distinguishes them by which
    /// registry the request `id` is tracked in.
    pub fn stream_chunk(id: JsonRpcId, value: Value) -> Self {
        WsResponseEnvelope::result(id, value)
    }

    /// Build a `streamEnd: true` sentinel response (spec Section 3.4).
    pub fn stream_end(id: JsonRpcId) -> Self {
        WsResponseEnvelope {
            id: Some(id),
            stream_end: Some(true),
            ..Default::default()
        }
    }

    /// Build a JSON-RPC error response for the given request id, or
    /// `Some(JsonRpcId::Null)` if the id could not be parsed (spec Section 3.3).
    pub fn error(id: Option<JsonRpcId>, error: JsonRpcError) -> Self {
        WsResponseEnvelope {
            id,
            error: Some(error),
            ..Default::default()
        }
    }

    /// Build a `ReauthenticationRequired` control frame (spec Section 9.3.2).
    /// Control frames carry no request `id`.
    pub fn reauth_required(reason: impl Into<String>, retry_after_ms: u64) -> Self {
        WsResponseEnvelope {
            control: Some(CONTROL_REAUTH_REQUIRED.to_string()),
            reason: Some(reason.into()),
            retry_after_ms: Some(retry_after_ms),
            ..Default::default()
        }
    }
}

/// Convert a flat per-request `serviceParams` map (as carried on the wire)
/// into the multi-valued `a2a_client::transport::ServiceParams` representation
/// used internally.
pub fn service_params_from_envelope(map: &HashMap<String, String>) -> HashMap<String, Vec<String>> {
    // Keys are lowercased to match the handshake headers, which arrive already
    // normalized by `HeaderMap`. Without this, `X-Tenant` in an envelope and
    // `x-tenant` from the handshake would land in the map as two unrelated
    // entries and a handler could read either one.
    map.iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), vec![v.clone()]))
        .collect()
}

/// Convert the multi-valued `ServiceParams` representation into the flat
/// string-string map used in the WebSocket envelope. Multiple values for a
/// single key are joined with `", "` per RFC 7230 / the WebSocket binding
/// spec recommendation.
pub fn service_params_to_envelope(
    map: &HashMap<String, Vec<String>>,
) -> Option<HashMap<String, String>> {
    if map.is_empty() {
        return None;
    }
    Some(
        map.iter()
            .map(|(k, values)| (k.clone(), values.join(", ")))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn methods_is_streaming_distinguishes_streaming_calls() {
        assert!(methods::is_streaming(methods::SEND_STREAMING_MESSAGE));
        assert!(methods::is_streaming(methods::SUBSCRIBE_TO_TASK));
        assert!(!methods::is_streaming(methods::SEND_MESSAGE));
        assert!(!methods::is_streaming(methods::GET_TASK));
        assert!(!methods::is_streaming("unknown"));
    }

    #[test]
    fn methods_is_known_recognises_all_inventory_methods() {
        for m in [
            methods::SEND_MESSAGE,
            methods::SEND_STREAMING_MESSAGE,
            methods::GET_TASK,
            methods::LIST_TASKS,
            methods::CANCEL_TASK,
            methods::SUBSCRIBE_TO_TASK,
            methods::CREATE_PUSH_CONFIG,
            methods::GET_PUSH_CONFIG,
            methods::LIST_PUSH_CONFIGS,
            methods::DELETE_PUSH_CONFIG,
            methods::GET_EXTENDED_AGENT_CARD,
        ] {
            assert!(methods::is_known(m), "{m} should be known");
        }
        // `Authenticate` is a binding-specific method, not part of the core
        // inventory, so it must not be reported as a known A2A method.
        assert!(!methods::is_known(methods::AUTHENTICATE));
        assert!(!methods::is_known("MessageSend"));
        assert!(!methods::is_known(""));
    }

    #[test]
    fn request_envelope_serializes_as_jsonrpc_with_extensions() {
        let req = WsRequestEnvelope {
            id: Some("req-1".into()),
            method: Some(methods::SEND_MESSAGE.into()),
            params: Some(serde_json::json!({"message": {"messageId": "m1"}})),
            service_params: Some(HashMap::from([("a2a-version".into(), "1.0".into())])),
            ..Default::default()
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["method"], "SendMessage");
        assert_eq!(value["serviceParams"]["a2a-version"], "1.0");
        assert!(value.get("cancelStream").is_none());
    }

    #[test]
    fn request_envelope_accepts_numeric_id() {
        let raw = r#"{"jsonrpc":"2.0","id":42,"method":"GetTask","params":{"id":"t1"}}"#;
        let req: WsRequestEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(req.id, Some(JsonRpcId::Number(42)));
        assert_eq!(id_key(req.id.as_ref().unwrap()), "n:42");
    }

    #[test]
    fn request_envelope_defaults_jsonrpc_when_absent() {
        // Even though the spec requires `jsonrpc`, tolerate its absence on parse
        // and default it; the server validates the value explicitly.
        let raw = r#"{"id":"x","method":"GetTask"}"#;
        let req: WsRequestEnvelope = serde_json::from_str(raw).unwrap();
        assert_eq!(req.jsonrpc, "2.0");
    }

    #[test]
    fn request_envelope_with_cancel_stream_only_omits_method() {
        let req = WsRequestEnvelope {
            id: Some("req-2".into()),
            cancel_stream: Some(true),
            ..Default::default()
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["cancelStream"], true);
        assert!(value.get("method").is_none());
        assert!(value.get("params").is_none());
    }

    #[test]
    fn response_envelope_result_omits_other_fields() {
        let resp = WsResponseEnvelope::result("req-1".into(), serde_json::json!({"ok": true}));
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["result"]["ok"], true);
        assert!(value.get("error").is_none());
        assert!(value.get("streamEnd").is_none());
    }

    #[test]
    fn response_envelope_stream_chunk_and_end_serialize_correctly() {
        let chunk =
            WsResponseEnvelope::stream_chunk("req-2".into(), serde_json::json!({"task": {}}));
        let value = serde_json::to_value(&chunk).unwrap();
        assert_eq!(value["id"], "req-2");
        assert!(value["result"].is_object());

        let end = WsResponseEnvelope::stream_end("req-2".into());
        let value = serde_json::to_value(&end).unwrap();
        assert_eq!(value["streamEnd"], true);
    }

    #[test]
    fn response_envelope_error_with_null_id_serializes_id_field() {
        let resp = WsResponseEnvelope::error(
            Some(JsonRpcId::Null),
            JsonRpcError {
                code: a2a::error_code::PARSE_ERROR,
                message: "bad json".to_string(),
                data: None,
            },
        );
        let value = serde_json::to_value(&resp).unwrap();
        assert!(value["id"].is_null());
        assert_eq!(value["error"]["code"], a2a::error_code::PARSE_ERROR);
    }

    #[test]
    fn reauth_required_control_frame_has_no_id() {
        let frame = WsResponseEnvelope::reauth_required("Token expiring soon", 0);
        let value = serde_json::to_value(&frame).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["control"], CONTROL_REAUTH_REQUIRED);
        assert_eq!(value["reason"], "Token expiring soon");
        assert_eq!(value["retryAfterMs"], 0);
        assert!(value.get("id").is_none());
    }

    #[test]
    fn service_params_envelope_round_trip_preserves_key_value_pairs() {
        let mut map = HashMap::new();
        map.insert("a2a-version".to_string(), vec!["1.0".to_string()]);
        map.insert(
            "x-multi".to_string(),
            vec!["a".to_string(), "b".to_string()],
        );

        let envelope = service_params_to_envelope(&map).unwrap();
        assert_eq!(envelope.get("a2a-version"), Some(&"1.0".to_string()));
        assert_eq!(envelope.get("x-multi"), Some(&"a, b".to_string()));

        let restored = service_params_from_envelope(&envelope);
        assert_eq!(restored.get("a2a-version"), Some(&vec!["1.0".to_string()]));
        assert_eq!(restored.get("x-multi"), Some(&vec!["a, b".to_string()]));
    }

    #[test]
    fn service_params_to_envelope_returns_none_for_empty_map() {
        let map: HashMap<String, Vec<String>> = HashMap::new();
        assert!(service_params_to_envelope(&map).is_none());
    }

    #[test]
    fn id_key_distinguishes_string_and_number() {
        assert_eq!(id_key(&JsonRpcId::String("1".into())), "s:1");
        assert_eq!(id_key(&JsonRpcId::Number(1)), "n:1");
        assert_ne!(
            id_key(&JsonRpcId::String("1".into())),
            id_key(&JsonRpcId::Number(1))
        );
    }
}
