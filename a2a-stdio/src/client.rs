// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Client side of the binding: the parent that spawns (2, 13).
//!
//! The mirror of [`crate::server`], with the roles inverted. One pipe pair
//! carries every call, so the same three-role split applies: a writer owning the
//! child's `STDIN`, a reader owning its `STDOUT`, and callers that never touch
//! either. What the server calls dispatch, this side calls correlation — a
//! response is matched to its caller by the `id` it was issued under (3.6),
//! because a pipe preserves no other association.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::Duration;

use a2a::*;
use a2a_client::{ServiceParams, Transport};
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use futures::{SinkExt, Stream, StreamExt};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::{FramedRead, FramedWrite};

use crate::codec::{DEFAULT_MAX_FRAME_SIZE, DEFAULT_MAX_HEADER_SIZE, StdioCodec};
use crate::frame::{Frame, FrameHeaders};
use crate::json::{self, ClientInbound, ClientMessage, ServerMessage, StdioRequest};
use crate::session::{self, NegotiationError};
use crate::wire::{ControlFrame, Variant};

/// Variants this client can actually speak, most preferred first.
///
/// 2.2 lets the server rank its own list and the client only veto, so this is a
/// capability set rather than a preference. It gains `stdio-proto` when 15 lands.
const SUPPORTED_VARIANTS: &[Variant] = &[Variant::Json];

pub struct ClientConfig {
    pub max_frame_size: usize,
    pub max_header_size: usize,
    /// Depth of the queue feeding the writer task.
    pub outbound_capacity: usize,
    /// Per-stream buffer. Reaching it stalls the reader, which is the only
    /// backpressure available when every stream shares one pipe (13.4).
    pub stream_capacity: usize,
    /// How long [`Transport::destroy`] waits for the child to exit (2.4).
    pub shutdown_timeout: Duration,
    /// Whether `destroy` asks with `system/shutdown` before closing `STDIN`.
    /// 2.4 makes this optional and 6.9 makes it an extension, so an agent that
    /// does not implement it answers `MethodNotFound` and exits on EOF anyway.
    pub send_shutdown_request: bool,
}

impl Default for ClientConfig {
    fn default() -> Self {
        ClientConfig {
            max_frame_size: DEFAULT_MAX_FRAME_SIZE,
            max_header_size: DEFAULT_MAX_HEADER_SIZE,
            outbound_capacity: 64,
            stream_capacity: 64,
            shutdown_timeout: Duration::from_secs(5), // 2.4 recommends 5 s
            send_shutdown_request: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Launch descriptor
// ---------------------------------------------------------------------------

/// How to spawn an agent (2.1), built to make the 13.1 rules unavoidable.
///
/// Program and arguments stay separate values all the way to `execve`, so there
/// is no point at which a shell could interpret them. Callers are still
/// responsible for the checks this type cannot make: resolving the program
/// against an allowlist of trusted binaries, and verifying its provenance.
pub struct Launch {
    program: OsString,
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
    current_dir: Option<PathBuf>,
    session_id: String,
    inherit_env: bool,
}

impl Launch {
    /// Starts with an empty environment and a fresh session id; add only what
    /// the agent needs.
    pub fn new(program: impl Into<OsString>) -> Self {
        Launch {
            program: program.into(),
            args: Vec::new(),
            envs: Vec::new(),
            current_dir: None,
            session_id: uuid::Uuid::now_v7().to_string(),
            inherit_env: false,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }

    pub fn current_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.current_dir = Some(dir.into());
        self
    }

    /// Override the generated `A2A_SESSION_ID`. The server echoes it in the
    /// handshake and a mismatch fails the session (2.2 step 2).
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = id.into();
        self
    }

    /// 2.1: the A2A version this client intends to use.
    pub fn protocol_version(self, version: impl Into<OsString>) -> Self {
        self.env(session::ENV_PROTOCOL_VERSION, version)
    }

    pub fn log_level(self, level: impl Into<OsString>) -> Self {
        self.env(session::ENV_LOG_LEVEL, level)
    }

    /// 4.1: session-scoped parameters, the inverse of
    /// [`session::service_params_from_env`].
    pub fn service_params(mut self, params: &ServiceParams) -> Self {
        for (key, values) in params {
            let name = format!(
                "{}{}",
                session::ENV_SERVICE_PARAM_PREFIX,
                key.to_ascii_uppercase().replace('-', "_")
            );
            self = self.env(name, values.join(","));
        }
        self
    }

    /// Pass the parent's whole environment through. 13.2 argues against it:
    /// the child then sees every unrelated secret this process holds.
    pub fn inherit_env(mut self, inherit: bool) -> Self {
        self.inherit_env = inherit;
        self
    }

    fn build(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        if !self.inherit_env {
            cmd.env_clear();
        }
        cmd.env(session::ENV_SESSION_ID, &self.session_id);
        for (key, value) in &self.envs {
            cmd.env(key, value);
        }
        if let Some(dir) = &self.current_dir {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped());
        // 13.3 and 14: diagnostics must never reach the protocol stream, so
        // they are passed through to ours instead of being read as frames.
        cmd.stderr(Stdio::inherit());
        // 13.4: a transport dropped without `destroy` must not leak a process.
        cmd.kill_on_drop(true);
        cmd
    }
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

/// A2A client over one spawned agent process.
pub struct StdioTransport {
    session: Arc<Session>,
}

/// Only the negotiated identity: 13.5 keeps payloads out of diagnostics.
impl std::fmt::Debug for StdioTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioTransport")
            .field("session_id", &self.session.session_id)
            .field("variant", &self.session.variant)
            .field("alive", &self.session.alive.load(Ordering::Relaxed))
            .finish()
    }
}

type Pendings = Arc<Mutex<HashMap<JsonRpcId, Pending>>>;

/// A caller waiting on an `id`. 3.5 makes a unary response and a stream chunk
/// byte-identical, so which of these is registered is what tells them apart.
enum Pending {
    Unary(oneshot::Sender<Result<Value, A2AError>>),
    Stream(mpsc::Sender<Result<Value, A2AError>>),
}

impl Pending {
    async fn fail(self, error: A2AError) {
        match self {
            Pending::Unary(tx) => {
                let _ = tx.send(Err(error));
            }
            Pending::Stream(tx) => {
                let _ = tx.send(Err(error)).await;
            }
        }
    }
}

struct Session {
    /// `None` once [`Session::shutdown`] has run. Dropping the last sender is
    /// what closes the child's `STDIN`, so this doubles as the closed flag.
    outbound: Mutex<Option<mpsc::Sender<Frame>>>,
    pending: Pendings,
    /// Cleared by the reader task when the agent's `STDOUT` ends.
    alive: Arc<AtomicBool>,
    next_id: AtomicU64,
    session_id: String,
    variant: Variant,
    features: Vec<String>,
    child: tokio::sync::Mutex<Option<Child>>,
    stream_capacity: usize,
    shutdown_timeout: Duration,
    send_shutdown_request: bool,
}

impl StdioTransport {
    /// Spawn an agent and complete the handshake (2.1, 2.2).
    pub async fn connect(launch: Launch, config: ClientConfig) -> Result<Self, A2AError> {
        let session_id = launch.session_id.clone();
        let mut child = launch
            .build()
            .spawn()
            .map_err(|e| A2AError::internal(format!("cannot spawn agent: {e}")))?;

        // Both were requested as pipes above, so absence is a bug, not input.
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| A2AError::internal("child STDOUT is not a pipe"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| A2AError::internal("child STDIN is not a pipe"))?;

        // `kill_on_drop` reaps the child if the handshake below fails.
        start(stdout, stdin, session_id, config, Some(child)).await
    }

