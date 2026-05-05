// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use a2a::*;
use a2a_client::transport::{ServiceParams, Transport, TransportFactory};
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use async_trait::async_trait;
use fastwebsockets::{
    FragmentCollector, Frame, OpCode, Payload, WebSocketError, handshake,
};
use futures::Stream;
use futures::stream::BoxStream;
use http::header::{CONNECTION, HOST, UPGRADE};
use http::Request;
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use crate::common::{
    DEFAULT_MAX_FRAME_BYTES, SUBPROTOCOL, TRANSPORT_PROTOCOL_WEBSOCKET, WsRequestEnvelope,
    WsResponseEnvelope, methods, service_params_to_envelope,
};
use crate::errors::ws_error_to_a2a_error;

#[derive(Debug)]
enum OutboundClient {
    Frame(String),
    Close,
}

#[derive(Default)]
struct Pending {
    unary: HashMap<String, oneshot::Sender<Result<Value, A2AError>>>,
    streaming: HashMap<String, mpsc::UnboundedSender<Result<StreamResponse, A2AError>>>,
    closed: bool,
    close_error: Option<A2AError>,
}

impl Pending {
    fn fail_all(&mut self, error: A2AError) {
        self.closed = true;
        self.close_error = Some(error.clone());
        for (_id, tx) in self.unary.drain() {
            let _ = tx.send(Err(error.clone()));
        }
        for (_id, tx) in self.streaming.drain() {
            let _ = tx.send(Err(error.clone()));
        }
    }
}

struct ConnectionInner {
    outbound: mpsc::UnboundedSender<OutboundClient>,
    pending: Arc<Mutex<Pending>>,
}

impl ConnectionInner {
    fn send_outbound(&self, message: OutboundClient) -> Result<(), A2AError> {
        self.outbound
            .send(message)
            .map_err(|_| connection_closed_error(&self.pending))
    }

    fn register_unary(
        &self,
        id: &str,
    ) -> Result<oneshot::Receiver<Result<Value, A2AError>>, A2AError> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending.lock().unwrap();
        if pending.closed {
            let err = pending
                .close_error
                .clone()
                .unwrap_or_else(|| A2AError::internal("websocket connection closed"));
            return Err(err);
        }
        pending.unary.insert(id.to_string(), tx);
        Ok(rx)
    }

    fn register_streaming(
        &self,
        id: &str,
    ) -> Result<mpsc::UnboundedReceiver<Result<StreamResponse, A2AError>>, A2AError> {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut pending = self.pending.lock().unwrap();
        if pending.closed {
            let err = pending
                .close_error
                .clone()
                .unwrap_or_else(|| A2AError::internal("websocket connection closed"));
            return Err(err);
        }
        pending.streaming.insert(id.to_string(), tx);
        Ok(rx)
    }

    fn deregister_streaming(&self, id: &str) {
        let mut pending = self.pending.lock().unwrap();
        pending.streaming.remove(id);
    }

    fn close(&self) {
        let _ = self.outbound.send(OutboundClient::Close);
    }
}

fn connection_closed_error(pending: &Arc<Mutex<Pending>>) -> A2AError {
    let pending = pending.lock().unwrap();
    pending
        .close_error
        .clone()
        .unwrap_or_else(|| A2AError::internal("websocket connection closed"))
}

/// WebSocket transport — implements the [`Transport`] trait by multiplexing
/// requests and streams over a single persistent connection.
pub struct WebSocketTransport {
    inner: Arc<ConnectionInner>,
}

