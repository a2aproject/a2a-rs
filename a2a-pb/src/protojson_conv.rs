// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use std::fmt;

use a2a::{
    AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetExtendedAgentCardRequest, GetTaskPushNotificationConfigRequest, GetTaskRequest,
    ListTaskPushNotificationConfigsRequest, ListTaskPushNotificationConfigsResponse,
    ListTasksRequest, ListTasksResponse, SendMessageRequest, SendMessageResponse, StreamResponse,
    SubscribeToTaskRequest, Task, TaskPushNotificationConfig,
};
use prost::Message;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

#[derive(Debug)]
pub enum ProtoJsonPayloadError {
    Json(serde_json::Error),
    Decode(prost::DecodeError),
    MissingPayload(&'static str),
}

impl fmt::Display for ProtoJsonPayloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(f, "ProtoJSON error: {error}"),
            Self::Decode(error) => write!(f, "protobuf transcode error: {error}"),
            Self::MissingPayload(type_name) => {
                write!(f, "ProtoJSON payload missing required data for {type_name}")
            }
        }
    }
}

impl std::error::Error for ProtoJsonPayloadError {}

pub trait ProtoJsonPayload: Sized {
    type Proto: Message + Default;
    type ProtoJson: Message + Default + Serialize + DeserializeOwned;

    fn to_proto(value: &Self) -> Self::Proto;
    fn try_from_proto(value: &Self::Proto) -> Result<Self, ProtoJsonPayloadError>;

    /// Adjusts the emitted JSON after the generic proto3-JSON conversion. A
    /// no-op for every type except where the wire mapping needs a field
    /// always present that proto3's own JSON convention omits when it's
    /// left at its default.
    fn normalize_json(_value: &mut Value) {}
}

pub fn to_value<T: ProtoJsonPayload>(value: &T) -> Result<Value, ProtoJsonPayloadError> {
    let proto = T::to_proto(value);
    let protojson: T::ProtoJson = transcode_message(&proto)?;
    let mut json = serde_json::to_value(protojson).map_err(ProtoJsonPayloadError::Json)?;
    normalize_timestamp_strings(&mut json);
    T::normalize_json(&mut json);
    Ok(json)
}

/// JSON field names in `a2a.proto` whose value is a `google.protobuf.Timestamp`.
const TIMESTAMP_FIELD_NAMES: &[&str] = &["timestamp", "statusTimestampAfter"];

/// `pbjson_types::Timestamp`'s own `Serialize` emits an RFC 3339 string with
/// a numeric `+00:00` offset (e.g. `"...846477544+00:00"`); the protobuf
/// JSON canonical mapping requires the `Z` UTC designator instead
/// (<https://protobuf.dev/programming-guides/proto3/#json>), and DM-SERIAL-001
/// enforces exactly that. Recurses through the whole tree, keyed by field
/// name rather than a blind string match, so an unrelated string value that
/// happens to end the same way (e.g. user-supplied message text) is never
/// touched.
fn normalize_timestamp_strings(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, v) in map.iter_mut() {
                if TIMESTAMP_FIELD_NAMES.contains(&key.as_str()) {
                    if let Value::String(s) = v {
                        if let Some(prefix) = s.strip_suffix("+00:00") {
                            *s = format!("{prefix}Z");
                        }
                    }
                }
                normalize_timestamp_strings(v);
            }
        }
        Value::Array(items) => {
            for item in items {
                normalize_timestamp_strings(item);
            }
        }
        _ => {}
    }
}

pub fn from_value<T: ProtoJsonPayload>(value: Value) -> Result<T, ProtoJsonPayloadError> {
    let protojson: T::ProtoJson =
        serde_json::from_value(value).map_err(ProtoJsonPayloadError::Json)?;
    let proto: T::Proto = transcode_message(&protojson)?;
    T::try_from_proto(&proto)
}

pub fn from_str<T: ProtoJsonPayload>(value: &str) -> Result<T, ProtoJsonPayloadError> {
    let protojson: T::ProtoJson =
        serde_json::from_str(value).map_err(ProtoJsonPayloadError::Json)?;
    let proto: T::Proto = transcode_message(&protojson)?;
    T::try_from_proto(&proto)
}

fn transcode_message<Src, Dst>(source: &Src) -> Result<Dst, ProtoJsonPayloadError>
where
    Src: Message,
    Dst: Message + Default,
{
    Dst::decode(source.encode_to_vec().as_slice()).map_err(ProtoJsonPayloadError::Decode)
}

macro_rules! impl_protojson_payload {
    ($native:path, $proto:path, $protojson:path, $to_proto:path, $from_proto:path) => {
        impl ProtoJsonPayload for $native {
            type Proto = $proto;
            type ProtoJson = $protojson;

            fn to_proto(value: &Self) -> Self::Proto {
                $to_proto(value)
            }

            fn try_from_proto(value: &Self::Proto) -> Result<Self, ProtoJsonPayloadError> {
                Ok($from_proto(value))
            }
        }
    };
}

