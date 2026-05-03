// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;
use std::sync::Arc;

use a2a::jsonrpc::methods;
use a2a::websocket::{WebSocketRequestEnvelope, WebSocketResponseEnvelope};
use a2a::*;
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use async_trait::async_trait;
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};
use tokio::sync::{Mutex, mpsc, oneshot};
use tokio_stream::wrappers::UnboundedReceiverStream;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::push_config_compat::{
    deserialize_list_task_push_notification_configs_response,
    deserialize_task_push_notification_config,
    serialize_create_task_push_notification_config_request,
};
use crate::transport::{ServiceParams, Transport, TransportFactory};

type UnaryResultTx = oneshot::Sender<Result<serde_json::Value, A2AError>>;
type StreamResultTx = mpsc::UnboundedSender<Result<StreamResponse, A2AError>>;

struct WebSocketConnection {
    writer: mpsc::UnboundedSender<Message>,
    pending_unary: Arc<Mutex<HashMap<String, UnaryResultTx>>>,
    pending_streams: Arc<Mutex<HashMap<String, StreamResultTx>>>,
}

impl WebSocketConnection {
    async fn connect(url: &str) -> Result<Self, A2AError> {
        let request = url.into_client_request().map_err(|e| {
            A2AError::invalid_request(format!("invalid websocket URL '{url}': {e}"))
        })?;
        let (ws_stream, _) = connect_async(request)
            .await
            .map_err(|e| A2AError::internal(format!("failed to connect websocket: {e}")))?;
        let (mut ws_writer, mut ws_reader) = ws_stream.split();

        let (writer_tx, mut writer_rx) = mpsc::unbounded_channel::<Message>();
        let pending_unary = Arc::new(Mutex::new(HashMap::new()));
        let pending_streams = Arc::new(Mutex::new(HashMap::new()));

        tokio::spawn(async move {
            while let Some(msg) = writer_rx.recv().await {
                if ws_writer.send(msg).await.is_err() {
                    break;
                }
            }
            let _ = ws_writer.close().await;
        });

        let reader_pending_unary = pending_unary.clone();
        let reader_pending_streams = pending_streams.clone();
        tokio::spawn(async move {
            while let Some(next_msg) = ws_reader.next().await {
                let msg = match next_msg {
                    Ok(msg) => msg,
                    Err(error) => {
                        fail_all_pending(
                            &reader_pending_unary,
                            &reader_pending_streams,
                            A2AError::internal(format!("websocket read error: {error}")),
                        )
                        .await;
                        return;
                    }
                };

                match msg {
                    Message::Text(text) => {
                        if let Err(error) = route_incoming_frame(
                            &reader_pending_unary,
                            &reader_pending_streams,
                            &text,
                        )
                        .await
                        {
                            fail_all_pending(&reader_pending_unary, &reader_pending_streams, error)
                                .await;
                            return;
                        }
                    }
                    Message::Close(_) => {
                        fail_all_pending(
                            &reader_pending_unary,
                            &reader_pending_streams,
                            A2AError::unsupported_operation("websocket connection closed"),
                        )
                        .await;
                        return;
                    }
                    _ => {}
                }
            }

            fail_all_pending(
                &reader_pending_unary,
                &reader_pending_streams,
                A2AError::unsupported_operation("websocket connection closed"),
            )
            .await;
        });

        Ok(Self {
            writer: writer_tx,
            pending_unary,
            pending_streams,
        })
    }

    fn send(&self, envelope: &WebSocketRequestEnvelope) -> Result<(), A2AError> {
        let payload = serde_json::to_string(envelope)
            .map_err(|e| A2AError::internal(format!("failed to serialize websocket frame: {e}")))?;
        self.writer
            .send(Message::Text(payload))
            .map_err(|_| A2AError::unsupported_operation("websocket connection is not writable"))
    }

