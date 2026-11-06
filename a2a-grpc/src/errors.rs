// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;

use a2a::errordetails::{self, TypedDetail};
use a2a::{A2AError, a2a_error_from_details, error_reason, protocol_error_code};
use prost::Message;
use tonic_types::{ErrorDetails, FieldViolation as GrpcFieldViolation, StatusExt};

/// Trailer metadata an error response may carry, measured the way HTTP/2 counts a
/// header list (RFC 9113 §6.5.2: name + value + 32 per field, before HPACK). 8 KiB is
/// what gRPC clients accept by default: grpc-java's Netty transport fails the RPC above
/// 8192 (`maxInboundMetadataSize`, soft and hard limit alike), gRPC C-core rejects a
/// random share of RPCs between 8 KB and 16 KB (`grpc.max_metadata_size` and
/// `grpc.absolute_max_metadata_size`, over the whole batch of headers), and grpc-go is
/// moving to 8 KiB (`upcomingDefaultHeaderListSize`); hyper, behind tonic clients,
/// accepts 16 KiB. The peer's own `SETTINGS_MAX_HEADER_LIST_SIZE`, which grpc-go and
/// grpc-java check on the sending side, is not visible to a tonic service, so the
/// default is the budget.
pub const MAX_ERROR_TRAILER_BYTES: usize = 8 * 1024;

/// The fields of an error response that tonic does not write, as HTTP/2 counts them: the
/// `:status` pseudo-header every response carries (RFC 9113 §8.3.2) and the `date` an
/// origin server sends (RFC 9110 §6.6.1; hyper adds it, 29 bytes).
const RESPONSE_FIELD_BYTES: usize = (7 + 3 + 32) + (4 + 29 + 32);

/// Convert an A2A error to a tonic gRPC status.
///
/// `ErrorInfo` (reason from the code, protocol domain, merged metadata) and `BadRequest`
/// are encoded as their `google.rpc` types, and `google.protobuf.Struct` details as
/// `Struct` (numbers pass through `f64`, so integers above 2^53 lose precision). A detail
/// of any other type has no protobuf encoding here and is dropped with a warning rather
/// than sent under a type URL that is not its own. The trailers are kept within
/// [`MAX_ERROR_TRAILER_BYTES`], the message and the `ErrorInfo` and `BadRequest` first:
/// the detail that does not fit and the ones after it are dropped with a warning.
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

    let status = tonic::Status::with_error_details(code, &err.message, details);

    // `ErrorDetails` cannot carry other details, so Struct details go after its
    // `ErrorInfo` and `BadRequest`.
    let mut others = err
        .details
        .as_deref()
        .unwrap_or_default()
        .iter()
        .filter(|detail| {
            detail.type_url != errordetails::ERROR_INFO_TYPE
                && detail.type_url != errordetails::BAD_REQUEST_TYPE
        })
        .filter_map(|detail| {
            if detail.type_url != errordetails::STRUCT_TYPE {
                tracing::warn!(
                    type_url = %detail.type_url,
                    "error detail type has no gRPC encoding, dropping it"
                );
                return None;
            }
            Some(prost_types::Any {
                type_url: errordetails::STRUCT_TYPE.to_string(),
                value: a2a_pb::pbconv::to_proto_struct(&detail.value).encode_to_vec(),
            })
        })
        .peekable();
    if others.peek().is_none() {
        return status;
    }
    let mut rpc_status = tonic_types::pb::Status::decode(status.details())
        .expect("tonic just encoded this google.rpc.Status");
    let mut status = status;
    let mut dropped = 0;
    for any in others {
        if dropped == 0 {
            rpc_status.details.push(any);
            let candidate =
                tonic::Status::with_details(code, &err.message, rpc_status.encode_to_vec().into());
            if trailer_bytes(&candidate) <= MAX_ERROR_TRAILER_BYTES {
                status = candidate;
                continue;
            }
            rpc_status.details.pop();
        }
        dropped += 1;
    }
    if dropped > 0 {
        tracing::warn!(
            dropped,
            limit = MAX_ERROR_TRAILER_BYTES,
            "gRPC error trailers over the size limit, dropping the extra details"
        );
    }
    status
}

/// The HTTP/2 header list size of the response tonic sends for `status`: `content-type`,
/// `grpc-status`, the percent-encoded `grpc-message` and the base64
/// `grpc-status-details-bin`, plus [`RESPONSE_FIELD_BYTES`].
fn trailer_bytes(status: &tonic::Status) -> usize {
    RESPONSE_FIELD_BYTES
        + status
            .clone()
            .into_http::<()>()
            .headers()
            .iter()
            .map(|(name, value)| name.as_str().len() + value.len() + 32)
            .sum::<usize>()
}

