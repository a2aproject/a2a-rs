// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::{A2AError, error_code};

use crate::common::{WsErrorObject, close_codes, error_types};

/// Translate an [`A2AError`] into the canonical WebSocket error object.
///
/// The mapping mirrors the table in Section 7.2 of the WebSocket binding
/// specification.
pub fn a2a_error_to_ws_error(err: &A2AError) -> WsErrorObject {
    let error_type = match err.code {
        error_code::TASK_NOT_FOUND => error_types::TASK_NOT_FOUND,
        error_code::TASK_NOT_CANCELABLE => error_types::TASK_NOT_CANCELABLE,
        error_code::PUSH_NOTIFICATION_NOT_SUPPORTED => {
            error_types::PUSH_NOTIFICATION_NOT_SUPPORTED
        }
        error_code::UNSUPPORTED_OPERATION => error_types::UNSUPPORTED_OPERATION,
        error_code::CONTENT_TYPE_NOT_SUPPORTED => error_types::CONTENT_TYPE_NOT_SUPPORTED,
        error_code::INVALID_AGENT_RESPONSE => error_types::INVALID_AGENT_RESPONSE,
        error_code::EXTENDED_CARD_NOT_CONFIGURED => error_types::EXTENDED_CARD_NOT_CONFIGURED,
        error_code::EXTENSION_SUPPORT_REQUIRED => error_types::EXTENSION_SUPPORT_REQUIRED,
        error_code::VERSION_NOT_SUPPORTED => error_types::VERSION_NOT_SUPPORTED,
        error_code::PARSE_ERROR => error_types::JSON_PARSE,
        error_code::INVALID_REQUEST => error_types::INVALID_REQUEST,
        error_code::METHOD_NOT_FOUND => error_types::METHOD_NOT_FOUND,
        error_code::INVALID_PARAMS => error_types::INVALID_PARAMS,
        error_code::INTERNAL_ERROR => error_types::INTERNAL,
        _ => error_types::INTERNAL,
    };
    WsErrorObject {
        error_type: error_type.to_string(),
        message: err.message.clone(),
        details: None,
    }
}

/// Translate a WebSocket error object back into an [`A2AError`] using the
/// reverse mapping. Unknown type strings collapse to `INTERNAL_ERROR`.
pub fn ws_error_to_a2a_error(err: &WsErrorObject) -> A2AError {
    let code = match err.error_type.as_str() {
        error_types::TASK_NOT_FOUND => error_code::TASK_NOT_FOUND,
        error_types::TASK_NOT_CANCELABLE => error_code::TASK_NOT_CANCELABLE,
        error_types::PUSH_NOTIFICATION_NOT_SUPPORTED => {
            error_code::PUSH_NOTIFICATION_NOT_SUPPORTED
        }
        error_types::UNSUPPORTED_OPERATION => error_code::UNSUPPORTED_OPERATION,
        error_types::CONTENT_TYPE_NOT_SUPPORTED => error_code::CONTENT_TYPE_NOT_SUPPORTED,
        error_types::INVALID_AGENT_RESPONSE => error_code::INVALID_AGENT_RESPONSE,
        error_types::EXTENDED_CARD_NOT_CONFIGURED => error_code::EXTENDED_CARD_NOT_CONFIGURED,
        error_types::EXTENSION_SUPPORT_REQUIRED => error_code::EXTENSION_SUPPORT_REQUIRED,
        error_types::VERSION_NOT_SUPPORTED => error_code::VERSION_NOT_SUPPORTED,
        error_types::JSON_PARSE => error_code::PARSE_ERROR,
        error_types::INVALID_REQUEST => error_code::INVALID_REQUEST,
        error_types::METHOD_NOT_FOUND => error_code::METHOD_NOT_FOUND,
        error_types::INVALID_PARAMS => error_code::INVALID_PARAMS,
        error_types::INTERNAL => error_code::INTERNAL_ERROR,
        _ => error_code::INTERNAL_ERROR,
    };
    A2AError::new(code, err.message.clone())
}

