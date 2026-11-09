// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Startup recovery against a persistence-only store shared across runtime lifetimes.

use a2a::*;
use a2a_server::task_store::TaskVersion;
use a2a_server::{
    Agent, AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore, NewMessage,
    RequestHandler, ServiceParams, Start, TaskStore,
};
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use tokio::{
    sync::mpsc,
    time::{Duration, timeout},
};

#[derive(Clone, Default)]
struct Store {
    inner: Arc<InMemoryTaskStore>,
    lists: Arc<AtomicUsize>,
    repeat_token: Arc<AtomicBool>,
    cancel_during_scan: Arc<AtomicBool>,
    get_errors: Arc<Mutex<HashMap<TaskId, A2AError>>>,
    list_error_at: Arc<AtomicUsize>,
}
#[async_trait]
impl TaskStore for Store {
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.inner.create(task).await
    }
    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.inner.update(task).await
    }
    async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
        if let Some(error) = self.get_errors.lock().unwrap().remove(id) {
            return Err(error);
        }
        self.inner.get(id).await
    }
    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        let page = self.lists.fetch_add(1, Ordering::SeqCst) + 1;
        if self.list_error_at.load(Ordering::SeqCst) == page {
            return Err(A2AError::internal("recovery listing failed"));
        }
        let request = if self.repeat_token.load(Ordering::SeqCst) {
            ListTasksRequest {
                page_token: None,
                ..req.clone()
            }
        } else {
            req.clone()
        };
        let mut page = self.inner.list(&request).await?;
        if self.cancel_during_scan.swap(false, Ordering::SeqCst) {
            for task in &page.tasks {
                if !task.status.state.is_terminal() {
                    self.inner.begin_cancel(&task.id).await?;
                }
            }
        }
        if self.repeat_token.load(Ordering::SeqCst) {
            page.next_page_token = "repeated".into();
        }
        Ok(page)
    }
    async fn begin_cancel(&self, id: &str) -> Result<Task, A2AError> {
        self.inner.begin_cancel(id).await
    }
}
async fn seed(store: &Store, id: &str, state: TaskState) {
    store
        .create(Task {
            id: id.into(),
            context_id: "session".into(),
            generation: 0,
            timeline: None,
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: Some(vec![Message::new(
                Role::User,
                vec![Part::text("persisted input")],
            )]),
            metadata: Some(
                serde_json::from_value(
                    serde_json::json!({"checkpoint": "saved state", "user": "untrusted metadata"}),
                )
                .unwrap(),
            ),
        })
        .await
        .unwrap();
}
fn status(ctx: &ExecutorContext, state: TaskState) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        generation: 0,
        metadata: None,
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
    })
}
fn send(id: Option<&str>) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text("finish")]);
    message.task_id = id.map(str::to_owned);
    SendMessageRequest {
        message,
        artifact_updates: vec![],
        metadata: None,
        tenant: None,
        configuration: None,
    }
}
struct Observed {
    id: String,
    task: Task,
    recovered: bool,
    empty_caller: bool,
}
struct Executor(mpsc::UnboundedSender<Observed>);
impl Executor {
    fn observe(&self, ctx: &ExecutorContext, recovered: bool) {
        self.0
            .send(Observed {
                id: ctx.execution_id.clone(),
                task: ctx.stored_task.clone().unwrap(),
                recovered,
                empty_caller: ctx.message.is_none()
                    && ctx.user.is_none()
                    && ctx.metadata.is_none()
                    && ctx.tenant.is_none()
                    && ctx.service_params.is_empty()
                    && !ctx.new_task,
            })
            .unwrap();
    }
}
impl AgentExecutor for Executor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        self.observe(&ctx, false);
        Box::pin(async_stream::stream! {
            yield Ok(status(&ctx, TaskState::Working));
            futures::future::pending::<()>().await;
        })
    }
    fn restart(
        &self,
        mut ctx: ExecutorContext,
    ) -> Option<BoxStream<'static, Result<StreamResponse, A2AError>>> {
        self.observe(&ctx, true);
        Some(Box::pin(async_stream::stream! {
            yield Ok(status(&ctx, TaskState::Working));
            if ctx.inputs.recv().await.is_some() { yield Ok(status(&ctx, TaskState::Completed)); }
        }))
    }
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        futures::stream::once(async move { Ok(status(&ctx, TaskState::Canceled)) }).boxed()
    }
}
fn handler(executor: impl AgentExecutor, store: Store) -> DefaultRequestHandler {
    DefaultRequestHandler::new(executor, store).with_capabilities(AgentCapabilities {
        streaming: Some(true),
        ..Default::default()
    })
}

