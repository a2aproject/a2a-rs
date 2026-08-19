// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
//! Error mapping between [`A2AError`] and JSON-RPC 2.0 error objects.
//!
//! The WebSocket binding reuses the A2A JSON-RPC binding's error model: the
//! numeric `code` is the canonical A2A error code and the human-readable
//! `message` is carried verbatim, with any structured detail in `data`.

use a2a::{A2AError, error_code};
use fastwebsockets::WebSocketError;

use crate::common::{JsonRpcError, close_codes};

/// Translate an [`A2AError`] into a JSON-RPC 2.0 error object, preserving the
/// numeric code and structured `data` (spec Section 7).
pub fn a2a_error_to_jsonrpc(err: &A2AError) -> JsonRpcError {
    err.to_jsonrpc_error()
}

/// Translate a JSON-RPC 2.0 error object back into an [`A2AError`], preserving
/// the numeric code and message.
pub fn jsonrpc_error_to_a2a(err: &JsonRpcError) -> A2AError {
    A2AError::new(err.code, err.message.clone())
}

/// Return the WebSocket close code that should be used when an A2A error is
/// fatal to the connection. Most errors are non-fatal, so the function returns
/// `None`. See Section 7 of the spec.
pub fn close_code_for_fatal(err: &A2AError) -> Option<u16> {
    match err.code {
        error_code::PARSE_ERROR => Some(close_codes::PROTOCOL_ERROR),
        error_code::VERSION_NOT_SUPPORTED => Some(close_codes::VERSION_NOT_SUPPORTED),
        _ => None,
    }
}

/// Return the close code to send when reading an inbound frame fails, or
/// `None` when the transport is already broken and a Close frame would have
/// nowhere to go.
///
/// A message exceeding the negotiated maximum size is reported as `1009`
/// (Message Too Big) per Section 3.6; the remaining RFC 6455 framing
/// violations are reported as `1002` (Protocol Error) per the close-code table
/// in Section 2.3.
pub fn close_code_for_read_error(err: &WebSocketError) -> Option<u16> {
    match err {
        WebSocketError::FrameTooLarge => Some(close_codes::MESSAGE_TOO_BIG),
        WebSocketError::InvalidFragment
        | WebSocketError::InvalidUTF8
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
    fn close_code_for_fatal_only_set_for_known_fatal_errors() {
        assert_eq!(
            close_code_for_fatal(&A2AError::new(error_code::PARSE_ERROR, "x")),
            Some(close_codes::PROTOCOL_ERROR)
        );
        assert_eq!(
            close_code_for_fatal(&A2AError::new(error_code::VERSION_NOT_SUPPORTED, "x")),
            Some(close_codes::VERSION_NOT_SUPPORTED)
        );
        assert_eq!(
            close_code_for_fatal(&A2AError::new(error_code::TASK_NOT_FOUND, "x")),
            None
        );
        assert_eq!(
            close_code_for_fatal(&A2AError::new(error_code::INTERNAL_ERROR, "x")),
            None
        );
    }

    #[test]
    fn oversize_frame_maps_to_message_too_big() {
        assert_eq!(
            close_code_for_read_error(&WebSocketError::FrameTooLarge),
            Some(close_codes::MESSAGE_TOO_BIG)
        );
    }

    #[test]
    fn framing_violations_map_to_protocol_error() {
        for err in [
            WebSocketError::InvalidFragment,
            WebSocketError::InvalidUTF8,
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
