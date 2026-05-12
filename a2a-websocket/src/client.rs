// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;
use std::fmt::Display;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use a2a::*;
use a2a_client::transport::{ServiceParams, Transport, TransportFactory};
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use async_trait::async_trait;
use fastwebsockets::{FragmentCollector, Frame, OpCode, Payload, WebSocketError, handshake};
use futures::Stream;
use futures::stream::BoxStream;
use http::Request;
use http::header::{CONNECTION, HOST, UPGRADE};
use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::common::{
    DEFAULT_MAX_FRAME_BYTES, SUBPROTOCOL, TRANSPORT_PROTOCOL_WEBSOCKET, WsRequestEnvelope,
    WsResponseEnvelope, methods, service_params_to_envelope,
};
use crate::errors::ws_error_to_a2a_error;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const OUTBOUND_BUFFER_CAPACITY: usize = 64;

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
    outbound: mpsc::Sender<OutboundClient>,
    pending: Arc<Mutex<Pending>>,
}

impl ConnectionInner {
    async fn send_outbound(&self, message: OutboundClient) -> Result<(), A2AError> {
        self.outbound
            .send(message)
            .await
            .map_err(|_| connection_closed_error(&self.pending))
    }

    fn try_send_outbound(&self, message: OutboundClient) -> Result<(), A2AError> {
        self.outbound
            .try_send(message)
            .map_err(|_| connection_closed_error(&self.pending))
    }

    fn register_unary(
        &self,
        id: &str,
    ) -> Result<oneshot::Receiver<Result<Value, A2AError>>, A2AError> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self.pending.lock();
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
        let mut pending = self.pending.lock();
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
        let mut pending = self.pending.lock();
        pending.streaming.remove(id);
    }

    async fn close(&self) {
        let _ = self.send_outbound(OutboundClient::Close).await;
    }
}