macro_rules! impl_protojson_payload_optional {
    ($native:path, $proto:path, $protojson:path, $to_proto:path, $from_proto:path) => {
        impl ProtoJsonPayload for $native {
            type Proto = $proto;
            type ProtoJson = $protojson;

            fn to_proto(value: &Self) -> Self::Proto {
                $to_proto(value)
            }

            fn try_from_proto(value: &Self::Proto) -> Result<Self, ProtoJsonPayloadError> {
                $from_proto(value).ok_or(ProtoJsonPayloadError::MissingPayload(stringify!($native)))
            }
        }
    };
}

impl_protojson_payload!(
    SendMessageRequest,
    crate::proto::SendMessageRequest,
    crate::protojson::SendMessageRequest,
    crate::pbconv::to_proto_send_message_request,
    crate::pbconv::from_proto_send_message_request
);
impl_protojson_payload!(
    GetTaskRequest,
    crate::proto::GetTaskRequest,
    crate::protojson::GetTaskRequest,
    crate::pbconv::to_proto_get_task_request,
    crate::pbconv::from_proto_get_task_request
);
impl_protojson_payload!(
    ListTasksRequest,
    crate::proto::ListTasksRequest,
    crate::protojson::ListTasksRequest,
    crate::pbconv::to_proto_list_tasks_request,
    crate::pbconv::from_proto_list_tasks_request
);
impl_protojson_payload!(
    CancelTaskRequest,
    crate::proto::CancelTaskRequest,
    crate::protojson::CancelTaskRequest,
    crate::pbconv::to_proto_cancel_task_request,
    crate::pbconv::from_proto_cancel_task_request
);
impl_protojson_payload!(
    SubscribeToTaskRequest,
    crate::proto::SubscribeToTaskRequest,
    crate::protojson::SubscribeToTaskRequest,
    crate::pbconv::to_proto_subscribe_to_task_request,
    crate::pbconv::from_proto_subscribe_to_task_request
);
impl_protojson_payload!(
    GetExtendedAgentCardRequest,
    crate::proto::GetExtendedAgentCardRequest,
    crate::protojson::GetExtendedAgentCardRequest,
    crate::pbconv::to_proto_get_extended_agent_card_request,
    crate::pbconv::from_proto_get_extended_agent_card_request
);
impl_protojson_payload!(
    GetTaskPushNotificationConfigRequest,
    crate::proto::GetTaskPushNotificationConfigRequest,
    crate::protojson::GetTaskPushNotificationConfigRequest,
    crate::pbconv::to_proto_get_task_push_notification_config_request,
    crate::pbconv::from_proto_get_task_push_notification_config_request
);
impl_protojson_payload!(
    DeleteTaskPushNotificationConfigRequest,
    crate::proto::DeleteTaskPushNotificationConfigRequest,
    crate::protojson::DeleteTaskPushNotificationConfigRequest,
    crate::pbconv::to_proto_delete_task_push_notification_config_request,
    crate::pbconv::from_proto_delete_task_push_notification_config_request
);
impl_protojson_payload!(
    ListTaskPushNotificationConfigsRequest,
    crate::proto::ListTaskPushNotificationConfigsRequest,
    crate::protojson::ListTaskPushNotificationConfigsRequest,
    crate::pbconv::to_proto_list_task_push_notification_configs_request,
    crate::pbconv::from_proto_list_task_push_notification_configs_request
);
impl_protojson_payload!(
    TaskPushNotificationConfig,
    crate::proto::TaskPushNotificationConfig,
    crate::protojson::TaskPushNotificationConfig,
    crate::pbconv::to_proto_task_push_notification_config,
    crate::pbconv::from_proto_task_push_notification_config
);
impl_protojson_payload!(
    Task,
    crate::proto::Task,
    crate::protojson::Task,
    crate::pbconv::to_proto_task,
    crate::pbconv::from_proto_task
);
impl ProtoJsonPayload for ListTasksResponse {
    type Proto = crate::proto::ListTasksResponse;
    type ProtoJson = crate::protojson::ListTasksResponse;

    fn to_proto(value: &Self) -> Self::Proto {
        crate::pbconv::to_proto_list_tasks_response(value)
    }

    fn try_from_proto(value: &Self::Proto) -> Result<Self, ProtoJsonPayloadError> {
        Ok(crate::pbconv::from_proto_list_tasks_response(value))
    }

