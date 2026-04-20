// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Test-only subprocess helper for `agntcy-a2a-stdio` integration tests.
//!
//! This binary is spawned by tests in `tests/spawned.rs` to exercise paths
//! that require a real child process. It is not part of the public API.
//!
//! Modes (selected via the first CLI argument):
//!
//! - `hang-no-handshake` — block forever; never write the handshake.
//! - `handshake-then-hang` — write handshake, read ack, then block forever
//!   (ignores stdin EOF so `child.wait()` hangs).
//! - `full-echo` — run a `StdioServer` that implements all 11
//!   `RequestHandler` methods with canned data. `send_message` sleeps
//!   `STDIO_TEST_DELAY_MS` ms (default 200) before responding so the
//!   request-serialization test can race two calls.
//! - `bad-response` — manually write a JSON-RPC response whose `result`
//!   cannot be deserialized into the expected type. Used to exercise the
//!   client-side `deserialize result` error branch.
//! - `stream-bad-frame` — handshake, then on the first request write a
//!   non-JSON byte payload as a single frame. Exercises the streaming
//!   `parse frame` branch in the client background reader.
//! - `stream-bad-notification` — handshake, then on the first request write
//!   one streaming notification whose `params` cannot be parsed as a
//!   `StreamResponse`, followed by a valid final response. Exercises the
//!   streaming `parse streaming event` branch.
//! - `stream-bad-final` — handshake, then on the first request write a
//!   final JSON-RPC response (with id) whose `result` is not a
//!   `StreamResponse`. Exercises the streaming `failed to parse final
//!   stream response` branch.

use std::env;
use std::sync::Arc;
use std::time::Duration;

use a2a::event::{StreamResponse, TaskStatusUpdateEvent};
use a2a::*;
use a2a_server::RequestHandler;
use a2a_server::middleware::ServiceParams;
use a2a_stdio::framing;
use a2a_stdio::handshake::{Handshake, read_handshake_ack};
use a2a_stdio::{handshake, serve};
use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use tokio::io::{BufReader, stdin, stdout};
use tokio::time::sleep;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mode = env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "hang-no-handshake" => hang_no_handshake().await,
        "handshake-then-hang" => handshake_then_hang().await,
        "full-echo" => full_echo().await,
        "bad-response" => bad_response().await,
        "stream-bad-frame" => stream_bad_frame().await,
        "stream-bad-notification" => stream_bad_notification().await,
        "stream-bad-final" => stream_bad_final().await,
        other => {
            eprintln!("stdio_test_helper: unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}

async fn hang_no_handshake() {
    std::future::pending::<()>().await;
}

async fn handshake_then_hang() {
    let mut writer = stdout();
    let mut reader = BufReader::new(stdin());

    let hs = Handshake::new("test-session".to_string(), vec!["a2a/v1".to_string()]);
    handshake::write_handshake(&mut writer, &hs).await.unwrap();
    let _ = read_handshake_ack(&mut reader).await;

    std::future::pending::<()>().await;
}

async fn full_echo() {
    let delay_ms: u64 = env::var("STDIO_TEST_DELAY_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    let handler = Arc::new(EchoHandler {
        send_delay: Duration::from_millis(delay_ms),
    });
    // Drive the server through the public `serve()` convenience function so
    // that path is covered by the spawned-subprocess tests.
    let _ = serve(handler).await;
}

/// Performs the handshake, reads one request, then writes a single frame
/// containing non-JSON bytes. The client streaming reader hits the
/// `parse frame` error branch.
async fn stream_bad_frame() {
    let mut writer = stdout();
    let mut reader = BufReader::new(stdin());

    let hs = Handshake::new("test-session".to_string(), vec!["a2a/v1".to_string()]);
    handshake::write_handshake(&mut writer, &hs).await.unwrap();
    let _ = read_handshake_ack(&mut reader).await;

    let _ = framing::read_frame(&mut reader).await.unwrap().unwrap();
    framing::write_frame(&mut writer, b"not-json-at-all")
        .await
        .unwrap();
}

/// Performs the handshake, reads one request, then writes one streaming
/// notification whose `params` cannot be parsed as a `StreamResponse`,
/// followed by a valid final response. Exercises the
/// `parse streaming event` branch in the client background reader.
async fn stream_bad_notification() {
    let mut writer = stdout();
    let mut reader = BufReader::new(stdin());

    let hs = Handshake::new("test-session".to_string(), vec!["a2a/v1".to_string()]);
    handshake::write_handshake(&mut writer, &hs).await.unwrap();
    let _ = read_handshake_ack(&mut reader).await;

    let frame = framing::read_frame(&mut reader).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&frame).unwrap();
    let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);

    // Streaming notification (no id) with garbage params.
    let notif = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "stream/event",
        "params": "not-a-stream-response",
    });
    framing::write_frame(&mut writer, &serde_json::to_vec(&notif).unwrap())
        .await
        .unwrap();

    // Final response that ends the stream cleanly.
    let final_resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": serde_json::Value::Null,
    });
    framing::write_frame(&mut writer, &serde_json::to_vec(&final_resp).unwrap())
        .await
        .unwrap();
}

