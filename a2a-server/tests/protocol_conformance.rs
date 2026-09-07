// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
//! Released A2A v1.0.1 qualification of the pinned official Rust server.
//! These are normative gates, not assertions that preserve known upstream bugs.
//! Fixtures use public SDK APIs and in-process HTTP. No native host is contacted.

use a2a::{
    A2AError, AgentCapabilities, AgentCard, AgentInterface, Artifact, HttpAuthSecurityScheme,
    Message, Part, Role, SecurityScheme, StreamResponse, Task, TaskArtifactUpdateEvent, TaskState,
    TaskStatus, TaskStatusUpdateEvent,
};
use a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore, StaticAgentCard,
    TaskStore,
};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use futures::{
    StreamExt,
    stream::{self, BoxStream},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::Notify;
use tower::ServiceExt;

const LIMIT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct Execution {
    message_id: String,
    task_id: String,
    context_id: String,
    transport_identity: Option<String>,
    stored_history_ids: Vec<String>,
}

struct FixtureExecutor {
    finish: TaskState,
    executions: Arc<Mutex<Vec<Execution>>>,
    release: Option<Arc<Notify>>,
}

impl AgentExecutor for FixtureExecutor {
    fn execute(
        &self,
        context: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let message = context.message.unwrap();
        self.executions.lock().unwrap().push(Execution {
            message_id: message.message_id.clone(),
            task_id: context.task_id.clone(),
            context_id: context.context_id.clone(),
            transport_identity: context
                .service_params
                .get("x-fixture-identity")
                .and_then(|values| values.first())
                .cloned(),
            stored_history_ids: context
                .stored_task
                .as_ref()
                .and_then(|task| task.history.as_ref())
                .into_iter()
                .flatten()
                .map(|message| message.message_id.clone())
                .collect(),
        });
        let status = |state| TaskStatus {
            state,
            message: None,
            timestamp: None,
        };
        let update = |state| {
            StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: context.task_id.clone(),
                context_id: context.context_id.clone(),
                status: status(state),
                metadata: None,
            })
        };
        let task = Task {
            id: context.task_id.clone(),
            context_id: context.context_id.clone(),
            status: status(TaskState::Submitted),
            artifacts: None,
            history: context
                .stored_task
                .and_then(|task| task.history)
                .or_else(|| Some(vec![message])),
            metadata: None,
        };
        let release = self.release.clone();
        let events = [
            Ok(StreamResponse::Task(task)),
            Ok(update(TaskState::Working)),
            Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: context.task_id.clone(),
                context_id: context.context_id.clone(),
                artifact: Artifact {
                    artifact_id: "selected-reply".into(),
                    name: None,
                    description: None,
                    parts: vec![Part::text("fixture reply")],
                    metadata: None,
                    extensions: None,
                },
                append: Some(false),
                last_chunk: Some(true),
                metadata: None,
            })),
            Ok(update(self.finish.clone())),
        ];
        Box::pin(stream::iter(events).then(move |event| {
            let release = release.clone();
            async move {
                if matches!(&event, Ok(StreamResponse::StatusUpdate(update))
                    if update.status.state == TaskState::Working)
                {
                    if let Some(release) = release {
                        release.notified().await;
                    }
                }
                event
            }
        }))
    }

    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        panic!("send qualification must not cancel native work")
    }
}

fn fixture(finish: TaskState) -> (Router, Arc<Mutex<Vec<Execution>>>) {
    let executions = Arc::new(Mutex::new(Vec::new()));
    let handler = DefaultRequestHandler::new(
        FixtureExecutor {
            finish,
            executions: executions.clone(),
            release: None,
        },
        InMemoryTaskStore::new(),
    );
    (
        a2a_server::jsonrpc::jsonrpc_router(Arc::new(handler)),
        executions,
    )
}

fn message(id: &str) -> Value {
    json!({
        "message": {
            "messageId": id,
            "role": "ROLE_USER",
            "parts": [{"text": "bounded fixture request"}]
        }
    })
}

