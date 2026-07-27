// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;

use a2a::{A2AError, error_reason, errordetails, reason_to_error_code};
use tonic_types::{ErrorDetails, FieldViolation as GrpcFieldViolation, StatusExt};

/// Convert an A2A error to a tonic gRPC status code.
pub fn a2a_error_to_status(err: &A2AError) -> tonic::Status {
    use a2a::error_code;
    let code = match err.code {
        error_code::TASK_NOT_FOUND => tonic::Code::NotFound,
        error_code::TASK_NOT_CANCELABLE => tonic::Code::FailedPrecondition,
        error_code::PUSH_NOTIFICATION_NOT_SUPPORTED => tonic::Code::FailedPrecondition,
        error_code::UNSUPPORTED_OPERATION => tonic::Code::FailedPrecondition,
        error_code::CONTENT_TYPE_NOT_SUPPORTED => tonic::Code::InvalidArgument,
        error_code::INVALID_AGENT_RESPONSE => tonic::Code::Internal,
        error_code::EXTENDED_CARD_NOT_CONFIGURED => tonic::Code::FailedPrecondition,
        error_code::EXTENSION_SUPPORT_REQUIRED => tonic::Code::FailedPrecondition,
        error_code::VERSION_NOT_SUPPORTED => tonic::Code::FailedPrecondition,
        error_code::PARSE_ERROR => tonic::Code::InvalidArgument,
        error_code::INVALID_REQUEST => tonic::Code::InvalidArgument,
        error_code::METHOD_NOT_FOUND => tonic::Code::Unimplemented,
        error_code::INVALID_PARAMS => tonic::Code::InvalidArgument,
        error_code::INTERNAL_ERROR => tonic::Code::Internal,
        _ => tonic::Code::Unknown,
    };
    let metadata: HashMap<String, String> = err
        .details
        .as_deref()
        .and_then(|details| {
            details
                .iter()
                .find(|detail| detail.type_url == errordetails::ERROR_INFO_TYPE)
        })
        .and_then(|detail| detail.value.get("metadata"))
        .and_then(|metadata| metadata.as_object())
        .map(|metadata| {
            metadata
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut details = ErrorDetails::with_error_info(
        error_reason(err.code),
        errordetails::PROTOCOL_DOMAIN,
        metadata,
    );

    let mut field_violations = Vec::new();
    if let Some(error_details) = &err.details {
        for detail in error_details
            .iter()
            .filter(|detail| detail.type_url == errordetails::BAD_REQUEST_TYPE)
        {
            let Some(violations) = detail
                .value
                .get("fieldViolations")
                .and_then(|value| value.as_array())
            else {
                continue;
            };
            field_violations.extend(violations.iter().filter_map(|violation| {
                Some(GrpcFieldViolation::new(
                    violation.get("field")?.as_str()?,
                    violation.get("description")?.as_str()?,
                ))
            }));
        }
    }
    if !field_violations.is_empty() {
        details.set_bad_request(field_violations);
    }

    tonic::Status::with_error_details(code, &err.message, details)
}

/// Convert a tonic gRPC status to an A2A error.
pub fn status_to_a2a_error(status: &tonic::Status) -> A2AError {
    use a2a::error_code;
    let code = status
        .get_details_error_info()
        .filter(|detail| detail.domain == errordetails::PROTOCOL_DOMAIN)
        .and_then(|detail| reason_to_error_code(&detail.reason))
        .unwrap_or_else(|| match status.code() {
            tonic::Code::NotFound => error_code::TASK_NOT_FOUND,
            tonic::Code::FailedPrecondition => error_code::TASK_NOT_CANCELABLE,
            tonic::Code::Unimplemented => error_code::METHOD_NOT_FOUND,
            tonic::Code::InvalidArgument => error_code::INVALID_PARAMS,
            tonic::Code::Internal => error_code::INTERNAL_ERROR,
            _ => error_code::INTERNAL_ERROR,
        });
    A2AError::new(code, status.message())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use a2a::{error_code, errordetails};
    use tonic_types::{ErrorDetails, StatusExt};

    const FAILED_PRECONDITION_ERRORS: [(i32, &str); 6] = [
        (error_code::TASK_NOT_CANCELABLE, "TASK_NOT_CANCELABLE"),
        (
            error_code::PUSH_NOTIFICATION_NOT_SUPPORTED,
            "PUSH_NOTIFICATION_NOT_SUPPORTED",
        ),
        (error_code::UNSUPPORTED_OPERATION, "UNSUPPORTED_OPERATION"),
        (
            error_code::EXTENDED_CARD_NOT_CONFIGURED,
            "EXTENDED_AGENT_CARD_NOT_CONFIGURED",
        ),
        (
            error_code::EXTENSION_SUPPORT_REQUIRED,
            "EXTENSION_SUPPORT_REQUIRED",
        ),
        (error_code::VERSION_NOT_SUPPORTED, "VERSION_NOT_SUPPORTED"),
    ];
    const INVALID_ARGUMENT_ERRORS: [(i32, &str); 4] = [
        (
            error_code::CONTENT_TYPE_NOT_SUPPORTED,
            "CONTENT_TYPE_NOT_SUPPORTED",
        ),
        (error_code::PARSE_ERROR, "PARSE_ERROR"),
        (error_code::INVALID_REQUEST, "INVALID_REQUEST"),
        (error_code::INVALID_PARAMS, "INVALID_PARAMS"),
    ];

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
                tonic::Code::FailedPrecondition,
            ),
            (
                error_code::UNSUPPORTED_OPERATION,
                tonic::Code::FailedPrecondition,
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
            (tonic::Code::Unimplemented, error_code::METHOD_NOT_FOUND),
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
    fn test_failed_precondition_errors_round_trip_with_error_info() {
        for (code, expected_reason) in FAILED_PRECONDITION_ERRORS {
            let status = a2a_error_to_status(&A2AError::new(code, "test"));

            assert_eq!(status.code(), tonic::Code::FailedPrecondition);
            let error_info = status
                .get_details_error_info()
                .expect("A2A gRPC statuses should include ErrorInfo");
            assert_eq!(error_info.reason, expected_reason);
            assert_eq!(error_info.domain, errordetails::PROTOCOL_DOMAIN);

            let decoded = status_to_a2a_error(&status);
            assert_eq!(decoded.code, code, "failed to recover {expected_reason}");
        }
    }

    #[test]
    fn test_invalid_argument_errors_round_trip_with_error_info() {
        for (code, expected_reason) in INVALID_ARGUMENT_ERRORS {
            let status = a2a_error_to_status(&A2AError::new(code, "test"));

            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            let error_info = status
                .get_details_error_info()
                .expect("A2A gRPC statuses should include ErrorInfo");
            assert_eq!(error_info.reason, expected_reason);
            assert_eq!(error_info.domain, errordetails::PROTOCOL_DOMAIN);

            let decoded = status_to_a2a_error(&status);
            assert_eq!(decoded.code, code, "failed to recover {expected_reason}");
        }
    }

    #[test]
    fn test_a2a_error_details_are_preserved_in_grpc_status() {
        let metadata = HashMap::from([
            ("requestId".to_string(), "request-123".to_string()),
            ("tenant".to_string(), "example".to_string()),
        ]);
        let err = A2AError::invalid_params("invalid message").with_details(vec![
            errordetails::TypedDetail::error_info(
                "INVALID_PARAMS",
                errordetails::PROTOCOL_DOMAIN,
                Some(metadata.clone()),
            ),
            errordetails::TypedDetail::bad_request(vec![
                errordetails::FieldViolation {
                    field: "message.parts".to_string(),
                    description: "at least one part is required".to_string(),
                },
                errordetails::FieldViolation {
                    field: "message.role".to_string(),
                    description: "must be user or agent".to_string(),
                },
            ]),
        ]);

        let status = a2a_error_to_status(&err);

        let error_info = status
            .get_details_error_info()
            .expect("A2A gRPC statuses should include ErrorInfo");
        assert_eq!(error_info.metadata, metadata);

        let bad_request = status
            .get_details_bad_request()
            .expect("BadRequest details should be preserved");
        assert_eq!(bad_request.field_violations.len(), 2);
        assert_eq!(bad_request.field_violations[0].field, "message.parts");
        assert_eq!(
            bad_request.field_violations[0].description,
            "at least one part is required"
        );
        assert_eq!(bad_request.field_violations[1].field, "message.role");
        assert_eq!(
            bad_request.field_violations[1].description,
            "must be user or agent"
        );
    }

    #[test]
    fn test_protocol_error_info_recovers_precise_a2a_code() {
        for (expected_code, reason) in FAILED_PRECONDITION_ERRORS {
            let status = tonic::Status::with_error_details(
                tonic::Code::FailedPrecondition,
                "test",
                ErrorDetails::with_error_info(
                    reason,
                    errordetails::PROTOCOL_DOMAIN,
                    HashMap::new(),
                ),
            );

            let decoded = status_to_a2a_error(&status);
            assert_eq!(decoded.code, expected_code, "failed to decode {reason}");
        }
    }

    #[test]
    fn test_untrusted_error_info_falls_back_to_grpc_code() {
        let cases = [
            ErrorDetails::with_error_info(
                "PUSH_NOTIFICATION_NOT_SUPPORTED",
                "example.com",
                HashMap::new(),
            ),
            ErrorDetails::with_error_info(
                "UNKNOWN_REASON",
                errordetails::PROTOCOL_DOMAIN,
                HashMap::new(),
            ),
        ];

        for details in cases {
            let status =
                tonic::Status::with_error_details(tonic::Code::FailedPrecondition, "test", details);
            let decoded = status_to_a2a_error(&status);
            assert_eq!(decoded.code, error_code::TASK_NOT_CANCELABLE);
        }
    }

    #[test]
    fn test_malformed_error_details_fall_back_to_grpc_code() {
        let status = tonic::Status::with_details(
            tonic::Code::FailedPrecondition,
            "test",
            prost::bytes::Bytes::from_static(b"not a google.rpc.Status"),
        );

        let decoded = status_to_a2a_error(&status);
        assert_eq!(decoded.code, error_code::TASK_NOT_CANCELABLE);
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
                tonic::Code::FailedPrecondition,
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
