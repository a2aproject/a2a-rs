// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;

use chrono::Utc;
use serde_json::Value;

use crate::errordetails::{self, TypedDetail};

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

/// Returns the reason string for an error code.
pub fn error_reason(code: i32) -> &'static str {
    match code {
        error_code::TASK_NOT_FOUND => "TASK_NOT_FOUND",
        error_code::TASK_NOT_CANCELABLE => "TASK_NOT_CANCELABLE",
        error_code::PUSH_NOTIFICATION_NOT_SUPPORTED => "PUSH_NOTIFICATION_NOT_SUPPORTED",
        error_code::UNSUPPORTED_OPERATION => "UNSUPPORTED_OPERATION",
        error_code::CONTENT_TYPE_NOT_SUPPORTED => "CONTENT_TYPE_NOT_SUPPORTED",
        error_code::INVALID_AGENT_RESPONSE => "INVALID_AGENT_RESPONSE",
        error_code::EXTENDED_CARD_NOT_CONFIGURED => "EXTENDED_AGENT_CARD_NOT_CONFIGURED",
        error_code::EXTENSION_SUPPORT_REQUIRED => "EXTENSION_SUPPORT_REQUIRED",
        error_code::VERSION_NOT_SUPPORTED => "VERSION_NOT_SUPPORTED",
        error_code::PARSE_ERROR => "PARSE_ERROR",
        error_code::INVALID_REQUEST => "INVALID_REQUEST",
        error_code::METHOD_NOT_FOUND => "METHOD_NOT_FOUND",
        error_code::INVALID_PARAMS => "INVALID_PARAMS",
        _ => "INTERNAL_ERROR",
    }
}

/// Returns the error code for a reason string, or `None` if unrecognized.
pub fn reason_to_error_code(reason: &str) -> Option<i32> {
    match reason {
        "TASK_NOT_FOUND" => Some(error_code::TASK_NOT_FOUND),
        "TASK_NOT_CANCELABLE" => Some(error_code::TASK_NOT_CANCELABLE),
        "PUSH_NOTIFICATION_NOT_SUPPORTED" => Some(error_code::PUSH_NOTIFICATION_NOT_SUPPORTED),
        "UNSUPPORTED_OPERATION" => Some(error_code::UNSUPPORTED_OPERATION),
        "UNSUPPORTED_CONTENT_TYPE" | "CONTENT_TYPE_NOT_SUPPORTED" => {
            Some(error_code::CONTENT_TYPE_NOT_SUPPORTED)
        }
        "INVALID_AGENT_RESPONSE" => Some(error_code::INVALID_AGENT_RESPONSE),
        "EXTENDED_AGENT_CARD_NOT_CONFIGURED" | "EXTENDED_CARD_NOT_CONFIGURED" => {
            Some(error_code::EXTENDED_CARD_NOT_CONFIGURED)
        }
        "EXTENSION_SUPPORT_REQUIRED" => Some(error_code::EXTENSION_SUPPORT_REQUIRED),
        "VERSION_NOT_SUPPORTED" => Some(error_code::VERSION_NOT_SUPPORTED),
        "PARSE_ERROR" => Some(error_code::PARSE_ERROR),
        "INVALID_REQUEST" => Some(error_code::INVALID_REQUEST),
        "METHOD_NOT_FOUND" => Some(error_code::METHOD_NOT_FOUND),
        "INVALID_PARAMS" => Some(error_code::INVALID_PARAMS),
        "INTERNAL_ERROR" => Some(error_code::INTERNAL_ERROR),
        _ => None,
    }
}

/// An A2A protocol error.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{message}")]
pub struct A2AError {
    pub code: i32,
    pub message: String,
    pub details: Option<Vec<TypedDetail>>,
}

impl A2AError {
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        A2AError {
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn with_details(mut self, details: Vec<TypedDetail>) -> Self {
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
            error_code::TASK_NOT_CANCELABLE => 400,
            error_code::PUSH_NOTIFICATION_NOT_SUPPORTED => 400,
            error_code::UNSUPPORTED_OPERATION => 400,
            error_code::CONTENT_TYPE_NOT_SUPPORTED => 400,
            error_code::VERSION_NOT_SUPPORTED => 400,
            error_code::PARSE_ERROR => 400,
            error_code::INVALID_REQUEST => 400,
            error_code::METHOD_NOT_FOUND => 501,
            error_code::INVALID_PARAMS => 400,
            error_code::INTERNAL_ERROR => 500,
            _ => 500,
        }
    }

