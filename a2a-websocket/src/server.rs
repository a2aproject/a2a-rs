// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use a2a::*;
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use a2a_server::RequestHandler;
use a2a_server::middleware::{ServiceParams, check_a2a_version};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use fastwebsockets::{
    FragmentCollectorRead, Frame, OpCode, Payload, WebSocketError, WebSocketWrite,
    upgrade::IncomingUpgrade,
};
use futures::stream::{BoxStream, StreamExt};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::io::WriteHalf;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::auth::{AuthContext, AuthStatus, AuthenticateParams, WsAuthenticator};
use crate::common::{
    DEFAULT_MAX_FRAME_BYTES, JSONRPC_VERSION, JsonRpcId, SUBPROTOCOL, StreamControlParams,
    WsRequestEnvelope, WsResponseEnvelope, binding_errors, close_codes, id_key, methods,
    service_params_from_envelope,
};
use crate::errors::{a2a_error_to_jsonrpc, binding_error, close_code_for_read_error};
use crate::limits::{ConnectionLimitPolicy, IdentityConnectionCounter};
use crate::liveness::{ActivityTracker, Liveness, LivenessPolicy};
use crate::ratelimit::{ConnectionRateLimiter, IdentityRateLimiter, RateLimitPolicy};

const SEC_WEBSOCKET_PROTOCOL: &str = "sec-websocket-protocol";
const OUTBOUND_BUFFER_CAPACITY: usize = 64;

/// Service parameter key of the A2A protocol version, lowercased.
const VERSION_KEY: &str = "a2a-version";

/// Payload carried by keep-alive pings, so a pong can be recognized in a capture.
const PING_PAYLOAD: &[u8] = b"a2a-keepalive";

/// Error message returned when a connection already has its maximum number of
/// active streams (spec Section 8.6).
const TOO_MANY_STREAMS: &str = "Too many concurrent streams";

/// Error message returned when an inbound rate limit is exceeded (spec
/// Section 7.3).
const RATE_LIMIT_EXCEEDED: &str = "Rate limit exceeded";

/// Failed `ws/Authenticate` attempts allowed per connection before it is closed
/// with `1008` (spec Section 9.3).
const MAX_FAILED_AUTHENTICATE_ATTEMPTS: u32 = 3;

/// Messages that are not valid JSON tolerated per connection before it is
/// closed with `4000` (spec Section 3.3).
const MAX_MALFORMED_MESSAGES: u32 = 10;

/// Server configuration for the WebSocket binding.
#[derive(Clone, Default)]
pub struct WebSocketConfig {
    /// Optional handshake authenticator (spec Section 9). When `None`, the
    /// binding performs no authentication and treats every connection as
    /// anonymous — appropriate only when a trusted layer in front of the agent
    /// handles authentication.
    pub authenticator: Option<Arc<dyn WsAuthenticator>>,
    /// Optional allowlist of permitted `Origin` header values (spec Section
    /// 9.7, cross-site WebSocket hijacking mitigation). When `Some`, upgrades
    /// whose `Origin` is missing or not listed are rejected with `403`. Set it
    /// whenever the authenticator accepts credentials a browser sends
    /// automatically, such as cookies.
    pub allowed_origins: Option<Vec<String>>,
    /// Maximum accepted size of a single inbound message. A larger message is
    /// rejected by closing the connection with code `1009` (spec Section 3.6).
    /// `None` applies the spec's recommended [`DEFAULT_MAX_FRAME_BYTES`].
    pub max_frame_bytes: Option<usize>,
    /// Inbound message rate limit, enforced per-connection and per-authenticated
    /// identity (spec Section 7.4). Defaults to [`DEFAULT_RATE_LIMIT`]; switch
    /// it off explicitly with [`RateLimitPolicy::Disabled`].
    ///
    /// [`DEFAULT_RATE_LIMIT`]: crate::ratelimit::DEFAULT_RATE_LIMIT
    pub rate_limit: RateLimitPolicy,
    /// Keep-alive pings and idle timeouts (spec Section 2.2). Defaults to
    /// [`DEFAULT_LIVENESS`]; switch it off explicitly with
    /// [`LivenessPolicy::Disabled`].
    ///
    /// [`DEFAULT_LIVENESS`]: crate::liveness::DEFAULT_LIVENESS
    pub liveness: LivenessPolicy,
    /// Caps on concurrent connections per identity and concurrent streams per
    /// connection (spec Section 8.6). Defaults to
    /// [`DEFAULT_CONNECTION_LIMITS`]; switch them off explicitly with
    /// [`ConnectionLimitPolicy::Disabled`].
    ///
    /// [`DEFAULT_CONNECTION_LIMITS`]: crate::limits::DEFAULT_CONNECTION_LIMITS
    pub connection_limits: ConnectionLimitPolicy,
}

impl WebSocketConfig {
    /// The effective inbound message size limit (spec Section 3.6).
    fn max_frame_bytes(&self) -> usize {
        self.max_frame_bytes.unwrap_or(DEFAULT_MAX_FRAME_BYTES)
    }
}

/// Shared state for the WebSocket binding handler.
pub struct WebSocketState<H: RequestHandler> {
    pub handler: Arc<H>,
    pub config: Arc<WebSocketConfig>,
    /// Rate-limit buckets keyed by authenticated identity, shared across every
    /// connection served by this router (spec Section 7.4).
    pub identity_rate_limiter: Option<Arc<IdentityRateLimiter>>,
    /// Live connection counts keyed by authenticated identity, shared across
    /// every connection served by this router (spec Section 8.6).
    pub identity_connections: Option<Arc<IdentityConnectionCounter>>,
}

impl<H: RequestHandler> Clone for WebSocketState<H> {
    fn clone(&self) -> Self {
        WebSocketState {
            handler: self.handler.clone(),
            config: self.config.clone(),
            identity_rate_limiter: self.identity_rate_limiter.clone(),
            identity_connections: self.identity_connections.clone(),
        }
    }
}

/// Per-connection authentication state (spec Section 9.4).
struct ConnAuth {
    authenticator: Option<Arc<dyn WsAuthenticator>>,
    ctx: StdMutex<AuthContext>,
    /// Service parameters derived from the handshake and the *current* context.
    /// Held here rather than alongside the connection loop so that an in-band
    /// refresh replaces the parameters the handler sees at the same moment it
    /// replaces the credentials (Section 9.3).
    params: StdMutex<Arc<ConnectionParams>>,
    /// Set once `ws/ReauthenticationRequired` has been sent for the current
    /// credentials, so it is sent only once.
    reauth_signaled: AtomicBool,
    /// Bumped by every successful `ws/Authenticate`, so a close scheduled for
    /// the previous credentials' expiry can tell it has been superseded.
    generation: AtomicU64,
    failed_authenticate_attempts: AtomicU32,
}

impl ConnAuth {
    fn params(&self) -> Arc<ConnectionParams> {
        self.params.lock().unwrap().clone()
    }
}

/// Build an `axum::Router` exposing the A2A WebSocket binding without
/// authentication.
///
/// Mount the router under whatever path your application uses (e.g.
/// `Router::new().nest("/a2a/ws", websocket_router(handler))`).
pub fn websocket_router<H: RequestHandler>(handler: Arc<H>) -> axum::Router {
    websocket_router_with_config(handler, WebSocketConfig::default())
}

/// Build a router that authenticates every connection during the upgrade
/// handshake using the supplied [`WsAuthenticator`] (spec Section 9).
pub fn websocket_router_with_auth<H: RequestHandler>(
    handler: Arc<H>,
    authenticator: Arc<dyn WsAuthenticator>,
) -> axum::Router {
    websocket_router_with_config(
        handler,
        WebSocketConfig {
            authenticator: Some(authenticator),
            ..Default::default()
        },
    )
}

/// Build a router with a fully specified [`WebSocketConfig`].
pub fn websocket_router_with_config<H: RequestHandler>(
    handler: Arc<H>,
    config: WebSocketConfig,
) -> axum::Router {
    let identity_rate_limiter = config
        .rate_limit
        .limit()
        .map(|limit| Arc::new(IdentityRateLimiter::new(limit)));
    let identity_connections = config.connection_limits.limits().map(|limits| {
        Arc::new(IdentityConnectionCounter::new(
            limits.max_connections_per_identity,
        ))
    });
    let state = WebSocketState {
        handler,
        config: Arc::new(config),
        identity_rate_limiter,
        identity_connections,
    };
    axum::Router::new()
        .route("/", axum::routing::any(handle_upgrade::<H>))
        .with_state(state)
}

async fn handle_upgrade<H: RequestHandler>(
    State(state): State<WebSocketState<H>>,
    uri: Uri,
    headers: HeaderMap,
    upgrade: IncomingUpgrade,
) -> Response {
    if !subprotocol_is_negotiated(&headers) {
        return (
            StatusCode::BAD_REQUEST,
            format!("Sec-WebSocket-Protocol header must include '{SUBPROTOCOL}'"),
        )
            .into_response();
    }

    // Origin validation (spec Section 9.7) — only enforced when an allowlist
    // is configured.
    if let Some(allowed) = state.config.allowed_origins.as_ref() {
        if !origin_allowed(&headers, allowed) {
            return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
        }
    }

    // `A2A-Version` is connection-scoped: an absent value means 0.3, and an
    // unsupported version rejects the upgrade (spec Section 4.1).
    let base_params = capture_connection_params(&headers, &uri);
    if let Err(err) = check_a2a_version(&base_params) {
        tracing::debug!(error = %err.message, "websocket upgrade with unsupported A2A-Version");
        return (StatusCode::BAD_REQUEST, err.message).into_response();
    }

    // Handshake authentication (spec Section 9.2). On failure the upgrade is
    // rejected with 401/403 and never reaches the 101 response.
    let auth_ctx = match state.config.authenticator.as_ref() {
        Some(authenticator) => match authenticator.authenticate(&headers).await {
            Ok(ctx) => ctx,
            Err(err) => {
                let status = StatusCode::from_u16(err.status).unwrap_or(StatusCode::UNAUTHORIZED);
                tracing::debug!(status = err.status, "websocket handshake auth rejected");
                return (status, err.message).into_response();
            }
        },
        None => AuthContext::default(),
    };

    // Per-identity connection cap. Checked after
    // authentication, since an identity is what the cap is attributed to, and
    // before the 101 response so an over-quota client is told to retry rather
    // than handed a connection that is immediately closed. Anonymous
    // connections are not counted — there is no identity to charge them to.
    let connection_slot = match (
        state.identity_connections.as_ref(),
        auth_ctx.user.as_ref().map(|user| user.name.as_str()),
    ) {
        (Some(counter), Some(identity)) => match counter.try_acquire(identity) {
            Some(slot) => Some(slot),
            None => {
                tracing::debug!(
                    identity,
                    "per-identity connection limit reached; refusing upgrade"
                );
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    "too many concurrent connections for this identity",
                )
                    .into_response();
            }
        },
        _ => None,
    };

    let connection_params = ConnectionParams::from_base(base_params, &auth_ctx);

    let (mut response, fut) = match upgrade.upgrade() {
        Ok(pair) => pair,
        Err(err) => {
            tracing::warn!(error = %err, "websocket upgrade rejected");
            return (StatusCode::BAD_REQUEST, "websocket upgrade failed").into_response();
        }
    };

    response.headers_mut().insert(
        header::HeaderName::from_static(SEC_WEBSOCKET_PROTOCOL),
        HeaderValue::from_static(SUBPROTOCOL),
    );

    let handler = state.handler.clone();
    let max_frame_bytes = state.config.max_frame_bytes();
    // The per-identity scope only applies to authenticated connections; an
    // anonymous connection is limited by its own bucket alone.
    let rate_limiter = state.config.rate_limit.limit().map(|limit| {
        ConnectionRateLimiter::new(
            limit,
            auth_ctx.user.as_ref().map(|user| user.name.clone()),
            state.identity_rate_limiter.clone(),
        )
    });
    let liveness = state.config.liveness.liveness();
    let max_streams = state
        .config
        .connection_limits
        .limits()
        .map(|limits| limits.max_streams_per_connection);
    let conn_auth = Arc::new(ConnAuth {
        authenticator: state.config.authenticator.clone(),
        ctx: StdMutex::new(auth_ctx),
        params: StdMutex::new(Arc::new(connection_params)),
        reauth_signaled: AtomicBool::new(false),
        generation: AtomicU64::new(0),
        failed_authenticate_attempts: AtomicU32::new(0),
    });
    tokio::spawn(async move {
        // Held for the life of the connection so the identity's slot is
        // released however the connection ends.
        let _connection_slot = connection_slot;
        match fut.await {
            Ok(ws) => {
                run_connection(
                    ws,
                    handler,
                    conn_auth,
                    ConnectionOptions {
                        max_frame_bytes,
                        rate_limiter,
                        liveness,
                        max_streams,
                    },
                )
                .await
            }
            Err(err) => tracing::warn!(error = %err, "websocket upgrade future failed"),
        }
    });

    response.into_response()
}