impl WebSocketTransport {
    /// Connect to the agent at the given endpoint URL. Accepts `ws://`,
    /// `wss://` (returns an unsupported error in this build), or bare
    /// `host:port[/path]` strings (normalized to `ws://`).
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self, A2AError> {
        let endpoint = endpoint.into();
        let parsed = parse_endpoint(&endpoint)?;

        let stream = TcpStream::connect((parsed.host.as_str(), parsed.port))
            .await
            .map_err(|err| {
                A2AError::internal(format!(
                    "failed to connect to {}:{}: {err}",
                    parsed.host, parsed.port
                ))
            })?;

        let host_header = if uses_default_port(&parsed.scheme, parsed.port) {
            parsed.host.clone()
        } else {
            format!("{}:{}", parsed.host, parsed.port)
        };

        let req = Request::builder()
            .method("GET")
            .uri(parsed.path.clone())
            .header(HOST, host_header)
            .header(UPGRADE, "websocket")
            .header(CONNECTION, "upgrade")
            .header("Sec-WebSocket-Key", handshake::generate_key())
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Protocol", SUBPROTOCOL)
            .body(Empty::<Bytes>::new())
            .map_err(|err| A2AError::internal(format!("failed to build upgrade request: {err}")))?;

        let (ws, response) = handshake::client(&SpawnExecutor, req, stream)
            .await
            .map_err(|err| A2AError::internal(format!("websocket handshake failed: {err}")))?;

        if !response_subprotocol_matches(&response) {
            return Err(A2AError::internal(
                "server did not negotiate the 'a2a.v1' sub-protocol",
            ));
        }

        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel::<OutboundClient>();
        let pending = Arc::new(Mutex::new(Pending::default()));
        let inner = Arc::new(ConnectionInner {
            outbound: outbound_tx,
            pending: pending.clone(),
        });

        tokio::spawn(run_connection(ws, outbound_rx, pending));

        Ok(WebSocketTransport { inner })
    }

    async fn call_unary<Req, Resp>(
        &self,
        method: &str,
        params: &ServiceParams,
        request: &Req,
    ) -> Result<Resp, A2AError>
    where
        Req: ProtoJsonPayload,
        Resp: ProtoJsonPayload,
    {
        let value = self.call_unary_value(method, params, request).await?;
        protojson_conv::from_value(value)
            .map_err(|err| A2AError::internal(format!("failed to deserialize result: {err}")))
    }

    async fn call_unary_value<Req>(
        &self,
        method: &str,
        params: &ServiceParams,
        request: &Req,
    ) -> Result<Value, A2AError>
    where
        Req: ProtoJsonPayload,
    {
        let payload = protojson_conv::to_value(request).map_err(|err| {
            A2AError::internal(format!("failed to serialize request as ProtoJSON: {err}"))
        })?;
        self.call_unary_raw(method, params, payload).await
    }

    async fn call_unary_raw(
        &self,
        method: &str,
        params: &ServiceParams,
        request_params: Value,
    ) -> Result<Value, A2AError> {
        let id = uuid::Uuid::now_v7().to_string();
        let envelope = WsRequestEnvelope {
            id: id.clone(),
            method: Some(method.to_string()),
            params: Some(request_params),
            service_params: service_params_to_envelope(params),
            cancel_stream: None,
        };

        let receiver = self.inner.register_unary(&id)?;
        self.inner.send_outbound(OutboundClient::Frame(
            serde_json::to_string(&envelope)
                .map_err(|err| A2AError::internal(format!("failed to serialize envelope: {err}")))?,
        ))?;

        match receiver.await {
            Ok(result) => result,
            Err(_) => Err(connection_closed_error(&self.inner.pending)),
        }
    }

    async fn call_streaming<Req>(
        &self,
        method: &str,
        params: &ServiceParams,
        request: &Req,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError>
    where
        Req: ProtoJsonPayload,
    {
        let payload = protojson_conv::to_value(request).map_err(|err| {
            A2AError::internal(format!("failed to serialize request as ProtoJSON: {err}"))
        })?;
        let id = uuid::Uuid::now_v7().to_string();
        let envelope = WsRequestEnvelope {
            id: id.clone(),
            method: Some(method.to_string()),
            params: Some(payload),
            service_params: service_params_to_envelope(params),
            cancel_stream: None,
        };

        let receiver = self.inner.register_streaming(&id)?;
        self.inner.send_outbound(OutboundClient::Frame(
            serde_json::to_string(&envelope)
                .map_err(|err| A2AError::internal(format!("failed to serialize envelope: {err}")))?,
        ))?;

        let stream = StreamingResponse {
            receiver,
            inner: self.inner.clone(),
            id: id.clone(),
            cancel_sent: false,
            terminated: false,
        };
        Ok(Box::pin(stream))
    }
}

struct StreamingResponse {
    receiver: mpsc::UnboundedReceiver<Result<StreamResponse, A2AError>>,
    inner: Arc<ConnectionInner>,
    id: String,
    cancel_sent: bool,
    terminated: bool,
}

impl Stream for StreamingResponse {
    type Item = Result<StreamResponse, A2AError>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.terminated {
            return std::task::Poll::Ready(None);
        }
        let poll = Pin::new(&mut self.receiver).poll_recv(cx);
        if let std::task::Poll::Ready(None) = poll {
            self.terminated = true;
        }
        poll
    }
}