/// Return the WebSocket close code that should be used when an A2A error is
/// fatal to the connection. Most errors are non-fatal, so the function returns
/// `None`. See Section 7.2 of the spec.
pub fn close_code_for_fatal(err: &A2AError) -> Option<u16> {
    match err.code {
        error_code::PARSE_ERROR => Some(close_codes::PROTOCOL_ERROR),
        error_code::VERSION_NOT_SUPPORTED => Some(close_codes::VERSION_NOT_SUPPORTED),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a2a_error_maps_to_canonical_type_strings() {
        let cases = [
            (error_code::TASK_NOT_FOUND, error_types::TASK_NOT_FOUND),
            (
                error_code::TASK_NOT_CANCELABLE,
                error_types::TASK_NOT_CANCELABLE,
            ),
            (
                error_code::PUSH_NOTIFICATION_NOT_SUPPORTED,
                error_types::PUSH_NOTIFICATION_NOT_SUPPORTED,
            ),
            (
                error_code::UNSUPPORTED_OPERATION,
                error_types::UNSUPPORTED_OPERATION,
            ),
            (
                error_code::CONTENT_TYPE_NOT_SUPPORTED,
                error_types::CONTENT_TYPE_NOT_SUPPORTED,
            ),
            (
                error_code::INVALID_AGENT_RESPONSE,
                error_types::INVALID_AGENT_RESPONSE,
            ),
            (
                error_code::EXTENDED_CARD_NOT_CONFIGURED,
                error_types::EXTENDED_CARD_NOT_CONFIGURED,
            ),
            (
                error_code::EXTENSION_SUPPORT_REQUIRED,
                error_types::EXTENSION_SUPPORT_REQUIRED,
            ),
            (
                error_code::VERSION_NOT_SUPPORTED,
                error_types::VERSION_NOT_SUPPORTED,
            ),
            (error_code::PARSE_ERROR, error_types::JSON_PARSE),
            (error_code::INVALID_REQUEST, error_types::INVALID_REQUEST),
            (error_code::METHOD_NOT_FOUND, error_types::METHOD_NOT_FOUND),
            (error_code::INVALID_PARAMS, error_types::INVALID_PARAMS),
            (error_code::INTERNAL_ERROR, error_types::INTERNAL),
        ];

        for (code, expected) in cases {
            let err = A2AError::new(code, "msg");
            let ws = a2a_error_to_ws_error(&err);
            assert_eq!(ws.error_type, expected, "code {code} should map to {expected}");
            assert_eq!(ws.message, "msg");
        }
    }

    #[test]
    fn unknown_a2a_code_collapses_to_internal_error_type() {
        let err = A2AError::new(99999, "boom");
        assert_eq!(a2a_error_to_ws_error(&err).error_type, error_types::INTERNAL);
    }

    #[test]
    fn ws_error_round_trips_to_a2a_error_codes() {
        let cases = [
            (error_types::TASK_NOT_FOUND, error_code::TASK_NOT_FOUND),
            (
                error_types::TASK_NOT_CANCELABLE,
                error_code::TASK_NOT_CANCELABLE,
            ),
            (
                error_types::UNSUPPORTED_OPERATION,
                error_code::UNSUPPORTED_OPERATION,
            ),
            (error_types::JSON_PARSE, error_code::PARSE_ERROR),
            (error_types::INVALID_REQUEST, error_code::INVALID_REQUEST),
            (error_types::METHOD_NOT_FOUND, error_code::METHOD_NOT_FOUND),
            (error_types::INVALID_PARAMS, error_code::INVALID_PARAMS),
            (error_types::INTERNAL, error_code::INTERNAL_ERROR),
        ];

        for (type_str, expected_code) in cases {
            let err = ws_error_to_a2a_error(&WsErrorObject {
                error_type: type_str.to_string(),
                message: "msg".to_string(),
                details: None,
            });
            assert_eq!(err.code, expected_code, "{type_str} -> {expected_code}");
            assert_eq!(err.message, "msg");
        }
    }

    #[test]
    fn ws_error_with_unknown_type_falls_back_to_internal_error() {
        let err = ws_error_to_a2a_error(&WsErrorObject {
            error_type: "MysteryError".to_string(),
            message: "hmm".to_string(),
            details: None,
        });
        assert_eq!(err.code, error_code::INTERNAL_ERROR);
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
}
