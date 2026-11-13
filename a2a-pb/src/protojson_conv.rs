// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use std::fmt;

use a2a::{
    AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest, FieldViolation,
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
    /// A required field is not set; `field` is its JSON path in the payload.
    Invalid(FieldViolation),
}

impl fmt::Display for ProtoJsonPayloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Json(error) => write!(f, "ProtoJSON error: {error}"),
            Self::Decode(error) => write!(f, "protobuf transcode error: {error}"),
            Self::MissingPayload(type_name) => {
                write!(f, "ProtoJSON payload missing required data for {type_name}")
            }
            Self::Invalid(violation) => write!(
                f,
                "invalid ProtoJSON payload: {}",
                crate::pbconv::describe(violation)
            ),
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

/// Timestamps come out in wire form (`Z`, spec §5.6.1) from the generated
/// serializers themselves (see `build.rs`); the JSON is not touched afterwards,
/// so caller content such as `metadata` is written exactly as it was given.
pub fn to_value<T: ProtoJsonPayload>(value: &T) -> Result<Value, ProtoJsonPayloadError> {
    let proto = T::to_proto(value);
    let protojson: T::ProtoJson = transcode_message(&proto)?;
    let mut json = serde_json::to_value(protojson).map_err(ProtoJsonPayloadError::Json)?;
    T::normalize_json(&mut json);
    Ok(json)
}

