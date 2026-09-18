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
    T::normalize_json(&mut json);
    Ok(json)
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
}
