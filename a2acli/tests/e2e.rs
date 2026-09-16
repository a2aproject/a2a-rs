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

/// (Authorization, x-api-key, x-trace-id, A2A-Version) recorded per call to
/// the *agent*. The version is recorded as every value joined by `,`, so a
/// test can tell one explicit version from two appended ones.
/// Kept separate from [`CardHeaders`] because the card fetch and the agent
/// call go out over different clients: a credential reaching one is no
/// evidence it reaches the other.
type AgentHeaders = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Every value sent for `name`, joined by `,` — distinguishes a header set
/// once from one appended to twice.
fn joined_header_values(headers: &HeaderMap, name: &str) -> Option<String> {
    let values: Vec<String> = headers
        .get_all(name)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
        .collect();
    (!values.is_empty()).then(|| values.join(","))
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(ToOwned::to_owned)
}

#[derive(Default)]
struct ServerState {
    tasks: Mutex<BTreeMap<String, Task>>,
    push_configs: Mutex<BTreeMap<(String, String), TaskPushNotificationConfig>>,
    card_headers: Mutex<Vec<CardHeaders>>,
    agent_headers: Mutex<Vec<AgentHeaders>>,
    /// Number of times `get_task` has been called for each of the
    /// poll-until-settled fixture task ids below.
    poll_counts: Mutex<BTreeMap<String, u32>>,
    /// The `tenant` field of each `send_message` request received, in
    /// order — for TX_003's "selected interface's own tenant, absent an
    /// explicit --tenant" fallback.
    received_send_tenants: Mutex<Vec<Option<String>>>,
    /// Every RPC that actually reached the agent, in order. A capability
    /// pre-flight (§13.3) is only doing its job if the gated call is absent
    /// from here — the CLI's own output cannot show that.
    received_calls: Mutex<Vec<&'static str>>,
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
    /// Spawn with a public card declaring exactly `capabilities`, for the
    /// §13.3 pre-flight tests.
    async fn spawn_with_capabilities(capabilities: AgentCapabilities) -> Self {
        Self::spawn_configured(None, Some(capabilities)).await
    }

    async fn spawn_with_card_tenant(tenant: Option<&str>) -> Self {
        Self::spawn_configured(tenant, None).await
    }

