// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

use a2a::{
    Message, Part, Role, SendMessageRequest, StreamResponse, shared_task_attribution,
};

const MEDIA_TASK: &str = "application/vnd.a2a.task+json";
const MEDIA_STATUS_UPDATE: &str = "application/vnd.a2a.task-status-update+json";
// v1.0 fallback: no artifact_updates field until v1.1
const MEDIA_ARTIFACT_UPDATE: &str = "application/vnd.a2a.task-artifact-update+json";

/// Translate a peer `StreamResponse` into a `SendMessageRequest` per spec §5.1.
///
/// Returns `None` for variants that should not be relayed (currently none, but
/// callers should still handle the `None` case for forward-compatibility).
///
/// Session IDs are rewritten to `receiving_context_id` / `receiving_task_id`.
/// `sender_identity` is injected as the shared-task `message-sender` attribution.
pub fn translate_peer_response(
    response: &StreamResponse,
    sender_identity: &str,
    receiving_context_id: &str,
    receiving_task_id: &str,
) -> Option<SendMessageRequest> {
    let attribution = shared_task_attribution(sender_identity);

    let parts: Vec<Part> = match response {
        StreamResponse::Task(task) => {
            let v = serde_json::to_value(task).ok()?;
            vec![Part::data(v).with_media_type(MEDIA_TASK)]
        }

        StreamResponse::StatusUpdate(event) => {
            let data_part = {
                let v = serde_json::to_value(event).ok()?;
                Part::data(v).with_media_type(MEDIA_STATUS_UPDATE)
            };
            match &event.status.message {
                Some(msg) => {
                    // Forward original message parts then append the event envelope.
                    let mut parts = msg.parts.clone();
                    parts.push(data_part);
                    parts
                }
                None => vec![data_part],
            }
        }

        // v1.0 fallback: translate to Part::data since artifact_updates doesn't exist until v1.1
        StreamResponse::ArtifactUpdate(event) => {
            let v = serde_json::to_value(event).ok()?;
            vec![Part::data(v).with_media_type(MEDIA_ARTIFACT_UPDATE)]
        }

        StreamResponse::Message(msg) => msg.parts.clone(),
    };

    let mut message = Message::new(Role::Agent, parts);
    message.context_id = Some(receiving_context_id.to_string());
    message.task_id = Some(receiving_task_id.to_string());
    message.metadata = Some(attribution);

    Some(SendMessageRequest {
        message,
        configuration: None,
        metadata: None,
        tenant: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{
        Artifact, EXTENSION_URI_SHARED_TASK, StreamResponse, Task, TaskArtifactUpdateEvent,
        TaskState, TaskStatus, TaskStatusUpdateEvent,
    };

    const SENDER: &str = "agent-orders";
    const CTX: &str = "ctx-recv";
    const TASK: &str = "task-recv";

    fn translate(r: &StreamResponse) -> SendMessageRequest {
        translate_peer_response(r, SENDER, CTX, TASK).expect("should produce a request")
    }

    fn assert_common(req: &SendMessageRequest) {
        assert_eq!(req.message.context_id.as_deref(), Some(CTX));
        assert_eq!(req.message.task_id.as_deref(), Some(TASK));
        let meta = req.message.metadata.as_ref().unwrap();
        let inner = meta[EXTENSION_URI_SHARED_TASK].as_object().unwrap();
        assert_eq!(inner["message-sender"], serde_json::json!(SENDER));
    }

    #[test]
    fn task_variant_produces_data_part() {
        let task = Task {
            id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        let req = translate(&StreamResponse::Task(task));
        assert_common(&req);
        assert_eq!(req.message.parts.len(), 1);
        assert_eq!(
            req.message.parts[0].media_type.as_deref(),
            Some(MEDIA_TASK)
        );
    }

    #[test]
    fn status_update_without_message_produces_single_data_part() {
        let event = TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        };
        let req = translate(&StreamResponse::StatusUpdate(event));
        assert_common(&req);
        assert_eq!(req.message.parts.len(), 1);
        assert_eq!(
            req.message.parts[0].media_type.as_deref(),
            Some(MEDIA_STATUS_UPDATE)
        );
    }

    #[test]
    fn status_update_with_message_appends_data_part() {
        let inner_msg = Message::new(
            Role::Agent,
            vec![Part::text("Order shipped"), Part::text("yesterday")],
        );
        let event = TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: Some(inner_msg),
                timestamp: None,
            },
            metadata: None,
        };
        let req = translate(&StreamResponse::StatusUpdate(event));
        assert_common(&req);
        // 2 original text parts + 1 appended data envelope
        assert_eq!(req.message.parts.len(), 3);
        assert_eq!(
            req.message.parts[2].media_type.as_deref(),
            Some(MEDIA_STATUS_UPDATE)
        );
    }

    #[test]
    fn artifact_update_translates_as_data_part_v1_fallback() {
        let event = TaskArtifactUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            artifact: Artifact {
                artifact_id: "a1".into(),
                name: None,
                description: None,
                parts: vec![Part::text("data")],
                metadata: None,
                extensions: None,
            },
            append: Some(false),
            last_chunk: Some(true),
            metadata: None,
        };
        let req = translate(&StreamResponse::ArtifactUpdate(event));
        assert_common(&req);
        assert_eq!(req.message.parts.len(), 1);
        assert_eq!(
            req.message.parts[0].media_type.as_deref(),
            Some(MEDIA_ARTIFACT_UPDATE)
        );
    }

    #[test]
    fn message_variant_passes_through_parts() {
        let original = Message::new(
            Role::Agent,
            vec![Part::text("hello"), Part::text("world")],
        );
        let req = translate(&StreamResponse::Message(original));
        assert_common(&req);
        assert_eq!(req.message.parts.len(), 2);
        // Parts passed through, no extra media_type added
        assert!(req.message.parts[0].media_type.is_none());
    }
}