    async fn call_unary_value(
        &self,
        method: &str,
        service_params: &ServiceParams,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, A2AError> {
        let id = uuid::Uuid::now_v7().to_string();
        let (tx, rx) = oneshot::channel();
        self.pending_unary.lock().await.insert(id.clone(), tx);

        let envelope = WebSocketRequestEnvelope::new(
            JsonRpcId::String(id.clone()),
            method,
            Some(payload),
            (!service_params.is_empty()).then_some(service_params.clone()),
        );
        if let Err(error) = self.send(&envelope) {
            self.pending_unary.lock().await.remove(&id);
            return Err(error);
        }

        rx.await
            .map_err(|_| A2AError::unsupported_operation("websocket response channel closed"))?
    }

    async fn call_stream(
        &self,
        method: &str,
        service_params: &ServiceParams,
        payload: serde_json::Value,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let id = uuid::Uuid::now_v7().to_string();
        let (stream_tx, stream_rx) = mpsc::unbounded_channel();
        self.pending_streams
            .lock()
            .await
            .insert(id.clone(), stream_tx);

        let envelope = WebSocketRequestEnvelope::new(
            JsonRpcId::String(id.clone()),
            method,
            Some(payload),
            (!service_params.is_empty()).then_some(service_params.clone()),
        );
        if let Err(error) = self.send(&envelope) {
            self.pending_streams.lock().await.remove(&id);
            return Err(error);
        }

        Ok(Box::pin(UnboundedReceiverStream::new(stream_rx)))
    }
}

async fn route_incoming_frame(
    pending_unary: &Arc<Mutex<HashMap<String, UnaryResultTx>>>,
    pending_streams: &Arc<Mutex<HashMap<String, StreamResultTx>>>,
    frame: &str,
) -> Result<(), A2AError> {
    let envelope: WebSocketResponseEnvelope = serde_json::from_str(frame)
        .map_err(|e| A2AError::parse_error(format!("invalid websocket frame: {e}")))?;

    let id = match envelope.id {
        JsonRpcId::String(id) => id,
        _ => {
            return Err(A2AError::invalid_request(
                "websocket response id must be a string",
            ));
        }
    };

    if let Some(stream_event) = envelope.stream_event {
        let event = protojson_conv::from_value::<StreamResponse>(stream_event)
            .map_err(|e| A2AError::internal(format!("failed to parse stream event: {e}")))?;
        let mut streams = pending_streams.lock().await;
        if let Some(tx) = streams.get(&id) {
            if tx.send(Ok(event)).is_err() {
                streams.remove(&id);
            }
        }
        return Ok(());
    }

    if let Some(stream_error) = envelope.stream_error {
        let error = A2AError::new(stream_error.code, stream_error.message);
        if let Some(tx) = pending_streams.lock().await.get(&id) {
            let _ = tx.send(Err(error));
        }
        pending_streams.lock().await.remove(&id);
        return Ok(());
    }

    if envelope.stream_end.unwrap_or(false) {
        pending_streams.lock().await.remove(&id);
        return Ok(());
    }

    let tx = pending_unary.lock().await.remove(&id);
    if let Some(tx) = tx {
        if let Some(err) = envelope.error {
            let _ = tx.send(Err(A2AError::new(err.code, err.message)));
        } else {
            let _ = tx.send(Ok(envelope.result.unwrap_or(serde_json::Value::Null)));
        }
    }

    Ok(())
}

async fn fail_all_pending(
    pending_unary: &Arc<Mutex<HashMap<String, UnaryResultTx>>>,
    pending_streams: &Arc<Mutex<HashMap<String, StreamResultTx>>>,
    error: A2AError,
) {
    let mut unary = pending_unary.lock().await;
    for (_, tx) in unary.drain() {
        let _ = tx.send(Err(error.clone()));
    }
    drop(unary);

    let mut streams = pending_streams.lock().await;
    for (_, tx) in streams.drain() {
        let _ = tx.send(Err(error.clone()));
    }
}

pub struct WebSocketTransport {
    connection: Arc<WebSocketConnection>,
}

impl WebSocketTransport {
    pub async fn connect(url: &str) -> Result<Self, A2AError> {
        Ok(Self {
            connection: Arc::new(WebSocketConnection::connect(url).await?),
        })
    }

    async fn call<Req, Resp>(
        &self,
        params: &ServiceParams,
        method: &str,
        request_params: &Req,
    ) -> Result<Resp, A2AError>
    where
        Req: ProtoJsonPayload,
        Resp: ProtoJsonPayload,
    {
        let payload = protojson_conv::to_value(request_params).map_err(|e| {
            A2AError::internal(format!("failed to serialize request as ProtoJSON: {e}"))
        })?;
        let result = self
            .connection
            .call_unary_value(method, params, payload)
            .await?;
        protojson_conv::from_value(result)
            .map_err(|e| A2AError::internal(format!("failed to deserialize result: {e}")))
    }

    async fn call_value<Req>(
        &self,
        params: &ServiceParams,
        method: &str,
        request_params: &Req,
    ) -> Result<serde_json::Value, A2AError>
    where
        Req: ProtoJsonPayload,
    {
        let payload = protojson_conv::to_value(request_params).map_err(|e| {
            A2AError::internal(format!("failed to serialize request as ProtoJSON: {e}"))
        })?;
        self.connection
            .call_unary_value(method, params, payload)
            .await
    }

