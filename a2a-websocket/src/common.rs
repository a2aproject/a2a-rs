// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Transport protocol identifier used in `AgentInterface::protocol_binding`.
///
/// Matches the `protocolBinding` value defined in the A2A WebSocket binding
/// specification (Section 1).
pub const TRANSPORT_PROTOCOL_WEBSOCKET: &str = "WEBSOCKET";

/// WebSocket sub-protocol negotiated via `Sec-WebSocket-Protocol`.
pub const SUBPROTOCOL: &str = "a2a.v1";

/// Recommended maximum frame size (1 MiB) per the binding spec.
pub const DEFAULT_MAX_FRAME_BYTES: usize = 1 << 20;

/// A2A method names supported by this binding (PascalCase, matching JSON-RPC
/// and gRPC).
pub mod methods {
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

/// Canonical A2A error type strings emitted in WebSocket error responses.
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

/// WebSocket close codes used by this binding.
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

/// A2A WebSocket request envelope. Each text frame sent by the client carries
/// exactly one of these.
///
/// See spec Section 3.1 (request) and Section 8.5 (`cancelStream` control
/// message).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WsRequestEnvelope {
    /// A unique request identifier provided by the client.
    pub id: String,

    /// The A2A method name. Optional only for control messages such as
    /// `cancelStream`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,

    /// Method-specific request parameters serialized as ProtoJSON.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,

    /// Per-request service parameters (flat string map).
    #[serde(
        default,
        rename = "serviceParams",
        skip_serializing_if = "Option::is_none"
    )]
    pub service_params: Option<HashMap<String, String>>,

    /// Stream cancellation control flag (Section 8.5 of the spec).
    #[serde(
        default,
        rename = "cancelStream",
        skip_serializing_if = "Option::is_none"
    )]
    pub cancel_stream: Option<bool>,
}

/// A2A WebSocket response envelope. Each text frame sent by the server is one
/// of: a unary `result`, an `error`, a stream `event`, or the `streamEnd`
/// sentinel.
///
/// `id` is `null` only when the request id could not be parsed (Section 3.3
/// of the spec).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WsResponseEnvelope {
    pub id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<WsErrorObject>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<Value>,

    #[serde(
        default,
        rename = "streamEnd",
        skip_serializing_if = "Option::is_none"
    )]
    pub stream_end: Option<bool>,
}

/// Error object embedded in a WebSocket error response.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WsErrorObject {
    /// Canonical A2A error type name (see `error_types`).
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<Value>>,
}

impl WsResponseEnvelope {
    /// Build a successful unary response.
    pub fn result(id: impl Into<String>, value: Value) -> Self {
        WsResponseEnvelope {
            id: Some(id.into()),
            result: Some(value),
            ..Default::default()
        }
    }

    /// Build a stream `event` response.
    pub fn event(id: impl Into<String>, event: Value) -> Self {
        WsResponseEnvelope {
            id: Some(id.into()),
            event: Some(event),
            ..Default::default()
        }
    }

    /// Build a `streamEnd: true` sentinel response.
    pub fn stream_end(id: impl Into<String>) -> Self {
        WsResponseEnvelope {
            id: Some(id.into()),
            stream_end: Some(true),
            ..Default::default()
        }
    }

    /// Build an error response for the given request id (or `None` if the id
    /// could not be parsed).
    pub fn error(id: Option<String>, error: WsErrorObject) -> Self {
        WsResponseEnvelope {
            id,
            error: Some(error),
            ..Default::default()
        }
    }
}