fn subprotocol_is_negotiated(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|item| item.trim())
        .any(|protocol| protocol.eq_ignore_ascii_case(SUBPROTOCOL))
}

fn origin_allowed(headers: &HeaderMap, allowed: &[String]) -> bool {
    match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        Some(origin) => allowed.iter().any(|a| a == origin),
        None => false,
    }
}

/// Connection-scoped service parameters: the upgrade request's headers, plus
/// `A2A-*` query parameters for clients that cannot set headers (spec Sections
/// 2.1.1 and 4.1). A header wins over a query parameter of the same name.
///
/// Only `A2A-*` keys are taken from the query, so a credential placed in the
/// URL never becomes a service parameter (Section 9.1).
fn capture_connection_params(headers: &HeaderMap, uri: &Uri) -> ServiceParams {
    let mut params: ServiceParams = HashMap::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        if is_internal_header(&key) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            params.entry(key).or_default().push(value.to_string());
        }
    }
    if let Ok(Query(query)) = Query::<Vec<(String, String)>>::try_from_uri(uri) {
        let mut from_query: ServiceParams = HashMap::new();
        for (name, value) in query {
            let key = name.to_ascii_lowercase();
            if key.starts_with("a2a-") && !params.contains_key(&key) {
                from_query.entry(key).or_default().push(value);
            }
        }
        params.extend(from_query);
    }
    params
}

fn is_internal_header(name: &str) -> bool {
    matches!(
        name,
        "host"
            | "connection"
            | "upgrade"
            | "sec-websocket-key"
            | "sec-websocket-version"
            | "sec-websocket-protocol"
            | "sec-websocket-extensions"
            | "content-length"
            | "transfer-encoding"
    )
}

#[derive(Debug)]
enum OutboundMessage {
    Frame(String),
    /// A control frame the read half owes the peer — a pong answering a ping,
    /// or the echo of a Close. `fastwebsockets` hands these to a callback
    /// instead of writing them itself once the socket is split, so they are
    /// queued like any other outbound frame.
    Control {
        opcode: OpCode,
        payload: Vec<u8>,
    },
    Close {
        code: u16,
        reason: String,
    },
}

/// Why an active stream is being stopped from outside its own task.
#[derive(Debug)]
enum StreamStop {
    /// The client sent `ws/CancelStream`, or the connection is closing: stop
    /// sending events and end with `ws/StreamEnd` (spec Section 3.4).
    Cancelled,
    /// An error has already been sent for the stream, which ends it; stop
    /// without `ws/StreamEnd` (spec Section 7.5).
    Failed,
}

/// Active streams by [`id_key`], with the request id each one answers.
type StreamRegistry = Arc<Mutex<HashMap<String, (JsonRpcId, oneshot::Sender<StreamStop>)>>>;

/// Per-connection settings resolved from [`WebSocketConfig`] at upgrade time.
struct ConnectionOptions {
    max_frame_bytes: usize,
    rate_limiter: Option<ConnectionRateLimiter>,
    liveness: Option<Liveness>,
    max_streams: Option<usize>,
}

async fn run_connection<H: RequestHandler>(
    mut ws: fastwebsockets::WebSocket<TokioIo<Upgraded>>,
    handler: Arc<H>,
    conn_auth: Arc<ConnAuth>,
    options: ConnectionOptions,
) {
    let ConnectionOptions {
        max_frame_bytes,
        mut rate_limiter,
        liveness,
        max_streams,
    } = options;

    ws.set_max_message_size(max_frame_bytes);
    ws.set_auto_close(true);
    ws.set_auto_pong(true);

    let (read_half, write_half) = ws.split(tokio::io::split);
    let mut ws = FragmentCollectorRead::new(read_half);

    let (out_tx, out_rx) = mpsc::channel::<OutboundMessage>(OUTBOUND_BUFFER_CAPACITY);
    let streams: StreamRegistry = Arc::new(Mutex::new(HashMap::new()));
    let activity = Arc::new(ActivityTracker::new());
    let mut writer = tokio::spawn(run_writer(write_half, out_rx, activity.clone()));

    // Keep-alive pings and idle timeouts (spec Section 2.2). The monitor holds a
    // sender, so it has to be stopped during teardown or the outbound channel
    // would never close and the writer would never exit.
    let monitor = liveness.map(|liveness| {
        tokio::spawn(run_liveness_monitor(
            liveness,
            activity.clone(),
            out_tx.clone(),
        ))
    });

    let mut obligated_send = {
        let out_tx = out_tx.clone();
        move |frame: Frame<'_>| {
            let out_tx = out_tx.clone();
            let message = OutboundMessage::Control {
                opcode: frame.opcode,
                payload: frame.payload.to_vec(),
            };
            async move {
                out_tx
                    .send(message)
                    .await
                    .map_err(|_| std::io::Error::other("outbound channel closed"))
            }
        }
    };

    // Tracked so the writer is not polled again after it has completed, which
    // panics with "JoinHandle polled after completion".
    let mut writer_finished = false;
    let mut malformed_messages = 0u32;

    loop {
        let incoming = tokio::select! {
            // The writer returns after it writes a Close, including one queued
            // by the liveness monitor, so this is how an idle or unresponsive
            // connection stops being read. Abandoning an in-flight `read_frame`
            // is safe here, and only here, because the branch is terminal.
            _ = &mut writer => {
                writer_finished = true;
                break;
            }

            incoming = ws.read_frame::<_, std::io::Error>(&mut obligated_send) => incoming,
        };

        match incoming {
            Ok(frame) => match frame.opcode {
                OpCode::Close => break,
                OpCode::Text => {
                    activity.record_message();
                    if !rate_limit_allows(rate_limiter.as_mut(), &out_tx).await {
                        break;
                    }
                    if !handle_text_frame(
                        &frame.payload,
                        &handler,
                        &streams,
                        &conn_auth,
                        &out_tx,
                        max_streams,
                        &mut malformed_messages,
                    )
                    .await
                    {
                        break;
                    }
                }
                OpCode::Binary => {
                    send_outbound(
                        &out_tx,
                        OutboundMessage::Close {
                            code: close_codes::UNSUPPORTED_DATA,
                            reason: "binary frames are reserved for future use".to_string(),
                        },
                    )
                    .await;
                    break;
                }
                // Ping is answered through `obligated_send`; a pong tells the
                // liveness monitor the peer is still there.
                OpCode::Pong => activity.record_pong(),
                _ => {}
            },
            Err(WebSocketError::ConnectionClosed) => break,
            Err(err) => {
                tracing::debug!(error = %err, "websocket read error; closing");
                // An oversize message must be answered with 1009, invalid UTF-8
                // with 1007, and a framing violation with 1002, rather than a
                // bare disconnect (spec Sections 3.6 and 7.4).
                if let Some(code) = close_code_for_read_error(&err) {
                    send_outbound(
                        &out_tx,
                        OutboundMessage::Close {
                            code,
                            reason: err.to_string(),
                        },
                    )
                    .await;
                }
                break;
            }
        }
    }

    // Cancel in-flight streams first so their tasks release their senders, then
    // release ours. The writer drains whatever is still queued — including a
    // final error and Close frame, in the order they were produced — and exits
    // once the last sender is gone.
    if let Some(monitor) = monitor {
        monitor.abort();
    }
    cancel_all_streams(&streams).await;
    // Every sender has to go before the writer can see the channel close, and
    // the obligated-send callback holds one of them.
    drop(obligated_send);
    drop(out_tx);
    if !writer_finished {
        let _ = writer.await;
    }
}

/// Send keep-alive pings and close connections that go quiet (spec Section 2.2).
///
/// Runs alongside the reader and writer, queueing its frames through the same
/// outbound channel so it never touches the socket directly.
async fn run_liveness_monitor(
    liveness: Liveness,
    activity: Arc<ActivityTracker>,
    out_tx: mpsc::Sender<OutboundMessage>,
) {
    loop {
        tokio::time::sleep(liveness.ping_interval).await;

        // A connection nobody is using is closed rather than pinged; there is
        // no point keeping it alive.
        if activity.idle_for() >= liveness.idle_timeout {
            tracing::debug!("connection idle past the timeout; closing");
            let _ = out_tx
                .send(OutboundMessage::Close {
                    code: close_codes::GOING_AWAY,
                    reason: "idle timeout".to_string(),
                })
                .await;
            return;
        }

        let pongs_before = activity.pong_count();
        if out_tx
            .send(OutboundMessage::Control {
                opcode: OpCode::Ping,
                payload: PING_PAYLOAD.to_vec(),
            })
            .await
            .is_err()
        {
            return;
        }

        tokio::time::sleep(liveness.pong_timeout).await;
        if activity.pong_count() == pongs_before {
            tracing::debug!("peer did not answer the keep-alive ping; closing");
            let _ = out_tx
                .send(OutboundMessage::Close {
                    code: close_codes::GOING_AWAY,
                    reason: "no pong received".to_string(),
                })
                .await;
            return;
        }
    }
}

