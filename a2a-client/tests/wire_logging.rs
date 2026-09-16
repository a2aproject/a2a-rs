// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Wire-logging behaviour driven through the real transports.
//!
//! `a2acli`'s own suite asserts this through the binary's stderr, which
//! proves the user-visible contract but cannot see a log line that is never
//! formatted. These tests install a capturing subscriber in-process, so the
//! assertions are about what the library emits rather than what a child
//! process happened to print — and the credential-redaction paths are
//! exercised where they live.

use std::io;
use std::sync::{Arc, Mutex};

use a2a::{
    DeleteTaskPushNotificationConfigRequest, GetTaskRequest, Message, Part, Role,
    SendMessageRequest,
};
use a2a_client::jsonrpc::JsonRpcTransport;
use a2a_client::{ServiceParams, Transport};
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing_subscriber::fmt::MakeWriter;

/// Collects formatted log output so a test can assert on it.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Installs a DEBUG subscriber for the current thread and returns both the
/// buffer and its guard. Every test here uses the default current-thread
/// runtime so that the thread-local default stays in force across `await`.
fn capture() -> (Captured, tracing::subscriber::DefaultGuard) {
    let captured = Captured::default();
    let subscriber = tracing_subscriber::fmt()
        .with_writer(captured.clone())
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (captured, guard)
}

/// A one-shot HTTP server: accepts one connection, reads what it can of the
/// request, and replies with `response`. Enough to drive a transport against
/// a canned reply — including a malformed one — without a web framework.
async fn one_shot(response: &'static str) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // One read is enough: nothing here asserts on the request from the
        // server side, and the reply must not wait for a body that reqwest
        // may still be writing.
        let mut buf = vec![0u8; 8192];
        let _ = socket.read(&mut buf).await;
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
    });

    format!("http://{addr}/jsonrpc")
}

fn http_ok(body: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// A reply that promises more body than it sends and then closes, so reading
/// the body fails mid-stream rather than parsing failing afterwards. These
/// are different paths: one reports a transport error, the other a parse
/// error, and only this shape reaches the first.
fn http_truncated() -> &'static str {
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort"
}

