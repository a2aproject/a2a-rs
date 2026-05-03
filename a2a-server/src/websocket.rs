// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;

use a2a::websocket::{WebSocketRequestEnvelope, WebSocketResponseEnvelope};
use a2a::*;
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use futures::{SinkExt, StreamExt, stream::BoxStream};
use serde_json::Value;
use tokio::sync::mpsc;

use crate::handler::RequestHandler;
use crate::middleware::ServiceParams;
use crate::push_config_compat::json_value as compat_json_value;

/// Shared state for the WebSocket handler.
pub struct WebSocketState<H: RequestHandler> {
    pub handler: Arc<H>,
}

impl<H: RequestHandler> Clone for WebSocketState<H> {
    fn clone(&self) -> Self {
        Self {
            handler: self.handler.clone(),
        }
    }
}

/// Create an axum router for the WebSocket protocol binding.
pub fn websocket_router<H: RequestHandler>(handler: Arc<H>) -> axum::Router {
    let state = WebSocketState { handler };
    axum::Router::new()
        .route("/", axum::routing::get(handle_websocket::<H>))
        .with_state(state)
}

async fn handle_websocket<H: RequestHandler>(
    State(state): State<WebSocketState<H>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let connection_params = service_params_from_headers(&headers);
    ws.on_upgrade(move |socket| serve_socket(socket, state, connection_params))
}

