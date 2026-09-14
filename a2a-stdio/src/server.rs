// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Server side of the binding: the process that was spawned (2, 3.6).
//!
//! 3.6 wants many in-flight requests, but a frame is an indivisible header
//! block plus body, so concurrent writers would interleave into garbage. Hence
//! three roles: a reader that never blocks on work, N request tasks, and exactly
//! one writer draining a bounded channel.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use a2a::error_code;
use a2a::jsonrpc::{JsonRpcError, JsonRpcId, methods};
use a2a_server::{RequestHandler, ServiceParams, dispatch};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::task::JoinHandle;
use tokio_util::codec::{FramedRead, FramedWrite};
use tokio_util::sync::CancellationToken;

use crate::codec::{DEFAULT_MAX_FRAME_SIZE, DEFAULT_MAX_HEADER_SIZE, StdioCodec, StdioCodecError};
use crate::frame::{Frame, FrameHeaders};
use crate::json::{self, ClientMessage, ServerInbound, ServerMessage};
use crate::metadata::{self, SessionBinding};
use crate::session::{self, AckOutcome, ExitCode, FEATURE_STREAMING};
use crate::wire::{ControlFrame, Variant, WireError};
type Cancels = Arc<Mutex<HashMap<JsonRpcId, CancellationToken>>>;

/// 7.2: framing and session failures after the handshake are reported from the
/// JSON-RPC implementation-defined server-error range.
const BINDING_ERROR: i32 = -32000;

/// Must stay well inside the ~5 s the parent waits before `SIGTERM` (2.4).
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

pub struct ServerConfig {
    pub max_frame_size: usize, // 3.7 recommends 1 MiB
    pub max_header_size: usize,
    pub outbound_capacity: usize, // mpsc depth, e.g. 64
    pub features: Vec<String>,    // FEATURE_STREAMING, ...
    /// 9.6 `launchDigest`, if the host computed one. There is no standard
    /// environment variable for it, so it has to be supplied out of band.
    pub launch_digest: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            max_header_size: DEFAULT_MAX_HEADER_SIZE,
            outbound_capacity: 64,
            features: vec![FEATURE_STREAMING.to_string()],
            launch_digest: None,
        }
    }
}

/// The 2.1 startup context handed to a spawned agent.
pub struct Startup {
    /// From `A2A_SESSION_ID`. The handshake echoes it and the client rejects a
    /// mismatch (2.2 step 2).
    pub session_id: String,
    /// 4.1: applied to every request unless that request overrides them.
    pub session_params: ServiceParams,
}

impl Startup {
    /// 2.1 makes `A2A_SESSION_ID` REQUIRED and `A2A_SP_*` optional.
    pub fn from_env() -> Result<Self, ExitCode> {
        Ok(Startup {
            session_id: session::session_id_from_env(|k| std::env::var(k).ok())?,
            session_params: session::service_params_from_env(std::env::vars()),
        })
    }
}

/// Run the agent on `STDIN`/`STDOUT` until EOF or the session ends.
///
/// Returns the 2.4 exit code. Pass it to `std::process::exit` only after this
/// future resolves, or queued frames are dropped unflushed.
pub async fn serve<H>(handler: H, config: ServerConfig) -> ExitCode
where
    H: RequestHandler + Send + Sync + 'static,
{
    let startup = match Startup::from_env() {
        Ok(startup) => startup,
        Err(code) => {
            eprintln!("a2a-stdio: {} is missing or empty", session::ENV_SESSION_ID);
            return code;
        }
    };
    serve_on(
        tokio::io::stdin(),
        tokio::io::stdout(),
        startup,
        handler,
        config,
    )
    .await
}

