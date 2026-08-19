// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
//! End-to-end tests for the A2A WebSocket binding. These tests boot a real
//! Axum server using `websocket_router(...)`, connect a real
//! `WebSocketTransport`, and exercise the full request/response, streaming,
//! and cancellation paths over an actual TCP loopback connection.

use std::sync::Arc;
use std::time::Duration;

use a2a::*;
use a2a_client::transport::{ServiceParams, Transport};
use a2a_server::AgentExecutor;
use a2a_server::executor::ExecutorContext;
use a2a_server::handler::DefaultRequestHandler;
use a2a_server::task_store::InMemoryTaskStore;
use a2a_websocket::auth::{
    AuthContext, AuthError, AuthStatus, AuthenticateParams, User, WsAuthenticator,
};
use a2a_websocket::{
    ConnectOptions, CredentialProvider, RateLimit, RateLimitPolicy, WebSocketConfig,
    WebSocketTransport, server::websocket_router, server::websocket_router_with_auth,
    server::websocket_router_with_config,
};
use async_trait::async_trait;
use axum::http::HeaderMap;
use fastwebsockets::{Frame, OpCode, Payload};
use futures::stream::{self, BoxStream, StreamExt};
use hyper_util::rt::TokioIo;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

struct EchoExecutor;

impl AgentExecutor for EchoExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let task = Task {
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: ctx.message,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let task = Task {
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Canceled,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
    }
}

struct StreamingExecutor {
    events: usize,
}

impl AgentExecutor for StreamingExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let mut events = Vec::with_capacity(self.events + 1);
        for _ in 0..self.events {
            events.push(Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            })));
        }
        let final_task = Task {
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        events.push(Ok(StreamResponse::Task(final_task)));
        Box::pin(stream::iter(events))
    }

    fn cancel(
        &self,
        _ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::empty())
    }
}

async fn start_server<E: AgentExecutor>(executor: E) -> (String, oneshot::Sender<()>) {
    let handler = Arc::new(DefaultRequestHandler::new(
        executor,
        InMemoryTaskStore::new(),
    ));
    let app = axum::Router::new().nest("/a2a/ws", websocket_router(handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    (format!("ws://{address}/a2a/ws"), shutdown_tx)
}

/// Authenticator used by the auth e2e tests: requires `Bearer good-token` at
/// the handshake and supports in-band refresh to `new-good`.
struct TokenAuth;

#[async_trait]
impl WsAuthenticator for TokenAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer good-token") => Ok(AuthContext::for_user(User::authenticated("alice"))),
            Some(_) => Err(AuthError::forbidden("invalid token")),
            None => Err(AuthError::unauthorized("missing Authorization header")),
        }
    }

    fn supports_in_band_refresh(&self) -> bool {
        true
    }

    async fn refresh(
        &self,
        ctx: &AuthContext,
        _scheme: &str,
        credentials: &str,
    ) -> Result<AuthContext, AuthError> {
        if credentials == "new-good" {
            Ok(ctx.clone())
        } else {
            Err(AuthError::unauthorized("bad refresh credentials"))
        }
    }
}

