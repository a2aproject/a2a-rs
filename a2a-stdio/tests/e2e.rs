// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! End-to-end tests for the STDIO transport.
//!
//! These tests wire the `StdioServer` to an in-process `tokio::io::duplex`
//! pair so we can exercise the full request→dispatch→response path without
//! spawning a real subprocess.

use std::sync::Arc;

use a2a::event::{StreamResponse, TaskStatusUpdateEvent};
use a2a::*;
use a2a_server::RequestHandler;
use a2a_server::middleware::ServiceParams;
use a2a_stdio::StdioServer;
use a2a_stdio::errors::StdioError;
use a2a_stdio::framing;
use a2a_stdio::handshake::{self, HandshakeAck};
use async_trait::async_trait;
use futures::stream::{self, BoxStream};
use tokio::io::BufReader;

// ---------------------------------------------------------------------------
// Helpers (same canned data used across the workspace's e2e tests)
// ---------------------------------------------------------------------------

fn sample_message(role: Role, text: &str, task_id: &str) -> Message {
    Message {
        message_id: format!("msg-{text}"),
        context_id: Some("ctx-1".to_string()),
        task_id: Some(task_id.to_string()),
        role,
        parts: vec![Part::text(text)],
        metadata: None,
        extensions: None,
        reference_task_ids: None,
    }
}

fn sample_task(id: &str, state: TaskState) -> Task {
    Task {
        id: id.to_string(),
        context_id: "ctx-1".to_string(),
        status: TaskStatus {
            state,
            message: Some(sample_message(Role::Agent, "done", id)),
            timestamp: None,
        },
        artifacts: None,
        history: None,
        metadata: None,
    }
}

fn sample_push_config(id: &str) -> TaskPushNotificationConfig {
    TaskPushNotificationConfig {
        task_id: "task-1".to_string(),
        tenant: None,
        config: PushNotificationConfig {
            url: "https://example.com/callback".to_string(),
            id: Some(id.to_string()),
            token: Some("tok-1".to_string()),
            authentication: None,
        },
    }
}

fn sample_agent_card() -> AgentCard {
    AgentCard {
        name: "STDIO Test Agent".to_string(),
        description: "Integration test agent".to_string(),
        version: VERSION.to_string(),
        supported_interfaces: vec![AgentInterface::new(
            "stdio://test-agent",
            TRANSPORT_PROTOCOL_STDIO,
        )],
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
    }
}

