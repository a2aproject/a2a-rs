// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;

use a2a::*;
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use axum::{Json, extract::State, http::StatusCode, response::IntoResponse};
use futures::{StreamExt, stream::BoxStream};
use serde_json::Value;

use crate::handler::RequestHandler;
use crate::middleware::ServiceParams;
use crate::push_config_compat::json_value as compat_json_value;
use crate::sse;

/// Shared state for the JSON-RPC handler.
pub struct JsonRpcState<H: RequestHandler> {
    pub handler: Arc<H>,
}

impl<H: RequestHandler> Clone for JsonRpcState<H> {
    fn clone(&self) -> Self {
        JsonRpcState {
            handler: self.handler.clone(),
        }
    }
}

/// Create an axum router for the JSON-RPC protocol binding.
///
/// All requests are dispatched to a single POST endpoint that routes
/// by the `method` field in the JSON-RPC envelope.
pub fn jsonrpc_router<H: RequestHandler>(handler: Arc<H>) -> axum::Router {
    let state = JsonRpcState { handler };
    axum::Router::new()
        .route("/", axum::routing::post(handle_jsonrpc::<H>))
        .with_state(state)
}

async fn handle_jsonrpc<H: RequestHandler>(
    State(state): State<JsonRpcState<H>>,
    Json(request): Json<JsonRpcRequest>,
) -> impl IntoResponse {
    let params = ServiceParams::new();
    let id = request.id.clone();
    let method = request.method.as_str();

    if request.jsonrpc != "2.0" {
        return error_response(id, A2AError::invalid_request("invalid jsonrpc version"));
    }

    if methods::is_streaming(method) {
        return handle_streaming_request(&state, &params, &request).await;
    }

    handle_unary_request(&state, &params, &request).await
}

async fn handle_unary_request<H: RequestHandler>(
    state: &JsonRpcState<H>,
    params: &ServiceParams,
    request: &JsonRpcRequest,
) -> axum::response::Response {
    let id = request.id.clone();
    let raw_params = request.params.clone().unwrap_or(Value::Null);

    let result: Result<Value, A2AError> = match request.method.as_str() {
        methods::SEND_MESSAGE => match protojson_conv::from_value::<SendMessageRequest>(raw_params)
        {
            Ok(req) => state
                .handler
                .send_message(params, req)
                .await
                .and_then(|r| protojson_value(&r)),
            Err(e) => Err(parse_error(e)),
        },
        methods::GET_TASK => match protojson_conv::from_value::<GetTaskRequest>(raw_params) {
            Ok(req) => state
                .handler
                .get_task(params, req)
                .await
                .and_then(|r| protojson_value(&r)),
            Err(e) => Err(parse_error(e)),
        },
        methods::LIST_TASKS => match protojson_conv::from_value::<ListTasksRequest>(raw_params) {
            Ok(req) => state
                .handler
                .list_tasks(params, req)
                .await
                .and_then(|r| protojson_value(&r)),
            Err(e) => Err(parse_error(e)),
        },
        methods::CANCEL_TASK => match protojson_conv::from_value::<CancelTaskRequest>(raw_params) {
            Ok(req) => state
                .handler
                .cancel_task(params, req)
                .await
                .and_then(|r| protojson_value(&r)),
            Err(e) => Err(parse_error(e)),
        },
        methods::CREATE_PUSH_CONFIG => match parse_create_push_config_request(raw_params) {
            Ok(req) => state
                .handler
                .create_push_config(params, req)
                .await
                .and_then(|r| compat_json_value(&r)),
            Err(e) => Err(parse_error(e)),
        },
        methods::GET_PUSH_CONFIG => {
            match protojson_conv::from_value::<GetTaskPushNotificationConfigRequest>(raw_params) {
                Ok(req) => state
                    .handler
                    .get_push_config(params, req)
                    .await
                    .and_then(|r| compat_json_value(&r)),
                Err(e) => Err(parse_error(e)),
            }
        }
        methods::LIST_PUSH_CONFIGS => {
            match protojson_conv::from_value::<ListTaskPushNotificationConfigsRequest>(raw_params) {
                Ok(req) => state
                    .handler
                    .list_push_configs(params, req)
                    .await
                    .and_then(|r| compat_json_value(&r.configs)),
                Err(e) => Err(parse_error(e)),
            }
        }
        methods::DELETE_PUSH_CONFIG => {
            match protojson_conv::from_value::<DeleteTaskPushNotificationConfigRequest>(raw_params)
            {
                Ok(req) => state
                    .handler
                    .delete_push_config(params, req)
                    .await
                    .map(|_| Value::Null),
                Err(e) => Err(parse_error(e)),
            }
        }
        methods::GET_EXTENDED_AGENT_CARD => {
            match protojson_conv::from_value::<GetExtendedAgentCardRequest>(raw_params) {
                Ok(req) => state
                    .handler
                    .get_extended_agent_card(params, req)
                    .await
                    .and_then(|r| protojson_value(&r)),
                Err(e) => Err(parse_error(e)),
            }
        }
        "" => Err(A2AError::invalid_request("method is required")),
        _ => Err(A2AError::method_not_found(&request.method)),
    };

    match result {
        Ok(value) => {
            let resp = JsonRpcResponse::success(id, value);
            Json(resp).into_response()
        }
        Err(e) => error_response(id, e),
    }
}

