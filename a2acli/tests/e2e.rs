// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::BTreeMap;
use std::process::Command as StdCommand;
use std::sync::{Arc, Mutex};

use a2a::event::{StreamResponse, TaskStatusUpdateEvent};
use a2a::*;
use a2a_server::jsonrpc::jsonrpc_router;
use a2a_server::rest::rest_router;
use a2a_server::{RequestHandler, ServiceParams, WELL_KNOWN_AGENT_CARD_PATH};
use assert_cmd::Command as AssertCommand;
use assert_cmd::assert::OutputAssertExt;
use assert_cmd::cargo::CommandCargoExt;
use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::get;
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use futures::stream::{self, BoxStream};
use serde_json::Value;
use tokio::net::TcpListener;

/// (Authorization, x-test, x-api-key) recorded per card fetch.
type CardHeaders = (Option<String>, Option<String>, Option<String>);

#[derive(Default)]
struct ServerState {
    tasks: Mutex<BTreeMap<String, Task>>,
    push_configs: Mutex<BTreeMap<(String, String), TaskPushNotificationConfig>>,
    card_headers: Mutex<Vec<CardHeaders>>,
    /// Number of times `get_task` has been called for each of the
    /// poll-until-settled fixture task ids below.
    poll_counts: Mutex<BTreeMap<String, u32>>,
    /// The `tenant` field of each `send_message` request received, in
    /// order — for TX_003's "selected interface's own tenant, absent an
    /// explicit --tenant" fallback.
    received_send_tenants: Mutex<Vec<Option<String>>>,
}

/// Fixture task ids that settle to `COMPLETED` only after this many
/// `get_task` calls, so tests can exercise the blocking-wait/poll loop
/// instead of a task that is already settled on the first read.
const POLLS_UNTIL_SETTLED: u32 = 3;
/// Fixture task id that never settles, for exercising `--timeout`.
const STUCK_TASK_ID: &str = "task-stuck";

struct TestHandler {
    state: Arc<ServerState>,
    extended_card: AgentCard,
}