fn send_request() -> SendMessageRequest {
    SendMessageRequest {
        message: Message {
            message_id: "m1".to_string(),
            context_id: None,
            task_id: None,
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

fn transport(endpoint: String) -> JsonRpcTransport {
    JsonRpcTransport::new(reqwest::Client::new(), endpoint)
}

/// A credential in `ServiceParams` reaches the log as its name only. This is
/// the library-side half of `A2ACLI_AUTH_004`: redaction happens where the
/// line is built, so there is no arrangement of subscriber or filter that
/// yields the value.
#[tokio::test]
async fn a_credential_header_is_logged_by_name_without_its_value() {
    let endpoint = one_shot(Box::leak(
        http_ok(r#"{"jsonrpc":"2.0","id":"1","result":{"id":"t1","contextId":"c1"}}"#)
            .into_boxed_str(),
    ))
    .await;
    let (captured, _guard) = capture();

    let mut params = ServiceParams::new();
    params.insert(
        "Authorization".to_string(),
        vec!["Bearer must-not-be-logged".to_string()],
    );
    params.insert("A2A-Version".to_string(), vec!["1.0".to_string()]);

    let _ = transport(endpoint)
        .get_task(
            &params,
            &GetTaskRequest {
                id: "t1".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await;

    let log = captured.text();
    assert!(log.contains("A2A wire request"), "no request logged: {log}");
    // Attachment is confirmable...
    assert!(log.contains("authorization: (redacted)"), "{log}");
    // ...the value is not present. Deliberately not interpolated into the
    // failure message, so a regression here does not print the credential.
    assert!(
        !log.contains("must-not-be-logged"),
        "the credential value reached the log"
    );
    // Protocol metadata stays readable, since version negotiation is one of
    // the things wire logging exists to explain.
    assert!(log.contains("a2a-version: 1.0"), "{log}");
}

/// The response body is logged, which is the half of §7.2 that the old
/// method-name-only logging could not provide.
#[tokio::test]
async fn the_response_body_is_logged() {
    let body = r#"{"jsonrpc":"2.0","id":"1","result":{"id":"task-42","contextId":"c1"}}"#;
    let endpoint = one_shot(Box::leak(http_ok(body).into_boxed_str())).await;
    let (captured, _guard) = capture();

    let _ = transport(endpoint)
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "task-42".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await;

    let log = captured.text();
    assert!(log.contains("A2A wire response"), "{log}");
    assert!(log.contains("status=200"), "{log}");
    assert!(log.contains("task-42"), "response body not logged: {log}");
}

/// A body that is not JSON at all still gets logged — that is precisely the
/// case where seeing the raw bytes is what explains the failure — and the
/// call reports a parse error rather than something vaguer.
#[tokio::test]
async fn a_malformed_response_is_logged_and_reported_as_a_parse_failure() {
    let endpoint = one_shot(Box::leak(
        http_ok("this is not json at all").into_boxed_str(),
    ))
    .await;
    let (captured, _guard) = capture();

    let error = transport(endpoint)
        .delete_push_config(
            &ServiceParams::new(),
            &DeleteTaskPushNotificationConfigRequest {
                task_id: "t1".to_string(),
                id: "c1".to_string(),
                tenant: None,
            },
        )
        .await
        .expect_err("a non-JSON body cannot deserialize");

    assert!(
        error.message.contains("failed to parse JSON-RPC response"),
        "unexpected error: {error}"
    );
    let log = captured.text();
    assert!(
        log.contains("this is not json at all"),
        "the unparseable body was not logged: {log}"
    );
}

/// A streaming call answered with a plain JSON content type takes the
/// non-SSE branch, where a malformed body is likewise a parse failure rather
/// than an empty stream.
#[tokio::test]
async fn a_streaming_call_answered_with_malformed_json_reports_a_parse_failure() {
    let endpoint = one_shot(Box::leak(http_ok("{not json").into_boxed_str())).await;
    let (captured, _guard) = capture();

    let error = transport(endpoint)
        .send_streaming_message(&ServiceParams::new(), &send_request())
        .await
        .err()
        .expect("a malformed non-SSE body cannot deserialize");

    assert!(
        error.message.contains("failed to parse JSON-RPC response"),
        "unexpected error: {error}"
    );
    assert!(captured.text().contains("{not json"), "body not logged");
}

/// A connection that cannot be opened is reported as a request failure, and
/// the attempt is still logged: knowing what was about to be sent is useful
/// precisely when nothing came back.
#[tokio::test]
async fn an_unreachable_endpoint_logs_the_attempt_and_fails() {
    // Port 1 on loopback: nothing listens there, and connecting fails fast
    // rather than hanging on a route.
    let (captured, _guard) = capture();

    let error = transport("http://127.0.0.1:1/jsonrpc".to_string())
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "t1".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .expect_err("nothing is listening");

    assert!(
        error.message.contains("HTTP request failed"),
        "unexpected error: {error}"
    );
    assert!(
        captured.text().contains("A2A wire request"),
        "the attempt was not logged"
    );
}

/// A body that cannot be read to completion fails as a transport error on
/// the unary path, distinct from a body that reads fine but will not parse.
#[tokio::test]
async fn a_truncated_unary_body_is_reported_as_a_read_failure() {
    let endpoint = one_shot(http_truncated()).await;
    let (_captured, _guard) = capture();

    let error = transport(endpoint)
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "t1".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .expect_err("the body ends before Content-Length");

    assert!(
        error.message.contains("failed to parse JSON-RPC response"),
        "unexpected error: {error}"
    );
}

/// The same truncation on the streaming path's non-SSE branch, which has its
/// own read-and-parse pair.
#[tokio::test]
async fn a_truncated_streaming_body_is_reported_as_a_read_failure() {
    let endpoint = one_shot(http_truncated()).await;
    let (_captured, _guard) = capture();

    let error = transport(endpoint)
        .send_streaming_message(&ServiceParams::new(), &send_request())
        .await
        .err()
        .expect("the body ends before Content-Length");

    assert!(
        error.message.contains("failed to parse JSON-RPC response"),
        "unexpected error: {error}"
    );
}

/// `delete_push_config` builds its request and reads its reply separately
/// from the shared helpers, so its own read-failure path needs its own case.
#[tokio::test]
async fn a_truncated_delete_body_is_reported_as_a_read_failure() {
    let endpoint = one_shot(http_truncated()).await;
    let (_captured, _guard) = capture();

    let error = transport(endpoint)
        .delete_push_config(
            &ServiceParams::new(),
            &DeleteTaskPushNotificationConfigRequest {
                task_id: "t1".to_string(),
                id: "c1".to_string(),
                tenant: None,
            },
        )
        .await
        .expect_err("the body ends before Content-Length");

    assert!(
        error.message.contains("failed to parse JSON-RPC response"),
        "unexpected error: {error}"
    );
}

/// A stream's payload is its events, so each raw SSE frame is logged as it
/// arrives — the streaming counterpart of logging a response body. The frame
/// here is deliberately not a valid `StreamResponse`: logging happens before
/// parsing, because seeing the bytes is exactly what explains a frame the
/// client could not understand.
#[tokio::test]
async fn each_raw_stream_frame_is_logged_as_it_arrives() {
    // No Content-Length: an event stream is read until the peer closes.
    let endpoint = one_shot(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"unparseable\":true}\n\n",
    )
    .await;
    let (captured, _guard) = capture();

    let mut stream = transport(endpoint)
        .send_streaming_message(&ServiceParams::new(), &send_request())
        .await
        .expect("an event-stream content type opens a stream");

    // Drive the parser far enough to consume the frame.
    let _ = stream.next().await;

    let log = captured.text();
    assert!(
        log.contains("A2A wire stream event"),
        "the frame was not logged: {log}"
    );
    assert!(
        log.contains("unparseable"),
        "the raw frame contents were not logged: {log}"
    );
}

/// A service parameter whose value cannot be a header value (here, one
/// containing a newline) fails when the request is built, before anything is
/// sent. It is reported as a request failure like any other: a caller cannot
/// act differently on "could not be built" than on "could not be sent", so
/// both carry the same message.
#[tokio::test]
async fn an_unsendable_header_value_fails_the_request() {
    let (_captured, _guard) = capture();

    let mut params = ServiceParams::new();
    params.insert(
        "X-Broken".to_string(),
        vec!["header\nvalues cannot contain newlines".to_string()],
    );

    let error = transport("http://127.0.0.1:1/jsonrpc".to_string())
        .get_task(
            &params,
            &GetTaskRequest {
                id: "t1".to_string(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .expect_err("the header value is not representable");

    assert!(
        error.message.contains("HTTP request failed"),
        "unexpected error: {error}"
    );
}