async fn serve_socket<H: RequestHandler>(
    socket: WebSocket,
    state: WebSocketState<H>,
    connection_params: ServiceParams,
) {
    let (mut sender, mut receiver) = socket.split();
    let (outgoing_tx, mut outgoing_rx) = mpsc::unbounded_channel::<Message>();

    let writer = tokio::spawn(async move {
        while let Some(message) = outgoing_rx.recv().await {
            if sender.send(message).await.is_err() {
                break;
            }
        }
    });

    while let Some(next) = receiver.next().await {
        let message = match next {
            Ok(msg) => msg,
            Err(_) => break,
        };

        match message {
            Message::Text(frame) => {
                handle_text_frame(
                    frame.as_str(),
                    &state,
                    &connection_params,
                    outgoing_tx.clone(),
                )
                .await;
            }
            Message::Binary(_) => {
                let _ = outgoing_tx.send(Message::Close(Some(axum::extract::ws::CloseFrame {
                    code: axum::extract::ws::close_code::UNSUPPORTED,
                    reason: "binary websocket frames are not supported".into(),
                })));
                break;
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    drop(outgoing_tx);
    let _ = writer.await;
}

async fn handle_text_frame<H: RequestHandler>(
    frame: &str,
    state: &WebSocketState<H>,
    connection_params: &ServiceParams,
    outgoing_tx: mpsc::UnboundedSender<Message>,
) {
    let request: WebSocketRequestEnvelope = match serde_json::from_str(frame) {
        Ok(request) => request,
        Err(error) => {
            let _ = send_response(
                &outgoing_tx,
                WebSocketResponseEnvelope::error(
                    JsonRpcId::Null,
                    parse_error(format!("invalid websocket frame: {error}")).to_jsonrpc_error(),
                ),
            );
            return;
        }
    };

    if request.jsonrpc != "2.0" {
        let _ = send_response(
            &outgoing_tx,
            WebSocketResponseEnvelope::error(
                request.id,
                A2AError::invalid_request("invalid jsonrpc version").to_jsonrpc_error(),
            ),
        );
        return;
    }

    let mut params = connection_params.clone();
    if let Some(message_params) = request.service_params.as_ref() {
        for (key, values) in message_params {
            params.insert(key.clone(), values.clone());
        }
    }

    if methods::is_streaming(&request.method) {
        spawn_streaming_call(state, params, request, outgoing_tx);
        return;
    }

    let id = request.id.clone();
    let result = handle_unary_request(state, &params, &request).await;
    let response = match result {
        Ok(value) => WebSocketResponseEnvelope::success(id, value),
        Err(error) => WebSocketResponseEnvelope::error(id, error.to_jsonrpc_error()),
    };
    let _ = send_response(&outgoing_tx, response);
}

fn spawn_streaming_call<H: RequestHandler>(
    state: &WebSocketState<H>,
    params: ServiceParams,
    request: WebSocketRequestEnvelope,
    outgoing_tx: mpsc::UnboundedSender<Message>,
) {
    let handler = state.handler.clone();
    tokio::spawn(async move {
        let id = request.id.clone();
        let raw_params = request.params.unwrap_or(Value::Null);

        let stream_result: Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> =
            match request.method.as_str() {
                methods::SEND_STREAMING_MESSAGE => {
                    match protojson_conv::from_value::<SendMessageRequest>(raw_params) {
                        Ok(req) => handler.send_streaming_message(&params, req).await,
                        Err(error) => Err(invalid_params_error(error)),
                    }
                }
                methods::SUBSCRIBE_TO_TASK => {
                    match protojson_conv::from_value::<SubscribeToTaskRequest>(raw_params) {
                        Ok(req) => handler.subscribe_to_task(&params, req).await,
                        Err(error) => Err(invalid_params_error(error)),
                    }
                }
                _ => Err(A2AError::method_not_found(&request.method)),
            };

        let mut stream = match stream_result {
            Ok(stream) => stream,
            Err(error) => {
                let _ = send_response(
                    &outgoing_tx,
                    WebSocketResponseEnvelope::error(id, error.to_jsonrpc_error()),
                );
                return;
            }
        };

        while let Some(item) = stream.next().await {
            match item {
                Ok(event) => match protojson_conv::to_value(&event) {
                    Ok(value) => {
                        let _ = send_response(
                            &outgoing_tx,
                            WebSocketResponseEnvelope::stream_event(id.clone(), value),
                        );
                    }
                    Err(error) => {
                        let _ = send_response(
                            &outgoing_tx,
                            WebSocketResponseEnvelope::stream_error(
                                id.clone(),
                                A2AError::internal(format!(
                                    "failed to serialize ProtoJSON stream payload: {error}"
                                ))
                                .to_jsonrpc_error(),
                            ),
                        );
                        return;
                    }
                },
                Err(error) => {
                    let _ = send_response(
                        &outgoing_tx,
                        WebSocketResponseEnvelope::stream_error(
                            id.clone(),
                            error.to_jsonrpc_error(),
                        ),
                    );
                    return;
                }
            }
        }

        let _ = send_response(&outgoing_tx, WebSocketResponseEnvelope::stream_end(id));
    });
}

async fn handle_unary_request<H: RequestHandler>(
    state: &WebSocketState<H>,
    params: &ServiceParams,
    request: &WebSocketRequestEnvelope,
) -> Result<Value, A2AError> {
    let raw_params = request.params.clone().unwrap_or(Value::Null);

    match request.method.as_str() {
        methods::SEND_MESSAGE => match protojson_conv::from_value::<SendMessageRequest>(raw_params)
        {
            Ok(req) => state
                .handler
                .send_message(params, req)
                .await
                .and_then(|r| protojson_value(&r)),
            Err(e) => Err(invalid_params_error(e)),
        },
        methods::GET_TASK => match protojson_conv::from_value::<GetTaskRequest>(raw_params) {
            Ok(req) => state
                .handler
                .get_task(params, req)
                .await
                .and_then(|r| protojson_value(&r)),
            Err(e) => Err(invalid_params_error(e)),
        },
        methods::LIST_TASKS => match protojson_conv::from_value::<ListTasksRequest>(raw_params) {
            Ok(req) => state
                .handler
                .list_tasks(params, req)
                .await
                .and_then(|r| protojson_value(&r)),
            Err(e) => Err(invalid_params_error(e)),
        },
        methods::CANCEL_TASK => match protojson_conv::from_value::<CancelTaskRequest>(raw_params) {
            Ok(req) => state
                .handler
                .cancel_task(params, req)
                .await
                .and_then(|r| protojson_value(&r)),
            Err(e) => Err(invalid_params_error(e)),
        },
        methods::CREATE_PUSH_CONFIG => match parse_create_push_config_request(raw_params) {
            Ok(req) => state
                .handler
                .create_push_config(params, req)
                .await
                .and_then(|r| compat_json_value(&r)),
            Err(e) => Err(invalid_params_error(e)),
        },
        methods::GET_PUSH_CONFIG => {
            match protojson_conv::from_value::<GetTaskPushNotificationConfigRequest>(raw_params) {
                Ok(req) => state
                    .handler
                    .get_push_config(params, req)
                    .await
                    .and_then(|r| compat_json_value(&r)),
                Err(e) => Err(invalid_params_error(e)),
            }
        }
        methods::LIST_PUSH_CONFIGS => {
            match protojson_conv::from_value::<ListTaskPushNotificationConfigsRequest>(raw_params) {
                Ok(req) => state
                    .handler
                    .list_push_configs(params, req)
                    .await
                    .and_then(|r| compat_json_value(&r)),
                Err(e) => Err(invalid_params_error(e)),
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
                Err(e) => Err(invalid_params_error(e)),
            }
        }
        methods::GET_EXTENDED_AGENT_CARD => {
            match protojson_conv::from_value::<GetExtendedAgentCardRequest>(raw_params) {
                Ok(req) => state
                    .handler
                    .get_extended_agent_card(params, req)
                    .await
                    .and_then(|r| protojson_value(&r)),
                Err(e) => Err(invalid_params_error(e)),
            }
        }
        "" => Err(A2AError::invalid_request("method is required")),
        _ => Err(A2AError::method_not_found(&request.method)),
    }
}

fn send_response(
    outgoing_tx: &mpsc::UnboundedSender<Message>,
    response: WebSocketResponseEnvelope,
) -> Result<(), A2AError> {
    let payload = serde_json::to_string(&response)
        .map_err(|e| A2AError::internal(format!("failed to serialize websocket frame: {e}")))?;
    outgoing_tx
        .send(Message::Text(payload.into()))
        .map_err(|_| A2AError::unsupported_operation("websocket output channel is closed"))
}

fn service_params_from_headers(headers: &HeaderMap) -> ServiceParams {
    let mut params = ServiceParams::new();
    for (name, value) in headers {
        if let Ok(text) = value.to_str() {
            params
                .entry(name.as_str().to_string())
                .or_default()
                .push(text.to_string());
        }
    }
    params
}

fn protojson_value<T: ProtoJsonPayload>(value: &T) -> Result<Value, A2AError> {
    protojson_conv::to_value(value)
        .map_err(|e| A2AError::internal(format!("failed to serialize ProtoJSON payload: {e}")))
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
    A2AError::parse_error(&e.to_string())
}

fn invalid_params_error(e: impl std::fmt::Display) -> A2AError {
    A2AError::invalid_params(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::jsonrpc::JsonRpcId;
    use a2a::websocket::WebSocketRequestEnvelope;
    use async_trait::async_trait;
    use axum::http::header::HeaderValue;
    use futures::stream;

    struct MockHandler {
        fail: bool,
    }

    impl MockHandler {
        fn ok() -> Arc<Self> {
            Arc::new(Self { fail: false })
        }
        fn failing() -> Arc<Self> {
            Arc::new(Self { fail: true })
        }
    }

    #[async_trait]
    impl RequestHandler for MockHandler {
        async fn send_message(
            &self,
            _: &ServiceParams,
            _: SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            if self.fail {
                return Err(A2AError::internal("fail"));
            }
            Ok(SendMessageResponse::Task(Task {
                id: "t1".to_string(),
                context_id: "ctx".to_string(),
                status: TaskStatus {
                    state: TaskState::Submitted,
                    message: None,
                    timestamp: None,
                },
                history: None,
                artifacts: None,
                metadata: None,
            }))
        }
        async fn send_streaming_message(
            &self,
            _: &ServiceParams,
            _: SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            if self.fail {
                return Err(A2AError::internal("stream-fail"));
            }
            Ok(Box::pin(stream::iter(vec![Ok(StreamResponse::Task(
                Task {
                    id: "t1".to_string(),
                    context_id: "ctx".to_string(),
                    status: TaskStatus {
                        state: TaskState::Completed,
                        message: None,
                        timestamp: None,
                    },
                    history: None,
                    artifacts: None,
                    metadata: None,
                },
            ))])))
        }
        async fn get_task(&self, _: &ServiceParams, req: GetTaskRequest) -> Result<Task, A2AError> {
            if req.id == "missing" {
                return Err(A2AError::task_not_found(&req.id));
            }
            Ok(Task {
                id: req.id,
                context_id: "ctx".to_string(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: None,
                },
                history: None,
                artifacts: None,
                metadata: None,
            })
        }
        async fn list_tasks(
            &self,
            _: &ServiceParams,
            _: ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            Ok(ListTasksResponse {
                tasks: vec![],
                next_page_token: "".into(),
                page_size: 0,
                total_size: 0,
            })
        }
        async fn cancel_task(
            &self,
            _: &ServiceParams,
            req: CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            Ok(Task {
                id: req.id,
                context_id: "ctx".to_string(),
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                history: None,
                artifacts: None,
                metadata: None,
            })
        }
        async fn subscribe_to_task(
            &self,
            _: &ServiceParams,
            _: SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            Ok(Box::pin(stream::empty()))
        }
        async fn create_push_config(
            &self,
            _: &ServiceParams,
            req: CreateTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            Ok(TaskPushNotificationConfig {
                task_id: req.task_id,
                config: req.config,
                tenant: req.tenant,
            })
        }
        async fn get_push_config(
            &self,
            _: &ServiceParams,
            req: GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            Ok(TaskPushNotificationConfig {
                task_id: req.task_id,
                config: PushNotificationConfig {
                    url: "http://cb".into(),
                    id: None,
                    token: None,
                    authentication: None,
                },
                tenant: req.tenant,
            })
        }
        async fn list_push_configs(
            &self,
            _: &ServiceParams,
            req: ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            Ok(ListTaskPushNotificationConfigsResponse {
                configs: vec![TaskPushNotificationConfig {
                    task_id: req.task_id,
                    config: PushNotificationConfig {
                        url: "http://cb".into(),
                        id: None,
                        token: None,
                        authentication: None,
                    },
                    tenant: req.tenant,
                }],
                next_page_token: None,
            })
        }
        async fn delete_push_config(
            &self,
            _: &ServiceParams,
            _: DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            Ok(())
        }
        async fn get_extended_agent_card(
            &self,
            _: &ServiceParams,
            _: GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            Ok(AgentCard {
                name: "test".into(),
                description: "".into(),
                version: "1".into(),
                capabilities: AgentCapabilities::default(),
                supported_interfaces: vec![],
                default_input_modes: vec![],
                default_output_modes: vec![],
                skills: vec![],
                provider: None,
                documentation_url: None,
                icon_url: None,
                security_schemes: None,
                security_requirements: None,
                signatures: None,
            })
        }
    }

    fn make_state(
        handler: Arc<MockHandler>,
    ) -> (
        WebSocketState<MockHandler>,
        mpsc::UnboundedSender<Message>,
        mpsc::UnboundedReceiver<Message>,
    ) {
        let state = WebSocketState { handler };
        let (tx, rx) = mpsc::unbounded_channel();
        (state, tx, rx)
    }

    fn unary_request(method: &str, params: serde_json::Value) -> WebSocketRequestEnvelope {
        WebSocketRequestEnvelope::new(
            JsonRpcId::String("req-1".into()),
            method,
            Some(params),
            None,
        )
    }

    fn typed_params<T: ProtoJsonPayload>(value: &T) -> serde_json::Value {
        protojson_conv::to_value(value).unwrap()
    }

    fn message_request() -> SendMessageRequest {
        SendMessageRequest {
            message: a2a::Message {
                message_id: "m1".to_string(),
                context_id: None,
                task_id: None,
                role: Role::User,
                parts: vec![Part::text("hello")],
                metadata: None,
                extensions: None,
                reference_task_ids: None,
            },
            configuration: None,
            metadata: None,
            tenant: None,
        }
    }

    fn push_config() -> PushNotificationConfig {
        PushNotificationConfig {
            url: "http://cb".to_string(),
            id: Some("cfg-1".to_string()),
            token: None,
            authentication: None,
        }
    }

    async fn recv_response(rx: &mut mpsc::UnboundedReceiver<Message>) -> serde_json::Value {
        let msg = rx.recv().await.unwrap();
        let text = match msg {
            Message::Text(t) => t.to_string(),
            _ => panic!("expected text frame"),
        };
        serde_json::from_str(&text).unwrap()
    }

    #[test]
    fn test_send_response_ok() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let resp = WebSocketResponseEnvelope::success(
            JsonRpcId::String("id".into()),
            serde_json::json!({}),
        );
        assert!(send_response(&tx, resp).is_ok());
    }

    #[test]
    fn test_send_response_closed_channel() {
        let (tx, rx) = mpsc::unbounded_channel::<Message>();
        drop(rx);
        let resp = WebSocketResponseEnvelope::success(
            JsonRpcId::String("id".into()),
            serde_json::json!({}),
        );
        assert!(send_response(&tx, resp).is_err());
    }

    #[test]
    fn test_service_params_from_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-custom", "value1".parse().unwrap());
        headers.insert("x-multi", "value2".parse().unwrap());
        headers.append("x-multi", "value3".parse().unwrap());
        let params = service_params_from_headers(&headers);
        assert_eq!(params.get("x-custom"), Some(&vec!["value1".to_string()]));
        assert_eq!(
            params.get("x-multi"),
            Some(&vec!["value2".to_string(), "value3".to_string()])
        );
    }

    #[test]
    fn test_service_params_from_headers_empty() {
        assert!(service_params_from_headers(&HeaderMap::new()).is_empty());
    }

    #[test]
    fn test_service_params_from_headers_invalid_utf8() {
        let mut headers = HeaderMap::new();
        headers.insert("x-valid", "value".parse().unwrap());
        headers.insert("x-invalid", HeaderValue::from_bytes(&[0xFF, 0xFE]).unwrap());
        let params = service_params_from_headers(&headers);
        assert_eq!(params.get("x-valid"), Some(&vec!["value".to_string()]));
        assert!(params.get("x-invalid").is_none());
    }

    #[test]
    fn test_parse_error() {
        let e = parse_error("bad input");
        assert_eq!(e.code, error_code::PARSE_ERROR);
        assert!(e.message.contains("bad input"));
    }

    #[tokio::test]
    async fn test_handle_unary_get_task_ok() {
        let (state, tx, rx) = make_state(MockHandler::ok());
        let params = typed_params(&GetTaskRequest {
            id: "task-1".to_string(),
            history_length: None,
            tenant: None,
        });
        let req = unary_request(methods::GET_TASK, params);
        let result = handle_unary_request(&state, &ServiceParams::new(), &req).await;
        assert!(result.is_ok());
        drop(tx);
        drop(rx);
    }

    #[tokio::test]
    async fn test_handle_unary_get_task_not_found() {
        let (state, tx, rx) = make_state(MockHandler::ok());
        let params = typed_params(&GetTaskRequest {
            id: "missing".to_string(),
            history_length: None,
            tenant: None,
        });
        let req = unary_request(methods::GET_TASK, params);
        let result = handle_unary_request(&state, &ServiceParams::new(), &req).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::TASK_NOT_FOUND);
        drop(tx);
        drop(rx);
    }

    #[tokio::test]
    async fn test_handle_unary_list_tasks() {
        let (state, tx, rx) = make_state(MockHandler::ok());
        let params = typed_params(&ListTasksRequest {
            context_id: None,
            status: None,
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        });
        let req = unary_request(methods::LIST_TASKS, params);
        assert!(
            handle_unary_request(&state, &ServiceParams::new(), &req)
                .await
                .is_ok()
        );
        drop(tx);
        drop(rx);
    }

    #[tokio::test]
    async fn test_handle_unary_cancel_task() {
        let (state, tx, rx) = make_state(MockHandler::ok());
        let params = typed_params(&CancelTaskRequest {
            id: "task-1".to_string(),
            metadata: None,
            tenant: None,
        });
        let req = unary_request(methods::CANCEL_TASK, params);
        assert!(
            handle_unary_request(&state, &ServiceParams::new(), &req)
                .await
                .is_ok()
        );
        drop(tx);
        drop(rx);
    }

    #[tokio::test]
    async fn test_handle_unary_send_message() {
        let (state, tx, rx) = make_state(MockHandler::ok());
        let req = unary_request(methods::SEND_MESSAGE, typed_params(&message_request()));
        assert!(
            handle_unary_request(&state, &ServiceParams::new(), &req)
                .await
                .is_ok()
        );
        drop(tx);
        drop(rx);
    }

    #[tokio::test]
    async fn test_handle_unary_send_message_fail() {
        let (state, tx, rx) = make_state(MockHandler::failing());
        let req = unary_request(methods::SEND_MESSAGE, typed_params(&message_request()));
        assert!(
            handle_unary_request(&state, &ServiceParams::new(), &req)
                .await
                .is_err()
        );
        drop(tx);
        drop(rx);
    }

    #[tokio::test]
    async fn test_handle_unary_unknown_method() {
        let (state, tx, rx) = make_state(MockHandler::ok());
        let req = unary_request("tasks/unknown", serde_json::json!({}));
        let result = handle_unary_request(&state, &ServiceParams::new(), &req).await;
        assert_eq!(result.unwrap_err().code, error_code::METHOD_NOT_FOUND);
        drop(tx);
        drop(rx);
    }

    #[tokio::test]
    async fn test_handle_unary_get_extended_agent_card() {
        let (state, tx, rx) = make_state(MockHandler::ok());
        let params = typed_params(&GetExtendedAgentCardRequest { tenant: None });
        let req = unary_request(methods::GET_EXTENDED_AGENT_CARD, params);
        assert!(
            handle_unary_request(&state, &ServiceParams::new(), &req)
                .await
                .is_ok()
        );
        drop(tx);
        drop(rx);
    }

    #[tokio::test]
    async fn test_handle_unary_push_config_crud() {
        let (state, tx, rx) = make_state(MockHandler::ok());
        let create_req = unary_request(
            methods::CREATE_PUSH_CONFIG,
            typed_params(&CreateTaskPushNotificationConfigRequest {
                task_id: "t1".to_string(),
                config: push_config(),
                tenant: None,
            }),
        );
        let get_req = unary_request(
            methods::GET_PUSH_CONFIG,
            typed_params(&GetTaskPushNotificationConfigRequest {
                task_id: "t1".to_string(),
                id: "cfg-1".to_string(),
                tenant: None,
            }),
        );
        let list_req = unary_request(
            methods::LIST_PUSH_CONFIGS,
            typed_params(&ListTaskPushNotificationConfigsRequest {
                task_id: "t1".to_string(),
                page_size: None,
                page_token: None,
                tenant: None,
            }),
        );
        let del_req = unary_request(
            methods::DELETE_PUSH_CONFIG,
            typed_params(&DeleteTaskPushNotificationConfigRequest {
                task_id: "t1".to_string(),
                id: "cfg-1".to_string(),
                tenant: None,
            }),
        );
        let p = ServiceParams::new();
        assert!(handle_unary_request(&state, &p, &create_req).await.is_ok());
        assert!(handle_unary_request(&state, &p, &get_req).await.is_ok());
        assert!(handle_unary_request(&state, &p, &list_req).await.is_ok());
        assert!(handle_unary_request(&state, &p, &del_req).await.is_ok());
        drop(tx);
        drop(rx);
    }

    #[tokio::test]
    async fn test_handle_text_frame_invalid_version_sends_error() {
        let (state, tx, mut rx) = make_state(MockHandler::ok());
        let frame = r#"{"jsonrpc":"1.0","id":"x","method":"GetTask","params":{"id":"t1"}}"#;
        handle_text_frame(frame, &state, &ServiceParams::new(), tx).await;
        let val = recv_response(&mut rx).await;
        assert_eq!(val["error"]["code"], error_code::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn test_handle_text_frame_parse_error() {
        let (state, tx, mut rx) = make_state(MockHandler::ok());
        handle_text_frame("not-json", &state, &ServiceParams::new(), tx).await;
        let val = recv_response(&mut rx).await;
        assert_eq!(val["error"]["code"], error_code::PARSE_ERROR);
    }

    #[tokio::test]
    async fn test_handle_text_frame_unary_ok() {
        let (state, tx, mut rx) = make_state(MockHandler::ok());
        let frame = r#"{"jsonrpc":"2.0","id":"x","method":"GetTask","params":{"id":"t1"}}"#;
        handle_text_frame(frame, &state, &ServiceParams::new(), tx).await;
        let val = recv_response(&mut rx).await;
        assert!(val["result"].is_object());
    }

    #[tokio::test]
    async fn test_handle_text_frame_unary_error() {
        let (state, tx, mut rx) = make_state(MockHandler::ok());
        let frame = r#"{"jsonrpc":"2.0","id":"x","method":"GetTask","params":{"id":"missing"}}"#;
        handle_text_frame(frame, &state, &ServiceParams::new(), tx).await;
        let val = recv_response(&mut rx).await;
        assert_eq!(val["error"]["code"], error_code::TASK_NOT_FOUND);
    }

    #[tokio::test]
    async fn test_spawn_streaming_call_ok() {
        let (state, tx, mut rx) = make_state(MockHandler::ok());
        let req = WebSocketRequestEnvelope::new(
            JsonRpcId::String("s1".into()),
            methods::SEND_STREAMING_MESSAGE,
            Some(typed_params(&message_request())),
            None,
        );
        spawn_streaming_call(&state, ServiceParams::new(), req, tx);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let first = recv_response(&mut rx).await;
        assert!(
            first.get("streamEvent").is_some()
                || first.get("streamEnd").is_some()
                || first.get("error").is_some()
        );
    }

    #[tokio::test]
    async fn test_spawn_streaming_call_error() {
        let (state, tx, mut rx) = make_state(MockHandler::failing());
        let req = WebSocketRequestEnvelope::new(
            JsonRpcId::String("s2".into()),
            methods::SEND_STREAMING_MESSAGE,
            Some(typed_params(&message_request())),
            None,
        );
        spawn_streaming_call(&state, ServiceParams::new(), req, tx);
        let val = recv_response(&mut rx).await;
        assert!(val["error"].is_object());
    }

    #[test]
    fn test_invalid_params_error() {
        let error = invalid_params_error("bad param");
        assert_eq!(error.code, error_code::INVALID_PARAMS);
        assert!(error.message.contains("bad param"));
    }
}
