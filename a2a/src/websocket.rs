// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use crate::{JsonRpcError, JsonRpcId};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Message metadata propagated as service parameters.
pub type WebSocketServiceParams = HashMap<String, Vec<String>>;

/// Client-to-server WebSocket frame shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSocketRequestEnvelope {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_params: Option<WebSocketServiceParams>,
}

impl WebSocketRequestEnvelope {
    pub fn new(
        id: JsonRpcId,
        method: impl Into<String>,
        params: Option<Value>,
        service_params: Option<WebSocketServiceParams>,
    ) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.into(),
            params,
            service_params,
        }
    }
}

/// Server-to-client WebSocket frame shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebSocketResponseEnvelope {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<JsonRpcError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_event: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_error: Option<JsonRpcError>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stream_end: Option<bool>,
}

impl WebSocketResponseEnvelope {
    pub fn success(id: JsonRpcId, result: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
            stream_event: None,
            stream_error: None,
            stream_end: None,
        }
    }

    pub fn error(id: JsonRpcId, error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(error),
            stream_event: None,
            stream_error: None,
            stream_end: None,
        }
    }

    pub fn stream_event(id: JsonRpcId, stream_event: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: None,
            stream_event: Some(stream_event),
            stream_error: None,
            stream_end: None,
        }
    }

    pub fn stream_error(id: JsonRpcId, stream_error: JsonRpcError) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: None,
            stream_event: None,
            stream_error: Some(stream_error),
            stream_end: None,
        }
    }

    pub fn stream_end(id: JsonRpcId) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: None,
            stream_event: None,
            stream_error: None,
            stream_end: Some(true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jsonrpc::JsonRpcId;
    use serde_json::json;

    #[test]
    fn test_websocket_request_envelope_serialization() {
        let envelope = WebSocketRequestEnvelope::new(
            JsonRpcId::String("test-id".into()),
            "test_method",
            Some(json!({"key": "value"})),
            None,
        );
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: WebSocketRequestEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.jsonrpc, "2.0");
        assert_eq!(parsed.id, JsonRpcId::String("test-id".into()));
        assert_eq!(parsed.method, "test_method");
    }

    #[test]
    fn test_websocket_request_envelope_with_service_params() {
        let mut params = WebSocketServiceParams::new();
        params.insert("x-custom".to_string(), vec!["value".to_string()]);
        let envelope = WebSocketRequestEnvelope::new(
            JsonRpcId::String("id".into()),
            "method",
            None,
            Some(params),
        );
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: WebSocketRequestEnvelope = serde_json::from_str(&json).unwrap();
        assert!(parsed.service_params.is_some());
    }

    #[test]
    fn test_websocket_response_envelope_success() {
        let envelope = WebSocketResponseEnvelope::success(
            JsonRpcId::String("id".into()),
            json!({"result": "ok"}),
        );
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: WebSocketResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert!(parsed.result.is_some());
        assert!(parsed.error.is_none());
    }

    #[test]
    fn test_websocket_response_envelope_error() {
        let error = JsonRpcError {
            code: -32700,
            message: "Parse error".to_string(),
            data: None,
        };
        let envelope = WebSocketResponseEnvelope::error(JsonRpcId::String("id".into()), error);
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: WebSocketResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert!(parsed.error.is_some());
        assert!(parsed.result.is_none());
    }

    #[test]
    fn test_websocket_response_envelope_stream_event() {
        let envelope = WebSocketResponseEnvelope::stream_event(
            JsonRpcId::String("id".into()),
            json!({"event": "data"}),
        );
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: WebSocketResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert!(parsed.stream_event.is_some());
        assert!(parsed.stream_error.is_none());
    }

    #[test]
    fn test_websocket_response_envelope_stream_error() {
        let error = JsonRpcError {
            code: -32000,
            message: "Stream error".to_string(),
            data: None,
        };
        let envelope =
            WebSocketResponseEnvelope::stream_error(JsonRpcId::String("id".into()), error);
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: WebSocketResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert!(parsed.stream_error.is_some());
    }

    #[test]
    fn test_websocket_response_envelope_stream_end() {
        let envelope = WebSocketResponseEnvelope::stream_end(JsonRpcId::String("id".into()));
        let json = serde_json::to_string(&envelope).unwrap();
        let parsed: WebSocketResponseEnvelope = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stream_end, Some(true));
    }
}
