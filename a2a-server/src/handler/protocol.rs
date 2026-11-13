// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

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

    match event {
        StreamResponse::Task(task) if task.status.state == TaskState::AuthRequired => {
            Some(task.id.clone())
        }
        StreamResponse::StatusUpdate(update) if update.status.state == TaskState::AuthRequired => {
            Some(update.task_id.clone())
        }
        _ => None,
    }
}

/// An artifact-only update explicitly omits its message.
pub(super) fn is_artifact_only(req: &SendMessageRequest) -> bool {
    req.message.is_none() && !req.artifact_updates.is_empty()
}

/// The task a send request names: the message's, or for an artifact-only
/// request the one its updates target.
pub(super) fn request_task_id(req: &SendMessageRequest) -> Option<String> {
    if is_artifact_only(req) {
        req.artifact_updates
            .iter()
            .map(|update| update.task_id.as_str())
            .find(|task_id| !task_id.is_empty())
            .map(str::to_string)
    } else {
        req.message
            .as_ref()
            .and_then(|message| message.task_id.clone())
    }
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
    fn update_events_route_to_their_task_and_auth_required_interrupts_unary_calls() {
        let artifact = StreamResponse::ArtifactUpdate(
            serde_json::from_value(serde_json::json!({
                "taskId":"task", "contextId":"context",
                "artifact":{"artifactId":"artifact", "parts":[{"text":"artifact"}]}
            }))
            .unwrap(),
        );
        let message = StreamResponse::MessageUpdate(
            serde_json::from_value(serde_json::json!({
                "taskId":"task", "contextId":"context",
                "message":{"messageId":"message", "role":"ROLE_USER", "parts":[{"text":"message"}]}
            }))
            .unwrap(),
        );
        assert_eq!(task_id_for_event(&artifact), Some("task"));
        assert_eq!(task_id_for_event(&message), Some("task"));
        let task: Task = serde_json::from_value(serde_json::json!({
            "id":"task", "contextId":"context", "status":{"state":"TASK_STATE_AUTH_REQUIRED"}
        }))
        .unwrap();
        let status = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task.id.clone(),
            context_id: task.context_id.clone(),
            status: task.status.clone(),
            metadata: None,
            generation: 0,
        });
        let request = send_req(None, None);
        assert_eq!(
            should_interrupt_non_streaming(&request, &StreamResponse::Task(task)),
            Some("task".into())
        );
        assert_eq!(
            should_interrupt_non_streaming(&request, &status),
            Some("task".into())
        );
    }
}
