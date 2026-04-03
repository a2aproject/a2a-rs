// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
//! Conversion functions between native `a2a` types and proto-generated types.
//!
//! These functions handle the mapping between the hand-written serde types
//! in the `a2a` crate and the prost-generated types from `a2a.proto`.

use crate::proto;
use a2a::agent_card::*;
use a2a::event::*;
use a2a::types::*;
use prost_types;
use serde_json::Value;
use std::collections::HashMap;

// ---------------------------------------------------------------------------
// Struct / Value / Timestamp helpers
// ---------------------------------------------------------------------------

/// Convert a `serde_json::Value` map to a `prost_types::Struct`.
pub fn to_proto_struct(value: &HashMap<String, Value>) -> prost_types::Struct {
    prost_types::Struct {
        fields: value
            .iter()
            .map(|(k, v)| (k.clone(), json_value_to_proto_value(v)))
            .collect(),
    }
}

/// Convert a `prost_types::Struct` to a `serde_json::Value` map.
pub fn from_proto_struct(s: &prost_types::Struct) -> HashMap<String, Value> {
    s.fields
        .iter()
        .map(|(k, v)| (k.clone(), proto_value_to_json_value(v)))
        .collect()
}

/// Convert a `serde_json::Value` to `prost_types::Value`.
pub fn json_value_to_proto_value(v: &Value) -> prost_types::Value {
    use prost_types::value::Kind;
    prost_types::Value {
        kind: Some(match v {
            Value::Null => Kind::NullValue(0),
            Value::Bool(b) => Kind::BoolValue(*b),
            Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
            Value::String(s) => Kind::StringValue(s.clone()),
            Value::Array(arr) => Kind::ListValue(prost_types::ListValue {
                values: arr.iter().map(json_value_to_proto_value).collect(),
            }),
            Value::Object(map) => Kind::StructValue(prost_types::Struct {
                fields: map
                    .iter()
                    .map(|(k, v)| (k.clone(), json_value_to_proto_value(v)))
                    .collect(),
            }),
        }),
    }
}

/// Convert a `prost_types::Value` to `serde_json::Value`.
pub fn proto_value_to_json_value(v: &prost_types::Value) -> Value {
    use prost_types::value::Kind;
    match &v.kind {
        Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::BoolValue(b)) => Value::Bool(*b),
        Some(Kind::NumberValue(n)) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Some(Kind::StringValue(s)) => Value::String(s.clone()),
        Some(Kind::ListValue(list)) => {
            Value::Array(list.values.iter().map(proto_value_to_json_value).collect())
        }
        Some(Kind::StructValue(s)) => {
            let map: serde_json::Map<String, Value> = s
                .fields
                .iter()
                .map(|(k, v)| (k.clone(), proto_value_to_json_value(v)))
                .collect();
            Value::Object(map)
        }
        None => Value::Null,
    }
}

/// Convert `chrono::DateTime<Utc>` to `prost_types::Timestamp`.
pub fn to_proto_timestamp(dt: &chrono::DateTime<chrono::Utc>) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: dt.timestamp(),
        nanos: dt.timestamp_subsec_nanos() as i32,
    }
}

/// Convert `prost_types::Timestamp` to `chrono::DateTime<Utc>`.
pub fn from_proto_timestamp(ts: &prost_types::Timestamp) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::from_timestamp(ts.seconds, ts.nanos as u32)
}

fn opt_metadata_to_proto(meta: &Option<HashMap<String, Value>>) -> Option<prost_types::Struct> {
    meta.as_ref().map(to_proto_struct)
}

fn opt_metadata_from_proto(s: &Option<prost_types::Struct>) -> Option<HashMap<String, Value>> {
    s.as_ref().map(from_proto_struct)
}

