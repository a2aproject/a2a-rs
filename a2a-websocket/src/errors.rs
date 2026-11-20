// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Error mapping between [`A2AError`] and JSON-RPC 2.0 error objects.
//!
//! The WebSocket binding reuses the A2A JSON-RPC binding's error model: the
//! numeric `code` is the canonical A2A error code and the human-readable
//! `message` is carried verbatim, with any structured detail in `data`.
//! Binding-specific errors share code `-32000` and are told apart by the
//! `reason` of their `google.rpc.ErrorInfo` (spec Section 7.3).

use std::collections::HashMap;

use a2a::errordetails::{ERROR_INFO_TYPE, PROTOCOL_DOMAIN, TypedDetail};
use a2a::{A2AError, a2a_error_from_details};
use fastwebsockets::WebSocketError;
use serde_json::Value;

use crate::common::{JsonRpcError, SERVER_ERROR_CODE, binding_errors, close_codes};

/// Build a binding-specific error: code `-32000` with an `ErrorInfo` carrying
/// `reason` (one of [`binding_errors`]) and optional metadata (spec Section 7.3).
pub fn binding_error(
    reason: &str,
    message: impl Into<String>,
    metadata: Option<HashMap<String, String>>,
) -> A2AError {
    A2AError::new(SERVER_ERROR_CODE, message).with_details(vec![TypedDetail::error_info(
        reason,
        PROTOCOL_DOMAIN,
        metadata,
    )])
}

/// The binding-specific `reason` of an error, if it is one (spec Section 7.3).
pub fn binding_error_reason(err: &A2AError) -> Option<&str> {
    if err.code != SERVER_ERROR_CODE {
        return None;
    }
    err.details.as_ref()?.iter().rev().find_map(|detail| {
        if detail.type_url != ERROR_INFO_TYPE
            || detail.value.get("domain").and_then(Value::as_str) != Some(PROTOCOL_DOMAIN)
        {
            return None;
        }
        let reason = detail.value.get("reason")?.as_str()?;
        binding_errors::is_binding_reason(reason).then_some(reason)
    })
}

/// Translate an [`A2AError`] into a JSON-RPC 2.0 error object, preserving the
/// numeric code and structured `data` (spec Section 7).
pub fn a2a_error_to_jsonrpc(err: &A2AError) -> JsonRpcError {
    let mut out = err.to_jsonrpc_error();
    // `to_jsonrpc_error` derives the `ErrorInfo` reason from the code, which for
    // `-32000` is `INTERNAL_ERROR`; a binding error must carry its own reason.
    if let Some(reason) = binding_error_reason(err) {
        if let Some(Value::Array(items)) = out.data.as_mut() {
            if let Some(info) = items
                .iter_mut()
                .rev()
                .find(|item| item.get("@type").and_then(Value::as_str) == Some(ERROR_INFO_TYPE))
            {
                info["reason"] = Value::String(reason.to_string());
            }
        }
    }
    out
}

/// Translate a JSON-RPC 2.0 error object back into an [`A2AError`], keeping the
/// typed details in `data` so callers can inspect a binding error's reason.
pub fn jsonrpc_error_to_a2a(err: &JsonRpcError) -> A2AError {
    let details: Vec<TypedDetail> = match &err.data {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|item| serde_json::from_value(item.clone()).ok())
            .collect(),
        _ => Vec::new(),
    };
    a2a_error_from_details(err.code, err.message.clone(), details)
}

