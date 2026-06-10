// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;

use a2a_pb::protojson::{
    AgentCapabilities, AgentCard, AgentInterface, AgentProvider, AgentSkill, Artifact,
    AuthenticationInfo, Message, Part, Role, SendMessageConfiguration, SendMessageRequest,
    StreamResponse, Task, TaskPushNotificationConfig, TaskState, TaskStatus, TaskStatusUpdateEvent,
    part, stream_response,
};
use pbjson_types::{Struct, Timestamp, Value, value};
use serde_json::{Value as JsonValue, json};

fn string_value(value: &str) -> Value {
    Value {
        kind: Some(value::Kind::StringValue(value.to_string())),
    }
}

fn number_value(value: f64) -> Value {
    Value {
        kind: Some(value::Kind::NumberValue(value)),
    }
}

fn bool_value(value: bool) -> Value {
    Value {
        kind: Some(value::Kind::BoolValue(value)),
    }
}

fn struct_value(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Struct {
    Struct {
        fields: fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect::<HashMap<_, _>>(),
    }
}

#[test]
fn send_message_request_protojson_roundtrip() {
    let request = SendMessageRequest {
        tenant: "tenant-1".to_string(),
        message: Some(Message {
            message_id: "msg-1".to_string(),
            context_id: "ctx-1".to_string(),
            task_id: "task-1".to_string(),
            role: Role::User as i32,
            parts: vec![Part {
                metadata: Some(struct_value([("segment", number_value(1.0))])),
                filename: String::new(),
                media_type: "text/plain".to_string(),
                content: Some(part::Content::Text("hello".to_string())),
            }],
            metadata: Some(struct_value([("origin", string_value("cli"))])),
            extensions: vec!["https://example.com/ext/log".to_string()],
            reference_task_ids: vec!["task-root".to_string()],
        }),
        configuration: Some(SendMessageConfiguration {
            accepted_output_modes: vec!["text/plain".to_string(), "application/json".to_string()],
            task_push_notification_config: Some(TaskPushNotificationConfig {
                tenant: "tenant-1".to_string(),
                id: "push-1".to_string(),
                task_id: String::new(),
                url: "https://example.com/hooks/tasks".to_string(),
                token: "tok-1".to_string(),
                authentication: Some(AuthenticationInfo {
                    scheme: "Bearer".to_string(),
                    credentials: "secret".to_string(),
                }),
            }),
            history_length: Some(5),
            return_immediately: true,
        }),
        metadata: Some(struct_value([
            ("requestId", string_value("req-1")),
            ("trace", bool_value(true)),
        ])),
    };

    let json = serde_json::to_value(&request).unwrap();
    assert_eq!(json["tenant"], json!("tenant-1"));
    assert_eq!(json["message"]["messageId"], json!("msg-1"));
    assert_eq!(json["message"]["role"], json!("ROLE_USER"));
    assert_eq!(json["message"]["parts"][0]["text"], json!("hello"));
    assert_eq!(
        json["configuration"]["acceptedOutputModes"],
        json!(["text/plain", "application/json"])
    );
    assert_eq!(json["configuration"]["historyLength"], json!(5));
    assert_eq!(json["metadata"]["requestId"], json!("req-1"));
    assert_eq!(json["metadata"]["trace"], json!(true));

    let roundtrip: SendMessageRequest = serde_json::from_value(json).unwrap();
    assert_eq!(request, roundtrip);
}

#[test]
fn task_protojson_roundtrip_serializes_protojson_primitives() {
    let task = Task {
        id: "task-1".to_string(),
        context_id: "ctx-1".to_string(),
        status: Some(TaskStatus {
            state: TaskState::Completed as i32,
            message: Some(Message {
                message_id: "msg-status".to_string(),
                context_id: "ctx-1".to_string(),
                task_id: "task-1".to_string(),
                role: Role::Agent as i32,
                parts: vec![Part {
                    metadata: None,
                    filename: String::new(),
                    media_type: "text/plain".to_string(),
                    content: Some(part::Content::Text("done".to_string())),
                }],
                metadata: None,
                extensions: vec![],
                reference_task_ids: vec![],
            }),
            timestamp: Some(Timestamp {
                seconds: 1_700_000_000,
                nanos: 0,
            }),
        }),
        artifacts: vec![Artifact {
            artifact_id: "artifact-1".to_string(),
            name: "payload".to_string(),
            description: String::new(),
            parts: vec![Part {
                metadata: None,
                filename: "payload.bin".to_string(),
                media_type: "application/octet-stream".to_string(),
                content: Some(part::Content::Raw(vec![0, 1, 2, 3])),
            }],
            metadata: Some(struct_value([("checksum", string_value("abc123"))])),
            extensions: vec!["ext://artifact".to_string()],
        }],
        history: vec![Message {
            message_id: "msg-history".to_string(),
            context_id: "ctx-1".to_string(),
            task_id: "task-1".to_string(),
            role: Role::User as i32,
            parts: vec![Part {
                metadata: None,
                filename: String::new(),
                media_type: "application/json".to_string(),
                content: Some(part::Content::Data(Value {
                    kind: Some(value::Kind::StructValue(struct_value([(
                        "score",
                        number_value(0.98),
                    )]))),
                })),
            }],
            metadata: None,
            extensions: vec![],
            reference_task_ids: vec![],
        }],
        metadata: Some(struct_value([("source", string_value("protojson-test"))])),
    };

    let json = serde_json::to_value(&task).unwrap();
    let timestamp = json["status"]["timestamp"].as_str().unwrap();
    assert_eq!(json["status"]["state"], json!("TASK_STATE_COMPLETED"));
    assert!(matches!(
        timestamp,
        "2023-11-14T22:13:20Z" | "2023-11-14T22:13:20+00:00"
    ));
    assert_eq!(json["artifacts"][0]["parts"][0]["raw"], json!("AAECAw=="));
    assert_eq!(json["history"][0]["parts"][0]["data"]["score"], json!(0.98));
    assert_eq!(json["metadata"]["source"], json!("protojson-test"));

    let roundtrip: Task = serde_json::from_value(json).unwrap();
    assert_eq!(task, roundtrip);
}

#[test]
fn agent_card_protojson_deserializes_null_skills_as_empty() {
    let card = AgentCard {
        name: "Directory Agent".to_string(),
        description: "Indexes and searches files".to_string(),
        supported_interfaces: vec![AgentInterface {
            url: "https://example.com/a2a".to_string(),
            protocol_binding: "HTTP+JSON".to_string(),
            tenant: "tenant-1".to_string(),
            protocol_version: "1.0".to_string(),
        }],
        provider: Some(AgentProvider {
            url: "https://example.com".to_string(),
            organization: "Example".to_string(),
        }),
        version: "1.2.3".to_string(),
        documentation_url: Some("https://example.com/docs".to_string()),
        capabilities: Some(AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            extensions: vec![],
            extended_agent_card: Some(true),
        }),
        security_schemes: HashMap::new(),
        security_requirements: vec![],
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["application/json".to_string()],
        skills: vec![AgentSkill {
            id: "search".to_string(),
            name: "Search".to_string(),
            description: "Finds files and symbols".to_string(),
            tags: vec!["search".to_string(), "files".to_string()],
            examples: vec!["find Cargo.toml".to_string()],
            input_modes: vec!["text/plain".to_string()],
            output_modes: vec!["application/json".to_string()],
            security_requirements: vec![],
        }],
        signatures: vec![],
        icon_url: None,
    };

    let mut json = serde_json::to_value(&card).unwrap();
    json.as_object_mut()
        .unwrap()
        .insert("skills".to_string(), JsonValue::Null);

    let parsed: AgentCard = serde_json::from_value(json).unwrap();

    let mut expected = card;
    expected.skills.clear();
    assert_eq!(parsed, expected);
}

