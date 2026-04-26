// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use reqwest::Client;
use std::collections::VecDeque;

use crate::push_config_compat::{
    deserialize_list_task_push_notification_configs_response,
    deserialize_task_push_notification_config,
    serialize_create_task_push_notification_config_request,
};
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

    async fn call_value_with_payload(
        &self,
        params: &ServiceParams,
        method: &str,
        payload: serde_json::Value,
    ) -> Result<serde_json::Value, A2AError> {
        let id = JsonRpcId::String(uuid::Uuid::now_v7().to_string());
        let rpc_request = JsonRpcRequest::new(id, method, Some(payload));

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

        rpc_response
            .result
            .ok_or_else(|| A2AError::internal("JSON-RPC response missing result"))
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

        self.call_value_with_payload(params, method, payload).await
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
        let result = self.call_value(params, method, request_params).await?;

        protojson_conv::from_value(result)
            .map_err(|e| A2AError::internal(format!("failed to deserialize result: {e}")))
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
        let id = JsonRpcId::String(uuid::Uuid::now_v7().to_string());
        let payload = protojson_conv::to_value(request_params).map_err(|e| {
            A2AError::internal(format!("failed to serialize request as ProtoJSON: {e}"))
        })?;
        let rpc_request = JsonRpcRequest::new(id, method, Some(payload));

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

/// Find the end of an SSE event boundary in a byte buffer.
///
/// SSE line terminators per https://html.spec.whatwg.org/multipage/server-sent-events.html#event-stream-interpretation:
/// `\n\n`, `\r\r`, and `\r\n\r\n` are all valid event separators.
fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some((i, i + 2));
        }
        if buf[i] == b'\r' && buf[i + 1] == b'\r' {
            return Some((i, i + 2));
        }
        if i + 3 < buf.len() && &buf[i..i + 4] == b"\r\n\r\n" {
            return Some((i, i + 4));
        }
    }
    None
}

/// Shared SSE byte-stream parser that abstracts over the event-parse callback.
///
/// `parse_event` is invoked on each complete `\n\n`/`\r\r`/`\r\n\r\n`-delimited
/// event; it should return `Some(result)` to emit the event or `None` to skip it.
fn parse_sse_bytes<F>(
    stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
    parse_event: F,
) -> BoxStream<'static, Result<StreamResponse, A2AError>>
where
    F: Fn(&str) -> Option<Result<StreamResponse, A2AError>> + Clone + Send + 'static,
{
    let mapped = stream::unfold(
        (
            Box::pin(stream),
            Vec::<u8>::new(),
            VecDeque::new(),
            parse_event,
        ),
        |(mut stream, mut buf, mut pending, parse_event)| async move {
            loop {
                // Drain already-parsed events before reading more bytes.
                // Without this, a single chunk carrying N complete SSE events
                // would only deliver the first; the remaining N-1 would be
                // silently dropped if the connection closed (burst-then-close).
                if let Some(item) = pending.pop_front() {
                    return Some((item, (stream, buf, pending, parse_event)));
                }
                match stream.next().await {
                    Some(Ok(chunk)) => {
                        // Keep the buffer as raw bytes; defer UTF-8 decoding
                        // until an event boundary is found. Per-chunk
                        // `from_utf8_lossy` would silently replace multi-byte
                        // characters split across chunk boundaries with U+FFFD.
                        buf.extend_from_slice(&chunk);
                        while let Some((start, end)) = find_event_boundary(&buf) {
                            let event_bytes: Vec<u8> = buf.drain(..end).collect();
                            let event_text = match std::str::from_utf8(&event_bytes[..start]) {
                                Ok(s) => s,
                                Err(e) => {
                                    pending.push_back(Err(A2AError::internal(format!(
                                        "SSE UTF-8 decode error: {e}"
                                    ))));
                                    continue;
                                }
                            };

                            if let Some(result) = parse_event(event_text) {
                                pending.push_back(result);
                            }
                        }
                    }
                    Some(Err(e)) => {
                        pending
                            .push_back(Err(A2AError::internal(format!("SSE stream error: {e}"))));
                    }
                    None => return None,
                }
            }
        },
    );

    Box::pin(mapped)
}

