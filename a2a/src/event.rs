// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::types::*;

// ---------------------------------------------------------------------------
// StreamResponse (field-presence union — 4 variants)
// ---------------------------------------------------------------------------

/// A streaming event. Uses field-presence serialization for wire compatibility.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "fuzzing", derive(arbitrary::Arbitrary))]
#[non_exhaustive]
pub enum StreamResponse {
    Task(Task),
    Message(Message),
    StatusUpdate(TaskStatusUpdateEvent),
    ArtifactUpdate(TaskArtifactUpdateEvent),
    /// A message appended to the task (A2A 1.1).
    MessageUpdate(TaskMessageUpdateEvent),
    /// A non-fatal rejection of a single client request on a live stream
    /// (A2A 1.1).
    Error(StreamError),
}

impl Serialize for StreamResponse {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        match self {
            StreamResponse::Task(t) => map.serialize_entry("task", t)?,
            StreamResponse::Message(m) => map.serialize_entry("message", m)?,
            StreamResponse::StatusUpdate(s) => map.serialize_entry("statusUpdate", s)?,
            StreamResponse::ArtifactUpdate(a) => map.serialize_entry("artifactUpdate", a)?,
            StreamResponse::MessageUpdate(m) => map.serialize_entry("messageUpdate", m)?,
            StreamResponse::Error(e) => map.serialize_entry("error", e)?,
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for StreamResponse {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw: HashMap<String, Value> = HashMap::deserialize(deserializer)?;
        if let Some(v) = raw.get("message") {
            Ok(StreamResponse::Message(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("task") {
            Ok(StreamResponse::Task(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("statusUpdate") {
            Ok(StreamResponse::StatusUpdate(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("artifactUpdate") {
            Ok(StreamResponse::ArtifactUpdate(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("messageUpdate") {
            Ok(StreamResponse::MessageUpdate(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else if let Some(v) = raw.get("error") {
            Ok(StreamResponse::Error(
                serde_json::from_value(v.clone()).map_err(serde::de::Error::custom)?,
            ))
        } else {
            Err(serde::de::Error::custom("unknown StreamResponse variant"))
        }
    }
}

// ---------------------------------------------------------------------------
// TaskStatusUpdateEvent
// ---------------------------------------------------------------------------

/// Event: a task's status has changed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "fuzzing", derive(arbitrary::Arbitrary))]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusUpdateEvent {
    pub task_id: TaskId,
    pub context_id: String,
    pub status: TaskStatus,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "fuzzing", arbitrary(default))]
    pub metadata: Option<HashMap<String, Value>>,

    /// The task generation after this update. Zero means the peer did not
    /// report one.
    #[serde(
        default,
        with = "crate::types::int64",
        skip_serializing_if = "crate::types::is_zero"
    )]
    pub generation: i64,
}

// ---------------------------------------------------------------------------
// TaskArtifactUpdateEvent
// ---------------------------------------------------------------------------

/// Event: an artifact has been generated or updated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "fuzzing", derive(arbitrary::Arbitrary))]
#[serde(rename_all = "camelCase")]
pub struct TaskArtifactUpdateEvent {
    pub task_id: TaskId,
    pub context_id: String,
    pub artifact: Artifact,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_chunk: Option<bool>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "fuzzing", arbitrary(default))]
    pub metadata: Option<HashMap<String, Value>>,

    /// The task generation after this update. Zero means the peer did not
    /// report one.
    #[serde(
        default,
        with = "crate::types::int64",
        skip_serializing_if = "crate::types::is_zero"
    )]
    pub generation: i64,
}

// ---------------------------------------------------------------------------
// TaskMessageUpdateEvent
// ---------------------------------------------------------------------------

/// Event: a message was appended to the task (e.g. client input). Sent only
/// to clients that announced protocol version 1.1 or later.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "fuzzing", derive(arbitrary::Arbitrary))]
#[serde(rename_all = "camelCase")]
pub struct TaskMessageUpdateEvent {
    pub task_id: TaskId,
    pub context_id: String,
    pub message: Message,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "fuzzing", arbitrary(default))]
    pub metadata: Option<HashMap<String, Value>>,

    /// The task generation after the message was appended.
    #[serde(
        default,
        with = "crate::types::int64",
        skip_serializing_if = "crate::types::is_zero"
    )]
    pub generation: i64,
}

// ---------------------------------------------------------------------------
// StreamError
// ---------------------------------------------------------------------------

/// A non-fatal error reported on a live stream. It rejects a single client
/// request without terminating the stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "fuzzing", derive(arbitrary::Arbitrary))]
#[serde(rename_all = "camelCase")]
pub struct StreamError {
    /// The task the rejected request applied to, when known.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub task_id: TaskId,

    /// A machine-readable identifier, using the same codes as the equivalent
    /// unary operation (e.g. `INVALID_ARGUMENT`).
    pub code: String,

    /// A human readable description of why the request was rejected.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,

    /// The `message_id` of the rejected client message, or the `artifact_id`
    /// of the rejected artifact update.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference_id: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "fuzzing", arbitrary(default))]
    pub metadata: Option<HashMap<String, Value>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_stream_response_status_update_serde() {
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            generation: 0,
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        let json = serde_json::to_string(&event).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("statusUpdate").is_some());

        let back: StreamResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, StreamResponse::StatusUpdate(_)));
    }