/// Spec §5.7: implementations SHOULD ignore unrecognized fields in messages,
/// allowing for forward compatibility. Unknown fields must be tolerated both
/// at the top level and inside nested messages.
#[test]
fn task_protojson_decode_ignores_unknown_fields() {
    let json = json!({
        "id": "task-1",
        "contextId": "ctx-1",
        "status": {
            "state": "TASK_STATE_COMPLETED",
            "futureStatusField": "ignored"
        },
        "futureTopLevelField": {"nested": ["values", 1, true]}
    });

    let task = a2a_pb::protojson_conv::from_value::<a2a::Task>(json)
        .expect("unknown fields must be ignored");
    assert_eq!(task.id, "task-1");
    assert_eq!(task.context_id, "ctx-1");
    assert_eq!(task.status.state, a2a::TaskState::Completed);
}

/// Unknown *enum string values* are still rejected: only unrecognized fields
/// are tolerated (`ignore_unknown_enum_variants` is intentionally not
/// enabled).
#[test]
fn task_protojson_decode_rejects_unknown_enum_values() {
    let json = json!({
        "id": "task-1",
        "contextId": "ctx-1",
        "status": {"state": "TASK_STATE_FROM_THE_FUTURE"}
    });

    let result = a2a_pb::protojson_conv::from_value::<a2a::Task>(json);
    assert!(
        result.is_err(),
        "unknown enum values must still be rejected"
    );
}

#[test]
fn stream_response_protojson_roundtrip_serializes_oneof_payloads() {
    let response = StreamResponse {
        payload: Some(stream_response::Payload::StatusUpdate(
            TaskStatusUpdateEvent {
                task_id: "task-42".to_string(),
                context_id: "ctx-42".to_string(),
                status: Some(TaskStatus {
                    state: TaskState::Working as i32,
                    message: Some(Message {
                        message_id: "msg-progress".to_string(),
                        context_id: "ctx-42".to_string(),
                        task_id: "task-42".to_string(),
                        role: Role::Agent as i32,
                        parts: vec![Part {
                            metadata: None,
                            filename: String::new(),
                            media_type: "text/plain".to_string(),
                            content: Some(part::Content::Text("working".to_string())),
                        }],
                        metadata: None,
                        extensions: vec![],
                        reference_task_ids: vec![],
                    }),
                    timestamp: Some(Timestamp {
                        seconds: 1_700_000_300,
                        nanos: 0,
                    }),
                }),
                metadata: Some(struct_value([("progress", number_value(0.5))])),
            },
        )),
    };

    let json = serde_json::to_value(&response).unwrap();
    assert!(json.get("statusUpdate").is_some());
    assert_eq!(
        json["statusUpdate"]["status"]["state"],
        json!("TASK_STATE_WORKING")
    );
    assert_eq!(json["statusUpdate"]["metadata"]["progress"], json!(0.5));

    let roundtrip: StreamResponse = serde_json::from_value(json).unwrap();
    assert_eq!(response, roundtrip);
}
