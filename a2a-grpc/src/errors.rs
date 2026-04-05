// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::A2AError;

/// Convert an A2A error to a tonic gRPC status code.
pub fn a2a_error_to_status(err: &A2AError) -> tonic::Status {
    use a2a::error_code;
    let code = match err.code {
        error_code::TASK_NOT_FOUND => tonic::Code::NotFound,
        error_code::TASK_NOT_CANCELABLE => tonic::Code::FailedPrecondition,
        error_code::PUSH_NOTIFICATION_NOT_SUPPORTED => tonic::Code::Unimplemented,
        error_code::UNSUPPORTED_OPERATION => tonic::Code::Unimplemented,
        error_code::CONTENT_TYPE_NOT_SUPPORTED => tonic::Code::InvalidArgument,
        error_code::INVALID_AGENT_RESPONSE => tonic::Code::Internal,
        error_code::EXTENDED_CARD_NOT_CONFIGURED => tonic::Code::Unimplemented,
        error_code::EXTENSION_SUPPORT_REQUIRED => tonic::Code::FailedPrecondition,
        error_code::VERSION_NOT_SUPPORTED => tonic::Code::FailedPrecondition,
        error_code::PARSE_ERROR => tonic::Code::InvalidArgument,
        error_code::INVALID_REQUEST => tonic::Code::InvalidArgument,
        error_code::METHOD_NOT_FOUND => tonic::Code::Unimplemented,
        error_code::INVALID_PARAMS => tonic::Code::InvalidArgument,
        error_code::INTERNAL_ERROR => tonic::Code::Internal,
        _ => tonic::Code::Unknown,
    };
    tonic::Status::new(code, &err.message)
}

/// Convert a tonic gRPC status to an A2A error.
pub fn status_to_a2a_error(status: &tonic::Status) -> A2AError {
    use a2a::error_code;
    let code = match status.code() {
        tonic::Code::NotFound => error_code::TASK_NOT_FOUND,
        tonic::Code::FailedPrecondition => error_code::TASK_NOT_CANCELABLE,
        tonic::Code::Unimplemented => error_code::UNSUPPORTED_OPERATION,
        tonic::Code::InvalidArgument => error_code::INVALID_PARAMS,
        tonic::Code::Internal => error_code::INTERNAL_ERROR,
        _ => error_code::INTERNAL_ERROR,
    };
    A2AError::new(code, status.message())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::error_code;

    #[test]
    fn test_a2a_error_to_status_mapping() {
        let cases = [
            (error_code::TASK_NOT_FOUND, tonic::Code::NotFound),
            (
                error_code::TASK_NOT_CANCELABLE,
                tonic::Code::FailedPrecondition,
            ),
            (
                error_code::PUSH_NOTIFICATION_NOT_SUPPORTED,
                tonic::Code::Unimplemented,
            ),
            (
                error_code::UNSUPPORTED_OPERATION,
                tonic::Code::Unimplemented,
            ),
            (
                error_code::CONTENT_TYPE_NOT_SUPPORTED,
                tonic::Code::InvalidArgument,
            ),
            (error_code::INVALID_AGENT_RESPONSE, tonic::Code::Internal),
            (error_code::INTERNAL_ERROR, tonic::Code::Internal),
            (error_code::PARSE_ERROR, tonic::Code::InvalidArgument),
            (error_code::INVALID_REQUEST, tonic::Code::InvalidArgument),
            (error_code::METHOD_NOT_FOUND, tonic::Code::Unimplemented),
            (error_code::INVALID_PARAMS, tonic::Code::InvalidArgument),
        ];

        for (code, expected_grpc) in cases {
            let err = A2AError::new(code, "test");
            let status = a2a_error_to_status(&err);
            assert_eq!(
                status.code(),
                expected_grpc,
                "code {code} should map to {expected_grpc:?}"
            );
        }
    }

    #[test]
    fn test_status_to_a2a_error_mapping() {
        let cases = [
            (tonic::Code::NotFound, error_code::TASK_NOT_FOUND),
            (
                tonic::Code::FailedPrecondition,
                error_code::TASK_NOT_CANCELABLE,
            ),
            (
                tonic::Code::Unimplemented,
                error_code::UNSUPPORTED_OPERATION,
            ),
            (tonic::Code::InvalidArgument, error_code::INVALID_PARAMS),
            (tonic::Code::Internal, error_code::INTERNAL_ERROR),
        ];

        for (grpc_code, expected_a2a) in cases {
            let status = tonic::Status::new(grpc_code, "test");
            let err = status_to_a2a_error(&status);
            assert_eq!(
                err.code, expected_a2a,
                "{grpc_code:?} should map to {expected_a2a}"
            );
        }
    }

    #[test]
    fn test_unknown_code_maps_to_unknown() {
        let err = A2AError::new(99999, "test");
        let status = a2a_error_to_status(&err);
        assert_eq!(status.code(), tonic::Code::Unknown);
    }

    #[test]
    fn test_additional_a2a_error_to_status_mappings() {
        let cases = [
            (
                error_code::EXTENDED_CARD_NOT_CONFIGURED,
                tonic::Code::Unimplemented,
            ),
            (
                error_code::EXTENSION_SUPPORT_REQUIRED,
                tonic::Code::FailedPrecondition,
            ),
            (
                error_code::VERSION_NOT_SUPPORTED,
                tonic::Code::FailedPrecondition,
            ),
        ];

        for (code, expected_grpc) in cases {
            let err = A2AError::new(code, "test");
            let status = a2a_error_to_status(&err);
            assert_eq!(status.code(), expected_grpc);
        }
    }

    #[test]
    fn test_unknown_status_maps_to_internal_error() {
        let status = tonic::Status::new(tonic::Code::Cancelled, "cancelled");
        let err = status_to_a2a_error(&status);
        assert_eq!(err.code, error_code::INTERNAL_ERROR);
        assert_eq!(err.message, "cancelled");
    }

    #[test]
    fn test_message_preserved() {
        let err = A2AError::new(error_code::TASK_NOT_FOUND, "task xyz not found");
        let status = a2a_error_to_status(&err);
        assert_eq!(status.message(), "task xyz not found");

        let back = status_to_a2a_error(&status);
        assert_eq!(back.message, "task xyz not found");
    }
}
