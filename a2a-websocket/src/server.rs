// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;
use std::sync::Arc;

use a2a::*;
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use a2a_server::RequestHandler;
use a2a_server::middleware::ServiceParams;
use axum::{
    extract::State,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use fastwebsockets::{
    FragmentCollector, Frame, OpCode, Payload, WebSocketError, upgrade::IncomingUpgrade,
};
use futures::stream::{BoxStream, StreamExt};
use hyper::upgrade::Upgraded;
use hyper_util::rt::TokioIo;
use serde_json::Value;
use tokio::sync::{Mutex, mpsc, oneshot};

use crate::common::{
    DEFAULT_MAX_FRAME_BYTES, SUBPROTOCOL, WsErrorObject, WsRequestEnvelope, WsResponseEnvelope,
    close_codes, error_types, methods, service_params_from_envelope,
};
use crate::errors::{a2a_error_to_ws_error, close_code_for_fatal};

const SEC_WEBSOCKET_PROTOCOL: &str = "sec-websocket-protocol";

/// Shared state for the WebSocket binding handler.
pub struct WebSocketState<H: RequestHandler> {
    pub handler: Arc<H>,
}

impl<H: RequestHandler> Clone for WebSocketState<H> {
    fn clone(&self) -> Self {
        WebSocketState {
            handler: self.handler.clone(),
        }
    }
}

/// Build an `axum::Router` exposing the A2A WebSocket binding at `/` of the
/// returned router.
///
/// Mount the router under whatever path your application uses for the
/// WebSocket endpoint (e.g. `Router::new().nest("/a2a/ws", websocket_router(handler))`).
///
/// Each accepted connection negotiates the `a2a.v1` sub-protocol and is
/// driven by a dedicated tokio task that multiplexes requests, streams, and
/// stream cancellations over the single connection.
pub fn websocket_router<H: RequestHandler>(handler: Arc<H>) -> axum::Router {
    let state = WebSocketState { handler };
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
            "Sec-WebSocket-Protocol header must include 'a2a.v1'",
        )
            .into_response();
    }

    let connection_params = capture_connection_params(&headers);

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
    tokio::spawn(async move {
        match fut.await {
            Ok(ws) => run_connection(ws, handler, connection_params).await,
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

fn capture_connection_params(headers: &HeaderMap) -> ServiceParams {
    let mut params: ServiceParams = HashMap::new();
    for (name, value) in headers {
        let key = name.as_str().to_ascii_lowercase();
        if is_internal_header(&key) {
            continue;
        }
        if let Ok(value) = value.to_str() {
            params
                .entry(key)
                .or_default()
                .push(value.to_string());
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
    connection_params: ServiceParams,
) {
    ws.set_max_message_size(DEFAULT_MAX_FRAME_BYTES);
    ws.set_auto_close(true);
    ws.set_auto_pong(true);
    let mut ws = FragmentCollector::new(ws);

    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<OutboundMessage>();
    let streams: StreamRegistry = Arc::new(Mutex::new(HashMap::new()));
    let connection_params = Arc::new(connection_params);

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
                            if !handle_text_frame(
                                &frame.payload,
                                &handler,
                                &connection_params,
                                &streams,
                                &out_tx,
                            ) {
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
                        break;
                    }
                }
            }
        }
    }

    cancel_all_streams(&streams).await;
}

async fn cancel_all_streams(streams: &StreamRegistry) {
    let mut map = streams.lock().await;
    for (_id, tx) in map.drain() {
        let _ = tx.send(());
    }
}

/// Returns `false` if the connection should be terminated (fatal protocol
/// error already signalled to the client via the outbound channel).
fn handle_text_frame<H: RequestHandler>(
    payload: &[u8],
    handler: &Arc<H>,
    connection_params: &Arc<ServiceParams>,
    streams: &StreamRegistry,
    out_tx: &mpsc::UnboundedSender<OutboundMessage>,
) -> bool {
    let envelope: WsRequestEnvelope = match serde_json::from_slice(payload) {
        Ok(envelope) => envelope,
        Err(err) => {
            send_outbound(
                out_tx,
                OutboundMessage::Frame(serialize_response(WsResponseEnvelope::error(
                    None,
                    WsErrorObject {
                        error_type: error_types::JSON_PARSE.to_string(),
                        message: format!("invalid JSON envelope: {err}"),
                        details: None,
                    },
                ))),
            );
            send_outbound(
                out_tx,
                OutboundMessage::Close {
                    code: close_codes::PROTOCOL_ERROR,
                    reason: "JSON parse error".to_string(),
                },
            );
            return false;
        }
    };

    if envelope.id.is_empty() {
        send_error(
            out_tx,
            None,
            error_types::INVALID_REQUEST,
            "request id is required",
        );
        return true;
    }

    if envelope.cancel_stream.unwrap_or(false) {
        let id = envelope.id.clone();
        let streams = streams.clone();
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            let canceled = {
                let mut map = streams.lock().await;
                map.remove(&id)
            };
            if let Some(tx) = canceled {
                let _ = tx.send(());
            }
            send_outbound(
                &out_tx,
                OutboundMessage::Frame(serialize_response(
                    WsResponseEnvelope::stream_end(id.clone()),
                )),
            );
        });
        return true;
    }

    let Some(method) = envelope.method.clone() else {
        send_error(
            out_tx,
            Some(envelope.id),
            error_types::INVALID_REQUEST,
            "method is required",
        );
        return true;
    };

    if !methods::is_known(&method) {
        send_error(
            out_tx,
            Some(envelope.id),
            error_types::METHOD_NOT_FOUND,
            &format!("method not found: {method}"),
        );
        return true;
    }

    let combined_params = combine_service_params(connection_params, &envelope);
    let request_id = envelope.id.clone();
    let raw_params = envelope.params.clone().unwrap_or(Value::Null);

    let handler = handler.clone();
    let out_tx_task = out_tx.clone();
    let streams = streams.clone();

    tokio::spawn(async move {
        if methods::is_streaming(&method) {
            run_streaming_request(
                method,
                request_id,
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
                request_id,
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

fn combine_service_params(
    connection_params: &ServiceParams,
    envelope: &WsRequestEnvelope,
) -> ServiceParams {
    let mut combined = connection_params.clone();
    if let Some(per_request) = envelope.service_params.as_ref() {
        for (key, values) in service_params_from_envelope(per_request) {
            combined.insert(key, values);
        }
    }
    combined
}

async fn run_unary_request<H: RequestHandler>(
    method: String,
    id: String,
    raw_params: Value,
    params: ServiceParams,
    handler: Arc<H>,
    out_tx: mpsc::UnboundedSender<OutboundMessage>,
) {
    let result = dispatch_unary(&method, &handler, &params, raw_params).await;
    match result {
        Ok(value) => {
            send_outbound(
                &out_tx,
                OutboundMessage::Frame(serialize_response(WsResponseEnvelope::result(
                    id, value,
                ))),
            );
        }
        Err(err) => {
            let error_obj = a2a_error_to_ws_error(&err);
            send_outbound(
                &out_tx,
                OutboundMessage::Frame(serialize_response(WsResponseEnvelope::error(
                    Some(id),
                    error_obj,
                ))),
            );
            if let Some(code) = close_code_for_fatal(&err) {
                send_outbound(
                    &out_tx,
                    OutboundMessage::Close {
                        code,
                        reason: err.message,
                    },
                );
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
            let req: CreateTaskPushNotificationConfigRequest = parse_params(raw_params)?;
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
    id: String,
    raw_params: Value,
    params: ServiceParams,
    handler: Arc<H>,
    streams: StreamRegistry,
    out_tx: mpsc::UnboundedSender<OutboundMessage>,
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
            send_outbound(
                &out_tx,
                OutboundMessage::Frame(serialize_response(WsResponseEnvelope::error(
                    Some(id),
                    a2a_error_to_ws_error(&err),
                ))),
            );
            return;
        }
    };

    let (cancel_tx, mut cancel_rx) = oneshot::channel::<()>();
    {
        let mut map = streams.lock().await;
        map.insert(id.clone(), cancel_tx);
    }

    let mut errored = false;
    loop {
        tokio::select! {
            biased;

            _ = &mut cancel_rx => {
                // Cancellation: stop sending events and emit streamEnd. The
                // streamEnd response itself is emitted by the cancel handler.
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
                                    WsResponseEnvelope::event(id.clone(), value),
                                )),
                            );
                        }
                        Err(err) => {
                            send_outbound(
                                &out_tx,
                                OutboundMessage::Frame(serialize_response(
                                    WsResponseEnvelope::error(
                                        Some(id.clone()),
                                        a2a_error_to_ws_error(&A2AError::internal(format!(
                                            "failed to serialize event: {err}"
                                        ))),
                                    ),
                                )),
                            );
                            errored = true;
                            break;
                        }
                    },
                    Err(err) => {
                        send_outbound(
                            &out_tx,
                            OutboundMessage::Frame(serialize_response(
                                WsResponseEnvelope::error(
                                    Some(id.clone()),
                                    a2a_error_to_ws_error(&err),
                                ),
                            )),
                        );
                        errored = true;
                        break;
                    }
                }
            }
        }
    }

    {
        let mut map = streams.lock().await;
        map.remove(&id);
    }

    if !errored {
        send_outbound(
            &out_tx,
            OutboundMessage::Frame(serialize_response(WsResponseEnvelope::stream_end(id))),
        );
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
            WsErrorObject {
                error_type: error_types::INTERNAL.to_string(),
                message: format!("failed to serialize response: {err}"),
                details: None,
            },
        );
        serde_json::to_string(&fallback).unwrap_or_else(|_| "{\"error\":{}}".to_string())
    })
}

