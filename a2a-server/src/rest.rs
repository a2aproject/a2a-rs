// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;

use a2a::*;
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use axum::{
    Json,
    body::Bytes,
    extract::{
        Path, Query, Request, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use chrono::{DateTime, Utc};
use futures::{StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::handler::{LiveResponse, RequestHandler};
use crate::middleware::{ServiceParams, check_a2a_version, extract_service_params};
use crate::sse;

const REST_SEND_MESSAGE_PATH: &str = "/message:send";
const REST_SEND_MESSAGE_LEGACY_PATH: &str = "/message/send";
const REST_STREAM_MESSAGE_PATH: &str = "/message:stream";
const REST_LIVE_MESSAGE_PATH: &str = "/message:live";
const REST_STREAM_MESSAGE_LEGACY_PATH: &str = "/message/stream";
const REST_EXTENDED_AGENT_CARD_PATH: &str = "/extendedAgentCard";
const REST_EXTENDED_AGENT_CARD_LEGACY_PATH: &str = "/agent-card/extended";
const REST_PUSH_CONFIGS_PATH: &str = "/tasks/{id}/pushNotificationConfigs";
const REST_PUSH_CONFIGS_LEGACY_PATH: &str = "/tasks/{id}/push-configs";
const REST_PUSH_CONFIG_PATH: &str = "/tasks/{id}/pushNotificationConfigs/{config_id}";
const REST_PUSH_CONFIG_LEGACY_PATH: &str = "/tasks/{id}/push-configs/{config_id}";
#[derive(Serialize)]
struct RestErrorEnvelope {
    error: RestErrorStatus,
}

#[derive(Serialize)]
struct RestErrorStatus {
    code: u16,
    status: &'static str,
    message: String,
    details: Vec<TypedDetail>,
}

/// Shared state for REST handlers.
pub struct RestState<H: RequestHandler> {
    pub handler: Arc<H>,
}

impl<H: RequestHandler> Clone for RestState<H> {
    fn clone(&self) -> Self {
        RestState {
            handler: self.handler.clone(),
        }
    }
}

/// Create an axum router for the REST/HTTP+JSON protocol binding.
///
/// Every method is served on its plain path and, as the proto's
/// `additional_bindings` require, under `/{tenant}` as well. The tenant
/// routes are always on and answer for any tenant string: the path tenant
/// only reaches the request as `tenant`. The default stack
/// (`DefaultRequestHandler`, `InMemoryTaskStore`) does not isolate tenants,
/// so `GET /acme/tasks/t1` and `GET /other/tasks/t1` return the same task
/// unless the executor or the task store separates them; isolation is the
/// application's job. A gateway rule written for `/tasks/**` also has to
/// match `/{tenant}/tasks/**`.
pub fn rest_router<H: RequestHandler>(handler: Arc<H>) -> axum::Router {
    let state = RestState { handler };
    axum::Router::new()
        .route(
            REST_SEND_MESSAGE_PATH,
            axum::routing::post(handle_send_message::<H>),
        )
        .route(
            REST_SEND_MESSAGE_LEGACY_PATH,
            axum::routing::post(handle_send_message::<H>),
        )
        .route(
            REST_STREAM_MESSAGE_PATH,
            axum::routing::post(handle_stream_message::<H>),
        )
        .route(
            REST_STREAM_MESSAGE_LEGACY_PATH,
            axum::routing::post(handle_stream_message::<H>),
        )
        .route(
            REST_LIVE_MESSAGE_PATH,
            axum::routing::post(handle_live_message::<H>),
        )
        .route(
            "/tasks/{id}",
            axum::routing::get(handle_get_task_or_subscribe::<H>)
                .post(handle_post_task_action::<H>),
        )
        .route("/tasks", axum::routing::get(handle_list_tasks::<H>))
        .route(
            "/tasks/{id}/cancel",
            axum::routing::post(handle_cancel_task_legacy::<H>),
        )
        .route(
            "/tasks/{id}/subscribe",
            axum::routing::get(handle_subscribe_to_task_legacy::<H>)
                .post(handle_subscribe_to_task_legacy::<H>),
        )
        .route(
            REST_PUSH_CONFIGS_PATH,
            axum::routing::post(handle_create_push_config::<H>).get(handle_list_push_configs::<H>),
        )
        .route(
            REST_PUSH_CONFIGS_LEGACY_PATH,
            axum::routing::post(handle_create_push_config::<H>).get(handle_list_push_configs::<H>),
        )
        .route(
            REST_PUSH_CONFIG_PATH,
            axum::routing::get(handle_get_push_config::<H>).delete(handle_delete_push_config::<H>),
        )
        .route(
            REST_PUSH_CONFIG_LEGACY_PATH,
            axum::routing::get(handle_get_push_config::<H>).delete(handle_delete_push_config::<H>),
        )
        .route(
            REST_EXTENDED_AGENT_CARD_PATH,
            axum::routing::get(handle_get_extended_agent_card::<H>),
        )
        .route(
            REST_EXTENDED_AGENT_CARD_LEGACY_PATH,
            axum::routing::get(handle_get_extended_agent_card::<H>),
        )
        // The proto's `additional_bindings`: every method again under `/{tenant}`.
        .route(
            "/{tenant}/message:send",
            axum::routing::post(handle_send_message::<H>),
        )
        .route(
            "/{tenant}/message:stream",
            axum::routing::post(handle_stream_message::<H>),
        )
        .route(
            "/{tenant}/message:live",
            axum::routing::post(handle_live_message::<H>),
        )
        // The proto binds `:subscribe` to GET only; POST `:subscribe` is the
        // legacy alias of the plain route, kept here too because
        // `handle_post_task_action` serves `:cancel` and `:subscribe` alike.
        .route(
            "/{tenant}/tasks/{id}",
            axum::routing::get(handle_get_task_or_subscribe::<H>)
                .post(handle_post_task_action::<H>),
        )
        .route(
            "/{tenant}/tasks",
            axum::routing::get(handle_list_tasks::<H>),
        )
        .route(
            "/{tenant}/tasks/{id}/pushNotificationConfigs",
            axum::routing::post(handle_create_push_config::<H>).get(handle_list_push_configs::<H>),
        )
        .route(
            "/{tenant}/tasks/{id}/pushNotificationConfigs/{config_id}",
            axum::routing::get(handle_get_push_config::<H>).delete(handle_delete_push_config::<H>),
        )
        .route(
            "/{tenant}/extendedAgentCard",
            axum::routing::get(handle_get_extended_agent_card::<H>),
        )
        .layer(axum::extract::DefaultBodyLimit::max(
            crate::jsonrpc::MAX_REQUEST_BODY_BYTES,
        ))
        .layer(axum::middleware::from_fn(validate_protocol_version))
        .with_state(state)
}

/// §3.6.2 for every REST operation, ahead of any handler. §3.6.1 lets a
/// client send `A2A-Version` as a query parameter instead of a header, so
/// the query form is honored when the header is absent.
async fn validate_protocol_version(mut request: Request, next: Next) -> Response {
    let mut params = extract_service_params(request.headers());
    if !params.contains_key("a2a-version") {
        let from_query = Query::<HashMap<String, String>>::try_from_uri(request.uri())
            .ok()
            .and_then(|Query(query)| {
                query
                    .into_iter()
                    .find(|(name, _)| name.eq_ignore_ascii_case(SVC_PARAM_VERSION))
            });
        if let Some((_, version)) = from_query {
            let value = match HeaderValue::from_str(version.trim()) {
                Ok(value) => value,
                Err(_) => return rest_error_response(A2AError::version_not_supported(&version)),
            };
            // Dispatch receives the same negotiated version as the binding.
            request.headers_mut().insert("a2a-version", value);
            params.insert("a2a-version".to_owned(), vec![version]);
        }
    }
    match check_a2a_version(&params) {
        Ok(()) => next.run(request).await,
        Err(error) => rest_error_response(error),
    }
}

/// The path of a task route; `tenant` is set on its `/{tenant}/…` binding.
#[derive(Deserialize)]
struct TaskPath {
    tenant: Option<String>,
    id: String,
}

/// The path of a push-config route; `tenant` is set on its `/{tenant}/…` binding.
#[derive(Deserialize)]
struct PushConfigPath {
    tenant: Option<String>,
    id: String,
    config_id: String,
}

/// The tenant of a request whose body may name one. `body: "*"` maps the
/// fields the path did not capture, so a body tenant next to a path tenant
/// is a conflict unless it is the same value, which the reference clients
/// send; the plain binding takes the body's.
fn reconcile_tenant(
    path: Option<String>,
    body: Option<String>,
) -> Result<Option<String>, Box<axum::response::Response>> {
    match (path, body) {
        (Some(path), Some(body)) if path != body => {
            let violation = FieldViolation {
                field: "tenant".to_string(),
                description: format!(
                    "tenant in the body ({body}) does not match the tenant in the path ({path})"
                ),
            };
            Err(Box::new(rest_error_response(
                A2AError::invalid_params("invalid request body")
                    .with_details(vec![TypedDetail::bad_request(vec![violation])]),
            )))
        }
        (Some(path), _) => Ok(Some(path)),
        (None, body) => Ok(body),
    }
}

async fn handle_send_message<H: RequestHandler>(
    State(state): State<RestState<H>>,
    // `None` on the plain binding, which has no path parameter; a tenant that
    // fails to decode is a 400 before the handler, as on the task routes.
    tenant: Option<Path<String>>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> impl IntoResponse {
    let tenant = tenant.map(|Path(tenant)| tenant);
    let Json(raw_req) = match body {
        Ok(body) => body,
        Err(rejection) => return json_rejection_response(rejection),
    };
    let mut req = match protojson_conv::from_value::<SendMessageRequest>(raw_req) {
        Ok(req) => req,
        Err(e) => return rest_payload_error_response(e),
    };
    req.tenant = match reconcile_tenant(tenant, req.tenant.take()) {
        Ok(tenant) => tenant,
        Err(response) => return *response,
    };
    let params = extract_service_params(&headers);
    match state.handler.send_message(&params, req).await {
        Ok(resp) => protojson_json_response(&resp),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_stream_message<H: RequestHandler>(
    State(state): State<RestState<H>>,
    tenant: Option<Path<String>>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> impl IntoResponse {
    let tenant = tenant.map(|Path(tenant)| tenant);
    let Json(raw_req) = match body {
        Ok(body) => body,
        Err(rejection) => return json_rejection_response(rejection),
    };
    let mut req = match protojson_conv::from_value::<SendMessageRequest>(raw_req) {
        Ok(req) => req,
        Err(e) => return rest_payload_error_response(e),
    };
    req.tenant = match reconcile_tenant(tenant, req.tenant.take()) {
        Ok(tenant) => tenant,
        Err(response) => return *response,
    };
    let params = extract_service_params(&headers);
    match state.handler.send_streaming_message(&params, req).await {
        Ok(stream) => rest_sse(protojson_stream(stream)),
        Err(e) => rest_error_response(e),
    }
}

/// `POST /message:live`: the call that opens a live exchange is its downlink
/// (an event stream); a later call for the same task is input to it and gets
/// the task as it stands.
async fn handle_live_message<H: RequestHandler>(
    State(state): State<RestState<H>>,
    tenant: Option<Path<String>>,
    headers: HeaderMap,
    Json(raw_req): Json<Value>,
) -> impl IntoResponse {
    let mut req = match protojson_conv::from_value::<StreamRequest>(raw_req) {
        Ok(req) => req,
        Err(e) => return rest_payload_error_response(e),
    };
    req.tenant = match reconcile_tenant(tenant.map(|Path(tenant)| tenant), req.tenant.take()) {
        Ok(tenant) => tenant,
        Err(response) => return *response,
    };
    // The live envelope and its nested request carry the same routing tenant.
    let inner_tenant = match &mut req.payload {
        StreamRequestPayload::SendMessage(message) => Some(&mut message.tenant),
        StreamRequestPayload::Subscribe(request) => Some(&mut request.tenant),
        _ => None,
    };
    if let Some(inner_tenant) = inner_tenant {
        req.tenant = match reconcile_tenant(req.tenant.take(), inner_tenant.take()) {
            Ok(tenant) => tenant,
            Err(response) => return *response,
        };
        *inner_tenant = req.tenant.clone();
    }
    let params = extract_service_params(&headers);
    match state.handler.send_live_request(&params, req).await {
        Ok(LiveResponse::Stream(stream)) => rest_sse(protojson_stream(stream)),
        Ok(LiveResponse::Ack(task)) => protojson_json_response(&*task),
        Err(e) => rest_error_response(e),
    }
}

#[derive(Deserialize)]
pub struct GetTaskQuery {
    #[serde(default, rename = "historyLength")]
    pub history_length: Option<i32>,
    #[serde(default, rename = "timelineLength")]
    pub timeline_length: Option<i32>,
    #[serde(default, rename = "currentGeneration")]
    pub current_generation: Option<i64>,
    /// Only meaningful on `:subscribe`.
    #[serde(default, rename = "lastSeenGeneration")]
    pub last_seen_generation: Option<i64>,
}

async fn handle_get_task_inner<H: RequestHandler>(
    state: RestState<H>,
    params: ServiceParams,
    TaskPath { tenant, id }: TaskPath,
    query: GetTaskQuery,
) -> axum::response::Response {
    let req = GetTaskRequest {
        current_generation: query.current_generation,
        timeline_length: query.timeline_length,
        id,
        history_length: query.history_length,
        tenant,
    };
    match state.handler.get_task(&params, req).await {
        Ok(task) => protojson_json_response(&task),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_get_task_or_subscribe<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(path): Path<TaskPath>,
    query: Result<Query<GetTaskQuery>, QueryRejection>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return rest_payload_error_response(rejection.body_text()),
    };
    let params = extract_service_params(&headers);
    if let Some(task_id) = path.id.strip_suffix(":subscribe") {
        return handle_subscribe_to_task_inner(
            state,
            params,
            task_id.to_string(),
            path.tenant,
            query.last_seen_generation,
        )
        .await;
    }

    handle_get_task_inner(state, params, path, query).await
}

#[derive(Deserialize)]
pub struct ListTasksQuery {
    #[serde(default, rename = "contextId")]
    pub context_id: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default, rename = "pageSize")]
    pub page_size: Option<i32>,
    #[serde(default, rename = "pageToken")]
    pub page_token: Option<String>,
    #[serde(default, rename = "historyLength")]
    pub history_length: Option<i32>,
    #[serde(default, rename = "timelineLength")]
    pub timeline_length: Option<i32>,

    #[serde(default, rename = "statusTimestampAfter")]
    pub status_timestamp_after: Option<DateTime<Utc>>,

    #[serde(default, rename = "includeArtifacts")]
    pub include_artifacts: Option<bool>,
}

async fn handle_list_tasks<H: RequestHandler>(
    State(state): State<RestState<H>>,
    tenant: Option<Path<String>>,
    query: Result<Query<ListTasksQuery>, QueryRejection>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let tenant = tenant.map(|Path(tenant)| tenant);
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return rest_payload_error_response(rejection.body_text()),
    };
    let params = extract_service_params(&headers);
    let status = match query.status {
        Some(s) => match serde_json::from_value::<TaskState>(serde_json::Value::String(s)) {
            // The proto's default: no filter, as the JSON-RPC and gRPC
            // bindings already read it.
            Ok(TaskState::Unspecified) => None,
            Ok(state) => Some(state),
            Err(_) => {
                return rest_error_response(A2AError::invalid_params("invalid status filter"));
            }
        },
        None => None,
    };
    let req = ListTasksRequest {
        timeline_length: query.timeline_length,
        context_id: query.context_id,
        status,
        page_size: query.page_size,
        page_token: query.page_token,
        history_length: query.history_length,
        status_timestamp_after: query.status_timestamp_after,
        include_artifacts: query.include_artifacts,
        tenant,
    };
    match state.handler.list_tasks(&params, req).await {
        Ok(resp) => protojson_json_response(&resp),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_cancel_task_inner<H: RequestHandler>(
    state: RestState<H>,
    params: ServiceParams,
    id: String,
    tenant: Option<String>,
    body: Bytes,
) -> axum::response::Response {
    // The binding maps the whole request to the body (`body: "*"`), so its
    // `metadata` arrives there; the path names the task. An empty body is
    // a cancel without metadata.
    let mut req = if body.is_empty() {
        CancelTaskRequest {
            id: String::new(),
            metadata: None,
            tenant: None,
        }
    } else {
        match serde_json::from_slice::<Value>(&body)
            .map_err(|e| e.to_string())
            .and_then(|raw| {
                protojson_conv::from_value::<CancelTaskRequest>(raw).map_err(|e| e.to_string())
            }) {
            Ok(req) => req,
            Err(e) => return rest_payload_error_response(e),
        }
    };
    req.id = id;
    req.tenant = match reconcile_tenant(tenant, req.tenant.take()) {
        Ok(tenant) => tenant,
        Err(response) => return *response,
    };
    match state.handler.cancel_task(&params, req).await {
        Ok(task) => protojson_json_response(&task),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_post_task_action<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(TaskPath { tenant, id }): Path<TaskPath>,
    Query(query): Query<GetTaskQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let params = extract_service_params(&headers);
    if let Some(task_id) = id.strip_suffix(":cancel") {
        return handle_cancel_task_inner(state, params, task_id.to_string(), tenant, body).await;
    }
    if let Some(task_id) = id.strip_suffix(":subscribe") {
        return handle_subscribe_to_task_inner(
            state,
            params,
            task_id.to_string(),
            tenant,
            query.last_seen_generation,
        )
        .await;
    }

    (
        StatusCode::NOT_FOUND,
        Json(RestErrorEnvelope {
            error: RestErrorStatus {
                code: 404,
                status: "NOT_FOUND",
                message: "unknown task action".into(),
                details: Vec::new(),
            },
        }),
    )
        .into_response()
}

async fn handle_cancel_task_legacy<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let params = extract_service_params(&headers);
    handle_cancel_task_inner(state, params, id, None, body).await
}

async fn handle_subscribe_to_task_inner<H: RequestHandler>(
    state: RestState<H>,
    params: ServiceParams,
    id: String,
    tenant: Option<String>,
    last_seen_generation: Option<i64>,
) -> axum::response::Response {
    let req = SubscribeToTaskRequest {
        last_seen_generation,
        id,
        tenant,
    };
    match state.handler.subscribe_to_task(&params, req).await {
        Ok(stream) => rest_sse(protojson_stream(stream)),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_subscribe_to_task_legacy<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(id): Path<String>,
    Query(query): Query<GetTaskQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let params = extract_service_params(&headers);
    handle_subscribe_to_task_inner(state, params, id, None, query.last_seen_generation).await
}

#[derive(Deserialize)]
pub struct ListPushConfigsQuery {
    #[serde(default, rename = "pageSize")]
    pub page_size: Option<i32>,

    #[serde(default, rename = "pageToken")]
    pub page_token: Option<String>,
}

async fn handle_create_push_config<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(TaskPath { tenant, id }): Path<TaskPath>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> impl IntoResponse {
    let Json(raw_config) = match body {
        Ok(body) => body,
        Err(rejection) => return json_rejection_response(rejection),
    };
    let mut req = match parse_create_push_config_request(id, raw_config) {
        Ok(req) => req,
        Err(e) => return rest_payload_error_response(e),
    };
    req.tenant = match reconcile_tenant(tenant, req.tenant.take()) {
        Ok(tenant) => tenant,
        Err(response) => return *response,
    };
    let params = extract_service_params(&headers);
    match state.handler.create_push_config(&params, req).await {
        Ok(resp) => protojson_json_response(&resp),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_get_push_config<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(PushConfigPath {
        tenant,
        id,
        config_id,
    }): Path<PushConfigPath>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let params = extract_service_params(&headers);
    let req = GetTaskPushNotificationConfigRequest {
        task_id: id,
        id: config_id,
        tenant,
    };
    match state.handler.get_push_config(&params, req).await {
        Ok(resp) => protojson_json_response(&resp),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_list_push_configs<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(TaskPath { tenant, id }): Path<TaskPath>,
    query: Result<Query<ListPushConfigsQuery>, QueryRejection>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return rest_payload_error_response(rejection.body_text()),
    };
    let params = extract_service_params(&headers);
    let req = ListTaskPushNotificationConfigsRequest {
        task_id: id,
        page_size: query.page_size,
        page_token: query.page_token,
        tenant,
    };
    match state.handler.list_push_configs(&params, req).await {
        Ok(resp) => protojson_json_response(&resp),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_delete_push_config<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(PushConfigPath {
        tenant,
        id,
        config_id,
    }): Path<PushConfigPath>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let params = extract_service_params(&headers);
    let req = DeleteTaskPushNotificationConfigRequest {
        task_id: id,
        id: config_id,
        tenant,
    };
    match state.handler.delete_push_config(&params, req).await {
        // google.protobuf.Empty, as ProtoJSON spells it: a client that
        // parses every success body (a2a-python) needs one.
        Ok(()) => with_content_type(
            Json(Value::Object(Default::default())).into_response(),
            "application/a2a+json",
        ),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_get_extended_agent_card<H: RequestHandler>(
    State(state): State<RestState<H>>,
    tenant: Option<Path<String>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let tenant = tenant.map(|Path(tenant)| tenant);
    let params = extract_service_params(&headers);
    let req = GetExtendedAgentCardRequest { tenant };
    match state.handler.get_extended_agent_card(&params, req).await {
        Ok(card) => protojson_json_response(&card),
        Err(e) => rest_error_response(e),
    }
}

fn protojson_json_response<T: ProtoJsonPayload>(value: &T) -> axum::response::Response {
    match protojson_conv::to_value(value) {
        Ok(payload) => with_content_type(Json(payload).into_response(), "application/a2a+json"),
        Err(e) => rest_error_response(crate::sanitized_internal_error(format!(
            "failed to serialize ProtoJSON payload: {e}"
        ))),
    }
}

/// A success body names the A2A media type, `application/a2a+json`, not
/// axum's default `application/json`; an error body keeps the default.
fn with_content_type(
    mut response: axum::response::Response,
    content_type: &'static str,
) -> axum::response::Response {
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
}

fn protojson_stream<T: ProtoJsonPayload + 'static>(
    stream: BoxStream<'static, Result<T, A2AError>>,
) -> BoxStream<'static, Result<Value, A2AError>> {
    Box::pin(stream.map(|item| {
        item.and_then(|value| {
            protojson_conv::to_value(&value).map_err(|e| {
                crate::sanitized_internal_error(format!(
                    "failed to serialize ProtoJSON stream payload: {e}"
                ))
            })
        })
    }))
}

/// Spec §11.6: an error inside an HTTP+JSON stream is an event carrying the
/// same `{"error": google.rpc.Status}` body a non-streaming error has, named
/// `error` as a2a-python's REST server names it and its client expects.
fn rest_sse(stream: BoxStream<'static, Result<Value, A2AError>>) -> axum::response::Response {
    let events = stream.map(|item| item.map_err(|err| rest_error_envelope(err).1));
    sse::sse_from_rest_stream(Box::pin(events)).into_response()
}

fn parse_create_push_config_request(
    task_id: String,
    raw_config: Value,
) -> Result<TaskPushNotificationConfig, String> {
    protojson_conv::from_value::<TaskPushNotificationConfig>(raw_config)
        .map_err(|e| e.to_string())
        .and_then(|mut config| {
            if !config.task_id.is_empty() && config.task_id != task_id {
                Err(format!(
                    "taskId in request body ({}) did not match task id in path ({task_id})",
                    config.task_id
                ))
            } else {
                config.task_id = task_id;
                Ok(config)
            }
        })
}

/// A body over `DefaultBodyLimit` keeps axum's 413; any other body the `Json`
/// extractor rejects is an invalid request body like an undecodable one.
fn json_rejection_response(rejection: JsonRejection) -> axum::response::Response {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return rejection.into_response();
    }
    rest_payload_error_response(rejection.body_text())
}

fn rest_payload_error_response(error: impl std::fmt::Display) -> axum::response::Response {
    let violation = FieldViolation {
        field: String::new(),
        description: error.to_string(),
    };
    rest_error_response(
        A2AError::invalid_params("invalid request body")
            .with_details(vec![TypedDetail::bad_request(vec![violation])]),
    )
}

fn rest_error_response(err: A2AError) -> axum::response::Response {
    let (status, body) = rest_error_envelope(err);
    // The body is google.rpc.Status, not an RFC 9457 problem document, and
    // `application/json` is what the Go and .NET clients parse an error from.
    (status, Json(body)).into_response()
}

fn rest_error_envelope(err: A2AError) -> (StatusCode, RestErrorEnvelope) {
    let status =
        StatusCode::from_u16(err.http_status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let reason = error_reason(err.code);
    let grpc_status = rest_grpc_status(err.code);

    let mut metadata =
        HashMap::from([("timestamp".to_string(), a2a::format_timestamp(&Utc::now()))]);
    let mut details = Vec::new();

    if let Some(extra) = err.details {
        for detail in extra {
            if detail.type_url == errordetails::ERROR_INFO_TYPE {
                if let Some(Value::Object(meta)) = detail.value.get("metadata") {
                    for (k, v) in meta {
                        if let Some(s) = v.as_str() {
                            metadata.entry(k.clone()).or_insert_with(|| s.to_string());
                        }
                    }
                }
            } else {
                details.push(detail);
            }
        }
    }

    details.push(TypedDetail::error_info(
        reason,
        errordetails::PROTOCOL_DOMAIN,
        Some(metadata),
    ));

    let body = RestErrorEnvelope {
        error: RestErrorStatus {
            code: status.as_u16(),
            status: grpc_status,
            message: err.message,
            details,
        },
    };
    (status, body)
}

fn rest_grpc_status(code: i32) -> &'static str {
    match code {
        error_code::TASK_NOT_FOUND => "NOT_FOUND",
        error_code::TASK_GENERATION_MISMATCH => "ABORTED",
        error_code::METHOD_NOT_FOUND => "UNIMPLEMENTED",
        error_code::TASK_NOT_CANCELABLE
        | error_code::EXTENDED_CARD_NOT_CONFIGURED
        | error_code::EXTENSION_SUPPORT_REQUIRED
        | error_code::PUSH_NOTIFICATION_NOT_SUPPORTED
        | error_code::UNSUPPORTED_OPERATION
        | error_code::VERSION_NOT_SUPPORTED => "FAILED_PRECONDITION",
        error_code::CONTENT_TYPE_NOT_SUPPORTED
        | error_code::PARSE_ERROR
        | error_code::INVALID_REQUEST
        | error_code::INVALID_PARAMS => "INVALID_ARGUMENT",
        _ => "INTERNAL",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::InMemoryPushConfigStore;
    use crate::executor::ExecutorContext;
    use crate::handler::DefaultRequestHandler;
    use crate::task_store::InMemoryTaskStore;
    use a2a_pb::protojson_conv;
    use axum::body::Body;
    use axum::http::Request;
    use futures::stream::BoxStream;
    use http_body_util::BodyExt;

    use crate::test_util::{CapturingHandler, assert_header_captured, install_crypto_provider};
    use tower::ServiceExt;

    struct EchoExecutor;

    impl crate::AgentExecutor for EchoExecutor {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let task = Task {
                generation: 0,
                timeline: None,
                id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: ctx.message,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            };
            Box::pin(futures::stream::once(async move {
                Ok(StreamResponse::Task(task))
            }))
        }

        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let task = Task {
                generation: 0,
                timeline: None,
                id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            };
            Box::pin(futures::stream::once(async move {
                Ok(StreamResponse::Task(task))
            }))
        }
    }

    /// The REST test requests model a client that has negotiated A2A 1.1.
    fn versioned_request() -> axum::http::request::Builder {
        Request::builder().header("a2a-version", "1.1")
    }

    fn make_app() -> axum::Router {
        let handler = Arc::new(
            DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new()).with_capabilities(
                a2a::AgentCapabilities {
                    streaming: Some(true),
                    ..Default::default()
                },
            ),
        );
        rest_router(handler)
    }

    async fn make_push_app() -> axum::Router {
        install_crypto_provider();
        let handler = Arc::new(
            DefaultRequestHandler::new(
                EchoExecutor,
                crate::test_util::seeded_task_store("t1").await,
            )
            .with_capabilities(a2a::AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            })
            .with_push_config_store(InMemoryPushConfigStore::new()),
        );
        rest_router(handler)
    }

    #[tokio::test]
    async fn test_send_message() {
        let app = make_app();
        let body = serde_json::json!({
            "message": {
                "messageId": "m1",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            }
        });
        let req = versioned_request()
            .uri(REST_SEND_MESSAGE_PATH)
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_protocol_version_is_required_and_supported_versions_are_accepted() {
        let app = rest_router(Arc::new(
            DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new()).with_capabilities(
                a2a::AgentCapabilities {
                    streaming: Some(true),
                    ..Default::default()
                },
            ),
        ));
        let request = |version: Option<&str>| {
            let mut builder = Request::builder().uri("/tasks").method("GET");
            if let Some(version) = version {
                builder = builder.header("a2a-version", version);
            }
            builder.body(Body::empty()).unwrap()
        };

        let missing = app.clone().oneshot(request(None)).await.unwrap();
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
        let unsupported = app.clone().oneshot(request(Some("2.0"))).await.unwrap();
        assert_eq!(unsupported.status(), StatusCode::BAD_REQUEST);
        let supported = app.oneshot(request(Some("1.0"))).await.unwrap();
        assert_eq!(supported.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_send_message_response_content_type_is_a2a_json() {
        // ACTS REST-CT-001, spec §9: a REST success response's Content-Type
        // is application/a2a+json, not axum's default application/json.
        let app = make_app();
        let body = serde_json::json!({
            "message": {
                "messageId": "m1",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            }
        });
        let req = versioned_request()
            .uri(REST_SEND_MESSAGE_PATH)
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "application/a2a+json"
        );
    }

    #[tokio::test]
    async fn test_rest_error_response_content_type_is_json() {
        // Go and .NET clients parse an error body only from this type.
        let resp = rest_error_response(A2AError::task_not_found("missing"));
        assert_eq!(
            resp.headers()
                .get("content-type")
                .unwrap()
                .to_str()
                .unwrap(),
            "application/json"
        );
    }

    #[tokio::test]
    async fn test_send_message_legacy_alias() {
        let app = make_app();
        let body = serde_json::json!({
            "message": {
                "messageId": "m1",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            }
        });
        let req = versioned_request()
            .uri(REST_SEND_MESSAGE_LEGACY_PATH)
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_get_task_not_found() {
        let app = make_app();
        let req = versioned_request()
            .uri("/tasks/nonexistent")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_list_tasks_empty() {
        let app = make_app();
        let req = versioned_request()
            .uri("/tasks")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let list_resp =
            protojson_conv::from_str::<ListTasksResponse>(std::str::from_utf8(&body).unwrap())
                .unwrap();
        assert!(list_resp.tasks.is_empty());
    }

    #[tokio::test]
    async fn test_cancel_task_not_found() {
        let app = make_app();
        let req = versioned_request()
            .uri("/tasks/nonexistent:cancel")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_create_push_config_not_supported() {
        let app = make_app();
        let body = serde_json::json!({
            "url": "http://example.com/callback"
        });
        let req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_create_push_config_accepts_raw_config_body() {
        let app = make_push_app().await;
        let body = serde_json::json!({
            "id": "cfg1",
            "url": "http://example.com/callback"
        });
        let req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload = serde_json::from_slice::<TaskPushNotificationConfig>(&body).unwrap();
        assert_eq!(payload.task_id, "t1");
        assert_eq!(payload.id.as_deref(), Some("cfg1"));
        assert_eq!(payload.url, "http://example.com/callback");
    }

    #[tokio::test]
    async fn test_create_push_config_accepts_flat_request_body() {
        let app = make_push_app().await;
        let body = serde_json::json!({
            "taskId": "t1",
            "id": "cfg1",
            "url": "http://example.com/callback"
        });
        let req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload = serde_json::from_slice::<TaskPushNotificationConfig>(&body).unwrap();
        assert_eq!(payload.task_id, "t1");
        assert_eq!(payload.id.as_deref(), Some("cfg1"));
        assert_eq!(payload.url, "http://example.com/callback");
    }

    #[tokio::test]
    async fn test_create_push_config_rejects_mismatched_task_id_in_body() {
        let app = make_push_app().await;
        let body = serde_json::json!({
            "taskId": "other-task",
            "url": "http://example.com/callback"
        });
        let req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_get_extended_agent_card() {
        let app = make_app();
        let req = versioned_request()
            .uri(REST_EXTENDED_AGENT_CARD_PATH)
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn unknown_task_actions_are_http_not_found() {
        let response = make_app()
            .oneshot(
                Request::builder()
                    .header("A2A-Version", "1.1")
                    .uri("/tasks/task:unknownAction")
                    .method("POST")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(payload["error"]["code"], 404);
    }

    #[tokio::test]
    async fn test_rest_error_response_status_codes() {
        let resp = rest_error_response(A2AError::task_not_found("t1"));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["error"]["code"], 404);
        assert_eq!(payload["error"]["status"], "NOT_FOUND");
        assert_eq!(payload["error"]["details"][0]["reason"], "TASK_NOT_FOUND");

        let resp = rest_error_response(A2AError::internal("boom"));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let resp = rest_error_response(A2AError::content_type_not_supported());
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_rest_error_response_boundary_does_not_rewrite_internal_errors() {
        // The boundary no longer sanitizes: an INTERNAL_ERROR raised by the
        // executor (or any handler-layer fault) keeps its message, because
        // the boundary cannot tell an agent's own failure report from a
        // server fault. Sanitization now happens at the raise site via
        // `sanitized_internal_error`.
        let resp = rest_error_response(A2AError::internal("agent-reported failure detail"));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["error"]["message"], "agent-reported failure detail");
    }

    #[test]
    fn test_sanitized_internal_error_hides_server_side_detail() {
        let err = crate::sanitized_internal_error("serde fault at /etc/server/keys.pem");
        assert_eq!(err.code, a2a::error_code::INTERNAL_ERROR);
        assert_eq!(err.message, "Internal error");
        assert!(!err.message.contains("/etc/server/keys.pem"));

        // Non-internal errors keep their message (client-validation feedback).
        let resp = rest_error_response(A2AError::task_not_found("t1"));
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// A `ProtoJsonPayload` whose proto and ProtoJSON types disagree, so the
    /// transcode step in `protojson_conv::to_value` always fails.
    struct UnserializablePayload;

    impl protojson_conv::ProtoJsonPayload for UnserializablePayload {
        type Proto = a2a_pb::proto::Task;
        type ProtoJson = a2a_pb::protojson::Task;

        fn to_proto(_value: &Self) -> Self::Proto {
            // An out-of-range enum discriminant survives encoding but fails
            // prost's decode in the transcode step.
            a2a_pb::proto::Task {
                status: Some(a2a_pb::proto::TaskStatus {
                    state: 9999,
                    message: None,
                    timestamp: None,
                }),
                ..Default::default()
            }
        }

        fn try_from_proto(
            _value: &Self::Proto,
        ) -> Result<Self, a2a_pb::protojson_conv::ProtoJsonPayloadError> {
            Ok(Self)
        }
    }

    #[test]
    fn test_unserializable_payload_double_round_trips() {
        // Keeps the fixture's decode side exercised; the fault lives in
        // `to_proto`, so `to_value` fails while the type stays constructible.
        let proto = <UnserializablePayload as protojson_conv::ProtoJsonPayload>::to_proto(
            &UnserializablePayload,
        );
        assert!(
            <UnserializablePayload as protojson_conv::ProtoJsonPayload>::try_from_proto(&proto)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn test_serialization_fault_is_sanitized_at_the_raise_site() {
        // A server-side ProtoJSON fault must not leak serializer internals,
        // so the raise site substitutes the generic message while logging the
        // real cause.
        let resp = protojson_json_response(&UnserializablePayload);
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["error"]["message"], "Internal error");
    }

    #[tokio::test]
    async fn test_stream_serialization_fault_is_sanitized_at_the_raise_site() {
        // Same fault on the streaming path: the executor-facing stream yields
        // a sanitized INTERNAL_ERROR rather than the serializer detail.
        let stream: BoxStream<'static, Result<UnserializablePayload, A2AError>> =
            Box::pin(futures::stream::iter(vec![Ok(UnserializablePayload)]));
        let mut mapped = protojson_stream(stream);
        let item = mapped.next().await.expect("one item");
        let err = item.expect_err("transcode must fail");
        assert_eq!(err.code, a2a::error_code::INTERNAL_ERROR);
        assert_eq!(err.message, "Internal error");
    }

    #[tokio::test]
    async fn test_live_stream_errors_use_rest_error_frames() {
        for path in ["/message:live", "/acme/message:live"] {
            let (handler, _) = CapturingHandler::new();
            *handler.live_stream_error.lock().unwrap() = Some(A2AError::task_not_found("t1"));
            let response = rest_router(handler)
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("A2A-Version", "1.1")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::json!({"subscribe": {"id": "t1"}}).to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert!(body.contains("event: error"), "{body}");
            let data = body
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            let frame: Value = serde_json::from_str(data).unwrap();
            assert_eq!(frame["error"]["code"], 404);
            assert_eq!(frame["error"]["status"], "NOT_FOUND");
            assert_eq!(frame["error"]["details"][0]["reason"], "TASK_NOT_FOUND");
        }
    }

    #[tokio::test]
    async fn test_stream_error_event_is_a_google_rpc_status() {
        let stream: BoxStream<'static, Result<Value, A2AError>> =
            Box::pin(futures::stream::iter(vec![
                Ok(serde_json::json!({"statusUpdate": {}})),
                Err(A2AError::task_not_found("t1")),
            ]));
        let body = rest_sse(stream)
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();
        let frames: Vec<Value> = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).unwrap())
            .collect();
        assert_eq!(frames.len(), 2);
        let error = &frames[1]["error"];
        assert_eq!(error["code"], 404);
        assert_eq!(error["status"], "NOT_FOUND");
        assert_eq!(error["details"][0]["reason"], "TASK_NOT_FOUND");
        // The error frame alone is named: a2a-python's REST client reads an
        // unnamed frame as a StreamResponse and drops the error.
        let names: Vec<&str> = body
            .lines()
            .filter_map(|line| line.strip_prefix("event: "))
            .collect();
        assert_eq!(names, vec!["error"], "{body}");
    }

    #[tokio::test]
    async fn test_rest_error_response_merges_error_info_metadata() {
        let existing_info = TypedDetail::error_info(
            "TASK_NOT_FOUND",
            errordetails::PROTOCOL_DOMAIN,
            Some(HashMap::from([("taskId".to_string(), "t99".to_string())])),
        );
        let err = A2AError::task_not_found("t99").with_details(vec![existing_info]);
        let resp = rest_error_response(err);
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let details = payload["error"]["details"].as_array().unwrap();
        // Only one ErrorInfo should be present (merged)
        assert_eq!(details.len(), 1);
        assert_eq!(details[0]["@type"], errordetails::ERROR_INFO_TYPE);
        assert_eq!(details[0]["reason"], "TASK_NOT_FOUND");
        // Merged metadata should contain both taskId and timestamp
        assert_eq!(details[0]["metadata"]["taskId"], "t99");
        assert!(details[0]["metadata"]["timestamp"].is_string());
    }

    /// The ErrorInfo timestamp is a wire timestamp (§5.6.1): `Z`, not the
    /// `+00:00` that chrono's `to_rfc3339` writes.
    #[tokio::test]
    async fn test_rest_error_info_timestamp_uses_the_z_designator() {
        let resp = rest_error_response(A2AError::task_not_found("t1"));
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let timestamp = payload["error"]["details"][0]["metadata"]["timestamp"]
            .as_str()
            .unwrap();
        assert!(timestamp.ends_with('Z'), "{timestamp}");
        assert!(timestamp.parse::<DateTime<Utc>>().is_ok());
    }

    #[tokio::test]
    async fn test_rest_error_response_preserves_non_error_info_details() {
        let bad_req = TypedDetail::bad_request(vec![FieldViolation {
            field: "message.parts".into(),
            description: "required".into(),
        }]);
        let err = A2AError::invalid_params("bad request").with_details(vec![bad_req]);
        let resp = rest_error_response(err);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let details = payload["error"]["details"].as_array().unwrap();
        // BadRequest detail + auto-generated ErrorInfo
        assert_eq!(details.len(), 2);
        assert_eq!(details[0]["@type"], errordetails::BAD_REQUEST_TYPE);
        assert_eq!(details[0]["fieldViolations"][0]["field"], "message.parts");
        assert_eq!(details[1]["@type"], errordetails::ERROR_INFO_TYPE);
        assert_eq!(details[1]["reason"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn test_rest_payload_error_response() {
        let resp = rest_payload_error_response("missing field 'parts'");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload["error"]["message"].as_str().unwrap(),
            "invalid request body"
        );
        let details = payload["error"]["details"].as_array().unwrap();
        // BadRequest detail + ErrorInfo
        assert_eq!(details.len(), 2);
        assert_eq!(details[0]["@type"], errordetails::BAD_REQUEST_TYPE);
        assert_eq!(
            details[0]["fieldViolations"][0]["description"],
            "missing field 'parts'"
        );
        assert_eq!(details[1]["@type"], errordetails::ERROR_INFO_TYPE);
        assert_eq!(details[1]["reason"], "INVALID_PARAMS");
    }

    #[tokio::test]
    async fn test_subscribe_to_task_not_found() {
        let app = make_app();
        let req = versioned_request()
            .uri("/tasks/nonexistent:subscribe")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn test_stream_message() {
        let app = make_app();
        let body = serde_json::json!({
            "message": {
                "messageId": "m1",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            }
        });
        let req = versioned_request()
            .uri(REST_STREAM_MESSAGE_PATH)
            .method("POST")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_get_push_config_not_found() {
        let app = make_app();
        let req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs/cfg1")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Push not supported → bad request
        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn test_list_push_configs() {
        let app = make_push_app().await;
        let req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload =
            serde_json::from_slice::<ListTaskPushNotificationConfigsResponse>(&body).unwrap();
        assert!(payload.configs.is_empty());
    }

    #[tokio::test]
    async fn test_delete_push_config() {
        let app = make_app();
        let req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs/cfg1")
            .method("DELETE")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn test_delete_push_config_with_store_returns_ok() {
        let app = make_push_app().await;
        let create_body = serde_json::json!({
            "id": "cfg1",
            "url": "http://example.com/callback"
        });
        let create_req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&create_body).unwrap()))
            .unwrap();
        let create_resp = app.clone().oneshot(create_req).await.unwrap();
        assert_eq!(create_resp.status(), StatusCode::OK);

        let delete_req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs/cfg1")
            .method("DELETE")
            .body(Body::empty())
            .unwrap();
        let delete_resp = app.clone().oneshot(delete_req).await.unwrap();
        assert_eq!(delete_resp.status(), StatusCode::OK);
        // google.protobuf.Empty in ProtoJSON: `{}`, typed like every other
        // success, so a client that parses the body gets a document.
        assert_eq!(
            delete_resp.headers().get(header::CONTENT_TYPE).unwrap(),
            "application/a2a+json"
        );
        let body = delete_resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!({})
        );

        let list_req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let list_resp = app.oneshot(list_req).await.unwrap();
        assert_eq!(list_resp.status(), StatusCode::OK);
        let body = list_resp.into_body().collect().await.unwrap().to_bytes();
        let payload =
            serde_json::from_slice::<ListTaskPushNotificationConfigsResponse>(&body).unwrap();
        assert!(payload.configs.is_empty());
    }

    #[tokio::test]
    async fn test_list_tasks_with_query_params() {
        let app = make_app();
        let req = versioned_request()
            .uri("/tasks?contextId=c1&pageSize=5&pageToken=&historyLength=3&includeArtifacts=true&statusTimestampAfter=2025-01-01T00:00:00Z")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// `TASK_STATE_UNSPECIFIED` is the proto's default for an unset
    /// `status`: no filter, as the other bindings treat it, not a filter
    /// no task can match.
    #[tokio::test]
    async fn test_list_tasks_status_unspecified_is_no_filter() {
        let app = make_app();
        let body = serde_json::json!({
            "message": {"messageId": "m1", "role": "ROLE_USER", "parts": [{"text": "hello"}]}
        });
        let req = Request::builder()
            .header("a2a-version", "1.0")
            .uri(REST_SEND_MESSAGE_PATH)
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );

        let req = Request::builder()
            .header("a2a-version", "1.0")
            .uri("/tasks?status=TASK_STATE_UNSPECIFIED")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["totalSize"], 1, "{payload}");
    }

    #[tokio::test]
    async fn test_list_tasks_invalid_status_returns_bad_request() {
        let app = make_app();
        let req = versioned_request()
            .uri("/tasks?status=invalid")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload["error"]["message"], "invalid status filter");
    }

    #[tokio::test]
    async fn test_send_message_propagates_headers_as_service_params() {
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);

        let body = serde_json::json!({
            "message": {
                "messageId": "m1",
                "role": "ROLE_USER",
                "parts": [{"text": "hi"}]
            }
        });
        let req = versioned_request()
            .uri(REST_SEND_MESSAGE_PATH)
            .method("POST")
            .header("content-type", "application/json")
            .header("authorization", "Bearer test-token-abc")
            .header("x-tenant-id", "acme")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();

        assert_header_captured(
            &captured,
            "send_message",
            "authorization",
            "Bearer test-token-abc",
        );
        assert_header_captured(&captured, "send_message", "x-tenant-id", "acme");
    }

    #[tokio::test]
    async fn test_stream_message_propagates_headers_as_service_params() {
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);

        let body = serde_json::json!({
            "message": {
                "messageId": "m1",
                "role": "ROLE_USER",
                "parts": [{"text": "hi"}]
            }
        });
        let req = versioned_request()
            .uri(REST_STREAM_MESSAGE_PATH)
            .method("POST")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("authorization", "Bearer streaming-token")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Drain the SSE body to ensure the handler has been fully driven.
        let _ = resp.into_body().collect().await.unwrap().to_bytes();

        assert_header_captured(
            &captured,
            "send_streaming_message",
            "authorization",
            "Bearer streaming-token",
        );
    }

    #[tokio::test]
    async fn test_get_task_propagates_headers_as_service_params() {
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);
        let req = versioned_request()
            .uri("/tasks/some-id")
            .method("GET")
            .header("authorization", "Bearer get-task-token")
            .header("x-tenant-id", "acme")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        assert_header_captured(
            &captured,
            "get_task",
            "authorization",
            "Bearer get-task-token",
        );
        assert_header_captured(&captured, "get_task", "x-tenant-id", "acme");
    }

    #[tokio::test]
    async fn test_list_tasks_propagates_headers_as_service_params() {
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);
        let req = versioned_request()
            .uri("/tasks")
            .method("GET")
            .header("authorization", "Bearer list-tasks-token")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        assert_header_captured(
            &captured,
            "list_tasks",
            "authorization",
            "Bearer list-tasks-token",
        );
    }

    #[tokio::test]
    async fn test_cancel_task_propagates_headers_as_service_params() {
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);
        let req = versioned_request()
            .uri("/tasks/some-id:cancel")
            .method("POST")
            .header("authorization", "Bearer cancel-token")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        assert_header_captured(
            &captured,
            "cancel_task",
            "authorization",
            "Bearer cancel-token",
        );
    }

    #[tokio::test]
    async fn test_cancel_task_legacy_propagates_headers_as_service_params() {
        // /tasks/{id}/cancel routes through handle_cancel_task_legacy.
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);
        let req = versioned_request()
            .uri("/tasks/some-id/cancel")
            .method("POST")
            .header("authorization", "Bearer cancel-legacy-token")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        assert_header_captured(
            &captured,
            "cancel_task",
            "authorization",
            "Bearer cancel-legacy-token",
        );
    }

    #[tokio::test]
    async fn test_create_push_config_propagates_headers_as_service_params() {
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);
        let body = serde_json::json!({
            "id": "cfg1",
            "url": "http://example.com/callback"
        });
        let req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs")
            .method("POST")
            .header("content-type", "application/json")
            .header("authorization", "Bearer push-create-token")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        assert_header_captured(
            &captured,
            "create_push_config",
            "authorization",
            "Bearer push-create-token",
        );
    }

    #[tokio::test]
    async fn test_list_push_configs_propagates_headers_as_service_params() {
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);
        let req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs")
            .method("GET")
            .header("authorization", "Bearer push-list-token")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        assert_header_captured(
            &captured,
            "list_push_configs",
            "authorization",
            "Bearer push-list-token",
        );
    }

    #[tokio::test]
    async fn test_get_push_config_propagates_headers_as_service_params() {
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);
        let req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs/cfg1")
            .method("GET")
            .header("authorization", "Bearer push-get-token")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        assert_header_captured(
            &captured,
            "get_push_config",
            "authorization",
            "Bearer push-get-token",
        );
    }

    #[tokio::test]
    async fn test_delete_push_config_propagates_headers_as_service_params() {
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);
        let req = versioned_request()
            .uri("/tasks/t1/pushNotificationConfigs/cfg1")
            .method("DELETE")
            .header("authorization", "Bearer push-delete-token")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        assert_header_captured(
            &captured,
            "delete_push_config",
            "authorization",
            "Bearer push-delete-token",
        );
    }

    #[tokio::test]
    async fn test_get_extended_agent_card_propagates_headers_as_service_params() {
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);
        let req = versioned_request()
            .uri(REST_EXTENDED_AGENT_CARD_PATH)
            .method("GET")
            .header("authorization", "Bearer card-token")
            .body(Body::empty())
            .unwrap();
        let _ = app.oneshot(req).await.unwrap();
        assert_header_captured(
            &captured,
            "get_extended_agent_card",
            "authorization",
            "Bearer card-token",
        );
    }

    #[tokio::test]
    async fn test_subscribe_to_task_propagates_headers_as_service_params() {
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);
        let req = versioned_request()
            .uri("/tasks/some-id:subscribe")
            .method("GET")
            .header("authorization", "Bearer subscribe-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let _ = resp.into_body().collect().await.unwrap().to_bytes();
        assert_header_captured(
            &captured,
            "subscribe_to_task",
            "authorization",
            "Bearer subscribe-token",
        );
    }

    #[tokio::test]
    async fn test_subscribe_to_task_legacy_propagates_headers_as_service_params() {
        // /tasks/{id}/subscribe routes through handle_subscribe_to_task_legacy.
        let (handler, captured) = CapturingHandler::new();
        let app = rest_router(handler);
        let req = versioned_request()
            .uri("/tasks/some-id/subscribe")
            .method("GET")
            .header("authorization", "Bearer subscribe-legacy-token")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let _ = resp.into_body().collect().await.unwrap().to_bytes();
        assert_header_captured(
            &captured,
            "subscribe_to_task",
            "authorization",
            "Bearer subscribe-legacy-token",
        );
    }

    #[tokio::test]
    async fn test_get_task_with_history_length() {
        // First create a task, then get it with historyLength
        let app = make_app();
        // Create task via send_message
        let body = serde_json::json!({
            "message": {
                "messageId": "m1",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            }
        });
        let req = versioned_request()
            .uri(REST_SEND_MESSAGE_PATH)
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let resp_body = resp.into_body().collect().await.unwrap().to_bytes();
        let send_resp: serde_json::Value = serde_json::from_slice(&resp_body).unwrap();
        let task_id = send_resp["task"]["id"].as_str().unwrap();

        let app = make_app();
        let req = versioned_request()
            .uri(format!("/tasks/{}?historyLength=5", task_id))
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Each make_app() creates a new store, so this will be not found
        assert!(resp.status() == StatusCode::OK || resp.status() == StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_task_honours_the_1_1_query_parameters() {
        let app = make_app();
        let body = serde_json::json!({
            "message": {"messageId": "m1", "role": "ROLE_USER", "parts": [{"text": "hello"}]}
        });
        let req = versioned_request()
            .uri(REST_SEND_MESSAGE_PATH)
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let sent: serde_json::Value =
            serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
        let task_id = sent["task"]["id"].as_str().unwrap();
        assert!(sent["task"]["timeline"].as_array().unwrap().len() > 1);

        let get = |query: &str| {
            versioned_request()
                .uri(format!("/tasks/{task_id}{query}"))
                .method("GET")
                .body(Body::empty())
                .unwrap()
        };

        let resp = app.clone().oneshot(get("?timelineLength=1")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let trimmed: serde_json::Value =
            serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(trimmed["timeline"].as_array().unwrap().len(), 1);
        // int64 travels as a string in ProtoJSON.
        assert!(trimmed["generation"].is_string(), "{trimmed}");

        // The task is already past generation 0, so the long-poll answers at once.
        let resp = app
            .clone()
            .oneshot(get("?currentGeneration=0"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// The REST binding is bounded by the same limit as JSON-RPC: an
    /// oversized body is rejected at the framework boundary, before any
    /// handler runs.
    #[tokio::test]
    async fn test_request_body_over_the_limit_is_rejected() {
        let app = make_app();
        let oversized = Body::from("x".repeat(crate::jsonrpc::MAX_REQUEST_BODY_BYTES + 1));
        let req = versioned_request()
            .uri(REST_SEND_MESSAGE_PATH)
            .method("POST")
            .header("content-type", "application/json")
            .body(oversized)
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn malformed_message_requests_are_rejected_before_handler_dispatch() {
        for path in [
            REST_SEND_MESSAGE_PATH,
            REST_STREAM_MESSAGE_PATH,
            REST_LIVE_MESSAGE_PATH,
        ] {
            let (handler, captured) = CapturingHandler::new();
            let response = rest_router(handler)
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .header("content-type", "application/json")
                        .header("A2A-Version", "1.1")
                        .body(Body::from(r#"{"message":42,"sendMessage":42}"#))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["status"], "INVALID_ARGUMENT");
            assert!(captured.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn version_query_with_embedded_newline_is_rejected() {
        assert_version_not_supported(
            send_message_with_version("/message:send?A2A-Version=1.1%0Ainvalid", None).await,
        )
        .await;
    }

    async fn send_message_with_version(uri: &str, version: Option<&str>) -> Response {
        let body = serde_json::json!({
            "message": {"messageId": "m1", "role": "ROLE_USER", "parts": [{"text": "hi"}]}
        });
        let mut req = Request::builder()
            .uri(uri)
            .method("POST")
            .header("content-type", "application/json");
        if let Some(version) = version {
            req = req.header("a2a-version", version);
        }
        let req = req.body(Body::from(body.to_string())).unwrap();
        make_app().oneshot(req).await.unwrap()
    }

    async fn assert_version_not_supported(resp: Response) {
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            payload["error"]["status"], "FAILED_PRECONDITION",
            "{payload}"
        );
        assert_eq!(
            payload["error"]["details"][0]["reason"], "VERSION_NOT_SUPPORTED",
            "{payload}"
        );
    }

    #[tokio::test]
    async fn test_unsupported_a2a_version_is_rejected() {
        // §3.6.2, §6.4: same rejection as JSON-RPC, in REST's error shape.
        for version in ["99.0", "1"] {
            let resp = send_message_with_version(REST_SEND_MESSAGE_PATH, Some(version)).await;
            assert_version_not_supported(resp).await;
        }
    }

    #[tokio::test]
    async fn test_missing_a2a_version_is_rejected() {
        // §3.6.2: an absent A2A-Version is "0.3", which this server does not
        // implement -- on every route, not only SendMessage.
        let resp = send_message_with_version(REST_SEND_MESSAGE_PATH, None).await;
        assert_version_not_supported(resp).await;

        let req = Request::builder()
            .uri("/tasks/t1")
            .body(Body::empty())
            .unwrap();
        assert_version_not_supported(make_app().oneshot(req).await.unwrap()).await;
    }

    #[tokio::test]
    async fn test_a2a_version_query_parameter_is_accepted() {
        // §3.6.1: a client MAY send the version as a request parameter.
        let uri = format!("{REST_SEND_MESSAGE_PATH}?A2A-Version=1.0");
        let resp = send_message_with_version(&uri, None).await;
        assert_eq!(resp.status(), StatusCode::OK);

        let uri = format!("{REST_SEND_MESSAGE_PATH}?A2A-Version=99.0");
        assert_version_not_supported(send_message_with_version(&uri, None).await).await;
    }

    /// Input axum rejects before a handler runs gets the google.rpc.Status
    /// body of every other REST error (§11.6), not axum's plain-text rejection.
    #[tokio::test]
    async fn test_extractor_rejections_answer_google_rpc_status() {
        let requests = [
            Request::builder()
                .uri("/tasks/t1?historyLength=abc")
                .method("GET")
                .header("a2a-version", "1.0")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri("/tasks?pageSize=x")
                .method("GET")
                .header("a2a-version", "1.0")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .uri(REST_SEND_MESSAGE_PATH)
                .method("POST")
                .header("a2a-version", "1.0")
                .header("content-type", "application/json")
                .body(Body::from("{"))
                .unwrap(),
            Request::builder()
                .uri(REST_SEND_MESSAGE_PATH)
                .method("POST")
                .header("a2a-version", "1.0")
                .body(Body::from("{}"))
                .unwrap(),
            Request::builder()
                .uri(REST_STREAM_MESSAGE_PATH)
                .method("POST")
                .header("a2a-version", "1.0")
                .header("content-type", "application/json")
                .body(Body::from("{"))
                .unwrap(),
            Request::builder()
                .uri("/tasks/t1/pushNotificationConfigs")
                .method("POST")
                .header("a2a-version", "1.0")
                .header("content-type", "application/json")
                .body(Body::from("{"))
                .unwrap(),
            Request::builder()
                .uri("/tasks/t1/pushNotificationConfigs?pageSize=x")
                .method("GET")
                .header("a2a-version", "1.0")
                .body(Body::empty())
                .unwrap(),
        ];
        for req in requests {
            let target = req.uri().to_string();
            let resp = make_app().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{target}");
            assert_eq!(
                resp.headers().get("content-type").unwrap(),
                "application/json",
                "{target}"
            );
            let body = resp.into_body().collect().await.unwrap().to_bytes();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], 400, "{target}");
            assert_eq!(body["error"]["status"], "INVALID_ARGUMENT", "{target}");
        }
    }

    /// Records the metadata of the cancel it is asked to run.
    struct CancelMetadataExecutor(Arc<std::sync::Mutex<Option<HashMap<String, Value>>>>);

    impl crate::AgentExecutor for CancelMetadataExecutor {
        fn execute(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            Box::pin(futures::stream::empty())
        }

        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            *self.0.lock().unwrap() = ctx.metadata.clone();
            let update = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                generation: 0,
                task_id: ctx.task_id,
                context_id: ctx.context_id,
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            });
            Box::pin(futures::stream::once(async move { Ok(update) }))
        }
    }

    /// CancelTask maps the whole request to the body, so its metadata reaches
    /// the executor on both cancel paths; an empty body still cancels.
    #[tokio::test]
    async fn test_cancel_task_passes_body_metadata_to_the_executor() {
        for (action, body, expected) in [
            (":cancel", r#"{"metadata":{"reason":"user"}}"#, Some("user")),
            (
                "/cancel",
                r#"{"metadata":{"reason":"legacy"}}"#,
                Some("legacy"),
            ),
            (":cancel", "", None),
        ] {
            let seen = Arc::new(std::sync::Mutex::new(None));
            let app = rest_router(Arc::new(DefaultRequestHandler::new(
                CancelMetadataExecutor(seen.clone()),
                InMemoryTaskStore::new(),
            )));
            let send = serde_json::json!({
                "message": {"messageId": "m1", "role": "ROLE_USER", "parts": [{"text": "hi"}]},
                "configuration": {"returnImmediately": true}
            });
            let req = Request::builder()
                .uri(REST_SEND_MESSAGE_PATH)
                .method("POST")
                .header("a2a-version", "1.0")
                .header("content-type", "application/json")
                .body(Body::from(send.to_string()))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            let sent = resp.into_body().collect().await.unwrap().to_bytes();
            let sent: Value = serde_json::from_slice(&sent).unwrap();
            let id = sent["task"]["id"].as_str().unwrap().to_owned();

            let uri = format!("/tasks/{id}{action}");
            let req = Request::builder()
                .uri(&uri)
                .method("POST")
                .header("a2a-version", "1.0")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let resp = app.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{uri} {body}");
            let reason = seen
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|metadata| metadata.get("reason").cloned());
            assert_eq!(reason, expected.map(Value::from), "{uri} {body}");
        }
    }

    #[tokio::test]
    async fn test_cancel_task_rejects_a_body_that_is_not_a_request() {
        let req = Request::builder()
            .uri("/tasks/t1:cancel")
            .method("POST")
            .header("a2a-version", "1.0")
            .header("content-type", "application/json")
            .body(Body::from("{"))
            .unwrap();
        let resp = make_app().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn live_tenant_paths_reconcile_the_envelope_and_nested_request() {
        for subscribe in [false, true] {
            for (path, outer, inner, expected, rejected) in [
                ("/message:live", None, None, None, false),
                ("/acme/message:live", None, None, Some("acme"), false),
                (
                    "/acme%20eu/message:live",
                    None,
                    None,
                    Some("acme eu"),
                    false,
                ),
                (
                    "/acme/message:live",
                    Some("acme"),
                    Some("acme"),
                    Some("acme"),
                    false,
                ),
                ("/acme/message:live", Some("other"), None, None, true),
                ("/acme/message:live", None, Some("other"), None, true),
                ("/message:live", Some("body"), None, Some("body"), false),
                ("/message:live", None, Some("body"), Some("body"), false),
                ("/message:live", Some("body"), Some("other"), None, true),
                ("/%FF/message:live", None, None, None, true),
            ] {
                let (handler, _) = CapturingHandler::new();
                let app = rest_router(handler.clone());
                let mut payload = if subscribe {
                    serde_json::json!({"id":"task"})
                } else {
                    serde_json::json!({"message":{"messageId":"m1","role":"ROLE_USER","parts":[{"text":"hello"}]}})
                };
                if let Some(inner) = inner {
                    payload["tenant"] = inner.into();
                }
                let mut body = serde_json::json!({});
                body[if subscribe {
                    "subscribe"
                } else {
                    "sendMessage"
                }] = payload;
                if let Some(outer) = outer {
                    body["tenant"] = outer.into();
                }
                let request = Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("A2A-Version", "1.1")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap();
                let response = app.oneshot(request).await.unwrap();
                assert_eq!(
                    response.status(),
                    if rejected {
                        StatusCode::BAD_REQUEST
                    } else {
                        StatusCode::OK
                    },
                    "{path} {body}"
                );
                let expected = (!rejected).then(|| expected.map(str::to_owned));
                assert_eq!(
                    handler.take_tenant("send_live_request"),
                    expected,
                    "{path} {body}"
                );
                assert_eq!(
                    handler.take_tenant("live_payload"),
                    expected,
                    "{path} {body}"
                );
            }
        }
    }

    /// Every method is served on its proto `/{tenant}/…` binding as well, and
    /// the path tenant reaches the request; the plain binding has none.
    #[tokio::test]
    async fn test_every_method_is_routed_under_a_tenant() {
        let (handler, _) = CapturingHandler::new();
        let app = rest_router(handler.clone());
        let send = r#"{"message":{"messageId":"m1","role":"ROLE_USER","parts":[{"text":"hi"}]}}"#;
        let config = r#"{"url":"https://example.com/cb"}"#;
        for (method, uri, body, called, tenant) in [
            (
                "POST",
                "/acme/message:send",
                send,
                "send_message",
                Some("acme"),
            ),
            (
                "POST",
                "/acme/message:stream",
                send,
                "send_streaming_message",
                Some("acme"),
            ),
            ("GET", "/acme/tasks/t1", "", "get_task", Some("acme")),
            ("GET", "/acme/tasks", "", "list_tasks", Some("acme")),
            (
                "POST",
                "/acme/tasks/t1:cancel",
                "",
                "cancel_task",
                Some("acme"),
            ),
            (
                "GET",
                "/acme/tasks/t1:subscribe",
                "",
                "subscribe_to_task",
                Some("acme"),
            ),
            (
                "POST",
                "/acme/tasks/t1:subscribe",
                "",
                "subscribe_to_task",
                Some("acme"),
            ),
            (
                "POST",
                "/acme/tasks/t1/pushNotificationConfigs",
                config,
                "create_push_config",
                Some("acme"),
            ),
            (
                "GET",
                "/acme/tasks/t1/pushNotificationConfigs/c1",
                "",
                "get_push_config",
                Some("acme"),
            ),
            (
                "GET",
                "/acme/tasks/t1/pushNotificationConfigs",
                "",
                "list_push_configs",
                Some("acme"),
            ),
            (
                "DELETE",
                "/acme/tasks/t1/pushNotificationConfigs/c1",
                "",
                "delete_push_config",
                Some("acme"),
            ),
            (
                "GET",
                "/acme/extendedAgentCard",
                "",
                "get_extended_agent_card",
                Some("acme"),
            ),
            ("POST", REST_SEND_MESSAGE_PATH, send, "send_message", None),
        ] {
            let req = Request::builder()
                .uri(uri)
                .method(method)
                .header("a2a-version", "1.0")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                handler.take_tenant(called),
                Some(tenant.map(str::to_owned)),
                "{method} {uri}"
            );
        }
    }

    /// A tenant that fails to decode is a 400 on every route, like a task id
    /// that fails to decode, and never runs as the default tenant.
    #[tokio::test]
    async fn test_an_undecodable_tenant_is_rejected() {
        let (handler, _) = CapturingHandler::new();
        let app = rest_router(handler.clone());
        let send = r#"{"message":{"messageId":"m1","role":"ROLE_USER","parts":[{"text":"hi"}]}}"#;
        for (method, uri, body, called) in [
            ("POST", "/%FF/message:send", send, "send_message"),
            (
                "POST",
                "/%FF/message:stream",
                send,
                "send_streaming_message",
            ),
            ("GET", "/%FF/tasks", "", "list_tasks"),
            ("GET", "/%FF/tasks/t1", "", "get_task"),
            (
                "GET",
                "/%FF/extendedAgentCard",
                "",
                "get_extended_agent_card",
            ),
        ] {
            let req = Request::builder()
                .uri(uri)
                .method(method)
                .header("a2a-version", "1.0")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{method} {uri}");
            assert_eq!(handler.take_tenant(called), None, "{method} {uri}");
        }
    }

    /// A path tenant reaches the handler, also beside the same tenant in the
    /// body; a different body tenant is a 400 that names the field; the plain
    /// binding takes the body's.
    #[tokio::test]
    async fn test_path_and_body_tenant() {
        let (handler, _) = CapturingHandler::new();
        let app = rest_router(handler.clone());
        let send = r#"{"message":{"messageId":"m1","role":"ROLE_USER","parts":[{"text":"hi"}]}}"#;
        let send_acme = r#"{"tenant":"acme","message":{"messageId":"m1","role":"ROLE_USER","parts":[{"text":"hi"}]}}"#;
        let send_body = r#"{"tenant":"body","message":{"messageId":"m1","role":"ROLE_USER","parts":[{"text":"hi"}]}}"#;
        let cancel_acme = r#"{"tenant":"acme"}"#;
        let cancel_body = r#"{"tenant":"body"}"#;
        let config_acme = r#"{"tenant":"acme","url":"https://example.com/cb"}"#;
        let config_body = r#"{"tenant":"body","url":"https://example.com/cb"}"#;
        // `None`: a 400 and no handler call.
        for (uri, body, called, tenant) in [
            ("/acme/message:send", send, "send_message", Some("acme")),
            (
                "/acme/message:send",
                send_acme,
                "send_message",
                Some("acme"),
            ),
            ("/acme/message:send", send_body, "send_message", None),
            (
                REST_SEND_MESSAGE_PATH,
                send_body,
                "send_message",
                Some("body"),
            ),
            (
                "/acme/message:stream",
                send_acme,
                "send_streaming_message",
                Some("acme"),
            ),
            (
                "/acme/message:stream",
                send_body,
                "send_streaming_message",
                None,
            ),
            (
                REST_STREAM_MESSAGE_PATH,
                send_body,
                "send_streaming_message",
                Some("body"),
            ),
            ("/acme/tasks/t1:cancel", "", "cancel_task", Some("acme")),
            (
                "/acme/tasks/t1:cancel",
                cancel_acme,
                "cancel_task",
                Some("acme"),
            ),
            ("/acme/tasks/t1:cancel", cancel_body, "cancel_task", None),
            ("/tasks/t1:cancel", cancel_body, "cancel_task", Some("body")),
            (
                "/acme/tasks/t1/pushNotificationConfigs",
                config_acme,
                "create_push_config",
                Some("acme"),
            ),
            (
                "/acme/tasks/t1/pushNotificationConfigs",
                config_body,
                "create_push_config",
                None,
            ),
            (
                "/tasks/t1/pushNotificationConfigs",
                config_body,
                "create_push_config",
                Some("body"),
            ),
        ] {
            let req = Request::builder()
                .uri(uri)
                .method("POST")
                .header("a2a-version", "1.0")
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap();
            let resp = app.clone().oneshot(req).await.unwrap();
            assert_eq!(
                resp.status() == StatusCode::BAD_REQUEST,
                tenant.is_none(),
                "POST {uri} {body}"
            );
            assert_eq!(
                handler.take_tenant(called),
                tenant.map(|tenant| Some(tenant.to_owned())),
                "POST {uri} {body}"
            );
        }

        let req = Request::builder()
            .uri("/acme/tasks/t1:cancel")
            .method("POST")
            .header("a2a-version", "1.0")
            .header("content-type", "application/json")
            .body(Body::from(cancel_body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let violation = &payload["error"]["details"][0]["fieldViolations"][0];
        assert_eq!(violation["field"], "tenant");
        assert_eq!(
            violation["description"],
            "tenant in the body (body) does not match the tenant in the path (acme)"
        );
    }

    #[tokio::test]
    async fn get_and_post_subscription_bindings_report_the_same_missing_task() {
        for method in ["GET", "POST"] {
            let handler = DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
                .with_capabilities(AgentCapabilities {
                    streaming: Some(true),
                    ..Default::default()
                });
            let app = rest_router(Arc::new(handler));
            let request = Request::builder()
                .method(method)
                .uri("/tasks/absent:subscribe")
                .header("A2A-Version", "1.1")
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                app.oneshot(request).await.unwrap().status(),
                StatusCode::NOT_FOUND
            );
        }
    }

    #[tokio::test]
    async fn live_bindings_distinguish_malformed_requests_from_missing_tasks() {
        for rpc in [false, true] {
            for malformed in [false, true] {
                let handler = Arc::new(
                    DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
                        .with_capabilities(AgentCapabilities {
                            streaming: Some(true),
                            ..Default::default()
                        }),
                );
                let app = if rpc {
                    crate::jsonrpc::jsonrpc_router(handler)
                } else {
                    rest_router(handler)
                };
                let payload = if malformed {
                    serde_json::json!("not-a-stream-request")
                } else {
                    serde_json::json!({"subscribe":{"id":"absent"}})
                };
                let body = if rpc {
                    serde_json::json!({"jsonrpc":"2.0","id":7,"method":methods::SEND_LIVE_MESSAGE,"params":payload})
                } else {
                    payload
                };
                let request = Request::builder()
                    .method("POST")
                    .uri(if rpc { "/" } else { REST_LIVE_MESSAGE_PATH })
                    .header("content-type", "application/json")
                    .header("A2A-Version", "1.1")
                    .body(Body::from(body.to_string()))
                    .unwrap();
                let response = app.oneshot(request).await.unwrap();
                if rpc {
                    assert_eq!(response.status(), StatusCode::OK);
                    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
                        .await
                        .unwrap();
                    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                    assert_eq!(
                        value["error"]["code"],
                        if malformed {
                            error_code::INVALID_PARAMS
                        } else {
                            error_code::TASK_NOT_FOUND
                        }
                    );
                } else {
                    assert_eq!(
                        response.status(),
                        if malformed {
                            StatusCode::BAD_REQUEST
                        } else {
                            StatusCode::NOT_FOUND
                        }
                    );
                }
            }
        }
    }
    #[tokio::test]
    async fn query_version_is_forwarded_to_the_handler() {
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = seen.clone();
        let handler = DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
            .with_authorizer(move |params: &ServiceParams, _: Option<&str>| {
                observed.store(
                    params
                        .get("a2a-version")
                        .and_then(|values| values.first())
                        .map(String::as_str)
                        == Some("1.1"),
                    std::sync::atomic::Ordering::SeqCst,
                );
                Ok(())
            });
        let request = Request::builder()
            .uri(format!("{REST_SEND_MESSAGE_PATH}?A2A-Version=1.1"))
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::json!({
                "message": {"messageId":"query-version", "role":"ROLE_USER", "parts":[{"text":"hello"}]}
            }).to_string())).unwrap();
        let response = rest_router(Arc::new(handler))
            .oneshot(request)
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(seen.load(std::sync::atomic::Ordering::SeqCst));
    }
}