    /// Complete the handshake over pipes obtained some other way.
    ///
    /// 2 permits attaching to an already-running process. Termination is then
    /// the caller's problem: [`Transport::destroy`] can close `STDIN` but has no
    /// process to wait on or kill.
    pub async fn attach<R, W>(
        stdout: R,
        stdin: W,
        session_id: impl Into<String>,
        config: ClientConfig,
    ) -> Result<Self, A2AError>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        start(stdout, stdin, session_id.into(), config, None).await
    }

    pub fn session_id(&self) -> &str {
        &self.session.session_id
    }

    pub fn variant(&self) -> Variant {
        self.session.variant
    }

    /// What the server advertised in the handshake (2.2), e.g. `streaming`.
    pub fn server_features(&self) -> &[String] {
        &self.session.features
    }

    async fn call<Req, Resp>(
        &self,
        params: &ServiceParams,
        method: &str,
        request: &Req,
    ) -> Result<Resp, A2AError>
    where
        Req: ProtoJsonPayload,
        Resp: ProtoJsonPayload,
    {
        let result = self.call_value(params, method, encode(request)?).await?;
        decode(result)
    }

    async fn call_value(
        &self,
        params: &ServiceParams,
        method: &str,
        payload: Value,
    ) -> Result<Value, A2AError> {
        let id = self.session.next_id();
        let (tx, rx) = oneshot::channel();
        lock(&self.session.pending).insert(id.clone(), Pending::Unary(tx));
        // From here every exit path, including the caller dropping this future,
        // has to unregister; a leaked entry would outlive its waiter.
        let _guard = Unregister {
            pending: self.session.pending.clone(),
            id: id.clone(),
        };

        self.session
            .send(encode_request(&id, method, payload, params)?)
            .await?;

        match rx.await {
            Ok(result) => result,
            // The entry was dropped without an answer, which only the teardown
            // path does, and it answers first. So the session is simply gone.
            Err(_) => Err(A2AError::internal("stdio session ended before a response")),
        }
    }

    async fn call_streaming<Req>(
        &self,
        params: &ServiceParams,
        method: &str,
        request: &Req,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError>
    where
        Req: ProtoJsonPayload,
    {
        let payload = encode(request)?;
        let id = self.session.next_id();
        let (tx, rx) = mpsc::channel(self.session.stream_capacity);
        lock(&self.session.pending).insert(id.clone(), Pending::Stream(tx));

        // The handle owns the registration from here, including the 8.5 cancel
        // it sends if the consumer drops the stream early.
        let mut handle = StreamHandle {
            id: id.clone(),
            rx,
            session: self.session.clone(),
            finished: false,
        };
        let frame = match encode_request(&id, method, payload, params) {
            Ok(frame) => frame,
            Err(e) => {
                handle.finished = true;
                return Err(e);
            }
        };
        if let Err(e) = self.session.send(frame).await {
            // Nothing reached the agent, so there is no stream to cancel.
            handle.finished = true;
            return Err(e);
        }
        Ok(Box::pin(handle))
    }
}

/// Read the handshake, answer it, and start the reader and writer tasks.
async fn start<R, W>(
    stdout: R,
    stdin: W,
    session_id: String,
    config: ClientConfig,
    child: Option<Child>,
) -> Result<StdioTransport, A2AError>
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = FramedRead::new(
        stdout,
        StdioCodec::new(config.max_frame_size, config.max_header_size),
    );
    let mut writer = FramedWrite::new(
        stdin,
        StdioCodec::new(config.max_frame_size, config.max_header_size),
    );

    // 2.2 step 1: the server speaks first, and this frame is always JSON.
    let handshake = match reader.next().await {
        Some(Ok(frame)) => match json::parse_from_server(&frame.body) {
            Ok(ClientInbound::Control(ControlFrame::Handshake(hs))) => hs,
            Ok(_) => {
                return Err(A2AError::internal(
                    "agent sent something other than a handshake as its first frame",
                ));
            }
            Err(e) => return Err(A2AError::internal(format!("malformed handshake: {e}"))),
        },
        Some(Err(e)) => {
            return Err(A2AError::internal(format!(
                "framing error during handshake: {e}"
            )));
        }
        None => {
            return Err(A2AError::internal(
                "agent closed STDOUT before sending a handshake",
            ));
        }
    };

