// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! STDIO client transport — spawns a subprocess and communicates over stdin/stdout.

use a2a::event::StreamResponse;
use a2a::*;
use a2a_client::transport::{ServiceParams, Transport, TransportFactory};
use async_trait::async_trait;
use futures::stream::BoxStream;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

use crate::errors::StdioError;
use crate::framing;
use crate::handshake::{self, HandshakeAck};

/// How long to wait for the subprocess to send its initial handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to wait for the subprocess to exit after closing its stdin
/// before forcibly killing it.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------------------
// Stdio method names (slash-separated, per maintainer design doc)
// ---------------------------------------------------------------------------

mod stdio_methods {
    pub const SEND_MESSAGE: &str = "message/send";
    pub const SEND_STREAMING_MESSAGE: &str = "message/stream";
    pub const GET_TASK: &str = "tasks/get";
    pub const LIST_TASKS: &str = "tasks/list";
    pub const CANCEL_TASK: &str = "tasks/cancel";
    pub const SUBSCRIBE_TO_TASK: &str = "tasks/subscribe";
    pub const CREATE_PUSH_CONFIG: &str = "tasks/pushNotificationConfig/create";
    pub const GET_PUSH_CONFIG: &str = "tasks/pushNotificationConfig/get";
    pub const LIST_PUSH_CONFIGS: &str = "tasks/pushNotificationConfig/list";
    pub const DELETE_PUSH_CONFIG: &str = "tasks/pushNotificationConfig/delete";
    pub const GET_EXTENDED_AGENT_CARD: &str = "agent/extendedCard";
}

// ---------------------------------------------------------------------------
// StdioTransport
// ---------------------------------------------------------------------------

/// A transport that communicates with an agent subprocess over stdio pipes.
///
/// The client spawns the child process, performs the handshake, then sends
/// JSON-RPC requests over the child's stdin and reads responses from stdout.
/// A transport that communicates with an agent subprocess over stdio pipes.
///
/// The client spawns the child process, performs the handshake, then sends
/// JSON-RPC requests over the child's stdin and reads responses from stdout.
///
/// # Concurrency
///
/// Each call to [`Transport`] takes a request-level lock that is held across
/// the full write/read cycle. Concurrent calls therefore block rather than
/// race on the underlying pipes — responses are not multiplexed by `id`.
pub struct StdioTransport {
    /// The child process. Held for lifetime management.
    child: Arc<Mutex<Child>>,
    /// Writer to the child's stdin (framed JSON-RPC).
    writer: Arc<Mutex<Box<dyn AsyncWrite + Send + Unpin>>>,
    /// Reader from the child's stdout (framed JSON-RPC).
    reader: Arc<Mutex<BufReader<tokio::process::ChildStdout>>>,
    /// Serializes full request/response cycles so concurrent callers do not
    /// interleave on the underlying pipes.
    request_lock: Arc<Mutex<()>>,
}