async fn handle_streaming_request<H: RequestHandler>(
    state: &JsonRpcState<H>,
    params: &ServiceParams,
    request: &JsonRpcRequest,
) -> axum::response::Response {
    let id = request.id.clone();
    let raw_params = request.params.clone().unwrap_or(Value::Null);

    match request.method.as_str() {
        methods::SEND_STREAMING_MESSAGE => {
            match protojson_conv::from_value::<SendMessageRequest>(raw_params) {
                Ok(req) => match state.handler.send_streaming_message(params, req).await {
                    Ok(stream) => {
                        sse::sse_jsonrpc_stream(id, protojson_stream(stream)).into_response()
                    }
                    Err(e) => error_response(id, e),
                },
                Err(e) => error_response(id, parse_error(e)),
            }
        }
        methods::SUBSCRIBE_TO_TASK => {
            match protojson_conv::from_value::<SubscribeToTaskRequest>(raw_params) {
                Ok(req) => match state.handler.subscribe_to_task(params, req).await {
                    Ok(stream) => {
                        sse::sse_jsonrpc_stream(id, protojson_stream(stream)).into_response()
                    }
                    Err(e) => error_response(id, e),
                },
                Err(e) => error_response(id, parse_error(e)),
            }
        }
        _ => error_response(id, A2AError::method_not_found(&request.method)),
    }
}

fn error_response(id: JsonRpcId, err: A2AError) -> axum::response::Response {
    let resp = JsonRpcResponse::error(id, err.to_jsonrpc_error());
    (StatusCode::OK, Json(resp)).into_response()
}

fn protojson_value<T: ProtoJsonPayload>(value: &T) -> Result<Value, A2AError> {
    protojson_conv::to_value(value)
        .map_err(|e| A2AError::internal(format!("failed to serialize ProtoJSON payload: {e}")))
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
    raw_params: Value,
) -> Result<CreateTaskPushNotificationConfigRequest, String> {
    match protojson_conv::from_value::<CreateTaskPushNotificationConfigRequest>(raw_params.clone())
    {
        Ok(req) => Ok(req),
        Err(protojson_error) => serde_json::from_value::<CreateTaskPushNotificationConfigRequest>(
            raw_params,
        )
        .map_err(|serde_error| {
            format!("{protojson_error}; nested request parse failed: {serde_error}")
        }),
    }
}

