// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use a2a::jsonrpc::{JsonRpcError, JsonRpcId};

/// Transport Identifier of this binding, used as the Agent Card
/// `protocolBinding` value (spec Sections 1 and 10).
pub const TRANSPORT_PROTOCOL_WEBSOCKET: &str =
    "https://a2a-protocol.org/bindings/experimental-websocket/v1";

/// WebSocket sub-protocol negotiated via `Sec-WebSocket-Protocol` (spec Sections
/// 1 and 2.1).
pub const SUBPROTOCOL: &str = "a2a.v1";

/// The JSON-RPC protocol version string. Every message carries this value in
/// its `jsonrpc` member (spec Section 3).
pub const JSONRPC_VERSION: &str = "2.0";

/// Recommended maximum message size (1 MiB) per the binding spec (Section 3.6).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1 << 20;

/// JSON-RPC code shared by every binding-specific error (spec Section 7.3). The
/// `reason` of the accompanying `ErrorInfo` tells the errors apart; see
/// [`binding_errors`].
pub const SERVER_ERROR_CODE: i32 = -32000;

fn default_jsonrpc() -> String {
    JSONRPC_VERSION.to_string()
}

/// Method names used by this binding: the A2A operations (PascalCase, matching
/// JSON-RPC and gRPC) and the binding control methods under the reserved `ws/`
/// prefix (spec Section 5).
pub mod methods {
    /// Prefix reserved for the binding control methods.
    pub const CONTROL_PREFIX: &str = "ws/";

    /// Server → client notification that terminates a stream (Section 3.4).
    pub const STREAM_END: &str = "ws/StreamEnd";
    /// Client → server notification that stops a stream without cancelling its
    /// task (Section 3.4).
    pub const CANCEL_STREAM: &str = "ws/CancelStream";
    /// Optional server → client notification warning that the connection's
    /// credentials are about to expire (Section 9.5).
    pub const REAUTHENTICATION_REQUIRED: &str = "ws/ReauthenticationRequired";
    /// Optional client → server request presenting credentials in-band
    /// (Section 9.3).
    pub const AUTHENTICATE: &str = "ws/Authenticate";

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

    /// Whether `method` is one of the A2A operations. Binding control methods
    /// are not A2A operations and are not included.
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

    pub fn is_control(method: &str) -> bool {
        method.starts_with(CONTROL_PREFIX)
    }
}

/// WebSocket close codes used by this binding (spec Section 2.3).
pub mod close_codes {
    pub const NORMAL_CLOSURE: u16 = 1000;
    pub const GOING_AWAY: u16 = 1001;
    pub const PROTOCOL_ERROR: u16 = 1002;
    pub const UNSUPPORTED_DATA: u16 = 1003;
    pub const INVALID_PAYLOAD: u16 = 1007;
    pub const POLICY_VIOLATION: u16 = 1008;
    pub const MESSAGE_TOO_BIG: u16 = 1009;
    pub const INTERNAL_ERROR: u16 = 1011;
    pub const TRY_AGAIN_LATER: u16 = 1013;
    pub const A2A_PROTOCOL_ERROR: u16 = 4000;
    pub const AUTHENTICATION_REQUIRED: u16 = 4001;
    pub const VERSION_NOT_SUPPORTED: u16 = 4002;
}

/// `ErrorInfo` reasons of the binding-specific errors, all sent with code
/// [`SERVER_ERROR_CODE`] and domain `a2a-protocol.org` (spec Section 7.3).
pub mod binding_errors {
    pub const UNAUTHENTICATED: &str = "WS_UNAUTHENTICATED";
    pub const CREDENTIALS_EXPIRED: &str = "WS_CREDENTIALS_EXPIRED";
    pub const PERMISSION_DENIED: &str = "WS_PERMISSION_DENIED";
    pub const RATE_LIMITED: &str = "WS_RATE_LIMITED";
    pub const TOO_MANY_IN_FLIGHT: &str = "WS_TOO_MANY_IN_FLIGHT";

    pub fn is_binding_reason(reason: &str) -> bool {
        matches!(
            reason,
            UNAUTHENTICATED
                | CREDENTIALS_EXPIRED
                | PERMISSION_DENIED
                | RATE_LIMITED
                | TOO_MANY_IN_FLIGHT
        )
    }
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

/// A client → server JSON-RPC 2.0 object: an A2A request, a `ws/Authenticate`
/// request, or a `ws/CancelStream` notification (spec Sections 3.1 and 3.4).
/// Each client text message carries exactly one of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsRequestEnvelope {
    /// **MUST** be exactly `"2.0"` (spec Section 3).
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,