/// Parse an SSE byte stream into StreamResponse events (JSON-RPC envelope).
fn parse_sse_stream(
    stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    parse_sse_bytes(stream, |event_text| {
        // Extract `data:` lines from the SSE event
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
            return None;
        }

        // Try JSON-RPC envelope first
        match serde_json::from_str::<JsonRpcResponse>(&data) {
            Ok(rpc_resp) => {
                if let Some(err) = rpc_resp.error {
                    return Some(Err(A2AError::new(err.code, err.message)));
                }
                if let Some(result) = rpc_resp.result {
                    match protojson_conv::from_value::<StreamResponse>(result) {
                        Ok(sr) => Some(Ok(sr)),
                        Err(e) => Some(Err(A2AError::internal(format!("SSE parse error: {e}")))),
                    }
                } else {
                    None
                }
            }
            Err(_) => {
                // Fallback: parse directly as StreamResponse
                match protojson_conv::from_str::<StreamResponse>(&data) {
                    Ok(sr) => Some(Ok(sr)),
                    Err(e) => Some(Err(A2AError::internal(format!("SSE parse error: {e}")))),
                }
            }
        }
    })
}

/// Parse an SSE byte stream into StreamResponse events (REST binding).
/// Unlike the JSON-RPC variant, this expects data lines to contain
/// raw StreamResponse JSON (not wrapped in a JSON-RPC envelope).
pub(crate) fn parse_sse_stream_rest(
    stream: impl futures::Stream<Item = Result<bytes::Bytes, reqwest::Error>> + Send + 'static,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    parse_sse_bytes(stream, |event_text| {
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
            return None;
        }

        match protojson_conv::from_str::<StreamResponse>(&data) {
            Ok(sr) => Some(Ok(sr)),
            Err(e) => Some(Err(A2AError::internal(format!("SSE parse error: {e}")))),
        }
    })
}

#[async_trait]
impl Transport for JsonRpcTransport {
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
        let result = self
            .call_value_with_payload(params, methods::CREATE_PUSH_CONFIG, payload)
            .await?;
        deserialize_task_push_notification_config(result)
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
        let id = JsonRpcId::String(uuid::Uuid::now_v7().to_string());
        let request_params = protojson_conv::to_value(req).map_err(|e| {
            A2AError::internal(format!("failed to serialize request as ProtoJSON: {e}"))
        })?;
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
        self.call(params, methods::GET_EXTENDED_AGENT_CARD, req)
            .await
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
    use a2a_pb::protojson_conv;
    use futures::StreamExt;
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;

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

    async fn spawn_jsonrpc_server(response_body: String) -> (String, oneshot::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (request_tx, request_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response_body.len(),
                response_body,
            );

            let _ = request_tx.send(request);
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        (format!("http://{addr}"), request_rx)
    }

    async fn read_http_request(socket: &mut TcpStream) -> String {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        let mut expected_len = None;

        loop {
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);

            if expected_len.is_none() {
                if let Some(header_end) = find_header_end(&buffer) {
                    let headers = String::from_utf8_lossy(&buffer[..header_end]);
                    expected_len = Some(header_end + parse_content_length(&headers));
                }
            }

            if let Some(total_len) = expected_len {
                if buffer.len() >= total_len {
                    break;
                }
            }
        }