/// Owns the write half for the lifetime of the connection.
///
/// Every outbound frame passes through here, which is what keeps writes off the
/// read path. Returns as soon as a Close frame is written, since nothing may
/// follow it, and otherwise runs until every sender has been dropped.
async fn run_writer(
    mut ws: WebSocketWrite<WriteHalf<TokioIo<Upgraded>>>,
    mut out_rx: mpsc::Receiver<OutboundMessage>,
    activity: Arc<ActivityTracker>,
) {
    while let Some(message) = out_rx.recv().await {
        let write = match message {
            OutboundMessage::Frame(text) => {
                // Counts as activity: a connection streaming events outbound is
                // busy, even if the peer has said nothing for minutes.
                activity.record_message();
                ws.write_frame(Frame::text(Payload::Owned(text.into_bytes())))
                    .await
            }
            OutboundMessage::Control { opcode, payload } => {
                ws.write_frame(Frame::new(true, opcode, None, Payload::Owned(payload)))
                    .await
            }
            OutboundMessage::Close { code, reason } => {
                let _ = ws.write_frame(Frame::close(code, reason.as_bytes())).await;
                return;
            }
        };
        if let Err(err) = write {
            tracing::debug!(error = %err, "failed to write frame; closing");
            return;
        }
    }
}

/// Admit one inbound message, or queue a `WS_RATE_LIMITED` error and the
/// `1008` close when a limit is exceeded (spec Sections 7.3 and 7.4).
///
/// The check runs before the frame is parsed, so the error carries `"id": null`
/// rather than a request id: refusing to parse is the point of the limit.
async fn rate_limit_allows(
    rate_limiter: Option<&mut ConnectionRateLimiter>,
    out_tx: &mpsc::Sender<OutboundMessage>,
) -> bool {
    let Some(limiter) = rate_limiter else {
        return true;
    };
    if limiter.allow() {
        return true;
    }
    tracing::debug!("inbound message rate limit exceeded; closing with 1008");
    send_error(
        out_tx,
        Some(JsonRpcId::Null),
        &binding_error(binding_errors::RATE_LIMITED, RATE_LIMIT_EXCEEDED, None),
    )
    .await;
    send_outbound(
        out_tx,
        OutboundMessage::Close {
            code: close_codes::POLICY_VIOLATION,
            reason: RATE_LIMIT_EXCEEDED.to_string(),
        },
    )
    .await;
    false
}

/// Stop every active stream; used when the connection closes.
async fn cancel_all_streams(streams: &StreamRegistry) {
    let mut map = streams.lock().await;
    for (_key, (_id, tx)) in map.drain() {
        let _ = tx.send(StreamStop::Cancelled);
    }
}

/// End every active stream with `err` (spec Section 9.4). The errors are
/// queued here, rather than by the stream tasks, so that they precede any
/// Close frame the caller queues next.
async fn fail_all_streams(
    streams: &StreamRegistry,
    out_tx: &mpsc::Sender<OutboundMessage>,
    err: &A2AError,
) {
    let active: Vec<_> = streams
        .lock()
        .await
        .drain()
        .map(|(_, entry)| entry)
        .collect();
    for (id, tx) in active {
        let _ = tx.send(StreamStop::Failed);
        send_error(out_tx, Some(id), err).await;
    }
}

enum AuthCheck {
    Proceed,
    Fatal,
}

/// `ErrorInfo` metadata naming the schemes the server accepts, for
/// `WS_UNAUTHENTICATED` and `WS_CREDENTIALS_EXPIRED` (spec Section 9.3).
fn schemes_metadata(authenticator: &dyn WsAuthenticator) -> Option<HashMap<String, String>> {
    let schemes = authenticator.accepted_schemes();
    (!schemes.is_empty()).then(|| HashMap::from([("schemes".to_string(), schemes.join(","))]))
}

fn credentials_expired_error(
    authenticator: &dyn WsAuthenticator,
    message: impl Into<String>,
) -> A2AError {
    binding_error(
        binding_errors::CREDENTIALS_EXPIRED,
        message,
        schemes_metadata(authenticator),
    )
}

/// Fail the active streams with `WS_CREDENTIALS_EXPIRED` and close the
/// connection with `4001` (spec Section 9.4).
async fn close_for_expired_credentials(
    streams: &StreamRegistry,
    out_tx: &mpsc::Sender<OutboundMessage>,
    err: &A2AError,
) {
    fail_all_streams(streams, out_tx, err).await;
    send_outbound(
        out_tx,
        OutboundMessage::Close {
            code: close_codes::AUTHENTICATION_REQUIRED,
            reason: "credentials expired or revoked".to_string(),
        },
    )
    .await;
}

/// Revalidate the connection's credentials before dispatching a request
/// (spec Sections 9.4 and 9.5).
async fn check_connection_auth(
    conn_auth: &Arc<ConnAuth>,
    streams: &StreamRegistry,
    out_tx: &mpsc::Sender<OutboundMessage>,
    id: &JsonRpcId,
) -> AuthCheck {
    let Some(authenticator) = conn_auth.authenticator.as_ref() else {
        return AuthCheck::Proceed;
    };
    let ctx = conn_auth.ctx.lock().unwrap().clone();
    match authenticator.revalidate(&ctx).await {
        AuthStatus::Valid => AuthCheck::Proceed,
        AuthStatus::ExpiresAt { expires_at } => {
            if !conn_auth.reauth_signaled.swap(true, Ordering::SeqCst) {
                send_outbound(
                    out_tx,
                    OutboundMessage::Frame(serialize_response(
                        WsResponseEnvelope::reauthentication_required(format_timestamp(
                            &expires_at,
                        )),
                    )),
                )
                .await;
                // The connection must stay open until `expires_at`. A successful
                // `ws/Authenticate` before then bumps the generation, which
                // cancels this close.
                let generation = conn_auth.generation.load(Ordering::SeqCst);
                let wait = (expires_at - Utc::now()).to_std().unwrap_or_default();
                let authenticator = authenticator.clone();
                let conn_auth = conn_auth.clone();
                let streams = streams.clone();
                let out_tx = out_tx.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(wait).await;
                    if conn_auth.generation.load(Ordering::SeqCst) != generation {
                        tracing::debug!("credentials refreshed in-band; cancelling 4001 close");
                        return;
                    }
                    let err = credentials_expired_error(&*authenticator, "Credentials expired");
                    close_for_expired_credentials(&streams, &out_tx, &err).await;
                });
            }
            AuthCheck::Proceed
        }
        AuthStatus::Expired { reason } => {
            let err = credentials_expired_error(&**authenticator, reason);
            send_error(out_tx, Some(id.clone()), &err).await;
            close_for_expired_credentials(streams, out_tx, &err).await;
            AuthCheck::Fatal
        }
    }
}

/// Handle a client notification. Notifications are never answered, and unknown
/// or malformed ones are ignored (spec Section 7.6).
async fn handle_notification(envelope: &WsRequestEnvelope, streams: &StreamRegistry) {
    if envelope.jsonrpc != JSONRPC_VERSION {
        return;
    }
    match envelope.method.as_deref() {
        Some(methods::CANCEL_STREAM) => {
            let Some(params) = envelope
                .params
                .clone()
                .and_then(|params| serde_json::from_value::<StreamControlParams>(params).ok())
            else {
                tracing::debug!("ignoring ws/CancelStream without a valid params.id");
                return;
            };
            // The stream task sends `ws/StreamEnd` once it has stopped. An id
            // with no active stream is ignored (spec Section 3.4).
            if let Some((_, tx)) = streams.lock().await.remove(&id_key(&params.id)) {
                let _ = tx.send(StreamStop::Cancelled);
            }
        }
        other => tracing::debug!(method = ?other, "ignoring notification"),
    }
}

/// Handle the optional `ws/Authenticate` method (spec Section 9.3). Returns
/// `false` once the connection is being closed.
async fn handle_authenticate(
    conn_auth: &Arc<ConnAuth>,
    out_tx: &mpsc::Sender<OutboundMessage>,
    id: JsonRpcId,
    params: Value,
) -> bool {
    let Some(authenticator) = conn_auth
        .authenticator
        .as_ref()
        .filter(|a| a.supports_in_band_refresh())
    else {
        send_error(
            out_tx,
            Some(id),
            &A2AError::method_not_found(methods::AUTHENTICATE),
        )
        .await;
        return true;
    };

    let parsed = match serde_json::from_value::<AuthenticateParams>(params) {
        Ok(p) if !p.credentials.is_empty() => p,
        Ok(_) => {
            let err = A2AError::invalid_params("params.credentials must not be empty");
            return authenticate_failed(conn_auth, out_tx, id, err).await;
        }
        Err(err) => {
            let err = A2AError::invalid_params(format!("invalid ws/Authenticate params: {err}"));
            return authenticate_failed(conn_auth, out_tx, id, err).await;
        }
    };

    let current = conn_auth.ctx.lock().unwrap().clone();
    let new_ctx = match authenticator.refresh(&current, &parsed.credentials).await {
        Ok(new_ctx) => new_ctx,
        Err(auth_err) => {
            let err = match auth_err.status {
                400 => A2AError::invalid_params(auth_err.message),
                403 => binding_error(binding_errors::PERMISSION_DENIED, auth_err.message, None),
                _ => binding_error(
                    binding_errors::UNAUTHENTICATED,
                    auth_err.message,
                    schemes_metadata(&**authenticator),
                ),
            };
            return authenticate_failed(conn_auth, out_tx, id, err).await;
        }
    };

    // A refresh must keep the principal; acting as another identity needs a
    // new connection (spec Section 9.3).
    let principal = |ctx: &AuthContext| ctx.user.as_ref().map(|user| user.name.clone());
    if current.user.is_some() && principal(&current) != principal(&new_ctx) {
        let err = binding_error(
            binding_errors::PERMISSION_DENIED,
            "ws/Authenticate credentials identify a different principal",
            None,
        );
        return authenticate_failed(conn_auth, out_tx, id, err).await;
    }

    // The parameters handed to the handler are derived from the context, so
    // they have to be rebuilt here as well.
    let refreshed = conn_auth.params().reauthenticated(&new_ctx);
    let expires_at = new_ctx.expires_at;
    *conn_auth.params.lock().unwrap() = Arc::new(refreshed);
    *conn_auth.ctx.lock().unwrap() = new_ctx;
    conn_auth.generation.fetch_add(1, Ordering::SeqCst);
    conn_auth.reauth_signaled.store(false, Ordering::SeqCst);

    let mut result = serde_json::Map::new();
    if let Some(expires_at) = expires_at {
        result.insert(
            "expiresAt".to_string(),
            Value::String(format_timestamp(&expires_at)),
        );
    }
    send_outbound(
        out_tx,
        OutboundMessage::Frame(serialize_response(WsResponseEnvelope::result(
            id,
            Value::Object(result),
        ))),
    )
    .await;
    true
}