/// Convert a tonic gRPC status to an A2A error.
///
/// `ErrorInfo`, `BadRequest` and `google.protobuf.Struct` details are kept as
/// [`TypedDetail`]s and folded into the code and message the way the JSON-RPC and
/// REST clients do it ([`a2a_error_from_details`]). A status that no A2A error maps
/// to becomes `INTERNAL_ERROR` with the gRPC code named in the message.
pub fn status_to_a2a_error(status: &tonic::Status) -> A2AError {
    use a2a::error_code;
    let details = typed_details(status);
    let transport_code = match status.code() {
        tonic::Code::NotFound => Some(error_code::TASK_NOT_FOUND),
        tonic::Code::FailedPrecondition => Some(error_code::TASK_NOT_CANCELABLE),
        tonic::Code::Unimplemented => Some(error_code::METHOD_NOT_FOUND),
        tonic::Code::InvalidArgument => Some(error_code::INVALID_PARAMS),
        tonic::Code::Internal => Some(error_code::INTERNAL_ERROR),
        _ => None,
    };
    let (code, message) = match protocol_error_code(&details).or(transport_code) {
        Some(code) => (code, status.message().to_owned()),
        // No A2A error maps to this status: keep the gRPC code, which the message alone would lose.
        None => (
            error_code::INTERNAL_ERROR,
            format!("gRPC {:?}: {}", status.code(), status.message()),
        ),
    };
    a2a_error_from_details(code, message, details)
}

