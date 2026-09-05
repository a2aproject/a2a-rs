// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! This crate's client talking to this crate's server.
//!
//! The unit tests on either side script their peer by hand, which shows that
//! each half matches one reading of the spec but not that the two readings
//! agree. Here the only thing between them is a pair of in-memory pipes, so a
//! disagreement about framing, correlation, or lifecycle shows up as a hang or
//! a wrong answer rather than as two tests that both pass.

use std::collections::HashMap;
use std::time::Duration;

use a2a::*;
use a2a_client::{ServiceParams, Transport};
use a2a_server::{AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore};
use a2a_stdio::client::{ClientConfig, StdioTransport};
use a2a_stdio::metadata::BINDING_URI;
use a2a_stdio::server::{ServerConfig, Startup, serve_on};
use a2a_stdio::session::ExitCode;
use a2a_stdio::wire::Variant;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::task::JoinHandle;

const SESSION_ID: &str = "loopback-session";
const LAUNCH_DIGEST: &str = "sha256:test";
const PIPE: usize = 64 * 1024;

/// Metadata key the agent reports its received 4.1 parameters under.
const OBSERVED: &str = "test/serviceParams";

/// Emits the 8.2 shape: an initial `Task`, then a terminal status update.
///
/// The task echoes the service parameters the agent was handed, which is the
/// only way a test on the client side can see what survived the trip.
struct ScriptedAgent;

impl AgentExecutor for ScriptedAgent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (task_id, context_id) = ctx.task_info();
        let observed: Value = ctx
            .service_params
            .iter()
            .map(|(k, v)| (k.clone(), json!(v)))
            .collect::<serde_json::Map<_, _>>()
            .into();
        let task = Task {
            id: task_id.clone(),
            context_id: context_id.clone(),
            status: TaskStatus {
                state: TaskState::Working,
                message: ctx.message,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: Some(HashMap::from([(OBSERVED.to_string(), observed)])),
        };
        let done = TaskStatusUpdateEvent {
            task_id,
            context_id,
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            metadata: None,
        };
        Box::pin(futures::stream::iter(vec![
            Ok(StreamResponse::Task(task)),
            Ok(StreamResponse::StatusUpdate(done)),
        ]))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (task_id, context_id) = ctx.task_info();
        Box::pin(futures::stream::once(async move {
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                context_id,
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            }))
        }))
    }
}

struct Loopback {
    client: StdioTransport,
    server: JoinHandle<ExitCode>,
}

impl Loopback {
    /// Shut down the way 2.4 prescribes and report the agent's exit code.
    async fn shutdown(self) -> ExitCode {
        self.client.destroy().await.expect("destroy failed");
        tokio::time::timeout(Duration::from_secs(5), self.server)
            .await
            .expect("the agent did not exit after STDIN closed")
            .expect("the agent panicked")
    }
}

async fn loopback() -> Loopback {
    // Two pairs, because a child's STDIN and STDOUT are separate objects and
    // closing one direction must not close the other.
    let (client_read, server_write) = tokio::io::duplex(PIPE);
    let (server_read, client_write) = tokio::io::duplex(PIPE);

    let handler = DefaultRequestHandler::new(ScriptedAgent, InMemoryTaskStore::new())
        .with_capabilities(AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        });

    let mut session_params = ServiceParams::new();
    session_params.insert("a2a-version".into(), vec!["1.0".into()]);

    let server = tokio::spawn(serve_on(
        server_read,
        server_write,
        Startup {
            session_id: SESSION_ID.into(),
            session_params,
        },
        handler,
        ServerConfig {
            launch_digest: Some(LAUNCH_DIGEST.into()),
            ..ServerConfig::default()
        },
    ));

    let client = StdioTransport::attach(
        client_read,
        client_write,
        SESSION_ID,
        ClientConfig::default(),
    )
    .await
    .expect("handshake failed");

    Loopback { client, server }
}

fn hello() -> SendMessageRequest {
    SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text("hello")]),
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

/// 9.6: the server stamps the local session onto everything it emits, and it
/// has to survive the client's decode to be of any use.
#[track_caller]
fn assert_bound(metadata: &Option<HashMap<String, Value>>) {
    let binding = metadata
        .as_ref()
        .and_then(|m| m.get(BINDING_URI))
        .expect("no session binding in metadata");
    assert_eq!(binding["sessionId"], json!(SESSION_ID));
    assert_eq!(binding["variant"], json!("stdio-json"));
    assert_eq!(binding["launchDigest"], json!(LAUNCH_DIGEST));
    assert!(binding["pid"].is_number(), "pid should be present");
}

#[track_caller]
fn task_of(response: SendMessageResponse) -> Task {
    match response {
        SendMessageResponse::Task(task) => task,
        other => panic!("expected a task, got {other:?}"),
    }
}

/// What the agent reported seeing under [`OBSERVED`].
#[track_caller]
fn observed(task: &Task) -> &Value {
    &task.metadata.as_ref().expect("no metadata")[OBSERVED]
}

