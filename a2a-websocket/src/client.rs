// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;
use std::fmt::Display;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Weak};
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

use crate::auth::AuthenticateParams;
use crate::common::{
    CONTROL_REAUTH_REQUIRED, DEFAULT_MAX_FRAME_BYTES, JsonRpcId, SERVER_ERROR_CODE, SUBPROTOCOL,
    TRANSPORT_PROTOCOL_WEBSOCKET, WsRequestEnvelope, WsResponseEnvelope, close_codes, id_key,
    methods, service_params_to_envelope,
};
use crate::errors::jsonrpc_error_to_a2a;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const OUTBOUND_BUFFER_CAPACITY: usize = 64;

/// Details carried by a server-sent `ReauthenticationRequired` control frame
/// (spec Section 9.3.2).
#[derive(Debug, Clone, Default)]
pub struct ReauthRequired {
    /// Human-readable explanation supplied by the server, if any.
    pub reason: Option<String>,
    /// Suggested delay before reconnecting, in milliseconds.
    pub retry_after_ms: Option<u64>,
}

/// Source of fresh credentials for a live connection.
///
/// Implement this to let the transport satisfy a `ReauthenticationRequired`
/// signal by refreshing in-band (spec Section 9.3.3) rather than forcing the
/// application to rebuild the connection.
#[async_trait]
pub trait CredentialProvider: Send + Sync + 'static {
    /// Produce credentials to install on the connection. Called when the server
    /// asks for reauthentication.
    async fn fresh_credentials(&self) -> Result<AuthenticateParams, A2AError>;
}

/// How the client reacts to a `ReauthenticationRequired` control frame. Section
/// 9.3.2 requires clients to *handle* the signal, so a policy should be set on
/// any connection whose credentials can expire.
#[derive(Clone)]
pub enum ReauthPolicy {
    /// Invoke the callback so the application can obtain fresh credentials and
    /// establish a new connection itself.
    Notify(Arc<dyn Fn(ReauthRequired) + Send + Sync>),
    /// Fetch fresh credentials from the provider and install them on the live
    /// connection using the in-band `Authenticate` method (spec Section 9.3.3),
    /// avoiding a reconnect. A server that does not support in-band refresh
    /// answers `UnsupportedOperationError` and then closes with `4001`, so the
    /// application should still be prepared to reconnect.
    RefreshInBand(Arc<dyn CredentialProvider>),
}

impl std::fmt::Debug for ReauthPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReauthPolicy::Notify(_) => f.write_str("ReauthPolicy::Notify(..)"),
            ReauthPolicy::RefreshInBand(_) => f.write_str("ReauthPolicy::RefreshInBand(..)"),
        }
    }
}

/// Options controlling how a [`WebSocketTransport`] connects.
#[derive(Debug, Clone, Default)]
pub struct ConnectOptions {
    /// Extra HTTP headers to send on the upgrade handshake — typically the
    /// `Authorization` header carrying connection-scoped credentials
    /// (spec Section 9.1). Header names are sent as-is.
    pub headers: Vec<(String, String)>,

    /// TLS options applied when connecting to a `wss://` endpoint (requires the
    /// `tls` crate feature). Ignored for plaintext `ws://` connections.
    pub tls: TlsOptions,

    /// Response to a `ReauthenticationRequired` control frame (spec Section
    /// 9.3.2). When `None` the frame is only logged, and the server will
    /// subsequently close the connection with `4001`.
    pub reauth: Option<ReauthPolicy>,
}

impl ConnectOptions {
    /// Convenience constructor adding an `Authorization: Bearer <token>` header.
    pub fn with_bearer_token(token: impl AsRef<str>) -> Self {
        ConnectOptions {
            headers: vec![(
                "Authorization".to_string(),
                format!("Bearer {}", token.as_ref()),
            )],
            ..Default::default()
        }
    }

    /// Add an arbitrary header to the handshake request.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Replace the TLS options used for `wss://` connections.
    pub fn with_tls(mut self, tls: TlsOptions) -> Self {
        self.tls = tls;
        self
    }