fn empty_to_none(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

// ---------------------------------------------------------------------------
// Role
// ---------------------------------------------------------------------------

pub fn to_proto_role(r: &Role) -> i32 {
    match r {
        Role::Unspecified => proto::Role::Unspecified as i32,
        Role::User => proto::Role::User as i32,
        Role::Agent => proto::Role::Agent as i32,
    }
}

pub fn from_proto_role(v: i32) -> Role {
    match proto::Role::try_from(v) {
        Ok(proto::Role::User) => Role::User,
        Ok(proto::Role::Agent) => Role::Agent,
        _ => Role::Unspecified,
    }
}

// ---------------------------------------------------------------------------
// TaskState
// ---------------------------------------------------------------------------

pub fn to_proto_task_state(s: &TaskState) -> i32 {
    match s {
        TaskState::Unspecified => proto::TaskState::Unspecified as i32,
        TaskState::Submitted => proto::TaskState::Submitted as i32,
        TaskState::Working => proto::TaskState::Working as i32,
        TaskState::Completed => proto::TaskState::Completed as i32,
        TaskState::Failed => proto::TaskState::Failed as i32,
        TaskState::Canceled => proto::TaskState::Canceled as i32,
        TaskState::InputRequired => proto::TaskState::InputRequired as i32,
        TaskState::Rejected => proto::TaskState::Rejected as i32,
        TaskState::AuthRequired => proto::TaskState::AuthRequired as i32,
    }
}

pub fn from_proto_task_state(v: i32) -> TaskState {
    match proto::TaskState::try_from(v) {
        Ok(proto::TaskState::Submitted) => TaskState::Submitted,
        Ok(proto::TaskState::Working) => TaskState::Working,
        Ok(proto::TaskState::Completed) => TaskState::Completed,
        Ok(proto::TaskState::Failed) => TaskState::Failed,
        Ok(proto::TaskState::Canceled) => TaskState::Canceled,
        Ok(proto::TaskState::InputRequired) => TaskState::InputRequired,
        Ok(proto::TaskState::Rejected) => TaskState::Rejected,
        Ok(proto::TaskState::AuthRequired) => TaskState::AuthRequired,
        _ => TaskState::Unspecified,
    }
}

// ---------------------------------------------------------------------------
// Part
// ---------------------------------------------------------------------------

pub fn to_proto_part(p: &Part) -> proto::Part {
    let content = match &p.content {
        PartContent::Text(t) => Some(proto::part::Content::Text(t.clone())),
        PartContent::Raw(r) => Some(proto::part::Content::Raw(r.clone())),
        PartContent::Url(u) => Some(proto::part::Content::Url(u.clone())),
        PartContent::Data(d) => Some(proto::part::Content::Data(json_value_to_proto_value(d))),
    };
    proto::Part {
        content,
        filename: p.filename.clone().unwrap_or_default(),
        media_type: p.media_type.clone().unwrap_or_default(),
        metadata: opt_metadata_to_proto(&p.metadata),
    }
}

pub fn from_proto_part(p: &proto::Part) -> Part {
    let content = match &p.content {
        Some(proto::part::Content::Text(t)) => PartContent::Text(t.clone()),
        Some(proto::part::Content::Raw(r)) => PartContent::Raw(r.clone()),
        Some(proto::part::Content::Url(u)) => PartContent::Url(u.clone()),
        Some(proto::part::Content::Data(d)) => PartContent::Data(proto_value_to_json_value(d)),
        None => PartContent::Text(String::new()),
    };
    Part {
        content,
        filename: empty_to_none(&p.filename),
        media_type: empty_to_none(&p.media_type),
        metadata: opt_metadata_from_proto(&p.metadata),
    }
}

// ---------------------------------------------------------------------------
// Message
// ---------------------------------------------------------------------------

pub fn to_proto_message(m: &Message) -> proto::Message {
    proto::Message {
        message_id: m.message_id.clone(),
        context_id: m.context_id.clone().unwrap_or_default(),
        task_id: m.task_id.clone().unwrap_or_default(),
        role: to_proto_role(&m.role),
        parts: m.parts.iter().map(to_proto_part).collect(),
        metadata: opt_metadata_to_proto(&m.metadata),
        extensions: m.extensions.clone().unwrap_or_default(),
        reference_task_ids: m.reference_task_ids.clone().unwrap_or_default(),
    }
}

pub fn from_proto_message(m: &proto::Message) -> Message {
    Message {
        message_id: m.message_id.clone(),
        context_id: empty_to_none(&m.context_id),
        task_id: empty_to_none(&m.task_id),
        role: from_proto_role(m.role),
        parts: m.parts.iter().map(from_proto_part).collect(),
        metadata: opt_metadata_from_proto(&m.metadata),
        extensions: if m.extensions.is_empty() {
            None
        } else {
            Some(m.extensions.clone())
        },
        reference_task_ids: if m.reference_task_ids.is_empty() {
            None
        } else {
            Some(m.reference_task_ids.clone())
        },
    }
}

// ---------------------------------------------------------------------------
// TaskStatus
// ---------------------------------------------------------------------------

pub fn to_proto_task_status(s: &TaskStatus) -> proto::TaskStatus {
    proto::TaskStatus {
        state: to_proto_task_state(&s.state),
        message: s.message.as_ref().map(to_proto_message),
        timestamp: s.timestamp.as_ref().map(to_proto_timestamp),
    }
}

pub fn from_proto_task_status(s: &proto::TaskStatus) -> TaskStatus {
    TaskStatus {
        state: from_proto_task_state(s.state),
        message: s.message.as_ref().map(from_proto_message),
        timestamp: s.timestamp.as_ref().and_then(from_proto_timestamp),
    }
}

// ---------------------------------------------------------------------------
// Artifact
// ---------------------------------------------------------------------------

pub fn to_proto_artifact(a: &Artifact) -> proto::Artifact {
    proto::Artifact {
        artifact_id: a.artifact_id.clone(),
        name: a.name.clone().unwrap_or_default(),
        description: a.description.clone().unwrap_or_default(),
        parts: a.parts.iter().map(to_proto_part).collect(),
        metadata: opt_metadata_to_proto(&a.metadata),
        extensions: a.extensions.clone().unwrap_or_default(),
    }
}

pub fn from_proto_artifact(a: &proto::Artifact) -> Artifact {
    Artifact {
        artifact_id: a.artifact_id.clone(),
        name: empty_to_none(&a.name),
        description: empty_to_none(&a.description),
        parts: a.parts.iter().map(from_proto_part).collect(),
        metadata: opt_metadata_from_proto(&a.metadata),
        extensions: if a.extensions.is_empty() {
            None
        } else {
            Some(a.extensions.clone())
        },
    }
}

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

pub fn to_proto_task(t: &Task) -> proto::Task {
    proto::Task {
        id: t.id.clone(),
        context_id: t.context_id.clone(),
        status: Some(to_proto_task_status(&t.status)),
        artifacts: t
            .artifacts
            .as_ref()
            .map(|a| a.iter().map(to_proto_artifact).collect())
            .unwrap_or_default(),
        history: t
            .history
            .as_ref()
            .map(|h| h.iter().map(to_proto_message).collect())
            .unwrap_or_default(),
        metadata: opt_metadata_to_proto(&t.metadata),
    }
}

pub fn from_proto_task(t: &proto::Task) -> Task {
    Task {
        id: t.id.clone(),
        context_id: t.context_id.clone(),
        status: t
            .status
            .as_ref()
            .map(from_proto_task_status)
            .unwrap_or(TaskStatus {
                state: TaskState::Unspecified,
                message: None,
                timestamp: None,
            }),
        artifacts: if t.artifacts.is_empty() {
            None
        } else {
            Some(t.artifacts.iter().map(from_proto_artifact).collect())
        },
        history: if t.history.is_empty() {
            None
        } else {
            Some(t.history.iter().map(from_proto_message).collect())
        },
        metadata: opt_metadata_from_proto(&t.metadata),
    }
}

// ---------------------------------------------------------------------------
// AuthenticationInfo
// ---------------------------------------------------------------------------

pub fn to_proto_authentication_info(a: &AuthenticationInfo) -> proto::AuthenticationInfo {
    proto::AuthenticationInfo {
        scheme: a.scheme.clone(),
        credentials: a.credentials.clone().unwrap_or_default(),
    }
}

pub fn from_proto_authentication_info(a: &proto::AuthenticationInfo) -> AuthenticationInfo {
    AuthenticationInfo {
        scheme: a.scheme.clone(),
        credentials: empty_to_none(&a.credentials),
    }
}

// ---------------------------------------------------------------------------
// PushNotificationConfig / TaskPushNotificationConfig
// ---------------------------------------------------------------------------

pub fn to_proto_task_push_notification_config(
    c: &TaskPushNotificationConfig,
) -> proto::TaskPushNotificationConfig {
    proto::TaskPushNotificationConfig {
        tenant: c.tenant.clone().unwrap_or_default(),
        id: c.config.id.clone().unwrap_or_default(),
        task_id: c.task_id.clone(),
        url: c.config.url.clone(),
        token: c.config.token.clone().unwrap_or_default(),
        authentication: c
            .config
            .authentication
            .as_ref()
            .map(to_proto_authentication_info),
    }
}

pub fn from_proto_task_push_notification_config(
    c: &proto::TaskPushNotificationConfig,
) -> TaskPushNotificationConfig {
    TaskPushNotificationConfig {
        task_id: c.task_id.clone(),
        tenant: empty_to_none(&c.tenant),
        config: PushNotificationConfig {
            url: c.url.clone(),
            id: empty_to_none(&c.id),
            token: empty_to_none(&c.token),
            authentication: c
                .authentication
                .as_ref()
                .map(from_proto_authentication_info),
        },
    }
}

// ---------------------------------------------------------------------------
// SendMessageConfiguration
// ---------------------------------------------------------------------------

pub fn to_proto_send_message_configuration(
    c: &SendMessageConfiguration,
) -> proto::SendMessageConfiguration {
    proto::SendMessageConfiguration {
        accepted_output_modes: c.accepted_output_modes.clone().unwrap_or_default(),
        task_push_notification_config: c.push_notification_config.as_ref().map(|pnc| {
            proto::TaskPushNotificationConfig {
                tenant: String::new(),
                id: pnc.id.clone().unwrap_or_default(),
                task_id: String::new(),
                url: pnc.url.clone(),
                token: pnc.token.clone().unwrap_or_default(),
                authentication: pnc
                    .authentication
                    .as_ref()
                    .map(to_proto_authentication_info),
            }
        }),
        history_length: c.history_length,
        return_immediately: c.return_immediately.unwrap_or(false),
    }
}

pub fn from_proto_send_message_configuration(
    c: &proto::SendMessageConfiguration,
) -> SendMessageConfiguration {
    SendMessageConfiguration {
        accepted_output_modes: if c.accepted_output_modes.is_empty() {
            None
        } else {
            Some(c.accepted_output_modes.clone())
        },
        push_notification_config: c.task_push_notification_config.as_ref().map(|tpnc| {
            PushNotificationConfig {
                url: tpnc.url.clone(),
                id: empty_to_none(&tpnc.id),
                token: empty_to_none(&tpnc.token),
                authentication: tpnc
                    .authentication
                    .as_ref()
                    .map(from_proto_authentication_info),
            }
        }),
        history_length: c.history_length,
        return_immediately: if c.return_immediately {
            Some(true)
        } else {
            None
        },
    }
}

// ---------------------------------------------------------------------------
// Request types
// ---------------------------------------------------------------------------

pub fn to_proto_send_message_request(r: &SendMessageRequest) -> proto::SendMessageRequest {
    proto::SendMessageRequest {
        tenant: r.tenant.clone().unwrap_or_default(),
        message: Some(to_proto_message(&r.message)),
        configuration: r
            .configuration
            .as_ref()
            .map(to_proto_send_message_configuration),
        metadata: opt_metadata_to_proto(&r.metadata),
    }
}

pub fn from_proto_send_message_request(r: &proto::SendMessageRequest) -> SendMessageRequest {
    SendMessageRequest {
        tenant: empty_to_none(&r.tenant),
        message: r
            .message
            .as_ref()
            .map(from_proto_message)
            .unwrap_or_else(|| Message::new(Role::User, vec![])),
        configuration: r
            .configuration
            .as_ref()
            .map(from_proto_send_message_configuration),
        metadata: opt_metadata_from_proto(&r.metadata),
    }
}

pub fn to_proto_get_task_request(r: &GetTaskRequest) -> proto::GetTaskRequest {
    proto::GetTaskRequest {
        tenant: r.tenant.clone().unwrap_or_default(),
        id: r.id.clone(),
        history_length: r.history_length,
    }
}

pub fn from_proto_get_task_request(r: &proto::GetTaskRequest) -> GetTaskRequest {
    GetTaskRequest {
        tenant: empty_to_none(&r.tenant),
        id: r.id.clone(),
        history_length: r.history_length,
    }
}

pub fn to_proto_list_tasks_request(r: &ListTasksRequest) -> proto::ListTasksRequest {
    proto::ListTasksRequest {
        tenant: r.tenant.clone().unwrap_or_default(),
        context_id: r.context_id.clone().unwrap_or_default(),
        status: r.status.as_ref().map(to_proto_task_state).unwrap_or(0),
        page_size: r.page_size,
        page_token: r.page_token.clone().unwrap_or_default(),
        history_length: r.history_length,
        status_timestamp_after: r.status_timestamp_after.as_ref().map(to_proto_timestamp),
        include_artifacts: r.include_artifacts,
    }
}

pub fn from_proto_list_tasks_request(r: &proto::ListTasksRequest) -> ListTasksRequest {
    ListTasksRequest {
        tenant: empty_to_none(&r.tenant),
        context_id: empty_to_none(&r.context_id),
        status: if r.status == 0 {
            None
        } else {
            Some(from_proto_task_state(r.status))
        },
        page_size: r.page_size,
        page_token: empty_to_none(&r.page_token),
        history_length: r.history_length,
        status_timestamp_after: r
            .status_timestamp_after
            .as_ref()
            .and_then(from_proto_timestamp),
        include_artifacts: r.include_artifacts,
    }
}

pub fn to_proto_list_tasks_response(r: &ListTasksResponse) -> proto::ListTasksResponse {
    proto::ListTasksResponse {
        tasks: r.tasks.iter().map(to_proto_task).collect(),
        next_page_token: r.next_page_token.clone(),
        page_size: r.page_size,
        total_size: r.total_size,
    }
}

pub fn from_proto_list_tasks_response(r: &proto::ListTasksResponse) -> ListTasksResponse {
    ListTasksResponse {
        tasks: r.tasks.iter().map(from_proto_task).collect(),
        next_page_token: r.next_page_token.clone(),
        page_size: r.page_size,
        total_size: r.total_size,
    }
}

pub fn to_proto_cancel_task_request(r: &CancelTaskRequest) -> proto::CancelTaskRequest {
    proto::CancelTaskRequest {
        tenant: r.tenant.clone().unwrap_or_default(),
        id: r.id.clone(),
        metadata: opt_metadata_to_proto(&r.metadata),
    }
}

pub fn from_proto_cancel_task_request(r: &proto::CancelTaskRequest) -> CancelTaskRequest {
    CancelTaskRequest {
        tenant: empty_to_none(&r.tenant),
        id: r.id.clone(),
        metadata: opt_metadata_from_proto(&r.metadata),
    }
}

pub fn to_proto_subscribe_to_task_request(
    r: &SubscribeToTaskRequest,
) -> proto::SubscribeToTaskRequest {
    proto::SubscribeToTaskRequest {
        tenant: r.tenant.clone().unwrap_or_default(),
        id: r.id.clone(),
    }
}

pub fn from_proto_subscribe_to_task_request(
    r: &proto::SubscribeToTaskRequest,
) -> SubscribeToTaskRequest {
    SubscribeToTaskRequest {
        tenant: empty_to_none(&r.tenant),
        id: r.id.clone(),
    }
}

pub fn to_proto_get_extended_agent_card_request(
    r: &GetExtendedAgentCardRequest,
) -> proto::GetExtendedAgentCardRequest {
    proto::GetExtendedAgentCardRequest {
        tenant: r.tenant.clone().unwrap_or_default(),
    }
}

pub fn from_proto_get_extended_agent_card_request(
    r: &proto::GetExtendedAgentCardRequest,
) -> GetExtendedAgentCardRequest {
    GetExtendedAgentCardRequest {
        tenant: empty_to_none(&r.tenant),
    }
}

// ---------------------------------------------------------------------------
// Push notification request types
// ---------------------------------------------------------------------------

pub fn to_proto_get_task_push_notification_config_request(
    r: &GetTaskPushNotificationConfigRequest,
) -> proto::GetTaskPushNotificationConfigRequest {
    proto::GetTaskPushNotificationConfigRequest {
        tenant: r.tenant.clone().unwrap_or_default(),
        task_id: r.task_id.clone(),
        id: r.id.clone(),
    }
}

pub fn from_proto_get_task_push_notification_config_request(
    r: &proto::GetTaskPushNotificationConfigRequest,
) -> GetTaskPushNotificationConfigRequest {
    GetTaskPushNotificationConfigRequest {
        tenant: empty_to_none(&r.tenant),
        task_id: r.task_id.clone(),
        id: r.id.clone(),
    }
}

pub fn to_proto_delete_task_push_notification_config_request(
    r: &DeleteTaskPushNotificationConfigRequest,
) -> proto::DeleteTaskPushNotificationConfigRequest {
    proto::DeleteTaskPushNotificationConfigRequest {
        tenant: r.tenant.clone().unwrap_or_default(),
        task_id: r.task_id.clone(),
        id: r.id.clone(),
    }
}

pub fn from_proto_delete_task_push_notification_config_request(
    r: &proto::DeleteTaskPushNotificationConfigRequest,
) -> DeleteTaskPushNotificationConfigRequest {
    DeleteTaskPushNotificationConfigRequest {
        tenant: empty_to_none(&r.tenant),
        task_id: r.task_id.clone(),
        id: r.id.clone(),
    }
}

pub fn to_proto_list_task_push_notification_configs_request(
    r: &ListTaskPushNotificationConfigsRequest,
) -> proto::ListTaskPushNotificationConfigsRequest {
    proto::ListTaskPushNotificationConfigsRequest {
        tenant: r.tenant.clone().unwrap_or_default(),
        task_id: r.task_id.clone(),
        page_size: r.page_size.unwrap_or(0),
        page_token: r.page_token.clone().unwrap_or_default(),
    }
}

pub fn from_proto_list_task_push_notification_configs_request(
    r: &proto::ListTaskPushNotificationConfigsRequest,
) -> ListTaskPushNotificationConfigsRequest {
    ListTaskPushNotificationConfigsRequest {
        tenant: empty_to_none(&r.tenant),
        task_id: r.task_id.clone(),
        page_size: if r.page_size == 0 {
            None
        } else {
            Some(r.page_size)
        },
        page_token: empty_to_none(&r.page_token),
    }
}

pub fn to_proto_list_task_push_notification_configs_response(
    r: &ListTaskPushNotificationConfigsResponse,
) -> proto::ListTaskPushNotificationConfigsResponse {
    proto::ListTaskPushNotificationConfigsResponse {
        configs: r
            .configs
            .iter()
            .map(to_proto_task_push_notification_config)
            .collect(),
        next_page_token: r.next_page_token.clone().unwrap_or_default(),
    }
}

pub fn from_proto_list_task_push_notification_configs_response(
    r: &proto::ListTaskPushNotificationConfigsResponse,
) -> ListTaskPushNotificationConfigsResponse {
    ListTaskPushNotificationConfigsResponse {
        configs: r
            .configs
            .iter()
            .map(from_proto_task_push_notification_config)
            .collect(),
        next_page_token: empty_to_none(&r.next_page_token),
    }
}

// ---------------------------------------------------------------------------
// SendMessageResponse
// ---------------------------------------------------------------------------

pub fn to_proto_send_message_response(r: &SendMessageResponse) -> proto::SendMessageResponse {
    let payload = match r {
        SendMessageResponse::Task(t) => Some(proto::send_message_response::Payload::Task(
            to_proto_task(t),
        )),
        SendMessageResponse::Message(m) => Some(proto::send_message_response::Payload::Message(
            to_proto_message(m),
        )),
    };
    proto::SendMessageResponse { payload }
}

pub fn from_proto_send_message_response(
    r: &proto::SendMessageResponse,
) -> Option<SendMessageResponse> {
    match &r.payload {
        Some(proto::send_message_response::Payload::Task(t)) => {
            Some(SendMessageResponse::Task(from_proto_task(t)))
        }
        Some(proto::send_message_response::Payload::Message(m)) => {
            Some(SendMessageResponse::Message(from_proto_message(m)))
        }
        None => None,
    }
}

// ---------------------------------------------------------------------------
// StreamResponse events
// ---------------------------------------------------------------------------

pub fn to_proto_task_status_update_event(
    e: &TaskStatusUpdateEvent,
) -> proto::TaskStatusUpdateEvent {
    proto::TaskStatusUpdateEvent {
        task_id: e.task_id.clone(),
        context_id: e.context_id.clone(),
        status: Some(to_proto_task_status(&e.status)),
        metadata: opt_metadata_to_proto(&e.metadata),
    }
}

pub fn from_proto_task_status_update_event(
    e: &proto::TaskStatusUpdateEvent,
) -> TaskStatusUpdateEvent {
    TaskStatusUpdateEvent {
        task_id: e.task_id.clone(),
        context_id: e.context_id.clone(),
        status: e
            .status
            .as_ref()
            .map(from_proto_task_status)
            .unwrap_or(TaskStatus {
                state: TaskState::Unspecified,
                message: None,
                timestamp: None,
            }),
        metadata: opt_metadata_from_proto(&e.metadata),
    }
}

pub fn to_proto_task_artifact_update_event(
    e: &TaskArtifactUpdateEvent,
) -> proto::TaskArtifactUpdateEvent {
    proto::TaskArtifactUpdateEvent {
        task_id: e.task_id.clone(),
        context_id: e.context_id.clone(),
        artifact: Some(to_proto_artifact(&e.artifact)),
        append: e.append.unwrap_or(false),
        last_chunk: e.last_chunk.unwrap_or(false),
        metadata: opt_metadata_to_proto(&e.metadata),
    }
}

pub fn from_proto_task_artifact_update_event(
    e: &proto::TaskArtifactUpdateEvent,
) -> TaskArtifactUpdateEvent {
    TaskArtifactUpdateEvent {
        task_id: e.task_id.clone(),
        context_id: e.context_id.clone(),
        artifact: e
            .artifact
            .as_ref()
            .map(from_proto_artifact)
            .unwrap_or(Artifact {
                artifact_id: String::new(),
                name: None,
                description: None,
                parts: vec![],
                metadata: None,
                extensions: None,
            }),
        append: if e.append { Some(true) } else { None },
        last_chunk: if e.last_chunk { Some(true) } else { None },
        metadata: opt_metadata_from_proto(&e.metadata),
    }
}

pub fn to_proto_stream_response(r: &StreamResponse) -> proto::StreamResponse {
    let payload = match r {
        StreamResponse::Task(t) => Some(proto::stream_response::Payload::Task(to_proto_task(t))),
        StreamResponse::Message(m) => Some(proto::stream_response::Payload::Message(
            to_proto_message(m),
        )),
        StreamResponse::StatusUpdate(s) => Some(proto::stream_response::Payload::StatusUpdate(
            to_proto_task_status_update_event(s),
        )),
        StreamResponse::ArtifactUpdate(a) => Some(proto::stream_response::Payload::ArtifactUpdate(
            to_proto_task_artifact_update_event(a),
        )),
    };
    proto::StreamResponse { payload }
}

pub fn from_proto_stream_response(r: &proto::StreamResponse) -> Option<StreamResponse> {
    match &r.payload {
        Some(proto::stream_response::Payload::Task(t)) => {
            Some(StreamResponse::Task(from_proto_task(t)))
        }
        Some(proto::stream_response::Payload::Message(m)) => {
            Some(StreamResponse::Message(from_proto_message(m)))
        }
        Some(proto::stream_response::Payload::StatusUpdate(s)) => Some(
            StreamResponse::StatusUpdate(from_proto_task_status_update_event(s)),
        ),
        Some(proto::stream_response::Payload::ArtifactUpdate(a)) => Some(
            StreamResponse::ArtifactUpdate(from_proto_task_artifact_update_event(a)),
        ),
        None => None,
    }
}

// ---------------------------------------------------------------------------
// AgentCard and related types
// ---------------------------------------------------------------------------

pub fn to_proto_agent_interface(i: &AgentInterface) -> proto::AgentInterface {
    proto::AgentInterface {
        url: i.url.clone(),
        protocol_binding: i.protocol_binding.clone(),
        tenant: i.tenant.clone().unwrap_or_default(),
        protocol_version: i.protocol_version.clone(),
    }
}

pub fn from_proto_agent_interface(i: &proto::AgentInterface) -> AgentInterface {
    AgentInterface {
        url: i.url.clone(),
        protocol_binding: i.protocol_binding.clone(),
        protocol_version: i.protocol_version.clone(),
        tenant: empty_to_none(&i.tenant),
    }
}

pub fn to_proto_agent_provider(p: &AgentProvider) -> proto::AgentProvider {
    proto::AgentProvider {
        organization: p.organization.clone(),
        url: p.url.clone(),
    }
}

pub fn from_proto_agent_provider(p: &proto::AgentProvider) -> AgentProvider {
    AgentProvider {
        organization: p.organization.clone(),
        url: p.url.clone(),
    }
}

pub fn to_proto_agent_extension(e: &AgentExtension) -> proto::AgentExtension {
    proto::AgentExtension {
        uri: e.uri.clone(),
        description: e.description.clone().unwrap_or_default(),
        required: e.required.unwrap_or(false),
        params: e.params.as_ref().map(to_proto_struct),
    }
}

pub fn from_proto_agent_extension(e: &proto::AgentExtension) -> AgentExtension {
    AgentExtension {
        uri: e.uri.clone(),
        description: empty_to_none(&e.description),
        required: if e.required { Some(true) } else { None },
        params: opt_metadata_from_proto(&e.params),
    }
}

pub fn to_proto_agent_capabilities(c: &AgentCapabilities) -> proto::AgentCapabilities {
    proto::AgentCapabilities {
        streaming: c.streaming,
        push_notifications: c.push_notifications,
        extensions: c
            .extensions
            .as_ref()
            .map(|exts| exts.iter().map(to_proto_agent_extension).collect())
            .unwrap_or_default(),
        extended_agent_card: c.extended_agent_card,
    }
}

pub fn from_proto_agent_capabilities(c: &proto::AgentCapabilities) -> AgentCapabilities {
    AgentCapabilities {
        streaming: c.streaming,
        push_notifications: c.push_notifications,
        extensions: if c.extensions.is_empty() {
            None
        } else {
            Some(
                c.extensions
                    .iter()
                    .map(from_proto_agent_extension)
                    .collect(),
            )
        },
        extended_agent_card: c.extended_agent_card,
    }
}

pub fn to_proto_agent_skill(s: &AgentSkill) -> proto::AgentSkill {
    proto::AgentSkill {
        id: s.id.clone(),
        name: s.name.clone(),
        description: s.description.clone(),
        tags: s.tags.clone(),
        examples: s.examples.clone().unwrap_or_default(),
        input_modes: s.input_modes.clone().unwrap_or_default(),
        output_modes: s.output_modes.clone().unwrap_or_default(),
        security_requirements: s
            .security_requirements
            .as_ref()
            .map(|reqs| reqs.iter().map(to_proto_security_requirement).collect())
            .unwrap_or_default(),
    }
}

pub fn from_proto_agent_skill(s: &proto::AgentSkill) -> AgentSkill {
    AgentSkill {
        id: s.id.clone(),
        name: s.name.clone(),
        description: s.description.clone(),
        tags: s.tags.clone(),
        examples: if s.examples.is_empty() {
            None
        } else {
            Some(s.examples.clone())
        },
        input_modes: if s.input_modes.is_empty() {
            None
        } else {
            Some(s.input_modes.clone())
        },
        output_modes: if s.output_modes.is_empty() {
            None
        } else {
            Some(s.output_modes.clone())
        },
        security_requirements: if s.security_requirements.is_empty() {
            None
        } else {
            Some(
                s.security_requirements
                    .iter()
                    .map(from_proto_security_requirement)
                    .collect(),
            )
        },
    }
}

pub fn to_proto_agent_card_signature(s: &AgentCardSignature) -> proto::AgentCardSignature {
    proto::AgentCardSignature {
        protected: s.protected.clone(),
        signature: s.signature.clone(),
        header: s.header.as_ref().map(to_proto_struct),
    }
}

pub fn from_proto_agent_card_signature(s: &proto::AgentCardSignature) -> AgentCardSignature {
    AgentCardSignature {
        protected: s.protected.clone(),
        signature: s.signature.clone(),
        header: opt_metadata_from_proto(&s.header),
    }
}

// ---------------------------------------------------------------------------
// SecurityScheme
// ---------------------------------------------------------------------------

pub fn to_proto_security_scheme(s: &SecurityScheme) -> proto::SecurityScheme {
    let scheme = match s {
        SecurityScheme::ApiKey(ak) => Some(proto::security_scheme::Scheme::ApiKeySecurityScheme(
            proto::ApiKeySecurityScheme {
                description: ak.description.clone().unwrap_or_default(),
                location: ak.location.clone(),
                name: ak.name.clone(),
            },
        )),
        SecurityScheme::HttpAuth(ha) => Some(
            proto::security_scheme::Scheme::HttpAuthSecurityScheme(proto::HttpAuthSecurityScheme {
                description: ha.description.clone().unwrap_or_default(),
                scheme: ha.scheme.clone(),
                bearer_format: ha.bearer_format.clone().unwrap_or_default(),
            }),
        ),
        SecurityScheme::OAuth2(oa) => Some(proto::security_scheme::Scheme::Oauth2SecurityScheme(
            proto::OAuth2SecurityScheme {
                description: oa.description.clone().unwrap_or_default(),
                flows: Some(to_proto_oauth_flows(&oa.flows)),
                oauth2_metadata_url: oa.oauth2_metadata_url.clone().unwrap_or_default(),
            },
        )),
        SecurityScheme::OpenIdConnect(oi) => {
            Some(proto::security_scheme::Scheme::OpenIdConnectSecurityScheme(
                proto::OpenIdConnectSecurityScheme {
                    description: oi.description.clone().unwrap_or_default(),
                    open_id_connect_url: oi.open_id_connect_url.clone(),
                },
            ))
        }
        SecurityScheme::MutualTls(mt) => Some(proto::security_scheme::Scheme::MtlsSecurityScheme(
            proto::MutualTlsSecurityScheme {
                description: mt.description.clone().unwrap_or_default(),
            },
        )),
    };
    proto::SecurityScheme { scheme }
}

pub fn from_proto_security_scheme(s: &proto::SecurityScheme) -> Option<SecurityScheme> {
    match &s.scheme {
        Some(proto::security_scheme::Scheme::ApiKeySecurityScheme(ak)) => {
            Some(SecurityScheme::ApiKey(ApiKeySecurityScheme {
                location: ak.location.clone(),
                name: ak.name.clone(),
                description: empty_to_none(&ak.description),
            }))
        }
        Some(proto::security_scheme::Scheme::HttpAuthSecurityScheme(ha)) => {
            Some(SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
                scheme: ha.scheme.clone(),
                description: empty_to_none(&ha.description),
                bearer_format: empty_to_none(&ha.bearer_format),
            }))
        }
        Some(proto::security_scheme::Scheme::Oauth2SecurityScheme(oa)) => {
            Some(SecurityScheme::OAuth2(OAuth2SecurityScheme {
                description: empty_to_none(&oa.description),
                flows: oa
                    .flows
                    .as_ref()
                    .and_then(from_proto_oauth_flows)
                    .unwrap_or(OAuthFlows::ClientCredentials(ClientCredentialsOAuthFlow {
                        token_url: String::new(),
                        scopes: HashMap::new(),
                        refresh_url: None,
                    })),
                oauth2_metadata_url: empty_to_none(&oa.oauth2_metadata_url),
            }))
        }
        Some(proto::security_scheme::Scheme::OpenIdConnectSecurityScheme(oi)) => {
            Some(SecurityScheme::OpenIdConnect(OpenIdConnectSecurityScheme {
                open_id_connect_url: oi.open_id_connect_url.clone(),
                description: empty_to_none(&oi.description),
            }))
        }
        Some(proto::security_scheme::Scheme::MtlsSecurityScheme(mt)) => {
            Some(SecurityScheme::MutualTls(MutualTlsSecurityScheme {
                description: empty_to_none(&mt.description),
            }))
        }
        None => None,
    }
}