/// Answer a failed `ws/Authenticate` and close the connection with `1008` once
/// the attempt limit is reached (spec Section 9.3). The current credentials
/// are unaffected.
async fn authenticate_failed(
    conn_auth: &Arc<ConnAuth>,
    out_tx: &mpsc::Sender<OutboundMessage>,
    id: JsonRpcId,
    err: A2AError,
) -> bool {
    send_error(out_tx, Some(id), &err).await;
    let failures = conn_auth
        .failed_authenticate_attempts
        .fetch_add(1, Ordering::SeqCst)
        + 1;
    if failures < MAX_FAILED_AUTHENTICATE_ATTEMPTS {
        return true;
    }
    tracing::debug!(
        failures,
        "too many failed ws/Authenticate attempts; closing with 1008"
    );
    send_outbound(
        out_tx,
        OutboundMessage::Close {
            code: close_codes::POLICY_VIOLATION,
            reason: "too many failed authentication attempts".to_string(),
        },
    )
    .await;
    false
}

/// Handle one inbound text message. Returns `false` once the connection is
/// being closed; the Close frame has already been queued.
#[allow(clippy::too_many_arguments)]
async fn handle_text_frame<H: RequestHandler>(
    payload: &[u8],
    handler: &Arc<H>,
    streams: &StreamRegistry,
    conn_auth: &Arc<ConnAuth>,
    out_tx: &mpsc::Sender<OutboundMessage>,
    max_streams: Option<usize>,
    malformed_messages: &mut u32,
) -> bool {
    // A text message that is not UTF-8 cannot be decoded, so no JSON-RPC error
    // can be sent for it (spec Section 7.4).
    if std::str::from_utf8(payload).is_err() {
        send_outbound(
            out_tx,
            OutboundMessage::Close {
                code: close_codes::INVALID_PAYLOAD,
                reason: "text message is not valid UTF-8".to_string(),
            },
        )
        .await;
        return false;
    }

    // Each message is framed separately, so invalid JSON fails only itself and
    // the connection stays open (spec Section 3.3).
    let value: Value = match serde_json::from_slice(payload) {
        Ok(value) => value,
        Err(err) => {
            send_error(
                out_tx,
                Some(JsonRpcId::Null),
                &A2AError::parse_error(format!("invalid JSON: {err}")),
            )
            .await;
            *malformed_messages += 1;
            if *malformed_messages < MAX_MALFORMED_MESSAGES {
                return true;
            }
            send_outbound(
                out_tx,
                OutboundMessage::Close {
                    code: close_codes::A2A_PROTOCOL_ERROR,
                    reason: "too many malformed messages".to_string(),
                },
            )
            .await;
            return false;
        }
    };

    let envelope: WsRequestEnvelope = match serde_json::from_value(value.clone()) {
        Ok(envelope) => envelope,
        Err(err) => {
            // A malformed notification is ignored rather than answered
            // (spec Section 7.6).
            let has_id = value.get("id").is_some();
            if !has_id && value.get("method").is_some() {
                tracing::debug!(error = %err, "ignoring malformed notification");
                return true;
            }
            let id = value
                .get("id")
                .and_then(|id| serde_json::from_value::<JsonRpcId>(id.clone()).ok())
                .unwrap_or(JsonRpcId::Null);
            send_error(
                out_tx,
                Some(id),
                &A2AError::invalid_request(format!("invalid JSON-RPC request: {err}")),
            )
            .await;
            return true;
        }
    };

    if envelope.is_notification() {
        handle_notification(&envelope, streams).await;
        return true;
    }

    let id = envelope.id.clone().unwrap_or(JsonRpcId::Null);
    if envelope.jsonrpc != JSONRPC_VERSION {
        send_error(
            out_tx,
            Some(id),
            &A2AError::invalid_request(format!(
                "unsupported jsonrpc version: '{}'",
                envelope.jsonrpc
            )),
        )
        .await;
        return true;
    }

    if matches!(id, JsonRpcId::Null) {
        send_error(
            out_tx,
            Some(JsonRpcId::Null),
            &A2AError::invalid_request("request id must not be null"),
        )
        .await;
        return true;
    }

    let Some(method) = envelope.method.clone() else {
        send_error(
            out_tx,
            Some(id),
            &A2AError::invalid_request("method is required"),
        )
        .await;
        return true;
    };

    // `ws/Authenticate` replaces the credentials, so it is exempt from
    // revalidation. It runs before the next message is read, so every later
    // request sees the outcome (spec Section 9.3).
    if method == methods::AUTHENTICATE {
        let params = envelope.params.clone().unwrap_or(Value::Null);
        return handle_authenticate(conn_auth, out_tx, id, params).await;
    }

    if methods::is_control(&method) || !methods::is_known(&method) {
        send_error(out_tx, Some(id), &A2AError::method_not_found(&method)).await;
        return true;
    }

    // Per-request credential revalidation (spec Section 9.4).
    match check_connection_auth(conn_auth, streams, out_tx, &id).await {
        AuthCheck::Proceed => {}
        AuthCheck::Fatal => return false,
    }

    let combined_params = match combine_service_params(&conn_auth.params(), &envelope) {
        Ok(params) => params,
        Err(err) => {
            send_error(out_tx, Some(id), &err).await;
            return true;
        }
    };
    let raw_params = envelope.params.clone().unwrap_or(Value::Null);

    let handler = handler.clone();
    let out_tx_task = out_tx.clone();
    let streams = streams.clone();

    tokio::spawn(async move {
        if methods::is_streaming(&method) {
            run_streaming_request(
                method,
                id,
                raw_params,
                combined_params,
                StreamContext {
                    handler,
                    streams,
                    out_tx: out_tx_task,
                    max_streams,
                },
            )
            .await;
        } else {
            run_unary_request(
                method,
                id,
                raw_params,
                combined_params,
                handler,
                out_tx_task,
            )
            .await;
        }
    });

    true
}

/// Service parameters established when the connection was accepted: the
/// handshake headers, overlaid with whatever the authenticator published.
///
/// The authenticator's keys are tracked separately because they are
/// authoritative for the lifetime of the connection. This is the seam through
/// which an authenticated identity reaches the [`RequestHandler`], so a client
/// must not be able to rewrite it — see [`combine_service_params`].
#[derive(Debug, Default)]
struct ConnectionParams {
    /// The handshake headers alone. Fixed for the lifetime of the connection,
    /// and retained so the set can be rebuilt against a replaced context.
    base: ServiceParams,
    /// `base` overlaid with the current context's parameters.
    params: ServiceParams,
    authenticated: HashSet<String>,
}

impl ConnectionParams {
    fn from_base(base: ServiceParams, auth_ctx: &AuthContext) -> Self {
        let mut params = base.clone();
        let mut authenticated = HashSet::new();
        // The authenticator runs after the headers are captured and wins on
        // conflict, so a client cannot pre-seed one of its keys at handshake
        // time either. Keys are lowercased to match the captured headers.
        for (key, values) in &auth_ctx.service_params {
            let key = key.to_ascii_lowercase();
            params.insert(key.clone(), values.clone());
            authenticated.insert(key);
        }
        Self {
            base,
            params,
            authenticated,
        }
    }

    /// Rebuild against a context that replaced the original one, so that a
    /// connection which refreshed its credentials in-band stops presenting the
    /// tenant or scopes it authenticated with earlier (Section 9.3).
    fn reauthenticated(&self, auth_ctx: &AuthContext) -> Self {
        Self::from_base(self.base.clone(), auth_ctx)
    }
}

/// Merge the connection-scoped parameters with a request's own `serviceParams`
/// (spec Section 4.2).
///
/// Per-request entries override connection-scoped ones, except:
///
/// * `a2a-version`, which is connection-scoped: a per-request value must equal
///   the connection's, otherwise the request fails with
///   `VersionNotSupportedError` (Section 4.1).
/// * keys the authenticator established. Letting a request overwrite those
///   would allow a client that authenticated as one identity or tenant to
///   present itself to the handler as another.
///
/// Keys that differ only in case fail the request with `InvalidRequestError`.
fn combine_service_params(
    connection: &ConnectionParams,
    envelope: &WsRequestEnvelope,
) -> Result<ServiceParams, A2AError> {
    let mut combined = connection.params.clone();
    let Some(per_request) = envelope.service_params.as_ref() else {
        return Ok(combined);
    };
    for (key, values) in
        service_params_from_envelope(per_request).map_err(A2AError::invalid_request)?
    {
        if key == VERSION_KEY {
            let connection_version = combined
                .get(VERSION_KEY)
                .and_then(|v| v.first())
                .map(String::as_str)
                .unwrap_or("0.3");
            let requested = values.first().map(String::as_str).unwrap_or_default();
            if requested.trim() != connection_version.trim() {
                return Err(A2AError::version_not_supported(requested));
            }
            continue;
        }
        if connection.authenticated.contains(&key) {
            tracing::debug!(
                %key,
                "ignoring per-request override of an authenticator-established service param"
            );
            continue;
        }
        combined.insert(key, values);
    }
    Ok(combined)
}

async fn run_unary_request<H: RequestHandler>(
    method: String,
    id: JsonRpcId,
    raw_params: Value,
    params: ServiceParams,
    handler: Arc<H>,
    out_tx: mpsc::Sender<OutboundMessage>,
) {
    let result = dispatch_unary(&method, &handler, &params, raw_params).await;
    match result {
        Ok(value) => {
            send_outbound(
                &out_tx,
                OutboundMessage::Frame(serialize_response(WsResponseEnvelope::result(id, value))),
            )
            .await;
        }
        Err(err) => send_error(&out_tx, Some(id), &err).await,
    }
}

