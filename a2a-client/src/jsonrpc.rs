// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use reqwest::Client;
use serde_json::Value;

use crate::transport::{ServiceParams, Transport, TransportFactory};

/// JSON-RPC transport implementation.
///
/// Sends all requests as JSON-RPC 2.0 POSTs to a single endpoint.
/// Streaming responses are received via Server-Sent Events (SSE).
pub struct JsonRpcTransport {
    client: Client,
    endpoint: String,
}

impl JsonRpcTransport {
    pub fn new(client: Client, endpoint: String) -> Self {
        JsonRpcTransport { client, endpoint }
    }

    async fn call<T: serde::de::DeserializeOwned>(
        &self,
        params: &ServiceParams,
        method: &str,
        request_params: Value,
    ) -> Result<T, A2AError> {
        let id = JsonRpcId::String(uuid::Uuid::now_v7().to_string());
        let rpc_request = JsonRpcRequest::new(id, method, Some(request_params));

        let mut builder = self.client.post(&self.endpoint);
        for (key, values) in params {
            for v in values {
                builder = builder.header(key, v);
            }
        }

        let response = builder
            .json(&rpc_request)
            .send()
            .await
            .map_err(|e| A2AError::internal(format!("HTTP request failed: {e}")))?;

        let rpc_response: JsonRpcResponse = response
            .json()
            .await
            .map_err(|e| A2AError::internal(format!("failed to parse JSON-RPC response: {e}")))?;

        if let Some(err) = rpc_response.error {
            return Err(A2AError::new(err.code, err.message));
        }

        let result = rpc_response
            .result
            .ok_or_else(|| A2AError::internal("JSON-RPC response missing result"))?;

        serde_json::from_value(result)
            .map_err(|e| A2AError::internal(format!("failed to deserialize result: {e}")))
    }

    async fn call_streaming(
        &self,
        params: &ServiceParams,
        method: &str,
        request_params: Value,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let id = JsonRpcId::String(uuid::Uuid::now_v7().to_string());
        let rpc_request = JsonRpcRequest::new(id, method, Some(request_params));

        let mut builder = self
            .client
            .post(&self.endpoint)
            .header("Accept", "text/event-stream");
        for (key, values) in params {
            for v in values {
                builder = builder.header(key, v);
            }
        }

        let response = builder
            .json(&rpc_request)
            .send()
            .await
            .map_err(|e| A2AError::internal(format!("HTTP request failed: {e}")))?;

        let stream = response.bytes_stream();
        let event_stream = parse_sse_stream(stream);
        Ok(event_stream)
    }
}