/// Return the close code to send when reading an inbound frame fails, or
/// `None` when the transport is already broken and a Close frame would have
/// nowhere to go.
///
/// A message exceeding the maximum size is reported as `1009` (Section 3.6),
/// a text message that is not valid UTF-8 as `1007`, and the remaining RFC
/// 6455 framing violations as `1002` (Sections 2.3 and 7.4).
pub fn close_code_for_read_error(err: &WebSocketError) -> Option<u16> {
    match err {
        WebSocketError::FrameTooLarge => Some(close_codes::MESSAGE_TOO_BIG),
        WebSocketError::InvalidUTF8 => Some(close_codes::INVALID_PAYLOAD),
        WebSocketError::InvalidFragment
        | WebSocketError::InvalidContinuationFrame
        | WebSocketError::InvalidCloseFrame
        | WebSocketError::InvalidCloseCode
        | WebSocketError::ReservedBitsNotZero
        | WebSocketError::ControlFrameFragmented
        | WebSocketError::PingFrameTooLarge => Some(close_codes::PROTOCOL_ERROR),
        // Peer hung up, I/O failed, or the error can only arise during the
        // handshake — nothing useful can be written to the socket.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::error_code;

    #[test]
    fn a2a_error_round_trips_through_jsonrpc() {
        let original = A2AError::new(error_code::TASK_NOT_FOUND, "missing task");
        let jsonrpc = a2a_error_to_jsonrpc(&original);
        assert_eq!(jsonrpc.code, error_code::TASK_NOT_FOUND);
        assert_eq!(jsonrpc.message, "missing task");

        let back = jsonrpc_error_to_a2a(&jsonrpc);
        assert_eq!(back.code, error_code::TASK_NOT_FOUND);
        assert_eq!(back.message, "missing task");
    }

    #[test]
    fn unknown_code_is_preserved_verbatim() {
        let jsonrpc = JsonRpcError {
            code: 99999,
            message: "boom".to_string(),
            data: None,
        };
        let back = jsonrpc_error_to_a2a(&jsonrpc);
        assert_eq!(back.code, 99999);
        assert_eq!(back.message, "boom");
    }

    #[test]
    fn binding_error_carries_its_reason_and_metadata_on_the_wire() {
        let err = binding_error(
            binding_errors::UNAUTHENTICATED,
            "Invalid credentials",
            Some(HashMap::from([(
                "schemes".to_string(),
                "oauth".to_string(),
            )])),
        );
        let jsonrpc = a2a_error_to_jsonrpc(&err);
        assert_eq!(jsonrpc.code, SERVER_ERROR_CODE);
        let data = jsonrpc.data.as_ref().unwrap().as_array().unwrap();
        let info = data
            .iter()
            .find(|item| item["@type"] == ERROR_INFO_TYPE)
            .unwrap();
        assert_eq!(info["reason"], "WS_UNAUTHENTICATED");
        assert_eq!(info["domain"], PROTOCOL_DOMAIN);
        assert_eq!(info["metadata"]["schemes"], "oauth");
    }

    #[test]
    fn binding_error_reason_survives_a_round_trip() {
        let err = binding_error(binding_errors::RATE_LIMITED, "Rate limit exceeded", None);
        let back = jsonrpc_error_to_a2a(&a2a_error_to_jsonrpc(&err));
        assert_eq!(back.code, SERVER_ERROR_CODE);
        assert_eq!(binding_error_reason(&back), Some("WS_RATE_LIMITED"));
    }

    #[test]
    fn a2a_errors_have_no_binding_reason() {
        let err = A2AError::new(error_code::TASK_NOT_FOUND, "x");
        assert_eq!(binding_error_reason(&err), None);
        let back = jsonrpc_error_to_a2a(&a2a_error_to_jsonrpc(&err));
        assert_eq!(binding_error_reason(&back), None);
    }

    #[test]
    fn oversize_frame_maps_to_message_too_big() {
        assert_eq!(
            close_code_for_read_error(&WebSocketError::FrameTooLarge),
            Some(close_codes::MESSAGE_TOO_BIG)
        );
    }

    #[test]
    fn invalid_utf8_maps_to_invalid_payload() {
        assert_eq!(
            close_code_for_read_error(&WebSocketError::InvalidUTF8),
            Some(close_codes::INVALID_PAYLOAD)
        );
    }

    #[test]
    fn framing_violations_map_to_protocol_error() {
        for err in [
            WebSocketError::InvalidFragment,
            WebSocketError::InvalidContinuationFrame,
            WebSocketError::InvalidCloseFrame,
            WebSocketError::InvalidCloseCode,
            WebSocketError::ReservedBitsNotZero,
            WebSocketError::ControlFrameFragmented,
            WebSocketError::PingFrameTooLarge,
        ] {
            assert_eq!(
                close_code_for_read_error(&err),
                Some(close_codes::PROTOCOL_ERROR),
                "{err} should close with 1002"
            );
        }
    }

    #[test]
    fn broken_transport_errors_have_no_close_code() {
        assert_eq!(
            close_code_for_read_error(&WebSocketError::ConnectionClosed),
            None
        );
        assert_eq!(
            close_code_for_read_error(&WebSocketError::UnexpectedEOF),
            None
        );
        assert_eq!(
            close_code_for_read_error(&WebSocketError::IoError(std::io::Error::other("boom"))),
            None
        );
    }

    #[test]
    fn close_frame_reasons_fit_the_rfc_6455_control_frame_limit() {
        // A Close frame's reason may be at most 123 bytes (RFC 6455 §5.5).
        for err in [
            WebSocketError::FrameTooLarge,
            WebSocketError::InvalidUTF8,
            WebSocketError::PingFrameTooLarge,
        ] {
            assert!(err.to_string().len() <= 123, "reason too long for {err}");
        }
    }
}