#[test]
fn a_new_server_runtime_resumes_the_snapshot_without_replaying_input_or_credentials() {
    let store = Store::default();
    let (sent, mut observed) = mpsc::unbounded_channel();
    let runtime = || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    };
    let old = runtime();
    let (execution_id, persisted) = old.block_on(async {
        seed(&store, "task", TaskState::Submitted).await;
        let server = handler(Executor(sent.clone()), store.clone());
        let mut stream = server
            .send_streaming_message(&ServiceParams::new(), send(Some("task")))
            .await
            .unwrap();
        stream.next().await.unwrap().unwrap();
        let original = observed.recv().await.unwrap();
        assert!(!original.recovered);
        (original.id, store.get("task").await.unwrap().unwrap())
    });
    // Shutting down the old runtime stops its workers; the backing store survives.
    drop(old);
    runtime().block_on(async {
        let server = handler(Executor(sent), store.clone());
        assert_eq!(server.restart_tasks().await.unwrap(), 1);
        let resumed = timeout(Duration::from_secs(2), observed.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(resumed.recovered && resumed.empty_caller);
        assert_ne!(resumed.id, execution_id);
        assert_eq!(resumed.task, persisted);
        let SendMessageResponse::Task(task) = server
            .send_message(&ServiceParams::new(), send(Some("task")))
            .await
            .unwrap()
        else {
            panic!("task")
        };
        assert_eq!(task.status.state, TaskState::Completed);
        assert_eq!(
            task.history.unwrap().len(),
            persisted.history.unwrap().len() + 1
        );
        assert_eq!(server.restart_tasks().await.unwrap(), 0);
    });
}