    // 2.2 steps 2-3.
    let (ack, variant) =
        match session::respond_to_handshake(&handshake, &session_id, SUPPORTED_VARIANTS) {
            Ok(pair) => pair,
            Err(e) => {
                // 2.2 step 4: refusing lets the agent exit 0 instead of waiting
                // on an ack that is never coming.
                let decline = ControlFrame::HandshakeAck(session::decline(&session_id));
                if let Ok(frame) = encode_control(&decline) {
                    let _ = writer.send(frame).await;
                }
                return Err(negotiation_failed(&e));
            }
        };
    writer
        .send(encode_control(&ControlFrame::HandshakeAck(ack))?)
        .await
        .map_err(|e| A2AError::internal(format!("cannot write handshakeAck: {e}")))?;

    let (tx, rx) = mpsc::channel::<Frame>(config.outbound_capacity);
    let pending: Pendings = Arc::new(Mutex::new(HashMap::new()));
    let alive = Arc::new(AtomicBool::new(true));

    tokio::spawn(write_loop(writer, rx));
    tokio::spawn(read_loop(reader, pending.clone(), alive.clone()));

    Ok(StdioTransport {
        session: Arc::new(Session {
            outbound: Mutex::new(Some(tx)),
            pending,
            alive,
            next_id: AtomicU64::new(1),
            session_id,
            variant,
            features: handshake.features,
            child: tokio::sync::Mutex::new(child),
            stream_capacity: config.stream_capacity,
            shutdown_timeout: config.shutdown_timeout,
            send_shutdown_request: config.send_shutdown_request,
        }),
    })
}

/// 2.2 step 4 asks the client to report why negotiation failed. Only the
/// version case has an A2A code of its own; the rest are local protocol faults.
fn negotiation_failed(e: &NegotiationError) -> A2AError {
    match e {
        NegotiationError::VersionNotSupported(v) => A2AError::version_not_supported(v),
        other => A2AError::internal(format!("stdio handshake failed: {other}")),
    }
}

/// The only task permitted to touch the child's `STDIN`.
///
/// Ends when every sender is gone, which drops the pipe and gives the agent the
/// EOF that 2.4 uses as the shutdown signal.
async fn write_loop<W>(mut sink: FramedWrite<W, StdioCodec>, mut rx: mpsc::Receiver<Frame>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(frame) = rx.recv().await {
        if let Err(e) = sink.send(frame).await {
            eprintln!("a2a-stdio: dropping outbound frame: {e}");
            break;
        }
    }
    let _ = sink.flush().await;
}

/// The only task reading the child's `STDOUT`, and so the only place a response
/// can be matched to its caller.
async fn read_loop<R>(
    mut reader: FramedRead<R, StdioCodec>,
    pending: Pendings,
    alive: Arc<AtomicBool>,
) where
    R: AsyncRead + Unpin,
{
    let reason = loop {
        let Some(item) = reader.next().await else {
            break A2AError::internal("stdio session ended: agent closed STDOUT");
        };
        let frame = match item {
            Ok(frame) => frame,
            // Fatal, unlike a bad body below: a desynchronized byte stream
            // cannot be resynchronized, and guessing where the next frame
            // starts is how a hostile agent would get to choose (13.3).
            Err(e) => break A2AError::internal(format!("stdio framing error: {e}")),
        };
        match json::parse_from_server(&frame.body) {
            Err(e) => unreadable(&pending, &frame.body, e).await,
            // 2.3: liveness only, and it resets nothing yet because this client
            // does not negotiate heartbeats.
            Ok(ClientInbound::Control(ControlFrame::Heartbeat(_))) => {}
            // A lifecycle violation rather than a bad message: 2.2 runs the
            // handshake once, so a second one means the agent's idea of the
            // session no longer matches ours and no id can be trusted.
            Ok(ClientInbound::Control(_)) => {
                break A2AError::internal("agent repeated the handshake mid-session");
            }
            Ok(ClientInbound::Message(msg)) => deliver(&pending, msg).await,
        }
    };

    alive.store(false, Ordering::SeqCst);
    // 8.3: process exit terminates every active stream, and a unary caller
    // would otherwise wait forever.
    let orphans: Vec<Pending> = lock(&pending).drain().map(|(_, p)| p).collect();
    for waiter in orphans {
        waiter.fail(reason.clone()).await;
    }
}

/// Absorb a frame whose body could not be read.
///
/// 3.1 forbids failing a message over content this side does not recognize, and
/// `Content-Length` already told us where the frame ended, so the session is
/// still intact. 3.4 keeps the id readable even when the shape is not, which
/// puts the cost on the one caller that cannot be answered instead of on every
/// in-flight request.
async fn unreadable(pending: &Pendings, body: &[u8], error: crate::wire::WireError) {
    eprintln!("a2a-stdio: ignoring an unreadable frame from the agent: {error}");
    let id = json::peek_id(body);
    if matches!(id, JsonRpcId::Null) {
        return;
    }
    let waiter = lock(pending).remove(&id);
    if let Some(waiter) = waiter {
        waiter
            .fail(A2AError::internal(format!(
                "agent sent a response this client cannot read: {error}"
            )))
            .await;
    }
}

