// Copyright AGNTCY Contributors (https://github.com/agntcy)
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
}

pub fn to_value<T: ProtoJsonPayload>(value: &T) -> Result<Value, ProtoJsonPayloadError> {
    let proto = T::to_proto(value);
    let protojson: T::ProtoJson = transcode_message(&proto)?;
    serde_json::to_value(protojson).map_err(ProtoJsonPayloadError::Json)
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
impl_protojson_payload!(
    ListTasksResponse,
    crate::proto::ListTasksResponse,
    crate::protojson::ListTasksResponse,
    crate::pbconv::to_proto_list_tasks_response,
    crate::pbconv::from_proto_list_tasks_response
);
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