// ---------------------------------------------------------------------------
// SecurityRequirement
// ---------------------------------------------------------------------------

pub fn to_proto_security_requirement(r: &SecurityRequirement) -> proto::SecurityRequirement {
    proto::SecurityRequirement {
        schemes: r
            .iter()
            .map(|(k, v)| (k.clone(), proto::StringList { list: v.clone() }))
            .collect(),
    }
}

pub fn from_proto_security_requirement(r: &proto::SecurityRequirement) -> SecurityRequirement {
    r.schemes
        .iter()
        .map(|(k, v)| (k.clone(), v.list.clone()))
        .collect()
}

// ---------------------------------------------------------------------------
// OAuthFlows
// ---------------------------------------------------------------------------

pub fn to_proto_oauth_flows(f: &OAuthFlows) -> proto::OAuthFlows {
    let flow = match f {
        OAuthFlows::AuthorizationCode(ac) => Some(proto::o_auth_flows::Flow::AuthorizationCode(
            proto::AuthorizationCodeOAuthFlow {
                authorization_url: ac.authorization_url.clone(),
                token_url: ac.token_url.clone(),
                refresh_url: ac.refresh_url.clone().unwrap_or_default(),
                scopes: ac.scopes.clone(),
                pkce_required: ac.pkce_required.unwrap_or(false),
            },
        )),
        OAuthFlows::ClientCredentials(cc) => Some(proto::o_auth_flows::Flow::ClientCredentials(
            proto::ClientCredentialsOAuthFlow {
                token_url: cc.token_url.clone(),
                refresh_url: cc.refresh_url.clone().unwrap_or_default(),
                scopes: cc.scopes.clone(),
            },
        )),
        OAuthFlows::DeviceCode(dc) => Some(proto::o_auth_flows::Flow::DeviceCode(
            proto::DeviceCodeOAuthFlow {
                device_authorization_url: dc.device_authorization_url.clone(),
                token_url: dc.token_url.clone(),
                refresh_url: dc.refresh_url.clone().unwrap_or_default(),
                scopes: dc.scopes.clone(),
            },
        )),
        OAuthFlows::Implicit(im) => Some(proto::o_auth_flows::Flow::Implicit(
            proto::ImplicitOAuthFlow {
                authorization_url: im.authorization_url.clone(),
                refresh_url: im.refresh_url.clone().unwrap_or_default(),
                scopes: im.scopes.clone(),
            },
        )),
        OAuthFlows::Password(pw) => Some(proto::o_auth_flows::Flow::Password(
            proto::PasswordOAuthFlow {
                token_url: pw.token_url.clone(),
                refresh_url: pw.refresh_url.clone().unwrap_or_default(),
                scopes: pw.scopes.clone(),
            },
        )),
    };
    proto::OAuthFlows { flow }
}