impl Drop for StreamingResponse {
    fn drop(&mut self) {
        if !self.cancel_sent && !self.terminated {
            self.cancel_sent = true;
            self.inner.deregister_streaming(&self.id);
            let envelope = WsRequestEnvelope {
                id: self.id.clone(),
                cancel_stream: Some(true),
                ..Default::default()
            };
            if let Ok(text) = serde_json::to_string(&envelope) {
                let _ = self.inner.send_outbound(OutboundClient::Frame(text));
            }
        } else {
            self.inner.deregister_streaming(&self.id);
        }
    }
}

fn response_subprotocol_matches<B>(response: &http::Response<B>) -> bool {
    response
        .headers()
        .get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|item| item.trim())
        .any(|protocol| protocol.eq_ignore_ascii_case(SUBPROTOCOL))
}

#[derive(Debug, PartialEq)]
struct ParsedEndpoint {
    scheme: String,
    host: String,
    port: u16,
    path: String,
}

fn parse_endpoint(endpoint: &str) -> Result<ParsedEndpoint, A2AError> {
    let (scheme, rest) = match endpoint.split_once("://") {
        Some(("ws", rest)) => ("ws".to_string(), rest),
        Some(("wss", _)) => {
            return Err(A2AError::internal(
                "wss:// endpoints are not supported by this build of a2a-websocket; \
                 use ws:// or wrap the connection with TLS upstream",
            ));
        }
        Some((scheme, _)) => {
            return Err(A2AError::internal(format!(
                "unsupported scheme '{scheme}'; expected ws:// or wss://"
            )));
        }
        None => ("ws".to_string(), endpoint),
    };

    let (host_port, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };

    if host_port.is_empty() {
        return Err(A2AError::internal("endpoint is missing a host"));
    }

    let (host, port) = match host_port.rsplit_once(':') {
        Some((host, port_str)) => {
            let port: u16 = port_str.parse().map_err(|err| {
                A2AError::internal(format!("invalid port '{port_str}': {err}"))
            })?;
            (host.to_string(), port)
        }
        None => (host_port.to_string(), default_port(&scheme)),
    };

    Ok(ParsedEndpoint {
        scheme,
        host,
        port,
        path: path.to_string(),
    })
}

fn default_port(scheme: &str) -> u16 {
    match scheme {
        "wss" => 443,
        _ => 80,
    }
}

fn uses_default_port(scheme: &str, port: u16) -> bool {
    port == default_port(scheme)
}

async fn run_connection(
    mut ws: fastwebsockets::WebSocket<TokioIo<Upgraded>>,
    mut outbound_rx: mpsc::UnboundedReceiver<OutboundClient>,
    pending: Arc<Mutex<Pending>>,
) {
    ws.set_max_message_size(DEFAULT_MAX_FRAME_BYTES);
    ws.set_auto_close(true);
    ws.set_auto_pong(true);
    let mut ws = FragmentCollector::new(ws);

    loop {
        tokio::select! {
            biased;

            outbound = outbound_rx.recv() => {
                match outbound {
                    Some(OutboundClient::Frame(text)) => {
                        if let Err(err) = ws
                            .write_frame(Frame::text(Payload::Owned(text.into_bytes())))
                            .await
                        {
                            tracing::debug!(error = %err, "client write failed; closing");
                            break;
                        }
                    }
                    Some(OutboundClient::Close) => {
                        let _ = ws
                            .write_frame(Frame::close(
                                crate::common::close_codes::NORMAL_CLOSURE,
                                b"client closing",
                            ))
                            .await;
                        break;
                    }
                    None => break,
                }
            }

            incoming = ws.read_frame() => {
                match incoming {
                    Ok(frame) => match frame.opcode {
                        OpCode::Close => break,
                        OpCode::Text => {
                            handle_incoming_text(&frame.payload, &pending);
                        }
                        OpCode::Binary => {
                            // Servers should not send binary frames in this binding.
                            tracing::debug!(
                                "received unexpected binary frame from server; closing"
                            );
                            break;
                        }
                        _ => {}
                    },
                    Err(WebSocketError::ConnectionClosed) => break,
                    Err(err) => {
                        tracing::debug!(error = %err, "client read error; closing");
                        break;
                    }
                }
            }
        }
    }

    let mut pending = pending.lock().unwrap();
    pending.fail_all(A2AError::internal("websocket connection closed"));
}