    async fn spawn_configured(
        tenant: Option<&str>,
        capabilities: Option<AgentCapabilities>,
    ) -> Self {
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
                "task-failed".to_string(),
                make_task("task-failed", "ctx-failed", TaskState::Failed, "it broke"),
            );
            tasks.insert(
                "task-rejected".to_string(),
                make_task(
                    "task-rejected",
                    "ctx-rejected",
                    TaskState::Rejected,
                    "not allowed",
                ),
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
        if let Some(capabilities) = capabilities {
            public_card.capabilities = capabilities;
        }
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
        let custom_path_card = public_card.clone();

        // Record the headers of every call that reaches the agent's own
        // endpoints, so a test can tell "the credential was attached to the
        // card fetch" apart from "the credential was attached to the RPC".
        let agent_state = state.clone();
        let record_agent_headers = axum::middleware::from_fn(
            move |headers: HeaderMap,
                  request: axum::extract::Request,
                  next: axum::middleware::Next| {
                let state = agent_state.clone();
                async move {
                    state.agent_headers.lock().unwrap().push((
                        header_value(&headers, header::AUTHORIZATION.as_str()),
                        header_value(&headers, "x-api-key"),
                        header_value(&headers, "x-trace-id"),
                        joined_header_values(&headers, "a2a-version"),
                    ));
                    next.run(request).await
                }
            },
        );

        let app = Router::new()
            .route(
                WELL_KNOWN_AGENT_CARD_PATH,
                get(move |headers: HeaderMap| {
                    let state = card_state.clone();
                    let card = card.clone();
                    async move {
                        state.card_headers.lock().unwrap().push((
                            header_value(&headers, header::AUTHORIZATION.as_str()),
                            header_value(&headers, "x-test"),
                            header_value(&headers, "x-api-key"),
                        ));
                        (StatusCode::OK, Json(card))
                    }
                }),
            )
            // The same card at a path that is *not* the well-known one, so a
            // test can prove a full card URL is used as-is rather than
            // having the well-known path appended to it.
            .route(
                "/custom/card.json",
                get({
                    let card = custom_path_card.clone();
                    move || {
                        let card = card.clone();
                        async move { (StatusCode::OK, Json(card)) }
                    }
                }),
            )
            .nest(
                "/jsonrpc",
                jsonrpc_router(handler.clone()).layer(record_agent_headers.clone()),
            )
            .nest("/rest", rest_router(handler).layer(record_agent_headers));

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
            .received_calls
            .lock()
            .unwrap()
            .push("send_message");
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
        // A task that lands FAILED on the first reply, so `send`'s own
        // reporting path can be observed naming a non-success outcome
        // (EXIT_002) rather than only `task get`'s.
        let state = if text == "send-failing" {
            TaskState::Failed
        } else {
            TaskState::Completed
        };
        let task = make_task_with_parts(&task_id, &context_id, state, response_parts);
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
        self.state
            .received_calls
            .lock()
            .unwrap()
            .push("send_streaming_message");
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
        self.state
            .received_calls
            .lock()
            .unwrap()
            .push("subscribe_to_task");
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
        self.state
            .received_calls
            .lock()
            .unwrap()
            .push("create_push_config");
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
            .received_calls
            .lock()
            .unwrap()
            .push("get_push_config");
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
        self.state
            .received_calls
            .lock()
            .unwrap()
            .push("list_push_configs");
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
        self.state
            .received_calls
            .lock()
            .unwrap()
            .push("delete_push_config");
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
        self.state
            .received_calls
            .lock()
            .unwrap()
            .push("get_extended_agent_card");
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
    command.args(["--agent-card", server.base_url.as_str(), "--output", "json"]);
    command.args(args);
    let output = command.assert().success().get_output().stdout.clone();
    String::from_utf8(output).unwrap()
}

fn run_cli_failure(server: &TestServer, args: &[&str]) -> (String, String) {
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    command.args(["--agent-card", server.base_url.as_str(), "--output", "json"]);
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
    command.args(["--agent-card", server.base_url.as_str(), "--output", "json"]);
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
            "--agent-card",
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
            "--agent-card",
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
            "--agent-card",
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
            "--agent-card",
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
            "--agent-card",
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

/// AUTH_003: the credential has to reach the *agent*, not only the card
/// fetch — those go out over separate clients, and `card get` never builds
/// the agent client at all, so it cannot witness this.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credentials_and_service_params_reach_the_agent_not_only_the_card_fetch() {
    let server = TestServer::spawn().await;

    run_cli_success(
        &server,
        &[
            "--api-key",
            "key-xyz",
            "--bearer",
            "token-xyz",
            "--svc-param",
            "X-Trace-Id:trace-1",
            "task",
            "get",
            "task-1",
        ],
    );

    let headers = server.state.agent_headers.lock().unwrap().clone();
    let (authorization, api_key, trace_id, _version) = headers
        .last()
        .cloned()
        .expect("the agent call should have been recorded");
    assert_eq!(api_key.as_deref(), Some("key-xyz"));
    assert_eq!(authorization.as_deref(), Some("Bearer token-xyz"));
    // `--svc-param` is a general transport-level pair, not a credential, and
    // rides along on the same call.
    assert_eq!(trace_id.as_deref(), Some("trace-1"));
}

/// `--insecure` swaps in transports built on a client that skips
/// certificate verification; against a plain-HTTP fixture the call still has
/// to succeed and still carry the credential, so the escape hatch can't
/// quietly drop either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insecure_flag_still_builds_a_working_authenticated_client() {
    let server = TestServer::spawn().await;

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args([
            "--agent-card",
            server.base_url.as_str(),
            "--output",
            "json",
            "--insecure",
            "--api-key",
            "key-insecure",
            "task",
            "get",
            "task-1",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let task: Value =
        serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
    assert_eq!(task["id"], "task-1");

    let headers = server.state.agent_headers.lock().unwrap().clone();
    assert_eq!(
        headers.last().unwrap().1.as_deref(),
        Some("key-insecure"),
        "--insecure must not drop the credential"
    );
    // §12.1/AUTH_003: never silent, even on a call that succeeded.
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--insecure disables TLS certificate verification"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn insecure_flag_prints_a_warning_and_names_the_credential_risk() {
    let server = TestServer::spawn().await;

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args([
            "--agent-card",
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
            "--agent-card",
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
        .args(["--agent-card", server.base_url.as_str(), "card", "get"])
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
async fn a_declared_interface_tenant_is_not_displaced_by_the_tenant_flag() {
    let server = TestServer::spawn_with_card_tenant(Some("card-declared-tenant")).await;

    run_cli_success(
        &server,
        &["--compact", "--tenant", "explicit-tenant", "send", "hello"],
    );

    // A2A §8.3.2 rule 4 / SPEC.md §13.1: the tenant sent MUST be exactly
    // the value the selected interface declares. --tenant does not get to
    // substitute another one (#199).
    let received = server.state.received_send_tenants.lock().unwrap().clone();
    assert_eq!(received, vec![Some("card-declared-tenant".to_string())]);
}

/// Where the card declares no tenant there is nothing to preserve, so an
/// explicit --tenant is used as given.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tenant_flag_applies_when_the_interface_declares_none() {
    let server = TestServer::spawn().await;

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
            "--agent-card",
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

// CONFIG_001 (a2aproject/a2a-rs#170).

/// A scratch working directory with `HOME`/`XDG_CONFIG_HOME` also pointed at
/// it (so `~/.config/a2a-cli/.env` resolves somewhere empty and controlled)
/// — isolates `.env`-discovery tests from both the real developer machine
/// and from each other despite running in parallel.
struct ConfigScratchDir {
    path: std::path::PathBuf,
}

impl ConfigScratchDir {
    fn new(name: &str) -> Self {
        let mut path = std::env::temp_dir();
        path.push(format!("a2acli-test-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn write_local_env(&self, contents: &str) {
        std::fs::write(self.path.join(".env"), contents).unwrap();
    }

    /// Write the global `.env` at `<xdg>/a2a-cli/.env` inside this scratch
    /// dir, and return the `<xdg>` directory for the caller to export as
    /// `XDG_CONFIG_HOME`.
    fn write_xdg_env(&self, contents: &str) -> std::path::PathBuf {
        let xdg = self.path.join("xdg");
        let dir = xdg.join("a2a-cli");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".env"), contents).unwrap();
        xdg
    }

    fn command(&self, server: &TestServer) -> StdCommand {
        let mut command = self.command_without_agent();
        command.args(["--agent-card", server.base_url.as_str()]);
        command
    }

    /// Like [`Self::command`] but with no agent configured at all — no
    /// `--agent-card`, no `A2ACLI_*` in the environment and no discoverable
    /// `.env` — for the commands that must work without one.
    fn command_without_agent(&self) -> StdCommand {
        let mut command = StdCommand::cargo_bin("a2acli").unwrap();
        command
            .current_dir(&self.path)
            .env("HOME", &self.path)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("A2ACLI_TENANT")
            .env_remove("A2ACLI_BASE_URL")
            .env_remove("A2ACLI_AGENT_CARD")
            .env_remove("A2ACLI_ENDPOINT");
        command
    }
}

impl Drop for ConfigScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_dotenv_file_sets_a_default_without_a_flag() {
    let server = TestServer::spawn().await;
    let scratch = ConfigScratchDir::new("local-dotenv");
    scratch.write_local_env("A2ACLI_TENANT=file-tenant\n");

    let output = scratch
        .command(&server)
        .args(["--output", "json", "--compact", "send", "hello"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let response: Value = serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
    assert_eq!(response["task"]["id"], "task-send");

    let received = server.state.received_send_tenants.lock().unwrap().clone();
    assert_eq!(received.last().unwrap().as_deref(), Some("file-tenant"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_env_var_outranks_a_local_dotenv_file() {
    let server = TestServer::spawn().await;
    let scratch = ConfigScratchDir::new("env-outranks-file");
    scratch.write_local_env("A2ACLI_TENANT=file-tenant\n");

    let output = scratch
        .command(&server)
        .env("A2ACLI_TENANT", "real-env-tenant")
        .args(["--output", "json", "--compact", "send", "hello"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let response: Value = serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
    assert_eq!(response["task"]["id"], "task-send");

    let received = server.state.received_send_tenants.lock().unwrap().clone();
    assert_eq!(received.last().unwrap().as_deref(), Some("real-env-tenant"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_config_flag_overrides_local_dotenv_discovery() {
    let server = TestServer::spawn().await;
    let scratch = ConfigScratchDir::new("explicit-config-flag");
    // The auto-discovered local .env says one thing...
    scratch.write_local_env("A2ACLI_TENANT=cwd-tenant\n");
    // ...but an explicit --config file says another, and must win.
    let explicit_path = scratch.path.join("prod.env");
    std::fs::write(&explicit_path, "A2ACLI_TENANT=explicit-tenant\n").unwrap();

    let output = scratch
        .command(&server)
        .args([
            "--config",
            explicit_path.to_str().unwrap(),
            "--output",
            "json",
            "--compact",
            "send",
            "hello",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let response: Value = serde_json::from_str(String::from_utf8(output).unwrap().trim()).unwrap();
    assert_eq!(response["task"]["id"], "task-send");

    let received = server.state.received_send_tenants.lock().unwrap().clone();
    assert_eq!(received.last().unwrap().as_deref(), Some("explicit-tenant"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_show_reports_effective_settings_and_sources() {
    let server = TestServer::spawn().await;
    let scratch = ConfigScratchDir::new("config-show");

    let output = scratch
        .command(&server)
        .args(["--bearer", "super-secret", "config", "show"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();

    // A flag-supplied credential is redacted, and its source is "flag".
    assert!(stdout.contains("bearer: (set, redacted) (source: flag)"));
    assert!(!stdout.contains("super-secret"));
    // An untouched setting reports the built-in default and its source.
    assert!(stdout.contains("(source: built-in default)"));
    assert!(stdout.contains("output: text"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_show_reports_dotenv_file_as_the_source() {
    let server = TestServer::spawn().await;
    let scratch = ConfigScratchDir::new("config-show-dotenv");
    scratch.write_local_env("A2ACLI_TENANT=file-tenant\n");

    let output = scratch
        .command(&server)
        .args(["config", "show"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    assert!(stdout.contains("tenant: file-tenant (source: local .env file)"));
}

/// The global `.env` lives at `~/.config/a2a-cli/.env`, but `$XDG_CONFIG_HOME`
/// relocates `~/.config` when it is set — so the lookup has to honor it
/// rather than hard-coding the home-relative path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_dotenv_is_discovered_under_xdg_config_home() {
    let server = TestServer::spawn().await;
    let scratch = ConfigScratchDir::new("config-show-xdg");
    let xdg = scratch.write_xdg_env("A2ACLI_TENANT=xdg-tenant\n");

    let output = scratch
        .command(&server)
        .env("XDG_CONFIG_HOME", &xdg)
        .args(["config", "show"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    assert!(
        stdout.contains("tenant: xdg-tenant (source: global .env file)"),
        "{stdout}"
    );
}

/// `config show` has to report an explicit `--transport` preference in the
/// order it will actually be applied — the point of the command is to settle
/// which transport wins without having to guess.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_show_reports_an_explicit_transport_preference_in_order() {
    let server = TestServer::spawn().await;
    let scratch = ConfigScratchDir::new("config-show-transport");

    let output = scratch
        .command(&server)
        .args([
            "--transport",
            "rest",
            "--transport",
            "jsonrpc",
            "config",
            "show",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    assert!(
        stdout.contains("transport: rest,jsonrpc (source: flag)"),
        "{stdout}"
    );

    // Absent the flag, the card's own ordering is reported rather than a
    // fabricated default preference.
    let output = scratch
        .command(&server)
        .args(["config", "show"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8(output).unwrap();
    assert!(
        stdout.contains("transport: (agent card's own order)"),
        "{stdout}"
    );
}

// CARD_GET_001 / §10.1 (a2aproject/a2a-rs#178): the Agent Card reference.

/// A bare host or origin gets the well-known path appended — the form every
/// other test in this file relies on, asserted here explicitly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_card_accepts_a_bare_origin() {
    let server = TestServer::spawn().await;

    let stdout = run_cli_success(&server, &["card", "get"]);
    let card: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(card["name"], "Fixture Agent");
    // The card fetch did reach the well-known handler.
    assert_eq!(server.state.card_headers.lock().unwrap().len(), 1);
}

/// A reference that already carries a path is a full card URL and is used
/// as-is — the well-known path must not be appended to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_card_accepts_a_full_card_url_without_appending_the_well_known_path() {
    let server = TestServer::spawn().await;

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let stdout = command
        .args([
            "--agent-card",
            &format!("{}/custom/card.json", server.base_url),
            "--output",
            "json",
            "card",
            "get",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let card: Value = serde_json::from_str(String::from_utf8(stdout).unwrap().trim()).unwrap();
    assert_eq!(card["name"], "Fixture Agent");
    // Served from /custom/card.json, so the well-known handler was never hit.
    assert!(server.state.card_headers.lock().unwrap().is_empty());
}

fn write_card_file(dir: &std::path::Path, name: &str, contents: &str) -> std::path::PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    path
}

fn scratch_dir(name: &str) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("a2acli-card-{name}-{}", std::process::id()));
    path
}

const FILE_CARD_JSON: &str = r#"{
  "name": "File Agent",
  "description": "resolved from a local file",
  "version": "1.0",
  "supportedInterfaces": [],
  "capabilities": {},
  "defaultInputModes": [],
  "defaultOutputModes": [],
  "skills": []
}"#;

/// A plain filesystem path resolves to a card on disk — how the tool is
/// driven in tests and air-gapped environments, with no agent running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_card_reads_a_plain_local_path() {
    let dir = scratch_dir("plain-path");
    let path = write_card_file(&dir, "card.json", FILE_CARD_JSON);

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args([
            "--agent-card",
            path.to_str().unwrap(),
            "--output",
            "json",
            "card",
            "get",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let card: Value =
        serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
    assert_eq!(card["name"], "File Agent");
    assert!(String::from_utf8(output.stderr).unwrap().is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// The `file://` form resolves the same way as a plain path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn agent_card_reads_a_file_url() {
    let dir = scratch_dir("file-url");
    let path = write_card_file(&dir, "card.json", FILE_CARD_JSON);

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let stdout = command
        .args([
            "--agent-card",
            &format!("file://{}", path.display()),
            "--output",
            "json",
            "card",
            "get",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let card: Value = serde_json::from_str(String::from_utf8(stdout).unwrap().trim()).unwrap();
    assert_eq!(card["name"], "File Agent");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Appendix D: a card file that isn't there is `CARD_NOT_FOUND`/3, the same
/// class and status as a card URL that answers non-2xx — a caller branching
/// on the exit code needn't know which form was used.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_card_file_reports_card_not_found() {
    let dir = scratch_dir("missing");
    let path = dir.join("absent.json");

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args(["--agent-card", path.to_str().unwrap(), "card", "get"])
        .assert()
        .failure()
        .get_output()
        .clone();

    let envelope = parse_error_envelope(&String::from_utf8(output.stderr).unwrap());
    assert_eq!(envelope["error"]["code"], "A2ACLI_ERR_CARD_NOT_FOUND");
    assert!(envelope["error"]["hint"].is_string());
    assert_eq!(output.status.code().unwrap(), 3);
}

/// A file that is there but isn't a card is `CARD_INVALID`/1 — the same
/// split the HTTP path makes between a bad response and a bad body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_file_that_is_not_a_card_reports_card_invalid() {
    let dir = scratch_dir("invalid");
    let path = write_card_file(&dir, "bad.json", r#"{"not":"a card"}"#);

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args(["--agent-card", path.to_str().unwrap(), "card", "get"])
        .assert()
        .failure()
        .get_output()
        .clone();

    let envelope = parse_error_envelope(&String::from_utf8(output.stderr).unwrap());
    assert_eq!(envelope["error"]["code"], "A2ACLI_ERR_CARD_INVALID");
    assert_eq!(output.status.code().unwrap(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// `--base-url` keeps working so pre-#178 invocations don't break, but says
/// it is deprecated. The warning goes to stderr and leaves stdout intact.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn base_url_still_resolves_a_card_but_warns_it_is_deprecated() {
    let server = TestServer::spawn().await;

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args([
            "--base-url",
            server.base_url.as_str(),
            "--output",
            "json",
            "card",
            "get",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let card: Value =
        serde_json::from_str(String::from_utf8(output.stdout).unwrap().trim()).unwrap();
    assert_eq!(card["name"], "Fixture Agent");

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("--base-url is deprecated"), "{stderr}");
    assert!(stderr.contains("--agent-card"), "{stderr}");
}

/// The warning names a flag the caller actually passed: leaving `--base-url`
/// at its built-in default is not a deprecated invocation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_base_url_does_not_warn_about_deprecation() {
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args(["config", "show"])
        .assert()
        .success()
        .get_output()
        .clone();

    assert!(
        !String::from_utf8(output.stderr)
            .unwrap()
            .contains("deprecated")
    );
}

/// §7.2: `--endpoint` connects straight to an interface, so no card is
/// fetched at all — asserted against the fixture's card handler never being
/// reached, not merely against the command succeeding.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_connects_without_resolving_a_card() {
    let server = TestServer::spawn().await;

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let stdout = command
        .args([
            "--endpoint",
            &format!("{}/jsonrpc", server.base_url),
            "--transport",
            "jsonrpc",
            "--output",
            "json",
            "task",
            "get",
            "task-1",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let task: Value = serde_json::from_str(String::from_utf8(stdout).unwrap().trim()).unwrap();
    assert_eq!(task["id"], "task-1");
    assert!(
        server.state.card_headers.lock().unwrap().is_empty(),
        "--endpoint must not resolve an agent card"
    );
}

/// With no card to declare the binding, the caller must name exactly one
/// transport: zero leaves the protocol ambiguous, more than one asks for a
/// preference order over a single interface.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_requires_exactly_one_transport() {
    let server = TestServer::spawn().await;
    let endpoint = format!("{}/jsonrpc", server.base_url);

    for transports in [vec![], vec!["jsonrpc", "rest"]] {
        let mut command = StdCommand::cargo_bin("a2acli").unwrap();
        command.args(["--endpoint", endpoint.as_str()]);
        for transport in &transports {
            command.args(["--transport", transport]);
        }
        let output = command
            .args(["task", "get", "task-1"])
            .assert()
            .failure()
            .get_output()
            .clone();

        let envelope = parse_error_envelope(&String::from_utf8(output.stderr).unwrap());
        assert_eq!(
            envelope["error"]["code"],
            "A2ACLI_ERR_USAGE",
            "with {} transport(s)",
            transports.len()
        );
        assert_eq!(output.status.code().unwrap(), 2);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoint_and_agent_card_are_mutually_exclusive() {
    let server = TestServer::spawn().await;

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args([
            "--agent-card",
            server.base_url.as_str(),
            "--endpoint",
            &format!("{}/jsonrpc", server.base_url),
            "--transport",
            "jsonrpc",
            "task",
            "get",
            "task-1",
        ])
        .assert()
        .failure()
        .get_output()
        .clone();

    let stderr = String::from_utf8(output.stderr).unwrap();
    let envelope = parse_error_envelope(&stderr);
    assert_eq!(envelope["error"]["code"], "A2ACLI_ERR_USAGE");
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("mutually exclusive"),
        "{stderr}"
    );
    assert_eq!(output.status.code().unwrap(), 2);
}

/// `config show` answers "which agent am I talking to?" in one line, so the
/// reader doesn't have to apply the reference rules themselves.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_show_reports_the_resolved_card_reference() {
    let server = TestServer::spawn().await;
    let scratch = ConfigScratchDir::new("resolved-card");

    let stdout = String::from_utf8(
        scratch
            .command(&server)
            .args(["config", "show"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(
        stdout.contains(&format!(
            "resolved-card: {}/.well-known/agent-card.json",
            server.base_url
        )),
        "{stdout}"
    );

    // Under --endpoint there is no card to resolve, and the line says so
    // rather than reporting a URL that is never fetched.
    let stdout = String::from_utf8(
        scratch
            .command(&server)
            .args([
                "--endpoint",
                &format!("{}/jsonrpc", server.base_url),
                "--transport",
                "jsonrpc",
                "config",
                "show",
            ])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(
        stdout.contains("resolved-card: (none; --endpoint"),
        "{stdout}"
    );
}

/// A local card file is reported as the file it will be read from, not as a
/// URL that would never be fetched. Built without `ConfigScratchDir`, whose
/// base command already supplies `--agent-card`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_show_reports_a_local_card_file_as_the_resolved_card() {
    let dir = scratch_dir("resolved-card-file");
    let path = write_card_file(&dir, "card.json", FILE_CARD_JSON);

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let stdout = command
        .env_remove("A2ACLI_AGENT_CARD")
        .env_remove("A2ACLI_BASE_URL")
        .env_remove("A2ACLI_ENDPOINT")
        .args(["--agent-card", path.to_str().unwrap(), "config", "show"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();

    let stdout = String::from_utf8(stdout).unwrap();
    assert!(
        stdout.contains(&format!("resolved-card: file://{}", path.display())),
        "{stdout}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// VER_001 / §13.2 (a2aproject/a2a-rs#179): protocol version signaling.

/// §13.2: the version is signaled on **every** request, and exactly once —
/// A2A reads an empty value as 0.3, and two values would leave which one
/// applies undefined.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a2a_version_is_signaled_once_on_every_request() {
    let server = TestServer::spawn().await;

    run_cli_success(&server, &["task", "get", "task-1"]);

    let headers = server.state.agent_headers.lock().unwrap().clone();
    assert!(
        !headers.is_empty(),
        "the agent call should have been recorded"
    );
    for (_, _, _, version) in &headers {
        // The fixture card declares this build's own version, so nothing is
        // negotiated away and exactly one value is sent.
        assert_eq!(version.as_deref(), Some(a2a::VERSION), "{headers:?}");
    }
}

/// An explicit `--a2a-version` is what goes on the wire, replacing the
/// client library's default rather than being appended to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_a2a_version_replaces_the_default_rather_than_appending() {
    let server = TestServer::spawn().await;

    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args([
            "--agent-card",
            server.base_url.as_str(),
            "--output",
            "json",
            "--a2a-version",
            "1.4",
            "task",
            "get",
            "task-1",
        ])
        .assert()
        .success()
        .get_output()
        .clone();

    let headers = server.state.agent_headers.lock().unwrap().clone();
    let version = headers.last().unwrap().3.clone();
    // Exactly "1.4" — not "1.0,1.4", which is what appending would produce.
    assert_eq!(version.as_deref(), Some("1.4"), "{headers:?}");

    // §13.2: no silent change of the signaled version.
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("signaling A2A-Version 1.4"), "{stderr}");
}

/// §11.6: a bad `--a2a-version` is a usage error, reported before any
/// network work — it must not need a reachable agent to surface.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invalid_a2a_version_is_a_usage_error_without_contacting_the_agent() {
    let server = TestServer::spawn().await;

    for bad in ["0.3", "2.0", "nonsense"] {
        let mut command = StdCommand::cargo_bin("a2acli").unwrap();
        let output = command
            .args([
                "--agent-card",
                server.base_url.as_str(),
                "--a2a-version",
                bad,
                "card",
                "get",
            ])
            .assert()
            .failure()
            .get_output()
            .clone();

        let envelope = parse_error_envelope(&String::from_utf8(output.stderr).unwrap());
        assert_eq!(
            envelope["error"]["code"], "A2ACLI_ERR_USAGE",
            "version {bad}"
        );
        assert_eq!(output.status.code().unwrap(), 2, "version {bad}");
    }

    // No card was ever fetched: the flag was rejected first.
    assert!(
        server.state.card_headers.lock().unwrap().is_empty(),
        "a usage error must not require contacting the agent"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_show_reports_the_effective_a2a_version() {
    let server = TestServer::spawn().await;
    let scratch = ConfigScratchDir::new("a2a-version");

    let stdout = String::from_utf8(
        scratch
            .command(&server)
            .args(["config", "show"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(
        stdout.contains("a2a-version: (negotiated from the agent card"),
        "{stdout}"
    );

    let stdout = String::from_utf8(
        scratch
            .command(&server)
            .args(["--a2a-version", "1.2", "config", "show"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(
        stdout.contains("a2a-version: 1.2 (source: flag)"),
        "{stdout}"
    );
}

// EXIT_002 / §6.6, §11.6 (a2aproject/a2a-rs#180): a non-success or paused
// outcome is named on stderr, while the exit status still reports only
// whether the CLI did its job.

/// Helper: run without `--output` overriding, returning (stdout, stderr, code).
fn run_cli_capturing(server: &TestServer, args: &[&str]) -> (String, String, i32) {
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .args(["--agent-card", server.base_url.as_str()])
        .args(args)
        .assert()
        .success()
        .get_output()
        .clone();
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
        output.status.code().unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_and_rejected_outcomes_are_named_on_stderr_and_still_exit_zero() {
    let server = TestServer::spawn().await;

    let (stdout, stderr, code) = run_cli_capturing(&server, &["task", "get", "task-failed"]);
    // §6.6: the CLI conducted and reported the turn, so it succeeded.
    assert_eq!(code, 0);
    assert!(stderr.contains("warning: task task-failed:"), "{stderr}");
    assert!(stderr.contains("FAILED"), "{stderr}");
    // The outcome is still carried in the payload on stdout.
    assert!(stdout.contains("State: FAILED"), "{stdout}");

    let (_stdout, stderr, code) = run_cli_capturing(&server, &["task", "get", "task-rejected"]);
    assert_eq!(code, 0);
    assert!(stderr.contains("warning: task task-rejected:"), "{stderr}");
    assert!(stderr.contains("REJECTED"), "{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_paused_outcome_is_named_on_stderr() {
    let server = TestServer::spawn().await;

    let (stdout, stderr, code) = run_cli_capturing(&server, &["task", "get", "task-needs-input"]);
    assert_eq!(code, 0);
    assert!(stderr.contains("INPUT_REQUIRED"), "{stderr}");
    assert!(stderr.contains("needs a reply"), "{stderr}");
    // The resume hint stays on stdout, where it is copy-pasteable
    // (INTERACT_004); the warning does not replace it.
    assert!(stdout.contains("Resume with: a2acli send"), "{stdout}");
}

/// §11.4's rule for the error envelope applies here too: diagnostics are not
/// gated on the output format.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_outcome_warning_is_emitted_in_json_mode_too() {
    let server = TestServer::spawn().await;

    let (stdout, stderr, _code) =
        run_cli_capturing(&server, &["--output", "json", "task", "get", "task-failed"]);

    assert!(stderr.contains("warning: task task-failed:"), "{stderr}");
    // stdout stays exactly the protocol document — the warning never
    // contaminates the payload (§11.1).
    let task: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(task["id"], "task-failed");
    assert_eq!(task["status"]["state"], "TASK_STATE_FAILED");
}

/// A successful task says nothing, and neither does a cancel: `CANCELED` is
/// the outcome `task cancel` was asked for, so warning about it is noise.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_and_cancelled_outcomes_stay_silent() {
    let server = TestServer::spawn().await;

    let (_stdout, stderr, code) = run_cli_capturing(&server, &["task", "get", "task-1"]);
    assert_eq!(code, 0);
    assert!(
        stderr.is_empty(),
        "completed task should be silent: {stderr}"
    );

    let (stdout, stderr, code) = run_cli_capturing(&server, &["task", "cancel", "task-1"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("State: CANCELED"), "{stdout}");
    assert!(
        !stderr.contains("warning: task"),
        "cancel should not warn about the state it was asked to produce: {stderr}"
    );
}

/// `send` reports through the same path, so a task that lands non-successful
/// is named there too rather than only under `task get`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_names_a_non_success_outcome() {
    let server = TestServer::spawn().await;

    let (stdout, stderr, code) = run_cli_capturing(&server, &["send", "send-failing"]);

    assert_eq!(code, 0);
    assert!(stderr.contains("warning: task task-send:"), "{stderr}");
    assert!(stderr.contains("FAILED"), "{stderr}");
    assert!(stdout.contains("State: FAILED"), "{stdout}");
}

/// Streamed status updates carry the outcome too, so `--stream` and
/// `task subscribe` are not a silent path around the warning.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streamed_status_updates_name_a_paused_outcome() {
    let server = TestServer::spawn().await;

    let (_stdout, stderr, code) =
        run_cli_capturing(&server, &["task", "subscribe", "task-needs-input"]);

    assert_eq!(code, 0);
    assert!(
        stderr.contains("warning: task task-needs-input:"),
        "{stderr}"
    );
}

// OUT_004 / §11.4 (a2aproject/a2a-rs#193): a malformed invocation is a
// CLI-local failure and must still be machine-readable.

/// Run `a2acli` with no agent configured at all — a parse failure must be
/// reported without contacting anything.
fn run_raw(args: &[&str]) -> (String, String, i32) {
    let mut command = StdCommand::cargo_bin("a2acli").unwrap();
    let output = command
        .env_remove("A2ACLI_AGENT_CARD")
        .env_remove("A2ACLI_BASE_URL")
        .env_remove("A2ACLI_ENDPOINT")
        .args(args)
        .output()
        .unwrap();
    (
        String::from_utf8(output.stdout).unwrap(),
        String::from_utf8(output.stderr).unwrap(),
        output.status.code().unwrap(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_malformed_invocation_reports_the_error_envelope() {
    // An unknown flag, a missing required argument, and an invalid enum
    // value are all parse failures, and all three are CLI-local (§11.4).
    let cases: [(&[&str], &str); 3] = [
        (&["--bogus-flag", "card", "get"], "--bogus-flag"),
        (&["task", "get"], "required"),
        (&["--transport", "smoke", "card", "get"], "smoke"),
    ];

    for (args, expected_fragment) in cases {
        let (stdout, stderr, code) = run_raw(args);

        let envelope = parse_error_envelope(&stderr);
        assert_eq!(
            envelope["error"]["code"], "A2ACLI_ERR_USAGE",
            "args {args:?}"
        );
        // clap's message is kept, since it names the offending argument.
        let message = envelope["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(expected_fragment),
            "args {args:?}: {message}"
        );
        // One line, one object (§11.4) — no usage block trailing it.
        assert_eq!(stderr.trim().lines().count(), 1, "args {args:?}: {stderr}");
        assert!(envelope["error"]["hint"].is_string(), "args {args:?}");
        assert_eq!(code, 2, "args {args:?}");
        // §11.1: nothing on stdout when the command failed.
        assert!(stdout.is_empty(), "args {args:?}: {stdout}");
    }
}

/// `--help` and `--version` travel the same clap `Err` channel as a parse
/// failure but are not failures: they keep writing to stdout and exiting 0
/// (`A2ACLI_CLI_001`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn help_and_version_are_not_treated_as_usage_errors() {
    for args in [
        vec!["--help"],
        vec!["-h"],
        vec!["help"],
        vec!["card", "--help"],
    ] {
        let (stdout, stderr, code) = run_raw(&args);
        assert_eq!(code, 0, "args {args:?}");
        assert!(!stdout.is_empty(), "args {args:?} should print to stdout");
        assert!(stderr.is_empty(), "args {args:?}: {stderr}");
        // Not an error envelope.
        assert!(!stdout.contains("A2ACLI_ERR"), "args {args:?}");
    }

    for args in [vec!["--version"], vec!["-V"]] {
        let (stdout, stderr, code) = run_raw(&args);
        assert_eq!(code, 0, "args {args:?}");
        assert!(stdout.contains("a2acli"), "args {args:?}: {stdout}");
        assert!(stderr.is_empty(), "args {args:?}: {stderr}");
    }
}

/// A bare invocation, and a command group with no subcommand, are usage
/// failures — clap already exits 2 for them — so they carry the envelope
/// like every other usage error rather than printing prose, with the hint
/// pointing at `--help` for the usage text it replaces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_subcommand_reports_the_error_envelope() {
    for args in [
        vec![],
        vec!["task"],
        vec!["card"],
        vec!["task", "push-config"],
    ] {
        let (stdout, stderr, code) = run_raw(&args);

        let envelope = parse_error_envelope(&stderr);
        assert_eq!(
            envelope["error"]["code"], "A2ACLI_ERR_USAGE",
            "args {args:?}"
        );
        assert!(
            envelope["error"]["hint"]
                .as_str()
                .unwrap()
                .contains("--help"),
            "args {args:?}"
        );
        assert_eq!(code, 2, "args {args:?}");
        assert!(stdout.is_empty(), "args {args:?}: {stdout}");
    }
}

/// §7.1 / `A2ACLI_CLI_002`. The script goes to stdout on its own: the output
/// is meant to be redirected to a file or `eval`'d, so a diagnostic sharing
/// stdout would corrupt it (§11.1). Run with no agent configured anywhere,
/// because `completion` must not resolve an Agent Card.
#[test]
fn completion_emits_a_script_on_stdout_for_every_supported_shell() {
    let scratch = ConfigScratchDir::new("completion-shells");

    for shell in ["bash", "zsh", "fish", "powershell", "elvish"] {
        let output = scratch
            .command_without_agent()
            .args(["completion", shell])
            .assert()
            .success()
            .get_output()
            .clone();

        let script = String::from_utf8(output.stdout).unwrap();
        assert!(
            script.contains("a2acli"),
            "{shell} script does not name the binary"
        );
        // A deep surface is the reason this requirement exists, so assert the
        // script reaches the nested commands rather than merely being
        // non-empty.
        assert!(
            script.contains("push-config"),
            "{shell} script is missing nested subcommands"
        );
        assert!(
            output.stderr.is_empty(),
            "{shell} wrote to stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// `-o json` has no meaning for a shell script, so the script is emitted as
/// itself rather than wrapped in an envelope that would not be `eval`-able.
#[test]
fn completion_ignores_the_json_output_mode() {
    let scratch = ConfigScratchDir::new("completion-json");

    let output = scratch
        .command_without_agent()
        .args(["--output", "json", "completion", "bash"])
        .assert()
        .success()
        .get_output()
        .clone();

    let script = String::from_utf8(output.stdout).unwrap();
    assert!(
        script.starts_with("_a2acli()"),
        "script was wrapped: {script:.60}"
    );
    assert!(output.stderr.is_empty());
}

/// An unrecognised shell is a CLI-local usage failure: exit 2 carrying the
/// Appendix B envelope with the accepted values named, and nothing on stdout
/// to corrupt a redirect.
#[test]
fn completion_rejects_an_unknown_shell_as_a_usage_error() {
    let scratch = ConfigScratchDir::new("completion-unknown");

    let output = scratch
        .command_without_agent()
        .args(["completion", "tcsh"])
        .assert()
        .failure()
        .get_output()
        .clone();

    assert_eq!(output.status.code(), Some(2));
    assert!(
        output.stdout.is_empty(),
        "a usage error must not put bytes on stdout"
    );

    let envelope: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stderr).unwrap()).unwrap();
    assert_eq!(envelope["error"]["code"], "A2ACLI_ERR_USAGE");
    let message = envelope["error"]["message"].as_str().unwrap();
    for shell in ["bash", "zsh", "fish", "powershell"] {
        assert!(message.contains(shell), "message omits {shell}: {message}");
    }
}

/// A card declaring nothing at all, so each pre-flight test can switch on
/// exactly the one capability it is about.
fn declares(
    streaming: bool,
    push_notifications: bool,
    extended_agent_card: bool,
) -> AgentCapabilities {
    AgentCapabilities {
        streaming: Some(streaming),
        push_notifications: Some(push_notifications),
        extensions: None,
        extended_agent_card: Some(extended_agent_card),
    }
}

fn calls(server: &TestServer) -> Vec<&'static str> {
    server.state.received_calls.lock().unwrap().clone()
}

/// §13.3 / `A2ACLI_VER_003`. `send --stream` against a card that does not
/// declare streaming keeps #173's behaviour — the caller asked for a result,
/// not specifically for a stream — but reaches it without opening a stream
/// first. The proof is server-side: `send_streaming_message` never arrives.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_stream_falls_back_when_the_card_does_not_declare_streaming() {
    let server = TestServer::spawn_with_capabilities(declares(false, true, true)).await;

    let output = StdCommand::cargo_bin("a2acli")
        .unwrap()
        .args(["--agent-card", server.base_url.as_str()])
        .args(["--output", "json", "send", "hello", "--stream"])
        .assert()
        .success()
        .get_output()
        .clone();

    let received = calls(&server);
    assert!(
        !received.contains(&"send_streaming_message"),
        "the stream was opened anyway: {received:?}"
    );
    assert!(
        received.contains(&"send_message"),
        "the fallback did not send: {received:?}"
    );

    // The reason is on stderr, and the payload still on stdout.
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("does not declare streaming"),
        "no reason given: {stderr}"
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["task"]["id"], "task-send");
}

/// `task subscribe` has no non-streaming equivalent, so an undeclared
/// capability is a failure — carrying the same code the agent would have
/// returned, so a caller branching on it sees no difference.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_subscribe_fails_when_the_card_does_not_declare_streaming() {
    let server = TestServer::spawn_with_capabilities(declares(false, true, true)).await;

    let output = StdCommand::cargo_bin("a2acli")
        .unwrap()
        .args(["--agent-card", server.base_url.as_str()])
        .args(["--output", "json", "task", "subscribe", "task-1"])
        .assert()
        .failure()
        .get_output()
        .clone();

    assert_eq!(output.status.code(), Some(1));
    let received = calls(&server);
    assert!(
        !received.contains(&"subscribe_to_task"),
        "the agent was asked anyway: {received:?}"
    );

    let envelope: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stderr).unwrap()).unwrap();
    assert_eq!(envelope["error"]["a2aCode"], -32004);
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not declare streaming"),
    );
}

/// `card get --extended` likewise. The code is `UNSUPPORTED_OPERATION`, not
/// `EXTENDED_CARD_NOT_CONFIGURED`: the latter means the agent offers
/// extended cards and this deployment has none, while a card that does not
/// declare the capability is saying it offers none at all.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn card_get_extended_fails_when_the_card_does_not_declare_it() {
    let server = TestServer::spawn_with_capabilities(declares(true, true, false)).await;

    let output = StdCommand::cargo_bin("a2acli")
        .unwrap()
        .args(["--agent-card", server.base_url.as_str()])
        .args(["--output", "json", "card", "get", "--extended"])
        .assert()
        .failure()
        .get_output()
        .clone();

    assert_eq!(output.status.code(), Some(1));
    let received = calls(&server);
    assert!(
        !received.contains(&"get_extended_agent_card"),
        "the agent was asked anyway: {received:?}"
    );

    let envelope: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stderr).unwrap()).unwrap();
    assert_eq!(envelope["error"]["a2aCode"], -32004);
    assert!(
        envelope["error"]["message"]
            .as_str()
            .unwrap()
            .contains("does not declare extendedAgentCard"),
    );
}

/// Push configs have their own protocol error, so the pre-flight uses it
/// rather than the generic one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn push_config_fails_when_the_card_does_not_declare_push_notifications() {
    let server = TestServer::spawn_with_capabilities(declares(true, false, true)).await;

    let output = StdCommand::cargo_bin("a2acli")
        .unwrap()
        .args(["--agent-card", server.base_url.as_str()])
        .args(["--output", "json", "task", "push-config", "list", "task-1"])
        .assert()
        .failure()
        .get_output()
        .clone();

    assert_eq!(output.status.code(), Some(1));
    let received = calls(&server);
    assert!(
        !received.contains(&"list_push_configs"),
        "the agent was asked anyway: {received:?}"
    );

    let envelope: serde_json::Value =
        serde_json::from_str(&String::from_utf8(output.stderr).unwrap()).unwrap();
    assert_eq!(envelope["error"]["a2aCode"], -32003);
}

/// An absent capability field reads as not declared, matching how the card
/// renderer prints it — `None` and `Some(false)` must not diverge.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_absent_capability_field_counts_as_undeclared() {
    let server = TestServer::spawn_with_capabilities(AgentCapabilities::default()).await;

    StdCommand::cargo_bin("a2acli")
        .unwrap()
        .args(["--agent-card", server.base_url.as_str()])
        .args(["--output", "json", "task", "subscribe", "task-1"])
        .assert()
        .failure();

    assert!(
        !calls(&server).contains(&"subscribe_to_task"),
        "an absent field was treated as a declaration"
    );
}

/// `--endpoint` resolves no card, and the card synthesized to stand in for
/// it declares nothing *because* nothing was read. Gating on that would
/// refuse operations the agent may well support, so the pre-flight stands
/// down and the agent answers for itself: the stream is attempted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_preflight_stands_down_when_no_card_was_resolved() {
    let server = TestServer::spawn_with_capabilities(declares(false, false, false)).await;

    StdCommand::cargo_bin("a2acli")
        .unwrap()
        .args(["--endpoint", &format!("{}/jsonrpc", server.base_url)])
        .args(["--transport", "jsonrpc"])
        .args(["--output", "json", "send", "hello", "--stream"])
        .assert()
        .success();

    assert!(
        calls(&server).contains(&"send_streaming_message"),
        "the pre-flight blocked a call it could not verify"
    );
}

/// §7.2 / `A2ACLI_OUT_007`. At Tier 2 `--debug` must show the raw protocol
/// messages, not just that a call happened — the difference between "it
/// failed" and "here is what we sent and what came back".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debug_logs_the_raw_request_and_response_bodies() {
    let server = TestServer::spawn().await;

    let output = StdCommand::cargo_bin("a2acli")
        .unwrap()
        .args(["--agent-card", server.base_url.as_str()])
        .args(["--output", "json", "--debug", "send", "hello-on-the-wire"])
        .assert()
        .success()
        .get_output()
        .clone();

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains("A2A wire request"),
        "no request logged: {stderr}"
    );
    assert!(
        stderr.contains("A2A wire response"),
        "no response logged: {stderr}"
    );
    // The request body, not merely the method name.
    assert!(
        stderr.contains("hello-on-the-wire"),
        "request body missing: {stderr}"
    );
    // The response body.
    assert!(
        stderr.contains("task-send"),
        "response body missing: {stderr}"
    );
    assert!(stderr.contains("method=POST"), "method missing: {stderr}");

    // §11.1: the diagnostics are on stderr and stdout is still only the
    // payload, so `-o json | jq` keeps working under --debug.
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(parsed["task"]["id"], "task-send");
}

/// `A2ACLI_AUTH_004`: redaction covers `--debug` raw-wire logging and is not
/// defeasible by a verbosity flag. Wire logging is the thing that row was
/// written for, so this asserts the secrets appear *nowhere* in stderr while
/// the header names still do — presence stays confirmable, values do not
/// leak.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn debug_logs_credential_header_names_but_never_their_values() {
    let server = TestServer::spawn().await;

    let output = StdCommand::cargo_bin("a2acli")
        .unwrap()
        .args(["--agent-card", server.base_url.as_str()])
        .args(["--bearer", "bearer-must-not-appear"])
        .args(["--api-key", "apikey-must-not-appear"])
        .args(["--svc-param", "X-Trace-Id:svcparam-must-not-appear"])
        .args(["--output", "json", "--debug", "send", "hello"])
        .assert()
        .success()
        .get_output()
        .clone();

    let stderr = String::from_utf8(output.stderr).unwrap();
    let stdout = String::from_utf8(output.stdout).unwrap();

    // Attachment is still visible. No failure message below interpolates a
    // credential, or a stream that would contain one if redaction were
    // broken: this test failing must not itself be what writes the
    // credential into a public CI log (rust/cleartext-logging).
    for name in ["authorization:", "x-api-key:", "x-trace-id:"] {
        assert!(stderr.contains(name), "header name {name} was not shown");
    }
    assert!(stderr.contains("(redacted)"), "nothing was redacted");

    // The values are not — in either stream.
    for (flag, credential) in [
        ("--bearer", "bearer-must-not-appear"),
        ("--api-key", "apikey-must-not-appear"),
        ("--svc-param", "svcparam-must-not-appear"),
    ] {
        assert!(
            !stderr.contains(credential),
            "the {flag} credential appeared in stderr"
        );
        assert!(
            !stdout.contains(credential),
            "the {flag} credential appeared in stdout"
        );
    }
}

/// Without `--debug` there is no subscriber, so the wire events are not
/// emitted at all: the default run stays quiet on stderr.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wire_logging_is_silent_without_debug() {
    let server = TestServer::spawn().await;

    let output = StdCommand::cargo_bin("a2acli")
        .unwrap()
        .args(["--agent-card", server.base_url.as_str()])
        .args(["--output", "json", "send", "hello"])
        .assert()
        .success()
        .get_output()
        .clone();

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        !stderr.contains("A2A wire"),
        "wire logging leaked without --debug: {stderr}"
    );
}