async fn dispatch_unary<H: RequestHandler>(
    method: &str,
    handler: &Arc<H>,
    params: &ServiceParams,
    raw_params: Value,
) -> Result<Value, A2AError> {
    match method {
        methods::SEND_MESSAGE => {
            let req: SendMessageRequest = parse_params(raw_params)?;
            let resp = handler.send_message(params, req).await?;
            to_value(&resp)
        }
        methods::GET_TASK => {
            let req: GetTaskRequest = parse_params(raw_params)?;
            let resp = handler.get_task(params, req).await?;
            to_value(&resp)
        }
        methods::LIST_TASKS => {
            let req: ListTasksRequest = parse_params(raw_params)?;
            let resp = handler.list_tasks(params, req).await?;
            to_value(&resp)
        }
        methods::CANCEL_TASK => {
            let req: CancelTaskRequest = parse_params(raw_params)?;
            let resp = handler.cancel_task(params, req).await?;
            to_value(&resp)
        }
        methods::CREATE_PUSH_CONFIG => {
            let req: TaskPushNotificationConfig = parse_params(raw_params)?;
            let resp = handler.create_push_config(params, req).await?;
            to_value(&resp)
        }
        methods::GET_PUSH_CONFIG => {
            let req: GetTaskPushNotificationConfigRequest = parse_params(raw_params)?;
            let resp = handler.get_push_config(params, req).await?;
            to_value(&resp)
        }
        methods::LIST_PUSH_CONFIGS => {
            let req: ListTaskPushNotificationConfigsRequest = parse_params(raw_params)?;
            let resp = handler.list_push_configs(params, req).await?;
            to_value(&resp)
        }
        methods::DELETE_PUSH_CONFIG => {
            let req: DeleteTaskPushNotificationConfigRequest = parse_params(raw_params)?;
            handler.delete_push_config(params, req).await?;
            Ok(Value::Object(serde_json::Map::new()))
        }
        methods::GET_EXTENDED_AGENT_CARD => {
            let req: GetExtendedAgentCardRequest = parse_params(raw_params)?;
            let resp = handler.get_extended_agent_card(params, req).await?;
            to_value(&resp)
        }
        other => Err(A2AError::method_not_found(other)),
    }
}

/// The connection-scoped handles a streaming request works against.
struct StreamContext<H: RequestHandler> {
    handler: Arc<H>,
    streams: StreamRegistry,
    out_tx: mpsc::Sender<OutboundMessage>,
    /// Cap on concurrent streams for this connection (spec Section 8.6).
    max_streams: Option<usize>,
}

async fn run_streaming_request<H: RequestHandler>(
    method: String,
    id: JsonRpcId,
    raw_params: Value,
    params: ServiceParams,
    ctx: StreamContext<H>,
) {
    let StreamContext {
        handler,
        streams,
        out_tx,
        max_streams,
    } = ctx;

    let key = id_key(&id);
    let (cancel_tx, mut cancel_rx) = oneshot::channel::<StreamStop>();

    // Claim the stream slot before invoking the handler, so the per-connection
    // cap (spec Section 8.6) is decided under one lock rather than after the
    // work has already started. Registering this early also means a
    // `ws/CancelStream` that arrives while the handler is still setting up is
    // honoured instead of missed.
    {
        let mut map = streams.lock().await;
        if let Some(max) = max_streams {
            if map.len() >= max {
                drop(map);
                tracing::debug!(max, "per-connection stream limit reached; refusing stream");
                send_error(
                    &out_tx,
                    Some(id),
                    &binding_error(binding_errors::TOO_MANY_IN_FLIGHT, TOO_MANY_STREAMS, None),
                )
                .await;
                return;
            }
        }
        map.insert(key.clone(), (id.clone(), cancel_tx));
    }

    let stream_result: Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> =
        match method.as_str() {
            methods::SEND_STREAMING_MESSAGE => match parse_params(raw_params) {
                Ok(req) => handler.send_streaming_message(&params, req).await,
                Err(err) => Err(err),
            },
            methods::SUBSCRIBE_TO_TASK => match parse_params(raw_params) {
                Ok(req) => handler.subscribe_to_task(&params, req).await,
                Err(err) => Err(err),
            },
            other => Err(A2AError::method_not_found(other)),
        };

    let mut stream = match stream_result {
        Ok(stream) => stream,
        Err(err) => {
            // Release the slot the failed stream reserved.
            streams.lock().await.remove(&key);
            send_error(&out_tx, Some(id), &err).await;
            return;
        }
    };

    let mut errored = false;
    loop {
        tokio::select! {
            biased;

            stop = &mut cancel_rx => {
                // Cancellation: stop sending events; `ws/StreamEnd` is sent
                // below once the stream has been removed from the registry.
                // After a failure the error already ended the stream.
                if matches!(stop, Ok(StreamStop::Failed)) {
                    errored = true;
                }
                break;
            }

            next = stream.next() => {
                let Some(item) = next else { break };
                match item {
                    Ok(event) => match protojson_conv::to_value(&event) {
                        Ok(value) => {
                            send_outbound(
                                &out_tx,
                                OutboundMessage::Frame(serialize_response(
                                    WsResponseEnvelope::stream_chunk(id.clone(), value),
                                )),
                            )
                            .await;
                        }
                        Err(err) => {
                            send_error(
                                &out_tx,
                                Some(id.clone()),
                                &A2AError::internal(format!("failed to serialize event: {err}")),
                            )
                            .await;
                            errored = true;
                            break;
                        }
                    },
                    Err(err) => {
                        send_error(&out_tx, Some(id.clone()), &err).await;
                        errored = true;
                        break;
                    }
                }
            }
        }
    }

    {
        let mut map = streams.lock().await;
        map.remove(&key);
    }

    if !errored {
        send_outbound(
            &out_tx,
            OutboundMessage::Frame(serialize_response(WsResponseEnvelope::stream_end(id))),
        )
        .await;
    }
}

fn parse_params<T: ProtoJsonPayload>(value: Value) -> Result<T, A2AError> {
    protojson_conv::from_value(value).map_err(|e| A2AError::invalid_params(format!("{e}")))
}

fn to_value<T: ProtoJsonPayload>(value: &T) -> Result<Value, A2AError> {
    protojson_conv::to_value(value)
        .map_err(|e| A2AError::internal(format!("failed to serialize ProtoJSON payload: {e}")))
}

fn serialize_response(resp: WsResponseEnvelope) -> String {
    serde_json::to_string(&resp).unwrap_or_else(|err| {
        tracing::warn!(error = %err, "failed to serialize WebSocket response envelope");
        let fallback = WsResponseEnvelope::error(
            resp.id.clone(),
            a2a_error_to_jsonrpc(&A2AError::internal(format!(
                "failed to serialize response: {err}"
            ))),
        );
        serde_json::to_string(&fallback).unwrap_or_else(|_| {
            "{\"jsonrpc\":\"2.0\",\"id\":null,\"error\":{\"code\":-32603,\"message\":\"serialization error\"}}"
                .to_string()
        })
    })
}

async fn send_outbound(out_tx: &mpsc::Sender<OutboundMessage>, message: OutboundMessage) {
    if out_tx.send(message).await.is_err() {
        tracing::debug!("outbound channel closed; dropping message");
    }
}