/// Hand one server message to whoever is waiting on its `id`.
///
/// An `id` with no waiter is dropped, not an error: 8.5 lets a stream be
/// cancelled while frames for it are already in the pipe.
async fn deliver(pending: &Pendings, msg: ServerMessage) {
    match msg {
        ServerMessage::Result { id, result } => match claim(pending, &id) {
            Some(Pending::Unary(tx)) => {
                let _ = tx.send(Ok(result));
            }
            Some(Pending::Stream(tx)) => {
                // Awaiting here stalls the pipe once a consumer falls behind,
                // which is the only backpressure one shared pipe can express.
                if tx.send(Ok(result)).await.is_err() {
                    lock(pending).remove(&id);
                }
            }
            None => {}
        },

        // 7.3 and 8.3: an error is terminal for a stream too, and no
        // `streamEnd` follows it. Removing the entry drops the sender, which is
        // what ends the consumer's stream after it sees this item.
        ServerMessage::Error { id, error } => {
            let waiter = lock(pending).remove(&id);
            if let Some(waiter) = waiter {
                waiter.fail(from_jsonrpc(error)).await;
            }
        }

        ServerMessage::StreamEnd { id } => match lock(pending).remove(&id) {
            Some(Pending::Unary(tx)) => {
                let _ = tx.send(Err(A2AError::internal(
                    "agent ended a stream for a unary request",
                )));
            }
            // Dropping the sender is the end of the stream (8.3).
            Some(Pending::Stream(_)) | None => {}
        },
    }
}

/// Take a unary waiter, or borrow a stream's sender without unregistering it.
fn claim(pending: &Pendings, id: &JsonRpcId) -> Option<Pending> {
    let mut map = lock(pending);
    match map.get(id) {
        Some(Pending::Stream(tx)) => Some(Pending::Stream(tx.clone())),
        Some(Pending::Unary(_)) => map.remove(id),
        None => None,
    }
}

impl Session {
    fn next_id(&self) -> JsonRpcId {
        // 3.2 allows a number; uniqueness within the session is all 3.6 needs.
        JsonRpcId::Number(self.next_id.fetch_add(1, Ordering::Relaxed) as i64)
    }

    fn sender(&self) -> Result<mpsc::Sender<Frame>, A2AError> {
        if !self.alive.load(Ordering::SeqCst) {
            return Err(A2AError::internal("stdio session ended"));
        }
        lock(&self.outbound)
            .clone()
            .ok_or_else(|| A2AError::internal("stdio session is closed"))
    }

    async fn send(&self, frame: Frame) -> Result<(), A2AError> {
        self.sender()?
            .send(frame)
            .await
            .map_err(|_| A2AError::internal("stdio session is closed"))
    }

    /// 8.5: stop one stream without disturbing the session or the task behind
    /// it. Called from `Drop`, so it cannot await and cannot report failure.
    fn cancel_stream(&self, id: &JsonRpcId) {
        let Ok(tx) = self.sender() else { return };
        let Ok(body) = ClientMessage::CancelStream { id: id.clone() }.to_vec() else {
            return;
        };
        let _ = tx.try_send(json_frame(body));
    }

    /// 2.4: ask, close `STDIN`, wait, then force.
    async fn shutdown(&self) -> Result<(), A2AError> {
        if self.send_shutdown_request {
            // 6.9 is an extension, so a refusal is expected and ignored; EOF
            // below is the part every agent must honour. No waiter is
            // registered because the answer may never arrive.
            if let Ok(frame) = encode_request(
                &self.next_id(),
                json::SYSTEM_SHUTDOWN,
                Value::Object(Default::default()),
                &ServiceParams::new(),
            ) {
                let _ = self.send(frame).await;
            }
        }

        // Queued frames still drain: the writer sees them before the close.
        lock(&self.outbound).take();

        let mut slot = self.child.lock().await;
        let Some(child) = slot.as_mut() else {
            // Attached rather than spawned, so there is no process to reap.
            return Ok(());
        };
        match tokio::time::timeout(self.shutdown_timeout, child.wait()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(A2AError::internal(format!("cannot reap agent: {e}"))),
            Err(_) => {
                // 13.4 forbids leaking the process, so the grace window is the
                // whole negotiation. 2.4 suggests escalating through SIGTERM
                // first; that needs a platform-specific signal call, and the
                // agent has already had `shutdown_timeout` after EOF.
                eprintln!(
                    "a2a-stdio: agent outlived the {:?} shutdown window; killing it",
                    self.shutdown_timeout
                );
                let _ = child.kill().await;
            }
        }
        *slot = None;
        Ok(())
    }
}

/// Removes a registration when a caller's future is dropped before its answer.
struct Unregister {
    pending: Pendings,
    id: JsonRpcId,
}

impl Drop for Unregister {
    fn drop(&mut self) {
        lock(&self.pending).remove(&self.id);
    }
}

/// A server stream, plus the cleanup its lifetime implies.
struct StreamHandle {
    id: JsonRpcId,
    rx: mpsc::Receiver<Result<Value, A2AError>>,
    session: Arc<Session>,
    finished: bool,
}

impl Stream for StreamHandle {
    type Item = Result<StreamResponse, A2AError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.finished {
            return Poll::Ready(None);
        }
        match this.rx.poll_recv(cx) {
            Poll::Pending => Poll::Pending,
            // The registration was dropped: `streamEnd`, or the session ended.
            Poll::Ready(None) => {
                this.finished = true;
                Poll::Ready(None)
            }
            Poll::Ready(Some(Err(e))) => {
                this.finished = true;
                Poll::Ready(Some(Err(e)))
            }
            // A payload this client cannot read is reported per item rather than
            // as the end of the stream: only the agent decides when that is.
            Poll::Ready(Some(Ok(value))) => Poll::Ready(Some(decode(value))),
        }
    }
}

