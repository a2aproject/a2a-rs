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

#[derive(Default)]
struct ServerState {
    tasks: Mutex<BTreeMap<String, Task>>,
    push_configs: Mutex<BTreeMap<(String, String), TaskPushNotificationConfig>>,
    card_headers: Mutex<Vec<(Option<String>, Option<String>)>>,
    /// Number of times `get_task` has been called for each of the
    /// poll-until-settled fixture task ids below.
    poll_counts: Mutex<BTreeMap<String, u32>>,
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
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(ServerState::default());

        {
            let mut tasks = state.tasks.lock().unwrap();
            tasks.insert(
                "task-1".to_string(),
                make_task("task-1", "ctx-1", TaskState::Completed, "seeded result"),
            );
        }

        let public_card = make_agent_card(&base_url, "Fixture Agent");
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
        let text = req.message.text().unwrap_or_default();
        if text == "send-error" {
            return Err(A2AError::invalid_request("send failed"));
        }
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

fn run_cli_success(server: &TestServer, args: &[&str]) -> String {
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    command.args(["--base-url", server.base_url.as_str()]);
    command.args(args);
    let output = command.assert().success().get_output().stdout.clone();
    String::from_utf8(output).unwrap()
}

fn run_cli_failure(server: &TestServer, args: &[&str]) -> (String, String) {
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    command.args(["--base-url", server.base_url.as_str()]);
    command.args(args);
    let output = command.assert().failure().get_output().clone();
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
    )
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
            "--bearer-token",
            "secret",
            "--header",
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
            "--binding",
            "http-json",
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

    let send = run_cli_success(
        &server,
        &[
            "--bearer-token",
            "secret",
            "--header",
            "X-Trace: 123",
            "send",
            "hello from cli",
            "--task-id",
            "task-send",
            "--context-id",
            "ctx-send",
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

    let base_url = unused_base_url().await;
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args(["--base-url", base_url.as_str(), "card", "get"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("http request failed:"));

    let (_stdout, stderr) =
        run_cli_failure(&server, &["card", "get", "--extended", "--tenant", "error"]);
    assert!(stderr.contains("a2a error -32004: extended card denied"));

    let (_stdout, stderr) = run_cli_failure(&server, &["send", "send-error"]);
    assert!(stderr.contains("a2a error -32600: send failed"));

    let (_stdout, stderr) = run_cli_failure(&server, &["task", "list", "--context-id", "error"]);
    assert!(stderr.contains("a2a error -32602: list failed"));

    let (_stdout, stderr) = run_cli_failure(&server, &["task", "get", "missing"]);
    assert!(stderr.contains("a2a error -32001: task not found: missing"));

    let (_stdout, stderr) = run_cli_failure(&server, &["task", "cancel", "missing"]);
    assert!(stderr.contains("a2a error -32001: task not found: missing"));

    let (_stdout, stderr) = run_cli_failure(&server, &["task", "subscribe", "stream-error"]);
    assert!(stderr.contains("a2a error -32603: stream failed"));

    let (_stdout, stderr) =
        run_cli_failure(&server, &["--compact", "send", "stream-error", "--stream"]);
    assert!(stderr.contains("a2a error -32603: stream failed"));

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
    assert!(stderr.contains("a2a error -32001: task not found: missing"));

    let (_stdout, stderr) = run_cli_failure(
        &server,
        &["task", "push-config", "get", "task-1", "missing"],
    );
    assert!(stderr.contains("a2a error -32001: task not found: task-1"));

    let (_stdout, stderr) = run_cli_failure(&server, &["task", "push-config", "list", "missing"]);
    assert!(stderr.contains("a2a error -32001: task not found: missing"));

    let (_stdout, stderr) = run_cli_failure(
        &server,
        &["task", "push-config", "delete", "task-1", "missing"],
    );
    assert!(stderr.contains("a2a error -32001: task not found: task-1"));

    let (_stdout, stderr) = run_cli_failure(
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
    assert!(stderr.contains("invalid input: --auth-credentials requires --auth-scheme"));
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
    assert!(stderr.contains("a2a error"));
    assert!(stderr.contains("transport exploded"));
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