struct TestServer {
    base_url: String,
    state: Arc<ServerState>,
    handle: tokio::task::JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl TestServer {
    async fn spawn() -> Self {
        Self::spawn_with_card_tenant(None).await
    }

    /// Like [`Self::spawn`], but the public card's JSON-RPC interface
    /// declares the given routing `tenant` (A2A §8.3.2), for exercising
    /// TX_003's "use the selected interface's own tenant absent an explicit
    /// --tenant" fallback.
    async fn spawn_with_card_tenant(tenant: Option<&str>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(ServerState::default());

        {
            let mut tasks = state.tasks.lock().unwrap();
            tasks.insert(
                "task-1".to_string(),
                make_task("task-1", "ctx-1", TaskState::Completed, "seeded result"),
            );
            tasks.insert(
                "task-needs-input".to_string(),
                make_task(
                    "task-needs-input",
                    "ctx-needs-input",
                    TaskState::InputRequired,
                    "what's the destination city?",
                ),
            );
        }

        let mut public_card = make_agent_card(&base_url, "Fixture Agent");
        if let Some(tenant) = tenant {
            public_card.supported_interfaces[0].tenant = Some(tenant.to_string());
        }
        let extended_card = make_agent_card(&base_url, "Fixture Agent (extended)");
        let handler = Arc::new(TestHandler {
            state: state.clone(),
            extended_card,
        });

        let card_state = state.clone();
        let card = public_card.clone();
        let app = Router::new()
            .route(
                WELL_KNOWN_AGENT_CARD_PATH,
                get(move |headers: HeaderMap| {
                    let state = card_state.clone();
                    let card = card.clone();
                    async move {
                        state.card_headers.lock().unwrap().push((
                            headers
                                .get(header::AUTHORIZATION)
                                .and_then(|value| value.to_str().ok())
                                .map(ToOwned::to_owned),
                            headers
                                .get("x-test")
                                .and_then(|value| value.to_str().ok())
                                .map(ToOwned::to_owned),
                            headers
                                .get("x-api-key")
                                .and_then(|value| value.to_str().ok())
                                .map(ToOwned::to_owned),
                        ));
                        (StatusCode::OK, Json(card))
                    }
                }),
            )
            .nest("/jsonrpc", jsonrpc_router(handler.clone()))
            .nest("/rest", rest_router(handler));

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        TestServer {
            base_url,
            state,
            handle,
        }
    }
}

#[async_trait]
impl RequestHandler for TestHandler {
    async fn send_message(
        &self,
        _params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        self.state
            .received_send_tenants
            .lock()
            .unwrap()
            .push(req.tenant.clone());

        let text = req.message.text().unwrap_or_default();
        if text == "send-error" {
            return Err(A2AError::invalid_request("send failed"));
        }

        // A2A §3.4.3: an explicit --task-id must reference an existing
        // task, and a --context-id given alongside it must match that
        // task's actual context — the server rejects a mismatch rather
        // than reconciling it (SPEC.md §8.1, INTERACT_002).
        if let Some(requested_task_id) = &req.message.task_id {
            let tasks = self.state.tasks.lock().unwrap();
            match tasks.get(requested_task_id) {
                None => return Err(A2AError::task_not_found(requested_task_id)),
                Some(existing) => {
                    if let Some(requested_context_id) = &req.message.context_id {
                        if requested_context_id != &existing.context_id {
                            return Err(A2AError::invalid_params(format!(
                                "task {requested_task_id} belongs to context {}, not {requested_context_id}",
                                existing.context_id
                            )));
                        }
                    }
                }
            }
        }

        let task_id = req
            .message
            .task_id
            .clone()
            .unwrap_or_else(|| "task-send".to_string());
        let context_id = req
            .message
            .context_id
            .clone()
            .unwrap_or_else(|| "ctx-send".to_string());
        if text == "reply-only" {
            // No task created: a direct Message reply (SEND_003 — the tool
            // must exit cleanly with it rather than waiting on a task that
            // doesn't exist).
            return Ok(SendMessageResponse::Message(Message {
                message_id: "msg-reply-only".to_string(),
                context_id: Some(context_id),
                task_id: None,
                role: Role::Agent,
                parts: vec![Part::text(format!("Echo: {text}"))],
                metadata: None,
                extensions: None,
                reference_task_ids: None,
            }));
        }
        if text == "start-pending" {
            // A task that starts WORKING and only settles after
            // POLLS_UNTIL_SETTLED calls to get_task, so tests can observe
            // send's blocking-by-default wait actually polling.
            let task = make_task(
                "task-pending-send",
                "ctx-pending-send",
                TaskState::Working,
                "pending",
            );
            self.state
                .tasks
                .lock()
                .unwrap()
                .insert("task-pending-send".to_string(), task.clone());
            return Ok(SendMessageResponse::Task(task));
        }

        // Multi-part messages are echoed back verbatim so tests can assert
        // on the exact parts (and their order/media types) the CLI sent;
        // a single-part message keeps the simpler "Echo: {text}" form so
        // existing single-part assertions are unaffected.
        let response_parts = if req.message.parts.len() > 1 {
            req.message.parts.clone()
        } else {
            vec![Part::text(format!("Echo: {text}"))]
        };
        let task =
            make_task_with_parts(&task_id, &context_id, TaskState::Completed, response_parts);
        self.state
            .tasks
            .lock()
            .unwrap()
            .insert(task_id.clone(), task.clone());
        Ok(SendMessageResponse::Task(task))
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
            .unwrap_or_else(|| "task-stream".to_string());
        let context_id = req
            .message
            .context_id
            .clone()
            .unwrap_or_else(|| "ctx-stream".to_string());
        let text = req.message.text().unwrap_or_default();
        if text == "stream-error" {
            return Ok(Box::pin(stream::once(async {
                Err(A2AError::internal("stream failed"))
            })));
        }
        if text == "stream-open-error" {
            // Unlike "stream-error" (a stream that opens, then yields an
            // error item), this fails *opening* the stream at all — the
            // case send's fallback-to-polling path (TASK_POLL_004) exists
            // for. UNSUPPORTED_OPERATION specifically, since that's the
            // only code the fallback should trigger on.
            return Err(A2AError::unsupported_operation("streaming not available"));
        }
        if text == "stream-open-real-error" {
            // A genuine failure opening the stream that is *not*
            // "streaming unsupported" — this must propagate as an error,
            // not be silently retried as a one-shot send.
            return Err(A2AError::internal("transport exploded"));
        }

        let task = make_task(
            &task_id,
            &context_id,
            TaskState::Completed,
            &format!("Echo: {text}"),
        );
        self.state
            .tasks
            .lock()
            .unwrap()
            .insert(task_id.clone(), task.clone());

        Ok(Box::pin(stream::iter(vec![
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                context_id,
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            })),
            Ok(StreamResponse::Task(task)),
        ])))
    }

    async fn get_task(
        &self,
        _params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        if req.id == STUCK_TASK_ID {
            return Ok(make_task(&req.id, "ctx-stuck", TaskState::Working, "stuck"));
        }

        if req.id == "task-pending" || req.id == "task-pending-send" {
            let mut counts = self.state.poll_counts.lock().unwrap();
            let count = counts.entry(req.id.clone()).or_insert(0);
            *count += 1;
            let state = if *count >= POLLS_UNTIL_SETTLED {
                TaskState::Completed
            } else {
                TaskState::Working
            };
            let context_id = if req.id == "task-pending" {
                "ctx-pending"
            } else {
                "ctx-pending-send"
            };
            return Ok(make_task(&req.id, context_id, state, "settled"));
        }

        self.state
            .tasks
            .lock()
            .unwrap()
            .get(&req.id)
            .cloned()
            .ok_or_else(|| A2AError::task_not_found(&req.id))
    }

    async fn list_tasks(
        &self,
        _params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        if req.context_id.as_deref() == Some("error") {
            return Err(A2AError::invalid_params("list failed"));
        }

        let tasks: Vec<Task> = self
            .state
            .tasks
            .lock()
            .unwrap()
            .values()
            .filter(|task| {
                req.context_id
                    .as_ref()
                    .is_none_or(|context_id| &task.context_id == context_id)
            })
            .filter(|task| {
                req.status
                    .as_ref()
                    .is_none_or(|status| &task.status.state == status)
            })
            .cloned()
            .collect();

        Ok(ListTasksResponse {
            total_size: tasks.len() as i32,
            page_size: req.page_size.unwrap_or(tasks.len() as i32),
            next_page_token: String::new(),
            tasks,
        })
    }

    async fn cancel_task(
        &self,
        _params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let mut tasks = self.state.tasks.lock().unwrap();
        let task = tasks
            .get(&req.id)
            .cloned()
            .ok_or_else(|| A2AError::task_not_found(&req.id))?;
        let canceled = make_task(&task.id, &task.context_id, TaskState::Canceled, "canceled");
        tasks.insert(req.id, canceled.clone());
        Ok(canceled)
    }

    async fn subscribe_to_task(
        &self,
        _params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        if req.id == "stream-error" {
            return Ok(Box::pin(stream::once(async {
                Err(A2AError::internal("stream failed"))
            })));
        }

        let task = self
            .state
            .tasks
            .lock()
            .unwrap()
            .get(&req.id)
            .cloned()
            .ok_or_else(|| A2AError::task_not_found(&req.id))?;

        Ok(Box::pin(stream::iter(vec![
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: req.id.clone(),
                context_id: task.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            })),
            Ok(StreamResponse::Task(task)),
        ])))
    }

    async fn create_push_config(
        &self,
        _params: &ServiceParams,
        req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        if !self.state.tasks.lock().unwrap().contains_key(&req.task_id) {
            return Err(A2AError::task_not_found(&req.task_id));
        }

        let mut config = req;
        let config_id = config
            .id
            .clone()
            .unwrap_or_else(|| "cfg-generated".to_string());
        config.id = Some(config_id.clone());
        self.state
            .push_configs
            .lock()
            .unwrap()
            .insert((config.task_id.clone(), config_id), config.clone());
        Ok(config)
    }

    async fn get_push_config(
        &self,
        _params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.state
            .push_configs
            .lock()
            .unwrap()
            .get(&(req.task_id.clone(), req.id.clone()))
            .cloned()
            .ok_or_else(|| A2AError::task_not_found(&req.task_id))
    }

    async fn list_push_configs(
        &self,
        _params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        if req.task_id == "missing" {
            return Err(A2AError::task_not_found(&req.task_id));
        }

        let configs = self
            .state
            .push_configs
            .lock()
            .unwrap()
            .values()
            .filter(|config| config.task_id == req.task_id)
            .cloned()
            .collect();
        Ok(ListTaskPushNotificationConfigsResponse {
            configs,
            next_page_token: None,
        })
    }

    async fn delete_push_config(
        &self,
        _params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let deleted = self
            .state
            .push_configs
            .lock()
            .unwrap()
            .remove(&(req.task_id.clone(), req.id.clone()));
        if deleted.is_none() {
            return Err(A2AError::task_not_found(&req.task_id));
        }
        Ok(())
    }

    async fn get_extended_agent_card(
        &self,
        _params: &ServiceParams,
        req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        if req.tenant.as_deref() == Some("error") {
            return Err(A2AError::unsupported_operation("extended card denied"));
        }

        Ok(self.extended_card.clone())
    }
}