async fn send_error(out_tx: &mpsc::Sender<OutboundMessage>, id: Option<JsonRpcId>, err: &A2AError) {
    let envelope = WsResponseEnvelope::error(id, a2a_error_to_jsonrpc(err));
    send_outbound(out_tx, OutboundMessage::Frame(serialize_response(envelope))).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{AuthContext, AuthError, User};
    use crate::errors::binding_error_reason;
    use a2a_server::handler::DefaultRequestHandler;
    use a2a_server::task_store::InMemoryTaskStore;
    use async_trait::async_trait;
    use axum::http::HeaderValue;
    use std::time::Duration;

    struct NoopExecutor;

    impl a2a_server::AgentExecutor for NoopExecutor {
        fn execute(
            &self,
            _ctx: a2a_server::executor::ExecutorContext,
        ) -> futures::stream::BoxStream<'static, Result<a2a::event::StreamResponse, A2AError>>
        {
            Box::pin(futures::stream::empty())
        }

        fn cancel(
            &self,
            _ctx: a2a_server::executor::ExecutorContext,
        ) -> futures::stream::BoxStream<'static, Result<a2a::event::StreamResponse, A2AError>>
        {
            Box::pin(futures::stream::empty())
        }
    }

    fn make_handler() -> Arc<DefaultRequestHandler> {
        Arc::new(DefaultRequestHandler::new(
            NoopExecutor,
            InMemoryTaskStore::new(),
        ))
    }

    fn conn_auth(
        authenticator: Option<Arc<dyn WsAuthenticator>>,
        ctx: AuthContext,
    ) -> Arc<ConnAuth> {
        Arc::new(ConnAuth {
            authenticator,
            ctx: StdMutex::new(ctx),
            params: StdMutex::new(Arc::new(ConnectionParams::default())),
            reauth_signaled: AtomicBool::new(false),
            generation: AtomicU64::new(0),
            failed_authenticate_attempts: AtomicU32::new(0),
        })
    }

    fn no_auth() -> Arc<ConnAuth> {
        conn_auth(None, AuthContext::default())
    }

    fn conn_auth_with(authenticator: Arc<dyn WsAuthenticator>) -> Arc<ConnAuth> {
        conn_auth(
            Some(authenticator),
            AuthContext::for_user(User::authenticated("alice")),
        )
    }

    #[derive(Default)]
    struct StubHandler {
        send_message_error: Option<A2AError>,
        streaming_pending: bool,
    }

    impl StubHandler {
        fn failing_send_message() -> Self {
            Self {
                send_message_error: Some(A2AError::new(error_code::PARSE_ERROR, "bad")),
                streaming_pending: false,
            }
        }

        fn pending_stream() -> Self {
            Self {
                send_message_error: None,
                streaming_pending: true,
            }
        }
    }

    fn sample_task(id: &str) -> Task {
        Task {
            id: id.into(),
            context_id: "ctx-1".into(),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    fn sample_message() -> Message {
        Message {
            message_id: "msg-1".into(),
            context_id: None,
            task_id: None,
            role: Role::User,
            parts: vec![Part::text("hello")],
            metadata: None,
            extensions: None,
            reference_task_ids: None,
        }
    }

    fn send_message_request() -> SendMessageRequest {
        SendMessageRequest {
            message: sample_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        }
    }

    fn describe(message: &OutboundMessage) -> String {
        match message {
            OutboundMessage::Frame(text) => format!("frame {text}"),
            OutboundMessage::Control { opcode, .. } => format!("control {opcode:?}"),
            OutboundMessage::Close { code, .. } => format!("close {code}"),
        }
    }

    fn frame_text(message: OutboundMessage) -> String {
        match message {
            OutboundMessage::Frame(text) => text,
            other => panic!("expected a frame, got {}", describe(&other)),
        }
    }

    fn close_code_of(message: OutboundMessage) -> u16 {
        match message {
            OutboundMessage::Close { code, .. } => code,
            other => panic!("expected a close, got {}", describe(&other)),
        }
    }

    fn frame_payload(message: OutboundMessage) -> WsResponseEnvelope {
        serde_json::from_str(&frame_text(message)).unwrap()
    }

    fn error_reason_of(resp: &WsResponseEnvelope) -> String {
        let err = resp.error.as_ref().expect("error present");
        let data = err.data.as_ref().expect("data present");
        let arr = data.as_array().expect("array");
        arr.last().unwrap()["reason"].as_str().unwrap().to_string()
    }

    fn binding_reason_of(resp: &WsResponseEnvelope) -> Option<String> {
        let err = crate::errors::jsonrpc_error_to_a2a(resp.error.as_ref().expect("error present"));
        binding_error_reason(&err).map(str::to_string)
    }

    #[async_trait]
    impl RequestHandler for StubHandler {
        async fn send_message(
            &self,
            _params: &ServiceParams,
            _req: SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            if let Some(error) = &self.send_message_error {
                return Err(error.clone());
            }
            Ok(SendMessageResponse::Task(sample_task("send")))
        }

        async fn send_streaming_message(
            &self,
            _params: &ServiceParams,
            _req: SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            if self.streaming_pending {
                return Ok(Box::pin(futures::stream::pending()));
            }
            Ok(Box::pin(futures::stream::iter(vec![Ok(
                StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: "stream".into(),
                    context_id: "ctx-1".into(),
                    status: TaskStatus {
                        state: TaskState::Working,
                        message: None,
                        timestamp: None,
                    },
                    metadata: None,
                }),
            )])))
        }

        async fn get_task(
            &self,
            _params: &ServiceParams,
            req: GetTaskRequest,
        ) -> Result<Task, A2AError> {
            Ok(sample_task(&req.id))
        }

        async fn list_tasks(
            &self,
            _params: &ServiceParams,
            _req: ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            Ok(ListTasksResponse {
                tasks: vec![sample_task("listed")],
                next_page_token: "".into(),
                page_size: 1,
                total_size: 1,
            })
        }

        async fn cancel_task(
            &self,
            _params: &ServiceParams,
            req: CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            Ok(sample_task(&req.id))
        }

        async fn subscribe_to_task(
            &self,
            _params: &ServiceParams,
            _req: SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            Ok(Box::pin(futures::stream::iter(vec![Err(
                A2AError::internal("stream failed"),
            )])))
        }

        async fn create_push_config(
            &self,
            _params: &ServiceParams,
            req: TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            Ok(req)
        }

        async fn get_push_config(
            &self,
            _params: &ServiceParams,
            req: GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            Ok(TaskPushNotificationConfig {
                url: "https://hook.example.test".into(),
                id: Some(req.id),
                task_id: req.task_id,
                token: None,
                authentication: None,
                tenant: req.tenant,
            })
        }

        async fn list_push_configs(
            &self,
            _params: &ServiceParams,
            _req: ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            Ok(ListTaskPushNotificationConfigsResponse {
                configs: vec![],
                next_page_token: None,
            })
        }

        async fn delete_push_config(
            &self,
            _params: &ServiceParams,
            _req: DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            Ok(())
        }

        async fn get_extended_agent_card(
            &self,
            _params: &ServiceParams,
            _req: GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            Err(A2AError::unsupported_operation("no extended card"))
        }
    }

    /// Authenticator used to exercise the revalidation and `ws/Authenticate`
    /// paths.
    struct RefreshAuth {
        status: AuthStatus,
        supports_refresh: bool,
        /// The result of a refresh: `Ok(principal)` or the error to return.
        refresh: Result<&'static str, AuthError>,
        expires_at: Option<chrono::DateTime<Utc>>,
    }

    impl RefreshAuth {
        fn new(status: AuthStatus) -> Self {
            RefreshAuth {
                status,
                supports_refresh: true,
                refresh: Ok("alice"),
                expires_at: None,
            }
        }
    }

    #[async_trait]
    impl WsAuthenticator for RefreshAuth {
        async fn authenticate(&self, _headers: &HeaderMap) -> Result<AuthContext, AuthError> {
            Ok(AuthContext::default())
        }
        async fn revalidate(&self, _ctx: &AuthContext) -> AuthStatus {
            self.status.clone()
        }
        fn accepted_schemes(&self) -> Vec<String> {
            vec!["bearer".to_string()]
        }
        fn supports_in_band_refresh(&self) -> bool {
            self.supports_refresh
        }
        async fn refresh(
            &self,
            _ctx: &AuthContext,
            _credentials: &HashMap<String, String>,
        ) -> Result<AuthContext, AuthError> {
            let principal = self.refresh.clone()?;
            let mut ctx = AuthContext::for_user(User::authenticated(principal));
            ctx.expires_at = self.expires_at;
            Ok(ctx)
        }
    }

    struct Harness {
        handler: Arc<StubHandler>,
        streams: StreamRegistry,
        conn_auth: Arc<ConnAuth>,
        out_tx: mpsc::Sender<OutboundMessage>,
        out_rx: mpsc::Receiver<OutboundMessage>,
        malformed: u32,
    }

    impl Harness {
        fn new() -> Self {
            Self::with_auth(no_auth())
        }

        fn with_auth(conn_auth: Arc<ConnAuth>) -> Self {
            let (out_tx, out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);
            Harness {
                handler: Arc::new(StubHandler::default()),
                streams: Arc::new(Mutex::new(HashMap::new())),
                conn_auth,
                out_tx,
                out_rx,
                malformed: 0,
            }
        }

        async fn send(&mut self, payload: &[u8]) -> bool {
            handle_text_frame(
                payload,
                &self.handler,
                &self.streams,
                &self.conn_auth,
                &self.out_tx,
                None,
                &mut self.malformed,
            )
            .await
        }

        async fn send_json(&mut self, value: Value) -> bool {
            self.send(&serde_json::to_vec(&value).unwrap()).await
        }

        fn next(&mut self) -> OutboundMessage {
            self.out_rx.try_recv().expect("an outbound message")
        }

        fn assert_silent(&mut self) {
            if let Ok(message) = self.out_rx.try_recv() {
                panic!("expected no output, got {}", describe(&message));
            }
        }
    }

    #[test]
    fn websocket_router_constructs_with_request_handler() {
        let _router = websocket_router(make_handler());
    }

    #[test]
    fn subprotocol_is_negotiated_accepts_exact_match() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("a2a.v1"),
        );
        assert!(subprotocol_is_negotiated(&headers));
    }

    #[test]
    fn subprotocol_is_negotiated_rejects_unknown_name() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("a2a.jsonrpc.v1"),
        );
        assert!(!subprotocol_is_negotiated(&headers));
    }

    #[test]
    fn origin_allowed_matches_allowlist() {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, HeaderValue::from_static("https://ok.test"));
        assert!(origin_allowed(&headers, &["https://ok.test".to_string()]));
        assert!(!origin_allowed(
            &headers,
            &["https://other.test".to_string()]
        ));
        // Missing Origin is rejected when an allowlist is configured.
        assert!(!origin_allowed(
            &HeaderMap::new(),
            &["https://ok.test".to_string()]
        ));
    }

    #[test]
    fn capture_connection_params_lowercases_keys_and_filters_internal_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("A2A-Version", HeaderValue::from_static("1.0"));
        headers.insert("Authorization", HeaderValue::from_static("Bearer t"));
        headers.insert(header::HOST, HeaderValue::from_static("agent.example.com"));

        let params = capture_connection_params(&headers, &Uri::from_static("/"));
        assert_eq!(params.get("a2a-version").unwrap(), &vec!["1.0".to_string()]);
        assert_eq!(
            params.get("authorization").unwrap(),
            &vec!["Bearer t".to_string()]
        );
        assert!(!params.contains_key("host"));
    }

    #[test]
    fn capture_connection_params_reads_a2a_query_parameters() {
        let uri = Uri::from_static(
            "/a2a/ws?A2A-Version=1.0&A2A-Extensions=https%3A%2F%2Fexample.com%2Fext&api_key=secret",
        );
        let params = capture_connection_params(&HeaderMap::new(), &uri);
        assert_eq!(params.get("a2a-version").unwrap(), &vec!["1.0".to_string()]);
        assert_eq!(
            params.get("a2a-extensions").unwrap(),
            &vec!["https://example.com/ext".to_string()]
        );
        // A credential in the URL never becomes a service parameter.
        assert!(!params.contains_key("api_key"));
    }

    #[test]
    fn capture_connection_params_prefers_the_header_over_the_query() {
        let mut headers = HeaderMap::new();
        headers.insert("A2A-Version", HeaderValue::from_static("1.0"));
        let uri = Uri::from_static("/?A2A-Version=0.3");
        let params = capture_connection_params(&headers, &uri);
        assert_eq!(params.get("a2a-version").unwrap(), &vec!["1.0".to_string()]);
    }

    fn envelope_with_params(pairs: &[(&str, &str)]) -> WsRequestEnvelope {
        WsRequestEnvelope {
            id: Some("req".into()),
            method: Some(methods::SEND_MESSAGE.into()),
            service_params: Some(
                pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
            ..Default::default()
        }
    }

    fn connection_with_version(version: &str) -> ConnectionParams {
        let mut connection = ConnectionParams::default();
        connection
            .params
            .insert(VERSION_KEY.into(), vec![version.into()]);
        connection
    }

    #[test]
    fn combine_service_params_per_request_overrides_connection_scope() {
        let mut connection = connection_with_version("1.0");
        connection
            .params
            .insert("x-keep".into(), vec!["preserve".into()]);
        connection
            .params
            .insert("x-trace".into(), vec!["old".into()]);

        let envelope = envelope_with_params(&[("x-trace", "new"), ("x-extra", "added")]);

        let combined = combine_service_params(&connection, &envelope).unwrap();
        assert_eq!(combined.get("x-trace").unwrap(), &vec!["new".to_string()]);
        assert_eq!(
            combined.get("x-keep").unwrap(),
            &vec!["preserve".to_string()]
        );
        assert_eq!(combined.get("x-extra").unwrap(), &vec!["added".to_string()]);
    }

    #[test]
    fn combine_service_params_accepts_a_matching_per_request_version() {
        let connection = connection_with_version("1.0");
        let envelope = envelope_with_params(&[("A2A-Version", "1.0")]);
        let combined = combine_service_params(&connection, &envelope).unwrap();
        assert_eq!(combined.get(VERSION_KEY).unwrap(), &vec!["1.0".to_string()]);
    }

    #[test]
    fn combine_service_params_rejects_a_different_per_request_version() {
        let connection = connection_with_version("1.0");
        let envelope = envelope_with_params(&[("a2a-version", "1.1")]);
        let err = combine_service_params(&connection, &envelope).unwrap_err();
        assert_eq!(err.code, error_code::VERSION_NOT_SUPPORTED);
    }

    #[test]
    fn combine_service_params_rejects_keys_differing_only_in_case() {
        let connection = connection_with_version("1.0");
        let envelope = envelope_with_params(&[("X-Trace", "a"), ("x-trace", "b")]);
        let err = combine_service_params(&connection, &envelope).unwrap_err();
        assert_eq!(err.code, error_code::INVALID_REQUEST);
    }

    #[test]
    fn combine_service_params_refuses_to_override_authenticated_params() {
        let auth_ctx = AuthContext {
            service_params: HashMap::from([("x-tenant".to_string(), vec!["acme".to_string()])]),
            ..Default::default()
        };
        let connection = ConnectionParams::from_base(ServiceParams::new(), &auth_ctx);

        let envelope = envelope_with_params(&[("x-tenant", "evil-corp")]);

        let combined = combine_service_params(&connection, &envelope).unwrap();
        assert_eq!(combined.get("x-tenant").unwrap(), &vec!["acme".to_string()]);
    }

    #[test]
    fn combine_service_params_matches_authenticated_keys_case_insensitively() {
        let auth_ctx = AuthContext {
            service_params: HashMap::from([("X-Tenant".to_string(), vec!["acme".to_string()])]),
            ..Default::default()
        };
        let connection = ConnectionParams::from_base(ServiceParams::new(), &auth_ctx);
        assert_eq!(
            connection.params.get("x-tenant").unwrap(),
            &vec!["acme".to_string()]
        );

        let envelope = envelope_with_params(&[("X-TENANT", "evil-corp")]);

        let combined = combine_service_params(&connection, &envelope).unwrap();
        assert_eq!(combined.get("x-tenant").unwrap(), &vec!["acme".to_string()]);
        assert!(!combined.contains_key("X-TENANT"));
    }

    #[test]
    fn connection_params_let_the_authenticator_override_a_handshake_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-tenant", HeaderValue::from_static("claimed-by-client"));
        let auth_ctx = AuthContext {
            service_params: HashMap::from([("x-tenant".to_string(), vec!["acme".to_string()])]),
            ..Default::default()
        };

        let connection = ConnectionParams::from_base(
            capture_connection_params(&headers, &Uri::from_static("/")),
            &auth_ctx,
        );
        assert_eq!(
            connection.params.get("x-tenant").unwrap(),
            &vec!["acme".to_string()]
        );
    }

    #[test]
    fn serialize_response_emits_a_stream_end_notification() {
        let json = serialize_response(WsResponseEnvelope::stream_end("req-1".into()));
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "jsonrpc": "2.0",
                "method": "ws/StreamEnd",
                "params": { "id": "req-1" }
            })
        );
    }

    #[tokio::test]
    async fn invalid_json_gets_a_parse_error_and_the_connection_stays_open() {
        let mut h = Harness::new();
        assert!(h.send(b"{not json").await);

        // The wire frame MUST carry an explicit `"id": null` (spec Section 3.3).
        let raw = frame_text(h.next());
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert!(value["id"].is_null());
        assert!(value.as_object().unwrap().contains_key("id"));
        assert_eq!(value["error"]["code"], error_code::PARSE_ERROR);
        h.assert_silent();
    }

    #[tokio::test]
    async fn repeated_invalid_json_closes_with_4000() {
        let mut h = Harness::new();
        for _ in 1..MAX_MALFORMED_MESSAGES {
            assert!(h.send(b"nope").await);
            frame_text(h.next());
        }
        assert!(!h.send(b"nope").await);
        frame_text(h.next());
        assert_eq!(close_code_of(h.next()), close_codes::A2A_PROTOCOL_ERROR);
    }

    #[tokio::test]
    async fn invalid_utf8_closes_with_1007_and_no_error() {
        let mut h = Harness::new();
        assert!(!h.send(&[b'{', 0xff, 0xfe, b'}']).await);
        assert_eq!(close_code_of(h.next()), close_codes::INVALID_PAYLOAD);
        h.assert_silent();
    }

    #[tokio::test]
    async fn bad_jsonrpc_version_is_invalid_request() {
        let mut h = Harness::new();
        assert!(
            h.send(br#"{"jsonrpc":"1.0","id":"r","method":"GetTask"}"#)
                .await
        );
        let resp = frame_payload(h.next());
        assert_eq!(resp.error.unwrap().code, error_code::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn a_request_with_an_invalid_shape_is_invalid_request_with_its_id() {
        let mut h = Harness::new();
        assert!(h.send(br#"{"jsonrpc":"2.0","id":"r","method":42}"#).await);
        let resp = frame_payload(h.next());
        assert_eq!(resp.id, Some(JsonRpcId::String("r".into())));
        assert_eq!(resp.error.unwrap().code, error_code::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn missing_method_sends_invalid_request() {
        let mut h = Harness::new();
        assert!(
            h.send_json(serde_json::json!({"jsonrpc": "2.0", "id": "req-1"}))
                .await
        );
        let response = frame_payload(h.next());
        assert_eq!(response.id, Some(JsonRpcId::String("req-1".into())));
        assert_eq!(response.error.unwrap().code, error_code::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn unknown_method_sends_method_not_found() {
        let mut h = Harness::new();
        assert!(
            h.send_json(serde_json::json!({"jsonrpc": "2.0", "id": "req-1", "method": "Bogus"}))
                .await
        );
        let response = frame_payload(h.next());
        assert_eq!(response.error.unwrap().code, error_code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn unknown_control_request_sends_method_not_found() {
        let mut h = Harness::new();
        assert!(
            h.send_json(serde_json::json!({"jsonrpc": "2.0", "id": "req-1", "method": "ws/Bogus"}))
                .await
        );
        let response = frame_payload(h.next());
        assert_eq!(response.error.unwrap().code, error_code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn notifications_are_never_answered() {
        let mut h = Harness::new();
        for notification in [
            serde_json::json!({"jsonrpc": "2.0", "method": "ws/Bogus"}),
            serde_json::json!({"jsonrpc": "2.0", "method": "GetTask", "params": {"id": "t"}}),
            serde_json::json!({"jsonrpc": "2.0", "method": "ws/CancelStream", "params": {"id": "unknown"}}),
            serde_json::json!({"jsonrpc": "2.0", "method": "ws/CancelStream"}),
            serde_json::json!({"jsonrpc": "2.0", "method": "ws/CancelStream", "params": {"id": {}}}),
            serde_json::json!({"jsonrpc": "2.0", "method": 7}),
        ] {
            assert!(h.send_json(notification.clone()).await, "{notification}");
            h.assert_silent();
        }
    }

    #[tokio::test]
    async fn cancel_stream_notification_stops_the_registered_stream() {
        let mut h = Harness::new();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        h.streams.lock().await.insert(
            id_key(&JsonRpcId::from("stream-1")),
            (JsonRpcId::from("stream-1"), cancel_tx),
        );
        let payload =
            serde_json::to_vec(&WsRequestEnvelope::cancel_stream("stream-1".into())).unwrap();

        assert!(h.send(&payload).await);
        assert!(matches!(cancel_rx.await.unwrap(), StreamStop::Cancelled));
        h.assert_silent();
        assert!(h.streams.lock().await.is_empty());
    }

    #[tokio::test]
    async fn run_unary_request_errors_do_not_close_the_connection() {
        let handler = Arc::new(StubHandler::failing_send_message());
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);

        run_unary_request(
            methods::SEND_MESSAGE.into(),
            "req-1".into(),
            protojson_conv::to_value(&send_message_request()).unwrap(),
            ServiceParams::new(),
            handler,
            out_tx,
        )
        .await;

        let response = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(response.error.unwrap().code, error_code::PARSE_ERROR);
        assert!(out_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn run_streaming_request_emits_result_chunk_and_stream_end() {
        let handler = Arc::new(StubHandler::default());
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);

        run_streaming_request(
            methods::SEND_STREAMING_MESSAGE.into(),
            "stream-1".into(),
            protojson_conv::to_value(&send_message_request()).unwrap(),
            ServiceParams::new(),
            StreamContext {
                handler,
                streams: streams.clone(),
                out_tx,
                max_streams: None,
            },
        )
        .await;

        let chunk = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(chunk.id, Some(JsonRpcId::String("stream-1".into())));
        assert!(chunk.result.is_some(), "streaming chunk must use `result`");
        let end = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(end.method.as_deref(), Some(methods::STREAM_END));
        assert_eq!(end.params, Some(serde_json::json!({"id": "stream-1"})));
        assert!(end.id.is_none());
        assert!(streams.lock().await.is_empty());
    }

    async fn start_pending_stream(
        id: &str,
    ) -> (
        StreamRegistry,
        mpsc::Receiver<OutboundMessage>,
        tokio::task::JoinHandle<()>,
        oneshot::Sender<StreamStop>,
    ) {
        let handler = Arc::new(StubHandler::pending_stream());
        let streams: StreamRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);
        let join = tokio::spawn(run_streaming_request(
            methods::SEND_STREAMING_MESSAGE.into(),
            id.into(),
            protojson_conv::to_value(&send_message_request()).unwrap(),
            ServiceParams::new(),
            StreamContext {
                handler,
                streams: streams.clone(),
                out_tx,
                max_streams: None,
            },
        ));
        let stop_tx = loop {
            if let Some((_, tx)) = streams.lock().await.remove(&id_key(&JsonRpcId::from(id))) {
                break tx;
            }
            tokio::task::yield_now().await;
        };
        (streams, out_rx, join, stop_tx)
    }

    #[tokio::test]
    async fn run_streaming_request_emits_stream_end_after_cancellation() {
        let (_streams, mut out_rx, join, stop_tx) = start_pending_stream("stream-cancel").await;
        stop_tx.send(StreamStop::Cancelled).unwrap();

        let end = tokio::time::timeout(Duration::from_secs(1), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            frame_payload(end).params,
            Some(serde_json::json!({"id": "stream-cancel"}))
        );
        join.await.unwrap();
    }

    #[tokio::test]
    async fn run_streaming_request_sends_no_stream_end_after_a_failure() {
        let (_streams, mut out_rx, join, stop_tx) = start_pending_stream("stream-fail").await;
        stop_tx.send(StreamStop::Failed).unwrap();
        join.await.unwrap();
        assert!(out_rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn streams_beyond_the_cap_get_ws_too_many_in_flight() {
        let handler = Arc::new(StubHandler::default());
        let (busy_tx, _busy_rx) = oneshot::channel();
        let streams: StreamRegistry = Arc::new(Mutex::new(HashMap::from([(
            id_key(&JsonRpcId::from("busy")),
            (JsonRpcId::from("busy"), busy_tx),
        )])));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);

        run_streaming_request(
            methods::SEND_STREAMING_MESSAGE.into(),
            "over".into(),
            protojson_conv::to_value(&send_message_request()).unwrap(),
            ServiceParams::new(),
            StreamContext {
                handler,
                streams,
                out_tx,
                max_streams: Some(1),
            },
        )
        .await;

        let resp = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(resp.error.as_ref().unwrap().code, crate::SERVER_ERROR_CODE);
        assert_eq!(
            binding_reason_of(&resp).as_deref(),
            Some("WS_TOO_MANY_IN_FLIGHT")
        );
    }

    async fn authenticate(
        conn_auth: &Arc<ConnAuth>,
        params: Value,
    ) -> (bool, Vec<OutboundMessage>) {
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);
        let keep_open = handle_authenticate(conn_auth, &out_tx, "auth-1".into(), params).await;
        drop(out_tx);
        let mut out = Vec::new();
        while let Some(message) = out_rx.recv().await {
            out.push(message);
        }
        (keep_open, out)
    }

    fn oauth_credentials() -> Value {
        serde_json::json!({"credentials": {"bearer": "new"}})
    }

    #[tokio::test]
    async fn authenticate_without_support_is_method_not_found() {
        let mut auth = RefreshAuth::new(AuthStatus::Valid);
        auth.supports_refresh = false;
        for conn_auth in [no_auth(), conn_auth_with(Arc::new(auth))] {
            let (keep_open, mut out) = authenticate(&conn_auth, oauth_credentials()).await;
            assert!(keep_open);
            let resp = frame_payload(out.remove(0));
            assert_eq!(resp.error.unwrap().code, error_code::METHOD_NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn authenticate_success_returns_an_empty_result() {
        let conn_auth = conn_auth_with(Arc::new(RefreshAuth::new(AuthStatus::Valid)));
        let (keep_open, mut out) = authenticate(&conn_auth, oauth_credentials()).await;
        assert!(keep_open);
        let resp = frame_payload(out.remove(0));
        assert_eq!(resp.id, Some(JsonRpcId::String("auth-1".into())));
        assert!(resp.result.unwrap().as_object().unwrap().is_empty());
        assert_eq!(conn_auth.generation.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn authenticate_success_returns_expires_at_when_known() {
        let expires_at = chrono::DateTime::parse_from_rfc3339("2026-11-10T10:00:00.250Z")
            .unwrap()
            .with_timezone(&Utc);
        let mut auth = RefreshAuth::new(AuthStatus::Valid);
        auth.expires_at = Some(expires_at);
        let conn_auth = conn_auth_with(Arc::new(auth));
        let (_, mut out) = authenticate(&conn_auth, oauth_credentials()).await;
        let result = frame_payload(out.remove(0)).result.unwrap();
        assert_eq!(result["expiresAt"], format_timestamp(&expires_at));
        assert_eq!(result["expiresAt"], "2026-11-10T10:00:00.250Z");
    }

    #[tokio::test]
    async fn authenticate_with_rejected_credentials_is_ws_unauthenticated() {
        let mut auth = RefreshAuth::new(AuthStatus::Valid);
        auth.refresh = Err(AuthError::unauthorized("bad token"));
        let conn_auth = conn_auth_with(Arc::new(auth));
        let (keep_open, mut out) = authenticate(&conn_auth, oauth_credentials()).await;
        assert!(keep_open);

        let resp = frame_payload(out.remove(0));
        assert_eq!(resp.error.as_ref().unwrap().code, crate::SERVER_ERROR_CODE);
        assert_eq!(
            binding_reason_of(&resp).as_deref(),
            Some("WS_UNAUTHENTICATED")
        );
        let data = resp.error.unwrap().data.unwrap();
        assert_eq!(data[0]["metadata"]["schemes"], "bearer");
        // The current credentials are kept.
        assert_eq!(
            conn_auth.ctx.lock().unwrap().user.as_ref().unwrap().name,
            "alice"
        );
    }

    #[tokio::test]
    async fn authenticate_as_another_principal_is_ws_permission_denied() {
        let mut auth = RefreshAuth::new(AuthStatus::Valid);
        auth.refresh = Ok("mallory");
        let conn_auth = conn_auth_with(Arc::new(auth));
        let (_, mut out) = authenticate(&conn_auth, oauth_credentials()).await;

        let resp = frame_payload(out.remove(0));
        assert_eq!(
            binding_reason_of(&resp).as_deref(),
            Some("WS_PERMISSION_DENIED")
        );
        assert_eq!(
            conn_auth.ctx.lock().unwrap().user.as_ref().unwrap().name,
            "alice"
        );
    }

    #[tokio::test]
    async fn authenticate_with_an_unknown_scheme_is_invalid_params() {
        let mut auth = RefreshAuth::new(AuthStatus::Valid);
        auth.refresh = Err(AuthError::invalid_params("unknown scheme"));
        let conn_auth = conn_auth_with(Arc::new(auth));
        for params in [
            oauth_credentials(),
            serde_json::json!({"credentials": {}}),
            serde_json::json!({"scheme": "Bearer", "credentials": "x"}),
        ] {
            let conn_auth = conn_auth.clone();
            conn_auth
                .failed_authenticate_attempts
                .store(0, Ordering::SeqCst);
            let (_, mut out) = authenticate(&conn_auth, params).await;
            let resp = frame_payload(out.remove(0));
            assert_eq!(resp.error.unwrap().code, error_code::INVALID_PARAMS);
        }
    }

    #[tokio::test]
    async fn repeated_authenticate_failures_close_with_1008() {
        let mut auth = RefreshAuth::new(AuthStatus::Valid);
        auth.refresh = Err(AuthError::unauthorized("bad token"));
        let conn_auth = conn_auth_with(Arc::new(auth));
        for _ in 1..MAX_FAILED_AUTHENTICATE_ATTEMPTS {
            let (keep_open, out) = authenticate(&conn_auth, oauth_credentials()).await;
            assert!(keep_open);
            assert_eq!(out.len(), 1);
        }
        let (keep_open, mut out) = authenticate(&conn_auth, oauth_credentials()).await;
        assert!(!keep_open);
        frame_payload(out.remove(0));
        assert_eq!(close_code_of(out.remove(0)), close_codes::POLICY_VIOLATION);
    }

    #[tokio::test]
    async fn ws_authenticate_is_dispatched_from_a_text_frame() {
        let conn_auth = conn_auth_with(Arc::new(RefreshAuth::new(AuthStatus::Valid)));
        let mut h = Harness::with_auth(conn_auth);
        assert!(
            h.send_json(serde_json::json!({
                "jsonrpc": "2.0",
                "id": "auth-1",
                "method": "ws/Authenticate",
                "params": {"credentials": {"bearer": "new"}}
            }))
            .await
        );
        let resp = frame_payload(h.next());
        assert!(resp.result.is_some());
    }

    #[tokio::test]
    async fn expired_credentials_fail_the_request_and_streams_then_close_with_4001() {
        let conn_auth = conn_auth_with(Arc::new(RefreshAuth::new(AuthStatus::Expired {
            reason: "revoked".into(),
        })));
        let (stream_tx, stream_rx) = oneshot::channel();
        let streams: StreamRegistry = Arc::new(Mutex::new(HashMap::from([(
            id_key(&JsonRpcId::from("stream-1")),
            (JsonRpcId::from("stream-1"), stream_tx),
        )])));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);

        let decision =
            check_connection_auth(&conn_auth, &streams, &out_tx, &JsonRpcId::from("r")).await;
        assert!(matches!(decision, AuthCheck::Fatal));

        let resp = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(resp.id, Some(JsonRpcId::String("r".into())));
        assert_eq!(
            binding_reason_of(&resp).as_deref(),
            Some("WS_CREDENTIALS_EXPIRED")
        );
        let stream_err = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(stream_err.id, Some(JsonRpcId::String("stream-1".into())));
        assert_eq!(
            binding_reason_of(&stream_err).as_deref(),
            Some("WS_CREDENTIALS_EXPIRED")
        );
        assert!(matches!(stream_rx.await.unwrap(), StreamStop::Failed));
        assert_eq!(
            close_code_of(out_rx.recv().await.unwrap()),
            close_codes::AUTHENTICATION_REQUIRED
        );
    }

    #[tokio::test]
    async fn expiring_credentials_send_reauthentication_required_once_then_close_at_expiry() {
        let expires_at = Utc::now() + chrono::Duration::milliseconds(100);
        let conn_auth = conn_auth_with(Arc::new(RefreshAuth::new(AuthStatus::ExpiresAt {
            expires_at,
        })));
        let streams: StreamRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);

        let decision =
            check_connection_auth(&conn_auth, &streams, &out_tx, &JsonRpcId::from("r")).await;
        assert!(matches!(decision, AuthCheck::Proceed));

        let resp = frame_payload(out_rx.recv().await.unwrap());
        assert!(resp.id.is_none(), "notifications carry no id");
        assert_eq!(
            resp.method.as_deref(),
            Some(methods::REAUTHENTICATION_REQUIRED)
        );
        assert_eq!(
            resp.params.unwrap()["expiresAt"],
            format_timestamp(&expires_at)
        );

        // A later request does not repeat the notification.
        let decision =
            check_connection_auth(&conn_auth, &streams, &out_tx, &JsonRpcId::from("r2")).await;
        assert!(matches!(decision, AuthCheck::Proceed));

        let msg = tokio::time::timeout(Duration::from_secs(2), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(Utc::now() >= expires_at, "closed before expiresAt");
        assert_eq!(close_code_of(msg), close_codes::AUTHENTICATION_REQUIRED);
    }

    #[tokio::test]
    async fn an_in_band_refresh_cancels_the_close_at_expiry() {
        let expires_at = Utc::now() + chrono::Duration::milliseconds(50);
        let conn_auth = conn_auth_with(Arc::new(RefreshAuth::new(AuthStatus::ExpiresAt {
            expires_at,
        })));
        let streams: StreamRegistry = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);

        check_connection_auth(&conn_auth, &streams, &out_tx, &JsonRpcId::from("r")).await;
        frame_payload(out_rx.recv().await.unwrap());

        let (keep_open, _) = authenticate(&conn_auth, oauth_credentials()).await;
        assert!(keep_open);

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            out_rx.try_recv().is_err(),
            "the 4001 close should be cancelled"
        );
    }

    #[tokio::test]
    async fn get_extended_agent_card_unsupported_reason_is_carried_in_error_data() {
        let handler = Arc::new(StubHandler::default());
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);
        run_unary_request(
            methods::GET_EXTENDED_AGENT_CARD.into(),
            "req".into(),
            protojson_conv::to_value(&GetExtendedAgentCardRequest { tenant: None }).unwrap(),
            ServiceParams::new(),
            handler,
            out_tx,
        )
        .await;
        let resp = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(
            resp.error.as_ref().unwrap().code,
            error_code::UNSUPPORTED_OPERATION
        );
        assert_eq!(error_reason_of(&resp), "UNSUPPORTED_OPERATION");
    }

    #[tokio::test]
    async fn websocket_router_rejects_requests_without_subprotocol() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let router = websocket_router(make_handler());
        let request = Request::builder().uri("/").body(Body::empty()).unwrap();
        let response = router.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