    /// Request identifier. Absent for notifications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonRpcId>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,

    /// Method-specific request parameters serialized as ProtoJSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,

    /// **Binding extension.** Per-request service parameters (spec Section 4.2).
    #[serde(
        default,
        rename = "serviceParams",
        skip_serializing_if = "Option::is_none"
    )]
    pub service_params: Option<HashMap<String, String>>,
}

impl Default for WsRequestEnvelope {
    fn default() -> Self {
        WsRequestEnvelope {
            jsonrpc: default_jsonrpc(),
            id: None,
            method: None,
            params: None,
            service_params: None,
        }
    }
}

impl WsRequestEnvelope {
    /// Build a `ws/CancelStream` notification for the stream opened by request
    /// `id` (spec Section 3.4).
    pub fn cancel_stream(id: JsonRpcId) -> Self {
        WsRequestEnvelope {
            method: Some(methods::CANCEL_STREAM.to_string()),
            params: Some(serde_json::to_value(StreamControlParams { id }).unwrap_or_default()),
            ..Default::default()
        }
    }

    pub fn is_notification(&self) -> bool {
        self.id.is_none()
    }
}

/// A server → client JSON-RPC 2.0 object: a response or stream event
/// (`result`), an error, or a binding notification (`method` and `params`)
/// such as `ws/StreamEnd` (spec Sections 3.2–3.4).
///
/// `id` is `Some(JsonRpcId::Null)` when the request id could not be parsed
/// (Section 3.3) and `None` for notifications.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WsResponseEnvelope {
    /// **MUST** be exactly `"2.0"` (spec Section 3).
    #[serde(default = "default_jsonrpc")]
    pub jsonrpc: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<JsonRpcId>,

    /// Unary result **or** a single streaming event (`StreamResponse`); both
    /// use `result` per Section 3.4.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,

    /// Notification method, e.g. `ws/StreamEnd`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,

    /// Notification parameters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Default for WsResponseEnvelope {
    fn default() -> Self {
        WsResponseEnvelope {
            jsonrpc: default_jsonrpc(),
            id: None,
            result: None,
            error: None,
            method: None,
            params: None,
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

    /// Build a single streaming `result` event (spec Section 3.4). This is the
    /// same shape as a unary result; the client distinguishes them by which
    /// registry the request `id` is tracked in.
    pub fn stream_chunk(id: JsonRpcId, value: Value) -> Self {
        WsResponseEnvelope::result(id, value)
    }

    /// Build a `ws/StreamEnd` notification for the stream opened by request
    /// `id` (spec Section 3.4).
    pub fn stream_end(id: JsonRpcId) -> Self {
        WsResponseEnvelope::notification(methods::STREAM_END, StreamControlParams { id })
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

    /// Build a `ws/ReauthenticationRequired` notification. `expires_at` is a
    /// ProtoJSON `Timestamp` (spec Section 9.5).
    pub fn reauthentication_required(expires_at: impl Into<String>) -> Self {
        WsResponseEnvelope::notification(
            methods::REAUTHENTICATION_REQUIRED,
            ReauthenticationRequiredParams {
                expires_at: expires_at.into(),
            },
        )
    }

    fn notification(method: &str, params: impl Serialize) -> Self {
        WsResponseEnvelope {
            method: Some(method.to_string()),
            params: Some(serde_json::to_value(params).unwrap_or_default()),
            ..Default::default()
        }
    }

    pub fn is_notification(&self) -> bool {
        self.method.is_some() && self.id.is_none()
    }
}

/// `params` of `ws/StreamEnd` and `ws/CancelStream`: the `id` of the request
/// that opened the stream (spec Section 3.4).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamControlParams {
    pub id: JsonRpcId,
}

/// `params` of `ws/ReauthenticationRequired` (spec Section 9.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReauthenticationRequiredParams {
    #[serde(rename = "expiresAt")]
    pub expires_at: String,
}

/// Convert a flat per-request `serviceParams` map (as carried on the wire)
/// into the multi-valued `ServiceParams` representation used internally.
///
/// Keys are lowercased to match the handshake headers, which arrive already
/// normalized by `HeaderMap`. Fails when two keys differ only in case, which
/// the spec requires the server to reject (Section 4.2).
pub fn service_params_from_envelope(
    map: &HashMap<String, String>,
) -> Result<HashMap<String, Vec<String>>, String> {
    let mut seen = HashSet::new();
    let mut out = HashMap::with_capacity(map.len());
    for (key, value) in map {
        let key = key.to_ascii_lowercase();
        if !seen.insert(key.clone()) {
            return Err(format!(
                "serviceParams contains keys that differ only in case: '{key}'"
            ));
        }
        out.insert(key, vec![value.clone()]);
    }
    Ok(out)
}