fn handle_incoming_text(payload: &[u8], pending: &Arc<Mutex<Pending>>) {
    let envelope: WsResponseEnvelope = match serde_json::from_slice(payload) {
        Ok(env) => env,
        Err(err) => {
            tracing::debug!(error = %err, "failed to parse incoming envelope");
            return;
        }
    };

    let Some(id) = envelope.id.clone() else {
        // Server emitted an error with id=null; nothing we can route to.
        if let Some(error) = envelope.error {
            tracing::warn!(error = %error.message, "received unrouted server error");
        }
        return;
    };

    if let Some(error) = envelope.error {
        let a2a_error = ws_error_to_a2a_error(&error);
        let mut pending = pending.lock().unwrap();
        if let Some(tx) = pending.unary.remove(&id) {
            let _ = tx.send(Err(a2a_error));
        } else if let Some(tx) = pending.streaming.remove(&id) {
            let _ = tx.send(Err(a2a_error));
        }
        return;
    }

    if let Some(value) = envelope.event {
        let pending = pending.lock().unwrap();
        if let Some(tx) = pending.streaming.get(&id) {
            match protojson_conv::from_value::<StreamResponse>(value) {
                Ok(sr) => {
                    let _ = tx.send(Ok(sr));
                }
                Err(err) => {
                    let _ = tx.send(Err(A2AError::internal(format!(
                        "failed to deserialize event: {err}"
                    ))));
                }
            }
        }
        return;
    }

    if envelope.stream_end.unwrap_or(false) {
        let mut pending = pending.lock().unwrap();
        pending.streaming.remove(&id);
        return;
    }

    if let Some(value) = envelope.result {
        let mut pending = pending.lock().unwrap();
        if let Some(tx) = pending.unary.remove(&id) {
            let _ = tx.send(Ok(value));
        }
    }
}

#[async_trait]
impl Transport for WebSocketTransport {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        self.call_unary(methods::SEND_MESSAGE, params, req).await
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.call_streaming(methods::SEND_STREAMING_MESSAGE, params, req)
            .await
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: &GetTaskRequest,
    ) -> Result<Task, A2AError> {
        self.call_unary(methods::GET_TASK, params, req).await
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        self.call_unary(methods::LIST_TASKS, params, req).await
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: &CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        self.call_unary(methods::CANCEL_TASK, params, req).await
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: &SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.call_streaming(methods::SUBSCRIBE_TO_TASK, params, req)
            .await
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: &CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.call_unary(methods::CREATE_PUSH_CONFIG, params, req).await
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.call_unary(methods::GET_PUSH_CONFIG, params, req).await
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        self.call_unary(methods::LIST_PUSH_CONFIGS, params, req).await
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        self.call_unary_value(methods::DELETE_PUSH_CONFIG, params, req)
            .await
            .map(|_| ())
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: &GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        self.call_unary(methods::GET_EXTENDED_AGENT_CARD, params, req)
            .await
    }

    async fn destroy(&self) -> Result<(), A2AError> {
        self.inner.close();
        Ok(())
    }
}

/// Factory for creating WebSocket transports from agent card interfaces.
pub struct WebSocketTransportFactory;

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
        let transport = WebSocketTransport::connect(&iface.url).await?;
        Ok(Box::new(transport))
    }
}

// ---------------------------------------------------------------------------
// Hyper executor adapter (binds hyper's executor to the tokio runtime).
// ---------------------------------------------------------------------------

struct SpawnExecutor;