#[tokio::test]
async fn repeated_startup_scans_skip_a_task_whose_restart_is_still_running() {
    let store = Store::default();
    seed(&store, "task", TaskState::InputRequired).await;
    let (sent, mut observed) = mpsc::unbounded_channel();
    let server = handler(Executor(sent), store.clone());
    assert_eq!(server.restart_tasks().await.unwrap(), 1);
    let first = observed.recv().await.unwrap();
    assert_eq!(first.task.status.state, TaskState::InputRequired);
    assert_eq!(server.restart_tasks().await.unwrap(), 0);
    assert!(observed.try_recv().is_err());
    let task = server
        .cancel_task(
            &ServiceParams::new(),
            CancelTaskRequest {
                id: "task".into(),
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(task.status.state, TaskState::Canceled);
}

struct Legacy;
impl AgentExecutor for Legacy {
    fn execute(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        futures::stream::once(async {
            Ok(StreamResponse::Message(Message::new(
                Role::Agent,
                vec![Part::text("reply")],
            )))
        })
        .boxed()
    }
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        futures::stream::once(async move { Ok(status(&ctx, TaskState::Canceled)) }).boxed()
    }
}
#[tokio::test]
async fn executors_without_a_restart_hook_leave_nonterminal_tasks_unchanged() {
    let store = Store::default();
    for (id, state) in [
        ("submitted", TaskState::Submitted),
        ("working", TaskState::Working),
        ("input", TaskState::InputRequired),
        ("auth", TaskState::AuthRequired),
    ] {
        seed(&store, id, state).await;
    }
    let server = handler(Legacy, store.clone());
    assert!(matches!(
        server
            .send_message(&ServiceParams::new(), send(None))
            .await
            .unwrap(),
        SendMessageResponse::Message(_)
    ));
    let before = store.get("input").await.unwrap().unwrap();
    assert_eq!(server.restart_tasks().await.unwrap(), 0);
    assert_eq!(store.get("input").await.unwrap().unwrap(), before);
    let canceled = server
        .cancel_task(
            &ServiceParams::new(),
            CancelTaskRequest {
                id: "input".into(),
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(canceled.status.state, TaskState::Canceled);
    let typed = DefaultRequestHandler::from_agent(PlainAgent, store.clone());
    assert!(matches!(
        typed
            .send_message(&ServiceParams::new(), send(None))
            .await
            .unwrap(),
        SendMessageResponse::Message(_)
    ));
    let before = store.get("working").await.unwrap().unwrap();
    assert_eq!(typed.restart_tasks().await.unwrap(), 0);
    assert_eq!(store.get("working").await.unwrap().unwrap(), before);
}

struct PlainAgent;
#[async_trait]
impl Agent for PlainAgent {
    async fn start(&self, msg: NewMessage) -> Result<Start, A2AError> {
        CompletingAgent.start(msg).await
    }
}

struct CompletingAgent;
#[async_trait]
impl Agent for CompletingAgent {
    async fn start(&self, _: NewMessage) -> Result<Start, A2AError> {
        Ok(Start::Message(Message::new(
            Role::Agent,
            vec![Part::text("reply")],
        )))
    }
    fn restart(
        &self,
        ctx: ExecutorContext,
    ) -> Option<BoxStream<'static, Result<StreamResponse, A2AError>>> {
        assert!(ctx.message.is_none());
        Some(futures::stream::once(async move { Ok(status(&ctx, TaskState::Completed)) }).boxed())
    }
}
#[tokio::test]
async fn recovery_paginates_and_uses_the_typed_agent_restart_hook_only_for_nonterminal_tasks() {
    let store = Store::default();
    for i in 0..105 {
        seed(
            &store,
            &format!("task-{i:03}"),
            if i % 35 == 0 {
                TaskState::Completed
            } else {
                TaskState::Working
            },
        )
        .await;
    }
    let server = DefaultRequestHandler::from_agent(CompletingAgent, store.clone());
    assert!(matches!(
        server
            .send_message(&ServiceParams::new(), send(None))
            .await
            .unwrap(),
        SendMessageResponse::Message(_)
    ));
    assert_eq!(server.restart_tasks().await.unwrap(), 102);
    assert!(store.lists.load(Ordering::SeqCst) >= 2);
    timeout(Duration::from_secs(3), async {
        loop {
            let mut complete = true;
            for i in 0..105 {
                complete &= store
                    .get(&format!("task-{i:03}"))
                    .await
                    .unwrap()
                    .unwrap()
                    .status
                    .state
                    .is_terminal();
            }
            if complete {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(server.restart_tasks().await.unwrap(), 0);
}

#[tokio::test]
async fn recovery_attempts_later_tasks_and_pages_after_reload_errors_then_allows_retry() {
    let store = Store::default();
    for i in 0..105 {
        seed(&store, &format!("task-{i:03}"), TaskState::Working).await;
    }
    let details = vec![TypedDetail::error_info(
        "RESTORE_FAILED",
        "recovery-test",
        None,
    )];
    for (id, error) in [
        (
            "task-000",
            A2AError::internal("first reload failure").with_details(details.clone()),
        ),
        (
            "task-002",
            A2AError::unsupported_operation("second reload failure"),
        ),
        ("task-100", A2AError::internal("later page reload failure")),
    ] {
        store.get_errors.lock().unwrap().insert(id.into(), error);
    }
    let (sent, mut observed) = mpsc::unbounded_channel();
    let server = handler(Executor(sent), store.clone());
    let error = timeout(Duration::from_secs(2), server.restart_tasks())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.code, error_code::INTERNAL_ERROR);
    assert_eq!(error.message, "first reload failure");
    assert_eq!(error.details, Some(details));
    assert_eq!(store.lists.load(Ordering::SeqCst), 2);
    assert!(store.get_errors.lock().unwrap().is_empty());
    let recovered: Vec<_> = std::iter::from_fn(|| observed.try_recv().ok())
        .map(|event| event.task.id)
        .collect();
    let expected: Vec<_> = (0..105)
        .filter(|i| ![0, 2, 100].contains(i))
        .map(|i| format!("task-{i:03}"))
        .collect();
    assert_eq!(recovered, expected);

    // Healthy executions survive the error return. Only the failed reloads
    // start on retry; no already-running executor receives a second invocation.
    assert_eq!(server.restart_tasks().await.unwrap(), 3);
    let mut retried: Vec<_> = std::iter::from_fn(|| observed.try_recv().ok())
        .map(|event| event.task.id)
        .collect();
    retried.sort();
    assert_eq!(retried, ["task-000", "task-002", "task-100"]);
    assert_eq!(server.restart_tasks().await.unwrap(), 0);
    assert!(observed.try_recv().is_err());
    server.shutdown().await;
}

struct PanicOnFirstTask(Executor);
impl AgentExecutor for PanicOnFirstTask {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        self.0.execute(ctx)
    }
    fn restart(
        &self,
        ctx: ExecutorContext,
    ) -> Option<BoxStream<'static, Result<StreamResponse, A2AError>>> {
        assert_ne!(ctx.task_id, "a-broken", "private recovery failure");
        self.0.restart(ctx)
    }
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        self.0.cancel(ctx)
    }
}

#[tokio::test]
async fn a_panicking_restart_hook_does_not_prevent_recovery_of_the_next_task() {
    let store = Store::default();
    seed(&store, "a-broken", TaskState::Working).await;
    seed(&store, "b-healthy", TaskState::Working).await;
    let (sent, mut observed) = mpsc::unbounded_channel();
    let server = handler(PanicOnFirstTask(Executor(sent)), store.clone());
    let error = timeout(Duration::from_secs(2), server.restart_tasks())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(
        error.message,
        "executor restart failed before accepting the task"
    );
    assert_eq!(observed.try_recv().unwrap().task.id, "b-healthy");
    assert!(observed.try_recv().is_err());
    timeout(Duration::from_secs(2), async {
        while store.get("a-broken").await.unwrap().unwrap().status.state != TaskState::Failed {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(server.restart_tasks().await.unwrap(), 0);
    server.shutdown().await;
}

#[tokio::test]
async fn a_listing_error_stops_recovery_without_undoing_successful_restarts() {
    let store = Store::default();
    for i in 0..105 {
        seed(&store, &format!("task-{i:03}"), TaskState::Working).await;
    }
    store.list_error_at.store(2, Ordering::SeqCst);
    let (sent, mut observed) = mpsc::unbounded_channel();
    let server = handler(Executor(sent), store.clone());
    let error = timeout(Duration::from_secs(2), server.restart_tasks())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.message, "recovery listing failed");
    assert_eq!(store.lists.load(Ordering::SeqCst), 2);
    let recovered: Vec<_> = std::iter::from_fn(|| observed.try_recv().ok())
        .map(|event| event.task.id)
        .collect();
    assert_eq!(
        recovered,
        (0..100).map(|i| format!("task-{i:03}")).collect::<Vec<_>>()
    );

    store.list_error_at.store(0, Ordering::SeqCst);
    assert_eq!(server.restart_tasks().await.unwrap(), 5);
    let recovered: Vec<_> = std::iter::from_fn(|| observed.try_recv().ok())
        .map(|event| event.task.id)
        .collect();
    assert_eq!(
        recovered,
        (100..105)
            .map(|i| format!("task-{i:03}"))
            .collect::<Vec<_>>()
    );
    server.shutdown().await;
}

struct Broken {
    panic: bool,
    foreign: bool,
}
impl AgentExecutor for Broken {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Legacy.execute(ctx)
    }
    fn restart(
        &self,
        mut ctx: ExecutorContext,
    ) -> Option<BoxStream<'static, Result<StreamResponse, A2AError>>> {
        if ctx.task_id != "task" {
            return None;
        }
        assert!(!self.panic, "private recovery failure");
        if self.foreign {
            ctx.task_id = "other".into();
            return Some(
                futures::stream::once(async move { Ok(status(&ctx, TaskState::Completed)) })
                    .boxed(),
            );
        }
        Some(
            futures::stream::once(async { Err(A2AError::internal("private recovery error")) })
                .boxed(),
        )
    }
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Legacy.cancel(ctx)
    }
}
#[tokio::test]
async fn restart_panics_and_stream_errors_use_normal_failure_persistence() {
    for (panic, foreign) in [(false, false), (true, false), (false, true)] {
        let store = Store::default();
        seed(&store, "task", TaskState::Working).await;
        seed(&store, "other", TaskState::Working).await;
        let other = store.get("other").await.unwrap().unwrap();
        let server = handler(Broken { panic, foreign }, store.clone());
        let result = timeout(Duration::from_secs(2), server.restart_tasks())
            .await
            .unwrap();
        assert_eq!(result.is_err(), panic);
        timeout(Duration::from_secs(2), async {
            loop {
                if store.get("task").await.unwrap().unwrap().status.state == TaskState::Failed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(store.get("other").await.unwrap().unwrap(), other);
        let failed = store.get("task").await.unwrap().unwrap();
        assert_eq!(
            failed.status.message.unwrap().text(),
            Some(if panic {
                "The agent failed unexpectedly."
            } else {
                "Internal error"
            })
        );
        assert!(
            server
                .cancel_task(
                    &ServiceParams::new(),
                    CancelTaskRequest {
                        id: "task".into(),
                        metadata: None,
                        tenant: None
                    }
                )
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn a_broken_store_cannot_loop_forever_by_repeating_its_recovery_cursor() {
    let store = Store::default();
    seed(&store, "done", TaskState::Completed).await;
    store.repeat_token.store(true, Ordering::SeqCst);
    let server = handler(Legacy, store.clone());
    let error = timeout(Duration::from_secs(2), server.restart_tasks())
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(error.message, "task store repeated a recovery page token");
    assert_eq!(store.lists.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn recovery_rechecks_tasks_canceled_or_purged_after_listing() {
    for purge in [false, true] {
        let store = Store {
            inner: Arc::new(if purge {
                InMemoryTaskStore::new().with_terminal_task_limit(0)
            } else {
                InMemoryTaskStore::new()
            }),
            ..Default::default()
        };
        seed(&store, "task", TaskState::Working).await;
        store.cancel_during_scan.store(true, Ordering::SeqCst);
        let (sent, mut observed) = mpsc::unbounded_channel();
        let server = handler(Executor(sent), store.clone());
        assert_eq!(server.restart_tasks().await.unwrap(), 0);
        assert!(observed.try_recv().is_err());
        let stored = store.get("task").await.unwrap();
        assert_eq!(stored.is_none(), purge);
        if let Some(task) = stored {
            assert_eq!(task.status.state, TaskState::Canceled);
        }
    }
}