fn send_outbound(out_tx: &mpsc::UnboundedSender<OutboundMessage>, message: OutboundMessage) {
    if out_tx.send(message).is_err() {
        tracing::debug!("outbound channel closed; dropping message");
    }
}

fn send_error(
    out_tx: &mpsc::UnboundedSender<OutboundMessage>,
    id: Option<String>,
    error_type: &str,
    message: &str,
) {
    let envelope = WsResponseEnvelope::error(
        id,
        WsErrorObject {
            error_type: error_type.to_string(),
            message: message.to_string(),
            details: None,
        },
    );
    send_outbound(
        out_tx,
        OutboundMessage::Frame(serialize_response(envelope)),
    );
}

// ---------------------------------------------------------------------------
// Construction helper: trait-object adapter (used by tests).
// ---------------------------------------------------------------------------

/// Helper alias that downcasts a `Bytes` slice to a `&str` without requiring
/// the caller to import the type. Currently unused; reserved for future
/// payload helpers.
#[allow(dead_code)]
pub(crate) fn bytes_as_str(bytes: &Bytes) -> Option<&str> {
    std::str::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a_server::handler::DefaultRequestHandler;
    use a2a_server::task_store::InMemoryTaskStore;
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

    #[test]
    fn websocket_router_constructs_with_request_handler() {
        let _router = websocket_router(make_handler());
    }

    #[test]
    fn websocket_state_is_cloneable() {
        let state = WebSocketState {
            handler: make_handler(),
        };
        let cloned = state.clone();
        assert!(Arc::ptr_eq(&state.handler, &cloned.handler));
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
    fn subprotocol_is_negotiated_accepts_csv_with_other_protocols() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("foo, a2a.v1, bar"),
        );
        assert!(subprotocol_is_negotiated(&headers));
    }

    #[test]
    fn subprotocol_is_negotiated_is_case_insensitive() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("A2A.V1"),
        );
        assert!(subprotocol_is_negotiated(&headers));
    }

    #[test]
    fn subprotocol_is_negotiated_rejects_missing_protocol() {
        let headers = HeaderMap::new();
        assert!(!subprotocol_is_negotiated(&headers));

        let mut headers = HeaderMap::new();
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("foo, bar"),
        );
        assert!(!subprotocol_is_negotiated(&headers));
    }

    #[test]
    fn capture_connection_params_lowercases_keys_and_filters_internal_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("A2A-Version", HeaderValue::from_static("1.0"));
        headers.insert("Authorization", HeaderValue::from_static("Bearer t"));
        headers.insert(header::HOST, HeaderValue::from_static("agent.example.com"));
        headers.insert(
            header::SEC_WEBSOCKET_PROTOCOL,
            HeaderValue::from_static("a2a.v1"),
        );

        let params = capture_connection_params(&headers);
        assert_eq!(params.get("a2a-version").unwrap(), &vec!["1.0".to_string()]);
        assert_eq!(
            params.get("authorization").unwrap(),
            &vec!["Bearer t".to_string()]
        );
        assert!(!params.contains_key("host"));
        assert!(!params.contains_key("sec-websocket-protocol"));
    }

    #[test]
    fn is_internal_header_lists_the_websocket_handshake_headers() {
        for name in [
            "host",
            "connection",
            "upgrade",
            "sec-websocket-key",
            "sec-websocket-version",
            "sec-websocket-protocol",
            "sec-websocket-extensions",
            "content-length",
            "transfer-encoding",
        ] {
            assert!(is_internal_header(name), "{name} should be internal");
        }
        assert!(!is_internal_header("authorization"));
        assert!(!is_internal_header("a2a-version"));
    }

    #[test]
    fn combine_service_params_per_request_overrides_connection_scope() {
        let mut connection: ServiceParams = HashMap::new();
        connection.insert(
            "a2a-version".into(),
            vec!["1.0".into()],
        );
        connection.insert(
            "x-keep".into(),
            vec!["preserve".into()],
        );

        let envelope = WsRequestEnvelope {
            id: "req".into(),
            method: Some(methods::SEND_MESSAGE.into()),
            params: None,
            service_params: Some(HashMap::from([
                ("a2a-version".into(), "1.5".into()),
                ("x-extra".into(), "added".into()),
            ])),
            cancel_stream: None,
        };

        let combined = combine_service_params(&connection, &envelope);
        assert_eq!(
            combined.get("a2a-version").unwrap(),
            &vec!["1.5".to_string()]
        );
        assert_eq!(combined.get("x-keep").unwrap(), &vec!["preserve".to_string()]);
        assert_eq!(combined.get("x-extra").unwrap(), &vec!["added".to_string()]);
    }

    #[test]
    fn serialize_response_emits_compact_json() {
        let json = serialize_response(WsResponseEnvelope::stream_end("req-1"));
        let value: Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["id"], "req-1");
        assert_eq!(value["streamEnd"], true);
    }

    #[test]
    fn parse_params_returns_invalid_params_error_on_bad_payload() {
        let value = serde_json::json!({ "bogus": true });
        let err: A2AError = parse_params::<SendMessageRequest>(value).unwrap_err();
        assert_eq!(err.code, error_code::INVALID_PARAMS);
    }
}