fn make_agent_card(base_url: &str, name: &str) -> AgentCard {
    AgentCard {
        name: name.to_string(),
        description: "CLI integration fixture".to_string(),
        version: VERSION.to_string(),
        supported_interfaces: vec![
            AgentInterface::new(format!("{base_url}/jsonrpc"), TRANSPORT_PROTOCOL_JSONRPC),
            AgentInterface::new(format!("{base_url}/rest"), TRANSPORT_PROTOCOL_HTTP_JSON),
        ],
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(true),
            extensions: None,
            extended_agent_card: Some(true),
        },
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

fn make_task(task_id: &str, context_id: &str, state: TaskState, text: &str) -> Task {
    make_task_with_parts(task_id, context_id, state, vec![Part::text(text)])
}

fn make_task_with_parts(
    task_id: &str,
    context_id: &str,
    state: TaskState,
    parts: Vec<Part>,
) -> Task {
    Task {
        id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state,
            message: Some(Message {
                message_id: format!("msg-{task_id}"),
                context_id: Some(context_id.to_string()),
                task_id: Some(task_id.to_string()),
                role: Role::Agent,
                parts,
                metadata: None,
                extensions: None,
                reference_task_ids: None,
            }),
            timestamp: None,
        },
        artifacts: None,
        history: None,
        metadata: None,
    }
}

/// Most of this suite asserts on the exact protocol JSON shape the CLI
/// received/produced, so these helpers default to `-o json` — the same way
/// the pre-#168 CLI always behaved. Tests that specifically exercise the new
/// `text` default or the error envelope build their own `StdCommand`
/// instead of going through these.
fn run_cli_success(server: &TestServer, args: &[&str]) -> String {
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    command.args(["--base-url", server.base_url.as_str(), "--output", "json"]);
    command.args(args);
    let output = command.assert().success().get_output().stdout.clone();
    String::from_utf8(output).unwrap()
}

fn run_cli_failure(server: &TestServer, args: &[&str]) -> (String, String) {
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    command.args(["--base-url", server.base_url.as_str(), "--output", "json"]);
    command.args(args);
    let output = command.assert().failure().get_output().clone();
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
}

/// Like [`run_cli_failure`], but also returns the process exit code, for the
/// handful of tests that check §11.6's exit-code contract explicitly.
fn run_cli_failure_status(server: &TestServer, args: &[&str]) -> (String, String, i32) {
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    command.args(["--base-url", server.base_url.as_str(), "--output", "json"]);
    command.args(args);
    let output = command.assert().failure().get_output().clone();
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
        output.status.code().unwrap(),
    )
}

/// Parse the Appendix B error envelope a failing command printed to stderr.
fn parse_error_envelope(stderr: &str) -> Value {
    serde_json::from_str(stderr.trim()).unwrap()
}