/// Parse an SSE byte stream into StreamResponse events.
fn parse_sse_stream(
    stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    let mapped = stream::unfold(
        (Box::pin(stream), String::new()),
        |(mut stream, mut buf)| async move {
            loop {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                        // Process complete SSE events (double newline terminated)
                        while let Some(pos) = buf.find(
                            "

",
                        ) {
                            let event_text = buf[..pos].to_string();
                            buf = buf[pos + 2..].to_string();

                            // Extract data from SSE event
                            let mut data = String::new();
                            for line in event_text.lines() {
                                if let Some(d) = line.strip_prefix("data: ") {
                                    if !data.is_empty() {
                                        data.push('\n');
                                    }
                                    data.push_str(d);
                                } else if let Some(d) = line.strip_prefix("data:") {
                                    if !data.is_empty() {
                                        data.push('\n');
                                    }
                                    data.push_str(d);
                                }
                            }

                            if data.is_empty() {
                                continue;
                            }

                            // Parse as JSON-RPC response containing a StreamResponse
                            match serde_json::from_str::<JsonRpcResponse>(&data) {
                                Ok(rpc_resp) => {
                                    if let Some(err) = rpc_resp.error {
                                        return Some((
                                            Err(A2AError::new(err.code, err.message)),
                                            (stream, buf),
                                        ));
                                    }
                                    if let Some(result) = rpc_resp.result {
                                        match serde_json::from_value::<StreamResponse>(result) {
                                            Ok(sr) => return Some((Ok(sr), (stream, buf))),
                                            Err(e) => {
                                                return Some((
                                                    Err(A2AError::internal(format!(
                                                        "SSE parse error: {e}"
                                                    ))),
                                                    (stream, buf),
                                                ));
                                            }
                                        }
                                    }
                                }
                                Err(_) => {
                                    // Try parsing directly as StreamResponse
                                    match serde_json::from_str::<StreamResponse>(&data) {
                                        Ok(sr) => return Some((Ok(sr), (stream, buf))),
                                        Err(e) => {
                                            return Some((
                                                Err(A2AError::internal(format!(
                                                    "SSE parse error: {e}"
                                                ))),
                                                (stream, buf),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        return Some((
                            Err(A2AError::internal(format!("SSE stream error: {e}"))),
                            (stream, buf),
                        ));
                    }
                    None => return None,
                }
            }
        },
    );

    Box::pin(mapped)
}

/// Parse an SSE byte stream into StreamResponse events (REST binding).
/// Unlike the JSON-RPC variant, this expects data lines to contain
/// raw StreamResponse JSON (not wrapped in a JSON-RPC envelope).
pub(crate) fn parse_sse_stream_rest(
    stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    let mapped = stream::unfold(
        (Box::pin(stream), String::new()),
        |(mut stream, mut buf)| async move {
            loop {
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        buf.push_str(&String::from_utf8_lossy(&chunk));
                        while let Some(pos) = buf.find(
                            "

",
                        ) {
                            let event_text = buf[..pos].to_string();
                            buf = buf[pos + 2..].to_string();

                            let mut data = String::new();
                            for line in event_text.lines() {
                                if let Some(d) = line.strip_prefix("data: ") {
                                    if !data.is_empty() {
                                        data.push('\n');
                                    }
                                    data.push_str(d);
                                } else if let Some(d) = line.strip_prefix("data:") {
                                    if !data.is_empty() {
                                        data.push('\n');
                                    }
                                    data.push_str(d);
                                }
                            }

                            if data.is_empty() {
                                continue;
                            }

                            match serde_json::from_str::<StreamResponse>(&data) {
                                Ok(sr) => return Some((Ok(sr), (stream, buf))),
                                Err(e) => {
                                    return Some((
                                        Err(A2AError::internal(format!("SSE parse error: {e}"))),
                                        (stream, buf),
                                    ));
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        return Some((
                            Err(A2AError::internal(format!("SSE stream error: {e}"))),
                            (stream, buf),
                        ));
                    }
                    None => return None,
                }
            }
        },
    );

    Box::pin(mapped)
}

#[async_trait]
impl Transport for JsonRpcTransport {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let p = serde_json::to_value(req).map_err(|e| A2AError::internal(e.to_string()))?;
        self.call(params, methods::SEND_MESSAGE, p).await
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let p = serde_json::to_value(req).map_err(|e| A2AError::internal(e.to_string()))?;
        self.call_streaming(params, methods::SEND_STREAMING_MESSAGE, p)
            .await
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: &GetTaskRequest,
    ) -> Result<Task, A2AError> {
        let p = serde_json::to_value(req).map_err(|e| A2AError::internal(e.to_string()))?;
        self.call(params, methods::GET_TASK, p).await
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        let p = serde_json::to_value(req).map_err(|e| A2AError::internal(e.to_string()))?;
        self.call(params, methods::LIST_TASKS, p).await
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: &CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let p = serde_json::to_value(req).map_err(|e| A2AError::internal(e.to_string()))?;
        self.call(params, methods::CANCEL_TASK, p).await
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: &SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let p = serde_json::to_value(req).map_err(|e| A2AError::internal(e.to_string()))?;
        self.call_streaming(params, methods::SUBSCRIBE_TO_TASK, p)
            .await
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: &CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let p = serde_json::to_value(req).map_err(|e| A2AError::internal(e.to_string()))?;
        self.call(params, methods::CREATE_PUSH_CONFIG, p).await
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let p = serde_json::to_value(req).map_err(|e| A2AError::internal(e.to_string()))?;
        self.call(params, methods::GET_PUSH_CONFIG, p).await
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let p = serde_json::to_value(req).map_err(|e| A2AError::internal(e.to_string()))?;
        self.call(params, methods::LIST_PUSH_CONFIGS, p).await
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let id = JsonRpcId::String(uuid::Uuid::now_v7().to_string());
        let request_params =
            serde_json::to_value(req).map_err(|e| A2AError::internal(e.to_string()))?;
        let rpc_request =
            JsonRpcRequest::new(id, methods::DELETE_PUSH_CONFIG, Some(request_params));

        let mut builder = self.client.post(&self.endpoint);
        for (key, values) in params {
            for v in values {
                builder = builder.header(key, v);
            }
        }

        let response = builder
            .json(&rpc_request)
            .send()
            .await
            .map_err(|e| A2AError::internal(format!("HTTP request failed: {e}")))?;

        let rpc_response: JsonRpcResponse = response
            .json()
            .await
            .map_err(|e| A2AError::internal(format!("failed to parse JSON-RPC response: {e}")))?;

        if let Some(err) = rpc_response.error {
            return Err(A2AError::new(err.code, err.message));
        }

        Ok(())
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: &GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        let p = serde_json::to_value(req).map_err(|e| A2AError::internal(e.to_string()))?;
        self.call(params, methods::GET_EXTENDED_AGENT_CARD, p).await
    }

    async fn destroy(&self) -> Result<(), A2AError> {
        Ok(())
    }
}

/// Factory for creating [`JsonRpcTransport`] instances.
pub struct JsonRpcTransportFactory {
    client: Client,
}

impl JsonRpcTransportFactory {
    pub fn new(client: Option<Client>) -> Self {
        JsonRpcTransportFactory {
            client: client.unwrap_or_default(),
        }
    }
}

#[async_trait]
impl TransportFactory for JsonRpcTransportFactory {
    fn protocol(&self) -> &str {
        TRANSPORT_PROTOCOL_JSONRPC
    }

    async fn create(
        &self,
        _card: &AgentCard,
        iface: &AgentInterface,
    ) -> Result<Box<dyn Transport>, A2AError> {
        Ok(Box::new(JsonRpcTransport::new(
            self.client.clone(),
            iface.url.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    /// Helper: build an SSE byte stream from raw text chunks.
    fn byte_stream(
        chunks: Vec<String>,
    ) -> impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static {
        stream::iter(
            chunks
                .into_iter()
                .map(|s| Ok(bytes::Bytes::from(s)))
                .collect::<Vec<_>>(),
        )
    }

    #[tokio::test]
    async fn test_parse_sse_stream_jsonrpc_envelope() {
        // Build a JSON-RPC response wrapping a StreamResponse (StatusUpdate)
        let status_update = TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        };
        let sr = StreamResponse::StatusUpdate(status_update);
        let result_val = serde_json::to_value(&sr).unwrap();
        let rpc_resp = JsonRpcResponse::success(JsonRpcId::Number(1), result_val);
        let data = serde_json::to_string(&rpc_resp).unwrap();
        let sse_text = format!("data: {}\n\n", data);

        let stream = byte_stream(vec![sse_text]);
        let mut parsed = parse_sse_stream(stream);
        let item = parsed.next().await.unwrap().unwrap();
        assert!(matches!(item, StreamResponse::StatusUpdate(_)));
        assert!(parsed.next().await.is_none());
    }

    #[tokio::test]
    async fn test_parse_sse_stream_jsonrpc_error() {
        let rpc_resp = JsonRpcResponse::error(
            JsonRpcId::Number(1),
            JsonRpcError {
                code: -32600,
                message: "bad request".into(),
                data: None,
            },
        );
        let data = serde_json::to_string(&rpc_resp).unwrap();
        let sse_text = format!("data: {}\n\n", data);

        let stream = byte_stream(vec![sse_text]);
        let mut parsed = parse_sse_stream(stream);
        let item = parsed.next().await.unwrap();
        assert!(item.is_err());
    }

    #[tokio::test]
    async fn test_parse_sse_stream_direct_stream_response() {
        // When the data is not a valid JSON-RPC envelope, try parsing directly as StreamResponse
        let status_update = TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            metadata: None,
        };
        let sr = StreamResponse::StatusUpdate(status_update);
        let data = serde_json::to_string(&sr).unwrap();
        let sse_text = format!("data: {}\n\n", data);

        let stream = byte_stream(vec![sse_text]);
        let mut parsed = parse_sse_stream(stream);
        let item = parsed.next().await.unwrap().unwrap();
        assert!(matches!(item, StreamResponse::StatusUpdate(_)));
    }

    #[tokio::test]
    async fn test_parse_sse_stream_rest_ok() {
        let status_update = TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        };
        let sr = StreamResponse::StatusUpdate(status_update);
        let data = serde_json::to_string(&sr).unwrap();
        let sse_text = format!("data: {}\n\n", data);

        let stream = byte_stream(vec![sse_text]);
        let mut parsed = parse_sse_stream_rest(stream);
        let item = parsed.next().await.unwrap().unwrap();
        assert!(matches!(item, StreamResponse::StatusUpdate(_)));
    }

    #[tokio::test]
    async fn test_parse_sse_stream_rest_parse_error() {
        let sse_text = "data: not-valid-json\n\n".to_string();
        let stream = byte_stream(vec![sse_text]);
        let mut parsed = parse_sse_stream_rest(stream);
        let item = parsed.next().await.unwrap();
        assert!(item.is_err());
    }

    #[tokio::test]
    async fn test_parse_sse_stream_empty_data_lines_skipped() {
        // Event with no data: lines should be skipped
        let sr = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        let data = serde_json::to_string(&sr).unwrap();
        // First event has no data, second has data
        let sse_text = format!("event: ping\n\ndata: {}\n\n", data);

        let stream = byte_stream(vec![sse_text]);
        let mut parsed = parse_sse_stream_rest(stream);
        let item = parsed.next().await.unwrap().unwrap();
        assert!(matches!(item, StreamResponse::StatusUpdate(_)));
    }

    #[tokio::test]
    async fn test_parse_sse_stream_chunked_delivery() {
        // Data arrives split across multiple chunks
        let sr = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        let data = serde_json::to_string(&sr).unwrap();
        let full = format!("data: {}\n\n", data);
        let mid = full.len() / 2;
        let chunk1 = full[..mid].to_string();
        let chunk2 = full[mid..].to_string();

        let stream = byte_stream(vec![chunk1, chunk2]);
        let mut parsed = parse_sse_stream_rest(stream);
        let item = parsed.next().await.unwrap().unwrap();
        assert!(matches!(item, StreamResponse::StatusUpdate(_)));
    }

    #[tokio::test]
    async fn test_parse_sse_stream_data_no_space() {
        // data:VALUE (no space after colon) is valid SSE
        let sr = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        let data = serde_json::to_string(&sr).unwrap();
        let sse_text = format!("data:{}\n\n", data);

        let stream = byte_stream(vec![sse_text]);
        let mut parsed = parse_sse_stream_rest(stream);
        let item = parsed.next().await.unwrap().unwrap();
        assert!(matches!(item, StreamResponse::StatusUpdate(_)));
    }

    #[tokio::test]
    async fn test_parse_sse_stream_multiple_events_separate_chunks() {
        let make_sr = |state| {
            StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: "t1".into(),
                context_id: "c1".into(),
                status: TaskStatus {
                    state,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            })
        };
        let sr1 = serde_json::to_string(&make_sr(TaskState::Working)).unwrap();
        let sr2 = serde_json::to_string(&make_sr(TaskState::Completed)).unwrap();
        let chunk1 = format!("data: {}\n\n", sr1);
        let chunk2 = format!("data: {}\n\n", sr2);

        let stream = byte_stream(vec![chunk1, chunk2]);
        let items: Vec<_> = parse_sse_stream_rest(stream).collect().await;
        assert_eq!(items.len(), 2);
        assert!(items[0].is_ok());
        assert!(items[1].is_ok());
    }

    #[test]
    fn test_jsonrpc_transport_new() {
        let t = JsonRpcTransport::new(Client::new(), "http://localhost:8080".into());
        assert_eq!(t.endpoint, "http://localhost:8080");
    }

    #[test]
    fn test_jsonrpc_transport_factory() {
        let f = JsonRpcTransportFactory::new(None);
        assert_eq!(f.protocol(), "JSONRPC");
    }

    #[tokio::test]
    async fn test_jsonrpc_transport_factory_create() {
        let f = JsonRpcTransportFactory::new(None);
        let card = AgentCard {
            name: "Test".into(),
            description: "Test".into(),
            version: "1.0".into(),
            supported_interfaces: vec![],
            capabilities: AgentCapabilities::default(),
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        };
        let iface = AgentInterface::new("http://localhost:8080", "JSONRPC");
        let transport = f.create(&card, &iface).await.unwrap();
        // Just verify it was created (it's a real transport but we can't call it without a server)
        transport.destroy().await.unwrap();
    }
}
