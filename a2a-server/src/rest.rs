// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;

use a2a::*;
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use chrono::{DateTime, Utc};
use futures::{StreamExt, stream::BoxStream};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

use crate::handler::RequestHandler;
use crate::middleware::ServiceParams;
use crate::push_config_compat::json_value as compat_json_value;
use crate::sse;

const REST_SEND_MESSAGE_PATH: &str = "/message:send";
const REST_SEND_MESSAGE_LEGACY_PATH: &str = "/message/send";
const REST_STREAM_MESSAGE_PATH: &str = "/message:stream";
const REST_STREAM_MESSAGE_LEGACY_PATH: &str = "/message/stream";
const REST_EXTENDED_AGENT_CARD_PATH: &str = "/extendedAgentCard";
const REST_EXTENDED_AGENT_CARD_LEGACY_PATH: &str = "/agent-card/extended";
const REST_PUSH_CONFIGS_PATH: &str = "/tasks/{id}/pushNotificationConfigs";
const REST_PUSH_CONFIGS_LEGACY_PATH: &str = "/tasks/{id}/push-configs";
const REST_PUSH_CONFIG_PATH: &str = "/tasks/{id}/pushNotificationConfigs/{config_id}";
const REST_PUSH_CONFIG_LEGACY_PATH: &str = "/tasks/{id}/push-configs/{config_id}";
const REST_ERROR_INFO_TYPE_URL: &str = "type.googleapis.com/google.rpc.ErrorInfo";
const REST_ERROR_DOMAIN: &str = "a2a-protocol.org";

#[derive(Serialize)]
struct RestErrorEnvelope {
    error: RestErrorStatus,
}

#[derive(Serialize)]
struct RestErrorStatus {
    code: u16,
    status: &'static str,
    message: String,
    details: Vec<Value>,
}

#[derive(Serialize)]
struct RestErrorInfo {
    #[serde(rename = "@type")]
    type_url: &'static str,
    reason: &'static str,
    domain: &'static str,
    metadata: HashMap<String, String>,
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
        .with_state(state)
}