fn connection_closed_error(pending: &Arc<Mutex<Pending>>) -> A2AError {
    let pending = pending.lock();
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

        let stream = connect_tcp(&parsed.host, parsed.port).await?;

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

        let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
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
        self.inner
            .send_outbound(OutboundClient::Frame(
                serde_json::to_string(&envelope).map_err(|err| {
                    A2AError::internal(format!("failed to serialize envelope: {err}"))
                })?,
            ))
            .await?;

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
        self.inner
            .send_outbound(OutboundClient::Frame(
                serde_json::to_string(&envelope).map_err(|err| {
                    A2AError::internal(format!("failed to serialize envelope: {err}"))
                })?,
            ))
            .await?;

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

async fn connect_tcp(host: &str, port: u16) -> Result<TcpStream, A2AError> {
    connect_with_timeout(
        TcpStream::connect((host, port)),
        DEFAULT_CONNECT_TIMEOUT,
        host,
        port,
    )
    .await
}

async fn connect_with_timeout<F, T, E>(
    connect: F,
    timeout_duration: Duration,
    host: &str,
    port: u16,
) -> Result<T, A2AError>
where
    F: Future<Output = Result<T, E>>,
    E: Display,
{
    match timeout(timeout_duration, connect).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(err)) => Err(A2AError::internal(format!(
            "failed to connect to {host}:{port}: {err}"
        ))),
        Err(_) => Err(A2AError::internal(format!(
            "timed out connecting to {host}:{port} after {timeout_duration:?}"
        ))),
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
                let _ = self.inner.try_send_outbound(OutboundClient::Frame(text));
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
            let port: u16 = port_str
                .parse()
                .map_err(|err| A2AError::internal(format!("invalid port '{port_str}': {err}")))?;
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
    mut outbound_rx: mpsc::Receiver<OutboundClient>,
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

    let mut pending = pending.lock();
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
        let mut pending = pending.lock();
        if let Some(tx) = pending.unary.remove(&id) {
            let _ = tx.send(Err(a2a_error));
        } else if let Some(tx) = pending.streaming.remove(&id) {
            let _ = tx.send(Err(a2a_error));
        }
        return;
    }

    if let Some(value) = envelope.event {
        let pending = pending.lock();
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
        let mut pending = pending.lock();
        pending.streaming.remove(&id);
        return;
    }

    if let Some(value) = envelope.result {
        let mut pending = pending.lock();
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
        req: &TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.call_unary(methods::CREATE_PUSH_CONFIG, params, req)
            .await
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
        self.call_unary(methods::LIST_PUSH_CONFIGS, params, req)
            .await
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
        self.inner.close().await;
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

    #[tokio::test]
    async fn connect_with_timeout_returns_successful_connection_result() {
        let result = connect_with_timeout(
            futures::future::ready(Ok::<_, std::io::Error>(())),
            Duration::from_secs(1),
            "example.com",
            80,
        )
        .await;

        assert_eq!(result.unwrap(), ());
    }

    #[tokio::test]
    async fn connect_with_timeout_maps_connection_errors() {
        let result = connect_with_timeout(
            futures::future::ready(Err::<(), _>(std::io::Error::new(
                std::io::ErrorKind::ConnectionRefused,
                "refused",
            ))),
            Duration::from_secs(1),
            "example.com",
            443,
        )
        .await;

        let err = result.unwrap_err();
        assert_eq!(err.code, error_code::INTERNAL_ERROR);
        assert!(err.message.contains("failed to connect to example.com:443"));
        assert!(err.message.contains("refused"));
    }

    #[tokio::test]
    async fn connect_with_timeout_fails_when_connection_attempt_hangs() {
        let result = connect_with_timeout(
            futures::future::pending::<Result<(), std::io::Error>>(),
            Duration::from_millis(0),
            "example.com",
            80,
        )
        .await;

        let err = result.unwrap_err();
        assert_eq!(err.code, error_code::INTERNAL_ERROR);
        assert!(
            err.message
                .contains("timed out connecting to example.com:80")
        );
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
        pending.lock().fail_all(A2AError::internal("dropped"));

        let (outbound, _outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
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
    fn connection_closed_error_uses_default_when_no_close_error_is_set() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let err = connection_closed_error(&pending);
        assert_eq!(err.code, error_code::INTERNAL_ERROR);
        assert_eq!(err.message, "websocket connection closed");
    }

    #[test]
    fn connection_closed_error_preserves_recorded_close_error() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        pending
            .lock()
            .fail_all(A2AError::invalid_request("bad close"));
        let err = connection_closed_error(&pending);
        assert_eq!(err.code, error_code::INVALID_REQUEST);
        assert_eq!(err.message, "bad close");
    }

    #[tokio::test]
    async fn connection_inner_send_outbound_succeeds_while_receiver_is_open() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (outbound, mut outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
        let inner = ConnectionInner { outbound, pending };

        inner
            .send_outbound(OutboundClient::Frame("{}".into()))
            .await
            .unwrap();

        assert!(matches!(
            outbound_rx.try_recv(),
            Ok(OutboundClient::Frame(_))
        ));
    }

    #[tokio::test]
    async fn connection_inner_send_outbound_returns_close_error_when_receiver_is_dropped() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        pending.lock().close_error = Some(A2AError::internal("closed earlier"));
        let (outbound, outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
        drop(outbound_rx);
        let inner = ConnectionInner { outbound, pending };

        let err = inner
            .send_outbound(OutboundClient::Close)
            .await
            .unwrap_err();
        assert_eq!(err.message, "closed earlier");
    }

    #[test]
    fn connection_inner_try_send_outbound_returns_error_when_buffer_is_full() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (outbound, _outbound_rx) = mpsc::channel::<OutboundClient>(1);
        let inner = ConnectionInner { outbound, pending };

        inner
            .try_send_outbound(OutboundClient::Frame("one".into()))
            .unwrap();
        let err = inner
            .try_send_outbound(OutboundClient::Frame("two".into()))
            .unwrap_err();

        assert_eq!(err.code, error_code::INTERNAL_ERROR);
        assert_eq!(err.message, "websocket connection closed");
    }

    #[test]
    fn register_and_deregister_streaming_updates_pending_map() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (outbound, _outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
        let inner = ConnectionInner {
            outbound,
            pending: pending.clone(),
        };

        let _rx = inner.register_streaming("stream-1").unwrap();
        assert!(pending.lock().streaming.contains_key("stream-1"));

        inner.deregister_streaming("stream-1");
        assert!(!pending.lock().streaming.contains_key("stream-1"));
    }

    #[tokio::test]
    async fn connection_inner_close_sends_close_message() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (outbound, mut outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
        let inner = ConnectionInner { outbound, pending };

        inner.close().await;

        assert!(matches!(outbound_rx.try_recv(), Ok(OutboundClient::Close)));
    }

    #[test]
    fn streaming_response_drop_sends_cancel_when_not_terminated() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (outbound, mut outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
        let inner = Arc::new(ConnectionInner {
            outbound,
            pending: pending.clone(),
        });
        let (tx, receiver) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();
        pending.lock().streaming.insert("s1".into(), tx);

        let stream = StreamingResponse {
            receiver,
            inner,
            id: "s1".into(),
            cancel_sent: false,
            terminated: false,
        };
        drop(stream);

        assert!(!pending.lock().streaming.contains_key("s1"));
        match outbound_rx.try_recv().unwrap() {
            OutboundClient::Frame(text) => {
                let envelope: WsRequestEnvelope = serde_json::from_str(&text).unwrap();
                assert_eq!(envelope.id, "s1");
                assert_eq!(envelope.cancel_stream, Some(true));
            }
            OutboundClient::Close => panic!("expected cancel frame"),
        }
    }

    #[test]
    fn streaming_response_drop_does_not_cancel_after_termination() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (outbound, mut outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
        let inner = Arc::new(ConnectionInner { outbound, pending });
        let (_tx, receiver) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();

        let stream = StreamingResponse {
            receiver,
            inner,
            id: "s1".into(),
            cancel_sent: false,
            terminated: true,
        };
        drop(stream);

        assert!(outbound_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn call_unary_raw_sends_envelope_and_routes_result() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (outbound, mut outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
        let transport = WebSocketTransport {
            inner: Arc::new(ConnectionInner {
                outbound,
                pending: pending.clone(),
            }),
        };
        let params = HashMap::from([(
            "x-trace".to_string(),
            vec!["a".to_string(), "b".to_string()],
        )]);

        let task = tokio::spawn(async move {
            transport
                .call_unary_raw(methods::GET_TASK, &params, serde_json::json!({"id": "t1"}))
                .await
        });

        let envelope = match outbound_rx.recv().await.unwrap() {
            OutboundClient::Frame(text) => {
                serde_json::from_str::<WsRequestEnvelope>(&text).unwrap()
            }
            OutboundClient::Close => panic!("expected request frame"),
        };
        assert_eq!(envelope.method.as_deref(), Some(methods::GET_TASK));
        assert_eq!(envelope.params.unwrap()["id"], "t1");
        assert_eq!(
            envelope.service_params.unwrap().get("x-trace").unwrap(),
            "a, b"
        );

        let tx = pending.lock().unary.remove(&envelope.id).unwrap();
        tx.send(Ok(serde_json::json!({"ok": true}))).unwrap();

        let value = task.await.unwrap().unwrap();
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn handle_incoming_text_dispatches_unary_result() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (tx, rx) = oneshot::channel::<Result<Value, A2AError>>();
        pending.lock().unary.insert("req-1".into(), tx);

        let response = WsResponseEnvelope::result("req-1", serde_json::json!({"ok": 1}));
        let json = serde_json::to_vec(&response).unwrap();
        handle_incoming_text(&json, &pending);

        let value = futures::executor::block_on(rx).unwrap().unwrap();
        assert_eq!(value["ok"], 1);
        assert!(pending.lock().unary.is_empty());
    }

    #[test]
    fn handle_incoming_text_dispatches_unary_error() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (tx, rx) = oneshot::channel::<Result<Value, A2AError>>();
        pending.lock().unary.insert("req-1".into(), tx);

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
    fn handle_incoming_text_dispatches_streaming_error_and_removes_sink() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();
        pending.lock().streaming.insert("req-2".into(), tx);

        let response = WsResponseEnvelope::error(
            Some("req-2".into()),
            crate::common::WsErrorObject {
                error_type: crate::common::error_types::INVALID_PARAMS.to_string(),
                message: "bad params".into(),
                details: None,
            },
        );
        let json = serde_json::to_vec(&response).unwrap();
        handle_incoming_text(&json, &pending);

        let err = rx.try_recv().unwrap().unwrap_err();
        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(!pending.lock().streaming.contains_key("req-2"));
    }

    #[test]
    fn handle_incoming_text_ignores_error_for_unknown_id() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let response = WsResponseEnvelope::error(
            Some("unknown".into()),
            crate::common::WsErrorObject {
                error_type: crate::common::error_types::INTERNAL.to_string(),
                message: "oops".into(),
                details: None,
            },
        );
        let json = serde_json::to_vec(&response).unwrap();

        handle_incoming_text(&json, &pending);

        assert!(pending.lock().unary.is_empty());
        assert!(pending.lock().streaming.is_empty());
    }

    #[test]
    fn handle_incoming_text_routes_stream_event_to_streaming_sink() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();
        pending.lock().streaming.insert("req-2".into(), tx);

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
        assert!(pending.lock().streaming.contains_key("req-2"));
    }

    #[test]
    fn handle_incoming_text_routes_bad_stream_event_as_error() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();
        pending.lock().streaming.insert("req-2".into(), tx);

        let response = WsResponseEnvelope::event("req-2", serde_json::json!({"unknown": {}}));
        let json = serde_json::to_vec(&response).unwrap();
        handle_incoming_text(&json, &pending);

        let err = rx.try_recv().unwrap().unwrap_err();
        assert_eq!(err.code, error_code::INTERNAL_ERROR);
        assert!(err.message.contains("failed to deserialize event"));
        assert!(pending.lock().streaming.contains_key("req-2"));
    }

    #[test]
    fn handle_incoming_text_ignores_event_for_unknown_id() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let response = WsResponseEnvelope::event("missing", serde_json::json!({"unknown": {}}));
        let json = serde_json::to_vec(&response).unwrap();

        handle_incoming_text(&json, &pending);

        assert!(pending.lock().streaming.is_empty());
    }

    #[test]
    fn handle_incoming_text_stream_end_removes_streaming_sink() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (tx, _rx) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();
        pending.lock().streaming.insert("req-2".into(), tx);

        let response = WsResponseEnvelope::stream_end("req-2");
        let json = serde_json::to_vec(&response).unwrap();
        handle_incoming_text(&json, &pending);

        assert!(!pending.lock().streaming.contains_key("req-2"));
    }

    #[test]
    fn handle_incoming_text_stream_end_unknown_id_is_noop() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let response = WsResponseEnvelope::stream_end("missing");
        let json = serde_json::to_vec(&response).unwrap();

        handle_incoming_text(&json, &pending);

        assert!(pending.lock().streaming.is_empty());
    }

    #[test]
    fn handle_incoming_text_result_for_unknown_id_is_noop() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let response = WsResponseEnvelope::result("missing", serde_json::json!({"ok": true}));
        let json = serde_json::to_vec(&response).unwrap();

        handle_incoming_text(&json, &pending);

        assert!(pending.lock().unary.is_empty());
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

    // ---------------------------------------------------------------------
    // Helpers for exercising the `Transport` trait methods without a real
    // websocket connection: we mock `ConnectionInner` and a background task
    // that listens for outbound envelopes and injects pre-canned results.
    // ---------------------------------------------------------------------

    fn make_mock_transport() -> (
        WebSocketTransport,
        Arc<Mutex<Pending>>,
        mpsc::Receiver<OutboundClient>,
    ) {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let (outbound, outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
        let transport = WebSocketTransport {
            inner: Arc::new(ConnectionInner {
                outbound,
                pending: pending.clone(),
            }),
        };
        (transport, pending, outbound_rx)
    }

    async fn respond_unary(
        outbound_rx: &mut mpsc::Receiver<OutboundClient>,
        pending: &Arc<Mutex<Pending>>,
        result: Value,
    ) -> WsRequestEnvelope {
        let envelope = match outbound_rx.recv().await.unwrap() {
            OutboundClient::Frame(text) => {
                serde_json::from_str::<WsRequestEnvelope>(&text).unwrap()
            }
            OutboundClient::Close => panic!("expected request frame"),
        };
        let tx = pending.lock().unary.remove(&envelope.id).unwrap();
        tx.send(Ok(result)).unwrap();
        envelope
    }

    #[tokio::test]
    async fn transport_send_message_dispatches_send_message_method() {
        let (transport, pending, mut outbound_rx) = make_mock_transport();
        let task_resp = SendMessageResponse::Task(Task {
            id: "t1".into(),
            context_id: "ctx".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        });
        let result_value = protojson_conv::to_value(&task_resp).unwrap();

        let handle = tokio::spawn(async move {
            let req = SendMessageRequest {
                message: Message::new(Role::User, vec![Part::text("hi")]),
                configuration: None,
                metadata: None,
                tenant: None,
            };
            transport
                .send_message(&ServiceParams::new(), &req)
                .await
                .unwrap()
        });

        let envelope = respond_unary(&mut outbound_rx, &pending, result_value).await;
        assert_eq!(envelope.method.as_deref(), Some(methods::SEND_MESSAGE));
        let resp = handle.await.unwrap();
        match resp {
            SendMessageResponse::Task(t) => assert_eq!(t.id, "t1"),
            _ => panic!("expected Task"),
        }
    }

    #[tokio::test]
    async fn transport_list_tasks_dispatches_list_tasks_method() {
        let (transport, pending, mut outbound_rx) = make_mock_transport();
        let listed = ListTasksResponse {
            tasks: vec![],
            next_page_token: "".into(),
            page_size: 0,
            total_size: 0,
        };
        let result_value = protojson_conv::to_value(&listed).unwrap();

        let handle = tokio::spawn(async move {
            let req = ListTasksRequest {
                context_id: None,
                status: None,
                page_size: None,
                page_token: None,
                history_length: None,
                status_timestamp_after: None,
                include_artifacts: None,
                tenant: None,
            };
            transport
                .list_tasks(&ServiceParams::new(), &req)
                .await
                .unwrap()
        });

        let envelope = respond_unary(&mut outbound_rx, &pending, result_value).await;
        assert_eq!(envelope.method.as_deref(), Some(methods::LIST_TASKS));
        let resp = handle.await.unwrap();
        assert_eq!(resp.total_size, 0);
    }

    #[tokio::test]
    async fn transport_cancel_task_dispatches_cancel_task_method() {
        let (transport, pending, mut outbound_rx) = make_mock_transport();
        let task = Task {
            id: "cancel".into(),
            context_id: "ctx".into(),
            status: TaskStatus {
                state: TaskState::Canceled,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        let result_value = protojson_conv::to_value(&task).unwrap();

        let handle = tokio::spawn(async move {
            let req = CancelTaskRequest {
                id: "cancel".into(),
                metadata: None,
                tenant: None,
            };
            transport
                .cancel_task(&ServiceParams::new(), &req)
                .await
                .unwrap()
        });

        let envelope = respond_unary(&mut outbound_rx, &pending, result_value).await;
        assert_eq!(envelope.method.as_deref(), Some(methods::CANCEL_TASK));
        let resp = handle.await.unwrap();
        assert_eq!(resp.id, "cancel");
    }

    #[tokio::test]
    async fn transport_create_push_config_dispatches_create_push_config_method() {
        let (transport, pending, mut outbound_rx) = make_mock_transport();
        let cfg = TaskPushNotificationConfig {
            url: "https://hook.example.test".into(),
            id: Some("cfg".into()),
            task_id: "t1".into(),
            token: None,
            authentication: None,
            tenant: None,
        };
        let result_value = protojson_conv::to_value(&cfg).unwrap();

        let handle = tokio::spawn(async move {
            let req = TaskPushNotificationConfig {
                url: "https://hook.example.test".into(),
                id: Some("cfg".into()),
                task_id: "t1".into(),
                token: None,
                authentication: None,
                tenant: None,
            };
            transport
                .create_push_config(&ServiceParams::new(), &req)
                .await
                .unwrap()
        });

        let envelope = respond_unary(&mut outbound_rx, &pending, result_value).await;
        assert_eq!(
            envelope.method.as_deref(),
            Some(methods::CREATE_PUSH_CONFIG)
        );
        let resp = handle.await.unwrap();
        assert_eq!(resp.task_id, "t1");
    }

    #[tokio::test]
    async fn transport_get_push_config_dispatches_get_push_config_method() {
        let (transport, pending, mut outbound_rx) = make_mock_transport();
        let cfg = TaskPushNotificationConfig {
            url: "https://hook.example.test".into(),
            id: Some("cfg".into()),
            task_id: "t1".into(),
            token: None,
            authentication: None,
            tenant: None,
        };
        let result_value = protojson_conv::to_value(&cfg).unwrap();

        let handle = tokio::spawn(async move {
            let req = GetTaskPushNotificationConfigRequest {
                task_id: "t1".into(),
                id: "cfg".into(),
                tenant: None,
            };
            transport
                .get_push_config(&ServiceParams::new(), &req)
                .await
                .unwrap()
        });

        let envelope = respond_unary(&mut outbound_rx, &pending, result_value).await;
        assert_eq!(envelope.method.as_deref(), Some(methods::GET_PUSH_CONFIG));
        let resp = handle.await.unwrap();
        assert_eq!(resp.task_id, "t1");
    }

    #[tokio::test]
    async fn transport_list_push_configs_dispatches_list_push_configs_method() {
        let (transport, pending, mut outbound_rx) = make_mock_transport();
        let listed = ListTaskPushNotificationConfigsResponse {
            configs: vec![],
            next_page_token: None,
        };
        let result_value = protojson_conv::to_value(&listed).unwrap();

        let handle = tokio::spawn(async move {
            let req = ListTaskPushNotificationConfigsRequest {
                task_id: "t1".into(),
                page_size: None,
                page_token: None,
                tenant: None,
            };
            transport
                .list_push_configs(&ServiceParams::new(), &req)
                .await
                .unwrap()
        });

        let envelope = respond_unary(&mut outbound_rx, &pending, result_value).await;
        assert_eq!(envelope.method.as_deref(), Some(methods::LIST_PUSH_CONFIGS));
        let resp = handle.await.unwrap();
        assert!(resp.configs.is_empty());
    }

    #[tokio::test]
    async fn transport_delete_push_config_dispatches_delete_push_config_method() {
        let (transport, pending, mut outbound_rx) = make_mock_transport();

        let handle = tokio::spawn(async move {
            let req = DeleteTaskPushNotificationConfigRequest {
                task_id: "t1".into(),
                id: "cfg".into(),
                tenant: None,
            };
            transport
                .delete_push_config(&ServiceParams::new(), &req)
                .await
                .unwrap();
        });

        let envelope = respond_unary(
            &mut outbound_rx,
            &pending,
            Value::Object(Default::default()),
        )
        .await;
        assert_eq!(
            envelope.method.as_deref(),
            Some(methods::DELETE_PUSH_CONFIG)
        );
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn transport_subscribe_to_task_dispatches_subscribe_method_and_streams_events() {
        use futures::StreamExt as _;
        let (transport, pending, mut outbound_rx) = make_mock_transport();

        let handle = tokio::spawn(async move {
            let req = SubscribeToTaskRequest {
                id: "t1".into(),
                tenant: None,
            };
            let mut stream = transport
                .subscribe_to_task(&ServiceParams::new(), &req)
                .await
                .unwrap();
            let first = stream.next().await.unwrap().unwrap();
            // Drop the stream to exercise cancel-on-drop path.
            drop(stream);
            first
        });

        let envelope = match outbound_rx.recv().await.unwrap() {
            OutboundClient::Frame(text) => {
                serde_json::from_str::<WsRequestEnvelope>(&text).unwrap()
            }
            OutboundClient::Close => panic!("expected request frame"),
        };
        assert_eq!(envelope.method.as_deref(), Some(methods::SUBSCRIBE_TO_TASK));
        // Inject a stream event.
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "ctx".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        {
            let p = pending.lock();
            let tx = p.streaming.get(&envelope.id).unwrap();
            tx.send(Ok(event)).unwrap();
        }

        let first = handle.await.unwrap();
        match first {
            StreamResponse::StatusUpdate(_) => {}
            _ => panic!("expected StatusUpdate"),
        }
    }

    #[tokio::test]
    async fn transport_destroy_emits_close_message() {
        let (transport, _pending, mut outbound_rx) = make_mock_transport();
        transport.destroy().await.unwrap();
        assert!(matches!(outbound_rx.try_recv(), Ok(OutboundClient::Close)));
    }

    #[tokio::test]
    async fn websocket_transport_factory_create_fails_for_unreachable_url() {
        let factory = WebSocketTransportFactory;
        let card = AgentCard {
            name: "x".into(),
            description: "".into(),
            version: "1".into(),
            supported_interfaces: vec![],
            capabilities: AgentCapabilities::default(),
            default_input_modes: vec![],
            default_output_modes: vec![],
            skills: vec![],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        };
        let iface = AgentInterface::new("ws://127.0.0.1:1", TRANSPORT_PROTOCOL_WEBSOCKET);
        let result = factory.create(&card, &iface).await;
        assert!(result.is_err());
    }

    #[test]
    fn streaming_response_poll_after_termination_returns_ready_none() {
        use futures::task::noop_waker;
        use std::task::{Context, Poll};

        let pending = Arc::new(Mutex::new(Pending::default()));
        let (outbound, _outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
        let inner = Arc::new(ConnectionInner { outbound, pending });
        let (_tx, receiver) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();
        let mut stream = StreamingResponse {
            receiver,
            inner,
            id: "s1".into(),
            cancel_sent: true,
            terminated: true,
        };

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Ready(None)
        ));
    }

    #[test]
    fn streaming_response_marks_terminated_when_receiver_closes() {
        use futures::task::noop_waker;
        use std::task::{Context, Poll};

        let pending = Arc::new(Mutex::new(Pending::default()));
        let (outbound, _outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
        let inner = Arc::new(ConnectionInner { outbound, pending });
        let (tx, receiver) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();
        drop(tx);
        let mut stream = StreamingResponse {
            receiver,
            inner,
            id: "s1".into(),
            cancel_sent: false,
            terminated: false,
        };

        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Ready(None)
        ));
        assert!(stream.terminated);
    }

    #[test]
    fn spawn_executor_executes_future_on_tokio_runtime() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (tx, rx) = oneshot::channel::<u32>();
            <SpawnExecutor as hyper::rt::Executor<_>>::execute(&SpawnExecutor, async move {
                tx.send(7).unwrap();
            });
            assert_eq!(rx.await.unwrap(), 7);
        });
    }
}
