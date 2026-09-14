// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Session binding stamped into emitted output (9.6).
//!
//! 9.5 splits local runtime facts from durable proof, and 9.6 keeps the former
//! inspectable after a result leaves the process by recording which spawn
//! produced it. These are local-scope facts only: a consumer may correlate them
//! with a spawn but must not read them as cross-host identity.

use serde_json::{Map, Value};

use crate::wire::Variant;

/// 9.6 metadata key, provisional until the binding is registered.
pub const BINDING_URI: &str = "https://a2a-protocol.org/bindings/stdio/v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionBinding {
    pub session_id: String,
    pub pid: Option<u32>,
    pub variant: Variant,
    /// Digest the host computed over the launch descriptor. 9.6 forbids
    /// secrets or raw environment values here.
    pub launch_digest: Option<String>,
}

impl SessionBinding {
    /// 9.6 wants `sessionId` and `pid`, and permits `variant` and
    /// `launchDigest`. Absent optionals are omitted rather than sent as null.
    pub fn to_value(&self) -> Value {
        let mut out = Map::new();
        out.insert(
            "sessionId".to_string(),
            Value::String(self.session_id.clone()),
        );
        if let Some(pid) = self.pid {
            out.insert("pid".to_string(), Value::Number(pid.into()));
        }
        out.insert(
            "variant".to_string(),
            Value::String(self.variant.as_str().to_string()),
        );
        if let Some(digest) = &self.launch_digest {
            out.insert("launchDigest".to_string(), Value::String(digest.clone()));
        }
        Value::Object(out)
    }
}

/// Stamp `binding` into every `Task`, `Message` and `Artifact` in a response.
///
/// The walk ignores container shape, so nested artifacts, history and stream
/// events are all reached without this module knowing any response layout. An
/// existing entry is never replaced: `Task.history` can outlive a spawn (8.4),
/// and restamping it would credit this session with older output.
pub fn stamp(value: &mut Value, binding: &Value) {
    match value {
        Value::Array(items) => {
            for item in items {
                stamp(item, binding);
            }
        }
        Value::Object(map) => {
            for nested in map.values_mut() {
                stamp(nested, binding);
            }
            if !is_emitted_object(map) {
                return;
            }
            let slot = map
                .entry("metadata")
                .or_insert_with(|| Value::Object(Map::new()));
            // ProtoJSON may render an absent Struct as null rather than omit it.
            if slot.is_null() {
                *slot = Value::Object(Map::new());
            }
            if let Value::Object(metadata) = slot {
                metadata
                    .entry(BINDING_URI)
                    .or_insert_with(|| binding.clone());
            }
        }
        _ => {}
    }
}