fn parse_json_lines(output: &str) -> Vec<Value> {
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

async fn unused_base_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_and_extended_card_commands_work() {
    let server = TestServer::spawn().await;

    let stdout = run_cli_success(
        &server,
        &[
            "--bearer",
            "secret",
            "--svc-param",
            "X-Test: abc",
            "card",
            "get",
        ],
    );
    let card: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(card["name"], "Fixture Agent");

    let headers = server.state.card_headers.lock().unwrap().clone();
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].0.as_deref(), Some("Bearer secret"));
    assert_eq!(headers[0].1.as_deref(), Some("abc"));

    let compact = run_cli_success(
        &server,
        &[
            "--transport",
            "rest",
            "--compact",
            "card",
            "get",
            "--extended",
        ],
    );
    assert!(!compact.trim_end().contains('\n'));
    let card: Value = serde_json::from_str(compact.trim()).unwrap();
    assert_eq!(card["name"], "Fixture Agent (extended)");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_task_list_and_cancel_commands_work() {
    let server = TestServer::spawn().await;

    // No --task-id/--context-id: this starts a *new* task (the server
    // assigns "task-send"/"ctx-send" per the fixture's defaults). A2A
    // §4.1 forbids a client from inventing a taskId for a new task, so
    // an explicit --task-id here would have to name an *existing* task
    // (see the INTERACT_002 rejection tests below).
    let send = run_cli_success(
        &server,
        &[
            "--bearer",
            "secret",
            "--svc-param",
            "X-Trace: 123",
            "send",
            "hello from cli",
            "--accept-output",
            "text/plain",
            "--return-immediately",
        ],
    );
    let send_json: Value = serde_json::from_str(&send).unwrap();
    assert_eq!(send_json["task"]["id"], "task-send");
    assert_eq!(
        send_json["task"]["status"]["message"]["parts"][0]["text"],
        "Echo: hello from cli"
    );

    let get_task = run_cli_success(
        &server,
        &["task", "get", "task-send", "--history-length", "1"],
    );
    let task_json: Value = serde_json::from_str(&get_task).unwrap();
    assert_eq!(task_json["id"], "task-send");

    let list = run_cli_success(
        &server,
        &[
            "--compact",
            "task",
            "list",
            "--context-id",
            "ctx-send",
            "--status",
            "completed",
        ],
    );
    let list_json: Value = serde_json::from_str(list.trim()).unwrap();
    assert_eq!(list_json["tasks"].as_array().unwrap().len(), 1);

    let cancel = run_cli_success(&server, &["task", "cancel", "task-send"]);
    let cancel_json: Value = serde_json::from_str(&cancel).unwrap();
    assert_eq!(cancel_json["status"]["state"], "TASK_STATE_CANCELED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stream_and_subscribe_commands_work() {
    let server = TestServer::spawn().await;

    let stream_output = run_cli_success(
        &server,
        &[
            "--compact",
            "send",
            "streaming request",
            "--stream",
            "--task-id",
            "task-stream",
            "--context-id",
            "ctx-stream",
        ],
    );
    let stream_events = parse_json_lines(&stream_output);
    assert_eq!(stream_events.len(), 2);
    assert_eq!(
        stream_events[0]["statusUpdate"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    assert_eq!(stream_events[1]["task"]["id"], "task-stream");

    let subscribe_output =
        run_cli_success(&server, &["--compact", "task", "subscribe", "task-stream"]);
    let subscribe_events = parse_json_lines(&subscribe_output);
    assert_eq!(subscribe_events.len(), 2);
    assert_eq!(subscribe_events[1]["task"]["id"], "task-stream");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_config_crud_commands_work() {
    let server = TestServer::spawn().await;

    let create = run_cli_success(
        &server,
        &[
            "--compact",
            "--tenant",
            "tenant-1",
            "task",
            "push-config",
            "create",
            "task-1",
            "https://example.com/callback",
            "--config-id",
            "cfg-1",
            "--token",
            "tok-1",
            "--auth-scheme",
            "Bearer",
            "--auth-credentials",
            "secret",
        ],
    );
    let create_json: Value = serde_json::from_str(create.trim()).unwrap();
    assert_eq!(create_json["taskId"], "task-1");
    assert_eq!(create_json["id"], "cfg-1");
    assert_eq!(create_json["tenant"], "tenant-1");

    let get = run_cli_success(
        &server,
        &["--compact", "task", "push-config", "get", "task-1", "cfg-1"],
    );
    let get_json: Value = serde_json::from_str(get.trim()).unwrap();
    assert_eq!(get_json["authentication"]["scheme"], "Bearer");

    let list = run_cli_success(
        &server,
        &[
            "--compact",
            "task",
            "push-config",
            "list",
            "task-1",
            "--page-size",
            "10",
        ],
    );
    let list_json: Value = serde_json::from_str(list.trim()).unwrap();
    assert_eq!(list_json["configs"].as_array().unwrap().len(), 1);

    let delete = run_cli_success(
        &server,
        &[
            "--compact",
            "task",
            "push-config",
            "delete",
            "task-1",
            "cfg-1",
        ],
    );
    let delete_json: Value = serde_json::from_str(delete.trim()).unwrap();
    assert_eq!(delete_json["deleted"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_reports_a2a_and_non_a2a_errors() {
    let server = TestServer::spawn().await;

    // Unreachable agent: A2ACLI_ERR_UNREACHABLE, exit 3 (Appendix D).
    let base_url = unused_base_url().await;
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args([
            "--base-url",
            base_url.as_str(),
            "--output",
            "json",
            "card",
            "get",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "A2ACLI_ERR_UNREACHABLE");
    assert_eq!(output.status.code().unwrap(), 3);

    // A protocol failure carries the A2A error name and its numeric code
    // unchanged (§11.4), and exits 1 — the CLI did its job of conducting
    // and reporting the call.
    let (_stdout, stderr, code) =
        run_cli_failure_status(&server, &["card", "get", "--extended", "--tenant", "error"]);
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "UNSUPPORTED_OPERATION");
    assert_eq!(envelope["error"]["message"], "extended card denied");
    assert_eq!(envelope["error"]["a2aCode"], -32004);
    assert_eq!(code, 1);

    let (_stdout, stderr) = run_cli_failure(&server, &["send", "send-error"]);
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "INVALID_REQUEST");
    assert_eq!(envelope["error"]["a2aCode"], -32600);

    let (_stdout, stderr) = run_cli_failure(&server, &["task", "list", "--context-id", "error"]);
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "INVALID_PARAMS");
    assert_eq!(envelope["error"]["a2aCode"], -32602);

    let (_stdout, stderr) = run_cli_failure(&server, &["task", "get", "missing"]);
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "TASK_NOT_FOUND");
    assert_eq!(envelope["error"]["message"], "task not found: missing");
    assert_eq!(envelope["error"]["a2aCode"], -32001);

    let (_stdout, stderr) = run_cli_failure(&server, &["task", "cancel", "missing"]);
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "TASK_NOT_FOUND");

    let (_stdout, stderr) = run_cli_failure(&server, &["task", "subscribe", "stream-error"]);
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "INTERNAL_ERROR");
    assert_eq!(envelope["error"]["a2aCode"], -32603);

    let (_stdout, stderr) =
        run_cli_failure(&server, &["--compact", "send", "stream-error", "--stream"]);
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "INTERNAL_ERROR");

    let (_stdout, stderr) = run_cli_failure(
        &server,
        &[
            "task",
            "push-config",
            "create",
            "missing",
            "https://example.com/callback",
            "--config-id",
            "cfg-missing",
        ],
    );
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "TASK_NOT_FOUND");

    let (_stdout, stderr) = run_cli_failure(
        &server,
        &["task", "push-config", "get", "task-1", "missing"],
    );
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "TASK_NOT_FOUND");

    let (_stdout, stderr) = run_cli_failure(&server, &["task", "push-config", "list", "missing"]);
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "TASK_NOT_FOUND");

    let (_stdout, stderr) = run_cli_failure(
        &server,
        &["task", "push-config", "delete", "task-1", "missing"],
    );
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "TASK_NOT_FOUND");

    // A CLI-local usage failure: A2ACLI_ERR_USAGE, exit 2, no a2aCode.
    let (_stdout, stderr, code) = run_cli_failure_status(
        &server,
        &[
            "task",
            "push-config",
            "create",
            "task-1",
            "https://example.com/callback",
            "--auth-credentials",
            "secret",
        ],
    );
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "A2ACLI_ERR_USAGE");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--auth-credentials requires --auth-scheme")
    );
    assert!(envelope["error"]["a2aCode"].is_null());
    assert_eq!(code, 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_blocks_by_default_until_task_settles() {
    let server = TestServer::spawn().await;

    let output = run_cli_success(
        &server,
        &[
            "--compact",
            "--poll-interval",
            "10ms",
            "--timeout",
            "5s",
            "send",
            "start-pending",
        ],
    );
    let response: Value = serde_json::from_str(output.trim()).unwrap();
    assert_eq!(response["task"]["id"], "task-pending-send");
    assert_eq!(response["task"]["status"]["state"], "TASK_STATE_COMPLETED");

    // The blocking wait must have actually polled get_task rather than
    // returning the initial WORKING response.
    let count = *server
        .state
        .poll_counts
        .lock()
        .unwrap()
        .get("task-pending-send")
        .unwrap_or(&0);
    assert!(count >= POLLS_UNTIL_SETTLED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_async_returns_immediately_without_waiting() {
    let server = TestServer::spawn().await;

    let output = run_cli_success(&server, &["--compact", "--async", "send", "start-pending"]);
    let response: Value = serde_json::from_str(output.trim()).unwrap();
    assert_eq!(response["task"]["id"], "task-pending-send");
    assert_eq!(response["task"]["status"]["state"], "TASK_STATE_WORKING");

    // --async must skip polling entirely.
    let count = *server
        .state
        .poll_counts
        .lock()
        .unwrap()
        .get("task-pending-send")
        .unwrap_or(&0);
    assert_eq!(count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_get_wait_polls_until_settled() {
    let server = TestServer::spawn().await;

    let output = run_cli_success(
        &server,
        &[
            "--compact",
            "--wait",
            "--poll-interval",
            "10ms",
            "--timeout",
            "5s",
            "task",
            "get",
            "task-pending",
        ],
    );
    let task: Value = serde_json::from_str(output.trim()).unwrap();
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");

    let count = *server
        .state
        .poll_counts
        .lock()
        .unwrap()
        .get("task-pending")
        .unwrap_or(&0);
    assert!(count >= POLLS_UNTIL_SETTLED);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_get_without_wait_does_not_poll() {
    let server = TestServer::spawn().await;

    let output = run_cli_success(&server, &["--compact", "task", "get", "task-pending"]);
    let task: Value = serde_json::from_str(output.trim()).unwrap();
    // Still WORKING: a one-shot read must not have polled to settlement.
    assert_eq!(task["status"]["state"], "TASK_STATE_WORKING");

    let count = *server
        .state
        .poll_counts
        .lock()
        .unwrap()
        .get("task-pending")
        .unwrap_or(&0);
    assert_eq!(count, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_get_wait_times_out() {
    let server = TestServer::spawn().await;

    let (_stdout, stderr) = run_cli_failure(
        &server,
        &[
            "--wait",
            "--poll-interval",
            "10ms",
            "--timeout",
            "50ms",
            "task",
            "get",
            STUCK_TASK_ID,
        ],
    );
    assert!(stderr.contains("timed out"));
    assert!(stderr.contains(STUCK_TASK_ID));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_with_ordered_message_parts_and_media_type() {
    let server = TestServer::spawn().await;

    let output = run_cli_success(
        &server,
        &[
            "--compact",
            "send",
            "--text-part",
            "hello",
            "--file-part",
            "https://example.com/doc.pdf",
            "--media-type",
            "application/pdf",
            "--data-part",
            r#"{"priority":"high"}"#,
        ],
    );
    let response: Value = serde_json::from_str(output.trim()).unwrap();
    let parts = response["task"]["status"]["message"]["parts"]
        .as_array()
        .unwrap();

    assert_eq!(parts.len(), 3);
    assert_eq!(parts[0]["text"], "hello");
    assert_eq!(parts[1]["url"], "https://example.com/doc.pdf");
    assert_eq!(parts[1]["mediaType"], "application/pdf");
    assert!(parts[1].get("text").is_none());
    assert_eq!(parts[2]["data"]["priority"], "high");
    assert!(parts[2].get("mediaType").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_reads_local_file_part_and_stdin_data_part() {
    let server = TestServer::spawn().await;

    let mut file_path = std::env::temp_dir();
    file_path.push(format!("a2acli-test-file-part-{}.bin", std::process::id()));
    std::fs::write(&file_path, b"binary payload").unwrap();

    let output = AssertCommand::cargo_bin("a2acli")
        .unwrap()
        .args([
            "--base-url",
            server.base_url.as_str(),
            "--output",
            "json",
            "--compact",
            "send",
            "--text-part",
            "hello",
            "--file-part",
            file_path.to_str().unwrap(),
            "--data-part",
            "-",
        ])
        .write_stdin(r#"{"ok":true}"#)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    std::fs::remove_file(&file_path).unwrap();

    let response: Value = serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
    let parts = response["task"]["status"]["message"]["parts"]
        .as_array()
        .unwrap();

    assert_eq!(parts.len(), 3);
    assert_eq!(
        parts[1]["filename"],
        file_path.file_name().unwrap().to_str().unwrap()
    );
    let decoded = BASE64.decode(parts[1]["raw"].as_str().unwrap()).unwrap();
    assert_eq!(decoded, b"binary payload");
    assert_eq!(parts[2]["data"]["ok"], true);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_rejects_media_type_without_preceding_part() {
    let server = TestServer::spawn().await;

    let (_stdout, stderr) = run_cli_failure(
        &server,
        &[
            "send",
            "--media-type",
            "application/pdf",
            "--text-part",
            "hello",
        ],
    );
    assert!(stderr.contains("--media-type must immediately follow"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_rejects_positional_text_combined_with_part_flags() {
    let server = TestServer::spawn().await;

    let (_stdout, stderr) = run_cli_failure(&server, &["send", "hello", "--text-part", "world"]);
    assert!(stderr.contains("cannot be combined"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_falls_back_to_polling_when_the_stream_fails_to_open() {
    let server = TestServer::spawn().await;

    // "stream-open-error" fails send_streaming_message itself (not a
    // stream item), which must trigger the one-shot-send-plus-poll
    // fallback rather than propagating the error or hanging.
    let stdout = run_cli_success(
        &server,
        &["--compact", "send", "stream-open-error", "--stream"],
    );
    let response: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["task"]["status"]["state"], "TASK_STATE_COMPLETED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_exits_cleanly_on_a_message_only_reply() {
    let server = TestServer::spawn().await;

    let stdout = run_cli_success(&server, &["--compact", "send", "reply-only"]);
    let response: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["message"]["messageId"], "msg-reply-only");
    assert!(response.get("task").is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_part_reports_a_usage_error_for_a_missing_local_path() {
    let server = TestServer::spawn().await;

    let (_stdout, stderr) = run_cli_failure(
        &server,
        &["send", "--file-part", "/no/such/file-part-path.bin"],
    );
    assert!(stderr.contains("failed to read"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_part_reads_json_from_a_local_file() {
    let server = TestServer::spawn().await;

    let mut file_path = std::env::temp_dir();
    file_path.push(format!("a2acli-test-data-part-{}.json", std::process::id()));
    std::fs::write(&file_path, r#"{"from":"file"}"#).unwrap();

    let stdout = run_cli_success(
        &server,
        &[
            "--compact",
            "send",
            "--text-part",
            "hello",
            "--data-part",
            file_path.to_str().unwrap(),
        ],
    );
    std::fs::remove_file(&file_path).unwrap();

    let response: Value = serde_json::from_str(stdout.trim()).unwrap();
    let parts = response["task"]["status"]["message"]["parts"]
        .as_array()
        .unwrap();
    assert_eq!(parts[1]["data"]["from"], "file");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_part_rejects_a_value_that_is_neither_a_file_nor_valid_json() {
    let server = TestServer::spawn().await;

    let (_stdout, stderr) = run_cli_failure(
        &server,
        &["send", "--data-part", "not json and no such file"],
    );
    assert!(stderr.contains("--data-part must be a file path"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_part_reports_a_usage_error_when_stdin_is_not_utf8() {
    let server = TestServer::spawn().await;

    let output = AssertCommand::cargo_bin("a2acli")
        .unwrap()
        .args([
            "--base-url",
            server.base_url.as_str(),
            "send",
            "--data-part",
            "-",
        ])
        // Invalid UTF-8: read_to_string fails with an io::Error, exercising
        // --data-part -'s ReadFile error path (distinct from malformed-but-
        // valid-UTF-8 JSON, covered by the "neither a file nor valid JSON"
        // case above).
        .write_stdin(vec![0xFF, 0xFE, 0xFD])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("failed to read <stdin>"));
}

// Review fixes (a2aproject/a2a-rs#172 review from msardara).

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_stream_propagates_a_non_unsupported_error_instead_of_falling_back() {
    let server = TestServer::spawn().await;

    let (_stdout, stderr) =
        run_cli_failure(&server, &["send", "stream-open-real-error", "--stream"]);
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "INTERNAL_ERROR");
    assert_eq!(envelope["error"]["message"], "transport exploded");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_rejects_a_message_with_no_content_at_all() {
    let server = TestServer::spawn().await;

    let (_stdout, stderr) = run_cli_failure(&server, &["send"]);
    assert!(stderr.contains("message must have at least one part"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn media_type_alone_with_no_other_part_flag_is_rejected() {
    let server = TestServer::spawn().await;

    let (_stdout, stderr) = run_cli_failure(&server, &["send", "--media-type", "application/json"]);
    assert!(stderr.contains("--media-type must immediately follow"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn data_part_reports_the_real_error_for_an_unreadable_existing_path() {
    let server = TestServer::spawn().await;

    // A directory exists but can never be read as file content — read_to_string
    // fails with something other than NotFound, which must be reported as a
    // ReadFile error rather than silently retried as inline JSON.
    let mut dir_path = std::env::temp_dir();
    dir_path.push(format!("a2acli-test-data-part-dir-{}", std::process::id()));
    std::fs::create_dir_all(&dir_path).unwrap();

    let (_stdout, stderr) = run_cli_failure(
        &server,
        &["send", "--data-part", dir_path.to_str().unwrap()],
    );

    std::fs::remove_dir_all(&dir_path).unwrap();

    assert!(stderr.contains("failed to read"));
    assert!(!stderr.contains("must be a file path"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_is_the_default_output_format() {
    let server = TestServer::spawn().await;

    // No -o/--output flag at all: this is the §6.5 default, not an opt-in.
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args([
            "--base-url",
            server.base_url.as_str(),
            "task",
            "get",
            "task-1",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        !stdout.trim_start().starts_with('{'),
        "expected text, got: {stdout}"
    );
    assert!(stdout.contains("Task ID: task-1"));
    assert!(stdout.contains("Context ID: ctx-1"));
    assert!(stdout.contains("State: COMPLETED"));
    assert!(stdout.contains("Text:"));
    assert!(stdout.contains("seeded result"));

    // §11.1: stderr carries no diagnostics on a successful run.
    assert!(String::from_utf8(output.stderr).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn text_output_prints_resume_hint_on_input_required() {
    let server = TestServer::spawn().await;

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args([
            "--base-url",
            server.base_url.as_str(),
            "task",
            "get",
            "task-needs-input",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();

    assert!(stdout.contains("State: INPUT_REQUIRED"));
    assert!(stdout.contains("Resume with: a2acli send --task-id task-needs-input \"<reply>\""));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn json_output_is_still_available_via_output_flag() {
    let server = TestServer::spawn().await;

    let stdout = run_cli_success(&server, &["task", "get", "task-1"]);
    let task: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(task["id"], "task-1");
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
}

// INTERACT_002: a rejected --task-id surfaces the protocol error, exits
// non-zero, and never falls back to silently starting a new task.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_rejects_unknown_task_id_without_creating_one() {
    let server = TestServer::spawn().await;

    let (_stdout, stderr, code) =
        run_cli_failure_status(&server, &["send", "hello", "--task-id", "no-such-task"]);
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "TASK_NOT_FOUND");
    assert_ne!(code, 0);

    // The rejected attempt must not have silently created "no-such-task".
    let (_stdout, stderr) = run_cli_failure(&server, &["task", "get", "no-such-task"]);
    assert_eq!(
        parse_error_envelope(&stderr)["error"]["code"],
        "TASK_NOT_FOUND"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_rejects_task_id_with_mismatched_context_id() {
    let server = TestServer::spawn().await;

    // task-1 actually belongs to ctx-1 (seeded by TestServer::spawn).
    let (_stdout, stderr, code) = run_cli_failure_status(
        &server,
        &[
            "send",
            "hello",
            "--task-id",
            "task-1",
            "--context-id",
            "ctx-wrong",
        ],
    );
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "INVALID_PARAMS");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("ctx-1")
    );
    assert_ne!(code, 0);

    // task-1 itself must be unchanged by the rejected attempt.
    let stdout = run_cli_success(&server, &["task", "get", "task-1"]);
    let task: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(task["contextId"], "ctx-1");
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_accepts_task_id_alone_without_requiring_context_id() {
    // §8.1: --task-id MAY be given without --context-id; the server
    // resolves the task's own context, so this must succeed.
    let server = TestServer::spawn().await;

    let stdout = run_cli_success(
        &server,
        &["--compact", "send", "hello", "--task-id", "task-1"],
    );
    let response: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["task"]["id"], "task-1");
    assert_eq!(response["task"]["status"]["state"], "TASK_STATE_COMPLETED");
}

// INTERACT_005: the CLI is completely stateless — it never remembers a
// previous run's identifiers and replays them absent an explicit flag.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_sends_without_explicit_ids_never_reuse_a_previous_task() {
    let server = TestServer::spawn().await;

    // Two independent `send` invocations, neither passing --task-id: if the
    // CLI remembered the first run's task id and replayed it, the second
    // call would be rejected as "continuing" a task that belongs to a
    // different, non-existent context. Since neither run passes any
    // identifier, the server's own defaults apply identically both times,
    // and both must succeed exactly the same way.
    let first = run_cli_success(&server, &["--compact", "send", "hello once"]);
    let second = run_cli_success(&server, &["--compact", "send", "hello twice"]);

    let first: Value = serde_json::from_str(first.trim()).unwrap();
    let second: Value = serde_json::from_str(second.trim()).unwrap();
    assert_eq!(first["task"]["id"], second["task"]["id"]);
    assert_eq!(
        second["task"]["status"]["message"]["parts"][0]["text"],
        "Echo: hello twice"
    );
}

// INTERACT_001/003: contextId is an opaque, server-assigned grouping value
// the CLI passes through unchanged — never fabricated, and never assumed to
// mean a "chat session" (e.g. reused across unrelated task ids).

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_id_is_passed_through_opaquely_to_a_new_task() {
    let server = TestServer::spawn().await;

    // A fresh --context-id with no --task-id starts a new task grouped
    // under that context; the CLI must forward it verbatim rather than
    // validating, transforming, or fabricating one of its own.
    let stdout = run_cli_success(
        &server,
        &[
            "--compact",
            "send",
            "hello",
            "--context-id",
            "ctx-custom-123",
        ],
    );
    let response: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(response["task"]["contextId"], "ctx-custom-123");
}

// AUTH_001/AUTH_003/TX_003 (a2aproject/a2a-rs#169).

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn api_key_is_attached_as_a_header_to_the_card_fetch() {
    let server = TestServer::spawn().await;

    run_cli_success(&server, &["--api-key", "key-abc", "card", "get"]);

    let headers = server.state.card_headers.lock().unwrap().clone();
    assert_eq!(headers.last().unwrap().2.as_deref(), Some("key-abc"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insecure_flag_prints_a_warning_and_names_the_credential_risk() {
    let server = TestServer::spawn().await;

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args([
            "--base-url",
            server.base_url.as_str(),
            "--insecure",
            "--bearer",
            "secret",
            "card",
            "get",
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--insecure disables TLS certificate verification"));
    assert!(stderr.contains("--bearer/--api-key"));

    // Without a credential, the warning is still printed but doesn't
    // mention sending one.
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args([
            "--base-url",
            server.base_url.as_str(),
            "--insecure",
            "card",
            "get",
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--insecure disables TLS certificate verification"));
    assert!(!stderr.contains("--bearer/--api-key"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_insecure_flag_means_no_warning() {
    let server = TestServer::spawn().await;

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args(["--base-url", server.base_url.as_str(), "card", "get"])
        .assert()
        .success()
        .get_output()
        .clone();
    assert!(String::from_utf8(output.stderr).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_interface_tenant_is_used_absent_an_explicit_tenant_flag() {
    let server = TestServer::spawn_with_card_tenant(Some("card-declared-tenant")).await;

    run_cli_success(&server, &["--compact", "send", "hello"]);

    let received = server.state.received_send_tenants.lock().unwrap().clone();
    assert_eq!(received, vec![Some("card-declared-tenant".to_string())]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_tenant_flag_overrides_the_interface_tenant() {
    let server = TestServer::spawn_with_card_tenant(Some("card-declared-tenant")).await;

    run_cli_success(
        &server,
        &["--compact", "--tenant", "explicit-tenant", "send", "hello"],
    );

    let received = server.state.received_send_tenants.lock().unwrap().clone();
    assert_eq!(received, vec![Some("explicit-tenant".to_string())]);
}

// AUTH_004 (a2aproject/a2a-rs#169).

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debug_flag_emits_diagnostics_without_leaking_the_bearer_token() {
    let server = TestServer::spawn().await;

    // --debug's diagnostics come from the client-call interceptor pipeline
    // (LoggingInterceptor), which only runs for actual A2A operations —
    // unlike a bare `card get`, `send` goes through resolve_client and so
    // exercises it.
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args([
            "--base-url",
            server.base_url.as_str(),
            "--debug",
            "--bearer",
            "super-secret-token",
            "--async",
            "send",
            "hello",
        ])
        .assert()
        .success()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();

    // --debug produces *some* diagnostic output...
    assert!(!stderr.is_empty());
    // ...but never the credential value, regardless of verbosity.
    assert!(!stderr.contains("super-secret-token"));
}