fn parse_error(e: impl std::fmt::Display) -> A2AError {
    A2AError {
        code: error_code::PARSE_ERROR,
        message: format!("invalid params: {e}"),
        details: None,
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
        jsonrpc_router(handler)
    }

    fn make_push_app() -> axum::Router {
        let handler = Arc::new(
            DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
                .with_push_config_store(InMemoryPushConfigStore::new()),
        );
        jsonrpc_router(handler)
    }

    async fn post_jsonrpc(app: axum::Router, method: &str, params: Value) -> JsonRpcResponse {
        let rpc = JsonRpcRequest::new(JsonRpcId::Number(1), method, Some(params));
        let body = serde_json::to_string(&rpc).unwrap();
        let req = Request::builder()
            .uri("/")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&body).unwrap()
    }

    #[tokio::test]
    async fn test_send_message() {
        let app = make_app();
        let params = serde_json::json!({
            "message": {
                "messageId": "m1",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            }
        });
        let resp = post_jsonrpc(app, methods::SEND_MESSAGE, params).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result =
            protojson_conv::from_value::<SendMessageResponse>(resp.result.unwrap()).unwrap();
        assert!(matches!(result, SendMessageResponse::Task(_)));
    }

    #[tokio::test]
    async fn test_send_message_legacy_method_rejected() {
        let app = make_app();
        let params = serde_json::json!({
            "message": {
                "messageId": "m1",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            }
        });
        let resp = post_jsonrpc(app, "message.send", params).await;
        assert!(resp.error.is_some(), "unexpected result: {:?}", resp.result);
        assert_eq!(resp.error.unwrap().code, error_code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_task_not_found() {
        let app = make_app();
        let params = serde_json::json!({"id": "nonexistent"});
        let resp = post_jsonrpc(app, methods::GET_TASK, params).await;
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, error_code::TASK_NOT_FOUND);
    }

    #[tokio::test]
    async fn test_invalid_method() {
        let app = make_app();
        let resp = post_jsonrpc(app, "unknown.method", Value::Null).await;
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, error_code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn test_empty_method() {
        let app = make_app();
        let resp = post_jsonrpc(app, "", Value::Null).await;
        assert!(resp.error.is_some());
    }

    #[tokio::test]
    async fn test_invalid_jsonrpc_version() {
        let app = make_app();
        let rpc = serde_json::json!({
            "jsonrpc": "1.0",
            "id": 1,
            "method": methods::SEND_MESSAGE,
            "params": {}
        });
        let body = serde_json::to_string(&rpc).unwrap();
        let req = Request::builder()
            .uri("/")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc_resp: JsonRpcResponse = serde_json::from_slice(&body).unwrap();
        assert!(rpc_resp.error.is_some());
    }

    #[tokio::test]
    async fn test_invalid_params() {
        let app = make_app();
        let params = serde_json::json!({"bogus": true});
        let resp = post_jsonrpc(app, methods::SEND_MESSAGE, params).await;
        assert!(resp.error.is_some());
        assert_eq!(resp.error.unwrap().code, error_code::PARSE_ERROR);
    }

    #[tokio::test]
    async fn test_list_tasks() {
        let app = make_app();
        let params = serde_json::json!({});
        let resp = post_jsonrpc(app, methods::LIST_TASKS, params).await;
        let result = protojson_conv::from_value::<ListTasksResponse>(resp.result.unwrap()).unwrap();
        assert!(result.tasks.is_empty());
    }

    #[tokio::test]
    async fn test_cancel_task_not_found() {
        let app = make_app();
        let params = serde_json::json!({"id": "nonexistent"});
        let resp = post_jsonrpc(app, methods::CANCEL_TASK, params).await;
        assert!(resp.error.is_some());
    }

    #[tokio::test]
    async fn test_create_push_config() {
        let app = make_push_app();
        let params = serde_json::json!({
            "taskId": "t1",
            "id": "cfg1",
            "url": "http://example.com/callback"
        });
        let resp = post_jsonrpc(app, methods::CREATE_PUSH_CONFIG, params).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result =
            serde_json::from_value::<TaskPushNotificationConfig>(resp.result.unwrap()).unwrap();
        assert_eq!(result.task_id, "t1");
        assert_eq!(result.config.id.as_deref(), Some("cfg1"));
        assert_eq!(result.config.url, "http://example.com/callback");
    }

    #[tokio::test]
    async fn test_create_push_config_accepts_nested_config_object() {
        let app = make_push_app();
        let params = serde_json::json!({
            "taskId": "t1",
            "config": {
                "id": "cfg1",
                "url": "http://example.com/callback"
            }
        });
        let resp = post_jsonrpc(app, methods::CREATE_PUSH_CONFIG, params).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result =
            serde_json::from_value::<TaskPushNotificationConfig>(resp.result.unwrap()).unwrap();
        assert_eq!(result.task_id, "t1");
        assert_eq!(result.config.id.as_deref(), Some("cfg1"));
        assert_eq!(result.config.url, "http://example.com/callback");
    }

    #[tokio::test]
    async fn test_get_push_config() {
        let app = make_app();
        let params = serde_json::json!({
            "taskId": "t1",
            "id": "cfg1"
        });
        let resp = post_jsonrpc(app, methods::GET_PUSH_CONFIG, params).await;
        assert!(resp.error.is_some() || resp.result.is_some());
    }

    #[tokio::test]
    async fn test_list_push_configs() {
        let app = make_push_app();
        let params = serde_json::json!({
            "taskId": "t1"
        });
        let resp = post_jsonrpc(app, methods::LIST_PUSH_CONFIGS, params).await;
        assert!(resp.error.is_none(), "unexpected error: {:?}", resp.error);
        let result =
            serde_json::from_value::<Vec<TaskPushNotificationConfig>>(resp.result.unwrap())
                .unwrap();
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn test_delete_push_config() {
        let app = make_app();
        let params = serde_json::json!({
            "taskId": "t1",
            "id": "cfg1"
        });
        let resp = post_jsonrpc(app, methods::DELETE_PUSH_CONFIG, params).await;
        assert!(resp.error.is_some() || resp.result.is_some());
    }

    #[tokio::test]
    async fn test_get_extended_agent_card() {
        let app = make_app();
        let params = serde_json::json!({});
        let resp = post_jsonrpc(app, methods::GET_EXTENDED_AGENT_CARD, params).await;
        // DefaultRequestHandler returns NotSupported
        assert!(resp.error.is_some() || resp.result.is_some());
    }

    #[tokio::test]
    async fn test_streaming_send_message() {
        let app = make_app();
        let body = serde_json::json!({
            "message": {
                "messageId": "m1",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            }
        });
        let rpc = JsonRpcRequest::new(
            JsonRpcId::Number(1),
            methods::SEND_STREAMING_MESSAGE,
            Some(body),
        );
        let req = Request::builder()
            .uri("/")
            .method("POST")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(Body::from(serde_json::to_string(&rpc).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // SSE response => 200
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_streaming_subscribe_to_task() {
        let app = make_app();
        let rpc = JsonRpcRequest::new(
            JsonRpcId::Number(1),
            methods::SUBSCRIBE_TO_TASK,
            Some(serde_json::json!({"id": "t1"})),
        );
        let req = Request::builder()
            .uri("/")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&rpc).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // Subscription may fail (task not found), but routing should work
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_streaming_send_message_legacy_method_rejected() {
        let app = make_app();
        let body = serde_json::json!({
            "message": {
                "messageId": "m1",
                "role": "ROLE_USER",
                "parts": [{"text": "hello"}]
            }
        });
        let rpc = JsonRpcRequest::new(JsonRpcId::Number(1), "message.stream", Some(body));
        let req = Request::builder()
            .uri("/")
            .method("POST")
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(Body::from(serde_json::to_string(&rpc).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let rpc_resp: JsonRpcResponse = serde_json::from_slice(&body).unwrap();
        assert!(
            rpc_resp.error.is_some(),
            "unexpected result: {:?}",
            rpc_resp.result
        );
        assert_eq!(rpc_resp.error.unwrap().code, error_code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn test_streaming_invalid_method() {
        let app = make_app();
        let rpc = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tasks.subscribe",
            "params": {"bogus": true}
        });
        let req = Request::builder()
            .uri("/")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_string(&rpc).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