async fn wire(
    router: &Router,
    method: &str,
    params: Value,
    version: Option<&str>,
    identity: Option<&str>,
) -> (bool, Vec<Value>) {
    let mut request = Request::builder()
        .method("POST")
        .uri("/")
        .header("content-type", "application/json");
    if let Some(version) = version {
        request = request.header("A2A-Version", version);
    }
    if let Some(identity) = identity {
        request = request.header("x-fixture-identity", identity);
    }
    let response = tokio::time::timeout(
        LIMIT,
        router.clone().oneshot(
            request
                .body(Body::from(
                    serde_json::to_vec(&json!({
                        "jsonrpc": "2.0", "id": "qualification", "method": method, "params": params
                    }))
                    .unwrap(),
                ))
                .unwrap(),
        ),
    )
    .await
    .expect("protocol operation must return within the fixture deadline")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let streaming = response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/event-stream");
    let body = tokio::time::timeout(LIMIT, to_bytes(response.into_body(), 256 * 1024))
        .await
        .expect("completed fixture stream must close")
        .unwrap();
    let frames: Vec<Value> = if streaming {
        std::str::from_utf8(&body)
            .unwrap()
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(|data| serde_json::from_str(data.trim()).unwrap())
            .collect()
    } else {
        vec![serde_json::from_slice(&body).unwrap()]
    };
    for frame in &frames {
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["id"], "qualification");
    }
    (streaming, frames)
}

async fn rpc(router: &Router, method: &str, params: Value) -> Value {
    let (streaming, mut frames) = wire(router, method, params, Some("1.0"), None).await;
    assert!(!streaming);
    assert_eq!(frames.len(), 1);
    frames.remove(0)
}