impl<Fut> hyper::rt::Executor<Fut> for SpawnExecutor
where
    Fut: Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    fn execute(&self, fut: Fut) {
        tokio::task::spawn(fut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_endpoint_accepts_ws_with_explicit_port_and_path() {
        let parsed = parse_endpoint("ws://example.com:9000/a2a/ws").unwrap();
        assert_eq!(
            parsed,
            ParsedEndpoint {
                scheme: "ws".into(),
                host: "example.com".into(),
                port: 9000,
                path: "/a2a/ws".into(),
            }
        );
    }

    #[test]
    fn parse_endpoint_uses_default_path_when_missing() {
        let parsed = parse_endpoint("ws://example.com:9000").unwrap();
        assert_eq!(parsed.path, "/");
    }

    #[test]
    fn parse_endpoint_uses_default_port_when_missing() {
        let parsed = parse_endpoint("ws://example.com").unwrap();
        assert_eq!(parsed.port, 80);
        assert_eq!(parsed.path, "/");
    }

    #[test]
    fn parse_endpoint_normalizes_bare_host_port() {
        let parsed = parse_endpoint("127.0.0.1:8080/path").unwrap();
        assert_eq!(parsed.scheme, "ws");
        assert_eq!(parsed.host, "127.0.0.1");
        assert_eq!(parsed.port, 8080);
        assert_eq!(parsed.path, "/path");
    }

    #[test]
    fn parse_endpoint_rejects_wss_in_this_build() {
        let err = parse_endpoint("wss://example.com").unwrap_err();
        assert!(err.message.contains("wss://"));
    }

    #[test]
    fn parse_endpoint_rejects_unknown_scheme() {
        let err = parse_endpoint("http://example.com").unwrap_err();
        assert!(err.message.contains("unsupported scheme"));
    }

    #[test]
    fn parse_endpoint_rejects_empty_host() {
        let err = parse_endpoint("ws:///path").unwrap_err();
        assert!(err.message.contains("missing a host"));
    }

    #[test]
    fn parse_endpoint_rejects_non_numeric_port() {
        let err = parse_endpoint("ws://example.com:not-a-port").unwrap_err();
        assert!(err.message.contains("invalid port"));
    }

    #[test]
    fn default_port_returns_443_for_wss_and_80_otherwise() {
        assert_eq!(default_port("ws"), 80);
        assert_eq!(default_port("wss"), 443);
        assert_eq!(default_port("anything-else"), 80);
    }

    #[test]
    fn uses_default_port_recognizes_default_combinations() {
        assert!(uses_default_port("ws", 80));
        assert!(uses_default_port("wss", 443));
        assert!(!uses_default_port("ws", 9000));
    }

    #[test]
    fn response_subprotocol_matches_recognises_a2a_v1() {
        let response = http::Response::builder()
            .status(101)
            .header("Sec-WebSocket-Protocol", "a2a.v1")
            .body(())
            .unwrap();
        assert!(response_subprotocol_matches(&response));

        let response = http::Response::builder()
            .status(101)
            .header("Sec-WebSocket-Protocol", "foo, A2A.V1, bar")
            .body(())
            .unwrap();
        assert!(response_subprotocol_matches(&response));

        let response = http::Response::builder().status(101).body(()).unwrap();
        assert!(!response_subprotocol_matches(&response));
    }

    #[test]
    fn websocket_transport_factory_protocol_string_is_websocket() {
        let f = WebSocketTransportFactory;
        assert_eq!(f.protocol(), TRANSPORT_PROTOCOL_WEBSOCKET);
        assert_eq!(f.protocol(), "WEBSOCKET");
    }

    #[tokio::test]
    async fn websocket_transport_connect_to_unreachable_endpoint_returns_error() {
        let result = WebSocketTransport::connect("ws://127.0.0.1:1").await;
        assert!(result.is_err());
    }

    #[test]
    fn pending_fail_all_propagates_error_to_unary_and_streaming_sinks() {
        let mut pending = Pending::default();
        let (utx, urx) = oneshot::channel::<Result<Value, A2AError>>();
        let (stx, mut srx) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();
        pending.unary.insert("u".into(), utx);
        pending.streaming.insert("s".into(), stx);

        pending.fail_all(A2AError::internal("closed"));

        let unary = futures::executor::block_on(urx).unwrap();
        assert!(unary.is_err());

        let stream_item = srx.try_recv().unwrap();
        assert!(stream_item.is_err());

        assert!(pending.closed);
        assert_eq!(
            pending.close_error.as_ref().unwrap().code,
            error_code::INTERNAL_ERROR
        );
    }

    #[test]
    fn pending_register_after_close_fails() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        pending
            .lock()
            .unwrap()
            .fail_all(A2AError::internal("dropped"));

        let (outbound, _outbound_rx) = mpsc::unbounded_channel::<OutboundClient>();
        let inner = ConnectionInner {
            outbound,
            pending: pending.clone(),
        };

        let unary_err = inner.register_unary("x").unwrap_err();
        assert_eq!(unary_err.code, error_code::INTERNAL_ERROR);

        let streaming_err = inner.register_streaming("y").unwrap_err();
        assert_eq!(streaming_err.code, error_code::INTERNAL_ERROR);
    }

    #[test]
    fn handle_incoming_text_dispatches_unary_result() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (tx, rx) = oneshot::channel::<Result<Value, A2AError>>();
        pending.lock().unwrap().unary.insert("req-1".into(), tx);

        let response = WsResponseEnvelope::result("req-1", serde_json::json!({"ok": 1}));
        let json = serde_json::to_vec(&response).unwrap();
        handle_incoming_text(&json, &pending);

        let value = futures::executor::block_on(rx).unwrap().unwrap();
        assert_eq!(value["ok"], 1);
        assert!(pending.lock().unwrap().unary.is_empty());
    }

    #[test]
    fn handle_incoming_text_dispatches_unary_error() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (tx, rx) = oneshot::channel::<Result<Value, A2AError>>();
        pending.lock().unwrap().unary.insert("req-1".into(), tx);

        let response = WsResponseEnvelope::error(
            Some("req-1".into()),
            crate::common::WsErrorObject {
                error_type: crate::common::error_types::TASK_NOT_FOUND.to_string(),
                message: "missing".into(),
                details: None,
            },
        );
        let json = serde_json::to_vec(&response).unwrap();
        handle_incoming_text(&json, &pending);

        let err = futures::executor::block_on(rx).unwrap().unwrap_err();
        assert_eq!(err.code, error_code::TASK_NOT_FOUND);
        assert_eq!(err.message, "missing");
    }

    #[test]
    fn handle_incoming_text_routes_stream_event_to_streaming_sink() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();
        pending
            .lock()
            .unwrap()
            .streaming
            .insert("req-2".into(), tx);

        // Build a TaskStatusUpdateEvent to embed.
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "task-1".into(),
            context_id: "ctx-1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        let event_value = protojson_conv::to_value(&event).unwrap();
        let response = WsResponseEnvelope::event("req-2", event_value);
        let json = serde_json::to_vec(&response).unwrap();
        handle_incoming_text(&json, &pending);

        let item = rx.try_recv().unwrap().unwrap();
        match item {
            StreamResponse::StatusUpdate(_) => {}
            _ => panic!("expected StatusUpdate"),
        }
        // Streaming sink stays registered until streamEnd or error.
        assert!(pending.lock().unwrap().streaming.contains_key("req-2"));
    }

    #[test]
    fn handle_incoming_text_stream_end_removes_streaming_sink() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (tx, _rx) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();
        pending
            .lock()
            .unwrap()
            .streaming
            .insert("req-2".into(), tx);

        let response = WsResponseEnvelope::stream_end("req-2");
        let json = serde_json::to_vec(&response).unwrap();
        handle_incoming_text(&json, &pending);

        assert!(!pending.lock().unwrap().streaming.contains_key("req-2"));
    }

    #[test]
    fn handle_incoming_text_ignores_envelope_with_null_id() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let envelope = WsResponseEnvelope::error(
            None,
            crate::common::WsErrorObject {
                error_type: crate::common::error_types::JSON_PARSE.to_string(),
                message: "bad json".into(),
                details: None,
            },
        );
        let json = serde_json::to_vec(&envelope).unwrap();
        // Should not panic and should not affect any sinks.
        handle_incoming_text(&json, &pending);
    }

    #[test]
    fn handle_incoming_text_ignores_invalid_json() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        // Should silently drop malformed payload.
        handle_incoming_text(b"not json", &pending);
    }
}
