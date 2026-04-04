// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::A2AError;

/// Convert an A2A error to a SLIMRPC status.
pub fn a2a_error_to_rpc_error(err: &A2AError) -> slim_bindings::RpcError {
    use a2a::error_code;

    match err.code {
        error_code::TASK_NOT_FOUND => slim_bindings::RpcError::not_found(err.message.clone()),
        error_code::TASK_NOT_CANCELABLE
        | error_code::EXTENSION_SUPPORT_REQUIRED
        | error_code::VERSION_NOT_SUPPORTED => {
            slim_bindings::RpcError::failed_precondition(err.message.clone())
        }
        error_code::PUSH_NOTIFICATION_NOT_SUPPORTED
        | error_code::UNSUPPORTED_OPERATION
        | error_code::EXTENDED_CARD_NOT_CONFIGURED => {
            slim_bindings::RpcError::unimplemented(err.message.clone())
        }
        error_code::CONTENT_TYPE_NOT_SUPPORTED
        | error_code::PARSE_ERROR
        | error_code::INVALID_REQUEST
        | error_code::INVALID_PARAMS => {
            slim_bindings::RpcError::invalid_argument(err.message.clone())
        }
        error_code::INVALID_AGENT_RESPONSE | error_code::INTERNAL_ERROR => {
            slim_bindings::RpcError::internal(err.message.clone())
        }
        _ => slim_bindings::RpcError::unknown(err.message.clone()),
    }
}

/// Convert a SLIMRPC status to an A2A error.
pub fn rpc_error_to_a2a_error(error: &slim_bindings::RpcError) -> A2AError {
    use a2a::error_code;
    use slim_bindings::RpcCode;

    let code = match error.code() {
        RpcCode::NotFound => error_code::TASK_NOT_FOUND,
        RpcCode::FailedPrecondition => error_code::TASK_NOT_CANCELABLE,
        RpcCode::Unimplemented => error_code::UNSUPPORTED_OPERATION,
        RpcCode::InvalidArgument => error_code::INVALID_PARAMS,
        RpcCode::DeadlineExceeded => error_code::INTERNAL_ERROR,
        RpcCode::Internal => error_code::INTERNAL_ERROR,
        RpcCode::Unavailable => error_code::INTERNAL_ERROR,
        RpcCode::Unauthenticated => error_code::INTERNAL_ERROR,
        _ => error_code::INTERNAL_ERROR,
    };

    A2AError::new(code, error.message())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::error_code;
    use slim_bindings::RpcCode;

    #[test]
    fn test_a2a_error_to_rpc_error_mapping() {
        let cases = [
            (error_code::TASK_NOT_FOUND, RpcCode::NotFound),
            (error_code::TASK_NOT_CANCELABLE, RpcCode::FailedPrecondition),
            (
                error_code::PUSH_NOTIFICATION_NOT_SUPPORTED,
                RpcCode::Unimplemented,
            ),
            (error_code::UNSUPPORTED_OPERATION, RpcCode::Unimplemented),
            (
                error_code::CONTENT_TYPE_NOT_SUPPORTED,
                RpcCode::InvalidArgument,
            ),
            (error_code::INVALID_AGENT_RESPONSE, RpcCode::Internal),
            (error_code::INVALID_PARAMS, RpcCode::InvalidArgument),
            (error_code::INTERNAL_ERROR, RpcCode::Internal),
        ];

        for (code, expected) in cases {
            let error = A2AError::new(code, "test");
            assert_eq!(a2a_error_to_rpc_error(&error).code(), expected);
        }
    }

    #[test]
    fn test_rpc_error_to_a2a_error_mapping() {
        let cases = [
            (RpcCode::NotFound, error_code::TASK_NOT_FOUND),
            (RpcCode::FailedPrecondition, error_code::TASK_NOT_CANCELABLE),
            (RpcCode::Unimplemented, error_code::UNSUPPORTED_OPERATION),
            (RpcCode::InvalidArgument, error_code::INVALID_PARAMS),
            (RpcCode::Internal, error_code::INTERNAL_ERROR),
        ];

        for (rpc_code, expected) in cases {
            let error = slim_bindings::RpcError::new(rpc_code, "test");
            assert_eq!(rpc_error_to_a2a_error(&error).code, expected);
        }
    }

    #[test]
    fn test_a2a_error_to_rpc_error_extended_mapping() {
        let cases = [
            (
                error_code::EXTENSION_SUPPORT_REQUIRED,
                RpcCode::FailedPrecondition,
            ),
            (
                error_code::VERSION_NOT_SUPPORTED,
                RpcCode::FailedPrecondition,
            ),
            (
                error_code::EXTENDED_CARD_NOT_CONFIGURED,
                RpcCode::Unimplemented,
            ),
            (error_code::PARSE_ERROR, RpcCode::InvalidArgument),
            (error_code::INVALID_REQUEST, RpcCode::InvalidArgument),
            (123_456, RpcCode::Unknown),
        ];

        for (code, expected) in cases {
            let error = A2AError::new(code, "test");
            assert_eq!(a2a_error_to_rpc_error(&error).code(), expected);
        }
    }

    #[test]
    fn test_rpc_error_to_a2a_error_extended_mapping() {
        let cases = [
            (RpcCode::DeadlineExceeded, error_code::INTERNAL_ERROR),
            (RpcCode::Unavailable, error_code::INTERNAL_ERROR),
            (RpcCode::Unauthenticated, error_code::INTERNAL_ERROR),
            (RpcCode::Unknown, error_code::INTERNAL_ERROR),
        ];

        for (rpc_code, expected) in cases {
            let error = slim_bindings::RpcError::new(rpc_code, "test");
            assert_eq!(rpc_error_to_a2a_error(&error).code, expected);
        }
    }
}