/// The `google.rpc.Status` details a client understands, in wire order. a2a-go's server
/// sends its details other than `ErrorInfo` as `google.protobuf.Struct`.
fn typed_details(status: &tonic::Status) -> Vec<TypedDetail> {
    let Ok(rpc_status) = tonic_types::pb::Status::decode(status.details()) else {
        return Vec::new();
    };
    rpc_status
        .details
        .iter()
        .filter_map(|any| {
            let value = any.value.as_slice();
            Some(match any.type_url.as_str() {
                errordetails::ERROR_INFO_TYPE => {
                    let info = tonic_types::pb::ErrorInfo::decode(value).ok()?;
                    let metadata = (!info.metadata.is_empty()).then_some(info.metadata);
                    TypedDetail::error_info(info.reason, info.domain, metadata)
                }
                errordetails::BAD_REQUEST_TYPE => {
                    let bad_request = tonic_types::pb::BadRequest::decode(value).ok()?;
                    let violations = bad_request
                        .field_violations
                        .into_iter()
                        .map(|violation| errordetails::FieldViolation {
                            field: violation.field,
                            description: violation.description,
                        })
                        .collect();
                    TypedDetail::bad_request(violations)
                }
                errordetails::STRUCT_TYPE => TypedDetail::from_struct(
                    a2a_pb::pbconv::from_proto_struct(&prost_types::Struct::decode(value).ok()?),
                ),
                _ => return None,
            })
        })
        .collect()
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
        assert_eq!(err.message, "gRPC Cancelled: cancelled");
    }

    #[test]
    fn test_message_preserved() {
        let err = A2AError::new(error_code::TASK_NOT_FOUND, "task xyz not found");
        let status = a2a_error_to_status(&err);
        assert_eq!(status.message(), "task xyz not found");

        let back = status_to_a2a_error(&status);
        assert_eq!(back.message, "task xyz not found");
    }

    #[test]
    fn test_status_details_round_trip_to_a2a_error() {
        let metadata = HashMap::from([("tenant".to_string(), "example".to_string())]);
        let err = A2AError::invalid_params("invalid message").with_details(vec![
            errordetails::TypedDetail::error_info(
                "INVALID_PARAMS",
                errordetails::PROTOCOL_DOMAIN,
                Some(metadata),
            ),
            errordetails::TypedDetail::bad_request(vec![errordetails::FieldViolation {
                field: "message.parts".to_string(),
                description: "at least one part is required".to_string(),
            }]),
        ]);

        let decoded = status_to_a2a_error(&a2a_error_to_status(&err));

        assert_eq!(decoded.code, error_code::INVALID_PARAMS);
        assert_eq!(
            decoded.message,
            "invalid message: message.parts: at least one part is required"
        );
        let details = decoded
            .details
            .expect("details should survive the gRPC round trip");
        let error_info = details
            .iter()
            .find(|detail| detail.type_url == errordetails::ERROR_INFO_TYPE)
            .expect("ErrorInfo detail should be preserved");
        assert_eq!(
            error_info.value.get("reason").and_then(|v| v.as_str()),
            Some("INVALID_PARAMS")
        );
        assert_eq!(
            error_info.value.get("domain").and_then(|v| v.as_str()),
            Some(errordetails::PROTOCOL_DOMAIN)
        );
        assert_eq!(
            error_info
                .value
                .get("metadata")
                .and_then(|v| v.get("tenant"))
                .and_then(|v| v.as_str()),
            Some("example")
        );
        let bad_request = details
            .iter()
            .find(|detail| detail.type_url == errordetails::BAD_REQUEST_TYPE)
            .expect("BadRequest detail should be preserved");
        let violation = &bad_request.value["fieldViolations"][0];
        assert_eq!(violation["field"].as_str(), Some("message.parts"));
        assert_eq!(
            violation["description"].as_str(),
            Some("at least one part is required")
        );
    }

    #[test]
    fn test_error_info_without_metadata_round_trips_without_a_metadata_key() {
        let status = a2a_error_to_status(&A2AError::new(error_code::TASK_NOT_FOUND, "missing"));

        let decoded = status_to_a2a_error(&status);

        let details = decoded.details.expect("ErrorInfo should be preserved");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].type_url, errordetails::ERROR_INFO_TYPE);
        assert!(!details[0].value.contains_key("metadata"));
    }

    #[test]
    fn test_transport_status_names_its_grpc_code_in_the_message() {
        let cases = [
            (tonic::Code::Unauthenticated, "Unauthenticated"),
            (tonic::Code::PermissionDenied, "PermissionDenied"),
            (tonic::Code::Unavailable, "Unavailable"),
            (tonic::Code::DeadlineExceeded, "DeadlineExceeded"),
        ];

        for (grpc_code, name) in cases {
            let err = status_to_a2a_error(&tonic::Status::new(grpc_code, "no credentials"));
            assert_eq!(err.code, error_code::INTERNAL_ERROR);
            assert_eq!(err.message, format!("gRPC {name}: no credentials"));
            assert!(err.details.is_none());
        }
    }

    #[test]
    fn test_grpc_gives_the_same_error_as_the_jsonrpc_client() {
        // The server puts ErrorInfo first in a gRPC status and last in JSON-RPC `error.data`.
        let err = A2AError::new(error_code::INTERNAL_ERROR, "invalid message").with_details(vec![
            errordetails::TypedDetail::bad_request(vec![errordetails::FieldViolation {
                field: "message.parts".to_string(),
                description: "at least one part is required".to_string(),
            }]),
        ]);

        let over_grpc = status_to_a2a_error(&a2a_error_to_status(&err));
        let jsonrpc = err.to_jsonrpc_error();
        let details: Vec<TypedDetail> = jsonrpc
            .data
            .and_then(|data| data.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|detail| serde_json::from_value(detail).ok())
            .collect();
        let over_jsonrpc = a2a_error_from_details(jsonrpc.code, jsonrpc.message, details);

        assert_eq!(over_grpc.code, over_jsonrpc.code);
        assert_eq!(over_grpc.message, over_jsonrpc.message);
        assert_eq!(
            over_grpc.message,
            "invalid message: message.parts: at least one part is required"
        );
    }

    #[test]
    fn test_foreign_error_info_on_an_unmapped_code_keeps_the_detail_and_the_prefix() {
        let status = tonic::Status::with_error_details(
            tonic::Code::Unauthenticated,
            "token expired",
            ErrorDetails::with_error_info("TOKEN_EXPIRED", "example.com", HashMap::new()),
        );

        let decoded = status_to_a2a_error(&status);

        assert_eq!(decoded.code, error_code::INTERNAL_ERROR);
        assert_eq!(decoded.message, "gRPC Unauthenticated: token expired");
        assert_eq!(
            decoded.details,
            Some(vec![errordetails::TypedDetail::error_info(
                "TOKEN_EXPIRED",
                "example.com",
                None
            )])
        );
    }

    #[test]
    fn test_bad_request_alone_folds_into_the_message() {
        let mut details = ErrorDetails::new();
        details.set_bad_request(vec![GrpcFieldViolation::new(
            "message.parts",
            "at least one part is required",
        )]);
        let status = tonic::Status::with_error_details(
            tonic::Code::InvalidArgument,
            "invalid message",
            details,
        );

        let decoded = status_to_a2a_error(&status);

        assert_eq!(decoded.code, error_code::INVALID_PARAMS);
        assert_eq!(
            decoded.message,
            "invalid message: message.parts: at least one part is required"
        );
        assert_eq!(
            decoded.details,
            Some(vec![errordetails::TypedDetail::bad_request(vec![
                errordetails::FieldViolation {
                    field: "message.parts".to_string(),
                    description: "at least one part is required".to_string(),
                }
            ])])
        );
    }

    #[test]
    fn test_struct_details_are_decoded_like_a2a_go_sends_them() {
        let fields = HashMap::from([
            ("hint".to_string(), "slow down".into()),
            ("retryAfter".to_string(), "30s".into()),
        ]);
        let rpc_status = tonic_types::pb::Status {
            code: tonic::Code::Internal as i32,
            message: "boom".to_string(),
            details: vec![prost_types::Any {
                type_url: errordetails::STRUCT_TYPE.to_string(),
                value: a2a_pb::pbconv::to_proto_struct(&fields).encode_to_vec(),
            }],
        };
        let status = tonic::Status::with_details(
            tonic::Code::Internal,
            "boom",
            rpc_status.encode_to_vec().into(),
        );

        let decoded = status_to_a2a_error(&status);

        assert_eq!(decoded.code, error_code::INTERNAL_ERROR);
        assert_eq!(decoded.message, "boom");
        assert_eq!(
            decoded.details,
            Some(vec![errordetails::TypedDetail::from_struct(fields)])
        );
    }

    #[test]
    fn test_other_google_rpc_details_are_dropped() {
        let mut details = ErrorDetails::new();
        details.set_retry_info(Some(std::time::Duration::from_secs(5)));
        let status =
            tonic::Status::with_error_details(tonic::Code::Unavailable, "try later", details);

        let decoded = status_to_a2a_error(&status);

        assert_eq!(decoded.code, error_code::INTERNAL_ERROR);
        assert_eq!(decoded.message, "gRPC Unavailable: try later");
        assert!(decoded.details.is_none());
    }

    #[test]
    fn test_struct_details_round_trip_through_a_grpc_status() {
        let fields = HashMap::from([
            ("hint".to_string(), "slow down".into()),
            ("retryAfter".to_string(), 30.into()),
        ]);
        let err = A2AError::new(error_code::INTERNAL_ERROR, "boom")
            .with_details(vec![errordetails::TypedDetail::from_struct(fields.clone())]);

        let decoded = status_to_a2a_error(&a2a_error_to_status(&err));

        assert_eq!(decoded.code, error_code::INTERNAL_ERROR);
        assert_eq!(decoded.message, "boom");
        assert_eq!(
            decoded.details,
            Some(vec![
                errordetails::TypedDetail::error_info(
                    "INTERNAL_ERROR",
                    errordetails::PROTOCOL_DOMAIN,
                    None
                ),
                errordetails::TypedDetail::from_struct(fields),
            ])
        );
    }

    #[test]
    fn test_error_info_and_bad_request_stay_first_on_the_wire() {
        let fields = HashMap::from([("hint".to_string(), "slow down".into())]);
        let violation = errordetails::FieldViolation {
            field: "message.parts".to_string(),
            description: "at least one part is required".to_string(),
        };
        let err = A2AError::invalid_params("invalid message").with_details(vec![
            errordetails::TypedDetail::from_struct(fields.clone()),
            errordetails::TypedDetail::bad_request(vec![violation.clone()]),
        ]);

        let status = a2a_error_to_status(&err);

        let rpc_status = tonic_types::pb::Status::decode(status.details()).unwrap();
        assert_eq!(rpc_status.code, tonic::Code::InvalidArgument as i32);
        assert_eq!(rpc_status.message, "invalid message");
        let type_urls: Vec<&str> = rpc_status
            .details
            .iter()
            .map(|any| any.type_url.as_str())
            .collect();
        assert_eq!(
            type_urls,
            [
                errordetails::ERROR_INFO_TYPE,
                errordetails::BAD_REQUEST_TYPE,
                errordetails::STRUCT_TYPE
            ]
        );
        assert_eq!(
            status_to_a2a_error(&status).details,
            Some(vec![
                errordetails::TypedDetail::error_info(
                    "INVALID_PARAMS",
                    errordetails::PROTOCOL_DOMAIN,
                    None
                ),
                errordetails::TypedDetail::bad_request(vec![violation]),
                errordetails::TypedDetail::from_struct(fields),
            ])
        );
    }

    #[test]
    fn test_details_of_other_types_are_not_sent_as_structs() {
        let fields = HashMap::from([("retryDelay".to_string(), "5s".into())]);
        let err = A2AError::new(error_code::INTERNAL_ERROR, "boom").with_details(vec![
            errordetails::TypedDetail::new("type.googleapis.com/google.rpc.RetryInfo", fields),
        ]);

        let decoded = status_to_a2a_error(&a2a_error_to_status(&err));

        assert_eq!(
            decoded.details,
            Some(vec![errordetails::TypedDetail::error_info(
                "INTERNAL_ERROR",
                errordetails::PROTOCOL_DOMAIN,
                None
            )])
        );
    }

    #[test]
    fn test_status_without_other_details_is_unchanged() {
        let metadata = HashMap::from([("tenant".to_string(), "example".to_string())]);
        let err = A2AError::invalid_params("invalid message").with_details(vec![
            errordetails::TypedDetail::error_info(
                "INVALID_PARAMS",
                errordetails::PROTOCOL_DOMAIN,
                Some(metadata.clone()),
            ),
            errordetails::TypedDetail::bad_request(vec![errordetails::FieldViolation {
                field: "message.parts".to_string(),
                description: "at least one part is required".to_string(),
            }]),
        ]);
        let mut expected = ErrorDetails::with_error_info(
            "INVALID_PARAMS",
            errordetails::PROTOCOL_DOMAIN,
            metadata,
        );
        expected.set_bad_request(vec![GrpcFieldViolation::new(
            "message.parts",
            "at least one part is required",
        )]);
        let expected = tonic::Status::with_error_details(
            tonic::Code::InvalidArgument,
            "invalid message",
            expected,
        );

        let status = a2a_error_to_status(&err);

        assert_eq!(status.code(), expected.code());
        assert_eq!(status.message(), expected.message());
        assert_eq!(status.details(), expected.details());
    }

    #[test]
    fn test_error_trailers_stay_within_the_default_grpc_metadata_limit() {
        let small = HashMap::from([("hint".to_string(), "slow down".into())]);
        let large = HashMap::from([(
            "blob".to_string(),
            "x".repeat(MAX_ERROR_TRAILER_BYTES).into(),
        )]);
        let err = A2AError::new(error_code::INTERNAL_ERROR, "boom").with_details(vec![
            errordetails::TypedDetail::from_struct(small.clone()),
            errordetails::TypedDetail::from_struct(large),
            errordetails::TypedDetail::from_struct(small.clone()),
        ]);

        let status = a2a_error_to_status(&err);

        assert!(trailer_bytes(&status) <= MAX_ERROR_TRAILER_BYTES);
        assert_eq!(status.message(), "boom");
        assert_eq!(
            status_to_a2a_error(&status).details,
            Some(vec![
                errordetails::TypedDetail::error_info(
                    "INTERNAL_ERROR",
                    errordetails::PROTOCOL_DOMAIN,
                    None
                ),
                errordetails::TypedDetail::from_struct(small),
            ])
        );
    }

    #[test]
    fn test_the_message_counts_against_the_trailer_budget() {
        let small = HashMap::from([("hint".to_string(), "slow down".into())]);
        let long_message = "m".repeat(MAX_ERROR_TRAILER_BYTES - 512);
        let err = A2AError::new(error_code::INTERNAL_ERROR, long_message.clone())
            .with_details(vec![errordetails::TypedDetail::from_struct(small.clone())]);

        let with_long_message = a2a_error_to_status(&err);
        let with_short_message = a2a_error_to_status(
            &A2AError::new(error_code::INTERNAL_ERROR, "boom")
                .with_details(vec![errordetails::TypedDetail::from_struct(small.clone())]),
        );

        assert_eq!(with_long_message.message(), long_message);
        assert_eq!(
            status_to_a2a_error(&with_long_message)
                .details
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            status_to_a2a_error(&with_short_message)
                .details
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn test_trailer_bytes_counts_fields_as_http2_does() {
        let status = tonic::Status::with_details(
            tonic::Code::Internal,
            "a b",
            prost::bytes::Bytes::from_static(b"abc"),
        );

        // content-type "application/grpc", grpc-status "13", grpc-message "a%20b",
        // grpc-status-details-bin "YWJj".
        assert_eq!(
            trailer_bytes(&status),
            RESPONSE_FIELD_BYTES + (12 + 16 + 32) + (11 + 2 + 32) + (12 + 5 + 32) + (23 + 4 + 32)
        );
    }
}