    async fn call_streaming<Req>(
        &self,
        params: &ServiceParams,
        method: &str,
        request_params: &Req,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError>
    where
        Req: ProtoJsonPayload,
    {
        let payload = protojson_conv::to_value(request_params).map_err(|e| {
            A2AError::internal(format!("failed to serialize request as ProtoJSON: {e}"))
        })?;
        self.connection.call_stream(method, params, payload).await
    }
}

#[async_trait]
impl Transport for WebSocketTransport {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        self.call(params, methods::SEND_MESSAGE, req).await
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.call_streaming(params, methods::SEND_STREAMING_MESSAGE, req)
            .await
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: &GetTaskRequest,
    ) -> Result<Task, A2AError> {
        self.call(params, methods::GET_TASK, req).await
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        self.call(params, methods::LIST_TASKS, req).await
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: &CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        self.call(params, methods::CANCEL_TASK, req).await
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: &SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.call_streaming(params, methods::SUBSCRIBE_TO_TASK, req)
            .await
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: &CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let payload = serialize_create_task_push_notification_config_request(req)?;
        let value = self
            .connection
            .call_unary_value(methods::CREATE_PUSH_CONFIG, params, payload)
            .await?;
        deserialize_task_push_notification_config(value)
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let result = self
            .call_value(params, methods::GET_PUSH_CONFIG, req)
            .await?;
        deserialize_task_push_notification_config(result)
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let result = self
            .call_value(params, methods::LIST_PUSH_CONFIGS, req)
            .await?;
        deserialize_list_task_push_notification_configs_response(result)
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let _ = self
            .call_value(params, methods::DELETE_PUSH_CONFIG, req)
            .await?;
        Ok(())
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: &GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        self.call(params, methods::GET_EXTENDED_AGENT_CARD, req)
            .await
    }

    async fn destroy(&self) -> Result<(), A2AError> {
        let _ = self.connection.writer.send(Message::Close(None));
        Ok(())
    }
}

pub struct WebSocketTransportFactory;

impl WebSocketTransportFactory {
    pub fn new() -> Self {
        Self
    }
}

impl Default for WebSocketTransportFactory {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TransportFactory for WebSocketTransportFactory {
    fn protocol(&self) -> &str {
        TRANSPORT_PROTOCOL_WEBSOCKET
    }

