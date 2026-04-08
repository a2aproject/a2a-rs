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
use assert_cmd::assert::OutputAssertExt;
use assert_cmd::cargo::CommandCargoExt;
use async_trait::async_trait;
use axum::http::{HeaderMap, StatusCode, header};
use axum::routing::get;
use axum::{Json, Router};
use futures::stream::{self, BoxStream};
use serde_json::Value;
use tokio::net::TcpListener;

#[derive(Default)]
struct ServerState {
    tasks: Mutex<BTreeMap<String, Task>>,
    push_configs: Mutex<BTreeMap<(String, String), TaskPushNotificationConfig>>,
    card_headers: Mutex<Vec<(Option<String>, Option<String>)>>,
}

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
        req: CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        if !self.state.tasks.lock().unwrap().contains_key(&req.task_id) {
            return Err(A2AError::task_not_found(&req.task_id));
        }

        let mut config = req.config;
        let config_id = config
            .id
            .clone()
            .unwrap_or_else(|| "cfg-generated".to_string());
        config.id = Some(config_id.clone());
        let task_config = TaskPushNotificationConfig {
            task_id: req.task_id.clone(),
            config,
            tenant: req.tenant,
        };
        self.state
            .push_configs
            .lock()
            .unwrap()
            .insert((req.task_id, config_id), task_config.clone());
        Ok(task_config)
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
                parts: vec![Part::text(text)],
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
        &["--binding", "http-json", "--compact", "extended-card"],
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

    let get_task = run_cli_success(&server, &["get-task", "task-send", "--history-length", "1"]);
    let task_json: Value = serde_json::from_str(&get_task).unwrap();
    assert_eq!(task_json["id"], "task-send");

    let list = run_cli_success(
        &server,
        &[
            "--compact",
            "list-tasks",
            "--context-id",
            "ctx-send",
            "--status",
            "completed",
        ],
    );
    let list_json: Value = serde_json::from_str(list.trim()).unwrap();
    assert_eq!(list_json["tasks"].as_array().unwrap().len(), 1);

    let cancel = run_cli_success(&server, &["cancel-task", "task-send"]);
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
            "stream",
            "streaming request",
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

    let subscribe_output = run_cli_success(&server, &["--compact", "subscribe", "task-stream"]);
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
    assert_eq!(create_json["config"]["id"], "cfg-1");
    assert_eq!(create_json["tenant"], "tenant-1");

    let get = run_cli_success(
        &server,
        &["--compact", "push-config", "get", "task-1", "cfg-1"],
    );
    let get_json: Value = serde_json::from_str(get.trim()).unwrap();
    assert_eq!(get_json["config"]["authentication"]["scheme"], "Bearer");

    let list = run_cli_success(
        &server,
        &[
            "--compact",
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
        &["--compact", "push-config", "delete", "task-1", "cfg-1"],
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
        .args(["--base-url", base_url.as_str(), "card"])
        .assert()
        .failure()
        .get_output()
        .clone();
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("http request failed:"));

    let (_stdout, stderr) = run_cli_failure(&server, &["extended-card", "--tenant", "error"]);
    assert!(stderr.contains("a2a error -32004: extended card denied"));

    let (_stdout, stderr) = run_cli_failure(&server, &["send", "send-error"]);
    assert!(stderr.contains("a2a error -32600: send failed"));

    let (_stdout, stderr) = run_cli_failure(&server, &["list-tasks", "--context-id", "error"]);
    assert!(stderr.contains("a2a error -32602: list failed"));

    let (_stdout, stderr) = run_cli_failure(&server, &["get-task", "missing"]);
    assert!(stderr.contains("a2a error -32001: task not found: missing"));

    let (_stdout, stderr) = run_cli_failure(&server, &["cancel-task", "missing"]);
    assert!(stderr.contains("a2a error -32001: task not found: missing"));

    let (_stdout, stderr) = run_cli_failure(&server, &["subscribe", "stream-error"]);
    assert!(stderr.contains("a2a error -32603: stream failed"));

    let (_stdout, stderr) = run_cli_failure(&server, &["--compact", "stream", "stream-error"]);
    assert!(stderr.contains("a2a error -32603: stream failed"));

    let (_stdout, stderr) = run_cli_failure(
        &server,
        &[
            "push-config",
            "create",
            "missing",
            "https://example.com/callback",
            "--config-id",
            "cfg-missing",
        ],
    );
    assert!(stderr.contains("a2a error -32001: task not found: missing"));

    let (_stdout, stderr) = run_cli_failure(&server, &["push-config", "get", "task-1", "missing"]);
    assert!(stderr.contains("a2a error -32001: task not found: task-1"));

    let (_stdout, stderr) = run_cli_failure(&server, &["push-config", "list", "missing"]);
    assert!(stderr.contains("a2a error -32001: task not found: missing"));

    let (_stdout, stderr) =
        run_cli_failure(&server, &["push-config", "delete", "task-1", "missing"]);
    assert!(stderr.contains("a2a error -32001: task not found: task-1"));

    let (_stdout, stderr) = run_cli_failure(
        &server,
        &[
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
