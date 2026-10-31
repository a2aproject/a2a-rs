// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Interpret request versions, response policies, and protocol errors.

use super::delivery::request_task_id;
use crate::middleware::ServiceParams;
use crate::task_store::{apply_history_length, apply_timeline_length};
use a2a::*;

/// The protocol version a request announced through `A2A-Version`, or "1.0"
/// when it sent none.
pub(super) fn requested_protocol_version(params: &ServiceParams) -> String {
    params
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(SVC_PARAM_VERSION))
        .and_then(|(_, values)| values.first())
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or("1.0")
        .to_string()
}

/// Whether `version` (an `A2A-Version` value) is 1.1 or later within 1.x.
pub(super) fn is_at_least_1_1(version: &str) -> bool {
    let mut parts = version.trim().split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts
        .next()
        .map_or(Some(0), |part| part.parse::<u32>().ok());
    matches!((major, minor), (Some(1), Some(minor)) if minor >= 1)
}

/// Applies the response-trimming parameters shared by every operation that
/// returns a task: the deprecated `historyLength` and its replacement
/// `timelineLength`.
pub(super) fn apply_lengths(
    task: &mut Task,
    history_length: Option<i32>,
    timeline_length: Option<i32>,
) {
    apply_history_length(task, history_length);
    apply_timeline_length(task, timeline_length);
}

pub(super) fn is_terminal_event(event: &StreamResponse) -> bool {
    match event {
        StreamResponse::Task(task) => task.status.state.is_terminal(),
        StreamResponse::StatusUpdate(update) => update.status.state.is_terminal(),
        _ => false,
    }
}

pub(super) fn task_id_for_event(event: &StreamResponse) -> Option<&str> {
    match event {
        StreamResponse::Task(task) => Some(&task.id),
        StreamResponse::StatusUpdate(update) => Some(&update.task_id),
        StreamResponse::ArtifactUpdate(update) => Some(&update.task_id),
        StreamResponse::MessageUpdate(update) => Some(&update.task_id),
        StreamResponse::Message(_) | StreamResponse::Error(_) => None,
        _ => None,
    }
}

pub(super) fn should_interrupt_non_streaming(
    req: &SendMessageRequest,
    event: &StreamResponse,
) -> Option<TaskId> {
    if req
        .configuration
        .as_ref()
        .and_then(|config| config.return_immediately)
        .unwrap_or(false)
    {
        return match event {
            StreamResponse::Message(_) => None,
            _ => task_id_for_event(event).map(ToOwned::to_owned),
        };
    }

    // An interrupted state ends a blocking call (A2A §3.1.1): the turn may
    // stay open to take more input, so the stream ending is not what to wait for.
    let interrupted =
        |state: &TaskState| matches!(state, TaskState::AuthRequired | TaskState::InputRequired);
    match event {
        StreamResponse::Task(task) if interrupted(&task.status.state) => Some(task.id.clone()),
        StreamResponse::StatusUpdate(update) if interrupted(&update.status.state) => {
            Some(update.task_id.clone())
        }
        _ => None,
    }
}

/// The error for a request on a unary or streaming call that could not be
/// carried out. A message the running agent will not take is reported as the
/// task being busy, which is what these calls said before they could join a
/// running turn.
pub(super) fn unary_refusal(rejection: Rejection, request: &SendMessageRequest) -> A2AError {
    match rejection {
        Rejection::Declined(reason) => A2AError::invalid_request(format!(
            "task execution is already in progress: {} ({reason})",
            request_task_id(request).unwrap_or_default()
        )),
        Rejection::Failed(error) => error,
    }
}

/// Why a live request was not carried out.
pub(super) enum Rejection {
    /// The bounded queue is full, so the request cannot be accepted now. The
    /// client may retry; this is not an error in the request.
    Declined(String),
    Failed(A2AError),
}

impl From<A2AError> for Rejection {
    fn from(error: A2AError) -> Self {
        Rejection::Failed(error)
    }
}