    #[test]
    fn test_stream_response_task_serde() {
        let task = Task {
            generation: 0,
            timeline: None,
            id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        let event = StreamResponse::Task(task.clone());
        let json = serde_json::to_string(&event).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("task").is_some());
        let back: StreamResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, StreamResponse::Task(_)));
    }

    #[test]
    fn test_stream_response_message_serde() {
        let msg = Message::new(Role::Agent, vec![Part::text("hello")]);
        let event = StreamResponse::Message(msg);
        let json = serde_json::to_string(&event).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("message").is_some());
        let back: StreamResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, StreamResponse::Message(_)));
    }

    #[test]
    fn test_stream_response_artifact_update_serde() {
        let event = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            generation: 0,
            task_id: "t1".into(),
            context_id: "c1".into(),
            artifact: Artifact {
                end_generation: None,
                role: None,
                start_generation: None,
                artifact_id: "a1".into(),
                name: None,
                description: None,
                parts: vec![],
                metadata: None,
                extensions: None,
            },
            append: Some(true),
            last_chunk: Some(false),
            metadata: None,
        });
        let json = serde_json::to_string(&event).unwrap();
        let v: Value = serde_json::from_str(&json).unwrap();
        assert!(v.get("artifactUpdate").is_some());
        let back: StreamResponse = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, StreamResponse::ArtifactUpdate(_)));
    }

    #[test]
    fn test_stream_response_unknown_variant() {
        let json = r#"{"unknown": {}}"#;
        let result = serde_json::from_str::<StreamResponse>(json);
        assert!(result.is_err());
    }

    #[test]
    fn test_task_status_update_event_with_metadata() {
        let mut meta = HashMap::new();
        meta.insert("key".to_string(), Value::String("val".to_string()));
        let event = TaskStatusUpdateEvent {
            generation: 0,
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: Some(Message::new(Role::Agent, vec![])),
                timestamp: None,
            },
            metadata: Some(meta),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: TaskStatusUpdateEvent = serde_json::from_str(&json).unwrap();
        assert!(back.metadata.is_some());
        assert_eq!(back.status.state, TaskState::Working);
    }

    #[test]
    fn test_task_artifact_update_event_full() {
        let event = TaskArtifactUpdateEvent {
            generation: 0,
            task_id: "t1".into(),
            context_id: "c1".into(),
            artifact: Artifact {
                end_generation: None,
                role: None,
                start_generation: None,
                artifact_id: "a1".into(),
                name: Some("file.txt".into()),
                description: Some("A file".into()),
                parts: vec![Part::text("content")],
                metadata: None,
                extensions: None,
            },
            append: None,
            last_chunk: Some(true),
            metadata: None,
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: TaskArtifactUpdateEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.last_chunk, Some(true));
        assert!(back.append.is_none());
    }
}