impl StdioTransport {
    /// Spawn a subprocess and perform the handshake.
    ///
    /// `program` and `args` define the command to run. An optional
    /// `session_id` is passed via the `A2A_SESSION_ID` environment variable.
    pub async fn spawn(
        program: &str,
        args: &[&str],
        session_id: Option<&str>,
    ) -> Result<Self, StdioError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit());

        if let Some(sid) = session_id {
            cmd.env("A2A_SESSION_ID", sid);
        }

        let mut child = cmd.spawn()?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| StdioError::HandshakeFailed("failed to open child stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| StdioError::HandshakeFailed("failed to open child stdout".into()))?;

        let mut reader = BufReader::new(stdout);
        let mut writer: Box<dyn AsyncWrite + Send + Unpin> = Box::new(stdin);

        // Server sends handshake first. Bound the wait so a hung subprocess
        // does not block the spawn indefinitely.
        let hs =
            match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake::read_handshake(&mut reader))
                .await
            {
                Ok(res) => res?,
                Err(_) => {
                    // Best-effort: kill the subprocess so it does not linger.
                    let _ = child.kill().await;
                    return Err(StdioError::HandshakeFailed(format!(
                        "timed out waiting for handshake after {:?}",
                        HANDSHAKE_TIMEOUT
                    )));
                }
            };

        // Select the first supported variant.
        let selected = hs.supported_variants.first().cloned().unwrap_or_default();

        let ack = HandshakeAck::accept(selected);
        handshake::write_handshake_ack(&mut writer, &ack).await?;

        // Handshake content (selected variant, features) is currently unused
        // beyond the accept/reject decision; drop it.
        let _ = hs;

        Ok(StdioTransport {
            child: Arc::new(Mutex::new(child)),
            writer: Arc::new(Mutex::new(writer)),
            reader: Arc::new(Mutex::new(reader)),
            request_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Send a JSON-RPC request and read the response.
    async fn call<Req: Serialize, Resp: DeserializeOwned>(
        &self,
        method: &str,
        params: &Req,
    ) -> Result<Resp, A2AError> {
        let id = uuid::Uuid::now_v7().to_string();
        let request = JsonRpcRequest::new(
            JsonRpcId::String(id),
            method,
            Some(
                serde_json::to_value(params)
                    .map_err(|e| A2AError::internal(format!("serialize params: {e}")))?,
            ),
        );

        let body = serde_json::to_vec(&request)
            .map_err(|e| A2AError::internal(format!("serialize request: {e}")))?;

        // Hold the request lock across the full write+read cycle so concurrent
        // callers do not interleave on the underlying pipes.
        let _guard = self.request_lock.lock().await;

        // Write request.
        {
            let mut writer = self.writer.lock().await;
            framing::write_frame(&mut *writer, &body)
                .await
                .map_err(|e| A2AError::internal(format!("write frame: {e}")))?;
        }

        // Read response.
        let frame = {
            let mut reader = self.reader.lock().await;
            framing::read_frame(&mut *reader)
                .await
                .map_err(|e| A2AError::internal(format!("read frame: {e}")))?
                .ok_or_else(|| A2AError::internal("subprocess closed stdout unexpectedly"))?
        };

        let rpc_response: JsonRpcResponse = serde_json::from_slice(&frame)
            .map_err(|e| A2AError::internal(format!("parse response: {e}")))?;

        if let Some(err) = rpc_response.error {
            return Err(A2AError::new(err.code, err.message));
        }

        // For methods returning `()` the JSON-RPC response carries
        // `"result": null`, which serde deserializes into `None` for
        // `Option<Value>`. Treat a missing/null result as `Value::Null`
        // so unit responses round-trip correctly while non-null results
        // still flow through normal deserialization.
        let result = rpc_response.result.unwrap_or(serde_json::Value::Null);

        serde_json::from_value(result)
            .map_err(|e| A2AError::internal(format!("deserialize result: {e}")))
    }

    /// Send a JSON-RPC request for a streaming method.
    ///
    /// Returns a true async stream that yields events as they arrive.
    /// Notifications (no `id`) are yielded as streaming events.
    /// The stream ends when the final response (with `id`) is received.
    async fn call_streaming<Req: Serialize>(
        &self,
        method: &str,
        params: &Req,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let id = uuid::Uuid::now_v7().to_string();
        let request = JsonRpcRequest::new(
            JsonRpcId::String(id.clone()),
            method,
            Some(
                serde_json::to_value(params)
                    .map_err(|e| A2AError::internal(format!("serialize params: {e}")))?,
            ),
        );

        let body = serde_json::to_vec(&request)
            .map_err(|e| A2AError::internal(format!("serialize request: {e}")))?;

        // Acquire the request lock for the entire stream lifetime so concurrent
        // callers do not interleave on the underlying pipes. The owned guard is
        // moved into the background task and dropped when the stream ends.
        let request_guard = Arc::clone(&self.request_lock).lock_owned().await;

        // Write request.
        {
            let mut writer = self.writer.lock().await;
            framing::write_frame(&mut *writer, &body)
                .await
                .map_err(|e| A2AError::internal(format!("write frame: {e}")))?;
        }

        // Spawn a background task that reads frames and sends events through
        // a channel, yielding them to the caller as they arrive.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamResponse, A2AError>>(32);
        let reader = Arc::clone(&self.reader);

        tokio::spawn(async move {
            // Tie the lock guard to this task so it is released when the stream
            // terminates (either at the final response or on error).
            let _request_guard = request_guard;
            loop {
                let frame = {
                    let mut r = reader.lock().await;
                    framing::read_frame(&mut *r).await
                };

                let data = match frame {
                    Ok(Some(data)) => data,
                    Ok(None) => break, // EOF
                    Err(e) => {
                        let _ = tx
                            .send(Err(A2AError::internal(format!("read frame: {e}"))))
                            .await;
                        break;
                    }
                };

                let value: serde_json::Value = match serde_json::from_slice(&data) {
                    Ok(v) => v,
                    Err(e) => {
                        let _ = tx
                            .send(Err(A2AError::internal(format!("parse frame: {e}"))))
                            .await;
                        break;
                    }
                };

                // Final response — has a non-null "id" field.
                if value.get("id").is_some() && !value["id"].is_null() {
                    let rpc_response: JsonRpcResponse = match serde_json::from_value(value) {
                        Ok(r) => r,
                        Err(e) => {
                            let _ = tx
                                .send(Err(A2AError::internal(format!("parse response: {e}"))))
                                .await;
                            break;
                        }
                    };

                    if let Some(err) = rpc_response.error {
                        let _ = tx.send(Err(A2AError::new(err.code, err.message))).await;
                    } else if let Some(result) = rpc_response.result {
                        if !result.is_null() {
                            match serde_json::from_value::<StreamResponse>(result) {
                                Ok(sr) => {
                                    if tx.send(Ok(sr)).await.is_err() {
                                        // Receiver dropped, no need to continue.
                                        break;
                                    }
                                }
                                Err(e) => {
                                    let _ = tx
                                        .send(Err(A2AError::internal(format!(
                                            "failed to parse final stream response: {e}"
                                        ))))
                                        .await;
                                }
                            }
                        }
                    }
                    break;
                }

                // Notification (no id) — parse params as StreamResponse.
                if let Some(params_value) = value.get("params") {
                    match serde_json::from_value::<StreamResponse>(params_value.clone()) {
                        Ok(sr) => {
                            if tx.send(Ok(sr)).await.is_err() {
                                break; // Receiver dropped.
                            }
                        }
                        Err(e) => {
                            let _ = tx
                                .send(Err(A2AError::internal(format!(
                                    "parse streaming event: {e}"
                                ))))
                                .await;
                        }
                    }
                }
            }
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send_message(
        &self,
        _params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        self.call(stdio_methods::SEND_MESSAGE, req).await
    }

    async fn send_streaming_message(
        &self,
        _params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.call_streaming(stdio_methods::SEND_STREAMING_MESSAGE, req)
            .await
    }

    async fn get_task(
        &self,
        _params: &ServiceParams,
        req: &GetTaskRequest,
    ) -> Result<Task, A2AError> {
        self.call(stdio_methods::GET_TASK, req).await
    }

    async fn list_tasks(
        &self,
        _params: &ServiceParams,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        self.call(stdio_methods::LIST_TASKS, req).await
    }

    async fn cancel_task(
        &self,
        _params: &ServiceParams,
        req: &CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        self.call(stdio_methods::CANCEL_TASK, req).await
    }

    async fn subscribe_to_task(
        &self,
        _params: &ServiceParams,
        req: &SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.call_streaming(stdio_methods::SUBSCRIBE_TO_TASK, req)
            .await
    }

    async fn create_push_config(
        &self,
        _params: &ServiceParams,
        req: &CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.call(stdio_methods::CREATE_PUSH_CONFIG, req).await
    }

    async fn get_push_config(
        &self,
        _params: &ServiceParams,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.call(stdio_methods::GET_PUSH_CONFIG, req).await
    }

    async fn list_push_configs(
        &self,
        _params: &ServiceParams,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        self.call(stdio_methods::LIST_PUSH_CONFIGS, req).await
    }

    async fn delete_push_config(
        &self,
        _params: &ServiceParams,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        self.call(stdio_methods::DELETE_PUSH_CONFIG, req).await
    }

    async fn get_extended_agent_card(
        &self,
        _params: &ServiceParams,
        req: &GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        self.call(stdio_methods::GET_EXTENDED_AGENT_CARD, req).await
    }

    async fn destroy(&self) -> Result<(), A2AError> {
        // Close stdin to signal the child to shut down.
        {
            let mut writer = self.writer.lock().await;
            writer
                .shutdown()
                .await
                .map_err(|e| A2AError::internal(format!("shutdown stdin: {e}")))?;
        }

        // Wait for the child to exit, but do not block forever — fall back
        // to killing it if it does not exit within `CLOSE_TIMEOUT`.
        let mut child = self.child.lock().await;
        match tokio::time::timeout(CLOSE_TIMEOUT, child.wait()).await {
            Ok(res) => {
                res.map_err(|e| A2AError::internal(format!("wait for child: {e}")))?;
            }
            Err(_) => {
                let _ = child.kill().await;
                // Reap the killed child so it does not become a zombie.
                let _ = child.wait().await;
                return Err(A2AError::internal(format!(
                    "subprocess did not exit within {:?}; killed",
                    CLOSE_TIMEOUT
                )));
            }
        }

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// StdioTransportFactory
// ---------------------------------------------------------------------------

/// Factory that creates [`StdioTransport`] instances.
///
/// The agent card's interface URL is expected to contain the command to spawn,
/// e.g. `stdio://path/to/agent --flag`.
pub struct StdioTransportFactory;

#[async_trait]
impl TransportFactory for StdioTransportFactory {
    fn protocol(&self) -> &str {
        a2a::TRANSPORT_PROTOCOL_STDIO
    }

    async fn create(
        &self,
        _card: &AgentCard,
        iface: &AgentInterface,
    ) -> Result<Box<dyn Transport>, A2AError> {
        // Parse the URL to extract the command.
        // Expected format: "stdio:///path/to/binary?arg1&arg2" or just the path.
        let url = &iface.url;
        let (program, args) = parse_stdio_url(url)?;

        let transport = StdioTransport::spawn(
            &program,
            &args.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            None,
        )
        .await
        .map_err(|e| A2AError::internal(format!("failed to spawn stdio transport: {e}")))?;

        Ok(Box::new(transport))
    }
}

/// Parse a stdio URL into program and args.
///
/// Supports formats:
/// - `stdio:///path/to/binary` → program="/path/to/binary", args=[]
/// - `stdio:///path/to/binary?arg1&arg2` → program="/path/to/binary", args=["arg1", "arg2"]
/// - `/path/to/binary arg1 arg2` → program="/path/to/binary", args=["arg1", "arg2"]
///
/// # Limitations
///
/// Arguments in the `stdio://` form are split on `&` and the path is split on
/// the first `?`. As a result, individual arguments cannot themselves contain
/// the `&` or `?` characters. For commands that need such arguments, use the
/// plain `program arg1 arg2` form instead.
fn parse_stdio_url(url: &str) -> Result<(String, Vec<String>), A2AError> {
    if let Some(rest) = url.strip_prefix("stdio://") {
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        let program = path.to_string();
        let args: Vec<String> = if query.is_empty() {
            vec![]
        } else {
            query.split('&').map(|s| s.to_string()).collect()
        };
        Ok((program, args))
    } else {
        // Plain command string — split on whitespace.
        let parts: Vec<&str> = url.split_whitespace().collect();
        if parts.is_empty() {
            return Err(A2AError::internal("empty stdio command"));
        }
        let program = parts[0].to_string();
        let args = parts[1..].iter().map(|s| s.to_string()).collect();
        Ok((program, args))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stdio_url_with_scheme() {
        let (prog, args) = parse_stdio_url("stdio:///usr/bin/agent").unwrap();
        assert_eq!(prog, "/usr/bin/agent");
        assert!(args.is_empty());
    }

    #[test]
    fn test_parse_stdio_url_with_args() {
        let (prog, args) = parse_stdio_url("stdio:///usr/bin/agent?--port&8080").unwrap();
        assert_eq!(prog, "/usr/bin/agent");
        assert_eq!(args, vec!["--port", "8080"]);
    }

    #[test]
    fn test_parse_stdio_url_plain_command() {
        let (prog, args) = parse_stdio_url("/usr/bin/agent --port 8080").unwrap();
        assert_eq!(prog, "/usr/bin/agent");
        assert_eq!(args, vec!["--port", "8080"]);
    }

    #[test]
    fn test_parse_stdio_url_empty() {
        let err = parse_stdio_url("").unwrap_err();
        assert!(err.message.contains("empty"));
    }
}