/// Spec §5.7: a REQUIRED field MUST be present, so the defaults proto3-JSON
/// leaves out are written back.
fn insert_required(value: &mut Value, defaults: Value) {
    if let (Value::Object(map), Value::Object(defaults)) = (value, defaults) {
        for (key, default) in defaults {
            map.entry(key).or_insert(default);
        }
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

/// Implements [`ProtoJsonPayload`] for a native type. The converter from proto
/// is a function that cannot fail, or `|value| expr` for one that can; trait
/// items to override follow in braces.
macro_rules! impl_protojson_payload {
    ($native:path, $proto:path, $protojson:path, $to_proto:path, |$value:ident| $from_proto:expr $(, { $($item:tt)* })?) => {
        impl ProtoJsonPayload for $native {
            type Proto = $proto;
            type ProtoJson = $protojson;

            fn to_proto(value: &Self) -> Self::Proto {
                $to_proto(value)
            }

            fn try_from_proto($value: &Self::Proto) -> Result<Self, ProtoJsonPayloadError> {
                $from_proto
            }

            $($($item)*)?
        }
    };
    ($native:path, $proto:path, $protojson:path, $to_proto:path, $from_proto:path) => {
        impl_protojson_payload!($native, $proto, $protojson, $to_proto, |value| Ok($from_proto(
            value
        )));
    };
}

impl_protojson_payload!(
    SendMessageRequest,
    crate::proto::SendMessageRequest,
    crate::protojson::SendMessageRequest,
    crate::pbconv::to_proto_send_message_request,
    |value| crate::pbconv::from_proto_send_message_request(value)
        .map_err(ProtoJsonPayloadError::Invalid)
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
    |value| crate::pbconv::from_proto_task(value).map_err(ProtoJsonPayloadError::Invalid)
);
impl_protojson_payload!(
    ListTasksResponse,
    crate::proto::ListTasksResponse,
    crate::protojson::ListTasksResponse,
    crate::pbconv::to_proto_list_tasks_response,
    |value| crate::pbconv::from_proto_list_tasks_response(value)
        .map_err(ProtoJsonPayloadError::Invalid),
    {
        /// REQ-TASK-LIST-002: `nextPageToken` must always be present, even on
        /// the last page, but the generated serializer follows proto3-JSON's
        /// own convention of omitting a string left at its default (""). The
        /// other REQUIRED fields go the same way on an empty page.
        fn normalize_json(value: &mut Value) {
            insert_required(
                value,
                serde_json::json!({"tasks": [], "nextPageToken": "", "pageSize": 0, "totalSize": 0}),
            );
        }
    }
);
impl_protojson_payload!(
    ListTaskPushNotificationConfigsResponse,
    crate::proto::ListTaskPushNotificationConfigsResponse,
    crate::protojson::ListTaskPushNotificationConfigsResponse,
    crate::pbconv::to_proto_list_task_push_notification_configs_response,
    crate::pbconv::from_proto_list_task_push_notification_configs_response
);
impl ProtoJsonPayload for AgentCard {
    type Proto = crate::proto::AgentCard;
    type ProtoJson = crate::protojson::AgentCard;

    fn to_proto(value: &Self) -> Self::Proto {
        crate::pbconv::to_proto_agent_card(value)
    }

    fn try_from_proto(value: &Self::Proto) -> Result<Self, ProtoJsonPayloadError> {
        Ok(crate::pbconv::from_proto_agent_card(value))
    }

    /// An extended card without skills or input/output modes still carries
    /// its REQUIRED top-level fields.
    fn normalize_json(value: &mut Value) {
        insert_required(
            value,
            serde_json::json!({
                "name": "",
                "description": "",
                "supportedInterfaces": [],
                "version": "",
                "defaultInputModes": [],
                "defaultOutputModes": [],
                "skills": []
            }),
        );
    }
}
impl_protojson_payload!(
    SendMessageResponse,
    crate::proto::SendMessageResponse,
    crate::protojson::SendMessageResponse,
    crate::pbconv::to_proto_send_message_response,
    |value| crate::pbconv::from_proto_send_message_response(value)
        .map_err(ProtoJsonPayloadError::Invalid)?
        .ok_or(ProtoJsonPayloadError::MissingPayload("SendMessageResponse"))
);
impl_protojson_payload!(
    StreamResponse,
    crate::proto::StreamResponse,
    crate::protojson::StreamResponse,
    crate::pbconv::to_proto_stream_response,
    |value| crate::pbconv::from_proto_stream_response(value)
        .map_err(ProtoJsonPayloadError::Invalid)?
        .ok_or(ProtoJsonPayloadError::MissingPayload("StreamResponse"))
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

    /// §5.7: every REQUIRED field of an empty page is present.
    #[test]
    fn test_empty_list_tasks_response_keeps_every_required_field() {
        let response = ListTasksResponse {
            tasks: vec![],
            next_page_token: String::new(),
            page_size: 0,
            total_size: 0,
        };
        assert_eq!(
            to_value(&response).unwrap(),
            serde_json::json!({"tasks": [], "nextPageToken": "", "pageSize": 0, "totalSize": 0})
        );
    }

    /// §5.7: a card with no skills or modes keeps its REQUIRED fields.
    #[test]
    fn test_agent_card_keeps_empty_required_fields() {
        let card: AgentCard = serde_json::from_value(serde_json::json!({
            "name": "agent",
            "description": "",
            "supportedInterfaces": [],
            "version": "1.0.0",
            "capabilities": {},
            "defaultInputModes": [],
            "defaultOutputModes": [],
            "skills": []
        }))
        .unwrap();
        let json = to_value(&card).unwrap();
        for key in [
            "name",
            "description",
            "supportedInterfaces",
            "version",
            "defaultInputModes",
            "defaultOutputModes",
            "skills",
        ] {
            assert!(json.get(key).is_some(), "{key} missing from {json}");
        }
        let decoded: AgentCard = from_value(json).unwrap();
        assert_eq!(decoded, card);
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
    fn test_task_status_timestamp_keeps_only_the_digits_the_value_needs() {
        for (input, expected) in [
            ("2026-09-23T02:07:57Z", "2026-09-23T02:07:57Z"),
            ("2026-09-23T02:07:57.5Z", "2026-09-23T02:07:57.500Z"),
            ("2026-09-23T02:07:57.000123Z", "2026-09-23T02:07:57.000123Z"),
        ] {
            let timestamp = input.parse::<chrono::DateTime<chrono::Utc>>().unwrap();
            let json = to_value(&sample_task_with_timestamp(timestamp)).unwrap();
            assert_eq!(json["status"]["timestamp"], expected, "{input}");
        }
    }

    #[test]
    fn test_nested_task_timestamps_use_z_suffix() {
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

    /// `statusTimestampAfter` is the other `google.protobuf.Timestamp` field
    /// in the proto.
    #[test]
    fn test_status_timestamp_after_uses_z_suffix() {
        let request = ListTasksRequest {
            context_id: None,
            status: None,
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: Some("2026-09-23T02:07:57.5Z".parse().unwrap()),
            include_artifacts: None,
            tenant: None,
        };
        let json = to_value(&request).unwrap();
        assert_eq!(json["statusTimestampAfter"], "2026-09-23T02:07:57.500Z");
    }

    /// `metadata` and a data part hold the caller's own content. A key named
    /// `timestamp` there is theirs: whatever it says reaches the peer as it
    /// was written, including a `+00:00` offset.
    #[test]
    fn test_caller_content_under_a_timestamp_key_is_left_as_sent() {
        let offset = "2026-09-23T02:07:57+00:00";
        let timestamp = "2026-09-23T02:07:57Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        let mut task = sample_task_with_timestamp(timestamp);
        task.metadata = Some(std::collections::HashMap::from([(
            "timestamp".to_string(),
            Value::String(offset.to_string()),
        )]));
        task.artifacts = Some(vec![a2a::Artifact {
            artifact_id: "artifact-1".to_string(),
            name: None,
            description: None,
            parts: vec![a2a::Part::data(serde_json::json!({ "timestamp": offset }))],
            metadata: None,
            extensions: None,
        }]);
        let json = to_value(&task).unwrap();
        assert_eq!(json["status"]["timestamp"], "2026-09-23T02:07:57Z");
        assert_eq!(json["metadata"]["timestamp"], offset);
        assert_eq!(
            json["artifacts"][0]["parts"][0]["data"]["timestamp"],
            offset
        );
    }
}
