// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, Ordering};

use a2a::*;
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use a2a_server::RequestHandler;
use a2a_server::middleware::ServiceParams;
use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use fastwebsockets::{
    FragmentCollector, Frame, OpCode, Payload, WebSocketError, upgrade::IncomingUpgrade,
};
use futures::stream::{BoxStream, StreamExt};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::auth::{AuthContext, AuthStatus, AuthenticateParams, WsAuthenticator};
use crate::common::{
    DEFAULT_MAX_FRAME_BYTES, JSONRPC_VERSION, JsonRpcId, SERVER_ERROR_CODE, SUBPROTOCOL,
    WsRequestEnvelope, WsResponseEnvelope, close_codes, id_key, methods,
    service_params_from_envelope,
};
use crate::errors::{a2a_error_to_jsonrpc, close_code_for_fatal, close_code_for_read_error};
use crate::ratelimit::{ConnectionRateLimiter, IdentityRateLimiter, RateLimitPolicy};

const SEC_WEBSOCKET_PROTOCOL: &str = "sec-websocket-protocol";
const OUTBOUND_BUFFER_CAPACITY: usize = 64;

/// Error message returned when an inbound rate limit is exceeded. The exact
/// wording is prescribed by spec Section 13.3.
const RATE_LIMIT_EXCEEDED: &str = "Rate limit exceeded";

/// Server configuration for the WebSocket binding.
#[derive(Clone, Default)]
pub struct WebSocketConfig {
    /// Optional handshake authenticator (spec Section 9). When `None`, the
    /// binding performs no authentication and treats every connection as
    /// anonymous — appropriate only when a trusted layer in front of the agent
    /// handles authentication.
    pub authenticator: Option<Arc<dyn WsAuthenticator>>,
    /// Optional allowlist of permitted `Origin` header values (spec Section
    /// 13.2, Cross-Site WebSocket Hijacking mitigation). When `Some`, upgrades
    /// whose `Origin` is missing or not listed are rejected with `403`.
    pub allowed_origins: Option<Vec<String>>,
    /// Maximum accepted size of a single inbound message. A larger message is
    /// rejected by closing the connection with code `1009` (spec Section 3.6).
    /// `None` applies the spec's recommended [`DEFAULT_MAX_FRAME_BYTES`].
    pub max_frame_bytes: Option<usize>,
    /// Inbound message rate limit, enforced per-connection and per-authenticated
    /// identity (spec Section 13.3). Defaults to [`DEFAULT_RATE_LIMIT`]; switch
    /// it off explicitly with [`RateLimitPolicy::Disabled`].
    ///
    /// [`DEFAULT_RATE_LIMIT`]: crate::ratelimit::DEFAULT_RATE_LIMIT
    pub rate_limit: RateLimitPolicy,
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
    /// connection served by this router (spec Section 13.3).
    pub identity_rate_limiter: Option<Arc<IdentityRateLimiter>>,
}

impl<H: RequestHandler> Clone for WebSocketState<H> {
    fn clone(&self) -> Self {
        WebSocketState {
            handler: self.handler.clone(),
            config: self.config.clone(),
            identity_rate_limiter: self.identity_rate_limiter.clone(),
        }
    }
}

/// Per-connection authentication state (spec Section 9.2).
struct ConnAuth {
    authenticator: Option<Arc<dyn WsAuthenticator>>,
    ctx: StdMutex<AuthContext>,
    /// Service parameters derived from the handshake and the *current* context.
    /// Held here rather than alongside the connection loop so that an in-band
    /// refresh replaces the identity the handler sees at the same moment it
    /// replaces the credentials (Section 9.3.3).
    params: StdMutex<Arc<ConnectionParams>>,
    reauth_signaled: AtomicBool,
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
    let state = WebSocketState {
        handler,
        config: Arc::new(config),
        identity_rate_limiter,
    };
    axum::Router::new()
        .route("/", axum::routing::any(handle_upgrade::<H>))
        .with_state(state)
}