/// Run the agent over pipes the caller supplies.
///
/// 2 permits serving handles obtained by other means than a spawn, so the
/// standard streams are a default rather than a requirement.
pub async fn serve_on<R, W, H>(
    input: R,
    output: W,
    startup: Startup,
    handler: H,
    config: ServerConfig,
) -> ExitCode
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin + Send + 'static,
    H: RequestHandler + Send + Sync + 'static,
{
    let Startup {
        session_id,
        session_params,
    } = startup;

    let mut reader = FramedRead::new(
        input,
        StdioCodec::new(config.max_frame_size, config.max_header_size),
    );
    let mut writer = FramedWrite::new(
        output,
        StdioCodec::new(config.max_frame_size, config.max_header_size),
    );

    // ---- handshake (2.2) -------------------------------------------------
    // Server speaks first, straight onto the sink: no concurrency exists yet.
    let offered = session::build_handshake(
        session_id,
        vec![Variant::Json],
        config.features,
        Some(std::process::id()),
    );
    let hello = match encode_control(&ControlFrame::Handshake(offered.clone())) {
        Ok(frame) => frame,
        Err(e) => {
            eprintln!("a2a-stdio: cannot encode handshake: {e}");
            return ExitCode::GenericError;
        }
    };
    if let Err(e) = writer.send(hello).await {
        eprintln!("a2a-stdio: cannot write handshake: {e}");
        return ExitCode::GenericError;
    }

    // 2.2 step 3: exactly one frame, and it must be a `handshakeAck`.
    let ack = match reader.next().await {
        Some(Ok(frame)) => match json::parse_from_client(&frame.body) {
            Ok(ServerInbound::Control(ControlFrame::HandshakeAck(ack))) => ack,
            Ok(_) => {
                eprintln!("a2a-stdio: expected handshakeAck before any other frame");
                return ExitCode::ProtocolError;
            }
            Err(e) => {
                eprintln!("a2a-stdio: malformed handshakeAck: {e}");
                return ExitCode::ProtocolError;
            }
        },
        Some(Err(e)) => {
            eprintln!("a2a-stdio: framing error during handshake: {e}");
            return ExitCode::ProtocolError;
        }
        // Parent left before acking: nothing to serve, nothing wrong.
        None => return ExitCode::Ok,
    };

    let variant = match session::accept_ack(&offered, &ack) {
        Ok(AckOutcome::Accepted(v)) => v,
        // 2.2 step 4: a refusal is a clean shutdown.
        Ok(AckOutcome::Declined) => return ExitCode::Ok,
        Err(e) => {
            eprintln!("a2a-stdio: {e}");
            return e.exit_code();
        }
    };
    debug_assert_eq!(
        variant,
        Variant::Json,
        "only stdio-json is advertised, so nothing else can be selected"
    );

    // ---- serving ----------------------------------------------------------
    let binding = Arc::new(
        SessionBinding {
            session_id: offered.session_id,
            pid: offered.pid,
            variant,
            launch_digest: config.launch_digest,
        }
        .to_value(),
    );

    let (tx, rx) = mpsc::channel::<Frame>(config.outbound_capacity);
    let writer_task = tokio::spawn(write_loop(writer, rx));
    let cancels = Cancels::default();

    let exit = read_loop(
        reader,
        tx.clone(),
        Arc::new(handler),
        session_params,
        binding,
        cancels.clone(),
    )
    .await;

    drain_and_flush(&cancels, tx, writer_task).await;
    exit
}

/// The 2.4 shutdown sequence: abort in-flight work, then flush what is queued.
///
/// `write_loop` ends only once every sender drops, and each in-flight task holds
/// a clone, so an open stream would keep it alive forever. Cancelling is the
/// "abort" half of 2.4; the timeout covers unary handlers, which hold a sender
/// but observe no token. Returns whether the writer finished in time.
async fn drain_and_flush(cancels: &Cancels, tx: Sender<Frame>, writer: JoinHandle<()>) -> bool {
    for (_, token) in lock(cancels).drain() {
        token.cancel();
    }
    drop(tx);
    if tokio::time::timeout(SHUTDOWN_GRACE, writer).await.is_err() {
        eprintln!("a2a-stdio: in-flight work outlived the {SHUTDOWN_GRACE:?} drain window");
        return false;
    }
    true
}

/// The only task permitted to touch `STDOUT`.
async fn write_loop<W>(mut sink: FramedWrite<W, StdioCodec>, mut rx: Receiver<Frame>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        if let Err(e) = sink.send(frame).await {
            // Either a closed pipe or an encoder refusal (3.7 oversize). Both
            // are unreportable in-band, so 1's diagnostic channel separates them.
            eprintln!("a2a-stdio: dropping outbound frame: {e}");
            break;
        }
    }
    let _ = sink.flush().await;
}