/// Convert a flat per-request `serviceParams` map (as carried on the wire)
/// into the multi-valued `a2a_client::transport::ServiceParams` representation
/// used internally.
pub fn service_params_from_envelope(
    map: &HashMap<String, String>,
) -> HashMap<String, Vec<String>> {
    map.iter()
        .map(|(k, v)| (k.clone(), vec![v.clone()]))
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
        assert!(!methods::is_known("Authenticate"));
        assert!(!methods::is_known("MessageSend"));
        assert!(!methods::is_known(""));
    }

    #[test]
    fn request_envelope_serializes_with_protojson_field_names() {
        let req = WsRequestEnvelope {
            id: "req-1".into(),
            method: Some(methods::SEND_MESSAGE.into()),
            params: Some(serde_json::json!({"message": {"messageId": "m1"}})),
            service_params: Some(HashMap::from([
                ("a2a-version".into(), "1.0".into()),
            ])),
            cancel_stream: None,
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["method"], "SendMessage");
        assert_eq!(value["serviceParams"]["a2a-version"], "1.0");
        assert!(value.get("cancelStream").is_none());
    }

    #[test]
    fn request_envelope_with_cancel_stream_only_omits_method() {
        let req = WsRequestEnvelope {
            id: "req-2".into(),
            cancel_stream: Some(true),
            ..Default::default()
        };
        let value = serde_json::to_value(&req).unwrap();
        assert_eq!(value["cancelStream"], true);
        assert!(value.get("method").is_none());
        assert!(value.get("params").is_none());
    }

    #[test]
    fn request_envelope_round_trips_through_json() {
        let original = WsRequestEnvelope {
            id: "req-3".into(),
            method: Some(methods::GET_TASK.into()),
            params: Some(serde_json::json!({"id": "t1"})),
            service_params: None,
            cancel_stream: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let back: WsRequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, original.id);
        assert_eq!(back.method, original.method);
        assert_eq!(back.params, original.params);
    }

    #[test]
    fn response_envelope_result_omits_other_fields() {
        let resp = WsResponseEnvelope::result("req-1", serde_json::json!({"ok": true}));
        let value = serde_json::to_value(&resp).unwrap();
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["result"]["ok"], true);
        assert!(value.get("error").is_none());
        assert!(value.get("event").is_none());
        assert!(value.get("streamEnd").is_none());
    }

    #[test]
    fn response_envelope_event_and_stream_end_serialize_correctly() {
        let event = WsResponseEnvelope::event("req-2", serde_json::json!({"task": {}}));
        let value = serde_json::to_value(&event).unwrap();
        assert_eq!(value["id"], "req-2");
        assert!(value["event"].is_object());

        let end = WsResponseEnvelope::stream_end("req-2");
        let value = serde_json::to_value(&end).unwrap();
        assert_eq!(value["streamEnd"], true);
    }

    #[test]
    fn response_envelope_error_with_null_id_serializes_id_field() {
        let resp = WsResponseEnvelope::error(
            None,
            WsErrorObject {
                error_type: error_types::JSON_PARSE.to_string(),
                message: "bad json".to_string(),
                details: None,
            },
        );
        let value = serde_json::to_value(&resp).unwrap();
        assert!(value["id"].is_null());
        assert_eq!(value["error"]["type"], "JSONParseError");
    }

    #[test]
    fn service_params_envelope_round_trip_preserves_key_value_pairs() {
        let mut map = HashMap::new();
        map.insert("a2a-version".to_string(), vec!["1.0".to_string()]);
        map.insert("x-multi".to_string(), vec!["a".to_string(), "b".to_string()]);

        let envelope = service_params_to_envelope(&map).unwrap();
        assert_eq!(envelope.get("a2a-version"), Some(&"1.0".to_string()));
        assert_eq!(envelope.get("x-multi"), Some(&"a, b".to_string()));

        let restored = service_params_from_envelope(&envelope);
        assert_eq!(restored.get("a2a-version"), Some(&vec!["1.0".to_string()]));
        // Joined values come back as a single-element vec.
        assert_eq!(restored.get("x-multi"), Some(&vec!["a, b".to_string()]));
    }

    #[test]
    fn service_params_to_envelope_returns_none_for_empty_map() {
        let map: HashMap<String, Vec<String>> = HashMap::new();
        assert!(service_params_to_envelope(&map).is_none());
    }
}