fn send_message_request(task_id: &str) -> SendMessageRequest {
    SendMessageRequest {
        message: sample_message(Role::User, "hello", task_id),
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

// ---------------------------------------------------------------------------
// TestHandler — implements all 11 RequestHandler methods with canned data
// ---------------------------------------------------------------------------

struct TestHandler;

#[async_trait]
impl RequestHandler for TestHandler {
    async fn send_message(
        &self,
        _params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let task_id = req
            .message
            .task_id
            .clone()
            .unwrap_or_else(|| "task-1".to_string());
        Ok(SendMessageResponse::Task(sample_task(
            &task_id,
            TaskState::Completed,
        )))
    }

    async fn send_streaming_message(
        &self,
        _params: &ServiceParams,
        _req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        Ok(Box::pin(stream::iter(vec![
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: "task-1".to_string(),
                context_id: "ctx-1".to_string(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            })),
            Ok(StreamResponse::Task(sample_task(
                "task-1",
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
        Ok(sample_task(&req.id, TaskState::Completed))
    }

    async fn list_tasks(
        &self,
        _params: &ServiceParams,
        _req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        Ok(ListTasksResponse {
            tasks: vec![sample_task("task-1", TaskState::Completed)],
            next_page_token: "next-page".to_string(),
            page_size: 1,
            total_size: 1,
        })
    }

    async fn cancel_task(
        &self,
        _params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        if req.id == "missing" {
            return Err(A2AError::task_not_found(&req.id));
        }
        Ok(sample_task(&req.id, TaskState::Canceled))
    }

    async fn subscribe_to_task(
        &self,
        _params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        if req.id == "missing" {
            return Err(A2AError::task_not_found(&req.id));
        }
        Ok(Box::pin(stream::once(async move {
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: req.id,
                context_id: "ctx-1".to_string(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            }))
        })))
    }

    async fn create_push_config(
        &self,
        _params: &ServiceParams,
        req: CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        Ok(TaskPushNotificationConfig {
            task_id: req.task_id,
            config: req.config,
            tenant: req.tenant,
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
                url: "https://example.com/callback".to_string(),
                id: Some(req.id),
                token: Some("tok-1".to_string()),
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
                ..sample_push_config("cfg-1")
            }],
            next_page_token: Some("next".to_string()),
        })
    }

    async fn delete_push_config(
        &self,
        _params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        if req.id == "missing" {
            return Err(A2AError::task_not_found(&req.id));
        }
        Ok(())
    }

    async fn get_extended_agent_card(
        &self,
        _params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        Ok(sample_agent_card())
    }
}

// ---------------------------------------------------------------------------
// Test client helper — speaks the STDIO protocol over raw reader/writer
// ---------------------------------------------------------------------------

/// A minimal test client that performs the handshake and sends/receives
/// JSON-RPC frames. This avoids depending on `StdioTransport` (which
/// requires a real subprocess) and lets us test the server in-process.
struct TestClient<R, W> {
    reader: BufReader<R>,
    writer: W,
}

impl<R, W> TestClient<R, W>
where
    R: tokio::io::AsyncRead + Unpin + Send,
    W: tokio::io::AsyncWrite + Unpin + Send,
{
    /// Connect by reading the server's handshake and sending an ack.
    async fn connect(reader: R, writer: W) -> Result<Self, StdioError> {
        let mut reader = BufReader::new(reader);
        let mut writer = writer;

        // Read server's handshake.
        let hs = handshake::read_handshake(&mut reader).await?;
        assert_eq!(hs.msg_type, "handshake");
        assert!(hs.supported_variants.contains(&"a2a/v1".to_string()));

        // Send ack.
        let ack = HandshakeAck::accept("a2a/v1".to_string());
        handshake::write_handshake_ack(&mut writer, &ack).await?;

        Ok(Self { reader, writer })
    }

    /// Send a JSON-RPC request and read a single response frame.
    async fn call(&mut self, method: &str, params: serde_json::Value) -> JsonRpcResponse {
        let id = JsonRpcId::String(uuid::Uuid::now_v7().to_string());
        let request = JsonRpcRequest::new(id, method, Some(params));

        let body = serde_json::to_vec(&request).unwrap();
        framing::write_frame(&mut self.writer, &body).await.unwrap();

        let frame = framing::read_frame(&mut self.reader)
            .await
            .unwrap()
            .expect("expected response frame");
        serde_json::from_slice(&frame).unwrap()
    }

    /// Send a JSON-RPC request and collect all notification frames
    /// until a response with an `id` arrives (the final frame).
    async fn call_streaming(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> (Vec<serde_json::Value>, JsonRpcResponse) {
        let id = JsonRpcId::String(uuid::Uuid::now_v7().to_string());
        let request = JsonRpcRequest::new(id, method, Some(params));

        let body = serde_json::to_vec(&request).unwrap();
        framing::write_frame(&mut self.writer, &body).await.unwrap();

        let mut notifications = Vec::new();
        loop {
            let frame = framing::read_frame(&mut self.reader)
                .await
                .unwrap()
                .expect("expected frame");
            let value: serde_json::Value = serde_json::from_slice(&frame).unwrap();

            // Notifications have "method" but no "id".
            // The final response has "id".
            if value.get("id").is_some() {
                let resp: JsonRpcResponse = serde_json::from_value(value).unwrap();
                return (notifications, resp);
            }
            notifications.push(value);
        }
    }
}

// ---------------------------------------------------------------------------
// Helper to start a server + client pair over duplex channels
// ---------------------------------------------------------------------------

async fn setup() -> (
    tokio::task::JoinHandle<Result<(), StdioError>>,
    TestClient<tokio::io::DuplexStream, tokio::io::DuplexStream>,
) {
    // duplex(1) = "server_to_client": server writes, client reads
    // duplex(2) = "client_to_server": client writes, server reads
    let (server_writer, client_reader) = tokio::io::duplex(64 * 1024);
    let (client_writer, server_reader) = tokio::io::duplex(64 * 1024);

    let handler = Arc::new(TestHandler);
    let server = StdioServer::new(handler);

    let server_handle = tokio::spawn(async move { server.run(server_reader, server_writer).await });

    let client = TestClient::connect(client_reader, client_writer)
        .await
        .expect("handshake should succeed");

    (server_handle, client)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stdio_unary_send_message() {
    let (_server, mut client) = setup().await;

    let params = serde_json::to_value(send_message_request("task-1")).unwrap();
    let resp = client.call("message/send", params).await;

    assert!(resp.error.is_none(), "expected success: {:?}", resp.error);
    let result: SendMessageResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
    match result {
        SendMessageResponse::Task(t) => {
            assert_eq!(t.id, "task-1");
            assert_eq!(t.status.state, TaskState::Completed);
        }
        other => panic!("expected Task variant, got: {other:?}"),
    }
}

#[tokio::test]
async fn stdio_unary_get_task() {
    let (_server, mut client) = setup().await;

    let params = serde_json::to_value(&GetTaskRequest {
        id: "task-42".to_string(),
        history_length: None,
        tenant: None,
    })
    .unwrap();
    let resp = client.call("tasks/get", params).await;

    assert!(resp.error.is_none());
    let task: Task = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(task.id, "task-42");
}

#[tokio::test]
async fn stdio_unary_get_task_not_found() {
    let (_server, mut client) = setup().await;

    let params = serde_json::to_value(&GetTaskRequest {
        id: "missing".to_string(),
        history_length: None,
        tenant: None,
    })
    .unwrap();
    let resp = client.call("tasks/get", params).await;

    assert!(resp.error.is_some());
    let err = resp.error.unwrap();
    assert_eq!(err.code, error_code::TASK_NOT_FOUND);
}

#[tokio::test]
async fn stdio_unary_list_tasks() {
    let (_server, mut client) = setup().await;

    let params = serde_json::to_value(&ListTasksRequest {
        context_id: Some("ctx-1".to_string()),
        status: None,
        page_size: None,
        page_token: None,
        history_length: None,
        status_timestamp_after: None,
        include_artifacts: None,
        tenant: None,
    })
    .unwrap();
    let resp = client.call("tasks/list", params).await;

    assert!(resp.error.is_none());
    let list: ListTasksResponse = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(list.tasks.len(), 1);
    assert_eq!(list.tasks[0].id, "task-1");
}

#[tokio::test]
async fn stdio_unary_cancel_task() {
    let (_server, mut client) = setup().await;

    let params = serde_json::to_value(&CancelTaskRequest {
        id: "task-1".to_string(),
        metadata: None,
        tenant: None,
    })
    .unwrap();
    let resp = client.call("tasks/cancel", params).await;

    assert!(resp.error.is_none());
    let task: Task = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(task.status.state, TaskState::Canceled);
}

#[tokio::test]
async fn stdio_unary_push_config_crud() {
    let (_server, mut client) = setup().await;

    // Create
    let params = serde_json::to_value(&CreateTaskPushNotificationConfigRequest {
        task_id: "task-1".to_string(),
        config: sample_push_config("cfg-1").config,
        tenant: None,
    })
    .unwrap();
    let resp = client
        .call("tasks/pushNotificationConfig/create", params)
        .await;
    assert!(resp.error.is_none());
    let cfg: TaskPushNotificationConfig = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(cfg.task_id, "task-1");

    // Get
    let params = serde_json::to_value(&GetTaskPushNotificationConfigRequest {
        task_id: "task-1".to_string(),
        id: "cfg-1".to_string(),
        tenant: None,
    })
    .unwrap();
    let resp = client
        .call("tasks/pushNotificationConfig/get", params)
        .await;
    assert!(resp.error.is_none());

    // List
    let params = serde_json::to_value(&ListTaskPushNotificationConfigsRequest {
        task_id: "task-1".to_string(),
        page_size: None,
        page_token: None,
        tenant: None,
    })
    .unwrap();
    let resp = client
        .call("tasks/pushNotificationConfig/list", params)
        .await;
    assert!(resp.error.is_none());
    let list: ListTaskPushNotificationConfigsResponse =
        serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(list.configs.len(), 1);

    // Delete
    let params = serde_json::to_value(&DeleteTaskPushNotificationConfigRequest {
        task_id: "task-1".to_string(),
        id: "cfg-1".to_string(),
        tenant: None,
    })
    .unwrap();
    let resp = client
        .call("tasks/pushNotificationConfig/delete", params)
        .await;
    assert!(resp.error.is_none());
}

#[tokio::test]
async fn stdio_unary_get_extended_agent_card() {
    let (_server, mut client) = setup().await;

    let params = serde_json::to_value(&GetExtendedAgentCardRequest { tenant: None }).unwrap();
    let resp = client.call("agent/extendedCard", params).await;

    assert!(resp.error.is_none());
    let card: AgentCard = serde_json::from_value(resp.result.unwrap()).unwrap();
    assert_eq!(card.name, "STDIO Test Agent");
}

#[tokio::test]
async fn stdio_streaming_send_message() {
    let (_server, mut client) = setup().await;

    let params = serde_json::to_value(send_message_request("task-1")).unwrap();
    let (notifications, final_resp) = client.call_streaming("message/stream", params).await;

    // We expect 2 notifications (Working + Completed) then a final success.
    assert_eq!(notifications.len(), 2, "expected 2 stream events");

    // First event should be a status update (Working).
    let first_method = notifications[0]["method"].as_str().unwrap();
    assert_eq!(first_method, "event/streamResponse");

    // Final response should be success.
    assert!(final_resp.error.is_none());
}

#[tokio::test]
async fn stdio_streaming_subscribe_to_task() {
    let (_server, mut client) = setup().await;

    let params = serde_json::to_value(&SubscribeToTaskRequest {
        id: "task-1".to_string(),
        tenant: None,
    })
    .unwrap();
    let (notifications, final_resp) = client.call_streaming("tasks/subscribe", params).await;

    assert_eq!(notifications.len(), 1);
    assert!(final_resp.error.is_none());
}

#[tokio::test]
async fn stdio_unknown_method_returns_error() {
    let (_server, mut client) = setup().await;

    let resp = client.call("unknown/method", serde_json::json!({})).await;

    assert!(resp.error.is_some());
    let err = resp.error.unwrap();
    assert_eq!(err.code, error_code::METHOD_NOT_FOUND);
    assert!(err.message.contains("unknown/method"));
}

#[tokio::test]
async fn stdio_invalid_params_returns_error() {
    let (_server, mut client) = setup().await;

    // Send garbage params to a typed method.
    let resp = client
        .call("tasks/get", serde_json::json!({"wrong_field": true}))
        .await;

    assert!(resp.error.is_some());
    let err = resp.error.unwrap();
    assert_eq!(err.code, error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn stdio_server_exits_on_eof() {
    let (server_writer, client_reader) = tokio::io::duplex(64 * 1024);
    let (client_writer, server_reader) = tokio::io::duplex(64 * 1024);

    let handler = Arc::new(TestHandler);
    let server = StdioServer::new(handler);

    let server_handle = tokio::spawn(async move { server.run(server_reader, server_writer).await });

    // Complete the handshake, then immediately drop the client.
    {
        let _client = TestClient::connect(client_reader, client_writer)
            .await
            .expect("handshake should succeed");
    }
    // client is dropped here — server should see EOF and return Ok(()).

    let result = server_handle.await.unwrap();
    assert!(
        result.is_ok(),
        "server should exit cleanly on EOF: {result:?}"
    );
}

#[tokio::test]
async fn stdio_handshake_reject() {
    let (server_writer, client_reader) = tokio::io::duplex(64 * 1024);
    let (client_writer, server_reader) = tokio::io::duplex(64 * 1024);

    let handler = Arc::new(TestHandler);
    let server = StdioServer::new(handler);

    let server_handle = tokio::spawn(async move { server.run(server_reader, server_writer).await });

    // Read the server's handshake, then send a reject ack.
    let mut reader = BufReader::new(client_reader);
    let mut writer = client_writer;

    let _hs = handshake::read_handshake(&mut reader).await.unwrap();
    let ack = HandshakeAck {
        msg_type: "handshakeAck".to_string(),
        selected_variant: String::new(),
        accept: false,
    };
    handshake::write_handshake_ack(&mut writer, &ack)
        .await
        .unwrap();

    let result = server_handle.await.unwrap();
    assert!(result.is_err(), "server should fail when client rejects");
    match result.unwrap_err() {
        StdioError::HandshakeFailed(msg) => {
            assert!(msg.contains("rejected"), "msg: {msg}");
        }
        other => panic!("expected HandshakeFailed, got: {other:?}"),
    }
}

#[tokio::test]
async fn stdio_multiple_requests_in_sequence() {
    let (_server, mut client) = setup().await;

    // Send multiple requests on the same connection.
    for i in 0..5 {
        let task_id = format!("task-{i}");
        let params = serde_json::to_value(&GetTaskRequest {
            id: task_id.clone(),
            history_length: None,
            tenant: None,
        })
        .unwrap();

        let resp = client.call("tasks/get", params).await;
        assert!(resp.error.is_none());
        let task: Task = serde_json::from_value(resp.result.unwrap()).unwrap();
        assert_eq!(task.id, task_id);
    }
}

// ---------------------------------------------------------------------------
// Server-side coverage: error branches in the request loop and dispatchers
// ---------------------------------------------------------------------------

/// Send a raw frame to the server bypassing TestClient::call, then read the
/// next response frame back. Returns the parsed JsonRpcResponse.
async fn send_raw_and_read(
    client: &mut TestClient<tokio::io::DuplexStream, tokio::io::DuplexStream>,
    frame: &[u8],
) -> JsonRpcResponse {
    framing::write_frame(&mut client.writer, frame)
        .await
        .unwrap();
    let resp_frame = framing::read_frame(&mut client.reader)
        .await
        .unwrap()
        .expect("expected response frame");
    serde_json::from_slice(&resp_frame).unwrap()
}

#[tokio::test]
async fn stdio_server_replies_parse_error_for_malformed_json() {
    let (_server, mut client) = setup().await;

    // Body that frames cleanly but is not valid JSON.
    let resp = send_raw_and_read(&mut client, b"not-json-at-all").await;
    let err = resp.error.expect("server should send a JSON-RPC error");
    assert_eq!(err.code, error_code::PARSE_ERROR);
    assert!(err.message.to_lowercase().contains("invalid json"));
}

#[tokio::test]
async fn stdio_server_replies_parse_error_for_non_jsonrpc_object() {
    let (_server, mut client) = setup().await;

    // Valid JSON object that does not deserialize into JsonRpcRequest.
    let resp = send_raw_and_read(&mut client, br#"{"hello":"world"}"#).await;
    let err = resp.error.expect("server should send a JSON-RPC error");
    assert_eq!(err.code, error_code::PARSE_ERROR);
    assert!(err.message.to_lowercase().contains("invalid request"));
}

#[tokio::test]
async fn stdio_server_recovers_after_malformed_json() {
    let (_server, mut client) = setup().await;

    // First frame: garbage. Second frame: a valid request. The server must
    // continue serving after the parse-error response instead of exiting.
    let resp1 = send_raw_and_read(&mut client, b"<not json>").await;
    assert_eq!(resp1.error.unwrap().code, error_code::PARSE_ERROR);

    let params = serde_json::to_value(&GetTaskRequest {
        id: "task-after-garbage".to_string(),
        history_length: None,
        tenant: None,
    })
    .unwrap();
    let resp2 = client.call("tasks/get", params).await;
    assert!(resp2.error.is_none(), "server should still serve requests");
    let task: Task = serde_json::from_value(resp2.result.unwrap()).unwrap();
    assert_eq!(task.id, "task-after-garbage");
}

#[tokio::test]
async fn stdio_streaming_invalid_params_returns_error() {
    let (_server, mut client) = setup().await;

    // Hits the dispatch_streaming! invalid-params branch.
    let resp = client
        .call("message/stream", serde_json::json!({"foo": 1}))
        .await;
    let err = resp.error.expect("expected INVALID_PARAMS");
    assert_eq!(err.code, error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn stdio_subscribe_invalid_params_returns_error() {
    let (_server, mut client) = setup().await;

    let resp = client.call("tasks/subscribe", serde_json::json!({})).await;
    let err = resp.error.expect("expected INVALID_PARAMS");
    assert_eq!(err.code, error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn stdio_subscribe_handler_error_returns_error_response() {
    // Hits the dispatch_streaming! handler-error branch (TestHandler returns
    // task_not_found for id == "missing").
    let (_server, mut client) = setup().await;

    let params = serde_json::to_value(&SubscribeToTaskRequest {
        id: "missing".to_string(),
        tenant: None,
    })
    .unwrap();
    let resp = client.call("tasks/subscribe", params).await;
    let err = resp.error.expect("expected handler error");
    assert!(err.message.to_lowercase().contains("missing") || err.code != 0);
}

#[tokio::test]
async fn stdio_get_task_handler_error_returns_error_response() {
    // Hits the dispatch_unary! handler-error branch.
    let (_server, mut client) = setup().await;

    let params = serde_json::to_value(&GetTaskRequest {
        id: "missing".to_string(),
        history_length: None,
        tenant: None,
    })
    .unwrap();
    let resp = client.call("tasks/get", params).await;
    let err = resp.error.expect("expected handler error");
    assert!(err.message.to_lowercase().contains("missing") || err.code != 0);
}