pub fn from_proto_oauth_flows(f: &proto::OAuthFlows) -> Option<OAuthFlows> {
    match &f.flow {
        Some(proto::o_auth_flows::Flow::AuthorizationCode(ac)) => {
            Some(OAuthFlows::AuthorizationCode(AuthorizationCodeOAuthFlow {
                authorization_url: ac.authorization_url.clone(),
                token_url: ac.token_url.clone(),
                scopes: ac.scopes.clone(),
                refresh_url: empty_to_none(&ac.refresh_url),
                pkce_required: if ac.pkce_required { Some(true) } else { None },
            }))
        }
        Some(proto::o_auth_flows::Flow::ClientCredentials(cc)) => {
            Some(OAuthFlows::ClientCredentials(ClientCredentialsOAuthFlow {
                token_url: cc.token_url.clone(),
                scopes: cc.scopes.clone(),
                refresh_url: empty_to_none(&cc.refresh_url),
            }))
        }
        Some(proto::o_auth_flows::Flow::DeviceCode(dc)) => {
            Some(OAuthFlows::DeviceCode(DeviceCodeOAuthFlow {
                device_authorization_url: dc.device_authorization_url.clone(),
                token_url: dc.token_url.clone(),
                scopes: dc.scopes.clone(),
                refresh_url: empty_to_none(&dc.refresh_url),
            }))
        }
        Some(proto::o_auth_flows::Flow::Implicit(im)) => {
            Some(OAuthFlows::Implicit(ImplicitOAuthFlow {
                authorization_url: im.authorization_url.clone(),
                scopes: im.scopes.clone(),
                refresh_url: empty_to_none(&im.refresh_url),
            }))
        }
        Some(proto::o_auth_flows::Flow::Password(pw)) => {
            Some(OAuthFlows::Password(PasswordOAuthFlow {
                token_url: pw.token_url.clone(),
                scopes: pw.scopes.clone(),
                refresh_url: empty_to_none(&pw.refresh_url),
            }))
        }
        None => None,
    }
}

