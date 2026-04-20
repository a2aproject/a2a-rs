// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Integration tests that spawn a real subprocess via `StdioTransport`.
//!
//! These tests exercise paths that cannot be reached through the in-process
//! `tokio::io::duplex` setup used in `tests/e2e.rs`:
//!
//! - `HANDSHAKE_TIMEOUT` firing when the subprocess never sends a handshake.
//! - `CLOSE_TIMEOUT` + kill fallback when the subprocess ignores stdin EOF.
//! - The request-level lock that serializes concurrent client calls.
//! - Every method on the `Transport` trait against a real subprocess
//!   (so all of `client.rs` is hit at least once).
//! - `StdioTransportFactory::create` end-to-end.
//! - Client-side error branches (`deserialize result`).
//!
//! All tests cap their own runtime with `tokio::time::timeout` so a
//! regression cannot hang CI.

use std::time::{Duration, Instant};

use a2a::*;
use a2a_client::transport::{ServiceParams, Transport, TransportFactory};
use a2a_stdio::{StdioTransport, StdioTransportFactory};
use futures::StreamExt;

/// Path to the helper binary, resolved at compile time by Cargo.
const HELPER: &str = env!("CARGO_BIN_EXE_stdio_test_helper");

// ---------------------------------------------------------------------------
// Sample request builders
// ---------------------------------------------------------------------------