    async fn create(
        &self,
        _card: &AgentCard,
        iface: &AgentInterface,
    ) -> Result<Box<dyn Transport>, A2AError> {
        if !(iface.url.starts_with("ws://") || iface.url.starts_with("wss://")) {
            return Err(A2AError::invalid_request(format!(
                "websocket transport requires ws:// or wss:// URL, got '{}'",
                iface.url
            )));
        }
        let transport = WebSocketTransport::connect(&iface.url).await?;
        Ok(Box::new(transport))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::jsonrpc::JsonRpcId;

    fn test_connection() -> (WebSocketConnection, mpsc::UnboundedReceiver<Message>) {
        let (writer, rx) = mpsc::unbounded_channel();
        (
            WebSocketConnection {
                writer,
                pending_unary: Arc::new(Mutex::new(HashMap::new())),
                pending_streams: Arc::new(Mutex::new(HashMap::new())),
            },
            rx,
        )
    }

    fn closed_connection() -> WebSocketConnection {
        let (writer, rx) = mpsc::unbounded_channel();
        drop(rx);
        WebSocketConnection {
            writer,
            pending_unary: Arc::new(Mutex::new(HashMap::new())),
            pending_streams: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[test]
    fn test_factory_protocol() {
        let factory = WebSocketTransportFactory::new();
        assert_eq!(factory.protocol(), TRANSPORT_PROTOCOL_WEBSOCKET);
    }

    #[test]
    fn test_factory_default() {
        let factory = WebSocketTransportFactory::default();
        assert_eq!(factory.protocol(), TRANSPORT_PROTOCOL_WEBSOCKET);
    }

    #[test]
    fn test_connection_send_writes_text_frame() {
        let (connection, mut rx) = test_connection();
        let envelope = WebSocketRequestEnvelope::new(
            JsonRpcId::String("id".into()),
            methods::GET_TASK,
            Some(serde_json::json!({"id": "task-1"})),
            None,
        );

        connection.send(&envelope).unwrap();

        let sent = rx.try_recv().unwrap();
        let text = sent.into_text().unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], "id");
        assert_eq!(value["method"], methods::GET_TASK);
    }

    #[test]
    fn test_connection_send_closed_writer() {
        let connection = closed_connection();
        let envelope = WebSocketRequestEnvelope::new(
            JsonRpcId::String("id".into()),
            methods::GET_TASK,
            None,
            None,
        );

        let err = connection.send(&envelope).unwrap_err();
        assert_eq!(err.code, error_code::UNSUPPORTED_OPERATION);
    }

    #[tokio::test]
    async fn test_call_unary_value_sends_service_params_and_receives_result() {
        let (connection, mut rx) = test_connection();
        let mut params = ServiceParams::new();
        params.insert("x-test".to_string(), vec!["value".to_string()]);
        let pending = connection.pending_unary.clone();

        let call = tokio::spawn(async move {
            connection
                .call_unary_value(
                    methods::GET_TASK,
                    &params,
                    serde_json::json!({"id": "task-1"}),
                )
                .await
        });

        let sent = rx.recv().await.unwrap();
        let text = sent.into_text().unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let id = value["id"].as_str().unwrap().to_string();
        assert_eq!(value["serviceParams"]["x-test"][0], "value");

        let response = WebSocketResponseEnvelope::success(
            JsonRpcId::String(id),
            serde_json::json!({"id": "task-1"}),
        );
        let frame = serde_json::to_string(&response).unwrap();
        route_incoming_frame(&pending, &Arc::new(Mutex::new(HashMap::new())), &frame)
            .await
            .unwrap();

        let result = call.await.unwrap().unwrap();
        assert_eq!(result["id"], "task-1");
    }

    #[tokio::test]
    async fn test_call_unary_value_removes_pending_on_send_error() {
        let connection = closed_connection();
        let err = connection
            .call_unary_value(
                methods::GET_TASK,
                &ServiceParams::new(),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();

        assert_eq!(err.code, error_code::UNSUPPORTED_OPERATION);
        assert!(connection.pending_unary.lock().await.is_empty());
    }

    #[tokio::test]
    async fn test_call_stream_sends_frame_and_stream_end_removes_pending() {
        let (connection, mut rx) = test_connection();
        let pending_streams = connection.pending_streams.clone();
        let stream = connection
            .call_stream(
                methods::SUBSCRIBE_TO_TASK,
                &ServiceParams::new(),
                serde_json::json!({"id": "task-1"}),
            )
            .await
            .unwrap();

        let sent = rx.recv().await.unwrap();
        let text = sent.into_text().unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let id = value["id"].as_str().unwrap().to_string();
        assert_eq!(value["method"], methods::SUBSCRIBE_TO_TASK);
        assert!(pending_streams.lock().await.contains_key(&id));

        let response = WebSocketResponseEnvelope::stream_end(JsonRpcId::String(id.clone()));
        let frame = serde_json::to_string(&response).unwrap();
        route_incoming_frame(
            &Arc::new(Mutex::new(HashMap::new())),
            &pending_streams,
            &frame,
        )
        .await
        .unwrap();

        assert!(!pending_streams.lock().await.contains_key(&id));
        drop(stream);
    }

    #[tokio::test]
    async fn test_call_stream_removes_pending_on_send_error() {
        let connection = closed_connection();
        let result = connection
            .call_stream(
                methods::SUBSCRIBE_TO_TASK,
                &ServiceParams::new(),
                serde_json::json!({}),
            )
            .await;
        let err = match result {
            Ok(_) => panic!("expected call_stream to fail"),
            Err(err) => err,
        };

        assert_eq!(err.code, error_code::UNSUPPORTED_OPERATION);
        assert!(connection.pending_streams.lock().await.is_empty());
    }

    #[tokio::test]
    async fn test_route_incoming_frame_success() {
        let pending_unary = Arc::new(Mutex::new(HashMap::new()));
        let pending_streams = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending_unary.lock().await.insert("test-id".to_string(), tx);

        let response = WebSocketResponseEnvelope::success(
            JsonRpcId::String("test-id".into()),
            serde_json::json!({"result": "ok"}),
        );
        let frame = serde_json::to_string(&response).unwrap();
        route_incoming_frame(&pending_unary, &pending_streams, &frame)
            .await
            .unwrap();

        let result = rx.await.unwrap().unwrap();
        assert_eq!(result, serde_json::json!({"result": "ok"}));
    }

    #[tokio::test]
    async fn test_route_incoming_frame_error() {
        let pending_unary = Arc::new(Mutex::new(HashMap::new()));
        let pending_streams = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = oneshot::channel();
        pending_unary.lock().await.insert("test-id".to_string(), tx);

        let error = JsonRpcError {
            code: -32600,
            message: "Invalid request".to_string(),
            data: None,
        };
        let response = WebSocketResponseEnvelope::error(JsonRpcId::String("test-id".into()), error);
        let frame = serde_json::to_string(&response).unwrap();
        route_incoming_frame(&pending_unary, &pending_streams, &frame)
            .await
            .unwrap();

        let result = rx.await.unwrap().unwrap_err();
        assert_eq!(result.code, -32600);
    }

    #[tokio::test]
    async fn test_route_incoming_frame_invalid_json() {
        let pending_unary = Arc::new(Mutex::new(HashMap::new()));
        let pending_streams = Arc::new(Mutex::new(HashMap::new()));

        let result = route_incoming_frame(&pending_unary, &pending_streams, "invalid json").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::PARSE_ERROR);
    }

    #[tokio::test]
    async fn test_route_incoming_frame_non_string_id() {
        let pending_unary = Arc::new(Mutex::new(HashMap::new()));
        let pending_streams = Arc::new(Mutex::new(HashMap::new()));

        let response =
            WebSocketResponseEnvelope::success(JsonRpcId::Number(123), serde_json::json!({}));
        let frame = serde_json::to_string(&response).unwrap();
        let result = route_incoming_frame(&pending_unary, &pending_streams, &frame).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn test_fail_all_pending() {
        let pending_unary = Arc::new(Mutex::new(HashMap::new()));
        let pending_streams = Arc::new(Mutex::new(HashMap::new()));
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        let (stream_tx, mut stream_rx) = mpsc::unbounded_channel();

        pending_unary.lock().await.insert("id1".to_string(), tx1);
        pending_unary.lock().await.insert("id2".to_string(), tx2);
        pending_streams
            .lock()
            .await
            .insert("stream-id".to_string(), stream_tx);

        let error = A2AError::internal("test error");
        fail_all_pending(&pending_unary, &pending_streams, error.clone()).await;

        assert!(rx1.await.unwrap().unwrap_err().code == error.code);
        assert!(rx2.await.unwrap().unwrap_err().code == error.code);
        assert!(stream_rx.recv().await.unwrap().unwrap_err().code == error.code);
    }

    #[tokio::test]
    async fn test_route_incoming_frame_stream_end() {
        let pending_streams = Arc::new(Mutex::new(HashMap::new()));
        let (tx, _rx) = mpsc::unbounded_channel();
        pending_streams
            .lock()
            .await
            .insert("test-id".to_string(), tx);

        let response = WebSocketResponseEnvelope::stream_end(JsonRpcId::String("test-id".into()));
        let frame = serde_json::to_string(&response).unwrap();
        route_incoming_frame(
            &Arc::new(Mutex::new(HashMap::new())),
            &pending_streams,
            &frame,
        )
        .await
        .unwrap();

        assert!(pending_streams.lock().await.get("test-id").is_none());
    }

    #[tokio::test]
    async fn test_route_incoming_frame_stream_error() {
        let pending_streams = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        pending_streams
            .lock()
            .await
            .insert("test-id".to_string(), tx);
        let error = JsonRpcError {
            code: error_code::INTERNAL_ERROR,
            message: "stream failed".to_string(),
            data: None,
        };
        let response =
            WebSocketResponseEnvelope::stream_error(JsonRpcId::String("test-id".into()), error);
        let frame = serde_json::to_string(&response).unwrap();

        route_incoming_frame(
            &Arc::new(Mutex::new(HashMap::new())),
            &pending_streams,
            &frame,
        )
        .await
        .unwrap();

        let err = rx.recv().await.unwrap().unwrap_err();
        assert_eq!(err.code, error_code::INTERNAL_ERROR);
        assert!(pending_streams.lock().await.get("test-id").is_none());
    }

    #[tokio::test]
    async fn test_route_incoming_frame_stream_event_drops_closed_sender() {
        let pending_streams = Arc::new(Mutex::new(HashMap::new()));
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        pending_streams
            .lock()
            .await
            .insert("test-id".to_string(), tx);
        let event = StreamResponse::Task(Task {
            id: "task-1".to_string(),
            context_id: "ctx".to_string(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        });
        let response = WebSocketResponseEnvelope::stream_event(
            JsonRpcId::String("test-id".into()),
            protojson_conv::to_value(&event).unwrap(),
        );
        let frame = serde_json::to_string(&response).unwrap();

        route_incoming_frame(
            &Arc::new(Mutex::new(HashMap::new())),
            &pending_streams,
            &frame,
        )
        .await
        .unwrap();

        assert!(pending_streams.lock().await.get("test-id").is_none());
    }
}
