// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

// ---------------------------------------------------------------------------
// JSON-RPC 2.0 types
// ---------------------------------------------------------------------------

/// JSON-RPC 2.0 request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl JsonRpcRequest {
    pub fn new(id: JsonRpcId, method: impl Into<String>, params: Option<Value>) -> Self {
        JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// JSON-RPC 2.0 response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
}

impl JsonRpcResponse {
    pub fn success(id: JsonRpcId, result: Value) -> Self {
        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }

    pub fn error(id: JsonRpcId, error: JsonRpcError) -> Self {
        JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

// ---------------------------------------------------------------------------
// JsonRpcId — preserves string vs number
// ---------------------------------------------------------------------------

/// A JSON-RPC ID that can be a string, integer, or null.
#[derive(Debug, Clone, PartialEq)]
pub enum JsonRpcId {
    String(String),
    Number(i64),
    Null,
}

impl Serialize for JsonRpcId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            JsonRpcId::String(s) => serializer.serialize_str(s),
            JsonRpcId::Number(n) => serializer.serialize_i64(*n),
            JsonRpcId::Null => serializer.serialize_none(),
        }
    }
}

impl<'de> Deserialize<'de> for JsonRpcId {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let v = Value::deserialize(deserializer)?;
        match v {
            Value::String(s) => Ok(JsonRpcId::String(s)),
            Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(JsonRpcId::Number(i))
                } else {
                    Err(serde::de::Error::custom(
                        "JSON-RPC id must be a non-fractional number",
                    ))
                }
            }
            Value::Null => Ok(JsonRpcId::Null),
            _ => Err(serde::de::Error::custom(
                "JSON-RPC id must be string, integer, or null",
            )),
        }
    }
}

impl From<String> for JsonRpcId {
    fn from(s: String) -> Self {
        JsonRpcId::String(s)
    }
}

impl From<&str> for JsonRpcId {
    fn from(s: &str) -> Self {
        JsonRpcId::String(s.to_string())
    }
}

impl From<i64> for JsonRpcId {
    fn from(n: i64) -> Self {
        JsonRpcId::Number(n)
    }
}

