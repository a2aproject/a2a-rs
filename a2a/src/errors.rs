// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use serde_json::Value;
use std::collections::HashMap;

/// A2A-specific error codes (JSON-RPC).
pub mod error_code {
    // A2A application errors
    pub const TASK_NOT_FOUND: i32 = -32001;
    pub const TASK_NOT_CANCELABLE: i32 = -32002;
    pub const PUSH_NOTIFICATION_NOT_SUPPORTED: i32 = -32003;
    pub const UNSUPPORTED_OPERATION: i32 = -32004;
    pub const CONTENT_TYPE_NOT_SUPPORTED: i32 = -32005;
    pub const INVALID_AGENT_RESPONSE: i32 = -32006;
    pub const EXTENDED_CARD_NOT_CONFIGURED: i32 = -32007;
    pub const EXTENSION_SUPPORT_REQUIRED: i32 = -32008;
    pub const VERSION_NOT_SUPPORTED: i32 = -32009;

    // Standard JSON-RPC errors
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
}

/// An A2A protocol error.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct A2AError {
    pub code: i32,
    pub message: String,
    pub details: Option<HashMap<String, Value>>,
}

impl A2AError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        A2AError {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: HashMap<String, Value>) -> Self {
        self.details = Some(details);
        self
    }

    // Convenience constructors for common errors

    pub fn task_not_found(task_id: &str) -> Self {
        A2AError::new(
            error_code::TASK_NOT_FOUND,
            format!("task not found: {task_id}"),
        )
    }

    pub fn task_not_cancelable(task_id: &str) -> Self {
        A2AError::new(
            error_code::TASK_NOT_CANCELABLE,
            format!("task cannot be canceled: {task_id}"),
        )
    }

    pub fn push_notification_not_supported() -> Self {
        A2AError::new(
            error_code::PUSH_NOTIFICATION_NOT_SUPPORTED,
            "push notification not supported",
        )
    }

    pub fn unsupported_operation(msg: impl Into<String>) -> Self {
        A2AError::new(error_code::UNSUPPORTED_OPERATION, msg)
    }

    pub fn content_type_not_supported() -> Self {
        A2AError::new(
            error_code::CONTENT_TYPE_NOT_SUPPORTED,
            "incompatible content types",
        )
    }

    pub fn invalid_agent_response() -> Self {
        A2AError::new(error_code::INVALID_AGENT_RESPONSE, "invalid agent response")
    }

    pub fn version_not_supported(version: &str) -> Self {
        A2AError::new(
            error_code::VERSION_NOT_SUPPORTED,
            format!("version not supported: {version}"),
        )
    }

    pub fn internal(msg: impl Into<String>) -> Self {
        A2AError::new(error_code::INTERNAL_ERROR, msg)
    }

    pub fn invalid_params(msg: impl Into<String>) -> Self {
        A2AError::new(error_code::INVALID_PARAMS, msg)
    }

    pub fn parse_error(msg: impl Into<String>) -> Self {
        A2AError::new(error_code::PARSE_ERROR, msg)
    }

    pub fn invalid_request(msg: impl Into<String>) -> Self {
        A2AError::new(error_code::INVALID_REQUEST, msg)
    }

    pub fn method_not_found(method: &str) -> Self {
        A2AError::new(
            error_code::METHOD_NOT_FOUND,
            format!("method not found: {method}"),
        )
    }

    /// Map A2A error code to HTTP status code for REST binding.
    pub fn http_status_code(&self) -> u16 {
        match self.code {
            error_code::TASK_NOT_FOUND => 404,
            error_code::TASK_NOT_CANCELABLE => 409,
            error_code::PUSH_NOTIFICATION_NOT_SUPPORTED => 400,
            error_code::UNSUPPORTED_OPERATION => 400,
            error_code::CONTENT_TYPE_NOT_SUPPORTED => 415,
            error_code::VERSION_NOT_SUPPORTED => 400,
            error_code::PARSE_ERROR => 400,
            error_code::INVALID_REQUEST => 400,
            error_code::METHOD_NOT_FOUND => 404,
            error_code::INVALID_PARAMS => 400,
            error_code::INTERNAL_ERROR => 500,
            _ => 500,
        }
    }

    /// Convert to a JSON-RPC error object.
    pub fn to_jsonrpc_error(&self) -> crate::JsonRpcError {
        crate::JsonRpcError {
            code: self.code,
            message: self.message.clone(),
            data: self
                .details
                .as_ref()
                .map(|d| serde_json::to_value(d).unwrap_or_default()),
        }
    }
}