impl Rejection {
    /// The error to give a caller that is not on a stream yet.
    pub(super) fn into_error(self) -> A2AError {
        match self {
            Rejection::Declined(reason) => A2AError::unsupported_operation(reason),
            Rejection::Failed(error) => error,
        }
    }

    /// The non-fatal rejection of one request on a live stream: the code is the
    /// one the equivalent unary operation would report.
    pub(super) fn into_stream_error(
        self,
        task_id: &str,
        reference_id: Option<String>,
    ) -> StreamResponse {
        let (code, message) = match self {
            Rejection::Declined(reason) => ("UNAVAILABLE".to_string(), reason),
            Rejection::Failed(error) => (stream_error_code(&error), error.message),
        };
        StreamResponse::Error(StreamError {
            task_id: task_id.to_string(),
            code,
            message,
            reference_id: reference_id.filter(|id| !id.is_empty()),
            metadata: None,
        })
    }
}

fn stream_error_code(error: &A2AError) -> String {
    match error.code {
        error_code::INVALID_PARAMS
        | error_code::INVALID_REQUEST
        | error_code::PARSE_ERROR
        | error_code::CONTENT_TYPE_NOT_SUPPORTED => "INVALID_ARGUMENT".to_string(),
        error_code::TASK_NOT_FOUND => "NOT_FOUND".to_string(),
        code => error_reason(code).to_string(),
    }
}

/// The generation an event reports, if it reports one.
pub(super) fn generation_of(event: &StreamResponse) -> Option<i64> {
    match event {
        StreamResponse::Task(task) => Some(task.generation),
        StreamResponse::StatusUpdate(update) => Some(update.generation),
        StreamResponse::ArtifactUpdate(update) => Some(update.generation),
        StreamResponse::MessageUpdate(update) => Some(update.generation),
        _ => None,
    }
}

/// Whether `task` holds the message `request` carries (always true for a
/// request that has no message id to look for).
pub(super) fn records_message(task: &Task, request: &StreamRequest) -> bool {
    let StreamRequestPayload::SendMessage(send) = &request.payload else {
        return true;
    };
    let id = &send.message.message_id;
    id.is_empty()
        || task.history.iter().flatten().any(|m| &m.message_id == id)
        || task.timeline.iter().flatten().any(
            |entry| matches!(&entry.content, TimelineContent::Message(m) if &m.message_id == id),
        )
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::handler::test_support::{make_message, send_req};

    #[test]
    fn immediate_unary_responses_do_not_discard_a_direct_message() {
        let mut request = send_req(None, None);
        request.configuration = Some(SendMessageConfiguration {
            return_immediately: Some(true),
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: None,
            timeline_length: None,
            if_generation_match: None,
        });
        let reply = StreamResponse::Message(make_message());
        assert_eq!(should_interrupt_non_streaming(&request, &reply), None);
        assert_eq!(task_id_for_event(&reply), None);
    }

    #[test]
    fn live_rejections_preserve_codes_and_omit_empty_reference_ids() {
        for (error, expected) in [
            (A2AError::invalid_params("bad input"), "INVALID_ARGUMENT"),
            (A2AError::task_not_found("gone"), "NOT_FOUND"),
            (A2AError::internal("store unavailable"), "INTERNAL_ERROR"),
        ] {
            let StreamResponse::Error(rejection) =
                Rejection::from(error.clone()).into_stream_error("task", Some(String::new()))
            else {
                panic!("expected stream error")
            };
            assert_eq!(rejection.code, expected);
            assert_eq!(rejection.message, error.message);
            assert_eq!(rejection.reference_id, None);
            let returned = Rejection::from(error.clone()).into_error();
            assert_eq!(returned.code, error.code);
            assert_eq!(returned.message, error.message);
        }
        assert_eq!(
            Rejection::Declined("busy".into()).into_error().code,
            error_code::UNSUPPORTED_OPERATION
        );
        let refusal = unary_refusal(Rejection::Declined("full".into()), &send_req(None, None));
        assert_eq!(refusal.code, error_code::INVALID_REQUEST);
        assert!(refusal.message.contains("full"));
    }
}