/// Convert the multi-valued `ServiceParams` representation into the flat
/// string-string map used in the WebSocket envelope. Multiple values for a
/// single key are comma-separated (spec Section 4).
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
    fn transport_identifier_is_the_binding_uri() {
        assert_eq!(
            TRANSPORT_PROTOCOL_WEBSOCKET,
            "https://a2a-protocol.org/bindings/experimental-websocket/v1"
        );
    }

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
            assert!(!methods::is_control(m), "{m} is not a control method");
        }
        assert!(!methods::is_known("MessageSend"));
        assert!(!methods::is_known(""));
    }

    #[test]
    fn control_methods_use_the_reserved_prefix_and_are_not_a2a_operations() {
        for m in [
            methods::STREAM_END,
            methods::CANCEL_STREAM,
            methods::REAUTHENTICATION_REQUIRED,
            methods::AUTHENTICATE,
        ] {
            assert!(methods::is_control(m), "{m} should use the ws/ prefix");
            assert!(!methods::is_known(m), "{m} is not an A2A operation");
        }
    }

    #[test]
    fn request_envelope_serializes_as_jsonrpc_with_service_params() {
        let req = WsRequestEnvelope {
            id: Some("req-1".into()),
            method: Some(methods::SEND_MESSAGE.into()),
            params: Some(serde_json::json!({"message": {"messageId": "m1"}})),
            service_params: Some(HashMap::from([(
                "a2a-extensions".into(),
                "https://example.com/ext".into(),
            )])),
            ..Default::default()
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["method"], "SendMessage");
        assert_eq!(
            value["serviceParams"]["a2a-extensions"],
            "https://example.com/ext"
        );
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
    fn cancel_stream_is_a_notification_referencing_the_stream_id() {
        let value = serde_json::to_value(WsRequestEnvelope::cancel_stream("req-2".into())).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "ws/CancelStream",
                "params": { "id": "req-2" }
            })
        );
    }

    #[test]
    fn response_envelope_result_omits_other_fields() {
        let resp = WsResponseEnvelope::result("req-1".into(), serde_json::json!({"ok": true}));
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["result"]["ok"], true);
        assert!(value.get("error").is_none());
        assert!(value.get("method").is_none());
    }

    #[test]
    fn stream_end_is_a_notification_referencing_the_stream_id() {
        let chunk =
            WsResponseEnvelope::stream_chunk("req-2".into(), serde_json::json!({"task": {}}));
        let value = serde_json::to_value(&chunk).unwrap();
        assert_eq!(value["id"], "req-2");
        assert!(value["result"].is_object());

        let end = WsResponseEnvelope::stream_end(JsonRpcId::Number(7));
        assert!(end.is_notification());
        assert_eq!(
            serde_json::to_value(&end).unwrap(),
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "ws/StreamEnd",
                "params": { "id": 7 }
            })
        );
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
    fn reauthentication_required_carries_expires_at_and_no_id() {
        let frame = WsResponseEnvelope::reauthentication_required("2026-11-10T10:00:00.000Z");
        assert_eq!(
            serde_json::to_value(&frame).unwrap(),
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "ws/ReauthenticationRequired",
                "params": { "expiresAt": "2026-11-10T10:00:00.000Z" }
            })
        );
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

        let restored = service_params_from_envelope(&envelope).unwrap();
        assert_eq!(restored.get("a2a-version"), Some(&vec!["1.0".to_string()]));
        assert_eq!(restored.get("x-multi"), Some(&vec!["a, b".to_string()]));
    }

    #[test]
    fn service_params_from_envelope_lowercases_keys() {
        let map = HashMap::from([("X-Trace".to_string(), "t".to_string())]);
        let params = service_params_from_envelope(&map).unwrap();
        assert_eq!(params.get("x-trace"), Some(&vec!["t".to_string()]));
    }

    #[test]
    fn service_params_from_envelope_rejects_keys_differing_only_in_case() {
        let map = HashMap::from([
            ("X-Trace".to_string(), "a".to_string()),
            ("x-trace".to_string(), "b".to_string()),
        ]);
        assert!(service_params_from_envelope(&map).is_err());
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