async fn handle_upgrade<H: RequestHandler>(
    State(state): State<WebSocketState<H>>,
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

    // Origin validation (spec Section 13.2) — only enforced when an allowlist
    // is configured.
    if let Some(allowed) = state.config.allowed_origins.as_ref() {
        if !origin_allowed(&headers, allowed) {
            return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
        }
    }

    // Handshake authentication (spec Section 9.1). On failure the upgrade is
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

    let connection_params = ConnectionParams::new(&headers, &auth_ctx);

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
    // anonymous connection is limited by its own bucket alone (Section 13.3).
    let rate_limiter = state.config.rate_limit.limit().map(|limit| {
        ConnectionRateLimiter::new(
            limit,
            auth_ctx.user.as_ref().map(|user| user.name.clone()),
            state.identity_rate_limiter.clone(),
        )
    });
    let conn_auth = Arc::new(ConnAuth {
        authenticator: state.config.authenticator.clone(),
        ctx: StdMutex::new(auth_ctx),
        params: StdMutex::new(Arc::new(connection_params)),
        reauth_signaled: AtomicBool::new(false),
    });
    tokio::spawn(async move {
        match fut.await {
            Ok(ws) => run_connection(ws, handler, conn_auth, max_frame_bytes, rate_limiter).await,
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

fn capture_connection_params(headers: &HeaderMap) -> ServiceParams {
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
    Close { code: u16, reason: String },
}

type StreamRegistry = Arc<Mutex<HashMap<String, oneshot::Sender<()>>>>;

async fn run_connection<H: RequestHandler>(
    mut ws: fastwebsockets::WebSocket<TokioIo<Upgraded>>,
    handler: Arc<H>,
    conn_auth: Arc<ConnAuth>,
    max_frame_bytes: usize,
    mut rate_limiter: Option<ConnectionRateLimiter>,
) {
    ws.set_max_message_size(max_frame_bytes);
    ws.set_auto_close(true);
    ws.set_auto_pong(true);
    let mut ws = FragmentCollector::new(ws);

    let (out_tx, mut out_rx) = mpsc::channel::<OutboundMessage>(OUTBOUND_BUFFER_CAPACITY);
    let streams: StreamRegistry = Arc::new(Mutex::new(HashMap::new()));

    // Set when the connection is torn down after queueing a final error and
    // Close frame, which still have to be written before the socket goes away.
    let mut flush_pending = false;

    loop {
        tokio::select! {
            biased;

            outbound = out_rx.recv() => {
                let Some(message) = outbound else { break };
                match message {
                    OutboundMessage::Frame(text) => {
                        if let Err(err) = ws
                            .write_frame(Frame::text(Payload::Owned(text.into_bytes())))
                            .await
                        {
                            tracing::debug!(error = %err, "failed to write frame; closing");
                            break;
                        }
                    }
                    OutboundMessage::Close { code, reason } => {
                        let _ = ws
                            .write_frame(Frame::close(code, reason.as_bytes()))
                            .await;
                        break;
                    }
                }
            }

            incoming = ws.read_frame() => {
                match incoming {
                    Ok(frame) => match frame.opcode {
                        OpCode::Close => break,
                        OpCode::Text => {
                            if !rate_limit_allows(rate_limiter.as_mut(), &out_tx).await {
                                flush_pending = true;
                                break;
                            }
                            if !handle_text_frame(
                                &frame.payload,
                                &handler,
                                &streams,
                                &conn_auth,
                                &out_tx,
                            )
                            .await
                            {
                                flush_pending = true;
                                break;
                            }
                        }
                        OpCode::Binary => {
                            let _ = ws
                                .write_frame(Frame::close(
                                    close_codes::UNSUPPORTED_DATA,
                                    b"binary frames are reserved for future use",
                                ))
                                .await;
                            break;
                        }
                        // Ping/pong are handled internally when auto_pong = true.
                        _ => {}
                    },
                    Err(WebSocketError::ConnectionClosed) => break,
                    Err(err) => {
                        tracing::debug!(error = %err, "websocket read error; closing");
                        // An oversize message must be answered with 1009 and a
                        // framing violation with 1002, rather than a bare
                        // disconnect (spec Sections 3.6 and 2.3).
                        if let Some(code) = close_code_for_read_error(&err) {
                            let _ = ws
                                .write_frame(Frame::close(code, err.to_string().as_bytes()))
                                .await;
                        }
                        break;
                    }
                }
            }
        }
    }

    if flush_pending {
        flush_outbound(&mut ws, &mut out_rx).await;
    }

    cancel_all_streams(&streams).await;
}

/// Admit one inbound message, or queue the `-32000` error and the `1008` close
/// the spec prescribes when a limit is exceeded (Section 13.3).
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
        &A2AError::new(SERVER_ERROR_CODE, RATE_LIMIT_EXCEEDED),
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

/// Write everything already queued on the outbound channel, stopping after a
/// Close frame. Without this, a fatal error response and its Close frame would
/// be dropped when the read loop exits, leaving the peer to guess why.
async fn flush_outbound(
    ws: &mut FragmentCollector<TokioIo<Upgraded>>,
    out_rx: &mut mpsc::Receiver<OutboundMessage>,
) {
    while let Ok(message) = out_rx.try_recv() {
        match message {
            OutboundMessage::Frame(text) => {
                if ws
                    .write_frame(Frame::text(Payload::Owned(text.into_bytes())))
                    .await
                    .is_err()
                {
                    return;
                }
            }
            OutboundMessage::Close { code, reason } => {
                let _ = ws.write_frame(Frame::close(code, reason.as_bytes())).await;
                return;
            }
        }
    }
}

async fn cancel_all_streams(streams: &StreamRegistry) {
    let mut map = streams.lock().await;
    for (_id, tx) in map.drain() {
        let _ = tx.send(());
    }
}

enum AuthCheck {
    Proceed,
    Fatal,
}

/// Revalidate the connection's credentials before dispatching a request
/// (spec Section 9.3.1 / 9.3.2).
async fn check_connection_auth(
    conn_auth: &Arc<ConnAuth>,
    out_tx: &mpsc::Sender<OutboundMessage>,
    id: &JsonRpcId,
) -> AuthCheck {
    let Some(authenticator) = conn_auth.authenticator.as_ref() else {
        return AuthCheck::Proceed;
    };
    let ctx = conn_auth.ctx.lock().unwrap().clone();
    match authenticator.revalidate(&ctx).await {
        AuthStatus::Valid => AuthCheck::Proceed,
        AuthStatus::ReauthRequired {
            reason,
            retry_after_ms,
        } => {
            if !conn_auth.reauth_signaled.swap(true, Ordering::SeqCst) {
                send_outbound(
                    out_tx,
                    OutboundMessage::Frame(serialize_response(
                        WsResponseEnvelope::reauth_required(reason, retry_after_ms),
                    )),
                )
                .await;
                // Allow a grace period for in-flight requests, then close with
                // 4001 so the client reconnects with fresh credentials. A
                // successful in-band `Authenticate` in the meantime clears the
                // flag and cancels the close, which is the whole point of
                // Section 9.3.3.
                let grace = authenticator.reauth_grace();
                let out_tx = out_tx.clone();
                let conn_auth = conn_auth.clone();
                tokio::spawn(async move {
                    tokio::time::sleep(grace).await;
                    if !conn_auth.reauth_signaled.load(Ordering::SeqCst) {
                        tracing::debug!("credentials refreshed in-band; cancelling 4001 close");
                        return;
                    }
                    let _ = out_tx
                        .send(OutboundMessage::Close {
                            code: close_codes::AUTHENTICATION_REQUIRED,
                            reason: "reauthentication required".to_string(),
                        })
                        .await;
                });
            }
            AuthCheck::Proceed
        }
        AuthStatus::Expired { reason } => {
            send_error(
                out_tx,
                Some(id.clone()),
                &A2AError::new(SERVER_ERROR_CODE, reason),
            )
            .await;
            send_outbound(
                out_tx,
                OutboundMessage::Close {
                    code: close_codes::AUTHENTICATION_REQUIRED,
                    reason: "authentication expired or revoked".to_string(),
                },
            )
            .await;
            AuthCheck::Fatal
        }
    }
}

/// Handle the binding-specific in-band `Authenticate` refresh method
/// (spec Section 9.3.3).
async fn handle_authenticate(
    conn_auth: &Arc<ConnAuth>,
    out_tx: &mpsc::Sender<OutboundMessage>,
    id: JsonRpcId,
    params: Value,
) {
    let supported = conn_auth
        .authenticator
        .as_ref()
        .map(|a| a.supports_in_band_refresh())
        .unwrap_or(false);
    if !supported {
        send_error(
            out_tx,
            Some(id),
            &A2AError::unsupported_operation("in-band token refresh is not supported"),
        )
        .await;
        return;
    }
    let authenticator = conn_auth.authenticator.as_ref().unwrap();

    let parsed: AuthenticateParams = match serde_json::from_value(params) {
        Ok(p) => p,
        Err(err) => {
            send_error(
                out_tx,
                Some(id),
                &A2AError::invalid_params(format!("invalid Authenticate params: {err}")),
            )
            .await;
            return;
        }
    };

    let current = conn_auth.ctx.lock().unwrap().clone();
    match authenticator
        .refresh(&current, &parsed.scheme, &parsed.credentials)
        .await
    {
        Ok(new_ctx) => {
            // The parameters handed to the handler are derived from the
            // context, so they have to be rebuilt here; otherwise the
            // connection would keep acting as the principal it just replaced.
            let refreshed = conn_auth.params().reauthenticated(&new_ctx);
            *conn_auth.params.lock().unwrap() = Arc::new(refreshed);
            *conn_auth.ctx.lock().unwrap() = new_ctx;
            conn_auth.reauth_signaled.store(false, Ordering::SeqCst);
            send_outbound(
                out_tx,
                OutboundMessage::Frame(serialize_response(WsResponseEnvelope::result(
                    id,
                    Value::Object(serde_json::Map::new()),
                ))),
            )
            .await;
        }
        Err(auth_err) => {
            send_error(
                out_tx,
                Some(id),
                &A2AError::new(SERVER_ERROR_CODE, auth_err.message),
            )
            .await;
        }
    }
}

/// Returns `false` if the connection should be terminated (fatal protocol
/// error already signalled to the client via the outbound channel).
async fn handle_text_frame<H: RequestHandler>(
    payload: &[u8],
    handler: &Arc<H>,
    streams: &StreamRegistry,
    conn_auth: &Arc<ConnAuth>,
    out_tx: &mpsc::Sender<OutboundMessage>,
) -> bool {
    let envelope: WsRequestEnvelope = match serde_json::from_slice(payload) {
        Ok(envelope) => envelope,
        Err(err) => {
            send_error(
                out_tx,
                Some(JsonRpcId::Null),
                &A2AError::parse_error(format!("invalid JSON envelope: {err}")),
            )
            .await;
            send_outbound(
                out_tx,
                OutboundMessage::Close {
                    code: close_codes::PROTOCOL_ERROR,
                    reason: "JSON parse error".to_string(),
                },
            )
            .await;
            return false;
        }
    };

    if envelope.jsonrpc != JSONRPC_VERSION {
        send_error(
            out_tx,
            envelope.id.clone(),
            &A2AError::invalid_request(format!(
                "unsupported jsonrpc version: '{}'",
                envelope.jsonrpc
            )),
        )
        .await;
        return true;
    }

    let Some(id) = envelope.id.clone() else {
        send_error(
            out_tx,
            Some(JsonRpcId::Null),
            &A2AError::invalid_request("request id is required"),
        )
        .await;
        return true;
    };

    if matches!(id, JsonRpcId::Null) {
        send_error(
            out_tx,
            Some(JsonRpcId::Null),
            &A2AError::invalid_request("request id must not be null"),
        )
        .await;
        return true;
    }

    if envelope.cancel_stream.unwrap_or(false) {
        let key = id_key(&id);
        let streams = streams.clone();
        tokio::spawn(async move {
            if let Some(tx) = streams.lock().await.remove(&key) {
                let _ = tx.send(());
            }
        });
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

    // In-band token refresh is a binding-specific method handled outside the
    // normal A2A dispatch and is exempt from revalidation (spec Section 9.3.3).
    if method == methods::AUTHENTICATE {
        let params = envelope.params.clone().unwrap_or(Value::Null);
        let conn_auth = conn_auth.clone();
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            handle_authenticate(&conn_auth, &out_tx, id, params).await;
        });
        return true;
    }

    // Per-request credential revalidation (spec Section 9.3.1).
    match check_connection_auth(conn_auth, out_tx, &id).await {
        AuthCheck::Proceed => {}
        AuthCheck::Fatal => return false,
    }

    if !methods::is_known(&method) {
        send_error(out_tx, Some(id), &A2AError::method_not_found(&method)).await;
        return true;
    }

    let combined_params = combine_service_params(&conn_auth.params(), &envelope);
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
                handler,
                streams,
                out_tx_task,
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
    fn new(headers: &HeaderMap, auth_ctx: &AuthContext) -> Self {
        Self::from_base(capture_connection_params(headers), auth_ctx)
    }

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
    /// identity, tenant, or scopes it authenticated with earlier
    /// (Section 9.3.3).
    fn reauthenticated(&self, auth_ctx: &AuthContext) -> Self {
        Self::from_base(self.base.clone(), auth_ctx)
    }
}

/// Merge the connection-scoped parameters with a request's own `serviceParams`.
///
/// Per-request entries override connection-scoped ones, *except* for keys the
/// authenticator established (spec Section 9.1). Letting a request overwrite
/// those would allow a client that authenticated as one identity or tenant to
/// present itself to the handler as another.
fn combine_service_params(
    connection: &ConnectionParams,
    envelope: &WsRequestEnvelope,
) -> ServiceParams {
    let mut combined = connection.params.clone();
    if let Some(per_request) = envelope.service_params.as_ref() {
        for (key, values) in service_params_from_envelope(per_request) {
            if connection.authenticated.contains(&key) {
                tracing::debug!(
                    %key,
                    "ignoring per-request override of an authenticator-established service param"
                );
                continue;
            }
            combined.insert(key, values);
        }
    }
    combined
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
        Err(err) => {
            send_error(&out_tx, Some(id), &err).await;
            if let Some(code) = close_code_for_fatal(&err) {
                send_outbound(
                    &out_tx,
                    OutboundMessage::Close {
                        code,
                        reason: err.message,
                    },
                )
                .await;
            }
        }
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

async fn run_streaming_request<H: RequestHandler>(
    method: String,
    id: JsonRpcId,
    raw_params: Value,
    params: ServiceParams,
    handler: Arc<H>,
    streams: StreamRegistry,
    out_tx: mpsc::Sender<OutboundMessage>,
) {
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
            send_error(&out_tx, Some(id), &err).await;
            return;
        }
    };

    let key = id_key(&id);
    let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
    {
        let mut map = streams.lock().await;
        map.insert(key.clone(), cancel_tx);
    }

    let mut errored = false;
    loop {
        tokio::select! {
            biased;

            _ = &mut cancel_rx => {
                // Cancellation: stop sending events; the final streamEnd is
                // emitted below once the stream has been removed from the registry.
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
    use crate::auth::{AuthContext, AuthError};
    use a2a_server::handler::DefaultRequestHandler;
    use a2a_server::task_store::InMemoryTaskStore;
    use async_trait::async_trait;
    use axum::http::HeaderValue;

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

    fn no_auth() -> Arc<ConnAuth> {
        Arc::new(ConnAuth {
            authenticator: None,
            ctx: StdMutex::new(AuthContext::default()),
            params: StdMutex::new(Arc::new(ConnectionParams::default())),
            reauth_signaled: AtomicBool::new(false),
        })
    }

    fn conn_auth_with(authenticator: Arc<dyn WsAuthenticator>) -> Arc<ConnAuth> {
        Arc::new(ConnAuth {
            authenticator: Some(authenticator),
            ctx: StdMutex::new(AuthContext::default()),
            params: StdMutex::new(Arc::new(ConnectionParams::default())),
            reauth_signaled: AtomicBool::new(false),
        })
    }

    #[derive(Default)]
    struct StubHandler {
        send_message_error: Option<A2AError>,
        streaming_pending: bool,
    }

    impl StubHandler {
        fn fatal_send_message() -> Self {
            Self {
                send_message_error: Some(A2AError::new(error_code::PARSE_ERROR, "fatal parse")),
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

    fn frame_payload(message: OutboundMessage) -> WsResponseEnvelope {
        match message {
            OutboundMessage::Frame(text) => serde_json::from_str(&text).unwrap(),
            OutboundMessage::Close { .. } => panic!("expected frame"),
        }
    }

    fn error_reason_of(resp: &WsResponseEnvelope) -> String {
        let err = resp.error.as_ref().expect("error present");
        let data = err.data.as_ref().expect("data present");
        let arr = data.as_array().expect("array");
        arr.last().unwrap()["reason"].as_str().unwrap().to_string()
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

    // In-band-refresh authenticator used to exercise the auth pathways.
    struct RefreshAuth {
        status: AuthStatus,
        supports_refresh: bool,
        refresh_ok: bool,
    }

    #[async_trait]
    impl WsAuthenticator for RefreshAuth {
        async fn authenticate(&self, _headers: &HeaderMap) -> Result<AuthContext, AuthError> {
            Ok(AuthContext::default())
        }
        async fn revalidate(&self, _ctx: &AuthContext) -> AuthStatus {
            self.status.clone()
        }
        fn supports_in_band_refresh(&self) -> bool {
            self.supports_refresh
        }
        async fn refresh(
            &self,
            _ctx: &AuthContext,
            _scheme: &str,
            _credentials: &str,
        ) -> Result<AuthContext, AuthError> {
            if self.refresh_ok {
                Ok(AuthContext::default())
            } else {
                Err(AuthError::unauthorized("bad token"))
            }
        }
        fn reauth_grace(&self) -> std::time::Duration {
            std::time::Duration::from_millis(10)
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

        let params = capture_connection_params(&headers);
        assert_eq!(params.get("a2a-version").unwrap(), &vec!["1.0".to_string()]);
        assert_eq!(
            params.get("authorization").unwrap(),
            &vec!["Bearer t".to_string()]
        );
        assert!(!params.contains_key("host"));
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

    #[test]
    fn combine_service_params_per_request_overrides_connection_scope() {
        let mut connection = ConnectionParams::default();
        connection
            .params
            .insert("a2a-version".into(), vec!["1.0".into()]);
        connection
            .params
            .insert("x-keep".into(), vec!["preserve".into()]);

        let envelope = envelope_with_params(&[("a2a-version", "1.5"), ("x-extra", "added")]);

        let combined = combine_service_params(&connection, &envelope);
        assert_eq!(
            combined.get("a2a-version").unwrap(),
            &vec!["1.5".to_string()]
        );
        assert_eq!(
            combined.get("x-keep").unwrap(),
            &vec!["preserve".to_string()]
        );
        assert_eq!(combined.get("x-extra").unwrap(), &vec!["added".to_string()]);
    }

    #[test]
    fn combine_service_params_refuses_to_override_authenticated_params() {
        let auth_ctx = AuthContext {
            service_params: HashMap::from([("x-tenant".to_string(), vec!["acme".to_string()])]),
            ..Default::default()
        };
        let connection = ConnectionParams::new(&HeaderMap::new(), &auth_ctx);

        let envelope = envelope_with_params(&[("x-tenant", "evil-corp")]);

        let combined = combine_service_params(&connection, &envelope);
        assert_eq!(combined.get("x-tenant").unwrap(), &vec!["acme".to_string()]);
    }

    #[test]
    fn combine_service_params_matches_authenticated_keys_case_insensitively() {
        let auth_ctx = AuthContext {
            service_params: HashMap::from([("X-Tenant".to_string(), vec!["acme".to_string()])]),
            ..Default::default()
        };
        let connection = ConnectionParams::new(&HeaderMap::new(), &auth_ctx);
        assert_eq!(
            connection.params.get("x-tenant").unwrap(),
            &vec!["acme".to_string()]
        );

        let envelope = envelope_with_params(&[("X-TENANT", "evil-corp")]);

        let combined = combine_service_params(&connection, &envelope);
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

        let connection = ConnectionParams::new(&headers, &auth_ctx);
        assert_eq!(
            connection.params.get("x-tenant").unwrap(),
            &vec!["acme".to_string()]
        );
    }

    #[test]
    fn serialize_response_emits_jsonrpc_stream_end() {
        let json = serialize_response(WsResponseEnvelope::stream_end("req-1".into()));
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["streamEnd"], true);
    }

    #[tokio::test]
    async fn handle_text_frame_invalid_json_sends_error_and_close() {
        let handler = Arc::new(StubHandler::default());
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);

        assert!(!handle_text_frame(b"{not json", &handler, &streams, &no_auth(), &out_tx).await);

        // The wire frame MUST carry an explicit `"id": null` per spec Section 3.3.
        let raw = match out_rx.try_recv().unwrap() {
            OutboundMessage::Frame(text) => text,
            OutboundMessage::Close { .. } => panic!("expected frame"),
        };
        let value: Value = serde_json::from_str(&raw).unwrap();
        assert!(value["id"].is_null());
        assert!(value.as_object().unwrap().contains_key("id"));
        assert_eq!(value["error"]["code"], error_code::PARSE_ERROR);

        match out_rx.try_recv().unwrap() {
            OutboundMessage::Close { code, .. } => assert_eq!(code, close_codes::PROTOCOL_ERROR),
            OutboundMessage::Frame(_) => panic!("expected close frame"),
        }
    }

    #[tokio::test]
    async fn handle_text_frame_bad_jsonrpc_version_is_invalid_request() {
        let handler = Arc::new(StubHandler::default());
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);
        let payload = br#"{"jsonrpc":"1.0","id":"r","method":"GetTask"}"#;

        assert!(handle_text_frame(payload, &handler, &streams, &no_auth(), &out_tx).await);
        let resp = frame_payload(out_rx.try_recv().unwrap());
        assert_eq!(resp.error.unwrap().code, error_code::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn handle_text_frame_missing_method_sends_invalid_request() {
        let handler = Arc::new(StubHandler::default());
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);
        let envelope = WsRequestEnvelope {
            id: Some("req-1".into()),
            ..Default::default()
        };
        let payload = serde_json::to_vec(&envelope).unwrap();

        assert!(handle_text_frame(&payload, &handler, &streams, &no_auth(), &out_tx).await);
        let response = frame_payload(out_rx.try_recv().unwrap());
        assert_eq!(response.id, Some(JsonRpcId::String("req-1".into())));
        assert_eq!(response.error.unwrap().code, error_code::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn handle_text_frame_unknown_method_sends_method_not_found() {
        let handler = Arc::new(StubHandler::default());
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);
        let envelope = WsRequestEnvelope {
            id: Some("req-1".into()),
            method: Some("Bogus".into()),
            ..Default::default()
        };
        let payload = serde_json::to_vec(&envelope).unwrap();

        assert!(handle_text_frame(&payload, &handler, &streams, &no_auth(), &out_tx).await);
        let response = frame_payload(out_rx.try_recv().unwrap());
        assert_eq!(response.error.unwrap().code, error_code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn handle_text_frame_cancel_stream_removes_registered_stream() {
        let handler = Arc::new(StubHandler::default());
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let (cancel_tx, cancel_rx) = oneshot::channel();
        streams
            .lock()
            .await
            .insert(id_key(&JsonRpcId::from("stream-1")), cancel_tx);
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);
        let envelope = WsRequestEnvelope {
            id: Some("stream-1".into()),
            cancel_stream: Some(true),
            ..Default::default()
        };
        let payload = serde_json::to_vec(&envelope).unwrap();

        assert!(handle_text_frame(&payload, &handler, &streams, &no_auth(), &out_tx).await);
        cancel_rx.await.unwrap();
        assert!(out_rx.try_recv().is_err());
        assert!(streams.lock().await.is_empty());
    }

    #[tokio::test]
    async fn run_unary_request_emits_close_for_fatal_error() {
        let handler = Arc::new(StubHandler::fatal_send_message());
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);
        let req = SendMessageRequest {
            message: sample_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        };

        run_unary_request(
            methods::SEND_MESSAGE.into(),
            "req-fatal".into(),
            protojson_conv::to_value(&req).unwrap(),
            ServiceParams::new(),
            handler,
            out_tx,
        )
        .await;

        let response = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(response.error.unwrap().code, error_code::PARSE_ERROR);
        match out_rx.recv().await.unwrap() {
            OutboundMessage::Close { code, .. } => assert_eq!(code, close_codes::PROTOCOL_ERROR),
            OutboundMessage::Frame(_) => panic!("expected close after fatal error"),
        }
    }

    #[tokio::test]
    async fn run_streaming_request_emits_result_chunk_and_stream_end() {
        let handler = Arc::new(StubHandler::default());
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);
        let req = SendMessageRequest {
            message: sample_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        };

        run_streaming_request(
            methods::SEND_STREAMING_MESSAGE.into(),
            "stream-1".into(),
            protojson_conv::to_value(&req).unwrap(),
            ServiceParams::new(),
            handler,
            streams.clone(),
            out_tx,
        )
        .await;

        let chunk = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(chunk.id, Some(JsonRpcId::String("stream-1".into())));
        assert!(chunk.result.is_some(), "streaming chunk must use `result`");
        let end = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(end.stream_end, Some(true));
        assert!(streams.lock().await.is_empty());
    }

    #[tokio::test]
    async fn run_streaming_request_emits_stream_end_after_cancellation() {
        let handler = Arc::new(StubHandler::pending_stream());
        let streams = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);
        let req = SendMessageRequest {
            message: sample_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let task_streams = streams.clone();
        let join = tokio::spawn(run_streaming_request(
            methods::SEND_STREAMING_MESSAGE.into(),
            "stream-cancel".into(),
            protojson_conv::to_value(&req).unwrap(),
            ServiceParams::new(),
            handler,
            task_streams,
            out_tx,
        ));

        let cancel_tx = loop {
            if let Some(tx) = streams
                .lock()
                .await
                .remove(&id_key(&JsonRpcId::from("stream-cancel")))
            {
                break tx;
            }
            tokio::task::yield_now().await;
        };
        cancel_tx.send(()).unwrap();

        let end = tokio::time::timeout(std::time::Duration::from_secs(1), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frame_payload(end).stream_end, Some(true));
        join.await.unwrap();
    }

    #[tokio::test]
    async fn authenticate_unsupported_returns_unsupported_operation() {
        let conn_auth = conn_auth_with(Arc::new(RefreshAuth {
            status: AuthStatus::Valid,
            supports_refresh: false,
            refresh_ok: false,
        }));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);

        handle_authenticate(
            &conn_auth,
            &out_tx,
            "auth-1".into(),
            serde_json::json!({"scheme":"Bearer","credentials":"x"}),
        )
        .await;

        let resp = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(resp.error.unwrap().code, error_code::UNSUPPORTED_OPERATION);
    }

    #[tokio::test]
    async fn authenticate_success_returns_empty_result() {
        let conn_auth = conn_auth_with(Arc::new(RefreshAuth {
            status: AuthStatus::Valid,
            supports_refresh: true,
            refresh_ok: true,
        }));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);

        handle_authenticate(
            &conn_auth,
            &out_tx,
            "auth-1".into(),
            serde_json::json!({"scheme":"Bearer","credentials":"new"}),
        )
        .await;

        let resp = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(resp.id, Some(JsonRpcId::String("auth-1".into())));
        assert!(resp.result.unwrap().as_object().unwrap().is_empty());
    }

    #[tokio::test]
    async fn authenticate_failure_returns_server_error_code() {
        let conn_auth = conn_auth_with(Arc::new(RefreshAuth {
            status: AuthStatus::Valid,
            supports_refresh: true,
            refresh_ok: false,
        }));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);

        handle_authenticate(
            &conn_auth,
            &out_tx,
            "auth-1".into(),
            serde_json::json!({"scheme":"Bearer","credentials":"bad"}),
        )
        .await;

        let resp = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(resp.error.unwrap().code, SERVER_ERROR_CODE);
    }

    #[tokio::test]
    async fn check_connection_auth_expired_sends_error_and_close() {
        let conn_auth = conn_auth_with(Arc::new(RefreshAuth {
            status: AuthStatus::Expired {
                reason: "revoked".into(),
            },
            supports_refresh: false,
            refresh_ok: false,
        }));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);

        let decision = check_connection_auth(&conn_auth, &out_tx, &JsonRpcId::from("r")).await;
        assert!(matches!(decision, AuthCheck::Fatal));

        let resp = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(resp.error.unwrap().code, SERVER_ERROR_CODE);
        match out_rx.recv().await.unwrap() {
            OutboundMessage::Close { code, .. } => {
                assert_eq!(code, close_codes::AUTHENTICATION_REQUIRED)
            }
            OutboundMessage::Frame(_) => panic!("expected 4001 close"),
        }
    }

    #[tokio::test]
    async fn check_connection_auth_reauth_required_emits_control_frame() {
        let conn_auth = conn_auth_with(Arc::new(RefreshAuth {
            status: AuthStatus::ReauthRequired {
                reason: "expiring".into(),
                retry_after_ms: 0,
            },
            supports_refresh: false,
            refresh_ok: false,
        }));
        let (out_tx, mut out_rx) = mpsc::channel(OUTBOUND_BUFFER_CAPACITY);

        let decision = check_connection_auth(&conn_auth, &out_tx, &JsonRpcId::from("r")).await;
        assert!(matches!(decision, AuthCheck::Proceed));

        let resp = frame_payload(out_rx.recv().await.unwrap());
        assert_eq!(
            resp.control.as_deref(),
            Some(crate::common::CONTROL_REAUTH_REQUIRED)
        );
        assert!(resp.id.is_none(), "control frames carry no id");

        // A subsequent request does not emit a second control frame.
        let decision = check_connection_auth(&conn_auth, &out_tx, &JsonRpcId::from("r2")).await;
        assert!(matches!(decision, AuthCheck::Proceed));
        // The grace-period close eventually arrives; the next message is a Close.
        let msg = tokio::time::timeout(std::time::Duration::from_secs(1), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(msg, OutboundMessage::Close { code, .. } if code == close_codes::AUTHENTICATION_REQUIRED)
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