impl From<A2AError> for crate::JsonRpcError {
    fn from(e: A2AError) -> Self {
        e.to_jsonrpc_error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_error_constructors() {
        let e = A2AError::task_not_found("t1");
        assert_eq!(e.code, error_code::TASK_NOT_FOUND);
        assert!(e.message.contains("t1"));

        let e = A2AError::task_not_cancelable("t2");
        assert_eq!(e.code, error_code::TASK_NOT_CANCELABLE);

        let e = A2AError::push_notification_not_supported();
        assert_eq!(e.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);

        let e = A2AError::unsupported_operation("nope");
        assert_eq!(e.code, error_code::UNSUPPORTED_OPERATION);

        let e = A2AError::content_type_not_supported();
        assert_eq!(e.code, error_code::CONTENT_TYPE_NOT_SUPPORTED);

        let e = A2AError::invalid_agent_response();
        assert_eq!(e.code, error_code::INVALID_AGENT_RESPONSE);

        let e = A2AError::version_not_supported("2.0");
        assert_eq!(e.code, error_code::VERSION_NOT_SUPPORTED);

        let e = A2AError::internal("boom");
        assert_eq!(e.code, error_code::INTERNAL_ERROR);

        let e = A2AError::invalid_params("bad param");
        assert_eq!(e.code, error_code::INVALID_PARAMS);

        let e = A2AError::parse_error("bad json");
        assert_eq!(e.code, error_code::PARSE_ERROR);

        let e = A2AError::invalid_request("bad req");
        assert_eq!(e.code, error_code::INVALID_REQUEST);

        let e = A2AError::method_not_found("foo");
        assert_eq!(e.code, error_code::METHOD_NOT_FOUND);
    }

    #[test]
    fn test_http_status_codes() {
        assert_eq!(A2AError::task_not_found("x").http_status_code(), 404);
        assert_eq!(A2AError::task_not_cancelable("x").http_status_code(), 409);
        assert_eq!(A2AError::internal("x").http_status_code(), 500);
        assert_eq!(A2AError::invalid_params("x").http_status_code(), 400);
        assert_eq!(
            A2AError::content_type_not_supported().http_status_code(),
            415
        );
        assert_eq!(A2AError::new(9999, "unknown").http_status_code(), 500);
    }

    #[test]
    fn test_http_status_codes_for_remaining_a2a_mappings() {
        assert_eq!(
            A2AError::push_notification_not_supported().http_status_code(),
            400
        );
        assert_eq!(
            A2AError::unsupported_operation("nope").http_status_code(),
            400
        );
        assert_eq!(
            A2AError::version_not_supported("9.9").http_status_code(),
            400
        );
        assert_eq!(A2AError::parse_error("bad").http_status_code(), 400);
        assert_eq!(A2AError::invalid_request("bad").http_status_code(), 400);
        assert_eq!(
            A2AError::method_not_found("missing").http_status_code(),
            404
        );
    }

    #[test]
    fn test_to_jsonrpc_error() {
        let e = A2AError::task_not_found("t1");
        let rpc = e.to_jsonrpc_error();
        assert_eq!(rpc.code, error_code::TASK_NOT_FOUND);
        assert!(rpc.message.contains("t1"));
        assert!(rpc.data.is_none());
    }

    #[test]
    fn test_with_details() {
        let mut details = HashMap::new();
        details.insert("key".to_string(), Value::String("val".to_string()));
        let e = A2AError::internal("err").with_details(details.clone());
        assert_eq!(e.details.as_ref().unwrap(), &details);

        let rpc = e.to_jsonrpc_error();
        assert!(rpc.data.is_some());
    }

    #[test]
    fn test_error_display() {
        let e = A2AError::internal("test message");
        assert_eq!(format!("{e}"), "test message");
    }

    #[test]
    fn test_jsonrpc_error_from() {
        let e = A2AError::internal("test");
        let rpc: crate::JsonRpcError = e.into();
        assert_eq!(rpc.code, error_code::INTERNAL_ERROR);
    }
}