#[tokio::test]
async fn the_handshake_agrees_on_stdio_json() {
    let lb = loopback().await;
    assert_eq!(lb.client.session_id(), SESSION_ID);
    assert_eq!(
        lb.client.variant(),
        Variant::Json,
        "both halves must reach the same variant (2.2)"
    );
    assert!(
        lb.client
            .server_features()
            .contains(&"streaming".to_string()),
        "features advertised by the server should reach the client"
    );
    assert_eq!(lb.shutdown().await, ExitCode::Ok);
}

#[tokio::test]
async fn a_unary_call_crosses_the_binding() {
    let lb = loopback().await;
    let task = task_of(
        lb.client
            .send_message(&ServiceParams::new(), &hello())
            .await
            .expect("send_message failed"),
    );
    assert_eq!(task.status.state, TaskState::Completed);
    assert_bound(&task.metadata);
    assert_eq!(lb.shutdown().await, ExitCode::Ok);
}

#[tokio::test]
async fn a_stream_crosses_the_binding_in_order_and_terminates() {
    let lb = loopback().await;
    let mut stream = lb
        .client
        .send_streaming_message(&ServiceParams::new(), &hello())
        .await
        .expect("send_streaming_message failed");

    let mut kinds = Vec::new();
    while let Some(item) = stream.next().await {
        kinds.push(match item.expect("stream item failed") {
            StreamResponse::Task(t) => {
                assert_bound(&t.metadata);
                "task"
            }
            StreamResponse::StatusUpdate(s) => {
                // 9.6 lists Task, Message and Artifact, so an update event is
                // left alone even mid-stream. Asserted rather than skipped
                // because stamping it would be the easy mistake to make.
                assert!(s.metadata.is_none(), "an update event is not stamped");
                "status"
            }
            StreamResponse::Message(_) => "message",
            StreamResponse::ArtifactUpdate(_) => "artifact",
        });
    }
    // 8.2 ordering, and the end came from `streamEnd` rather than a lost pipe:
    // the session is still usable below.
    assert_eq!(kinds, ["task", "status"]);
    assert_eq!(lb.shutdown().await, ExitCode::Ok);
}

#[tokio::test]
async fn two_calls_share_one_pipe_pair() {
    // 3.6: correlation by id is the only thing keeping these apart.
    let lb = loopback().await;
    let params = ServiceParams::new();

    let created = task_of(lb.client.send_message(&params, &hello()).await.unwrap());

    let fetched = lb
        .client
        .get_task(
            &params,
            &GetTaskRequest {
                id: created.id.clone(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .expect("get_task failed");

    assert_eq!(fetched.id, created.id);
    assert_bound(&fetched.metadata);
    assert_eq!(lb.shutdown().await, ExitCode::Ok);
}

#[tokio::test]
async fn a_server_error_keeps_its_code_across_the_binding() {
    // 7.2: the numeric code is the JSON-RPC binding's, so it must survive the
    // trip rather than arrive as a generic internal error.
    let lb = loopback().await;
    let err = lb
        .client
        .get_task(
            &ServiceParams::new(),
            &GetTaskRequest {
                id: "no-such-task".into(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .expect_err("a missing task should be an error");

    assert_eq!(err.code, error_code::TASK_NOT_FOUND);
    assert_eq!(lb.shutdown().await, ExitCode::Ok);
}

#[tokio::test]
async fn cancelling_a_stream_leaves_the_session_usable() {
    // 8.5: the point of a `cancelStream` frame is that it stops one stream
    // instead of tearing down the pipe the way closing an SSE response would.
    let lb = loopback().await;
    let params = ServiceParams::new();

    let created = task_of(lb.client.send_message(&params, &hello()).await.unwrap());

    let stream = lb
        .client
        .subscribe_to_task(
            &params,
            &SubscribeToTaskRequest {
                id: created.id.clone(),
                tenant: None,
            },
        )
        .await
        .expect("subscribe_to_task failed");
    drop(stream);

    let fetched = lb
        .client
        .get_task(
            &params,
            &GetTaskRequest {
                id: created.id.clone(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .expect("the session should have survived the cancel");
    assert_eq!(fetched.id, created.id);
    assert_eq!(lb.shutdown().await, ExitCode::Ok);
}

#[tokio::test]
async fn session_params_apply_and_a_request_overrides_them() {
    // 4.1. Both halves are checked in one session because the interesting part
    // is the difference between them, not either alone.
    let lb = loopback().await;

    let defaulted = task_of(
        lb.client
            .send_message(&ServiceParams::new(), &hello())
            .await
            .unwrap(),
    );
    assert_eq!(
        observed(&defaulted)["a2a-version"],
        json!(["1.0"]),
        "a request naming no parameters inherits the spawn's"
    );

    let mut overrides = ServiceParams::new();
    overrides.insert("a2a-version".into(), vec!["2.0".into()]);
    overrides.insert("a2a-tenant".into(), vec!["acme".into()]);
    let overridden = task_of(lb.client.send_message(&overrides, &hello()).await.unwrap());
    assert_eq!(
        observed(&overridden)["a2a-version"],
        json!(["2.0"]),
        "a request-scoped value replaces the session's"
    );
    assert_eq!(observed(&overridden)["a2a-tenant"], json!(["acme"]));

    assert_eq!(lb.shutdown().await, ExitCode::Ok);
}