impl Drop for StreamHandle {
    fn drop(&mut self) {
        // Only an early drop needs cancelling; a stream the agent already ended
        // has no server-side state left to stop.
        if !self.finished {
            self.session.cancel_stream(&self.id);
        }
        lock(&self.session.pending).remove(&self.id);
    }
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn encode<T: ProtoJsonPayload>(value: &T) -> Result<Value, A2AError> {
    protojson_conv::to_value(value)
        .map_err(|e| A2AError::internal(format!("failed to serialize ProtoJSON payload: {e}")))
}

fn decode<T: ProtoJsonPayload>(value: Value) -> Result<T, A2AError> {
    protojson_conv::from_value(value)
        .map_err(|e| A2AError::internal(format!("invalid response payload: {e}")))
}

/// 4.2 carries service parameters in the body, not in frame headers: the
/// headers in 3.1 are the transport's, and a per-request parameter is the
/// application's.
fn encode_request(
    id: &JsonRpcId,
    method: &str,
    payload: Value,
    params: &ServiceParams,
) -> Result<Frame, A2AError> {
    let request = StdioRequest::new(id.clone(), method, Some(payload)).with_service_params(params);
    let body = ClientMessage::Request(request)
        .to_vec()
        .map_err(|e| A2AError::internal(format!("cannot serialize request: {e}")))?;
    Ok(json_frame(body))
}

fn encode_control(frame: &ControlFrame) -> Result<Frame, A2AError> {
    let body = serde_json::to_vec(frame)
        .map_err(|e| A2AError::internal(format!("cannot serialize control frame: {e}")))?;
    Ok(json_frame(body))
}

/// No optional headers: 3.1 defaults `Content-Type` for `stdio-json`.
fn json_frame(body: Vec<u8>) -> Frame {
    Frame {
        headers: FrameHeaders::default(),
        body: Bytes::from(body),
    }
}

/// 7.1 puts the same JSON-RPC error object on the wire as the HTTP binding, so
/// the mapping is shared with the other clients rather than reimplemented.
fn from_jsonrpc(error: JsonRpcError) -> A2AError {
    let details: Vec<TypedDetail> = match error.data {
        Some(Value::Array(items)) => items
            .into_iter()
            .filter_map(|v| serde_json::from_value(v).ok())
            .collect(),
        _ => Vec::new(),
    };
    a2a_client::a2a_error_from_details(error.code, error.message, details)
}

/// No invariant here survives a panic badly, so poisoning is recovered from.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Transport
// ---------------------------------------------------------------------------

#[async_trait]
impl Transport for StdioTransport {
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
        req: &TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        // Plain ProtoJSON, matching what this binding's server decodes. The
        // HTTP JSON-RPC binding needs a compatibility shape here; stdio is new
        // enough to have no legacy peer to accommodate.
        self.call(params, methods::CREATE_PUSH_CONFIG, req).await
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.call(params, methods::GET_PUSH_CONFIG, req).await
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        self.call(params, methods::LIST_PUSH_CONFIGS, req).await
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        // The one method with no response body; the server answers `null`.
        self.call_value(params, methods::DELETE_PUSH_CONFIG, encode(req)?)
            .await
            .map(|_| ())
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
        self.session.shutdown().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::{ServerInbound, parse_from_client};
    use crate::wire::Handshake;
    use serde_json::json;
    use tokio::io::DuplexStream;

    const CAP: usize = 64 * 1024;

    /// Stands in for the agent, so the whole session is exercised without
    /// spawning a process: only `connect` differs from `attach` downstream.
    struct Peer {
        reader: FramedRead<DuplexStream, StdioCodec>,
        writer: FramedWrite<DuplexStream, StdioCodec>,
    }

    impl Peer {
        async fn recv(&mut self) -> ServerInbound {
            let frame = self
                .reader
                .next()
                .await
                .expect("client sent nothing")
                .expect("client sent a malformed frame");
            parse_from_client(&frame.body).expect("client sent an unclassifiable frame")
        }

        async fn recv_request(&mut self) -> StdioRequest {
            match self.recv().await {
                ServerInbound::Message(ClientMessage::Request(r)) => r,
                other => panic!("expected a request, got {other:?}"),
            }
        }

        async fn send(&mut self, msg: ServerMessage) {
            let frame = json_frame(msg.to_vec().unwrap());
            self.writer.send(frame).await.unwrap();
        }

        async fn send_control(&mut self, frame: ControlFrame) {
            self.writer
                .send(encode_control(&frame).unwrap())
                .await
                .unwrap();
        }

        async fn send_raw(&mut self, body: Value) {
            let frame = json_frame(serde_json::to_vec(&body).unwrap());
            self.writer.send(frame).await.unwrap();
        }

        async fn result(&mut self, id: JsonRpcId, value: &impl ProtoJsonPayload) {
            self.send(ServerMessage::Result {
                id,
                result: encode(value).unwrap(),
            })
            .await;
        }
    }

    fn codec() -> StdioCodec {
        StdioCodec::new(DEFAULT_MAX_FRAME_SIZE, DEFAULT_MAX_HEADER_SIZE)
    }

    /// Two independent pipes rather than one split duplex.
    ///
    /// A child's `STDIN` and `STDOUT` are separate objects, so dropping the
    /// writer closes one direction and leaves the other readable. Splitting a
    /// single duplex would keep it alive through the read half and hide the EOF
    /// that 2.4 shutdown depends on.
    fn wire() -> (DuplexStream, DuplexStream, Peer) {
        let (client_read, agent_write) = tokio::io::duplex(CAP);
        let (agent_read, client_write) = tokio::io::duplex(CAP);
        let peer = Peer {
            reader: FramedRead::new(agent_read, codec()),
            writer: FramedWrite::new(agent_write, codec()),
        };
        (client_read, client_write, peer)
    }

    fn offer(session_id: &str, variants: Vec<Variant>) -> ControlFrame {
        ControlFrame::Handshake(Handshake {
            protocol: session::PROTOCOL_NAME.into(),
            protocol_version: session::PROTOCOL_VERSION.into(),
            session_id: session_id.into(),
            supported_variants: variants,
            features: vec![session::FEATURE_STREAMING.into()],
            pid: Some(1),
        })
    }