#[tokio::test]
async fn first_send_assigns_server_ids_and_retains_a_lookupable_task() {
    let (router, executions) = fixture(TaskState::Completed);
    let response = rpc(&router, "SendMessage", message("first")).await;
    let task = &response["result"]["task"];
    let task_id = task["id"].as_str().expect("server must assign task ID");
    let context_id = task["contextId"]
        .as_str()
        .expect("server must assign context ID");
    assert!(!task_id.is_empty());
    assert!(!context_id.is_empty());
    assert_eq!(task["status"]["state"], "TASK_STATE_COMPLETED");
    let fetched = rpc(&router, "GetTask", json!({"id": task_id})).await;
    assert_eq!(&fetched["result"], task);
    assert_eq!(executions.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn supplied_unknown_task_id_cannot_create_a_task_or_dispatch_work() {
    let (router, executions) = fixture(TaskState::Completed);
    let mut request = message("unknown-task");
    request["message"]["taskId"] = json!("client-cannot-create-this-id");
    let response = rpc(&router, "SendMessage", request).await;
    assert_eq!(
        response["error"]["code"], -32001,
        "v1.0.1 section 3.4.2 requires TaskNotFoundError for an unknown supplied task ID"
    );
    assert!(executions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn mismatched_context_is_rejected_before_existing_task_execution() {
    let (router, executions) = fixture(TaskState::InputRequired);
    let first = rpc(&router, "SendMessage", message("first")).await;
    let mut request = message("mismatched-context");
    request["message"]["taskId"] = first["result"]["task"]["id"].clone();
    request["message"]["contextId"] = json!("different-context");
    let response = rpc(&router, "SendMessage", request).await;
    assert!(
        response.get("error").is_some(),
        "v1.0.1 section 3.4.3 requires rejection, not silent replacement of the supplied context"
    );
    assert_eq!(executions.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn terminal_continuation_returns_unsupported_operation_without_dispatch() {
    let (router, executions) = fixture(TaskState::Completed);
    let first = rpc(&router, "SendMessage", message("first")).await;
    let mut request = message("terminal-continuation");
    request["message"]["taskId"] = first["result"]["task"]["id"].clone();
    let response = rpc(&router, "SendMessage", request).await;
    assert_eq!(
        response["error"]["code"], -32004,
        "terminal tasks cannot accept another message under v1.0.1 section 3.1.1"
    );
    assert_eq!(executions.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn nonterminal_continuation_infers_context_and_context_only_send_starts_a_new_task() {
    let (router, executions) = fixture(TaskState::InputRequired);
    let first = rpc(&router, "SendMessage", message("first")).await;
    let first_task = &first["result"]["task"];
    let mut continuation = message("continuation");
    continuation["message"]["taskId"] = first_task["id"].clone();
    let continued = rpc(&router, "SendMessage", continuation).await;
    assert_eq!(continued["result"]["task"]["id"], first_task["id"]);
    assert_eq!(
        continued["result"]["task"]["contextId"],
        first_task["contextId"]
    );
    let mut followup = message("context-followup");
    followup["message"]["contextId"] = first_task["contextId"].clone();
    let next = rpc(&router, "SendMessage", followup).await;
    assert_ne!(next["result"]["task"]["id"], first_task["id"]);
    assert_eq!(next["result"]["task"]["contextId"], first_task["contextId"]);
    let executions = executions.lock().unwrap();
    assert_eq!(executions.len(), 3);
    assert_eq!(executions[1].message_id, "continuation");
    assert_eq!(executions[1].task_id, executions[0].task_id);
    assert_eq!(executions[1].context_id, executions[0].context_id);
}

async fn rejects_version(version: Option<&str>) {
    let (router, executions) = fixture(TaskState::Completed);
    let (_, frames) = wire(&router, "SendMessage", message("version"), version, None).await;
    assert_eq!(
        frames[0]["error"]["code"], -32009,
        "a 1.0-only endpoint must reject unsupported or implicit legacy 0.3 semantics"
    );
    assert!(executions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn absent_version_is_unsupported_legacy_0_3_not_implicit_1_0() {
    rejects_version(None).await;
}

#[tokio::test]
async fn empty_version_is_unsupported_legacy_0_3_not_implicit_1_0() {
    rejects_version(Some("")).await;
}

#[tokio::test]
async fn unsupported_explicit_version_is_rejected_before_execution() {
    rejects_version(Some("0.3")).await;
}

#[tokio::test]
async fn lifecycle_sse_preserves_initial_task_typed_updates_order_and_terminal_closure() {
    let (router, executions) = fixture(TaskState::Completed);
    let (streaming, frames) = wire(
        &router,
        "SendStreamingMessage",
        message("stream"),
        Some("1.0"),
        None,
    )
    .await;
    assert!(streaming);
    assert_eq!(frames.len(), 4);
    assert!(frames[0]["result"].get("task").is_some());
    assert_eq!(
        frames[1]["result"]["statusUpdate"]["status"]["state"],
        "TASK_STATE_WORKING"
    );
    assert!(frames[2]["result"].get("artifactUpdate").is_some());
    assert_eq!(
        frames[3]["result"]["statusUpdate"]["status"]["state"],
        "TASK_STATE_COMPLETED"
    );
    let task_id = &frames[0]["result"]["task"]["id"];
    let context_id = &frames[0]["result"]["task"]["contextId"];
    for (frame, key) in frames[1..]
        .iter()
        .zip(["statusUpdate", "artifactUpdate", "statusUpdate"])
    {
        assert_eq!(frame["result"].as_object().unwrap().len(), 1);
        assert_eq!(&frame["result"][key]["taskId"], task_id);
        assert_eq!(&frame["result"][key]["contextId"], context_id);
    }
    assert_eq!(executions.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn service_parameters_reach_each_execution_without_model_metadata_substitution() {
    let (router, executions) = fixture(TaskState::Completed);
    let mut a = message("a");
    a["metadata"] = json!({"x-fixture-identity": "forged-model-a"});
    let mut b = message("b");
    b["metadata"] = json!({"x-fixture-identity": "forged-model-b"});
    let (a, b) = tokio::join!(
        wire(&router, "SendMessage", a, Some("1.0"), Some("transport-a")),
        wire(&router, "SendMessage", b, Some("1.0"), Some("transport-b"))
    );
    assert!(a.1[0]["result"].get("task").is_some());
    assert!(b.1[0]["result"].get("task").is_some());
    let executions = executions.lock().unwrap();
    assert_eq!(executions.len(), 2);
    for (id, identity) in [("a", "transport-a"), ("b", "transport-b")] {
        let execution = executions
            .iter()
            .find(|execution| execution.message_id == id)
            .unwrap();
        assert_eq!(execution.transport_identity.as_deref(), Some(identity));
    }
}

#[tokio::test]
async fn public_agent_card_uses_canonical_protojson_security_requirements() {
    let card = AgentCard {
        name: "Fixture agent".into(),
        description: "Canonical discovery fixture".into(),
        version: "1".into(),
        supported_interfaces: vec![AgentInterface::new(
            "https://example.invalid/a2a",
            "JSONRPC",
        )],
        capabilities: AgentCapabilities::default(),
        default_input_modes: vec!["text/plain".into()],
        default_output_modes: vec!["text/plain".into()],
        skills: vec![],
        provider: None,
        documentation_url: None,
        icon_url: None,
        security_schemes: Some(HashMap::from([(
            "peer".into(),
            SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
                scheme: "bearer".into(),
                description: None,
                bearer_format: None,
            }),
        )])),
        security_requirements: Some(vec![HashMap::from([("peer".into(), vec![])])]),
        signatures: None,
    };
    let router =
        a2a_server::agent_card::agent_card_router(Arc::new(StaticAgentCard::new(card.clone())));
    let response = router
        .oneshot(
            Request::builder()
                .uri(a2a_server::WELL_KNOWN_AGENT_CARD_PATH)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
    let value: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        value["securityRequirements"],
        json!([{"schemes": {"peer": {}}}]),
        "released SecurityRequirement and StringList wrappers must survive HTTP discovery"
    );
    let decoded: AgentCard = a2a_pb::protojson_conv::from_value(value).unwrap();
    assert_eq!(decoded, card);
}

async fn history_fixture(release: Option<Arc<Notify>>) -> (Router, Arc<Mutex<Vec<Execution>>>) {
    let store = InMemoryTaskStore::new();
    let history = (0..4)
        .map(|index| {
            let mut message = Message::new(Role::User, vec![Part::text("fixture history")]);
            message.message_id = format!("history-{index}");
            message
        })
        .collect();
    store
        .create(Task {
            id: "history-task".into(),
            context_id: "history-context".into(),
            status: TaskStatus {
                state: TaskState::InputRequired,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: Some(history),
            metadata: None,
        })
        .await
        .unwrap();
    let executions = Arc::new(Mutex::new(Vec::new()));
    let handler = DefaultRequestHandler::new(
        FixtureExecutor {
            finish: TaskState::Completed,
            executions: executions.clone(),
            release,
        },
        store,
    );
    (
        a2a_server::jsonrpc::jsonrpc_router(Arc::new(handler)),
        executions,
    )
}

fn assert_history(task: &Value, limit: usize) {
    let actual: Vec<_> = task["history"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|message| message["messageId"].as_str().unwrap())
        .collect();
    let expected: Vec<_> = (4_usize.saturating_sub(limit)..4)
        .map(|index| format!("history-{index}"))
        .collect();
    assert_eq!(
        actual, expected,
        "only the most recent requested history belongs in the response"
    );
    if limit == 0 {
        assert!(task.get("history").is_none());
    }
}

fn history_send(limit: i32) -> Value {
    let mut request = message("history-continuation");
    request["message"]["taskId"] = json!("history-task");
    request["configuration"] = json!({"historyLength": limit});
    request
}

#[tokio::test]
async fn get_task_history_windows_do_not_modify_retained_history() {
    let (router, executions) = history_fixture(None).await;
    for limit in [0, 1, 3, 6] {
        let response = rpc(
            &router,
            "GetTask",
            json!({"id": "history-task", "historyLength": limit}),
        )
        .await;
        assert_history(&response["result"], limit);
        let retained = rpc(&router, "GetTask", json!({"id": "history-task"})).await;
        assert_history(&retained["result"], 4);
    }
    assert!(executions.lock().unwrap().is_empty());
}

#[tokio::test]
async fn list_task_history_windows_do_not_modify_retained_history() {
    let (router, _) = history_fixture(None).await;
    for limit in [0, 1, 3, 6] {
        let response = rpc(&router, "ListTasks", json!({"historyLength": limit})).await;
        assert_history(&response["result"]["tasks"][0], limit);
        let retained = rpc(&router, "GetTask", json!({"id": "history-task"})).await;
        assert_history(&retained["result"], 4);
    }
}

#[tokio::test]
async fn send_history_windows_preserve_storage_and_executor_input() {
    for limit in [0, 1, 3, 6] {
        let (router, executions) = history_fixture(None).await;
        let response = rpc(&router, "SendMessage", history_send(limit)).await;
        assert_history(&response["result"]["task"], limit as usize);
        let retained = rpc(&router, "GetTask", json!({"id": "history-task"})).await;
        assert_history(&retained["result"], 4);
        assert_eq!(
            executions.lock().unwrap()[0].stored_history_ids,
            ["history-0", "history-1", "history-2", "history-3"],
            "response projection must not remove native execution context"
        );
    }
}

#[tokio::test]
async fn immediate_send_applies_history_window_before_execution_completes() {
    for limit in [0, 2] {
        let release = Arc::new(Notify::new());
        let (router, _) = history_fixture(Some(release.clone())).await;
        let mut request = history_send(limit);
        request["configuration"]["returnImmediately"] = json!(true);
        let response = rpc(&router, "SendMessage", request).await;
        assert_eq!(
            response["result"]["task"]["status"]["state"],
            "TASK_STATE_SUBMITTED"
        );
        assert_history(&response["result"]["task"], limit as usize);
        let retained = rpc(&router, "GetTask", json!({"id": "history-task"})).await;
        assert_history(&retained["result"], 4);
        release.notify_one();
    }
}

#[tokio::test]
async fn streaming_initial_task_applies_history_window_without_losing_updates() {
    for limit in [0, 2] {
        let (router, executions) = history_fixture(None).await;
        let (streaming, frames) = wire(
            &router,
            "SendStreamingMessage",
            history_send(limit),
            Some("1.0"),
            None,
        )
        .await;
        assert!(streaming);
        assert_eq!(frames.len(), 4);
        assert_history(&frames[0]["result"]["task"], limit as usize);
        assert_eq!(
            frames[1]["result"]["statusUpdate"]["status"]["state"],
            "TASK_STATE_WORKING"
        );
        assert!(frames[2]["result"].get("artifactUpdate").is_some());
        assert_eq!(
            frames[3]["result"]["statusUpdate"]["status"]["state"],
            "TASK_STATE_COMPLETED"
        );
        let retained = rpc(&router, "GetTask", json!({"id": "history-task"})).await;
        assert_history(&retained["result"], 4);
        assert_eq!(executions.lock().unwrap()[0].stored_history_ids.len(), 4);
    }
}

#[tokio::test]
async fn negative_history_windows_fail_before_read_operations() {
    let (router, _) = history_fixture(None).await;
    for (method, request) in [
        (
            "GetTask",
            json!({"id": "history-task", "historyLength": -1}),
        ),
        ("ListTasks", json!({"historyLength": -1})),
    ] {
        let response = rpc(&router, method, request).await;
        assert_eq!(response["error"]["code"], -32602);
    }
}

#[tokio::test]
async fn negative_send_history_windows_fail_before_executor_dispatch() {
    for method in ["SendMessage", "SendStreamingMessage"] {
        let (router, executions) = fixture(TaskState::Completed);
        let mut request = message("negative-history");
        request["configuration"] = json!({"historyLength": -1});
        let (streaming, frames) = wire(&router, method, request, Some("1.0"), None).await;
        assert!(
            !streaming,
            "invalid streaming requests must fail before SSE starts"
        );
        assert_eq!(frames[0]["error"]["code"], -32602);
        assert!(executions.lock().unwrap().is_empty());
        let tasks = rpc(&router, "ListTasks", json!({})).await;
        assert!(
            tasks["result"]["tasks"]
                .as_array()
                .is_none_or(|tasks| tasks.is_empty())
        );
    }
}