/// 9.6 names exactly three emitted types, and each key here belongs to only one
/// message in `a2a.proto`. `contextId`, `role` and `parts` are deliberately not
/// required: ProtoJSON omits proto3 defaults, so any of them may be absent.
/// `status` pairs with `id` because the update events carry `taskId` instead.
fn is_emitted_object(map: &Map<String, Value>) -> bool {
    map.contains_key("messageId")
        || map.contains_key("artifactId")
        || (map.contains_key("id") && map.contains_key("status"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{Artifact, Message, Role, Task, TaskState, TaskStatus};
    use serde_json::json;

    fn binding() -> Value {
        SessionBinding {
            session_id: "s-1".into(),
            pid: Some(42137),
            variant: Variant::Json,
            launch_digest: None,
        }
        .to_value()
    }

    fn stamped(value: &Value) -> &Value {
        &value["metadata"][BINDING_URI]
    }

    #[test]
    fn variant_token_matches_the_serde_rename() {
        for v in [Variant::Json, Variant::Proto] {
            assert_eq!(serde_json::to_value(v).unwrap(), json!(v.as_str()));
        }
    }

    #[test]
    fn binding_omits_absent_optionals() {
        let v = SessionBinding {
            session_id: "s-1".into(),
            pid: None,
            variant: Variant::Proto,
            launch_digest: None,
        }
        .to_value();
        assert_eq!(v, json!({"sessionId": "s-1", "variant": "stdio-proto"}));
    }

    #[test]
    fn binding_carries_pid_and_digest_when_known() {
        let v = SessionBinding {
            session_id: "s-1".into(),
            pid: Some(7),
            variant: Variant::Json,
            launch_digest: Some("sha256:abc".into()),
        }
        .to_value();
        assert_eq!(v["pid"], json!(7));
        assert_eq!(v["launchDigest"], json!("sha256:abc"));
    }

    /// The discriminators are guesses about ProtoJSON unless a real payload
    /// proves them, so this builds one through the same conversion the server
    /// uses and checks every nesting level 9.6 mentions.
    #[test]
    fn a_real_protojson_task_is_stamped_at_every_level() {
        let task = Task {
            id: "t-1".into(),
            context_id: "c-1".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: Some(Message::new(Role::Agent, vec![])),
                timestamp: None,
            },
            artifacts: Some(vec![Artifact {
                artifact_id: "a-1".into(),
                name: None,
                description: None,
                parts: vec![],
                metadata: None,
                extensions: None,
            }]),
            history: Some(vec![Message::new(Role::User, vec![])]),
            metadata: None,
        };
        let mut v = a2a_pb::protojson_conv::to_value(&task).expect("task converts to ProtoJSON");

        stamp(&mut v, &binding());

        assert_eq!(stamped(&v)["sessionId"], json!("s-1"));
        assert_eq!(stamped(&v)["pid"], json!(42137));
        assert_eq!(stamped(&v["artifacts"][0])["sessionId"], json!("s-1"));
        assert_eq!(stamped(&v["history"][0])["sessionId"], json!("s-1"));
        assert_eq!(
            stamped(&v["status"]["message"])["sessionId"],
            json!("s-1"),
            "the message inside a status is emitted too"
        );
    }

    #[test]
    fn update_events_are_not_mistaken_for_tasks() {
        // 9.6 names Task, Message and Artifact only. A status update carries
        // `taskId` and `status`, which must not look like a task.
        let mut v = json!({
            "taskId": "t-1",
            "contextId": "c-1",
            "status": {"state": "TASK_STATE_WORKING"},
        });
        stamp(&mut v, &binding());
        assert!(v.get("metadata").is_none(), "got {v}");
    }

    #[test]
    fn an_artifact_inside_an_update_event_is_still_stamped() {
        let mut v = json!({
            "taskId": "t-1",
            "contextId": "c-1",
            "artifact": {"artifactId": "a-1", "parts": []},
        });
        stamp(&mut v, &binding());
        assert_eq!(stamped(&v["artifact"])["sessionId"], json!("s-1"));
        assert!(v.get("metadata").is_none(), "the event itself is untouched");
    }

    #[test]
    fn an_existing_binding_is_left_alone() {
        // 8.4 respawn: history can predate this session, so whatever stamped it
        // first stays.
        let older = json!({"sessionId": "s-0", "pid": 1});
        let mut v = json!({
            "id": "t-1",
            "status": {"state": "TASK_STATE_COMPLETED"},
            "metadata": {BINDING_URI: older.clone()},
        });
        stamp(&mut v, &binding());
        assert_eq!(*stamped(&v), older);
    }

    #[test]
    fn unrelated_metadata_survives() {
        let mut v = json!({
            "messageId": "m-1",
            "metadata": {"https://example.com/ext": {"keep": true}},
        });
        stamp(&mut v, &binding());
        assert_eq!(
            v["metadata"]["https://example.com/ext"],
            json!({"keep": true})
        );
        assert_eq!(stamped(&v)["sessionId"], json!("s-1"));
    }

    #[test]
    fn null_metadata_is_replaced_with_an_object() {
        let mut v = json!({"messageId": "m-1", "metadata": Value::Null});
        stamp(&mut v, &binding());
        assert_eq!(stamped(&v)["sessionId"], json!("s-1"));
    }

    #[test]
    fn agent_cards_and_scalars_are_untouched() {
        // `AgentSkill` has an `id` but no `status`, so it must not match.
        let mut v = json!({
            "name": "card",
            "skills": [{"id": "s", "name": "n", "description": "d", "tags": []}],
            "count": 3,
        });
        let before = v.clone();
        stamp(&mut v, &binding());
        assert_eq!(v, before);
    }

    #[test]
    fn a_null_body_is_not_a_panic() {
        let mut v = Value::Null;
        stamp(&mut v, &binding());
        assert_eq!(v, Value::Null);
    }
}