    /// Be notified when the server requests reauthentication, so the
    /// application can reconnect with fresh credentials (spec Section 9.3.2).
    pub fn on_reauth_required<F>(mut self, callback: F) -> Self
    where
        F: Fn(ReauthRequired) + Send + Sync + 'static,
    {
        self.reauth = Some(ReauthPolicy::Notify(Arc::new(callback)));
        self
    }

    /// Refresh credentials in-band via the `Authenticate` method when the server
    /// requests reauthentication, keeping the connection open
    /// (spec Section 9.3.3).
    pub fn with_credential_provider(mut self, provider: Arc<dyn CredentialProvider>) -> Self {
        self.reauth = Some(ReauthPolicy::RefreshInBand(provider));
        self
    }
}

/// TLS configuration for `wss://` connections (spec Section 13.1).
///
/// By default the platform trust store is used (falling back to the bundled
/// Mozilla root set). Additional trusted roots may be supplied for private
/// CAs or self-signed certificates.
#[derive(Debug, Clone, Default)]
pub struct TlsOptions {
    /// Extra trusted root certificates, in PEM form, appended to the default
    /// trust anchors. Useful for private CAs or pinned self-signed certs.
    pub extra_root_certs_pem: Vec<Vec<u8>>,

    /// **DANGER:** disable server certificate verification entirely. This
    /// defeats the protection TLS provides against man-in-the-middle attacks
    /// and must only ever be used in tests against a local server. Never enable
    /// this in production.
    pub danger_accept_invalid_certs: bool,
}

impl TlsOptions {
    /// Trust an additional root certificate (PEM). May be called repeatedly.
    pub fn trust_pem(mut self, pem: impl Into<Vec<u8>>) -> Self {
        self.extra_root_certs_pem.push(pem.into());
        self
    }

    /// **DANGER:** disable certificate verification. Tests only. See
    /// [`TlsOptions::danger_accept_invalid_certs`].
    pub fn danger_accept_invalid_certs(mut self) -> Self {
        self.danger_accept_invalid_certs = true;
        self
    }
}