    /// REQ-TASK-LIST-002: `nextPageToken` must always be present, even on
    /// the last page, but the generated serializer follows proto3-JSON's
    /// own convention of omitting a string left at its default ("").
    fn normalize_json(value: &mut Value) {
        if let Value::Object(map) = value {
            map.entry("nextPageToken")
                .or_insert_with(|| Value::String(String::new()));
        }
    }
}
impl_protojson_payload!(
    ListTaskPushNotificationConfigsResponse,
    crate::proto::ListTaskPushNotificationConfigsResponse,
    crate::protojson::ListTaskPushNotificationConfigsResponse,
    crate::pbconv::to_proto_list_task_push_notification_configs_response,
    crate::pbconv::from_proto_list_task_push_notification_configs_response
);
impl_protojson_payload!(
    AgentCard,
    crate::proto::AgentCard,
    crate::protojson::AgentCard,
    crate::pbconv::to_proto_agent_card,
    crate::pbconv::from_proto_agent_card
);
impl_protojson_payload_optional!(
    SendMessageResponse,
    crate::proto::SendMessageResponse,
    crate::protojson::SendMessageResponse,
    crate::pbconv::to_proto_send_message_response,
    crate::pbconv::from_proto_send_message_response
);
impl_protojson_payload_optional!(
    StreamResponse,
    crate::proto::StreamResponse,
    crate::protojson::StreamResponse,
    crate::pbconv::to_proto_stream_response,
    crate::pbconv::from_proto_stream_response
);

#[cfg(test)]
mod tests {
    use super::*;

    /// REQ-TASK-LIST-002: the field must survive even when it's the
    /// proto3 default, which the generated serializer would otherwise omit.
    #[test]
    fn test_list_tasks_response_always_includes_next_page_token() {
        let response = ListTasksResponse {
            tasks: vec![],
            next_page_token: String::new(),
            page_size: 0,
            total_size: 0,
        };
        let json = to_value(&response).unwrap();
        assert_eq!(
            json.get("nextPageToken"),
            Some(&Value::String(String::new()))
        );
    }

    #[test]
    fn test_list_tasks_response_keeps_a_non_empty_next_page_token() {
        let response = ListTasksResponse {
            tasks: vec![],
            next_page_token: "page-2".to_string(),
            page_size: 0,
            total_size: 0,
        };
        let json = to_value(&response).unwrap();
        assert_eq!(
            json.get("nextPageToken"),
            Some(&Value::String("page-2".to_string()))
        );
    }

    /// `serde_json::to_value` on a struct always yields `Value::Object`, so
    /// `to_value` never reaches the non-object case in practice -- but
    /// `normalize_json` is a public trait method, not an internal that gets
    /// to lean on that invariant, so its behavior on the rest of `Value` is
    /// still part of its contract and worth pinning directly.
    #[test]
    fn test_list_tasks_response_normalize_json_ignores_a_non_object_value() {
        let mut not_an_object = Value::Null;
        ListTasksResponse::normalize_json(&mut not_an_object);
        assert_eq!(not_an_object, Value::Null);
    }

    fn sample_task_with_timestamp(timestamp: chrono::DateTime<chrono::Utc>) -> Task {
        Task {
            id: "task-1".to_string(),
            context_id: "ctx-1".to_string(),
            status: a2a::TaskStatus {
                state: a2a::TaskState::Working,
                message: None,
                timestamp: Some(timestamp),
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    /// DM-SERIAL-001: the protobuf JSON canonical mapping requires a
    /// `Z`-suffixed RFC 3339 timestamp, but `pbjson_types::Timestamp`'s own
    /// `Serialize` emits the numeric `+00:00` offset form instead.
    #[test]
    fn test_task_status_timestamp_uses_z_suffix_not_offset() {
        let timestamp = "2026-09-23T02:07:57.846477544Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        let json = to_value(&sample_task_with_timestamp(timestamp)).unwrap();
        let rendered = json["status"]["timestamp"].as_str().unwrap();
        assert_eq!(rendered, "2026-09-23T02:07:57.846477544Z");
        assert!(!rendered.ends_with("+00:00"));
    }

    #[test]
    fn test_normalize_timestamp_strings_recurses_into_nested_tasks() {
        let timestamp = "2026-09-23T02:07:57Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        let response = ListTasksResponse {
            tasks: vec![sample_task_with_timestamp(timestamp)],
            next_page_token: String::new(),
            page_size: 1,
            total_size: 1,
        };
        let json = to_value(&response).unwrap();
        let rendered = json["tasks"][0]["status"]["timestamp"].as_str().unwrap();
        assert_eq!(rendered, "2026-09-23T02:07:57Z");
    }

    /// A key-based fix rather than a blind string replace: an unrelated
    /// field whose value happens to end the same way as the buggy timestamp
    /// format (e.g. user-supplied text) must never be rewritten.
    #[test]
    fn test_normalize_timestamp_strings_ignores_unrelated_fields() {
        let mut json = serde_json::json!({
            "notATimestamp": "ends with +00:00",
            "nested": { "alsoNotATimestamp": "also +00:00" }
        });
        normalize_timestamp_strings(&mut json);
        assert_eq!(json["notATimestamp"], "ends with +00:00");
        assert_eq!(json["nested"]["alsoNotATimestamp"], "also +00:00");
    }
}