async fn read_loop<R, H>(
    mut reader: FramedRead<R, StdioCodec>,
    tx: Sender<Frame>,
    handler: Arc<H>,
    session_params: ServiceParams,
    binding: Arc<Value>,
    cancels: Cancels,
) -> ExitCode
where
    R: AsyncRead + Unpin,
    H: RequestHandler + Send + Sync + 'static,
{
    while let Some(item) = reader.next().await {
        let frame = match item {
            Ok(frame) => frame,
            // 3.7: answer an oversize frame with -32000, then exit 2.
            Err(StdioCodecError::FrameTooLarge { len, max }) => {
                eprintln!("a2a-stdio: frame of {len} bytes exceeds {max}");
                send_error(&tx, JsonRpcId::Null, BINDING_ERROR, "Message too large").await;
                return ExitCode::ProtocolError;
            }
            Err(e) => {
                eprintln!("a2a-stdio: framing error: {e}");
                return ExitCode::ProtocolError;
            }
        };

        match json::parse_from_client(&frame.body) {
            // 7.2: fatal, because the id is unknowable.
            Err(WireError::Json(e)) => {
                eprintln!("a2a-stdio: {e}");
                send_error(&tx, JsonRpcId::Null, error_code::PARSE_ERROR, "Parse error").await;
                return ExitCode::ProtocolError;
            }
            // Not fatal. The shape was rejected but 3.4 still wants the real id
            // when it is readable, so callers can match the answer to the frame.
            Err(e) => {
                send_error(
                    &tx,
                    json::peek_id(&frame.body),
                    error_code::INVALID_REQUEST,
                    &e.to_string(),
                )
                .await;
            }

            // 2.3: a heartbeat carries nothing beyond liveness.
            Ok(ServerInbound::Control(ControlFrame::Heartbeat(_))) => {}

            // 2.2: the handshake happens once, before this loop is entered.
            Ok(ServerInbound::Control(_)) => {
                eprintln!("a2a-stdio: unexpected handshake frame mid-session");
                return ExitCode::ProtocolError;
            }

            // 8.5 step 3 forbids cancelling the underlying task, so this stops
            // the stream without ever reaching the handler.
            Ok(ServerInbound::Message(ClientMessage::CancelStream { id })) => {
                let token = lock(&cancels).remove(&id);
                if let Some(token) = token {
                    token.cancel();
                }
            }

            Ok(ServerInbound::Message(ClientMessage::Request(req))) => {
                // 6.9: binding-local method, absent from the A2A inventory.
                if req.method == json::SYSTEM_SHUTDOWN {
                    send(
                        &tx,
                        ServerMessage::Result {
                            id: req.id,
                            result: serde_json::json!({}),
                        },
                    )
                    .await;
                    return ExitCode::Ok;
                }

                let params = merge_service_params(&session_params, req.service_params.as_ref());
                let raw_params = req.params.unwrap_or(Value::Null);
                if methods::is_streaming(&req.method) {
                    // 3.6: ids are unique. Reusing an open one would overwrite
                    // its token and leave the first stream uncancellable.
                    if lock(&cancels).contains_key(&req.id) {
                        send_error(
                            &tx,
                            req.id,
                            error_code::INVALID_REQUEST,
                            "Duplicate request id: a stream with this id is still open",
                        )
                        .await;
                        continue;
                    }
                    spawn_stream(
                        handler.clone(),
                        tx.clone(),
                        cancels.clone(),
                        req.id,
                        req.method,
                        params,
                        raw_params,
                        binding.clone(),
                    );
                } else {
                    spawn_unary(
                        handler.clone(),
                        tx.clone(),
                        req.id,
                        req.method,
                        params,
                        raw_params,
                        binding.clone(),
                    );
                }
            }
        }
    }

    // The stream ended: `STDIN` reached EOF (2.4).
    ExitCode::Ok
}

#[allow(clippy::too_many_arguments)]
fn spawn_unary<H>(
    handler: Arc<H>,
    tx: Sender<Frame>,
    id: JsonRpcId,
    method: String,
    params: ServiceParams,
    raw_params: Value,
    binding: Arc<Value>,
) where
    H: RequestHandler + Send + Sync + 'static,
{
    tokio::spawn(async move {
        let msg = match dispatch::dispatch_unary(&*handler, &params, &method, raw_params).await {
            Ok(mut result) => {
                metadata::stamp(&mut result, &binding);
                ServerMessage::Result { id, result }
            }
            Err(e) => ServerMessage::Error {
                id,
                error: e.into(),
            },
        };
        send(&tx, msg).await;
    });
}