        String::from_utf8(buffer).unwrap()
    }

    fn find_header_end(buffer: &[u8]) -> Option<usize> {
        buffer
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|position| position + 4)
    }

    fn parse_content_length(headers: &str) -> usize {
        headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0)
    }

    fn sample_create_push_config_request() -> CreateTaskPushNotificationConfigRequest {
        CreateTaskPushNotificationConfigRequest {
            task_id: "task-1".into(),
            config: PushNotificationConfig {
                url: "https://example.invalid/webhook".into(),
                id: Some("cfg-1".into()),
                token: Some("secret-token".into()),
                authentication: Some(AuthenticationInfo {
                    scheme: "Bearer".into(),
                    credentials: Some("credential".into()),
                }),
            },
            tenant: Some("tenant-1".into()),
        }
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
        let result_val = protojson_conv::to_value(&sr).unwrap();
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
        let data = serde_json::to_string(&protojson_conv::to_value(&sr).unwrap()).unwrap();
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
        let data = serde_json::to_string(&protojson_conv::to_value(&sr).unwrap()).unwrap();
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
        let data = serde_json::to_string(&protojson_conv::to_value(&sr).unwrap()).unwrap();
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
        let data = serde_json::to_string(&protojson_conv::to_value(&sr).unwrap()).unwrap();
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
        let data = serde_json::to_string(&protojson_conv::to_value(&sr).unwrap()).unwrap();
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
        let sr1 =
            serde_json::to_string(&protojson_conv::to_value(&make_sr(TaskState::Working)).unwrap())
                .unwrap();
        let sr2 = serde_json::to_string(
            &protojson_conv::to_value(&make_sr(TaskState::Completed)).unwrap(),
        )
        .unwrap();
        let chunk1 = format!("data: {}\n\n", sr1);
        let chunk2 = format!("data: {}\n\n", sr2);

        let stream = byte_stream(vec![chunk1, chunk2]);
        let items: Vec<_> = parse_sse_stream_rest(stream).collect().await;
        assert_eq!(items.len(), 2);
        assert!(items[0].is_ok());
        assert!(items[1].is_ok());
    }

    /// Regression: multiple complete SSE events packed into a **single** byte
    /// chunk must all be delivered, even when the upstream byte stream ends
    /// immediately after that chunk.
    ///
    /// Prior to the drain-buffer fix, `parse_sse_stream` returned after parsing
    /// the first event in the buffer and only re-polled the byte stream on the
    /// next consumer call. If the byte stream was already exhausted (e.g. the
    /// server closed the connection after a final burst of events), the
    /// remaining events sitting in the parser buffer were silently dropped —
    /// commonly losing the terminal `TASK_STATE_COMPLETED` `Task` event.
    #[tokio::test]
    async fn test_parse_sse_stream_multiple_events_single_chunk() {
        let make_status = |state| {
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
        let rpc_payload = |sr: StreamResponse, id: i64| -> String {
            let result = protojson_conv::to_value(&sr).unwrap();
            serde_json::to_string(&JsonRpcResponse::success(JsonRpcId::Number(id), result)).unwrap()
        };
        let combined = format!(
            "data: {}\n\ndata: {}\n\n",
            rpc_payload(make_status(TaskState::Working), 1),
            rpc_payload(make_status(TaskState::Completed), 2),
        );

        // All events in one chunk; stream ends immediately after.
        let stream = byte_stream(vec![combined]);
        let items: Vec<_> = parse_sse_stream(stream).collect().await;
        assert_eq!(
            items.len(),
            2,
            "both events must be delivered even when packed into a single \
             byte chunk followed by end-of-stream; got {items:?}",
        );
        assert!(items[0].is_ok());
        assert!(items[1].is_ok());
    }

    /// REST-binding counterpart of
    /// [`test_parse_sse_stream_multiple_events_single_chunk`]. Same
    /// drain-buffer bug existed in `parse_sse_stream_rest`.
    #[tokio::test]
    async fn test_parse_sse_stream_rest_multiple_events_single_chunk() {
        let make_status = |state| {
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
        let encode = |sr: StreamResponse| -> String {
            serde_json::to_string(&protojson_conv::to_value(&sr).unwrap()).unwrap()
        };
        let combined = format!(
            "data: {}\n\ndata: {}\n\n",
            encode(make_status(TaskState::Working)),
            encode(make_status(TaskState::Completed)),
        );

        let stream = byte_stream(vec![combined]);
        let items: Vec<_> = parse_sse_stream_rest(stream).collect().await;
        assert_eq!(
            items.len(),
            2,
            "both events must be delivered even when packed into a single \
             byte chunk followed by end-of-stream; got {items:?}",
        );
        assert!(items[0].is_ok());
        assert!(items[1].is_ok());
    }

    /// Regression: a multi-byte UTF-8 character split across two byte chunks
    /// must be decoded intact.  Per-chunk `String::from_utf8_lossy` corrupts
    /// it into U+FFFD; per-event strict decode reassembles it correctly.
    #[tokio::test]
    async fn test_parse_sse_stream_split_multibyte_utf8() {
        // Build `data: {"statusUpdate":{..., "message":{..., "parts":[{"text":"中"}]}}}\n\n`
        // and split the 3-byte UTF-8 encoding of "中" across two byte chunks.
        let message = Message {
            message_id: "m-1".into(),
            role: Role::Agent,
            parts: vec![Part::text("中")],
            context_id: None,
            task_id: Some("t1".into()),
            metadata: None,
            extensions: None,
            reference_task_ids: None,
        };
        let status_update = TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: Some(message),
                timestamp: None,
            },
            metadata: None,
        };
        let sr = StreamResponse::StatusUpdate(status_update);
        let result = protojson_conv::to_value(&sr).unwrap();
        let rpc_resp = JsonRpcResponse::success(JsonRpcId::Number(1), result);
        let data = serde_json::to_string(&rpc_resp).unwrap();
        let sse_text = format!("data: {data}\n\n");
        let bytes = sse_text.as_bytes();

        // Find the first multi-byte UTF-8 sequence and split it down the middle.
        let zhong_start = bytes
            .windows(3)
            .position(|w| w == [0xE4, 0xB8, 0xAD])
            .expect("payload must contain the UTF-8 encoding of '中'");
        let split = zhong_start + 1;
        let chunk1 = String::from_utf8_lossy(&bytes[..split]).into_owned();
        let chunk2 = String::from_utf8_lossy(&bytes[split..]).into_owned();
        // Sanity: the naive per-chunk lossy decode we just used for the test
        // setup really did corrupt the payload, so the assertion below is
        // meaningful (the parser must do better).
        assert!(
            chunk1.ends_with(char::REPLACEMENT_CHARACTER)
                || chunk2.starts_with(char::REPLACEMENT_CHARACTER),
            "test setup should split a multi-byte char so naive decode corrupts it",
        );

        // But we feed the parser the ORIGINAL bytes, not the corrupted strings.
        let raw1 = bytes[..split].to_vec();
        let raw2 = bytes[split..].to_vec();
        let raw_stream = stream::iter(vec![
            Ok::<_, reqwest::Error>(bytes::Bytes::from(raw1)),
            Ok::<_, reqwest::Error>(bytes::Bytes::from(raw2)),
        ]);
        let items: Vec<_> = parse_sse_stream(raw_stream).collect().await;
        assert_eq!(items.len(), 1, "expected one event, got {items:?}");
        let event = items.into_iter().next().unwrap().expect("decoded");
        match event {
            StreamResponse::StatusUpdate(su) => {
                let msg = su.status.message.expect("message present");
                let text = match &msg.parts[0].content {
                    PartContent::Text(t) => t.clone(),
                    other => panic!("expected text part, got {other:?}"),
                };
                assert_eq!(text, "中", "multi-byte char must survive chunk split");
            }
            other => panic!("expected StatusUpdate, got {other:?}"),
        }
    }

    /// Invalid UTF-8 inside a complete event must surface an error rather
    /// than silently producing U+FFFD replacement characters.
    #[tokio::test]
    async fn test_parse_sse_stream_invalid_utf8_surfaces_error() {
        // "data: " prefix is ASCII; follow it with an invalid UTF-8 byte
        // sequence (a lone continuation byte), then the \n\n terminator.
        let mut bytes = b"data: ".to_vec();
        bytes.push(0xFF); // never valid in UTF-8
        bytes.extend_from_slice(b"\n\n");
        let raw_stream = stream::iter(vec![Ok::<_, reqwest::Error>(bytes::Bytes::from(bytes))]);
        let items: Vec<_> = parse_sse_stream(raw_stream).collect().await;
        assert_eq!(items.len(), 1, "expected one error item, got {items:?}");
        let err = items.into_iter().next().unwrap().expect_err("should error");
        assert!(
            err.to_string().contains("UTF-8"),
            "error should mention UTF-8 decoding: {err}",
        );
    }

    /// JSON-RPC envelope whose `result` is not a valid `StreamResponse`
    /// must surface a parse error instead of silently dropping the event.
    #[tokio::test]
    async fn test_parse_sse_stream_invalid_protojson_result_surfaces_error() {
        let rpc_resp = JsonRpcResponse::success(
            JsonRpcId::Number(1),
            serde_json::json!({"not_a_stream_response_field": 42}),
        );
        let data = serde_json::to_string(&rpc_resp).unwrap();
        let sse_text = format!("data: {data}\n\n");
        let stream = byte_stream(vec![sse_text]);
        let items: Vec<_> = parse_sse_stream(stream).collect().await;
        assert_eq!(items.len(), 1, "expected one error item, got {items:?}");
        let err = items.into_iter().next().unwrap().expect_err("should error");
        assert!(
            err.to_string().contains("parse error"),
            "error should mention parse error: {err}",
        );
    }

    /// Direct `StreamResponse` JSON (JSON-RPC fallback path) that fails
    /// protojson conversion must surface a parse error.
    #[tokio::test]
    async fn test_parse_sse_stream_direct_invalid_protojson_surfaces_error() {
        // Not a JSON-RPC envelope (missing `jsonrpc`/`id`) and not a valid
        // StreamResponse — exercises the `Err(_)` fallback branch.
        let sse_text = "data: {\"foo\":\"bar\"}\n\n".to_string();
        let stream = byte_stream(vec![sse_text]);
        let items: Vec<_> = parse_sse_stream(stream).collect().await;
        assert_eq!(items.len(), 1, "expected one error item, got {items:?}");
        let err = items.into_iter().next().unwrap().expect_err("should error");
        assert!(
            err.to_string().contains("parse error"),
            "error should mention parse error: {err}",
        );
    }

    /// Errors yielded by the underlying byte stream must be surfaced to the
    /// consumer rather than swallowed.  `futures::stream::iter` only accepts
    /// `Ok` variants typed as `reqwest::Error`, so we need an actual
    /// reqwest::Error instance — obtained by issuing a request against an
    /// address that cannot accept a connection.
    #[tokio::test]
    async fn test_parse_sse_stream_surfaces_stream_errors() {
        // Produce a real reqwest::Error by hitting a listener we bound and
        // then dropped, so the connect fails immediately.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = reqwest::get(format!("http://{addr}/"))
            .await
            .expect_err("connection should fail");

        let s = stream::iter(vec![Err::<bytes::Bytes, reqwest::Error>(err)]);
        let items: Vec<_> = parse_sse_stream(s).collect().await;
        assert_eq!(items.len(), 1, "expected one error item, got {items:?}");
        let got = items.into_iter().next().unwrap().expect_err("should error");
        assert!(
            got.to_string().contains("SSE stream error"),
            "error should mention SSE stream error: {got}",
        );
    }

    /// REST variant of [`test_parse_sse_stream_surfaces_stream_errors`].
    #[tokio::test]
    async fn test_parse_sse_stream_rest_surfaces_stream_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = reqwest::get(format!("http://{addr}/"))
            .await
            .expect_err("connection should fail");

        let s = stream::iter(vec![Err::<bytes::Bytes, reqwest::Error>(err)]);
        let items: Vec<_> = parse_sse_stream_rest(s).collect().await;
        assert_eq!(items.len(), 1, "expected one error item, got {items:?}");
        let got = items.into_iter().next().unwrap().expect_err("should error");
        assert!(
            got.to_string().contains("SSE stream error"),
            "error should mention SSE stream error: {got}",
        );
    }

    /// SSE spec allows `\r\n\r\n`, `\n\n`, and `\r\r` as event separators.
    /// This test exercises the `\r\n\r\n` variant.
    #[tokio::test]
    async fn test_parse_sse_stream_crlf_line_endings() {
        let sr = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        let data = serde_json::to_string(&protojson_conv::to_value(&sr).unwrap()).unwrap();
        // Use \r\n\r\n instead of \n\n
        let sse_text = format!("data: {}\r\n\r\n", data);

        let stream = byte_stream(vec![sse_text]);
        let mut parsed = parse_sse_stream(stream);
        let item = parsed.next().await.unwrap().unwrap();
        assert!(matches!(item, StreamResponse::StatusUpdate(_)));
        assert!(parsed.next().await.is_none());
    }

    /// `\r\r` is also a valid SSE event separator per the spec.
    #[tokio::test]
    async fn test_parse_sse_stream_cr_line_endings() {
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
        let data = serde_json::to_string(&protojson_conv::to_value(&sr).unwrap()).unwrap();
        let sse_text = format!("data: {}\r\r", data);

        let stream = byte_stream(vec![sse_text]);
        let mut parsed = parse_sse_stream_rest(stream);
        let item = parsed.next().await.unwrap().unwrap();
        assert!(matches!(item, StreamResponse::StatusUpdate(_)));
    }

    #[test]
    fn test_find_event_boundary_lf() {
        assert_eq!(find_event_boundary(b"foo\n\nbar"), Some((3, 5)));
    }

    #[test]
    fn test_find_event_boundary_cr() {
        assert_eq!(find_event_boundary(b"foo\r\rbar"), Some((3, 5)));
    }

    #[test]
    fn test_find_event_boundary_crlf() {
        assert_eq!(find_event_boundary(b"foo\r\n\r\nbar"), Some((3, 7)));
    }

    #[test]
    fn test_find_event_boundary_none() {
        assert!(find_event_boundary(b"no boundary here").is_none());
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

    #[tokio::test]
    async fn test_create_push_config_sends_nested_request_shape() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "result": {
                "taskId": "task-1",
                "config": {
                    "url": "https://example.invalid/webhook",
                    "id": "cfg-1",
                    "token": "secret-token",
                    "authentication": {
                        "scheme": "Bearer",
                        "credentials": "credential"
                    }
                },
                "tenant": "tenant-1"
            }
        })
        .to_string();
        let (endpoint, request_rx) = spawn_jsonrpc_server(response).await;
        let transport = JsonRpcTransport::new(Client::new(), endpoint);
        let mut params = ServiceParams::new();
        params.insert("x-trace".into(), vec!["alpha".into(), "beta".into()]);

        let result = transport
            .create_push_config(&params, &sample_create_push_config_request())
            .await
            .unwrap();

        assert_eq!(result.task_id, "task-1");
        assert_eq!(result.config.id.as_deref(), Some("cfg-1"));

        let request = request_rx.await.unwrap();
        let request_lower = request.to_ascii_lowercase();
        assert!(request_lower.contains("x-trace: alpha"));
        assert!(request_lower.contains("x-trace: beta"));

        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let payload: Value = serde_json::from_str(body).unwrap();
        assert_eq!(payload["method"], methods::CREATE_PUSH_CONFIG);
        assert_eq!(
            payload["params"],
            json!({
                "taskId": "task-1",
                "config": {
                    "url": "https://example.invalid/webhook",
                    "id": "cfg-1",
                    "token": "secret-token",
                    "authentication": {
                        "scheme": "Bearer",
                        "credentials": "credential"
                    }
                },
                "tenant": "tenant-1"
            })
        );
    }

    #[tokio::test]
    async fn test_create_push_config_surfaces_jsonrpc_error() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "error": {
                "code": error_code::INVALID_PARAMS,
                "message": "invalid params",
                "data": null
            }
        })
        .to_string();
        let (endpoint, _request_rx) = spawn_jsonrpc_server(response).await;
        let transport = JsonRpcTransport::new(Client::new(), endpoint);

        let error = transport
            .create_push_config(&ServiceParams::new(), &sample_create_push_config_request())
            .await
            .unwrap_err();

        assert_eq!(error.code, error_code::INVALID_PARAMS);
        assert_eq!(error.message, "invalid params");
    }

    #[tokio::test]
    async fn test_create_push_config_rejects_missing_result() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": "1"
        })
        .to_string();
        let (endpoint, _request_rx) = spawn_jsonrpc_server(response).await;
        let transport = JsonRpcTransport::new(Client::new(), endpoint);

        let error = transport
            .create_push_config(&ServiceParams::new(), &sample_create_push_config_request())
            .await
            .unwrap_err();

        assert_eq!(error.code, error_code::INTERNAL_ERROR);
        assert_eq!(error.message, "JSON-RPC response missing result");
    }

    #[tokio::test]
    async fn test_get_push_config_uses_protojson_request_path() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": "1",
            "result": {
                "taskId": "task-1",
                "config": {
                    "url": "https://example.invalid/webhook",
                    "id": "cfg-1",
                    "token": "secret-token"
                },
                "tenant": "tenant-1"
            }
        })
        .to_string();
        let (endpoint, request_rx) = spawn_jsonrpc_server(response).await;
        let transport = JsonRpcTransport::new(Client::new(), endpoint);

        let result = transport
            .get_push_config(
                &ServiceParams::new(),
                &GetTaskPushNotificationConfigRequest {
                    task_id: "task-1".into(),
                    id: "cfg-1".into(),
                    tenant: Some("tenant-1".into()),
                },
            )
            .await
            .unwrap();

        assert_eq!(result.task_id, "task-1");
        assert_eq!(result.config.id.as_deref(), Some("cfg-1"));

        let request = request_rx.await.unwrap();
        let body = request.split("\r\n\r\n").nth(1).unwrap();
        let payload: Value = serde_json::from_str(body).unwrap();
        assert_eq!(payload["method"], methods::GET_PUSH_CONFIG);
        assert_eq!(
            payload["params"],
            json!({
                "taskId": "task-1",
                "id": "cfg-1",
                "tenant": "tenant-1"
            })
        );
    }
}