// ---------------------------------------------------------------------------
// AgentCard
// ---------------------------------------------------------------------------

pub fn to_proto_agent_card(c: &AgentCard) -> proto::AgentCard {
    proto::AgentCard {
        name: c.name.clone(),
        description: c.description.clone(),
        version: c.version.clone(),
        supported_interfaces: c
            .supported_interfaces
            .iter()
            .map(to_proto_agent_interface)
            .collect(),
        provider: c.provider.as_ref().map(to_proto_agent_provider),
        documentation_url: c.documentation_url.clone(),
        capabilities: Some(to_proto_agent_capabilities(&c.capabilities)),
        security_schemes: c
            .security_schemes
            .as_ref()
            .map(|ss| {
                ss.iter()
                    .map(|(k, v)| (k.clone(), to_proto_security_scheme(v)))
                    .collect()
            })
            .unwrap_or_default(),
        security_requirements: c
            .security_requirements
            .as_ref()
            .map(|reqs| reqs.iter().map(to_proto_security_requirement).collect())
            .unwrap_or_default(),
        default_input_modes: c.default_input_modes.clone(),
        default_output_modes: c.default_output_modes.clone(),
        skills: c.skills.iter().map(to_proto_agent_skill).collect(),
        signatures: c
            .signatures
            .as_ref()
            .map(|sigs| sigs.iter().map(to_proto_agent_card_signature).collect())
            .unwrap_or_default(),
        icon_url: c.icon_url.clone(),
    }
}

pub fn from_proto_agent_card(c: &proto::AgentCard) -> AgentCard {
    AgentCard {
        name: c.name.clone(),
        description: c.description.clone(),
        version: c.version.clone(),
        supported_interfaces: c
            .supported_interfaces
            .iter()
            .map(from_proto_agent_interface)
            .collect(),
        capabilities: c
            .capabilities
            .as_ref()
            .map(from_proto_agent_capabilities)
            .unwrap_or_default(),
        default_input_modes: c.default_input_modes.clone(),
        default_output_modes: c.default_output_modes.clone(),
        skills: c.skills.iter().map(from_proto_agent_skill).collect(),
        provider: c.provider.as_ref().map(from_proto_agent_provider),
        documentation_url: c.documentation_url.clone(),
        icon_url: c.icon_url.clone(),
        security_schemes: if c.security_schemes.is_empty() {
            None
        } else {
            Some(
                c.security_schemes
                    .iter()
                    .filter_map(|(k, v)| from_proto_security_scheme(v).map(|ss| (k.clone(), ss)))
                    .collect(),
            )
        },
        security_requirements: if c.security_requirements.is_empty() {
            None
        } else {
            Some(
                c.security_requirements
                    .iter()
                    .map(from_proto_security_requirement)
                    .collect(),
            )
        },
        signatures: if c.signatures.is_empty() {
            None
        } else {
            Some(
                c.signatures
                    .iter()
                    .map(from_proto_agent_card_signature)
                    .collect(),
            )
        },
    }
}

// ---------------------------------------------------------------------------
// CreateTaskPushNotificationConfigRequest
// ---------------------------------------------------------------------------

pub fn to_proto_create_task_push_notification_config_request(
    r: &CreateTaskPushNotificationConfigRequest,
) -> proto::TaskPushNotificationConfig {
    let tpnc = TaskPushNotificationConfig {
        task_id: r.task_id.clone(),
        config: r.config.clone(),
        tenant: r.tenant.clone(),
    };
    to_proto_task_push_notification_config(&tpnc)
}