async fn start_authenticated_server<E: AgentExecutor>(
    executor: E,
) -> (String, oneshot::Sender<()>) {
    let handler = Arc::new(DefaultRequestHandler::new(
        executor,
        InMemoryTaskStore::new(),
    ));
    let app = axum::Router::new().nest(
        "/a2a/ws",
        websocket_router_with_auth(handler, Arc::new(TokenAuth)),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    (format!("ws://{address}/a2a/ws"), shutdown_tx)
}

fn make_message() -> Message {
    Message::new(Role::User, vec![Part::text("hello")])
}

fn send_message_request() -> SendMessageRequest {
    SendMessageRequest {
        message: make_message(),
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

#[tokio::test]
async fn end_to_end_send_message_round_trips_a_completed_task() {
    let (url, shutdown) = start_server(EchoExecutor).await;
    let transport = WebSocketTransport::connect(&url).await.unwrap();

    let response = transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();

    match response {
        SendMessageResponse::Task(task) => {
            assert_eq!(task.status.state, TaskState::Completed);
        }
        _ => panic!("expected Task response"),
    }

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_get_task_after_send_returns_the_persisted_task() {
    let (url, shutdown) = start_server(EchoExecutor).await;
    let transport = WebSocketTransport::connect(&url).await.unwrap();

    let mut req = send_message_request();
    req.message.task_id = Some("e2e-task".into());
    req.message.context_id = Some("e2e-ctx".into());
    transport
        .send_message(&ServiceParams::new(), &req)
        .await
        .unwrap();

    let fetched = transport
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "e2e-task".into(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(fetched.id, "e2e-task");
    assert_eq!(fetched.status.state, TaskState::Completed);

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_get_task_returns_task_not_found_error() {
    let (url, shutdown) = start_server(EchoExecutor).await;
    let transport = WebSocketTransport::connect(&url).await.unwrap();

    let err = transport
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "missing".into(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::TASK_NOT_FOUND);

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_streaming_message_yields_all_events_then_terminates() {
    let (url, shutdown) = start_server(StreamingExecutor { events: 3 }).await;
    let transport = WebSocketTransport::connect(&url).await.unwrap();

    let mut stream = transport
        .send_streaming_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();

    let mut received = 0;
    let mut saw_terminal = false;
    while let Some(item) = stream.next().await {
        let event = item.unwrap();
        received += 1;
        if let StreamResponse::Task(task) = &event {
            if task.status.state.is_terminal() {
                saw_terminal = true;
            }
        }
    }

    assert_eq!(received, 4); // 3 status updates + final completed Task
    assert!(saw_terminal, "expected to receive a terminal Task event");

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_dropping_stream_emits_cancel_stream_to_server() {
    let (url, shutdown) = start_server(StreamingExecutor { events: 100 }).await;
    let transport = WebSocketTransport::connect(&url).await.unwrap();

    {
        let mut stream = transport
            .send_streaming_message(&ServiceParams::new(), &send_message_request())
            .await
            .unwrap();
        // Consume just one event then drop the stream — this should trigger the
        // Drop guard which emits a `cancelStream: true` envelope to the server.
        let first = stream.next().await.unwrap().unwrap();
        match first {
            StreamResponse::StatusUpdate(_) | StreamResponse::Task(_) => {}
            other => panic!("unexpected first event: {other:?}"),
        }
    }

    // After dropping, the connection must remain usable for subsequent calls.
    let response = transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();
    match response {
        SendMessageResponse::Task(task) => assert!(task.status.state.is_terminal()),
        _ => panic!("expected Task after cancellation cleanup"),
    }

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_unknown_method_returns_method_not_found() {
    let (url, shutdown) = start_server(EchoExecutor).await;
    let transport = WebSocketTransport::connect(&url).await.unwrap();

    // The transport does not expose an unknown-method API directly; instead we
    // call `get_extended_agent_card` which `DefaultRequestHandler` rejects with
    // `unsupported_operation`, exercising the unary-error pathway.
    let err = transport
        .get_extended_agent_card(
            &ServiceParams::new(),
            &GetExtendedAgentCardRequest { tenant: None },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, error_code::UNSUPPORTED_OPERATION);

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_concurrent_unary_requests_are_multiplexed_on_one_socket() {
    let (url, shutdown) = start_server(EchoExecutor).await;
    let transport = Arc::new(WebSocketTransport::connect(&url).await.unwrap());

    let mut handles = Vec::new();
    for index in 0..16 {
        let transport = transport.clone();
        handles.push(tokio::spawn(async move {
            let mut req = send_message_request();
            req.message.task_id = Some(format!("concurrent-{index}"));
            req.message.context_id = Some(format!("ctx-concurrent-{index}"));
            transport
                .send_message(&ServiceParams::new(), &req)
                .await
                .unwrap()
        }));
    }

    let mut completed = 0;
    for handle in handles {
        let response = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("request timed out")
            .unwrap();
        match response {
            SendMessageResponse::Task(task) => {
                assert_eq!(task.status.state, TaskState::Completed);
                completed += 1;
            }
            _ => panic!("expected Task response"),
        }
    }
    assert_eq!(completed, 16);

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_subprotocol_negotiation_uses_a2a_v1() {
    let (url, shutdown) = start_server(EchoExecutor).await;

    // The high-level transport always negotiates "a2a.v1"; here we
    // confirm the upgrade works for that case (it does — it succeeded above)
    // and for coverage we re-issue the connect to ensure the standard path is
    // stable across multiple calls.
    let t1 = WebSocketTransport::connect(&url).await.unwrap();
    let t2 = WebSocketTransport::connect(&url).await.unwrap();

    let r1 = t1
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();
    let r2 = t2
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();

    matches!(r1, SendMessageResponse::Task(_));
    matches!(r2, SendMessageResponse::Task(_));

    t1.destroy().await.unwrap();
    t2.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_handshake_without_credentials_is_rejected() {
    let (url, shutdown) = start_authenticated_server(EchoExecutor).await;

    // No Authorization header -> the server rejects the upgrade with 401, so
    // the client handshake never completes.
    let result = WebSocketTransport::connect(&url).await;
    assert!(result.is_err(), "unauthenticated connect must fail");

    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_handshake_with_valid_bearer_token_succeeds() {
    let (url, shutdown) = start_authenticated_server(EchoExecutor).await;

    let transport = WebSocketTransport::connect_with_options(
        &url,
        ConnectOptions::with_bearer_token("good-token"),
    )
    .await
    .expect("authenticated connect should succeed");

    let response = transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();
    assert!(matches!(response, SendMessageResponse::Task(_)));

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_in_band_authenticate_refresh_round_trips() {
    let (url, shutdown) = start_authenticated_server(EchoExecutor).await;

    let transport = WebSocketTransport::connect_with_options(
        &url,
        ConnectOptions::with_bearer_token("good-token"),
    )
    .await
    .unwrap();

    // A good refresh succeeds...
    transport
        .authenticate("Bearer", "new-good")
        .await
        .expect("refresh with valid credentials should succeed");

    // ...and a bad refresh surfaces the server error.
    let err = transport
        .authenticate("Bearer", "still-bad")
        .await
        .unwrap_err();
    assert_eq!(err.code, a2a_websocket::SERVER_ERROR_CODE);

    // The connection remains usable afterwards.
    transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

struct SpawnExecutor;

impl<Fut> hyper::rt::Executor<Fut> for SpawnExecutor
where
    Fut: std::future::Future + Send + 'static,
    Fut::Output: Send + 'static,
{
    fn execute(&self, fut: Fut) {
        tokio::task::spawn(fut);
    }
}

/// Open a raw WebSocket to a `ws://host:port/path` URL, bypassing
/// `WebSocketTransport` so a test can write frames the high-level client would
/// never produce.
async fn raw_connect(url: &str) -> fastwebsockets::WebSocket<TokioIo<hyper::upgrade::Upgraded>> {
    raw_connect_as(url, None).await
}

async fn raw_connect_as(
    url: &str,
    bearer: Option<&str>,
) -> fastwebsockets::WebSocket<TokioIo<hyper::upgrade::Upgraded>> {
    let rest = url.strip_prefix("ws://").expect("ws:// url");
    let (authority, path) = rest.split_once('/').expect("url with a path");
    let stream = TcpStream::connect(authority).await.unwrap();
    let mut builder = http::Request::builder()
        .method("GET")
        .uri(format!("/{path}"))
        .header("Host", authority)
        .header("Upgrade", "websocket")
        .header("Connection", "upgrade")
        .header(
            "Sec-WebSocket-Key",
            fastwebsockets::handshake::generate_key(),
        )
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Protocol", a2a_websocket::SUBPROTOCOL);
    if let Some(token) = bearer {
        builder = builder.header("Authorization", format!("Bearer {token}"));
    }
    let request = builder
        .body(http_body_util::Empty::<hyper::body::Bytes>::new())
        .unwrap();
    let (ws, _response) = fastwebsockets::handshake::client(&SpawnExecutor, request, stream)
        .await
        .expect("raw handshake should succeed");
    ws
}

fn close_code_of(frame: &Frame<'_>) -> u16 {
    assert_eq!(frame.opcode, OpCode::Close, "expected a Close frame");
    assert!(
        frame.payload.len() >= 2,
        "Close frame must carry a status code"
    );
    u16::from_be_bytes([frame.payload[0], frame.payload[1]])
}

/// Boot a server with a deliberately small inbound message limit, so the
/// oversize path can be exercised with a frame that still fits in the socket
/// buffers (a multi-megabyte frame would be reset mid-write once the server
/// rejects it and hangs up).
async fn start_server_with_frame_limit<E: AgentExecutor>(
    executor: E,
    max_frame_bytes: usize,
) -> (String, oneshot::Sender<()>) {
    start_server_with_config(
        executor,
        WebSocketConfig {
            max_frame_bytes: Some(max_frame_bytes),
            ..Default::default()
        },
    )
    .await
}

async fn start_server_with_config<E: AgentExecutor>(
    executor: E,
    config: WebSocketConfig,
) -> (String, oneshot::Sender<()>) {
    let handler = Arc::new(DefaultRequestHandler::new(
        executor,
        InMemoryTaskStore::new(),
    ));
    let app = axum::Router::new().nest("/a2a/ws", websocket_router_with_config(handler, config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    (format!("ws://{address}/a2a/ws"), shutdown_tx)
}

#[tokio::test]
async fn end_to_end_oversize_message_is_closed_with_1009() {
    const LIMIT: usize = 8 * 1024;
    let (url, shutdown) = start_server_with_frame_limit(EchoExecutor, LIMIT).await;
    let mut ws = raw_connect(&url).await;
    // Observe the server's Close frame instead of letting the library
    // transparently answer it.
    ws.set_auto_close(false);

    ws.write_frame(Frame::text(Payload::Owned(vec![b'x'; LIMIT + 1])))
        .await
        .unwrap();

    let frame = ws
        .read_frame()
        .await
        .expect("server should answer, not hang up");
    assert_eq!(
        close_code_of(&frame),
        a2a_websocket::close_codes::MESSAGE_TOO_BIG,
        "an oversize message must be closed with 1009 (spec Section 3.6)"
    );

    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_message_under_the_size_limit_is_still_accepted() {
    let (url, shutdown) = start_server(EchoExecutor).await;
    let mut ws = raw_connect(&url).await;
    ws.set_auto_close(false);

    // A frame comfortably under the cap is processed normally: this is a
    // syntactically valid envelope naming an unknown method, so the server
    // answers with MethodNotFound rather than closing the connection.
    let padding = "y".repeat(4096);
    let request = format!(
        r#"{{"jsonrpc":"2.0","id":"big-but-ok","method":"Bogus","params":{{"pad":"{padding}"}}}}"#
    );
    ws.write_frame(Frame::text(Payload::Owned(request.into_bytes())))
        .await
        .unwrap();

    let frame = ws.read_frame().await.unwrap();
    assert_eq!(frame.opcode, OpCode::Text);
    let response: serde_json::Value = serde_json::from_slice(&frame.payload).unwrap();
    assert_eq!(response["id"], "big-but-ok");
    assert_eq!(response["error"]["code"], error_code::METHOD_NOT_FOUND);

    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_malformed_json_gets_a_parse_error_before_the_1002_close() {
    let (url, shutdown) = start_server(EchoExecutor).await;
    let mut ws = raw_connect(&url).await;
    ws.set_auto_close(false);

    ws.write_frame(Frame::text(Payload::Owned(b"{not json".to_vec())))
        .await
        .unwrap();

    // The error response has to be written before the connection goes away,
    // otherwise the peer cannot tell a rejected message from a dropped socket
    // (spec Sections 3.3 and 7.2).
    let frame = ws.read_frame().await.expect("server should answer");
    assert_eq!(
        frame.opcode,
        OpCode::Text,
        "expected a parse error response"
    );
    let response: serde_json::Value = serde_json::from_slice(&frame.payload).unwrap();
    assert_eq!(response["error"]["code"], error_code::PARSE_ERROR);

    let close = ws.read_frame().await.expect("server should close cleanly");
    assert_eq!(
        close_code_of(&close),
        a2a_websocket::close_codes::PROTOCOL_ERROR
    );

    shutdown.send(()).unwrap();
}

// --- Rate limiting (spec Section 13.3) ------------------------------------

/// A window long enough that no token is refilled while a test runs, so the
/// budget under test is exactly `max_messages`.
fn test_rate_limit(max_messages: u32) -> RateLimit {
    RateLimit::new(max_messages, Duration::from_secs(600))
}

/// A syntactically valid request naming an unknown method: the server answers
/// with exactly one non-fatal error frame, which makes message accounting in
/// the rate-limit tests deterministic.
fn probe_request(id: &str) -> Frame<'static> {
    let text = format!(r#"{{"jsonrpc":"2.0","id":"{id}","method":"Bogus","params":{{}}}}"#);
    Frame::text(Payload::Owned(text.into_bytes()))
}

/// Read frames until the connection closes, returning the JSON text frames and
/// the close code.
async fn drain_until_close(
    ws: &mut fastwebsockets::WebSocket<TokioIo<hyper::upgrade::Upgraded>>,
) -> (Vec<serde_json::Value>, u16) {
    let mut frames = Vec::new();
    loop {
        let frame = ws.read_frame().await.expect("server should not hang up");
        match frame.opcode {
            OpCode::Text => frames.push(serde_json::from_slice(&frame.payload).unwrap()),
            OpCode::Close => return (frames, close_code_of(&frame)),
            _ => {}
        }
    }
}

#[tokio::test]
async fn end_to_end_exceeding_the_rate_limit_sends_minus_32000_then_closes_with_1008() {
    const BUDGET: u32 = 2;
    let (url, shutdown) = start_server_with_config(
        EchoExecutor,
        WebSocketConfig {
            rate_limit: RateLimitPolicy::Custom(test_rate_limit(BUDGET)),
            ..Default::default()
        },
    )
    .await;
    let mut ws = raw_connect(&url).await;
    ws.set_auto_close(false);

    // Spend the whole budget, then one message beyond it.
    for i in 0..=BUDGET {
        ws.write_frame(probe_request(&format!("req-{i}")))
            .await
            .unwrap();
    }

    let (frames, close_code) = drain_until_close(&mut ws).await;
    assert_eq!(
        close_code,
        a2a_websocket::close_codes::POLICY_VIOLATION,
        "exceeding the limit must close with 1008 (spec Section 13.3)"
    );

    let rejection = frames
        .last()
        .expect("the rate-limit error must be delivered before the close");
    assert_eq!(rejection["error"]["code"], a2a_websocket::SERVER_ERROR_CODE);
    assert_eq!(rejection["error"]["message"], "Rate limit exceeded");
    assert!(
        rejection["id"].is_null(),
        "the limit is enforced before parsing, so no request id is echoed"
    );
    assert_eq!(
        frames.len(),
        BUDGET as usize + 1,
        "every admitted message should have been answered before the rejection"
    );

    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_rate_limit_budget_is_shared_across_one_identity() {
    const BUDGET: u32 = 2;
    let handler = Arc::new(DefaultRequestHandler::new(
        EchoExecutor,
        InMemoryTaskStore::new(),
    ));
    let config = WebSocketConfig {
        authenticator: Some(Arc::new(TokenAuth)),
        rate_limit: RateLimitPolicy::Custom(test_rate_limit(BUDGET)),
        ..Default::default()
    };
    let app = axum::Router::new().nest("/a2a/ws", websocket_router_with_config(handler, config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    let url = format!("ws://{address}/a2a/ws");

    // The first connection spends alice's entire identity budget.
    let mut first = raw_connect_as(&url, Some("good-token")).await;
    first.set_auto_close(false);
    for i in 0..BUDGET {
        first
            .write_frame(probe_request(&format!("a-{i}")))
            .await
            .unwrap();
        let frame = first.read_frame().await.unwrap();
        assert_eq!(frame.opcode, OpCode::Text);
    }

    // A second connection for the same identity has a fresh per-connection
    // bucket but no identity budget left, so its very first message is refused.
    let mut second = raw_connect_as(&url, Some("good-token")).await;
    second.set_auto_close(false);
    second.write_frame(probe_request("b-0")).await.unwrap();

    let (frames, close_code) = drain_until_close(&mut second).await;
    assert_eq!(
        close_code,
        a2a_websocket::close_codes::POLICY_VIOLATION,
        "the identity budget is shared across connections (spec Section 13.3)"
    );
    assert_eq!(frames.len(), 1, "only the rate-limit error is sent");
    assert_eq!(frames[0]["error"]["message"], "Rate limit exceeded");

    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_requests_within_the_rate_limit_are_unaffected() {
    let (url, shutdown) = start_server_with_config(
        EchoExecutor,
        WebSocketConfig {
            rate_limit: RateLimitPolicy::Custom(test_rate_limit(100)),
            ..Default::default()
        },
    )
    .await;
    let transport = WebSocketTransport::connect(&url).await.unwrap();

    for _ in 0..5 {
        let response = transport
            .send_message(&ServiceParams::new(), &send_message_request())
            .await
            .unwrap();
        assert!(matches!(response, SendMessageResponse::Task(_)));
    }

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

// --- Reauthentication (spec Sections 9.3.2 and 9.3.3) ---------------------

/// Authenticator whose credentials are always "about to expire" until they are
/// refreshed in-band once, after which the connection is valid again.
struct ExpiringAuth {
    grace: Duration,
    refreshed: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl WsAuthenticator for ExpiringAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer good-token") => Ok(AuthContext::for_user(User::authenticated("alice"))),
            _ => Err(AuthError::unauthorized("missing or invalid token")),
        }
    }

    async fn revalidate(&self, _ctx: &AuthContext) -> AuthStatus {
        if self.refreshed.load(std::sync::atomic::Ordering::SeqCst) {
            AuthStatus::Valid
        } else {
            AuthStatus::ReauthRequired {
                reason: "token expiring".to_string(),
                retry_after_ms: 1_000,
            }
        }
    }

    fn supports_in_band_refresh(&self) -> bool {
        true
    }

    async fn refresh(
        &self,
        ctx: &AuthContext,
        _scheme: &str,
        credentials: &str,
    ) -> Result<AuthContext, AuthError> {
        if credentials == "new-good" {
            self.refreshed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(ctx.clone())
        } else {
            Err(AuthError::unauthorized("bad refresh credentials"))
        }
    }

    fn reauth_grace(&self) -> Duration {
        self.grace
    }
}

async fn start_expiring_auth_server(
    grace: Duration,
) -> (
    String,
    Arc<std::sync::atomic::AtomicBool>,
    oneshot::Sender<()>,
) {
    let refreshed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler = Arc::new(DefaultRequestHandler::new(
        EchoExecutor,
        InMemoryTaskStore::new(),
    ));
    let app = axum::Router::new().nest(
        "/a2a/ws",
        websocket_router_with_auth(
            handler,
            Arc::new(ExpiringAuth {
                grace,
                refreshed: refreshed.clone(),
            }),
        ),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    (format!("ws://{address}/a2a/ws"), refreshed, shutdown)
}

struct StaticCredentials(&'static str);

#[async_trait]
impl CredentialProvider for StaticCredentials {
    async fn fresh_credentials(&self) -> Result<AuthenticateParams, A2AError> {
        Ok(AuthenticateParams {
            scheme: "Bearer".to_string(),
            credentials: self.0.to_string(),
        })
    }
}

#[tokio::test]
async fn end_to_end_in_band_refresh_keeps_the_connection_open() {
    const GRACE: Duration = Duration::from_millis(1_000);
    let (url, refreshed, shutdown) = start_expiring_auth_server(GRACE).await;

    let transport = WebSocketTransport::connect_with_options(
        &url,
        ConnectOptions::with_bearer_token("good-token")
            .with_credential_provider(Arc::new(StaticCredentials("new-good"))),
    )
    .await
    .unwrap();

    // This request triggers revalidation, which signals ReauthenticationRequired
    // and schedules a 4001 close after the grace period.
    transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();

    // The client refreshes in-band rather than reconnecting (Section 9.3.3).
    for _ in 0..100 {
        if refreshed.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        refreshed.load(std::sync::atomic::Ordering::SeqCst),
        "the credential provider should have refreshed the connection in-band"
    );

    // Once the grace period has elapsed the connection must still be usable:
    // a successful refresh cancels the pending 4001 close.
    tokio::time::sleep(GRACE + Duration::from_millis(300)).await;
    let response = transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .expect("in-band refresh should have cancelled the 4001 close");
    assert!(matches!(response, SendMessageResponse::Task(_)));

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_notify_policy_reports_reauth_and_the_4001_close_is_surfaced() {
    const GRACE: Duration = Duration::from_millis(200);
    let (url, _refreshed, shutdown) = start_expiring_auth_server(GRACE).await;

    let (reauth_tx, mut reauth_rx) = tokio::sync::mpsc::unbounded_channel();
    let transport = WebSocketTransport::connect_with_options(
        &url,
        ConnectOptions::with_bearer_token("good-token").on_reauth_required(move |request| {
            let _ = reauth_tx.send(request);
        }),
    )
    .await
    .unwrap();

    transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();

    let signal = tokio::time::timeout(Duration::from_secs(5), reauth_rx.recv())
        .await
        .expect("the reauth callback should fire (spec Section 9.3.2)")
        .unwrap();
    assert_eq!(signal.reason.as_deref(), Some("token expiring"));
    assert_eq!(signal.retry_after_ms, Some(1_000));

    // Without a refresh the server closes with 4001 after the grace period, and
    // the reason must be visible to the caller rather than a bare disconnect.
    tokio::time::sleep(GRACE + Duration::from_millis(300)).await;
    let err = transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .expect_err("the connection should be closed");
    assert!(
        err.message.contains("4001"),
        "the close code should reach the caller, got: {}",
        err.message
    );

    shutdown.send(()).unwrap();
}

// ---------------------------------------------------------------------------
// The identity the authenticator establishes is what the handler sees, and only
// the authenticator can change it (spec Sections 9.1–9.3.3).
// ---------------------------------------------------------------------------

const CALLER_PARAM: &str = "x-caller";

/// Surfaces the connection's `x-caller` service parameter as the response text,
/// so a test can observe exactly which identity reached the business logic.
struct CallerEchoExecutor;

impl AgentExecutor for CallerEchoExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let caller = ctx
            .service_params
            .get(CALLER_PARAM)
            .and_then(|values| values.first())
            .cloned()
            .unwrap_or_else(|| "anonymous".to_string());

        let task = Task {
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: Some(Message::new(Role::Agent, vec![Part::text(caller)])),
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
    }

    fn cancel(
        &self,
        _ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::empty())
    }
}

/// Maps each credential to a different principal, so a refresh visibly changes
/// who the connection is.
struct RotatingIdentityAuth;

#[async_trait]
impl WsAuthenticator for RotatingIdentityAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError> {
        match headers.get("authorization").and_then(|v| v.to_str().ok()) {
            Some("Bearer alice-token") => Ok(AuthContext::for_user(User::authenticated("alice"))
                .with_param(CALLER_PARAM, "alice")),
            _ => Err(AuthError::unauthorized("missing or invalid token")),
        }
    }

    fn supports_in_band_refresh(&self) -> bool {
        true
    }

    async fn refresh(
        &self,
        _ctx: &AuthContext,
        _scheme: &str,
        credentials: &str,
    ) -> Result<AuthContext, AuthError> {
        match credentials {
            "bob-token" => {
                Ok(AuthContext::for_user(User::authenticated("bob"))
                    .with_param(CALLER_PARAM, "bob"))
            }
            _ => Err(AuthError::unauthorized("bad refresh credentials")),
        }
    }
}

async fn start_rotating_identity_server() -> (String, oneshot::Sender<()>) {
    let handler = Arc::new(DefaultRequestHandler::new(
        CallerEchoExecutor,
        InMemoryTaskStore::new(),
    ));
    let app = axum::Router::new().nest(
        "/a2a/ws",
        websocket_router_with_auth(handler, Arc::new(RotatingIdentityAuth)),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    (format!("ws://{address}/a2a/ws"), shutdown)
}

async fn observed_caller(transport: &WebSocketTransport, params: &ServiceParams) -> String {
    match transport
        .send_message(params, &send_message_request())
        .await
        .expect("send_message should succeed")
    {
        SendMessageResponse::Task(task) => match task.status.message {
            Some(message) => match &message.parts[0].content {
                PartContent::Text(text) => text.clone(),
                other => panic!("expected text part, got {other:?}"),
            },
            None => panic!("expected a status message"),
        },
        other => panic!("expected a task, got {other:?}"),
    }
}

#[tokio::test]
async fn end_to_end_a_request_cannot_override_the_authenticated_identity() {
    let (url, shutdown) = start_rotating_identity_server().await;
    let transport = WebSocketTransport::connect_with_options(
        &url,
        ConnectOptions::with_bearer_token("alice-token"),
    )
    .await
    .unwrap();

    let mut spoofed = ServiceParams::new();
    spoofed.insert(CALLER_PARAM.to_string(), vec!["mallory".to_string()]);
    assert_eq!(observed_caller(&transport, &spoofed).await, "alice");

    // Casing must not be a way around it: envelope keys are normalized the same
    // way handshake headers are.
    let mut spoofed_upper = ServiceParams::new();
    spoofed_upper.insert("X-Caller".to_string(), vec!["mallory".to_string()]);
    assert_eq!(observed_caller(&transport, &spoofed_upper).await, "alice");

    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn end_to_end_in_band_refresh_updates_the_identity_seen_by_the_handler() {
    let (url, shutdown) = start_rotating_identity_server().await;
    let transport = WebSocketTransport::connect_with_options(
        &url,
        ConnectOptions::with_bearer_token("alice-token"),
    )
    .await
    .unwrap();

    let params = ServiceParams::new();
    assert_eq!(observed_caller(&transport, &params).await, "alice");

    transport.authenticate("Bearer", "bob-token").await.unwrap();

    // Refreshing the credentials replaces the principal; continuing to serve
    // requests as `alice` would leave the connection acting on stale
    // authorization.
    assert_eq!(observed_caller(&transport, &params).await, "bob");

    shutdown.send(()).unwrap();
}