/// Performs the handshake, reads one request, then writes a final JSON-RPC
/// response (with id) whose `result` is not a valid `StreamResponse`.
/// Exercises the `failed to parse final stream response` branch.
async fn stream_bad_final() {
    let mut writer = stdout();
    let mut reader = BufReader::new(stdin());

    let hs = Handshake::new("test-session".to_string(), vec!["a2a/v1".to_string()]);
    handshake::write_handshake(&mut writer, &hs).await.unwrap();
    let _ = read_handshake_ack(&mut reader).await;

    let frame = framing::read_frame(&mut reader).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&frame).unwrap();
    let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);

    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": "not-a-stream-response",
    });
    framing::write_frame(&mut writer, &serde_json::to_vec(&resp).unwrap())
        .await
        .unwrap();
}

/// Performs the handshake, then on the first request writes back a JSON-RPC
/// response whose `result` is a JSON string instead of the expected object.
/// Exercises the client's `deserialize result` error path.
async fn bad_response() {
    let mut writer = stdout();
    let mut reader = BufReader::new(stdin());

    let hs = Handshake::new("test-session".to_string(), vec!["a2a/v1".to_string()]);
    handshake::write_handshake(&mut writer, &hs).await.unwrap();
    let _ = read_handshake_ack(&mut reader).await;

    let frame = framing::read_frame(&mut reader).await.unwrap().unwrap();
    let value: serde_json::Value = serde_json::from_slice(&frame).unwrap();
    let id = value.get("id").cloned().unwrap_or(serde_json::Value::Null);

    let resp = serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": "this-is-not-a-task-object",
    });
    let body = serde_json::to_vec(&resp).unwrap();
    framing::write_frame(&mut writer, &body).await.unwrap();
}

// ---------------------------------------------------------------------------
// EchoHandler — canned responses for all 11 RequestHandler methods.
// ---------------------------------------------------------------------------

struct EchoHandler {
    send_delay: Duration,
}

fn echo_task(id: &str, state: TaskState) -> Task {
    Task {
        id: id.to_string(),
        context_id: "ctx".to_string(),
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        artifacts: None,
        history: None,
        metadata: None,
    }
}

#[async_trait]
impl RequestHandler for EchoHandler {
    async fn send_message(
        &self,
        _params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        sleep(self.send_delay).await;
        let task_id = req
            .message
            .task_id
            .clone()
            .unwrap_or_else(|| "unknown".to_string());
        Ok(SendMessageResponse::Task(echo_task(
            &task_id,
            TaskState::Completed,
        )))
    }

    async fn send_streaming_message(
        &self,
        _params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let task_id = req
            .message
            .task_id
            .clone()
            .unwrap_or_else(|| "stream-task".to_string());
        Ok(Box::pin(stream::iter(vec![
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: task_id.clone(),
                context_id: "ctx".to_string(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            })),
            Ok(StreamResponse::Task(echo_task(
                &task_id,
                TaskState::Completed,
            ))),
        ])))
    }

    async fn get_task(
        &self,
        _params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        if req.id == "missing" {
            return Err(A2AError::task_not_found(&req.id));
        }
        Ok(echo_task(&req.id, TaskState::Completed))
    }

    async fn list_tasks(
        &self,
        _params: &ServiceParams,
        _req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        Ok(ListTasksResponse {
            tasks: vec![echo_task("task-1", TaskState::Completed)],
            next_page_token: "next".to_string(),
            page_size: 1,
            total_size: 1,
        })
    }

    async fn cancel_task(
        &self,
        _params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        Ok(echo_task(&req.id, TaskState::Canceled))
    }

    async fn subscribe_to_task(
        &self,
        _params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        Ok(Box::pin(stream::iter(vec![
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: req.id.clone(),
                context_id: "ctx".to_string(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            })),
            Ok(StreamResponse::Task(echo_task(
                &req.id,
                TaskState::Completed,
            ))),
        ])))
    }

    async fn create_push_config(
        &self,
        _params: &ServiceParams,
        req: CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Ok(TaskPushNotificationConfig {
            task_id: req.task_id,
            tenant: req.tenant,
            config: req.config,
        })
    }

    async fn get_push_config(
        &self,
        _params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Ok(TaskPushNotificationConfig {
            task_id: req.task_id,
            tenant: req.tenant,
            config: PushNotificationConfig {
                url: "https://example.com/cb".to_string(),
                id: Some(req.id),
                token: None,
                authentication: None,
            },
        })
    }

    async fn list_push_configs(
        &self,
        _params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        Ok(ListTaskPushNotificationConfigsResponse {
            configs: vec![TaskPushNotificationConfig {
                task_id: req.task_id,
                tenant: req.tenant,
                config: PushNotificationConfig {
                    url: "https://example.com/cb".to_string(),
                    id: Some("cfg-1".to_string()),
                    token: None,
                    authentication: None,
                },
            }],
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
        Ok(AgentCard {
            name: "echo-agent".to_string(),
            description: "test".to_string(),
            version: "0.0.1".to_string(),
            supported_interfaces: vec![],
            capabilities: AgentCapabilities::default(),
            default_input_modes: vec!["text/plain".to_string()],
            default_output_modes: vec!["text/plain".to_string()],
            skills: vec![],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        })
    }
}