#[allow(clippy::too_many_arguments)]
fn spawn_stream<H>(
    handler: Arc<H>,
    tx: Sender<Frame>,
    cancels: Cancels,
    id: JsonRpcId,
    method: String,
    params: ServiceParams,
    raw_params: Value,
    binding: Arc<Value>,
) where
    H: RequestHandler + Send + Sync + 'static,
{
    let token = CancellationToken::new();
    lock(&cancels).insert(id.clone(), token.clone());

    tokio::spawn(async move {
        match dispatch::dispatch_streaming(&*handler, &params, &method, raw_params).await {
            // 7.3 and 8.3: an error terminates the stream and no `streamEnd` follows.
            Err(e) => {
                send(
                    &tx,
                    ServerMessage::Error {
                        id: id.clone(),
                        error: e.into(),
                    },
                )
                .await;
            }
            Ok(mut stream) => loop {
                tokio::select! {
                    // 8.5 step 2 still wants a proper termination, hence a token
                    // rather than aborting the task.
                    _ = token.cancelled() => {
                        send(&tx, ServerMessage::StreamEnd { id: id.clone() }).await;
                        break;
                    }
                    item = stream.next() => match item {
                        Some(Ok(mut result)) => {
                            metadata::stamp(&mut result, &binding);
                            send(&tx, ServerMessage::Result { id: id.clone(), result }).await;
                        }
                        Some(Err(e)) => {
                            send(&tx, ServerMessage::Error { id: id.clone(), error: e.into() }).await;
                            break;
                        }
                        None => {
                            send(&tx, ServerMessage::StreamEnd { id: id.clone() }).await;
                            break;
                        }
                    }
                }
            },
        }
        lock(&cancels).remove(&id);
    });
}

/// 4.1: session-scoped parameters apply unless overridden per-request.
fn merge_service_params(
    session: &ServiceParams,
    per_request: Option<&HashMap<String, String>>,
) -> ServiceParams {
    let mut merged = session.clone();
    if let Some(wire) = per_request {
        merged.extend(json::wire_to_service_params(wire));
    }
    merged
}

async fn send(tx: &Sender<Frame>, msg: ServerMessage) {
    match encode_message(&msg) {
        // A closed channel means the writer is gone; the reader will see EOF.
        Ok(frame) => {
            let _ = tx.send(frame).await;
        }
        Err(e) => eprintln!("a2a-stdio: cannot encode outbound frame: {e}"),
    }
}

async fn send_error(tx: &Sender<Frame>, id: JsonRpcId, code: i32, message: &str) {
    let error = JsonRpcError {
        code,
        message: message.to_string(),
        data: None,
    };
    send(tx, ServerMessage::Error { id, error }).await;
}

/// No invariant here survives a panic badly, so poisoning is recovered from.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// No optional headers: 3.1 defaults `Content-Type` for `stdio-json`.
fn json_frame(body: Vec<u8>) -> Frame {
    Frame {
        headers: FrameHeaders::default(),
        body: Bytes::from(body),
    }
}

fn encode_control(frame: &ControlFrame) -> Result<Frame, serde_json::Error> {
    Ok(json_frame(serde_json::to_vec(frame)?))
}