    /// Convert to a JSON-RPC error object.
    ///
    /// The `data` field is always populated with an array of typed detail
    /// objects. An `ErrorInfo` detail (with reason, domain, and timestamp)
    /// is always appended as the last element, matching the Go SDK behavior.
    pub fn to_jsonrpc_error(&self) -> crate::JsonRpcError {
        let reason = error_reason(self.code);
        let metadata = HashMap::from([("timestamp".to_string(), Utc::now().to_rfc3339())]);

        let mut data: Vec<Value> = self
            .details
            .as_ref()
            .map(|d| {
                d.iter()
                    .filter(|detail| detail.type_url != errordetails::ERROR_INFO_TYPE)
                    .map(|detail| serde_json::to_value(detail).unwrap_or_default())
                    .collect()
            })
            .unwrap_or_default();

        let mut error_info =
            TypedDetail::error_info(reason, errordetails::PROTOCOL_DOMAIN, Some(metadata));

        if let Some(details) = &self.details {
            if let Some(existing) = details
                .iter()
                .find(|d| d.type_url == errordetails::ERROR_INFO_TYPE)
            {
                if let Some(Value::Object(meta)) = existing.value.get("metadata") {
                    if let Some(Value::Object(info_meta)) = error_info.value.get_mut("metadata") {
                        for (k, v) in meta {
                            info_meta.entry(k.clone()).or_insert_with(|| v.clone());
                        }
                    }
                }
            }
        }

        data.push(serde_json::to_value(&error_info).unwrap_or_default());

        crate::JsonRpcError {
            code: self.code,
            message: self.message.clone(),
            data: Some(Value::Array(data)),
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
        assert_eq!(A2AError::task_not_cancelable("x").http_status_code(), 400);
        assert_eq!(A2AError::internal("x").http_status_code(), 500);
        assert_eq!(A2AError::invalid_params("x").http_status_code(), 400);
        assert_eq!(
            A2AError::content_type_not_supported().http_status_code(),
            400
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
            501
        );
    }

    #[test]
    fn test_to_jsonrpc_error() {
        let e = A2AError::task_not_found("t1");
        let rpc = e.to_jsonrpc_error();
        assert_eq!(rpc.code, error_code::TASK_NOT_FOUND);
        assert!(rpc.message.contains("t1"));
        let data = rpc.data.expect("data should always be present");
        let arr = data.as_array().expect("data should be an array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["@type"], errordetails::ERROR_INFO_TYPE);
        assert_eq!(arr[0]["reason"], "TASK_NOT_FOUND");
        assert_eq!(arr[0]["domain"], errordetails::PROTOCOL_DOMAIN);
        assert!(arr[0]["metadata"]["timestamp"].is_string());
    }

    #[test]
    fn test_with_details() {
        use std::collections::HashMap;
        let struct_detail = TypedDetail::from_struct(HashMap::from([(
            "key".to_string(),
            Value::String("val".to_string()),
        )]));
        let e = A2AError::internal("err").with_details(vec![struct_detail]);

        let rpc = e.to_jsonrpc_error();
        let data = rpc.data.expect("data should always be present");
        let arr = data.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["key"], "val");
        assert_eq!(arr[1]["@type"], errordetails::ERROR_INFO_TYPE);
        assert_eq!(arr[1]["reason"], "INTERNAL_ERROR");
    }

    #[test]
    fn test_to_jsonrpc_error_merges_existing_error_info_metadata() {
        let existing_info = TypedDetail::error_info(
            "TASK_NOT_FOUND",
            errordetails::PROTOCOL_DOMAIN,
            Some(HashMap::from([("taskId".to_string(), "t1".to_string())])),
        );
        let e = A2AError::task_not_found("t1").with_details(vec![existing_info]);
        let rpc = e.to_jsonrpc_error();
        let data = rpc.data.unwrap();
        let arr = data.as_array().unwrap();
        // Existing ErrorInfo is filtered; merged into auto-generated one
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["@type"], errordetails::ERROR_INFO_TYPE);
        assert_eq!(arr[0]["metadata"]["taskId"], "t1");
        assert!(arr[0]["metadata"]["timestamp"].is_string());
    }

    #[test]
    fn test_reason_to_error_code() {
        assert_eq!(
            reason_to_error_code("TASK_NOT_FOUND"),
            Some(error_code::TASK_NOT_FOUND)
        );
        assert_eq!(
            reason_to_error_code("TASK_NOT_CANCELABLE"),
            Some(error_code::TASK_NOT_CANCELABLE)
        );
        assert_eq!(
            reason_to_error_code("PUSH_NOTIFICATION_NOT_SUPPORTED"),
            Some(error_code::PUSH_NOTIFICATION_NOT_SUPPORTED)
        );
        assert_eq!(
            reason_to_error_code("UNSUPPORTED_OPERATION"),
            Some(error_code::UNSUPPORTED_OPERATION)
        );
        assert_eq!(
            reason_to_error_code("CONTENT_TYPE_NOT_SUPPORTED"),
            Some(error_code::CONTENT_TYPE_NOT_SUPPORTED)
        );
        assert_eq!(
            reason_to_error_code("UNSUPPORTED_CONTENT_TYPE"),
            Some(error_code::CONTENT_TYPE_NOT_SUPPORTED)
        );
        assert_eq!(
            reason_to_error_code("INVALID_AGENT_RESPONSE"),
            Some(error_code::INVALID_AGENT_RESPONSE)
        );
        assert_eq!(
            reason_to_error_code("EXTENDED_AGENT_CARD_NOT_CONFIGURED"),
            Some(error_code::EXTENDED_CARD_NOT_CONFIGURED)
        );
        assert_eq!(
            reason_to_error_code("EXTENDED_CARD_NOT_CONFIGURED"),
            Some(error_code::EXTENDED_CARD_NOT_CONFIGURED)
        );
        assert_eq!(
            reason_to_error_code("EXTENSION_SUPPORT_REQUIRED"),
            Some(error_code::EXTENSION_SUPPORT_REQUIRED)
        );
        assert_eq!(
            reason_to_error_code("VERSION_NOT_SUPPORTED"),
            Some(error_code::VERSION_NOT_SUPPORTED)
        );
        assert_eq!(
            reason_to_error_code("PARSE_ERROR"),
            Some(error_code::PARSE_ERROR)
        );
        assert_eq!(
            reason_to_error_code("INVALID_REQUEST"),
            Some(error_code::INVALID_REQUEST)
        );
        assert_eq!(
            reason_to_error_code("METHOD_NOT_FOUND"),
            Some(error_code::METHOD_NOT_FOUND)
        );
        assert_eq!(
            reason_to_error_code("INVALID_PARAMS"),
            Some(error_code::INVALID_PARAMS)
        );
        assert_eq!(
            reason_to_error_code("INTERNAL_ERROR"),
            Some(error_code::INTERNAL_ERROR)
        );
        assert_eq!(reason_to_error_code("UNKNOWN_REASON"), None);
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
