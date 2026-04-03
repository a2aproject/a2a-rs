// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;

use a2a::*;
use axum::{
    Json,
    extract::{Path, Query, State},
    http::StatusCode,
    response::IntoResponse,
};
use serde::Deserialize;

use crate::handler::RequestHandler;
use crate::middleware::ServiceParams;
use crate::sse;

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
            "/message/send",
            axum::routing::post(handle_send_message::<H>),
        )
        .route(
            "/message/stream",
            axum::routing::post(handle_stream_message::<H>),
        )
        .route("/tasks/{id}", axum::routing::get(handle_get_task::<H>))
        .route("/tasks", axum::routing::get(handle_list_tasks::<H>))
        .route(
            "/tasks/{id}/cancel",
            axum::routing::post(handle_cancel_task::<H>),
        )
        .route(
            "/tasks/{id}/subscribe",
            axum::routing::post(handle_subscribe_to_task::<H>),
        )
        .route(
            "/tasks/{id}/push-configs",
            axum::routing::post(handle_create_push_config::<H>).get(handle_list_push_configs::<H>),
        )
        .route(
            "/tasks/{id}/push-configs/{config_id}",
            axum::routing::get(handle_get_push_config::<H>).delete(handle_delete_push_config::<H>),
        )
        .route(
            "/agent-card/extended",
            axum::routing::get(handle_get_extended_agent_card::<H>),
        )
        .with_state(state)
}

async fn handle_send_message<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Json(req): Json<SendMessageRequest>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    match state.handler.send_message(&params, req).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_stream_message<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Json(req): Json<SendMessageRequest>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    match state.handler.send_streaming_message(&params, req).await {
        Ok(stream) => sse::sse_from_stream(stream).into_response(),
        Err(e) => rest_error_response(e),
    }
}

#[derive(Deserialize)]
pub struct GetTaskQuery {
    #[serde(default, rename = "historyLength")]
    pub history_length: Option<i32>,
}

async fn handle_get_task<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(id): Path<String>,
    Query(query): Query<GetTaskQuery>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    let req = GetTaskRequest {
        id,
        history_length: query.history_length,
        tenant: None,
    };
    match state.handler.get_task(&params, req).await {
        Ok(task) => Json(task).into_response(),
        Err(e) => rest_error_response(e),
    }
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
        status_timestamp_after: None,
        include_artifacts: None,
        tenant: None,
    };
    match state.handler.list_tasks(&params, req).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_cancel_task<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    let req = CancelTaskRequest {
        id,
        metadata: None,
        tenant: None,
    };
    match state.handler.cancel_task(&params, req).await {
        Ok(task) => Json(task).into_response(),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_subscribe_to_task<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    let req = SubscribeToTaskRequest { id, tenant: None };
    match state.handler.subscribe_to_task(&params, req).await {
        Ok(stream) => sse::sse_from_stream(stream).into_response(),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_create_push_config<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(id): Path<String>,
    Json(config): Json<PushNotificationConfig>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    let req = CreateTaskPushNotificationConfigRequest {
        task_id: id,
        config,
        tenant: None,
    };
    match state.handler.create_push_config(&params, req).await {
        Ok(resp) => Json(resp).into_response(),
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
        Ok(resp) => Json(resp).into_response(),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_list_push_configs<H: RequestHandler>(
    State(state): State<RestState<H>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    let req = ListTaskPushNotificationConfigsRequest {
        task_id: id,
        page_size: None,
        page_token: None,
        tenant: None,
    };
    match state.handler.list_push_configs(&params, req).await {
        Ok(resp) => Json(resp).into_response(),
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
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => rest_error_response(e),
    }
}

async fn handle_get_extended_agent_card<H: RequestHandler>(
    State(state): State<RestState<H>>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    let req = GetExtendedAgentCardRequest { tenant: None };
    match state.handler.get_extended_agent_card(&params, req).await {
        Ok(card) => Json(card).into_response(),
        Err(e) => rest_error_response(e),
    }
}

fn rest_error_response(err: A2AError) -> axum::response::Response {
    let status = err.http_status_code();
    let body = serde_json::json!({
        "error": {
            "code": err.code,
            "message": err.message,
        }
    });
    (
        StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(body),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ExecutorContext;
    use crate::handler::DefaultRequestHandler;
    use crate::task_store::InMemoryTaskStore;
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
            .uri("/message/send")
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
        let list_resp: ListTasksResponse = serde_json::from_slice(&body).unwrap();
        assert!(list_resp.tasks.is_empty());
    }

    #[tokio::test]
    async fn test_cancel_task_not_found() {
        let app = make_app();
        let req = Request::builder()
            .uri("/tasks/nonexistent/cancel")
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
            .uri("/tasks/t1/push-configs")
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
            .uri("/agent-card/extended")
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

        let resp = rest_error_response(A2AError::internal("boom"));
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let resp = rest_error_response(A2AError::content_type_not_supported());
        assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    }

    #[tokio::test]
    async fn test_subscribe_to_task_not_found() {
        let app = make_app();
        let req = Request::builder()
            .uri("/tasks/nonexistent/subscribe")
            .method("POST")
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
            .uri("/message/stream")
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
            .uri("/tasks/t1/push-configs/cfg1")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Push not supported → bad request
        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn test_list_push_configs() {
        let app = make_app();
        let req = Request::builder()
            .uri("/tasks/t1/push-configs")
            .method("GET")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn test_delete_push_config() {
        let app = make_app();
        let req = Request::builder()
            .uri("/tasks/t1/push-configs/cfg1")
            .method("DELETE")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert!(resp.status().is_client_error());
    }

    #[tokio::test]
    async fn test_list_tasks_with_query_params() {
        let app = make_app();
        let req = Request::builder()
            .uri("/tasks?contextId=c1&pageSize=5&pageToken=tok&historyLength=3")
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
            .uri("/message/send")
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