pub fn from_proto_create_task_push_notification_config_request(
    r: &proto::TaskPushNotificationConfig,
) -> CreateTaskPushNotificationConfigRequest {
    let tpnc = from_proto_task_push_notification_config(r);
    CreateTaskPushNotificationConfigRequest {
        task_id: tpnc.task_id,
        config: tpnc.config,
        tenant: tpnc.tenant,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_role_roundtrip() {
        for role in [Role::Unspecified, Role::User, Role::Agent] {
            let proto_val = to_proto_role(&role);
            let back = from_proto_role(proto_val);
            assert_eq!(role, back);
        }
    }

    #[test]
    fn test_task_state_roundtrip() {
        let states = [
            TaskState::Unspecified,
            TaskState::Submitted,
            TaskState::Working,
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Canceled,
            TaskState::InputRequired,
            TaskState::Rejected,
            TaskState::AuthRequired,
        ];
        for state in states {
            let proto_val = to_proto_task_state(&state);
            let back = from_proto_task_state(proto_val);
            assert_eq!(state, back);
        }
    }

    #[test]
    fn test_part_text_roundtrip() {
        let part = Part::text("hello world");
        let proto_part = to_proto_part(&part);
        let back = from_proto_part(&proto_part);
        assert_eq!(part, back);
    }

    #[test]
    fn test_part_raw_roundtrip() {
        let part = Part::raw(vec![1, 2, 3, 4]);
        let proto_part = to_proto_part(&part);
        let back = from_proto_part(&proto_part);
        assert_eq!(part, back);
    }

    #[test]
    fn test_part_with_metadata() {
        let mut meta = HashMap::new();
        meta.insert("key".to_string(), Value::String("value".to_string()));
        let part = Part {
            content: PartContent::Text("hello".to_string()),
            filename: Some("test.txt".to_string()),
            media_type: Some("text/plain".to_string()),
            metadata: Some(meta),
        };
        let proto_part = to_proto_part(&part);
        let back = from_proto_part(&proto_part);
        assert_eq!(part, back);
    }

    #[test]
    fn test_message_roundtrip() {
        let msg = Message {
            message_id: "msg-1".to_string(),
            context_id: Some("ctx-1".to_string()),
            task_id: Some("task-1".to_string()),
            role: Role::User,
            parts: vec![Part::text("hello")],
            metadata: None,
            extensions: Some(vec!["ext-1".to_string()]),
            reference_task_ids: None,
        };
        let proto_msg = to_proto_message(&msg);
        let back = from_proto_message(&proto_msg);
        assert_eq!(msg, back);
    }

    #[test]
    fn test_task_roundtrip() {
        let task = Task {
            id: "task-1".to_string(),
            context_id: "ctx-1".to_string(),
            status: TaskStatus {
                state: TaskState::Working,
                message: Some(Message::new(Role::Agent, vec![Part::text("processing")])),
                timestamp: None,
            },
            artifacts: Some(vec![Artifact {
                artifact_id: "art-1".to_string(),
                name: Some("result".to_string()),
                description: None,
                parts: vec![Part::text("output")],
                metadata: None,
                extensions: None,
            }]),
            history: None,
            metadata: None,
        };
        let proto_task = to_proto_task(&task);
        let back = from_proto_task(&proto_task);
        // Compare fields individually since Message has random message_id
        assert_eq!(task.id, back.id);
        assert_eq!(task.context_id, back.context_id);
        assert_eq!(task.status.state, back.status.state);
        assert!(back.artifacts.is_some());
    }

    #[test]
    fn test_stream_response_roundtrip() {
        let sr = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t1".to_string(),
            context_id: "c1".to_string(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        let proto_sr = to_proto_stream_response(&sr);
        let back = from_proto_stream_response(&proto_sr).unwrap();
        assert!(matches!(back, StreamResponse::StatusUpdate(_)));
    }

    #[test]
    fn test_send_message_response_task_roundtrip() {
        let resp = SendMessageResponse::Task(Task {
            id: "t1".to_string(),
            context_id: "c1".to_string(),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        });
        let proto_resp = to_proto_send_message_response(&resp);
        let back = from_proto_send_message_response(&proto_resp).unwrap();
        assert!(matches!(back, SendMessageResponse::Task(_)));
    }

    #[test]
    fn test_security_scheme_apikey_roundtrip() {
        let ss = SecurityScheme::ApiKey(ApiKeySecurityScheme {
            location: "header".to_string(),
            name: "X-API-Key".to_string(),
            description: Some("API key auth".to_string()),
        });
        let proto_ss = to_proto_security_scheme(&ss);
        let back = from_proto_security_scheme(&proto_ss).unwrap();
        assert_eq!(ss, back);
    }

    #[test]
    fn test_agent_card_roundtrip() {
        let card = AgentCard {
            name: "Test Agent".to_string(),
            description: "Test".to_string(),
            version: "1.0.0".to_string(),
            supported_interfaces: vec![AgentInterface::new("http://localhost:3000", "JSONRPC")],
            capabilities: AgentCapabilities::default(),
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: vec![AgentSkill {
                id: "echo".to_string(),
                name: "Echo".to_string(),
                description: "Echoes back".to_string(),
                tags: vec!["echo".to_string()],
                examples: None,
                input_modes: None,
                output_modes: None,
                security_requirements: None,
            }],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        };
        let proto_card = to_proto_agent_card(&card);
        let back = from_proto_agent_card(&proto_card);
        assert_eq!(card, back);
    }

    #[test]
    fn test_struct_roundtrip() {
        let mut map = HashMap::new();
        map.insert("str".to_string(), Value::String("hello".to_string()));
        map.insert(
            "num".to_string(),
            Value::Number(serde_json::Number::from_f64(42.0).unwrap()),
        );
        map.insert("bool".to_string(), Value::Bool(true));
        map.insert("null".to_string(), Value::Null);
        let proto_s = to_proto_struct(&map);
        let back = from_proto_struct(&proto_s);
        assert_eq!(map, back);
    }

    #[test]
    fn test_push_notification_config_roundtrip() {
        let tpnc = TaskPushNotificationConfig {
            task_id: "task-1".to_string(),
            tenant: Some("tenant-1".to_string()),
            config: PushNotificationConfig {
                url: "https://example.com/webhook".to_string(),
                id: Some("config-1".to_string()),
                token: Some("tok-123".to_string()),
                authentication: Some(AuthenticationInfo {
                    scheme: "Bearer".to_string(),
                    credentials: Some("secret".to_string()),
                }),
            },
        };
        let proto_tpnc = to_proto_task_push_notification_config(&tpnc);
        let back = from_proto_task_push_notification_config(&proto_tpnc);
        assert_eq!(tpnc, back);
    }

    #[test]
    fn test_send_message_request_roundtrip() {
        let req = SendMessageRequest {
            message: Message {
                message_id: "m1".to_string(),
                context_id: Some("ctx".to_string()),
                task_id: Some("t1".to_string()),
                role: Role::User,
                parts: vec![Part::text("hello")],
                metadata: None,
                extensions: None,
                reference_task_ids: Some(vec!["ref1".to_string()]),
            },
            configuration: Some(SendMessageConfiguration {
                accepted_output_modes: Some(vec!["text/plain".to_string()]),
                push_notification_config: Some(PushNotificationConfig {
                    url: "http://push.example.com".to_string(),
                    id: Some("pnc-1".to_string()),
                    token: Some("tok".to_string()),
                    authentication: Some(AuthenticationInfo {
                        scheme: "Bearer".to_string(),
                        credentials: Some("secret".to_string()),
                    }),
                }),
                history_length: Some(5),
                return_immediately: Some(true),
            }),
            metadata: None,
            tenant: Some("my-tenant".to_string()),
        };
        let proto = to_proto_send_message_request(&req);
        let back = from_proto_send_message_request(&proto);
        assert_eq!(req.tenant, back.tenant);
        assert_eq!(req.message.message_id, back.message.message_id);
        assert_eq!(
            req.configuration.as_ref().unwrap().accepted_output_modes,
            back.configuration.as_ref().unwrap().accepted_output_modes
        );
        assert_eq!(
            req.configuration.as_ref().unwrap().history_length,
            back.configuration.as_ref().unwrap().history_length
        );
        assert_eq!(
            req.configuration.as_ref().unwrap().return_immediately,
            back.configuration.as_ref().unwrap().return_immediately
        );
    }

    #[test]
    fn test_send_message_request_minimal() {
        let proto = proto::SendMessageRequest {
            tenant: String::new(),
            message: None,
            configuration: None,
            metadata: None,
        };
        let back = from_proto_send_message_request(&proto);
        assert!(back.tenant.is_none());
        assert_eq!(back.message.role, Role::User);
    }

    #[test]
    fn test_get_task_request_roundtrip() {
        let req = GetTaskRequest {
            id: "t1".to_string(),
            history_length: Some(10),
            tenant: Some("ten".to_string()),
        };
        let proto = to_proto_get_task_request(&req);
        let back = from_proto_get_task_request(&proto);
        assert_eq!(req, back);
    }

    #[test]
    fn test_list_tasks_request_roundtrip() {
        let req = ListTasksRequest {
            context_id: Some("ctx-1".to_string()),
            status: Some(TaskState::Working),
            page_size: Some(10),
            page_token: Some("token".to_string()),
            history_length: Some(5),
            status_timestamp_after: None,
            include_artifacts: Some(true),
            tenant: Some("my-tenant".to_string()),
        };
        let proto = to_proto_list_tasks_request(&req);
        let back = from_proto_list_tasks_request(&proto);
        assert_eq!(req.context_id, back.context_id);
        assert_eq!(req.status, back.status);
        assert_eq!(req.page_size, back.page_size);
        assert_eq!(req.page_token, back.page_token);
        assert_eq!(req.history_length, back.history_length);
        assert_eq!(req.include_artifacts, back.include_artifacts);
        assert_eq!(req.tenant, back.tenant);
    }

    #[test]
    fn test_list_tasks_response_roundtrip() {
        let resp = ListTasksResponse {
            tasks: vec![Task {
                id: "t1".to_string(),
                context_id: "c1".to_string(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            }],
            next_page_token: "next".to_string(),
            page_size: 10,
            total_size: 1,
        };
        let proto = to_proto_list_tasks_response(&resp);
        let back = from_proto_list_tasks_response(&proto);
        assert_eq!(resp.tasks.len(), back.tasks.len());
        assert_eq!(resp.next_page_token, back.next_page_token);
        assert_eq!(resp.page_size, back.page_size);
        assert_eq!(resp.total_size, back.total_size);
    }

    #[test]
    fn test_cancel_task_request_roundtrip() {
        let mut meta = HashMap::new();
        meta.insert("reason".to_string(), Value::String("user".to_string()));
        let req = CancelTaskRequest {
            id: "t1".to_string(),
            metadata: Some(meta),
            tenant: Some("ten".to_string()),
        };
        let proto = to_proto_cancel_task_request(&req);
        let back = from_proto_cancel_task_request(&proto);
        assert_eq!(req, back);
    }

    #[test]
    fn test_subscribe_to_task_request_roundtrip() {
        let req = SubscribeToTaskRequest {
            id: "t1".to_string(),
            tenant: Some("ten".to_string()),
        };
        let proto = to_proto_subscribe_to_task_request(&req);
        let back = from_proto_subscribe_to_task_request(&proto);
        assert_eq!(req, back);
    }

    #[test]
    fn test_get_extended_agent_card_request_roundtrip() {
        let req = GetExtendedAgentCardRequest {
            tenant: Some("ten".to_string()),
        };
        let proto = to_proto_get_extended_agent_card_request(&req);
        let back = from_proto_get_extended_agent_card_request(&proto);
        assert_eq!(req, back);
    }

    #[test]
    fn test_get_push_notification_config_request_roundtrip() {
        let req = GetTaskPushNotificationConfigRequest {
            task_id: "t1".to_string(),
            id: "cfg1".to_string(),
            tenant: Some("ten".to_string()),
        };
        let proto = to_proto_get_task_push_notification_config_request(&req);
        let back = from_proto_get_task_push_notification_config_request(&proto);
        assert_eq!(req, back);
    }

    #[test]
    fn test_delete_push_notification_config_request_roundtrip() {
        let req = DeleteTaskPushNotificationConfigRequest {
            task_id: "t1".to_string(),
            id: "cfg1".to_string(),
            tenant: Some("ten".to_string()),
        };
        let proto = to_proto_delete_task_push_notification_config_request(&req);
        let back = from_proto_delete_task_push_notification_config_request(&proto);
        assert_eq!(req, back);
    }

    #[test]
    fn test_list_push_notification_configs_request_roundtrip() {
        let req = ListTaskPushNotificationConfigsRequest {
            task_id: "t1".to_string(),
            page_size: Some(5),
            page_token: Some("tok".to_string()),
            tenant: Some("ten".to_string()),
        };
        let proto = to_proto_list_task_push_notification_configs_request(&req);
        let back = from_proto_list_task_push_notification_configs_request(&proto);
        assert_eq!(req, back);
    }

    #[test]
    fn test_list_push_notification_configs_response_roundtrip() {
        let resp = ListTaskPushNotificationConfigsResponse {
            configs: vec![TaskPushNotificationConfig {
                task_id: "t1".to_string(),
                tenant: None,
                config: PushNotificationConfig {
                    url: "http://x.com".to_string(),
                    id: Some("c1".to_string()),
                    token: None,
                    authentication: None,
                },
            }],
            next_page_token: Some("next".to_string()),
        };
        let proto = to_proto_list_task_push_notification_configs_response(&resp);
        let back = from_proto_list_task_push_notification_configs_response(&proto);
        assert_eq!(resp.configs.len(), back.configs.len());
        assert_eq!(resp.next_page_token, back.next_page_token);
    }

    #[test]
    fn test_send_message_response_message_roundtrip() {
        let msg = Message::new(Role::Agent, vec![Part::text("hi")]);
        let resp = SendMessageResponse::Message(msg);
        let proto = to_proto_send_message_response(&resp);
        let back = from_proto_send_message_response(&proto).unwrap();
        assert!(matches!(back, SendMessageResponse::Message(_)));
    }

    #[test]
    fn test_send_message_response_none() {
        let proto = proto::SendMessageResponse { payload: None };
        assert!(from_proto_send_message_response(&proto).is_none());
    }

    #[test]
    fn test_stream_response_task_roundtrip() {
        let task = Task {
            id: "t1".to_string(),
            context_id: "c1".to_string(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        let sr = StreamResponse::Task(task);
        let proto = to_proto_stream_response(&sr);
        let back = from_proto_stream_response(&proto).unwrap();
        assert!(matches!(back, StreamResponse::Task(_)));
    }

    #[test]
    fn test_stream_response_message_roundtrip() {
        let msg = Message::new(Role::Agent, vec![Part::text("hi")]);
        let sr = StreamResponse::Message(msg);
        let proto = to_proto_stream_response(&sr);
        let back = from_proto_stream_response(&proto).unwrap();
        assert!(matches!(back, StreamResponse::Message(_)));
    }

    #[test]
    fn test_stream_response_artifact_update_roundtrip() {
        let sr = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            task_id: "t1".to_string(),
            context_id: "c1".to_string(),
            artifact: Artifact {
                artifact_id: "a1".to_string(),
                name: Some("file".to_string()),
                description: None,
                parts: vec![Part::text("data")],
                metadata: None,
                extensions: Some(vec!["ext-1".to_string()]),
            },
            append: Some(true),
            last_chunk: Some(true),
            metadata: None,
        });
        let proto = to_proto_stream_response(&sr);
        let back = from_proto_stream_response(&proto).unwrap();
        assert!(matches!(back, StreamResponse::ArtifactUpdate(_)));
    }

    #[test]
    fn test_stream_response_none() {
        let proto = proto::StreamResponse { payload: None };
        assert!(from_proto_stream_response(&proto).is_none());
    }

    #[test]
    fn test_security_scheme_http_auth_roundtrip() {
        let ss = SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
            scheme: "bearer".to_string(),
            description: Some("Bearer token".to_string()),
            bearer_format: Some("JWT".to_string()),
        });
        let proto = to_proto_security_scheme(&ss);
        let back = from_proto_security_scheme(&proto).unwrap();
        assert_eq!(ss, back);
    }

    #[test]
    fn test_security_scheme_openid_roundtrip() {
        let ss = SecurityScheme::OpenIdConnect(OpenIdConnectSecurityScheme {
            open_id_connect_url: "https://example.com/.well-known/openid".to_string(),
            description: None,
        });
        let proto = to_proto_security_scheme(&ss);
        let back = from_proto_security_scheme(&proto).unwrap();
        assert_eq!(ss, back);
    }

    #[test]
    fn test_security_scheme_mtls_roundtrip() {
        let ss = SecurityScheme::MutualTls(MutualTlsSecurityScheme {
            description: Some("mTLS".to_string()),
        });
        let proto = to_proto_security_scheme(&ss);
        let back = from_proto_security_scheme(&proto).unwrap();
        assert_eq!(ss, back);
    }

    #[test]
    fn test_security_scheme_none() {
        let proto = proto::SecurityScheme { scheme: None };
        assert!(from_proto_security_scheme(&proto).is_none());
    }

    #[test]
    fn test_oauth_flows_authorization_code_roundtrip() {
        let flows = OAuthFlows::AuthorizationCode(AuthorizationCodeOAuthFlow {
            authorization_url: "https://auth.example.com".to_string(),
            token_url: "https://token.example.com".to_string(),
            scopes: {
                let mut s = HashMap::new();
                s.insert("read".to_string(), "Read access".to_string());
                s
            },
            refresh_url: Some("https://refresh.example.com".to_string()),
            pkce_required: Some(true),
        });
        let proto = to_proto_oauth_flows(&flows);
        let back = from_proto_oauth_flows(&proto).unwrap();
        assert_eq!(flows, back);
    }

    #[test]
    fn test_oauth_flows_client_credentials_roundtrip() {
        let flows = OAuthFlows::ClientCredentials(ClientCredentialsOAuthFlow {
            token_url: "https://token.example.com".to_string(),
            scopes: HashMap::new(),
            refresh_url: None,
        });
        let proto = to_proto_oauth_flows(&flows);
        let back = from_proto_oauth_flows(&proto).unwrap();
        assert_eq!(flows, back);
    }

    #[test]
    fn test_oauth_flows_device_code_roundtrip() {
        let flows = OAuthFlows::DeviceCode(DeviceCodeOAuthFlow {
            device_authorization_url: "https://device.example.com".to_string(),
            token_url: "https://token.example.com".to_string(),
            scopes: HashMap::new(),
            refresh_url: None,
        });
        let proto = to_proto_oauth_flows(&flows);
        let back = from_proto_oauth_flows(&proto).unwrap();
        assert_eq!(flows, back);
    }

    #[test]
    fn test_oauth_flows_implicit_roundtrip() {
        let flows = OAuthFlows::Implicit(ImplicitOAuthFlow {
            authorization_url: "https://auth.example.com".to_string(),
            scopes: HashMap::new(),
            refresh_url: None,
        });
        let proto = to_proto_oauth_flows(&flows);
        let back = from_proto_oauth_flows(&proto).unwrap();
        assert_eq!(flows, back);
    }

    #[test]
    fn test_oauth_flows_password_roundtrip() {
        let flows = OAuthFlows::Password(PasswordOAuthFlow {
            token_url: "https://token.example.com".to_string(),
            scopes: HashMap::new(),
            refresh_url: None,
        });
        let proto = to_proto_oauth_flows(&flows);
        let back = from_proto_oauth_flows(&proto).unwrap();
        assert_eq!(flows, back);
    }

    #[test]
    fn test_oauth_flows_none() {
        let proto = proto::OAuthFlows { flow: None };
        assert!(from_proto_oauth_flows(&proto).is_none());
    }

    #[test]
    fn test_security_scheme_oauth2_roundtrip() {
        let ss = SecurityScheme::OAuth2(OAuth2SecurityScheme {
            description: Some("OAuth2".to_string()),
            flows: OAuthFlows::ClientCredentials(ClientCredentialsOAuthFlow {
                token_url: "https://token.example.com".to_string(),
                scopes: HashMap::new(),
                refresh_url: None,
            }),
            oauth2_metadata_url: Some("https://meta.example.com".to_string()),
        });
        let proto = to_proto_security_scheme(&ss);
        let back = from_proto_security_scheme(&proto).unwrap();
        assert_eq!(ss, back);
    }

    #[test]
    fn test_agent_skill_with_all_fields() {
        let skill = AgentSkill {
            id: "s1".to_string(),
            name: "Skill".to_string(),
            description: "A skill".to_string(),
            tags: vec!["tag1".to_string()],
            examples: Some(vec!["example1".to_string()]),
            input_modes: Some(vec!["text/plain".to_string()]),
            output_modes: Some(vec!["application/json".to_string()]),
            security_requirements: Some(vec![{
                let mut r = HashMap::new();
                r.insert("api_key".to_string(), vec!["read".to_string()]);
                r
            }]),
        };
        let proto = to_proto_agent_skill(&skill);
        let back = from_proto_agent_skill(&proto);
        assert_eq!(skill, back);
    }

    #[test]
    fn test_agent_extension_roundtrip() {
        let ext = AgentExtension {
            uri: "https://ext.example.com".to_string(),
            description: Some("An extension".to_string()),
            required: Some(true),
            params: Some({
                let mut m = HashMap::new();
                m.insert("key".to_string(), Value::String("val".to_string()));
                m
            }),
        };
        let proto = to_proto_agent_extension(&ext);
        let back = from_proto_agent_extension(&proto);
        assert_eq!(ext, back);
    }

    #[test]
    fn test_agent_capabilities_with_extensions() {
        let caps = AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(true),
            extensions: Some(vec![AgentExtension {
                uri: "ext://1".to_string(),
                description: None,
                required: None,
                params: None,
            }]),
            extended_agent_card: Some(true),
        };
        let proto = to_proto_agent_capabilities(&caps);
        let back = from_proto_agent_capabilities(&proto);
        assert_eq!(caps, back);
    }

    #[test]
    fn test_agent_card_with_security_and_signatures() {
        let card = AgentCard {
            name: "Secure Agent".to_string(),
            description: "Agent with security".to_string(),
            version: "2.0".to_string(),
            supported_interfaces: vec![AgentInterface::new("http://localhost", "JSONRPC")],
            capabilities: AgentCapabilities::default(),
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: vec![],
            provider: Some(AgentProvider {
                organization: "ACME".to_string(),
                url: "https://acme.com".to_string(),
            }),
            documentation_url: Some("https://docs.acme.com".to_string()),
            icon_url: Some("https://acme.com/icon.png".to_string()),
            security_schemes: Some({
                let mut m = HashMap::new();
                m.insert(
                    "api_key".to_string(),
                    SecurityScheme::ApiKey(ApiKeySecurityScheme {
                        location: "header".to_string(),
                        name: "X-Key".to_string(),
                        description: None,
                    }),
                );
                m
            }),
            security_requirements: Some(vec![{
                let mut r = HashMap::new();
                r.insert("api_key".to_string(), vec![]);
                r
            }]),
            signatures: Some(vec![AgentCardSignature {
                protected: "eyJ0eXAiOiJKV1QifQ".to_string(),
                signature: "abc123".to_string(),
                header: None,
            }]),
        };
        let proto = to_proto_agent_card(&card);
        let back = from_proto_agent_card(&proto);
        assert_eq!(card, back);
    }

    #[test]
    fn test_timestamp_roundtrip() {
        use chrono::Utc;
        let now = Utc::now();
        let proto = to_proto_timestamp(&now);
        let back = from_proto_timestamp(&proto).unwrap();
        // Compare to microsecond level (proto loses sub-nanosecond)
        assert_eq!(now.timestamp(), back.timestamp());
    }

    #[test]
    fn test_task_status_with_timestamp() {
        let ts = TaskStatus {
            state: TaskState::Completed,
            message: Some(Message::new(Role::Agent, vec![Part::text("done")])),
            timestamp: chrono::DateTime::from_timestamp(1700000000, 0),
        };
        let proto = to_proto_task_status(&ts);
        let back = from_proto_task_status(&proto);
        assert_eq!(ts.state, back.state);
        assert!(back.timestamp.is_some());
        assert!(back.message.is_some());
    }

    #[test]
    fn test_task_with_history() {
        let task = Task {
            id: "t1".to_string(),
            context_id: "c1".to_string(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: Some(vec![
                Message::new(Role::User, vec![Part::text("q")]),
                Message::new(Role::Agent, vec![Part::text("a")]),
            ]),
            metadata: Some({
                let mut m = HashMap::new();
                m.insert("k".to_string(), Value::Bool(true));
                m
            }),
        };
        let proto = to_proto_task(&task);
        let back = from_proto_task(&proto);
        assert!(back.history.is_some());
        assert_eq!(back.history.unwrap().len(), 2);
        assert!(back.metadata.is_some());
    }

    #[test]
    fn test_part_url_roundtrip() {
        let part = Part {
            content: PartContent::Url("https://example.com/file.txt".to_string()),
            filename: None,
            media_type: None,
            metadata: None,
        };
        let proto = to_proto_part(&part);
        let back = from_proto_part(&proto);
        assert_eq!(part, back);
    }

    #[test]
    fn test_part_data_roundtrip() {
        // Use f64 numbers since proto Value stores all numbers as f64
        let data = serde_json::json!({"nested": [1.0, 2.0, 3.0]});
        let part = Part {
            content: PartContent::Data(data),
            filename: None,
            media_type: None,
            metadata: None,
        };
        let proto = to_proto_part(&part);
        let back = from_proto_part(&proto);
        assert_eq!(part, back);
    }

    #[test]
    fn test_part_none_content() {
        let proto = proto::Part {
            content: None,
            filename: String::new(),
            media_type: String::new(),
            metadata: None,
        };
        let back = from_proto_part(&proto);
        // None content maps to empty text
        assert!(matches!(back.content, PartContent::Text(ref s) if s.is_empty()));
    }

    #[test]
    fn test_json_value_list_roundtrip() {
        let val = Value::Array(vec![
            Value::String("a".to_string()),
            Value::Number(serde_json::Number::from(42)),
            Value::Bool(false),
            Value::Null,
        ]);
        let proto = json_value_to_proto_value(&val);
        let back = proto_value_to_json_value(&proto);
        // Numbers get stored as f64
        assert!(back.is_array());
        assert_eq!(back.as_array().unwrap().len(), 4);
    }

    #[test]
    fn test_proto_value_none_kind() {
        let v = prost_types::Value { kind: None };
        assert_eq!(proto_value_to_json_value(&v), Value::Null);
    }

    #[test]
    fn test_create_push_notification_config_request_roundtrip() {
        let req = CreateTaskPushNotificationConfigRequest {
            task_id: "t1".to_string(),
            config: PushNotificationConfig {
                url: "http://cb.example.com".to_string(),
                id: Some("c1".to_string()),
                token: Some("tok".to_string()),
                authentication: None,
            },
            tenant: Some("ten".to_string()),
        };
        let proto = to_proto_create_task_push_notification_config_request(&req);
        let back = from_proto_create_task_push_notification_config_request(&proto);
        assert_eq!(req.task_id, back.task_id);
        assert_eq!(req.config.url, back.config.url);
    }

    #[test]
    fn test_authentication_info_roundtrip() {
        let auth = AuthenticationInfo {
            scheme: "Bearer".to_string(),
            credentials: Some("token123".to_string()),
        };
        let proto = to_proto_authentication_info(&auth);
        let back = from_proto_authentication_info(&proto);
        assert_eq!(auth, back);
    }

    #[test]
    fn test_authentication_info_no_credentials() {
        let auth = AuthenticationInfo {
            scheme: "Bearer".to_string(),
            credentials: None,
        };
        let proto = to_proto_authentication_info(&auth);
        let back = from_proto_authentication_info(&proto);
        assert_eq!(auth, back);
    }

    #[test]
    fn test_agent_interface_roundtrip() {
        let iface = AgentInterface {
            url: "http://localhost:8080".to_string(),
            protocol_binding: "JSONRPC".to_string(),
            protocol_version: "1.0".to_string(),
            tenant: Some("ten".to_string()),
        };
        let proto = to_proto_agent_interface(&iface);
        let back = from_proto_agent_interface(&proto);
        assert_eq!(iface, back);
    }

    #[test]
    fn test_agent_card_signature_with_header() {
        let sig = AgentCardSignature {
            protected: "eyJ0eXAiOiJKV1QifQ".to_string(),
            signature: "sig123".to_string(),
            header: Some({
                let mut m = HashMap::new();
                m.insert("kid".to_string(), Value::String("key1".to_string()));
                m
            }),
        };
        let proto = to_proto_agent_card_signature(&sig);
        let back = from_proto_agent_card_signature(&proto);
        assert_eq!(sig, back);
    }

    #[test]
    fn test_send_message_configuration_defaults() {
        let config = SendMessageConfiguration {
            accepted_output_modes: None,
            push_notification_config: None,
            history_length: None,
            return_immediately: None,
        };
        let proto = to_proto_send_message_configuration(&config);
        let back = from_proto_send_message_configuration(&proto);
        assert_eq!(config, back);
    }

    #[test]
    fn test_security_requirement_roundtrip() {
        let mut req: SecurityRequirement = HashMap::new();
        req.insert(
            "oauth2".to_string(),
            vec!["read".to_string(), "write".to_string()],
        );
        req.insert("api_key".to_string(), vec![]);
        let proto = to_proto_security_requirement(&req);
        let back = from_proto_security_requirement(&proto);
        assert_eq!(req, back);
    }
}