fn id_key_of(id: &str) -> String {
    id_key(&JsonRpcId::String(id.to_string()))
}

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
        key: &str,
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
        pending.unary.insert(key.to_string(), tx);
        Ok(rx)
    }

    fn register_streaming(
        &self,
        key: &str,
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
        pending.streaming.insert(key.to_string(), tx);
        Ok(rx)
    }

    fn deregister_streaming(&self, key: &str) {
        let mut pending = self.pending.lock();
        pending.streaming.remove(key);
    }

    async fn close(&self) {
        let _ = self.send_outbound(OutboundClient::Close).await;
    }

    /// Issue a unary JSON-RPC request and await its correlated response.
    async fn call_unary_raw(
        &self,
        method: &str,
        params: &ServiceParams,
        request_params: Value,
    ) -> Result<Value, A2AError> {
        let id = uuid::Uuid::now_v7().to_string();
        let key = id_key_of(&id);
        let envelope = WsRequestEnvelope {
            id: Some(JsonRpcId::String(id)),
            method: Some(method.to_string()),
            params: Some(request_params),
            service_params: service_params_to_envelope(params),
            ..Default::default()
        };

        let receiver = self.register_unary(&key)?;
        self.send_outbound(OutboundClient::Frame(
            serde_json::to_string(&envelope).map_err(|err| {
                A2AError::internal(format!("failed to serialize envelope: {err}"))
            })?,
        ))
        .await?;

        match receiver.await {
            Ok(result) => result,
            Err(_) => Err(connection_closed_error(&self.pending)),
        }
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
    /// Connect to the agent at the given endpoint URL with default options.
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self, A2AError> {
        Self::connect_with_options(endpoint, ConnectOptions::default()).await
    }

    /// Connect to the agent, supplying handshake headers (e.g. credentials)
    /// and, for `wss://`, TLS options.
    ///
    /// Accepts `ws://`, `wss://`, or bare `host:port[/path]` (normalized to
    /// `ws://`). `wss://` requires the `tls` crate feature (enabled by
    /// default).
    pub async fn connect_with_options(
        endpoint: impl Into<String>,
        options: ConnectOptions,
    ) -> Result<Self, A2AError> {
        let endpoint = endpoint.into();
        let parsed = parse_endpoint(&endpoint)?;

        let stream = connect_tcp(&parsed.host, parsed.port).await?;

        let host_header = if uses_default_port(&parsed.scheme, parsed.port) {
            parsed.host.clone()
        } else {
            format!("{}:{}", parsed.host, parsed.port)
        };

        let mut builder = Request::builder()
            .method("GET")
            .uri(parsed.path.clone())
            .header(HOST, host_header)
            .header(UPGRADE, "websocket")
            .header(CONNECTION, "upgrade")
            .header("Sec-WebSocket-Key", handshake::generate_key())
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Protocol", SUBPROTOCOL);
        for (name, value) in &options.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        let req = builder
            .body(Empty::<Bytes>::new())
            .map_err(|err| A2AError::internal(format!("failed to build upgrade request: {err}")))?;

        let (ws, response) = if parsed.scheme == "wss" {
            #[cfg(feature = "tls")]
            {
                let tls = tls::connect(&parsed.host, stream, &options.tls).await?;
                handshake::client(&SpawnExecutor, req, tls)
                    .await
                    .map_err(|err| {
                        A2AError::internal(format!("websocket handshake failed: {err}"))
                    })?
            }
            #[cfg(not(feature = "tls"))]
            {
                let _ = (stream, req);
                return Err(A2AError::internal(
                    "wss:// requires the `tls` feature of a2a-websocket to be enabled",
                ));
            }
        } else {
            handshake::client(&SpawnExecutor, req, stream)
                .await
                .map_err(|err| A2AError::internal(format!("websocket handshake failed: {err}")))?
        };

        if !response_subprotocol_matches(&response) {
            return Err(A2AError::internal(format!(
                "server did not negotiate the '{SUBPROTOCOL}' sub-protocol"
            )));
        }

        let (outbound_tx, outbound_rx) = mpsc::channel::<OutboundClient>(OUTBOUND_BUFFER_CAPACITY);
        let pending = Arc::new(Mutex::new(Pending::default()));
        let inner = Arc::new(ConnectionInner {
            outbound: outbound_tx,
            pending: pending.clone(),
        });

        // The read loop holds a weak reference so that dropping the transport
        // still closes the outbound channel and lets the loop exit.
        tokio::spawn(run_connection(
            ws,
            outbound_rx,
            pending,
            Arc::downgrade(&inner),
            options.reauth,
        ));

        Ok(WebSocketTransport { inner })
    }

    /// Refresh connection credentials in-band via the optional `Authenticate`
    /// method (spec Section 9.3.3). Returns `UnsupportedOperationError` if the
    /// server does not support in-band refresh.
    pub async fn authenticate(
        &self,
        scheme: impl Into<String>,
        credentials: impl Into<String>,
    ) -> Result<(), A2AError> {
        let params = serde_json::json!({
            "scheme": scheme.into(),
            "credentials": credentials.into(),
        });
        self.call_unary_raw(methods::AUTHENTICATE, &ServiceParams::new(), params)
            .await
            .map(|_| ())
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
        self.inner
            .call_unary_raw(method, params, request_params)
            .await
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
        let key = id_key_of(&id);
        let envelope = WsRequestEnvelope {
            id: Some(JsonRpcId::String(id.clone())),
            method: Some(method.to_string()),
            params: Some(payload),
            service_params: service_params_to_envelope(params),
            ..Default::default()
        };

        let receiver = self.inner.register_streaming(&key)?;
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
            id,
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
        let key = id_key_of(&self.id);
        if !self.cancel_sent && !self.terminated {
            self.cancel_sent = true;
            self.inner.deregister_streaming(&key);
            let envelope = WsRequestEnvelope {
                id: Some(JsonRpcId::String(self.id.clone())),
                cancel_stream: Some(true),
                ..Default::default()
            };
            if let Ok(text) = serde_json::to_string(&envelope) {
                let _ = self.inner.try_send_outbound(OutboundClient::Frame(text));
            }
        } else {
            self.inner.deregister_streaming(&key);
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
        Some(("wss", rest)) => ("wss".to_string(), rest),
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
    inner: Weak<ConnectionInner>,
    reauth: Option<ReauthPolicy>,
) {
    ws.set_max_message_size(DEFAULT_MAX_FRAME_BYTES);
    ws.set_auto_close(true);
    ws.set_auto_pong(true);
    let mut ws = FragmentCollector::new(ws);

    // Error reported to in-flight requests once the loop exits, refined by the
    // server's Close code when one is received.
    let mut close_error = A2AError::internal("websocket connection closed");

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
                        OpCode::Close => {
                            close_error = close_error_for(&frame.payload);
                            break;
                        }
                        OpCode::Text => {
                            handle_incoming_text(&frame.payload, &pending, &inner, &reauth);
                        }
                        OpCode::Binary => {
                            tracing::debug!(
                                "received unexpected binary frame from server; closing"
                            );
                            let _ = ws
                                .write_frame(Frame::close(
                                    close_codes::UNSUPPORTED_DATA,
                                    b"binary frames are reserved for future use",
                                ))
                                .await;
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
    pending.fail_all(close_error);
}

/// Translate a Close frame into the error surfaced to in-flight requests, so an
/// authentication close is distinguishable from an ordinary disconnect.
fn close_error_for(payload: &[u8]) -> A2AError {
    let Some(code) = payload
        .get(..2)
        .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
    else {
        return A2AError::internal("websocket connection closed");
    };
    match code {
        close_codes::AUTHENTICATION_REQUIRED => A2AError::new(
            SERVER_ERROR_CODE,
            "connection closed with 4001: reauthentication required, reconnect with fresh credentials",
        ),
        close_codes::POLICY_VIOLATION => A2AError::new(
            SERVER_ERROR_CODE,
            "connection closed with 1008: policy violation (for example a rate limit)",
        ),
        close_codes::MESSAGE_TOO_BIG => A2AError::new(
            SERVER_ERROR_CODE,
            "connection closed with 1009: message exceeded the server's maximum size",
        ),
        close_codes::VERSION_NOT_SUPPORTED => A2AError::new(
            SERVER_ERROR_CODE,
            "connection closed with 4002: the requested A2A protocol version is not supported",
        ),
        close_codes::NORMAL_CLOSURE => A2AError::internal("websocket connection closed"),
        other => A2AError::internal(format!("websocket connection closed with code {other}")),
    }
}

/// Act on a `ReauthenticationRequired` control frame per the configured policy
/// (spec Section 9.3.2). The work is spawned because the read loop must keep
/// running to receive the `Authenticate` response.
fn handle_reauth_required(
    envelope: &WsResponseEnvelope,
    inner: &Weak<ConnectionInner>,
    reauth: &Option<ReauthPolicy>,
) {
    let request = ReauthRequired {
        reason: envelope.reason.clone(),
        retry_after_ms: envelope.retry_after_ms,
    };

    match reauth {
        Some(ReauthPolicy::Notify(callback)) => {
            let callback = callback.clone();
            tokio::spawn(async move { callback(request) });
        }
        Some(ReauthPolicy::RefreshInBand(provider)) => {
            let provider = provider.clone();
            let inner = inner.clone();
            tokio::spawn(async move {
                let credentials = match provider.fresh_credentials().await {
                    Ok(credentials) => credentials,
                    Err(err) => {
                        tracing::warn!(
                            error = %err.message,
                            "credential provider failed; the server will close this connection"
                        );
                        return;
                    }
                };
                let Some(inner) = inner.upgrade() else { return };
                let params = serde_json::json!({
                    "scheme": credentials.scheme,
                    "credentials": credentials.credentials,
                });
                match inner
                    .call_unary_raw(methods::AUTHENTICATE, &ServiceParams::new(), params)
                    .await
                {
                    Ok(_) => tracing::debug!("refreshed connection credentials in-band"),
                    Err(err) => tracing::warn!(
                        error = %err.message,
                        "in-band credential refresh failed; reconnect with fresh credentials"
                    ),
                }
            });
        }
        None => tracing::warn!(
            reason = request.reason.as_deref().unwrap_or_default(),
            "server requested reauthentication but no ReauthPolicy is configured; \
             set ConnectOptions::on_reauth_required or ::with_credential_provider"
        ),
    }
}

fn handle_incoming_text(
    payload: &[u8],
    pending: &Arc<Mutex<Pending>>,
    inner: &Weak<ConnectionInner>,
    reauth: &Option<ReauthPolicy>,
) {
    let envelope: WsResponseEnvelope = match serde_json::from_slice(payload) {
        Ok(env) => env,
        Err(err) => {
            tracing::debug!(error = %err, "failed to parse incoming envelope");
            return;
        }
    };

    // Server-originated control frames (spec Section 9.3.2) carry no id.
    if let Some(control) = envelope.control.as_deref() {
        if control == CONTROL_REAUTH_REQUIRED {
            handle_reauth_required(&envelope, inner, reauth);
        } else {
            tracing::debug!(control, "received unknown server control frame");
        }
        return;
    }

    let Some(id) = envelope.id.clone() else {
        if let Some(error) = envelope.error {
            tracing::warn!(error = %error.message, "received unrouted server error");
        }
        return;
    };
    let key = id_key(&id);

    if let Some(error) = envelope.error {
        let a2a_error = jsonrpc_error_to_a2a(&error);
        let mut pending = pending.lock();
        if let Some(tx) = pending.unary.remove(&key) {
            let _ = tx.send(Err(a2a_error));
        } else if let Some(tx) = pending.streaming.remove(&key) {
            let _ = tx.send(Err(a2a_error));
        }
        return;
    }

    if envelope.stream_end.unwrap_or(false) {
        let mut pending = pending.lock();
        pending.streaming.remove(&key);
        return;
    }

    // A `result` frame is either a unary response or a single streaming chunk;
    // the registry the id lives in disambiguates (spec Section 3.4).
    if let Some(value) = envelope.result {
        let mut pending = pending.lock();
        if let Some(tx) = pending.unary.remove(&key) {
            let _ = tx.send(Ok(value));
        } else if let Some(tx) = pending.streaming.get(&key) {
            match protojson_conv::from_value::<StreamResponse>(value) {
                Ok(sr) => {
                    let _ = tx.send(Ok(sr));
                }
                Err(err) => {
                    let _ = tx.send(Err(A2AError::internal(format!(
                        "failed to deserialize streaming result: {err}"
                    ))));
                }
            }
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

/// `wss://` (TLS) support, built on rustls with the `ring` crypto provider.
#[cfg(feature = "tls")]
mod tls {
    use std::sync::Arc;

    use a2a::A2AError;
    use rustls::pki_types::{CertificateDer, ServerName};
    use tokio::net::TcpStream;
    use tokio_rustls::{TlsConnector, client::TlsStream};

    use super::TlsOptions;

    /// Establish a TLS session over an existing TCP stream, validating the
    /// server certificate against the configured trust anchors.
    pub(super) async fn connect(
        host: &str,
        stream: TcpStream,
        options: &TlsOptions,
    ) -> Result<TlsStream<TcpStream>, A2AError> {
        let config = build_client_config(options)?;
        let connector = TlsConnector::from(Arc::new(config));
        let server_name = ServerName::try_from(host.to_string())
            .map_err(|_| A2AError::internal(format!("invalid TLS server name: {host}")))?;
        connector
            .connect(server_name, stream)
            .await
            .map_err(|err| A2AError::internal(format!("TLS handshake failed: {err}")))
    }

    pub(super) fn build_client_config(
        options: &TlsOptions,
    ) -> Result<rustls::ClientConfig, A2AError> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|err| A2AError::internal(format!("failed to configure TLS: {err}")))?;

        if options.danger_accept_invalid_certs {
            tracing::warn!(
                "TLS certificate verification is DISABLED (danger_accept_invalid_certs); \
                 never enable this outside of tests"
            );
            return Ok(builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(danger::NoVerification::new(provider)))
                .with_no_client_auth());
        }

        let mut roots = rustls::RootCertStore::empty();
        let native = rustls_native_certs::load_native_certs();
        for cert in native.certs {
            // Skip individual malformed platform certs rather than failing hard.
            let _ = roots.add(cert);
        }
        if roots.is_empty() {
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        }
        for cert in parse_pem_certs(&options.extra_root_certs_pem)? {
            roots
                .add(cert)
                .map_err(|err| A2AError::internal(format!("invalid root certificate: {err}")))?;
        }

        Ok(builder.with_root_certificates(roots).with_no_client_auth())
    }

    fn parse_pem_certs(pems: &[Vec<u8>]) -> Result<Vec<CertificateDer<'static>>, A2AError> {
        let mut out = Vec::new();
        for pem in pems {
            let mut reader = std::io::BufReader::new(pem.as_slice());
            for item in rustls_pemfile::certs(&mut reader) {
                let cert = item.map_err(|err| {
                    A2AError::internal(format!("failed to parse PEM certificate: {err}"))
                })?;
                out.push(cert);
            }
        }
        Ok(out)
    }

    mod danger {
        use std::sync::Arc;

        use rustls::client::danger::{
            HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
        };
        use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
        use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
        use rustls::{DigitallySignedStruct, Error, SignatureScheme};

        /// A verifier that accepts any certificate. Signatures are still checked
        /// against the crypto provider, but the certificate chain and hostname
        /// are NOT validated. Testing only.
        #[derive(Debug)]
        pub(super) struct NoVerification(Arc<CryptoProvider>);

        impl NoVerification {
            pub(super) fn new(provider: Arc<CryptoProvider>) -> Self {
                Self(provider)
            }
        }

        impl ServerCertVerifier for NoVerification {
            fn verify_server_cert(
                &self,
                _end_entity: &CertificateDer<'_>,
                _intermediates: &[CertificateDer<'_>],
                _server_name: &ServerName<'_>,
                _ocsp_response: &[u8],
                _now: UnixTime,
            ) -> Result<ServerCertVerified, Error> {
                Ok(ServerCertVerified::assertion())
            }

            fn verify_tls12_signature(
                &self,
                message: &[u8],
                cert: &CertificateDer<'_>,
                dss: &DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, Error> {
                verify_tls12_signature(
                    message,
                    cert,
                    dss,
                    &self.0.signature_verification_algorithms,
                )
            }

            fn verify_tls13_signature(
                &self,
                message: &[u8],
                cert: &CertificateDer<'_>,
                dss: &DigitallySignedStruct,
            ) -> Result<HandshakeSignatureValid, Error> {
                verify_tls13_signature(
                    message,
                    cert,
                    dss,
                    &self.0.signature_verification_algorithms,
                )
            }

            fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                self.0.signature_verification_algorithms.supported_schemes()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Route a response envelope through the read loop's dispatcher, without a
    /// live connection or reauthentication policy.
    fn dispatch(envelope: &WsResponseEnvelope, pending: &Arc<Mutex<Pending>>) {
        handle_incoming_text(
            &serde_json::to_vec(envelope).unwrap(),
            pending,
            &Weak::new(),
            &None,
        );
    }

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
    fn parse_endpoint_uses_default_path_and_port_when_missing() {
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
    fn parse_endpoint_accepts_wss_with_default_port() {
        let parsed = parse_endpoint("wss://example.com/a2a/ws").unwrap();
        assert_eq!(parsed.scheme, "wss");
        assert_eq!(parsed.host, "example.com");
        assert_eq!(parsed.port, 443);
        assert_eq!(parsed.path, "/a2a/ws");
    }

    #[cfg(feature = "tls")]
    #[test]
    fn tls_client_config_builds_with_default_roots() {
        let cfg = super::tls::build_client_config(&TlsOptions::default());
        assert!(cfg.is_ok(), "default TLS config should build");
    }

    #[test]
    fn parse_endpoint_rejects_unknown_scheme_and_empty_host() {
        assert!(
            parse_endpoint("http://example.com")
                .unwrap_err()
                .message
                .contains("unsupported scheme")
        );
        assert!(
            parse_endpoint("ws:///path")
                .unwrap_err()
                .message
                .contains("missing a host")
        );
    }

    #[test]
    fn connect_options_bearer_token_builds_authorization_header() {
        let opts = ConnectOptions::with_bearer_token("tok-123");
        assert_eq!(opts.headers.len(), 1);
        assert_eq!(opts.headers[0].0, "Authorization");
        assert_eq!(opts.headers[0].1, "Bearer tok-123");
    }

    #[test]
    fn response_subprotocol_matches_recognises_negotiated_protocol() {
        let response = http::Response::builder()
            .status(101)
            .header("Sec-WebSocket-Protocol", SUBPROTOCOL)
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

        assert!(futures::executor::block_on(urx).unwrap().is_err());
        assert!(srx.try_recv().unwrap().is_err());
        assert!(pending.closed);
    }

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

    #[tokio::test]
    async fn call_unary_raw_sends_jsonrpc_envelope_and_routes_result() {
        let (transport, pending, mut outbound_rx) = make_mock_transport();
        let params = HashMap::from([(
            "x-trace".to_string(),
            vec!["a".to_string(), "b".to_string()],
        )]);

        let task = tokio::spawn(async move {
            transport
                .call_unary_raw(methods::GET_TASK, &params, serde_json::json!({"id": "t1"}))
                .await
        });

        let (envelope, key) = match outbound_rx.recv().await.unwrap() {
            OutboundClient::Frame(text) => {
                let env: WsRequestEnvelope = serde_json::from_str(&text).unwrap();
                let key = id_key(env.id.as_ref().unwrap());
                (env, key)
            }
            OutboundClient::Close => panic!("expected request frame"),
        };
        assert_eq!(envelope.jsonrpc, "2.0");
        assert_eq!(envelope.method.as_deref(), Some(methods::GET_TASK));
        assert_eq!(
            envelope.service_params.unwrap().get("x-trace").unwrap(),
            "a, b"
        );

        let tx = pending.lock().unary.remove(&key).unwrap();
        tx.send(Ok(serde_json::json!({"ok": true}))).unwrap();

        let value = task.await.unwrap().unwrap();
        assert_eq!(value["ok"], true);
    }

    #[test]
    fn handle_incoming_text_dispatches_unary_result_and_error() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let key = id_key_of("req-1");
        let (tx, rx) = oneshot::channel::<Result<Value, A2AError>>();
        pending.lock().unary.insert(key.clone(), tx);

        let response = WsResponseEnvelope::result("req-1".into(), serde_json::json!({"ok": 1}));
        dispatch(&response, &pending);
        assert_eq!(futures::executor::block_on(rx).unwrap().unwrap()["ok"], 1);

        // Error routing with numeric JSON-RPC code.
        let (tx, rx) = oneshot::channel::<Result<Value, A2AError>>();
        pending.lock().unary.insert(key, tx);
        let err_resp = WsResponseEnvelope::error(
            Some("req-1".into()),
            a2a::jsonrpc::JsonRpcError {
                code: error_code::TASK_NOT_FOUND,
                message: "missing".into(),
                data: None,
            },
        );
        dispatch(&err_resp, &pending);
        let err = futures::executor::block_on(rx).unwrap().unwrap_err();
        assert_eq!(err.code, error_code::TASK_NOT_FOUND);
    }

    #[test]
    fn handle_incoming_text_routes_stream_result_chunk() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let key = id_key_of("req-2");
        let (tx, mut rx) = mpsc::unbounded_channel::<Result<StreamResponse, A2AError>>();
        pending.lock().streaming.insert(key.clone(), tx);

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
        let response = WsResponseEnvelope::stream_chunk(
            "req-2".into(),
            protojson_conv::to_value(&event).unwrap(),
        );
        dispatch(&response, &pending);

        assert!(matches!(
            rx.try_recv().unwrap().unwrap(),
            StreamResponse::StatusUpdate(_)
        ));
        // Sink stays registered until streamEnd or error.
        assert!(pending.lock().streaming.contains_key(&key));

        // streamEnd removes the sink.
        let end = WsResponseEnvelope::stream_end("req-2".into());
        dispatch(&end, &pending);
        assert!(!pending.lock().streaming.contains_key(&key));
    }

    #[test]
    fn reauth_control_frame_without_a_policy_is_ignored() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let frame = WsResponseEnvelope::reauth_required("expiring", 0);
        dispatch(&frame, &pending);
        assert!(pending.lock().unary.is_empty());
        assert!(pending.lock().streaming.is_empty());
    }

    #[tokio::test]
    async fn reauth_control_frame_invokes_the_notify_callback() {
        let pending = Arc::new(Mutex::new(Pending::default()));
        let seen = Arc::new(Mutex::new(None::<ReauthRequired>));
        let sink = seen.clone();
        let policy = Some(ReauthPolicy::Notify(Arc::new(move |request| {
            *sink.lock() = Some(request);
        })));

        let frame = WsResponseEnvelope::reauth_required("token expiring", 5_000);
        handle_incoming_text(
            &serde_json::to_vec(&frame).unwrap(),
            &pending,
            &Weak::new(),
            &policy,
        );

        // The callback runs on a spawned task, so yield until it lands.
        for _ in 0..100 {
            if seen.lock().is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
        let request = seen.lock().clone().expect("callback should have been run");
        assert_eq!(request.reason.as_deref(), Some("token expiring"));
        assert_eq!(request.retry_after_ms, Some(5_000));
    }

    #[test]
    fn close_error_distinguishes_reauth_from_an_ordinary_disconnect() {
        let reauth = close_error_for(&close_codes::AUTHENTICATION_REQUIRED.to_be_bytes());
        assert_eq!(reauth.code, SERVER_ERROR_CODE);
        assert!(
            reauth.message.contains("4001"),
            "the close code should be visible to the caller: {}",
            reauth.message
        );

        let rate_limited = close_error_for(&close_codes::POLICY_VIOLATION.to_be_bytes());
        assert!(rate_limited.message.contains("1008"));

        // A normal closure and an empty payload are both plain disconnects.
        assert_eq!(
            close_error_for(&close_codes::NORMAL_CLOSURE.to_be_bytes()).message,
            close_error_for(&[]).message
        );
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
        pending.lock().streaming.insert(id_key_of("s1"), tx);

        let stream = StreamingResponse {
            receiver,
            inner,
            id: "s1".into(),
            cancel_sent: false,
            terminated: false,
        };
        drop(stream);

        assert!(!pending.lock().streaming.contains_key(&id_key_of("s1")));
        match outbound_rx.try_recv().unwrap() {
            OutboundClient::Frame(text) => {
                let envelope: WsRequestEnvelope = serde_json::from_str(&text).unwrap();
                assert_eq!(envelope.id, Some(JsonRpcId::String("s1".into())));
                assert_eq!(envelope.cancel_stream, Some(true));
            }
            OutboundClient::Close => panic!("expected cancel frame"),
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
        assert!(factory.create(&card, &iface).await.is_err());
    }
}