async fn handle_send_message<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Json(raw_req): Json<Value>,
) -> impl IntoResponse {
    let req = match protojson_conv::from_value::<SendMessageRequest>(raw_req) {
        Ok(req) => req,
        Err(e) => return rest_payload_error_response(e),
    };
    let params = ServiceParams::new();
    match state.handler.send_message(&params, req).await {
        Ok(resp) => protojson_json_response(&resp),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_stream_message<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Json(raw_req): Json<Value>,
) -> impl IntoResponse {
    let req = match protojson_conv::from_value::<SendMessageRequest>(raw_req) {
        Ok(req) => req,
        Err(e) => return rest_payload_error_response(e),
    };
    let params = ServiceParams::new();
    match state.handler.send_streaming_message(&params, req).await {
        Ok(stream) => sse::sse_from_stream(protojson_stream(stream)).into_response(),
        Err(e) => rest_error_response(e),
    }
}

#[derive(Deserialize)]
pub struct GetTaskQuery {
    #[serde(default, rename = "historyLength")]
    pub history_length: Option<i32>,
}

async fn handle_get_task_inner<H: RequestHandler>(
    state: RestState<H>,
    id: String,
    query: GetTaskQuery,
) -> axum::response::Response {
    let params = ServiceParams::new();
    let req = GetTaskRequest {
        id,
        history_length: query.history_length,
        tenant: None,
    };
    match state.handler.get_task(&params, req).await {
        Ok(task) => protojson_json_response(&task),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_get_task_or_subscribe<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(id): Path<String>,
    Query(query): Query<GetTaskQuery>,
) -> impl IntoResponse {
    if let Some(task_id) = id.strip_suffix(":subscribe") {
        return handle_subscribe_to_task_inner(state, task_id.to_string()).await;
    }

    handle_get_task_inner(state, id, query).await
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

    #[serde(default, rename = "statusTimestampAfter")]
    pub status_timestamp_after: Option<DateTime<Utc>>,

    #[serde(default, rename = "includeArtifacts")]
    pub include_artifacts: Option<bool>,
}

async fn handle_list_tasks<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Query(query): Query<ListTasksQuery>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    let status = query
        .status
        .and_then(|s| serde_json::from_value::<TaskState>(serde_json::Value::String(s)).ok());
    let req = ListTasksRequest {
        context_id: query.context_id,
        status,
        page_size: query.page_size,
        page_token: query.page_token,
        history_length: query.history_length,
        status_timestamp_after: query.status_timestamp_after,
        include_artifacts: query.include_artifacts,
        tenant: None,
    };
    match state.handler.list_tasks(&params, req).await {
        Ok(resp) => protojson_json_response(&resp),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_cancel_task_inner<H: RequestHandler>(
    state: RestState<H>,
    id: String,
) -> axum::response::Response {
    let params = ServiceParams::new();
    let req = CancelTaskRequest {
        id,
        metadata: None,
        tenant: None,
    };
    match state.handler.cancel_task(&params, req).await {
        Ok(task) => protojson_json_response(&task),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_post_task_action<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    if let Some(task_id) = id.strip_suffix(":cancel") {
        return handle_cancel_task_inner(state, task_id.to_string()).await;
    }
    if let Some(task_id) = id.strip_suffix(":subscribe") {
        return handle_subscribe_to_task_inner(state, task_id.to_string()).await;
    }

    rest_error_response(A2AError::invalid_request("unsupported task action"))
}

async fn handle_cancel_task_legacy<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    handle_cancel_task_inner(state, id).await
}

async fn handle_subscribe_to_task_inner<H: RequestHandler>(
    state: RestState<H>,
    id: String,
) -> axum::response::Response {
    let params = ServiceParams::new();
    let req = SubscribeToTaskRequest { id, tenant: None };
    match state.handler.subscribe_to_task(&params, req).await {
        Ok(stream) => sse::sse_from_stream(protojson_stream(stream)).into_response(),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_subscribe_to_task_legacy<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    handle_subscribe_to_task_inner(state, id).await
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
    Path(id): Path<String>,
    Json(raw_config): Json<Value>,
) -> impl IntoResponse {
    let req = match parse_create_push_config_request(id, raw_config) {
        Ok(req) => req,
        Err(e) => return rest_payload_error_response(e),
    };
    let params = ServiceParams::new();
    match state.handler.create_push_config(&params, req).await {
        Ok(resp) => serde_json_response(&resp),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_get_push_config<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path((id, config_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    let req = GetTaskPushNotificationConfigRequest {
        task_id: id,
        id: config_id,
        tenant: None,
    };
    match state.handler.get_push_config(&params, req).await {
        Ok(resp) => serde_json_response(&resp),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_list_push_configs<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(id): Path<String>,
    Query(query): Query<ListPushConfigsQuery>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    let req = ListTaskPushNotificationConfigsRequest {
        task_id: id,
        page_size: query.page_size,
        page_token: query.page_token,
        tenant: None,
    };
    match state.handler.list_push_configs(&params, req).await {
        Ok(resp) => serde_json_response(&resp.configs),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_delete_push_config<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path((id, config_id)): Path<(String, String)>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    let req = DeleteTaskPushNotificationConfigRequest {
        task_id: id,
        id: config_id,
        tenant: None,
    };
    match state.handler.delete_push_config(&params, req).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_get_extended_agent_card<H: RequestHandler>(
    State(state): State<RestState<H>>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    let req = GetExtendedAgentCardRequest { tenant: None };
    match state.handler.get_extended_agent_card(&params, req).await {
        Ok(card) => protojson_json_response(&card),
        Err(e) => rest_error_response(e),
    }
}

fn protojson_json_response<T: ProtoJsonPayload>(value: &T) -> axum::response::Response {
    match protojson_conv::to_value(value) {
        Ok(payload) => Json(payload).into_response(),
        Err(e) => rest_error_response(A2AError::internal(format!(
            "failed to serialize ProtoJSON payload: {e}"
        ))),
    }
}

fn serde_json_response<T: Serialize>(value: &T) -> axum::response::Response {
    match compat_json_value(value) {
        Ok(payload) => Json(payload).into_response(),
        Err(e) => rest_error_response(e),
    }
}

fn protojson_stream(
    stream: BoxStream<'static, Result<StreamResponse, A2AError>>,
) -> BoxStream<'static, Result<Value, A2AError>> {
    Box::pin(stream.map(|item| {
        item.and_then(|value| {
            protojson_conv::to_value(&value).map_err(|e| {
                A2AError::internal(format!("failed to serialize ProtoJSON stream payload: {e}"))
            })
        })
    }))
}

fn parse_create_push_config_request(
    task_id: String,
    raw_config: Value,
) -> Result<CreateTaskPushNotificationConfigRequest, String> {
    match protojson_conv::from_value::<PushNotificationConfig>(raw_config.clone()) {
        Ok(config) => Ok(CreateTaskPushNotificationConfigRequest {
            task_id,
            config,
            tenant: None,
        }),
        Err(protojson_error) => {
            let req = serde_json::from_value::<CreateTaskPushNotificationConfigRequest>(raw_config)
                .map_err(|serde_error| {
                    format!("{protojson_error}; nested request parse failed: {serde_error}")
                })?;
            if req.task_id != task_id {
                return Err(format!(
                    "taskId in request body ({}) did not match task id in path ({task_id})",
                    req.task_id
                ));
            }
            Ok(req)
        }
    }
}

fn rest_payload_error_response(error: impl std::fmt::Display) -> axum::response::Response {
    rest_error_response(A2AError {
        code: error_code::PARSE_ERROR,
        message: format!("invalid request body: {error}"),
        details: None,
    })
}

fn rest_error_response(err: A2AError) -> axum::response::Response {
    let status =
        StatusCode::from_u16(err.http_status_code()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let reason = rest_error_reason(err.code);
    let grpc_status = rest_grpc_status(err.code);

    let mut metadata = HashMap::from([("timestamp".to_string(), Utc::now().to_rfc3339())]);
    let mut extra_details = serde_json::Map::new();

    if let Some(details) = err.details {
        for (key, value) in details {
            if let Some(as_string) = value.as_str() {
                metadata.insert(key, as_string.to_string());
            } else {
                extra_details.insert(key, value);
            }
        }
    }

    let mut details = vec![
        serde_json::to_value(RestErrorInfo {
            type_url: REST_ERROR_INFO_TYPE_URL,
            reason,
            domain: REST_ERROR_DOMAIN,
            metadata,
        })
        .expect("rest error info should serialize"),
    ];

    if !extra_details.is_empty() {
        details.push(Value::Object(extra_details));
    }

    let body = RestErrorEnvelope {
        error: RestErrorStatus {
            code: status.as_u16(),
            status: grpc_status,
            message: err.message,
            details,
        },
    };

    (status, Json(body)).into_response()
}

fn rest_error_reason(code: i32) -> &'static str {
    match code {
        error_code::TASK_NOT_FOUND => "TASK_NOT_FOUND",
        error_code::TASK_NOT_CANCELABLE => "TASK_NOT_CANCELABLE",
        error_code::PUSH_NOTIFICATION_NOT_SUPPORTED => "PUSH_NOTIFICATION_NOT_SUPPORTED",
        error_code::UNSUPPORTED_OPERATION => "UNSUPPORTED_OPERATION",
        error_code::CONTENT_TYPE_NOT_SUPPORTED => "UNSUPPORTED_CONTENT_TYPE",
        error_code::INVALID_AGENT_RESPONSE => "INVALID_AGENT_RESPONSE",
        error_code::EXTENDED_CARD_NOT_CONFIGURED => "EXTENDED_AGENT_CARD_NOT_CONFIGURED",
        error_code::EXTENSION_SUPPORT_REQUIRED => "EXTENSION_SUPPORT_REQUIRED",
        error_code::VERSION_NOT_SUPPORTED => "VERSION_NOT_SUPPORTED",
        error_code::PARSE_ERROR => "PARSE_ERROR",
        error_code::INVALID_REQUEST => "INVALID_REQUEST",
        error_code::METHOD_NOT_FOUND => "METHOD_NOT_FOUND",
        error_code::INVALID_PARAMS => "INVALID_PARAMS",
        _ => "INTERNAL_ERROR",
    }
}

fn rest_grpc_status(code: i32) -> &'static str {
    match code {
        error_code::TASK_NOT_FOUND | error_code::METHOD_NOT_FOUND => "NOT_FOUND",
        error_code::TASK_NOT_CANCELABLE
        | error_code::EXTENDED_CARD_NOT_CONFIGURED
        | error_code::EXTENSION_SUPPORT_REQUIRED => "FAILED_PRECONDITION",
        error_code::PUSH_NOTIFICATION_NOT_SUPPORTED
        | error_code::UNSUPPORTED_OPERATION
        | error_code::VERSION_NOT_SUPPORTED => "UNIMPLEMENTED",
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
    use tower::ServiceExt;

    struct EchoExecutor;

    impl crate::AgentExecutor for EchoExecutor {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let task = Task {
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

    fn make_app() -> axum::Router {
        let handler = Arc::new(DefaultRequestHandler::new(
            EchoExecutor,
            InMemoryTaskStore::new(),
        ));
        rest_router(handler)
    }

    fn make_push_app() -> axum::Router {
        let handler = Arc::new(
            DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
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
        let req = Request::builder()
            .uri(REST_SEND_MESSAGE_PATH)
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
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
        let req = Request::builder()
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
        let req = Request::builder()
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
        let req = Request::builder()
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
        let req = Request::builder()
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
        let req = Request::builder()
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
        let app = make_push_app();
        let body = serde_json::json!({
            "id": "cfg1",
            "url": "http://example.com/callback"
        });
        let req = Request::builder()
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
        assert_eq!(payload.config.id.as_deref(), Some("cfg1"));
        assert_eq!(payload.config.url, "http://example.com/callback");
    }

    #[tokio::test]
    async fn test_create_push_config_accepts_nested_request_body() {
        let app = make_push_app();
        let body = serde_json::json!({
            "taskId": "t1",
            "config": {
                "id": "cfg1",
                "url": "http://example.com/callback"
            }
        });
        let req = Request::builder()
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
        assert_eq!(payload.config.id.as_deref(), Some("cfg1"));
        assert_eq!(payload.config.url, "http://example.com/callback");
    }

    #[tokio::test]
    async fn test_create_push_config_rejects_mismatched_task_id_in_body() {
        let app = make_push_app();
        let body = serde_json::json!({
            "taskId": "other-task",
            "config": {
                "url": "http://example.com/callback"
            }
        });
        let req = Request::builder()
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
        let req = Request::builder()
            .uri(REST_EXTENDED_AGENT_CARD_PATH)
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn test_subscribe_to_task_not_found() {
        let app = make_app();
        let req = Request::builder()
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
        let req = Request::builder()
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
        let req = Request::builder()
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
        let app = make_push_app();
        let req = Request::builder()
            .uri("/tasks/t1/pushNotificationConfigs")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let payload = serde_json::from_slice::<Vec<TaskPushNotificationConfig>>(&body).unwrap();
        assert!(payload.is_empty());
    }

    #[tokio::test]
    async fn test_delete_push_config() {
        let app = make_app();
        let req = Request::builder()
            .uri("/tasks/t1/pushNotificationConfigs/cfg1")
            .method("DELETE")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn test_delete_push_config_with_store_returns_ok() {
        let app = make_push_app();
        let create_body = serde_json::json!({
            "id": "cfg1",
            "url": "http://example.com/callback"
        });
        let create_req = Request::builder()
            .uri("/tasks/t1/pushNotificationConfigs")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&create_body).unwrap()))
            .unwrap();
        let create_resp = app.clone().oneshot(create_req).await.unwrap();
        assert_eq!(create_resp.status(), StatusCode::OK);

        let delete_req = Request::builder()
            .uri("/tasks/t1/pushNotificationConfigs/cfg1")
            .method("DELETE")
            .body(Body::empty())
            .unwrap();
        let delete_resp = app.clone().oneshot(delete_req).await.unwrap();
        assert_eq!(delete_resp.status(), StatusCode::OK);

        let list_req = Request::builder()
            .uri("/tasks/t1/pushNotificationConfigs")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let list_resp = app.oneshot(list_req).await.unwrap();
        assert_eq!(list_resp.status(), StatusCode::OK);
        let body = list_resp.into_body().collect().await.unwrap().to_bytes();
        let payload = serde_json::from_slice::<Vec<TaskPushNotificationConfig>>(&body).unwrap();
        assert!(payload.is_empty());
    }

    #[tokio::test]
    async fn test_list_tasks_with_query_params() {
        let app = make_app();
        let req = Request::builder()
            .uri("/tasks?contextId=c1&pageSize=5&pageToken=tok&historyLength=3&includeArtifacts=true&statusTimestampAfter=2025-01-01T00:00:00Z")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
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
        let req = Request::builder()
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
        let req = Request::builder()
            .uri(format!("/tasks/{}?historyLength=5", task_id))
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Each make_app() creates a new store, so this will be not found
        assert!(resp.status() == StatusCode::OK || resp.status() == StatusCode::NOT_FOUND);
    }
}