// ---------------------------------------------------------------------------
// A2A JSON-RPC method names
// ---------------------------------------------------------------------------

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

    pub fn is_valid(method: &str) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_jsonrpc_id_string() {
        let id = JsonRpcId::String("abc".into());
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, r#""abc""#);
        let back: JsonRpcId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, JsonRpcId::String("abc".into()));
    }

    #[test]
    fn test_jsonrpc_id_number() {
        let id = JsonRpcId::Number(42);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "42");
        let back: JsonRpcId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, JsonRpcId::Number(42));
    }

    #[test]
    fn test_jsonrpc_request_roundtrip() {
        let req = JsonRpcRequest::new(
            JsonRpcId::String("req-1".into()),
            methods::SEND_MESSAGE,
            Some(serde_json::json!({"key": "value"})),
        );
        let json = serde_json::to_string(&req).unwrap();
        let back: JsonRpcRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.method, methods::SEND_MESSAGE);
        assert_eq!(back.jsonrpc, "2.0");
    }

    #[test]
    fn test_jsonrpc_id_null() {
        let id = JsonRpcId::Null;
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "null");
        let back: JsonRpcId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, JsonRpcId::Null);
    }

    #[test]
    fn test_jsonrpc_id_invalid_type() {
        let result = serde_json::from_str::<JsonRpcId>(r#"[1,2]"#);
        assert!(result.is_err());
    }

    #[test]
    fn test_jsonrpc_id_fractional_number() {
        let result = serde_json::from_str::<JsonRpcId>("3.14");
        assert!(result.is_err());
    }

    #[test]
    fn test_jsonrpc_id_from_string() {
        let id: JsonRpcId = String::from("test").into();
        assert_eq!(id, JsonRpcId::String("test".into()));
    }

    #[test]
    fn test_jsonrpc_id_from_str() {
        let id: JsonRpcId = "test".into();
        assert_eq!(id, JsonRpcId::String("test".into()));
    }

    #[test]
    fn test_jsonrpc_id_from_i64() {
        let id: JsonRpcId = 42i64.into();
        assert_eq!(id, JsonRpcId::Number(42));
    }

    #[test]
    fn test_jsonrpc_response_success() {
        let resp =
            JsonRpcResponse::success(JsonRpcId::Number(1), serde_json::json!({"status": "ok"}));
        assert_eq!(resp.jsonrpc, "2.0");
        assert!(resp.result.is_some());
        assert!(resp.error.is_none());
        let json = serde_json::to_string(&resp).unwrap();
        let back: JsonRpcResponse = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, JsonRpcId::Number(1));
    }

    #[test]
    fn test_jsonrpc_response_error() {
        let err = JsonRpcError {
            code: -32600,
            message: "invalid".into(),
            data: None,
        };
        let resp = JsonRpcResponse::error(JsonRpcId::String("e1".into()), err);
        assert!(resp.result.is_none());
        assert!(resp.error.is_some());
        assert_eq!(resp.error.as_ref().unwrap().code, -32600);
    }

    #[test]
    fn test_jsonrpc_error_with_data() {
        let err = JsonRpcError {
            code: -32000,
            message: "custom".into(),
            data: Some(serde_json::json!({"detail": "info"})),
        };
        let json = serde_json::to_string(&err).unwrap();
        let back: JsonRpcError = serde_json::from_str(&json).unwrap();
        assert!(back.data.is_some());
    }

    #[test]
    fn test_jsonrpc_request_no_params() {
        let req = JsonRpcRequest::new(JsonRpcId::Number(1), methods::GET_TASK, None);
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("params"));
    }

    #[test]
    fn test_methods_is_streaming() {
        assert!(methods::is_streaming(methods::SEND_STREAMING_MESSAGE));
        assert!(methods::is_streaming(methods::SUBSCRIBE_TO_TASK));
        assert!(!methods::is_streaming("message.stream"));
        assert!(!methods::is_streaming("tasks.resubscribe"));
        assert!(!methods::is_streaming(methods::SEND_MESSAGE));
        assert!(!methods::is_streaming(methods::GET_TASK));
        assert!(!methods::is_streaming("unknown"));
    }

    #[test]
    fn test_methods_is_valid() {
        assert!(methods::is_valid(methods::SEND_MESSAGE));
        assert!(methods::is_valid(methods::SEND_STREAMING_MESSAGE));
        assert!(methods::is_valid(methods::GET_TASK));
        assert!(methods::is_valid(methods::LIST_TASKS));
        assert!(methods::is_valid(methods::CANCEL_TASK));
        assert!(methods::is_valid(methods::SUBSCRIBE_TO_TASK));
        assert!(methods::is_valid(methods::CREATE_PUSH_CONFIG));
        assert!(methods::is_valid(methods::GET_PUSH_CONFIG));
        assert!(methods::is_valid(methods::LIST_PUSH_CONFIGS));
        assert!(methods::is_valid(methods::DELETE_PUSH_CONFIG));
        assert!(methods::is_valid(methods::GET_EXTENDED_AGENT_CARD));
        assert!(!methods::is_valid("message.send"));
        assert!(!methods::is_valid("message.stream"));
        assert!(!methods::is_valid("tasks.get"));
        assert!(!methods::is_valid("tasks.list"));
        assert!(!methods::is_valid("tasks.cancel"));
        assert!(!methods::is_valid("tasks.resubscribe"));
        assert!(!methods::is_valid("push-config.set"));
        assert!(!methods::is_valid("push-config.get"));
        assert!(!methods::is_valid("push-config.list"));
        assert!(!methods::is_valid("push-config.delete"));
        assert!(!methods::is_valid("agent-card.extended.get"));
        assert!(!methods::is_valid("unknown.method"));
    }
}