    async fn connected(config: ClientConfig) -> (StdioTransport, Peer) {
        let (client_read, client_write, mut peer) = wire();
        peer.send_control(offer("s-1", vec![Variant::Json])).await;
        let transport = StdioTransport::attach(client_read, client_write, "s-1", config)
            .await
            .expect("handshake failed");
        match peer.recv().await {
            ServerInbound::Control(ControlFrame::HandshakeAck(ack)) => {
                assert!(ack.accept);
                assert_eq!(ack.select_variant, Variant::Json);
                assert_eq!(ack.session_id, "s-1");
            }
            other => panic!("expected handshakeAck, got {other:?}"),
        }
        (transport, peer)
    }

    fn sample_task(id: &str) -> Task {
        Task {
            id: id.into(),
            context_id: "c-1".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    fn get_task(id: &str) -> GetTaskRequest {
        GetTaskRequest {
            id: id.into(),
            history_length: None,
            tenant: None,
        }
    }

    fn subscribe(id: &str) -> SubscribeToTaskRequest {
        SubscribeToTaskRequest {
            id: id.into(),
            tenant: None,
        }
    }

    // ---- handshake (2.2) ------------------------------------------------

    #[tokio::test]
    async fn the_handshake_is_acked_with_the_negotiated_variant() {
        let (transport, _peer) = connected(ClientConfig::default()).await;
        assert_eq!(transport.session_id(), "s-1");
        assert_eq!(transport.variant(), Variant::Json);
        assert_eq!(transport.server_features(), [session::FEATURE_STREAMING]);
    }

    #[tokio::test]
    async fn a_foreign_session_id_is_rejected() {
        // 2.2 step 2: the id must be the one we passed at spawn.
        let (client_read, client_write, mut peer) = wire();
        peer.send_control(offer("someone-else", vec![Variant::Json]))
            .await;
        let err = StdioTransport::attach(client_read, client_write, "s-1", ClientConfig::default())
            .await
            .unwrap_err();
        assert!(
            err.message.contains("session id mismatch"),
            "{}",
            err.message
        );
    }

    #[tokio::test]
    async fn no_common_variant_is_declined_rather_than_dropped() {
        // 2.2 step 4: without an ack the agent would wait instead of exiting 0.
        let (client_read, client_write, mut peer) = wire();
        peer.send_control(offer("s-1", vec![Variant::Proto])).await;
        let err = StdioTransport::attach(client_read, client_write, "s-1", ClientConfig::default())
            .await
            .unwrap_err();
        assert!(
            err.message.contains("no serialization variant"),
            "{}",
            err.message
        );

        match peer.recv().await {
            ServerInbound::Control(ControlFrame::HandshakeAck(ack)) => assert!(!ack.accept),
            other => panic!("expected a declining handshakeAck, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn an_incompatible_version_keeps_its_own_error_code() {
        let (client_read, client_write, mut peer) = wire();
        let mut frame = offer("s-1", vec![Variant::Json]);
        if let ControlFrame::Handshake(hs) = &mut frame {
            hs.protocol_version = "2.0".into();
        }
        peer.send_control(frame).await;
        let err = StdioTransport::attach(client_read, client_write, "s-1", ClientConfig::default())
            .await
            .unwrap_err();
        assert_eq!(err.code, error_code::VERSION_NOT_SUPPORTED);
    }

    #[tokio::test]
    async fn a_method_frame_before_the_handshake_is_refused() {
        let (client_read, client_write, mut peer) = wire();
        peer.result(JsonRpcId::Number(1), &sample_task("t-1")).await;
        let err = StdioTransport::attach(client_read, client_write, "s-1", ClientConfig::default())
            .await
            .unwrap_err();
        assert!(err.message.contains("first frame"), "{}", err.message);
    }

    // ---- unary calls -----------------------------------------------------

    #[tokio::test]
    async fn a_unary_call_round_trips() {
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let (params, req) = (ServiceParams::new(), get_task("t-1"));
        let call = transport.get_task(&params, &req);
        let agent = async {
            let req = peer.recv_request().await;
            assert_eq!(req.method, methods::GET_TASK);
            assert_eq!(req.params.as_ref().unwrap()["id"], json!("t-1"));
            peer.result(req.id, &sample_task("t-1")).await;
        };
        let (result, ()) = tokio::join!(call, agent);
        assert_eq!(result.unwrap().id, "t-1");
    }

    #[tokio::test]
    async fn concurrent_calls_are_correlated_by_id() {
        // 3.6: the pipe carries no other association between the two.
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let params = ServiceParams::new();
        let (a_req, b_req) = (get_task("t-a"), get_task("t-b"));
        let first = transport.get_task(&params, &a_req);
        let second = transport.get_task(&params, &b_req);
        let agent = async {
            let a = peer.recv_request().await;
            let b = peer.recv_request().await;
            assert_ne!(a.id, b.id, "each request needs its own id");
            // Answered in reverse, so ordering cannot stand in for correlation.
            for req in [b, a] {
                let asked = req.params.as_ref().unwrap()["id"]
                    .as_str()
                    .unwrap()
                    .to_string();
                peer.result(req.id, &sample_task(&asked)).await;
            }
        };
        let (a, b, ()) = tokio::join!(first, second, agent);
        assert_eq!(a.unwrap().id, "t-a");
        assert_eq!(b.unwrap().id, "t-b");
    }

    #[tokio::test]
    async fn service_params_travel_in_the_request_body() {
        // 4.2: a flat map, with 4.1's comma joining multi-values.
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let mut params = ServiceParams::new();
        params.insert("a2a-extensions".into(), vec!["a".into(), "b".into()]);

        let req = get_task("t-1");
        let call = transport.get_task(&params, &req);
        let agent = async {
            let req = peer.recv_request().await;
            let sp = req.service_params.clone().expect("serviceParams missing");
            assert_eq!(sp["a2a-extensions"], "a,b");
            peer.result(req.id, &sample_task("t-1")).await;
        };
        let (result, ()) = tokio::join!(call, agent);
        result.unwrap();
    }

    #[tokio::test]
    async fn an_error_frame_keeps_its_a2a_code() {
        // 7.1 and 7.2: the error object is the JSON-RPC binding's, so the code
        // has to survive the round trip rather than collapse to INTERNAL_ERROR.
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let (params, req) = (ServiceParams::new(), get_task("t-1"));
        let call = transport.get_task(&params, &req);
        let agent = async {
            let req = peer.recv_request().await;
            peer.send(ServerMessage::Error {
                id: req.id,
                error: A2AError::task_not_found("t-1").to_jsonrpc_error(),
            })
            .await;
        };
        let (result, ()) = tokio::join!(call, agent);
        let err = result.unwrap_err();
        assert_eq!(err.code, error_code::TASK_NOT_FOUND);
        assert!(err.message.contains("t-1"), "{}", err.message);
    }

    #[tokio::test]
    async fn a_null_result_satisfies_the_one_method_without_a_body() {
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let req = DeleteTaskPushNotificationConfigRequest {
            task_id: "t-1".into(),
            id: "cfg-1".into(),
            tenant: None,
        };
        let params = ServiceParams::new();
        let call = transport.delete_push_config(&params, &req);
        let agent = async {
            let req = peer.recv_request().await;
            assert_eq!(req.method, methods::DELETE_PUSH_CONFIG);
            peer.send(ServerMessage::Result {
                id: req.id,
                result: Value::Null,
            })
            .await;
        };
        let (result, ()) = tokio::join!(call, agent);
        result.unwrap();
    }

    #[tokio::test]
    async fn an_unreadable_frame_fails_one_call_and_not_the_session() {
        // 3.1 forbids failing over content we do not recognize, and 3.4 keeps
        // the id readable, so only the caller that cannot be answered suffers.
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let params = ServiceParams::new();

        let first = get_task("t-1");
        let call = transport.get_task(&params, &first);
        let agent = async {
            let req = peer.recv_request().await;
            // Valid JSON-RPC, but carrying neither a result nor an error.
            peer.send_raw(json!({"jsonrpc": "2.0", "id": req.id})).await;
        };
        let (result, ()) = tokio::join!(call, agent);
        let err = result.unwrap_err();
        assert!(err.message.contains("cannot read"), "{}", err.message);

        let second = get_task("t-2");
        let call = transport.get_task(&params, &second);
        let agent = async {
            let req = peer.recv_request().await;
            peer.result(req.id, &sample_task("t-2")).await;
        };
        let (result, ()) = tokio::join!(call, agent);
        assert_eq!(
            result.unwrap().id,
            "t-2",
            "one bad frame must not end the session"
        );
    }

    #[tokio::test]
    async fn an_unreadable_frame_without_an_id_is_only_logged() {
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        peer.send_raw(json!(["not", "an", "object"])).await;

        let params = ServiceParams::new();
        let req = get_task("t-1");
        let call = transport.get_task(&params, &req);
        let agent = async {
            let req = peer.recv_request().await;
            peer.result(req.id, &sample_task("t-1")).await;
        };
        let (result, ()) = tokio::join!(call, agent);
        result.unwrap();
    }

    // ---- streaming (8) --------------------------------------------------

    /// Issue a streaming call and hand back the id the agent saw for it.
    async fn open_stream(
        transport: &StdioTransport,
        peer: &mut Peer,
    ) -> (
        BoxStream<'static, Result<StreamResponse, A2AError>>,
        JsonRpcId,
    ) {
        let (params, req) = (ServiceParams::new(), subscribe("t-1"));
        let call = transport.subscribe_to_task(&params, &req);
        let agent = async { peer.recv_request().await };
        let (stream, req) = tokio::join!(call, agent);
        assert_eq!(req.method, methods::SUBSCRIBE_TO_TASK);
        (stream.unwrap(), req.id)
    }

    #[tokio::test]
    async fn a_stream_yields_items_until_stream_end() {
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let (mut stream, id) = open_stream(&transport, &mut peer).await;

        peer.result(id.clone(), &StreamResponse::Task(sample_task("t-1")))
            .await;
        peer.result(id.clone(), &StreamResponse::Task(sample_task("t-1")))
            .await;
        peer.send(ServerMessage::StreamEnd { id }).await;

        assert!(matches!(
            stream.next().await,
            Some(Ok(StreamResponse::Task(_)))
        ));
        assert!(matches!(
            stream.next().await,
            Some(Ok(StreamResponse::Task(_)))
        ));
        assert!(
            stream.next().await.is_none(),
            "streamEnd must end the stream"
        );
    }

    #[tokio::test]
    async fn an_error_terminates_a_stream_without_a_stream_end() {
        // 7.3 and 8.3: the error is the last item, and none follows it.
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let (mut stream, id) = open_stream(&transport, &mut peer).await;

        peer.result(id.clone(), &StreamResponse::Task(sample_task("t-1")))
            .await;
        peer.send(ServerMessage::Error {
            id,
            error: A2AError::internal("boom").to_jsonrpc_error(),
        })
        .await;

        assert!(matches!(stream.next().await, Some(Ok(_))));
        let err = stream.next().await.expect("error item").unwrap_err();
        assert_eq!(err.code, error_code::INTERNAL_ERROR);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn dropping_a_stream_early_cancels_it() {
        // 8.5: stop this stream only, and do not touch the task behind it.
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let (stream, id) = open_stream(&transport, &mut peer).await;
        drop(stream);

        match peer.recv().await {
            ServerInbound::Message(ClientMessage::CancelStream { id: cancelled }) => {
                assert_eq!(cancelled, id);
            }
            other => panic!("expected cancelStream, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_stream_that_ended_is_not_cancelled() {
        // Cancelling here would name an id the agent has already forgotten.
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let (mut stream, id) = open_stream(&transport, &mut peer).await;
        peer.send(ServerMessage::StreamEnd { id }).await;
        assert!(stream.next().await.is_none());
        drop(stream);

        // `destroy` closes STDIN, so EOF proves nothing else was written.
        let (result, ()) = tokio::join!(transport.destroy(), async {
            match peer.recv().await {
                ServerInbound::Message(ClientMessage::Request(r)) => {
                    assert_eq!(r.method, json::SYSTEM_SHUTDOWN);
                }
                other => panic!("expected only system/shutdown, got {other:?}"),
            }
            assert!(peer.reader.next().await.is_none(), "STDIN should be closed");
        });
        result.unwrap();
    }

    // ---- session end (2.4, 8.3) ---------------------------------------

    #[tokio::test]
    async fn losing_the_agent_fails_a_waiting_call() {
        let (transport, peer) = connected(ClientConfig::default()).await;
        let (params, req) = (ServiceParams::new(), get_task("t-1"));
        let call = transport.get_task(&params, &req);
        let agent = async {
            let mut peer = peer;
            peer.recv_request().await;
            // The agent exits without answering, which the client sees as EOF.
            drop(peer);
        };
        let (result, ()) = tokio::join!(call, agent);
        let err = result.unwrap_err();
        assert!(err.message.contains("closed STDOUT"), "{}", err.message);
    }

    #[tokio::test]
    async fn losing_the_agent_ends_an_open_stream() {
        // 8.3: process exit terminates every active stream.
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let (mut stream, _) = open_stream(&transport, &mut peer).await;
        drop(peer);

        let err = stream.next().await.expect("an ending reason").unwrap_err();
        assert!(err.message.contains("closed STDOUT"), "{}", err.message);
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn destroy_asks_politely_then_closes_stdin() {
        // 2.4: `system/shutdown` first, then EOF, which is the part that binds.
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let agent = async {
            match peer.recv().await {
                ServerInbound::Message(ClientMessage::Request(r)) => {
                    assert_eq!(r.method, json::SYSTEM_SHUTDOWN);
                }
                other => panic!("expected system/shutdown, got {other:?}"),
            }
            assert!(peer.reader.next().await.is_none(), "STDIN should be closed");
        };
        let (result, ()) = tokio::join!(transport.destroy(), agent);
        result.unwrap();
    }

    #[tokio::test]
    async fn destroy_can_skip_the_shutdown_request() {
        let config = ClientConfig {
            send_shutdown_request: false,
            ..ClientConfig::default()
        };
        let (transport, mut peer) = connected(config).await;
        let (result, ()) = tokio::join!(transport.destroy(), async {
            assert!(peer.reader.next().await.is_none(), "expected only EOF");
        });
        result.unwrap();
    }

    #[tokio::test]
    async fn calls_after_destroy_fail_instead_of_hanging() {
        let (transport, mut peer) = connected(ClientConfig::default()).await;
        let (result, ()) = tokio::join!(transport.destroy(), async {
            while peer.reader.next().await.is_some() {}
        });
        result.unwrap();

        let err = transport
            .get_task(&ServiceParams::new(), &get_task("t-1"))
            .await
            .unwrap_err();
        assert!(err.message.contains("closed"), "{}", err.message);
    }

    // ---- launch (2.1, 4.1, 13) ---------------------------------------

    fn explicit_envs(cmd: &Command) -> Vec<(String, String)> {
        cmd.as_std()
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect()
    }

    #[test]
    fn launch_keeps_arguments_as_a_vector() {
        // 13.1: the shell metacharacters below stay one opaque argument,
        // because nothing ever concatenates them into a command line.
        let cmd = Launch::new("/usr/local/bin/agent")
            .arg("--serve")
            .arg("; rm -rf /")
            .build();
        let std = cmd.as_std();
        assert_eq!(std.get_program(), "/usr/local/bin/agent");
        let args: Vec<_> = std.get_args().collect();
        assert_eq!(args, ["--serve", "; rm -rf /"]);
    }

    #[test]
    fn launch_always_passes_a_session_id() {
        // 2.1 makes it REQUIRED, and 2.2 negotiates against it.
        let envs = explicit_envs(&Launch::new("/bin/agent").build());
        let id = envs
            .iter()
            .find(|(k, _)| k == session::ENV_SESSION_ID)
            .map(|(_, v)| v.clone())
            .expect("A2A_SESSION_ID missing");
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "not a uuid: {id}");
    }

    #[test]
    fn launch_exports_service_params_the_agent_can_read_back() {
        // 4.1, checked against the server-side parser rather than a literal.
        let mut params = ServiceParams::new();
        params.insert("a2a-extensions".into(), vec!["a".into(), "b".into()]);
        params.insert("a2a-version".into(), vec!["1.0".into()]);

        let cmd = Launch::new("/bin/agent").service_params(&params).build();
        let parsed = session::service_params_from_env(explicit_envs(&cmd));
        assert_eq!(parsed["a2a-extensions"], vec!["a", "b"]);
        assert_eq!(parsed["a2a-version"], vec!["1.0"]);
    }

    #[test]
    fn launch_sets_the_optional_startup_context() {
        let cmd = Launch::new("/bin/agent")
            .protocol_version("1.0")
            .log_level("debug")
            .build();
        let envs = explicit_envs(&cmd);
        assert!(envs.contains(&(session::ENV_PROTOCOL_VERSION.into(), "1.0".into())));
        assert!(envs.contains(&(session::ENV_LOG_LEVEL.into(), "debug".into())));
    }

    #[test]
    fn each_launch_gets_its_own_session_id() {
        let a = Launch::new("/bin/agent").session_id("fixed");
        assert_eq!(a.session_id, "fixed");
        assert_ne!(
            Launch::new("/bin/agent").session_id,
            Launch::new("/bin/agent").session_id
        );
    }
}