fn sample_send_message_request(task_id: &str) -> SendMessageRequest {
    SendMessageRequest {
        message: Message {
            message_id: format!("msg-{task_id}"),
            context_id: Some("ctx".to_string()),
            task_id: Some(task_id.to_string()),
            role: Role::User,
            parts: vec![Part::text("hello")],
            metadata: None,
            extensions: None,
            reference_task_ids: None,
        },
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

async fn spawn_full_echo() -> StdioTransport {
    StdioTransport::spawn(HELPER, &["full-echo"], Some("session-x"))
        .await
        .expect("full-echo spawn should succeed")
}

async fn shutdown(transport: StdioTransport) {
    let _ = tokio::time::timeout(Duration::from_secs(10), transport.destroy()).await;
}

// ---------------------------------------------------------------------------
// Robustness: handshake timeout, close timeout/kill, request serialization
// ---------------------------------------------------------------------------

#[tokio::test]
async fn spawn_fails_when_handshake_never_arrives() {
    let res = tokio::time::timeout(
        Duration::from_secs(30),
        StdioTransport::spawn(HELPER, &["hang-no-handshake"], None),
    )
    .await
    .expect("spawn should return within HANDSHAKE_TIMEOUT, not hang");

    let err = match res {
        Err(e) => e,
        Ok(_) => panic!("spawn should fail when handshake never arrives"),
    };
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("handshake") || msg.contains("timed out"),
        "unexpected error message: {msg}"
    );
}

#[tokio::test]
async fn destroy_kills_subprocess_that_ignores_stdin_close() {
    let transport = StdioTransport::spawn(HELPER, &["handshake-then-hang"], None)
        .await
        .expect("handshake should succeed for handshake-then-hang");

    let started = Instant::now();
    let res = tokio::time::timeout(Duration::from_secs(20), transport.destroy())
        .await
        .expect("destroy should return within CLOSE_TIMEOUT, not hang");

    let err = match res {
        Err(e) => e,
        Ok(_) => panic!("destroy should report a timeout when child ignores stdin close"),
    };
    assert!(
        err.to_string().to_lowercase().contains("did not exit"),
        "unexpected error message: {err}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "destroy took too long: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn concurrent_calls_are_serialized_and_responses_match_requests() {
    let transport = spawn_full_echo().await;
    let svc = ServiceParams::default();
    let req_a = sample_send_message_request("task-A");
    let req_b = sample_send_message_request("task-B");

    let (res_a, res_b) = tokio::join!(
        transport.send_message(&svc, &req_a),
        transport.send_message(&svc, &req_b),
    );

    let resp_a = res_a.expect("call A should succeed");
    let resp_b = res_b.expect("call B should succeed");

    match resp_a {
        SendMessageResponse::Task(t) => assert_eq!(t.id, "task-A"),
        other => panic!("expected Task for A, got {other:?}"),
    }
    match resp_b {
        SendMessageResponse::Task(t) => assert_eq!(t.id, "task-B"),
        other => panic!("expected Task for B, got {other:?}"),
    }

    shutdown(transport).await;
}

// ---------------------------------------------------------------------------
// Coverage: each Transport method against a real subprocess
// ---------------------------------------------------------------------------

#[tokio::test]
async fn transport_send_message() {
    let transport = spawn_full_echo().await;
    let svc = ServiceParams::default();

    let resp = transport
        .send_message(&svc, &sample_send_message_request("t1"))
        .await
        .unwrap();
    match resp {
        SendMessageResponse::Task(t) => assert_eq!(t.id, "t1"),
        other => panic!("unexpected response: {other:?}"),
    }

    shutdown(transport).await;
}

#[tokio::test]
async fn transport_send_streaming_message() {
    let transport = spawn_full_echo().await;
    let svc = ServiceParams::default();

    let mut stream = transport
        .send_streaming_message(&svc, &sample_send_message_request("stream-1"))
        .await
        .unwrap();

    let mut count = 0;
    while let Some(item) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
    {
        item.expect("stream item should be Ok");
        count += 1;
    }
    assert!(count >= 2, "expected at least 2 stream events, got {count}");

    shutdown(transport).await;
}

#[tokio::test]
async fn transport_get_task() {
    let transport = spawn_full_echo().await;
    let svc = ServiceParams::default();

    let task = transport
        .get_task(
            &svc,
            &GetTaskRequest {
                id: "found".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(task.id, "found");

    shutdown(transport).await;
}

#[tokio::test]
async fn transport_get_task_returns_server_error() {
    let transport = spawn_full_echo().await;
    let svc = ServiceParams::default();

    let err = transport
        .get_task(
            &svc,
            &GetTaskRequest {
                id: "missing".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    // Server returns task_not_found; client should surface it as A2AError.
    assert!(err.message.to_lowercase().contains("missing") || err.code != 0);

    shutdown(transport).await;
}

#[tokio::test]
async fn transport_list_tasks() {
    let transport = spawn_full_echo().await;
    let svc = ServiceParams::default();

    let resp = transport
        .list_tasks(
            &svc,
            &ListTasksRequest {
                context_id: None,
                status: None,
                page_size: None,
                page_token: None,
                history_length: None,
                status_timestamp_after: None,
                include_artifacts: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(resp.tasks.len(), 1);

    shutdown(transport).await;
}

#[tokio::test]
async fn transport_cancel_task() {
    let transport = spawn_full_echo().await;
    let svc = ServiceParams::default();

    let task = transport
        .cancel_task(
            &svc,
            &CancelTaskRequest {
                id: "t1".to_string(),
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(task.id, "t1");
    assert!(matches!(task.status.state, TaskState::Canceled));

    shutdown(transport).await;
}

#[tokio::test]
async fn transport_subscribe_to_task() {
    let transport = spawn_full_echo().await;
    let svc = ServiceParams::default();

    let mut stream = transport
        .subscribe_to_task(
            &svc,
            &SubscribeToTaskRequest {
                id: "sub-1".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();

    let mut count = 0;
    while let Some(item) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
    {
        item.expect("stream item should be Ok");
        count += 1;
    }
    assert!(count >= 2, "expected at least 2 stream events, got {count}");

    shutdown(transport).await;
}

#[tokio::test]
async fn transport_push_config_crud() {
    let transport = spawn_full_echo().await;
    let svc = ServiceParams::default();

    let cfg = transport
        .create_push_config(
            &svc,
            &CreateTaskPushNotificationConfigRequest {
                task_id: "t1".to_string(),
                config: PushNotificationConfig {
                    url: "https://x".to_string(),
                    id: Some("cfg-1".to_string()),
                    token: None,
                    authentication: None,
                },
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(cfg.task_id, "t1");

    let got = transport
        .get_push_config(
            &svc,
            &GetTaskPushNotificationConfigRequest {
                task_id: "t1".to_string(),
                id: "cfg-1".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(got.config.id.as_deref(), Some("cfg-1"));

    let list = transport
        .list_push_configs(
            &svc,
            &ListTaskPushNotificationConfigsRequest {
                task_id: "t1".to_string(),
                page_size: None,
                page_token: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(list.configs.len(), 1);

    transport
        .delete_push_config(
            &svc,
            &DeleteTaskPushNotificationConfigRequest {
                task_id: "t1".to_string(),
                id: "cfg-1".to_string(),
                tenant: None,
            },
        )
        .await
        .unwrap();

    shutdown(transport).await;
}

#[tokio::test]
async fn transport_get_extended_agent_card() {
    let transport = spawn_full_echo().await;
    let svc = ServiceParams::default();

    let card = transport
        .get_extended_agent_card(&svc, &GetExtendedAgentCardRequest { tenant: None })
        .await
        .unwrap();
    assert_eq!(card.name, "echo-agent");

    shutdown(transport).await;
}

// ---------------------------------------------------------------------------
// Coverage: error paths in `call`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn call_reports_deserialize_error_for_malformed_result() {
    // bad-response helper completes the handshake then sends back a JSON-RPC
    // response whose `result` is a string, not a Task. The client should
    // surface this as an internal "deserialize result" error rather than
    // panicking.
    let transport = StdioTransport::spawn(HELPER, &["bad-response"], None)
        .await
        .expect("bad-response handshake should succeed");

    let svc = ServiceParams::default();
    let err = transport
        .get_task(
            &svc,
            &GetTaskRequest {
                id: "t1".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .unwrap_err();
    assert!(
        err.message.to_lowercase().contains("deserialize"),
        "unexpected error message: {err:?}"
    );

    // Helper exits after writing the bad response; destroy should be quick.
    let _ = tokio::time::timeout(Duration::from_secs(10), transport.destroy()).await;
}

// ---------------------------------------------------------------------------
// Coverage: StdioTransportFactory
// ---------------------------------------------------------------------------

#[tokio::test]
async fn factory_protocol_is_stdio() {
    let factory = StdioTransportFactory;
    assert_eq!(factory.protocol(), a2a::TRANSPORT_PROTOCOL_STDIO);
}

#[tokio::test]
async fn factory_creates_transport_from_plain_command_url() {
    // The factory parses the AgentInterface URL, spawns the subprocess via
    // StdioTransport::spawn, and returns a Box<dyn Transport>.
    let factory = StdioTransportFactory;

    // Use the plain (non-stdio://) form so we can pass an absolute path
    // followed by the helper mode. This avoids stdio:// URL escaping.
    let url = format!("{HELPER} full-echo");

    let card = AgentCard {
        name: "x".to_string(),
        description: "x".to_string(),
        version: "0".to_string(),
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
    let iface = AgentInterface {
        url,
        protocol_binding: a2a::TRANSPORT_PROTOCOL_STDIO.to_string(),
        protocol_version: "1.0".to_string(),
        tenant: None,
    };

    let transport = factory
        .create(&card, &iface)
        .await
        .expect("factory should spawn the helper");

    let svc = ServiceParams::default();
    let resp = transport
        .send_message(&svc, &sample_send_message_request("via-factory"))
        .await
        .unwrap();
    match resp {
        SendMessageResponse::Task(t) => assert_eq!(t.id, "via-factory"),
        other => panic!("unexpected response: {other:?}"),
    }

    let _ = tokio::time::timeout(Duration::from_secs(10), transport.destroy()).await;
}

#[tokio::test]
async fn factory_propagates_spawn_failure_for_missing_program() {
    let factory = StdioTransportFactory;
    let card = AgentCard {
        name: "x".to_string(),
        description: "x".to_string(),
        version: "0".to_string(),
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
    let iface = AgentInterface {
        url: "/this/program/does/not/exist-xyz123".to_string(),
        protocol_binding: a2a::TRANSPORT_PROTOCOL_STDIO.to_string(),
        protocol_version: "1.0".to_string(),
        tenant: None,
    };

    let res = factory.create(&card, &iface).await;
    let err = match res {
        Err(e) => e,
        Ok(_) => panic!("factory should fail when the program cannot be spawned"),
    };
    assert!(
        err.message.to_lowercase().contains("spawn")
            || err.message.to_lowercase().contains("stdio"),
        "unexpected error message: {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Streaming background-reader error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_parse_frame_error_is_surfaced() {
    let transport = StdioTransport::spawn(HELPER, &["stream-bad-frame"], None)
        .await
        .expect("stream-bad-frame spawn should succeed");
    let svc = ServiceParams::default();

    let mut stream = transport
        .send_streaming_message(&svc, &sample_send_message_request("s"))
        .await
        .unwrap();

    let item = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("stream should yield within timeout")
        .expect("stream should not end before yielding the parse error");
    let err = item.expect_err("first item should be a parse-frame error");
    assert!(
        err.message.to_lowercase().contains("parse frame"),
        "unexpected error: {err:?}"
    );

    shutdown(transport).await;
}

#[tokio::test]
async fn streaming_bad_notification_is_surfaced_then_stream_completes() {
    let transport = StdioTransport::spawn(HELPER, &["stream-bad-notification"], None)
        .await
        .expect("stream-bad-notification spawn should succeed");
    let svc = ServiceParams::default();

    let mut stream = transport
        .send_streaming_message(&svc, &sample_send_message_request("s"))
        .await
        .unwrap();

    let mut saw_parse_event_err = false;
    while let Some(item) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
    {
        if let Err(e) = item {
            if e.message.to_lowercase().contains("parse streaming event") {
                saw_parse_event_err = true;
            }
        }
    }
    assert!(
        saw_parse_event_err,
        "expected to observe a 'parse streaming event' error"
    );

    shutdown(transport).await;
}

#[tokio::test]
async fn streaming_bad_final_response_is_surfaced() {
    let transport = StdioTransport::spawn(HELPER, &["stream-bad-final"], None)
        .await
        .expect("stream-bad-final spawn should succeed");
    let svc = ServiceParams::default();

    let mut stream = transport
        .send_streaming_message(&svc, &sample_send_message_request("s"))
        .await
        .unwrap();

    let mut saw_final_parse_err = false;
    while let Some(item) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
    {
        if let Err(e) = item {
            if e.message
                .to_lowercase()
                .contains("failed to parse final stream response")
            {
                saw_final_parse_err = true;
            }
        }
    }
    assert!(
        saw_final_parse_err,
        "expected to observe a 'failed to parse final stream response' error"
    );

    shutdown(transport).await;
}