fn encode_message(msg: &ServerMessage) -> Result<Frame, serde_json::Error> {
    Ok(json_frame(msg.to_vec()?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{A2AError, error_code};
    use futures::stream::{self, BoxStream};
    use serde_json::json;

    // Only two methods are live: `DeleteTaskPushNotificationConfig` is the one
    // unary call returning `null`, and `SubscribeToTask` needs no constructed
    // items. The rest would drag fixtures into tests about framing.

    #[derive(Default, Clone, Copy, PartialEq)]
    enum StreamKind {
        #[default]
        Empty,
        /// Only cancellation can close this one.
        Pending,
        /// Fails before the first item (7.3).
        Rejected,
        /// One `Task` then end, for the 9.6 stamping check.
        OneTask,
    }

    fn sample_task() -> a2a::Task {
        a2a::Task {
            id: "t-1".into(),
            context_id: "c-1".into(),
            status: a2a::TaskStatus {
                state: a2a::TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    #[derive(Default)]
    struct MockHandler {
        stream: StreamKind,
        seen: Mutex<Vec<ServiceParams>>,
    }

    impl MockHandler {
        fn with_stream(stream: StreamKind) -> Self {
            MockHandler {
                stream,
                seen: Mutex::new(Vec::new()),
            }
        }

        fn unused(method: &str) -> A2AError {
            A2AError::internal(format!("{method} is not exercised by these tests"))
        }
    }

    #[async_trait::async_trait]
    impl RequestHandler for MockHandler {
        async fn delete_push_config(
            &self,
            params: &ServiceParams,
            _req: a2a::DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            lock(&self.seen).push(params.clone());
            Ok(())
        }

        async fn subscribe_to_task(
            &self,
            params: &ServiceParams,
            _req: a2a::SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<a2a::StreamResponse, A2AError>>, A2AError> {
            lock(&self.seen).push(params.clone());
            match self.stream {
                StreamKind::Empty => Ok(Box::pin(stream::empty())),
                StreamKind::Pending => Ok(Box::pin(stream::pending())),
                StreamKind::Rejected => Err(A2AError::internal("stream refused")),
                StreamKind::OneTask => Ok(Box::pin(stream::once(async {
                    Ok(a2a::StreamResponse::Task(sample_task()))
                }))),
            }
        }

        async fn send_message(
            &self,
            _params: &ServiceParams,
            _req: a2a::SendMessageRequest,
        ) -> Result<a2a::SendMessageResponse, A2AError> {
            Err(Self::unused("send_message"))
        }

        async fn send_streaming_message(
            &self,
            _params: &ServiceParams,
            _req: a2a::SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<a2a::StreamResponse, A2AError>>, A2AError> {
            Err(Self::unused("send_streaming_message"))
        }

        async fn get_task(
            &self,
            params: &ServiceParams,
            _req: a2a::GetTaskRequest,
        ) -> Result<a2a::Task, A2AError> {
            lock(&self.seen).push(params.clone());
            Ok(sample_task())
        }

        async fn list_tasks(
            &self,
            _params: &ServiceParams,
            _req: a2a::ListTasksRequest,
        ) -> Result<a2a::ListTasksResponse, A2AError> {
            Err(Self::unused("list_tasks"))
        }

        async fn cancel_task(
            &self,
            _params: &ServiceParams,
            _req: a2a::CancelTaskRequest,
        ) -> Result<a2a::Task, A2AError> {
            Err(Self::unused("cancel_task"))
        }

        async fn create_push_config(
            &self,
            _params: &ServiceParams,
            _req: a2a::TaskPushNotificationConfig,
        ) -> Result<a2a::TaskPushNotificationConfig, A2AError> {
            Err(Self::unused("create_push_config"))
        }

        async fn get_push_config(
            &self,
            _params: &ServiceParams,
            _req: a2a::GetTaskPushNotificationConfigRequest,
        ) -> Result<a2a::TaskPushNotificationConfig, A2AError> {
            Err(Self::unused("get_push_config"))
        }

        async fn list_push_configs(
            &self,
            _params: &ServiceParams,
            _req: a2a::ListTaskPushNotificationConfigsRequest,
        ) -> Result<a2a::ListTaskPushNotificationConfigsResponse, A2AError> {
            Err(Self::unused("list_push_configs"))
        }

        async fn get_extended_agent_card(
            &self,
            _params: &ServiceParams,
            _req: a2a::GetExtendedAgentCardRequest,
        ) -> Result<a2a::AgentCard, A2AError> {
            Err(Self::unused("get_extended_agent_card"))
        }
    }

    fn test_binding() -> Value {
        SessionBinding {
            session_id: "s-1".into(),
            pid: Some(42137),
            variant: Variant::Json,
            launch_digest: None,
        }
        .to_value()
    }

    /// Framed by hand, so an encoder bug cannot make these tests agree with
    /// themselves.
    fn framed(body: &[u8]) -> Vec<u8> {
        let mut out = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn frames(bodies: &[Value]) -> Vec<u8> {
        let mut out = Vec::new();
        for body in bodies {
            out.extend_from_slice(&framed(&serde_json::to_vec(body).unwrap()));
        }
        out
    }

    /// Drive `read_loop` to EOF and collect what reached the writer channel.
    /// The drain needs its timeout because an unending stream holds a sender.
    async fn run(
        handler: MockHandler,
        session_params: ServiceParams,
        input: Vec<u8>,
    ) -> (ExitCode, Arc<MockHandler>, Vec<ServerMessage>) {
        let handler = Arc::new(handler);
        let (tx, mut rx) = mpsc::channel::<Frame>(64);
        let reader = FramedRead::new(
            input.as_slice(),
            StdioCodec::new(DEFAULT_MAX_FRAME_SIZE, DEFAULT_MAX_HEADER_SIZE),
        );

        let exit = read_loop(
            reader,
            tx,
            handler.clone(),
            session_params,
            Arc::new(test_binding()),
            Cancels::default(),
        )
        .await;

        let mut out = Vec::new();
        while let Ok(Some(frame)) =
            tokio::time::timeout(Duration::from_millis(250), rx.recv()).await
        {
            match json::parse_from_server(&frame.body).expect("server emitted an unreadable frame")
            {
                crate::json::ClientInbound::Message(m) => out.push(m),
                other => panic!("unexpected control frame on the response path: {other:?}"),
            }
        }
        (exit, handler, out)
    }

    async fn run_plain(input: Vec<u8>) -> (ExitCode, Vec<ServerMessage>) {
        let (exit, _, out) = run(MockHandler::default(), ServiceParams::default(), input).await;
        (exit, out)
    }

    fn request(id: Value, method: &str) -> Value {
        json!({"jsonrpc": "2.0", "id": id, "method": method, "params": {}})
    }

    fn error_of(msg: &ServerMessage) -> &JsonRpcError {
        match msg {
            ServerMessage::Error { error, .. } => error,
            other => panic!("expected an error, got {other:?}"),
        }
    }

    // --- unary dispatch ---

    #[tokio::test]
    async fn unary_request_is_answered_once_and_eof_is_clean() {
        let (exit, out) = run_plain(frames(&[request(
            json!("d-1"),
            methods::DELETE_PUSH_CONFIG,
        )]))
        .await;

        assert_eq!(exit, ExitCode::Ok, "EOF on STDIN is 2.4 exit 0");
        assert_eq!(out.len(), 1, "got {out:?}");
        assert_eq!(
            out[0],
            ServerMessage::Result {
                id: JsonRpcId::String("d-1".into()),
                result: Value::Null,
            }
        );
    }

    #[tokio::test]
    async fn unknown_method_is_answered_without_ending_the_session() {
        let (exit, out) = run_plain(frames(&[
            request(json!(1), "NoSuchMethod"),
            request(json!(2), methods::DELETE_PUSH_CONFIG),
        ]))
        .await;

        assert_eq!(exit, ExitCode::Ok);
        assert_eq!(out.len(), 2, "the second request must still be served");
        assert_eq!(error_of(&out[0]).code, error_code::METHOD_NOT_FOUND);
    }

    #[tokio::test]
    async fn requests_are_not_serialized_behind_each_other() {
        // Two ids in flight at once: the reader must not block on the first.
        let input = frames(&[
            request(json!("a"), methods::DELETE_PUSH_CONFIG),
            request(json!("b"), methods::DELETE_PUSH_CONFIG),
            request(json!("c"), methods::DELETE_PUSH_CONFIG),
        ]);
        let (_, _, out) = run(MockHandler::default(), ServiceParams::default(), input).await;

        let mut ids: Vec<String> = out
            .iter()
            .map(|m| match m {
                ServerMessage::Result {
                    id: JsonRpcId::String(s),
                    ..
                } => s.clone(),
                other => panic!("expected string-id results, got {other:?}"),
            })
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    // --- malformed input (7.2) ---

    #[tokio::test]
    async fn malformed_json_is_fatal_and_answered_with_a_null_id() {
        let (exit, out) = run_plain(framed(b"{not json")).await;

        assert_eq!(exit, ExitCode::ProtocolError, "7.2 makes this exit 2");
        assert_eq!(out.len(), 1);
        assert_eq!(error_of(&out[0]).code, error_code::PARSE_ERROR);
        match &out[0] {
            ServerMessage::Error { id, .. } => assert_eq!(*id, JsonRpcId::Null),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unclassifiable_frame_is_answered_against_its_own_id() {
        // 3.4 keeps null for unreadable ids; this one is readable, the shape is not.
        let (exit, out) = run_plain(frames(&[
            json!({"jsonrpc": "2.0", "id": "req-7"}),
            request(json!("after"), methods::DELETE_PUSH_CONFIG),
        ]))
        .await;

        assert_eq!(exit, ExitCode::Ok, "a bad shape is not fatal");
        assert_eq!(out.len(), 2);
        assert_eq!(error_of(&out[0]).code, error_code::INVALID_REQUEST);
        match &out[0] {
            ServerMessage::Error { id, .. } => {
                assert_eq!(*id, JsonRpcId::String("req-7".into()));
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn oversize_frame_is_refused_and_ends_the_session() {
        let body =
            serde_json::to_vec(&json!({"jsonrpc": "2.0", "id": 1, "pad": "x".repeat(64)})).unwrap();
        let (tx, mut rx) = mpsc::channel::<Frame>(8);
        let input = framed(&body);
        // A limit below the body forces the 3.7 path without a megabyte of input.
        let reader = FramedRead::new(
            input.as_slice(),
            StdioCodec::new(16, DEFAULT_MAX_HEADER_SIZE),
        );

        let exit = read_loop(
            reader,
            tx,
            Arc::new(MockHandler::default()),
            ServiceParams::default(),
            Arc::new(test_binding()),
            Cancels::default(),
        )
        .await;

        assert_eq!(exit, ExitCode::ProtocolError);
        let frame = rx.recv().await.expect("expected a -32000 answer");
        let msg = json::parse_from_server(&frame.body).unwrap();
        match msg {
            crate::json::ClientInbound::Message(m) => {
                assert_eq!(error_of(&m).code, BINDING_ERROR);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    // --- control frames (2.2, 2.3) ---

    #[tokio::test]
    async fn heartbeat_is_accepted_and_answered_with_nothing() {
        let (exit, out) = run_plain(frames(&[
            json!({"type": "heartbeat", "sessionId": "s-1", "ts": 1_700_000_000_000u64}),
        ]))
        .await;

        assert_eq!(exit, ExitCode::Ok);
        assert!(out.is_empty(), "a heartbeat carries no reply: {out:?}");
    }

    #[tokio::test]
    async fn a_second_handshake_is_a_protocol_violation() {
        let (exit, _) = run_plain(frames(&[
            json!({"type": "handshakeAck", "sessionId": "s-1", "selectVariant": "stdio-json", "accept": true}),
        ]))
        .await;

        assert_eq!(exit, ExitCode::ProtocolError, "2.2 happens exactly once");
    }

    // --- streaming (8) ---

    #[tokio::test]
    async fn empty_stream_is_closed_with_stream_end() {
        let input = frames(&[request(json!("s-1"), methods::SUBSCRIBE_TO_TASK)]);
        let (exit, _, out) = run(
            MockHandler::with_stream(StreamKind::Empty),
            ServiceParams::default(),
            input,
        )
        .await;

        assert_eq!(exit, ExitCode::Ok);
        assert_eq!(
            out,
            vec![ServerMessage::StreamEnd {
                id: JsonRpcId::String("s-1".into())
            }]
        );
    }

    #[tokio::test]
    async fn a_failed_stream_ends_with_an_error_and_no_stream_end() {
        let input = frames(&[request(json!("s-1"), methods::SUBSCRIBE_TO_TASK)]);
        let (_, _, out) = run(
            MockHandler::with_stream(StreamKind::Rejected),
            ServiceParams::default(),
            input,
        )
        .await;

        assert_eq!(out.len(), 1, "7.3: the error is the termination: {out:?}");
        assert!(matches!(out[0], ServerMessage::Error { .. }), "got {out:?}");
    }

    #[tokio::test]
    async fn cancel_stream_terminates_an_open_stream_with_stream_end() {
        let input = frames(&[
            request(json!("s-1"), methods::SUBSCRIBE_TO_TASK),
            json!({"jsonrpc": "2.0", "id": "s-1", "cancelStream": true}),
        ]);
        let (exit, _, out) = run(
            MockHandler::with_stream(StreamKind::Pending),
            ServiceParams::default(),
            input,
        )
        .await;

        assert_eq!(exit, ExitCode::Ok);
        assert_eq!(
            out,
            vec![ServerMessage::StreamEnd {
                id: JsonRpcId::String("s-1".into())
            }]
        );
    }

    #[tokio::test]
    async fn cancelling_an_unknown_id_is_ignored() {
        let (exit, out) = run_plain(frames(&[
            json!({"jsonrpc": "2.0", "id": "ghost", "cancelStream": true}),
        ]))
        .await;

        assert_eq!(exit, ExitCode::Ok);
        assert!(out.is_empty(), "got {out:?}");
    }

    #[tokio::test]
    async fn a_reused_streaming_id_is_refused_while_the_first_is_open() {
        let input = frames(&[
            request(json!("dup"), methods::SUBSCRIBE_TO_TASK),
            request(json!("dup"), methods::SUBSCRIBE_TO_TASK),
        ]);
        let (exit, _, out) = run(
            MockHandler::with_stream(StreamKind::Pending),
            ServiceParams::default(),
            input,
        )
        .await;

        assert_eq!(exit, ExitCode::Ok);
        assert_eq!(out.len(), 1, "only the refusal is emitted: {out:?}");
        assert_eq!(error_of(&out[0]).code, error_code::INVALID_REQUEST);
    }

    // --- session binding (9.6) ---

    fn binding_of(msg: &ServerMessage) -> &Value {
        match msg {
            ServerMessage::Result { result, .. } => &result["metadata"][metadata::BINDING_URI],
            other => panic!("expected a result, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_unary_task_carries_the_session_binding() {
        let (_, _, out) = run(
            MockHandler::default(),
            ServiceParams::default(),
            frames(&[request(json!("g-1"), methods::GET_TASK)]),
        )
        .await;

        assert_eq!(out.len(), 1, "got {out:?}");
        assert_eq!(binding_of(&out[0])["sessionId"], json!("s-1"));
        assert_eq!(binding_of(&out[0])["pid"], json!(42137));
        assert_eq!(binding_of(&out[0])["variant"], json!("stdio-json"));
    }

    #[tokio::test]
    async fn each_stream_item_carries_the_session_binding() {
        let (_, _, out) = run(
            MockHandler::with_stream(StreamKind::OneTask),
            ServiceParams::default(),
            frames(&[request(json!("s-1"), methods::SUBSCRIBE_TO_TASK)]),
        )
        .await;

        assert_eq!(out.len(), 2, "one result then streamEnd: {out:?}");
        // A stream item is a `StreamResponse`, so the task sits under the oneof
        // field and the wrapper itself is not something 9.6 stamps.
        let task = match &out[0] {
            ServerMessage::Result { result, .. } => &result["task"],
            other => panic!("expected a result, got {other:?}"),
        };
        assert_eq!(
            task["metadata"][metadata::BINDING_URI]["sessionId"],
            json!("s-1")
        );
        assert!(
            matches!(out[1], ServerMessage::StreamEnd { .. }),
            "got {out:?}"
        );
    }

    // --- shutdown and service parameters ---

    #[tokio::test]
    async fn system_shutdown_is_answered_then_ends_the_loop() {
        let input = frames(&[
            json!({"jsonrpc": "2.0", "id": 9, "method": json::SYSTEM_SHUTDOWN}),
            request(json!(10), methods::DELETE_PUSH_CONFIG),
        ]);
        let (exit, out) = run_plain(input).await;

        assert_eq!(exit, ExitCode::Ok);
        assert_eq!(
            out,
            vec![ServerMessage::Result {
                id: JsonRpcId::Number(9),
                result: json!({}),
            }],
            "6.9: nothing after the shutdown is served"
        );
    }

    /// A writer over a discard sink, plus the channel feeding it.
    fn writer_pair() -> (Sender<Frame>, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel::<Frame>(8);
        let sink = FramedWrite::new(
            tokio::io::sink(),
            StdioCodec::new(DEFAULT_MAX_FRAME_SIZE, DEFAULT_MAX_HEADER_SIZE),
        );
        (tx, tokio::spawn(write_loop(sink, rx)))
    }

    #[tokio::test]
    async fn shutdown_aborts_an_open_stream_rather_than_waiting_on_it() {
        let (tx, writer) = writer_pair();
        let cancels = Cancels::default();

        // A stream that never ends on its own. Its sender used to keep
        // `write_loop` alive forever, so `serve` never returned.
        let token = CancellationToken::new();
        lock(&cancels).insert(JsonRpcId::String("open".into()), token.clone());
        let task_tx = tx.clone();
        tokio::spawn(async move {
            token.cancelled().await;
            drop(task_tx);
        });

        assert!(
            drain_and_flush(&cancels, tx, writer).await,
            "the drain must finish once the stream is cancelled"
        );
        assert!(lock(&cancels).is_empty(), "the registry is emptied");
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_gives_up_on_work_that_cancellation_cannot_reach() {
        let (tx, writer) = writer_pair();
        let cancels = Cancels::default();

        // A unary handler observes no token, so only the grace window bounds this.
        let task_tx = tx.clone();
        tokio::spawn(async move {
            std::future::pending::<()>().await;
            drop(task_tx);
        });

        assert!(
            !drain_and_flush(&cancels, tx, writer).await,
            "the drain must give up instead of hanging"
        );
    }

    #[tokio::test]
    async fn shutdown_with_nothing_in_flight_returns_immediately() {
        let (tx, writer) = writer_pair();
        assert!(drain_and_flush(&Cancels::default(), tx, writer).await);
    }

    #[tokio::test]
    async fn session_params_are_visible_to_the_handler_and_overridable() {
        let mut session = ServiceParams::default();
        session.insert("a2a-version".into(), vec!["1.0".into()]);
        session.insert("x-tenant".into(), vec!["acme".into()]);

        let input = frames(&[json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": methods::DELETE_PUSH_CONFIG,
            "params": {},
            "serviceParams": {"x-tenant": "override"},
        })]);
        let (_, handler, _) = run(MockHandler::default(), session, input).await;

        let seen = lock(&handler.seen);
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0]["a2a-version"], vec!["1.0".to_string()]);
        assert_eq!(
            seen[0]["x-tenant"],
            vec!["override".to_string()],
            "the request wins over the session"
        );
    }
}
