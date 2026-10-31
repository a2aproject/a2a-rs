// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! A2A 1.1 server semantics: task generation, timeline, optimistic
//! concurrency, long-polling, resumption and client-written artifacts.

use std::sync::Arc;
use std::time::{Duration, Instant};

use a2a::*;
use a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore, RequestHandler,
    ServiceParams,
};
use futures::{StreamExt, stream, stream::BoxStream};
use tokio::sync::Notify;

/// Works on a task, emits one artifact, then waits to be released before it
/// completes.
struct StepAgent {
    release: Arc<Notify>,
}

fn status(ctx: &ExecutorContext, state: TaskState, text: &str) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        status: TaskStatus {
            state,
            message: Some(Message::new(Role::Agent, vec![Part::text(text)])),
            timestamp: None,
        },
        metadata: None,
        generation: 0,
    })
}

impl AgentExecutor for StepAgent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let release = self.release.clone();
        Box::pin(async_stream::stream! {
            yield Ok(status(&ctx, TaskState::Working, "starting"));
            yield Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                artifact: Artifact {
                    artifact_id: "out".into(),
                    name: None,
                    description: None,
                    parts: vec![Part::text("result")],
                    metadata: None,
                    extensions: None,
                    start_generation: None,
                    end_generation: None,
                    role: None,
                },
                append: None,
                last_chunk: Some(true),
                metadata: None,
                generation: 0,
            }));
            release.notified().await;
            yield Ok(status(&ctx, TaskState::Completed, "done"));
        })
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::once(async move {
            Ok(status(&ctx, TaskState::Canceled, "canceled"))
        }))
    }
}

/// Completes straight away, asking for input first when the message says so.
struct QuickAgent;

impl AgentExecutor for QuickAgent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let wants_input = ctx.message.as_ref().and_then(|m| m.text()) == Some("needs input");
        let state = if wants_input {
            TaskState::InputRequired
        } else {
            TaskState::Completed
        };
        Box::pin(stream::once(async move { Ok(status(&ctx, state, "ok")) }))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::once(async move {
            Ok(status(&ctx, TaskState::Canceled, "canceled"))
        }))
    }
}

fn send(text: &str, task_id: Option<&str>) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task_id.map(Into::into);
    SendMessageRequest {
        message,
        configuration: None,
        metadata: None,
        tenant: None,
        artifact_updates: Vec::new(),
    }
}

fn with_config(
    mut req: SendMessageRequest,
    f: impl FnOnce(&mut SendMessageConfiguration),
) -> SendMessageRequest {
    let mut config = SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: None,
        history_length: None,
        timeline_length: None,
        return_immediately: None,
        if_generation_match: None,
    };
    f(&mut config);
    req.configuration = Some(config);
    req
}

fn get(id: &str) -> GetTaskRequest {
    GetTaskRequest {
        id: id.into(),
        history_length: None,
        tenant: None,
        timeline_length: None,
        current_generation: None,
    }
}

fn quick_handler() -> DefaultRequestHandler {
    DefaultRequestHandler::new(QuickAgent, InMemoryTaskStore::new())
}

fn task_of(response: SendMessageResponse) -> Task {
    match response {
        SendMessageResponse::Task(task) => task,
        other => panic!("expected a task, got {other:?}"),
    }
}

async fn open_step_task(handler: &DefaultRequestHandler) -> Task {
    let params = ServiceParams::new();
    task_of(
        handler
            .send_message(
                &params,
                with_config(send("go", None), |c| c.return_immediately = Some(true)),
            )
            .await
            .unwrap(),
    )
}

#[tokio::test]
async fn generation_counts_every_mutation_and_the_timeline_records_them() {
    let handler = quick_handler();
    let params = ServiceParams::new();

    let task = task_of(
        handler
            .send_message(&params, send("hi", None))
            .await
            .unwrap(),
    );

    let timeline = task.timeline.clone().expect("timeline is populated");
    assert!(
        matches!(&timeline[0].content, TimelineContent::Message(m) if m.text() == Some("hi")),
        "the client message opens the timeline: {timeline:?}"
    );
    assert_eq!(
        timeline[0].generation, 1,
        "a new task starts at generation 1"
    );
    let generations: Vec<_> = timeline.iter().map(|entry| entry.generation).collect();
    assert!(
        generations.windows(2).all(|pair| pair[1] > pair[0]),
        "entries are ordered by generation: {generations:?}"
    );
    assert_eq!(task.generation, *generations.last().unwrap());
    assert!(matches!(
        &timeline.last().unwrap().content,
        TimelineContent::Status(status) if status.state == TaskState::Completed
    ));
    assert!(
        task.history.is_some(),
        "history stays populated through 1.x"
    );
}

#[tokio::test]
async fn a_follow_up_message_is_appended_and_advances_the_generation() {
    let handler = quick_handler();
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("needs input", None))
            .await
            .unwrap(),
    );
    assert_eq!(first.status.state, TaskState::InputRequired);

    let second = task_of(
        handler
            .send_message(&params, send("here it is", Some(&first.id)))
            .await
            .unwrap(),
    );

    assert!(second.generation > first.generation);
    let timeline = second.timeline.unwrap();
    let client_messages: Vec<_> = timeline
        .iter()
        .filter_map(|entry| match &entry.content {
            TimelineContent::Message(message) => message.text(),
            _ => None,
        })
        .collect();
    assert_eq!(client_messages, ["needs input", "here it is"]);
}

#[tokio::test]
async fn streamed_events_carry_consecutive_generations() {
    let release = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        StepAgent {
            release: release.clone(),
        },
        InMemoryTaskStore::new(),
    );
    let params = ServiceParams::new();
    let mut stream = handler
        .send_streaming_message(&params, send("go", None))
        .await
        .unwrap();

    let mut generations = Vec::new();
    while generations.len() < 2 {
        match stream.next().await.unwrap().unwrap() {
            StreamResponse::StatusUpdate(update) => generations.push(update.generation),
            StreamResponse::ArtifactUpdate(update) => generations.push(update.generation),
            _ => {}
        }
    }
    release.notify_one();
    while let Some(event) = stream.next().await {
        if let StreamResponse::StatusUpdate(update) = event.unwrap() {
            generations.push(update.generation);
        }
    }

    assert!(generations.iter().all(|generation| *generation > 0));
    assert!(
        generations.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "one event per mutation, numbered consecutively: {generations:?}"
    );
}

#[tokio::test]
async fn artifacts_record_the_generations_they_started_and_ended_at() {
    let params = ServiceParams::new();
    let release = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        StepAgent {
            release: release.clone(),
        },
        InMemoryTaskStore::new(),
    );
    let task = open_step_task(&handler).await;
    // Let the executor reach its wait, then read the stored state.
    let mut stored = handler.get_task(&params, get(&task.id)).await.unwrap();
    for _ in 0..50 {
        if stored.artifacts.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        stored = handler.get_task(&params, get(&task.id)).await.unwrap();
    }
    release.notify_one();

    let artifact = &stored.artifacts.expect("the artifact was stored")[0];
    let start = artifact.start_generation.expect("start generation");
    assert_eq!(
        artifact.end_generation,
        Some(start),
        "a single chunk starts and ends at once"
    );
}

#[tokio::test]
async fn timeline_length_trims_the_response() {
    let handler = quick_handler();
    let params = ServiceParams::new();
    let task = task_of(
        handler
            .send_message(
                &params,
                with_config(send("hi", None), |c| c.timeline_length = Some(1)),
            )
            .await
            .unwrap(),
    );
    assert_eq!(task.timeline.as_ref().map(Vec::len), Some(1));

    let full = handler.get_task(&params, get(&task.id)).await.unwrap();
    assert!(
        full.timeline.unwrap().len() > 1,
        "the store keeps everything"
    );

    let mut request = get(&task.id);
    request.timeline_length = Some(0);
    let none = handler.get_task(&params, request).await.unwrap();
    assert!(none.timeline.is_none(), "zero omits the timeline");

    let listed = handler
        .list_tasks(
            &params,
            ListTasksRequest {
                context_id: None,
                status: None,
                page_size: None,
                page_token: None,
                history_length: None,
                timeline_length: Some(1),
                status_timestamp_after: None,
                include_artifacts: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(listed.tasks[0].timeline.as_ref().map(Vec::len), Some(1));
}

#[tokio::test]
async fn if_generation_match_rejects_a_stale_follow_up() {
    let handler = quick_handler();
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("needs input", None))
            .await
            .unwrap(),
    );

    let stale = handler
        .send_message(
            &params,
            with_config(send("late", Some(&first.id)), |c| {
                c.if_generation_match = Some(first.generation - 1)
            }),
        )
        .await
        .unwrap_err();

    assert_eq!(stale.code, error_code::TASK_GENERATION_MISMATCH);
    assert_eq!(stale.http_status_code(), 409);
    let info = stale.to_jsonrpc_error().data.unwrap().to_string();
    assert!(info.contains("TASK_GENERATION_MISMATCH"), "{info}");
    assert!(info.contains(&first.generation.to_string()), "{info}");

    let current = task_of(
        handler
            .send_message(
                &params,
                with_config(send("on time", Some(&first.id)), |c| {
                    c.if_generation_match = Some(first.generation)
                }),
            )
            .await
            .unwrap(),
    );
    assert!(current.generation > first.generation);
}

#[tokio::test]
async fn if_generation_match_is_ignored_when_the_message_creates_the_task() {
    let handler = quick_handler();
    let params = ServiceParams::new();

    let created = handler
        .send_message(
            &params,
            with_config(send("new", None), |c| c.if_generation_match = Some(99)),
        )
        .await;

    assert!(created.is_ok(), "{created:?}");
}

#[tokio::test]
async fn long_poll_returns_at_once_when_the_task_is_already_ahead() {
    let handler = quick_handler();
    let params = ServiceParams::new();
    let task = task_of(
        handler
            .send_message(&params, send("hi", None))
            .await
            .unwrap(),
    );

    let mut request = get(&task.id);
    request.current_generation = Some(0);
    let started = Instant::now();
    let polled = handler.get_task(&params, request).await.unwrap();

    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(polled.generation, task.generation);
}

#[tokio::test]
async fn long_poll_holds_until_the_generation_advances() {
    let release = Arc::new(Notify::new());
    let handler = Arc::new(DefaultRequestHandler::new(
        StepAgent {
            release: release.clone(),
        },
        InMemoryTaskStore::new(),
    ));
    let params = ServiceParams::new();
    let task = open_step_task(&handler).await;
    // Wait for the executor to settle at its release point.
    let mut settled = handler.get_task(&params, get(&task.id)).await.unwrap();
    for _ in 0..50 {
        if settled.artifacts.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        settled = handler.get_task(&params, get(&task.id)).await.unwrap();
    }

    let mut request = get(&task.id);
    request.current_generation = Some(settled.generation);
    let poller = {
        let handler = handler.clone();
        tokio::spawn(async move { handler.get_task(&ServiceParams::new(), request).await })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !poller.is_finished(),
        "nothing changed, so the poll is held"
    );

    release.notify_one();
    let polled = tokio::time::timeout(Duration::from_secs(5), poller)
        .await
        .expect("the poll returns once the task advances")
        .unwrap()
        .unwrap();
    assert!(polled.generation > settled.generation);
}

#[tokio::test]
async fn long_poll_gives_up_with_the_current_state_after_its_timeout() {
    let release = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        StepAgent {
            release: release.clone(),
        },
        InMemoryTaskStore::new(),
    )
    .with_long_poll_timeout(Duration::from_millis(150));
    let params = ServiceParams::new();
    let task = open_step_task(&handler).await;
    let mut settled = handler.get_task(&params, get(&task.id)).await.unwrap();
    for _ in 0..50 {
        if settled.artifacts.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        settled = handler.get_task(&params, get(&task.id)).await.unwrap();
    }

    let mut request = get(&task.id);
    request.current_generation = Some(settled.generation);
    let started = Instant::now();
    let polled = handler.get_task(&params, request).await.unwrap();

    assert!(started.elapsed() >= Duration::from_millis(100));
    assert_eq!(polled.generation, settled.generation);
    release.notify_one();
}

#[tokio::test]
async fn subscribe_resumes_without_a_snapshot_when_the_client_is_current() {
    let release = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        StepAgent {
            release: release.clone(),
        },
        InMemoryTaskStore::new(),
    );
    let params = ServiceParams::new();
    let task = open_step_task(&handler).await;
    let mut settled = handler.get_task(&params, get(&task.id)).await.unwrap();
    for _ in 0..50 {
        if settled.artifacts.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        settled = handler.get_task(&params, get(&task.id)).await.unwrap();
    }

    let mut current = handler
        .subscribe_to_task(
            &params,
            SubscribeToTaskRequest {
                id: task.id.clone(),
                tenant: None,
                last_seen_generation: Some(settled.generation),
            },
        )
        .await
        .unwrap();
    let mut behind = handler
        .subscribe_to_task(
            &params,
            SubscribeToTaskRequest {
                id: task.id.clone(),
                tenant: None,
                last_seen_generation: Some(settled.generation - 1),
            },
        )
        .await
        .unwrap();

    // A client that is behind cannot be replayed to, so it gets a snapshot.
    assert!(matches!(
        behind.next().await.unwrap().unwrap(),
        StreamResponse::Task(_)
    ));
    release.notify_one();
    // A current client goes straight to what follows.
    match current.next().await.unwrap().unwrap() {
        StreamResponse::StatusUpdate(update) => {
            assert_eq!(update.generation, settled.generation + 1);
            assert_eq!(update.status.state, TaskState::Completed);
        }
        other => panic!("expected the next update, not a snapshot: {other:?}"),
    }
}

fn artifact_update(text: &str) -> TaskArtifactUpdateEvent {
    TaskArtifactUpdateEvent {
        task_id: String::new(),
        context_id: String::new(),
        artifact: Artifact {
            artifact_id: "from-client".into(),
            name: None,
            description: None,
            parts: vec![Part::text(text)],
            metadata: None,
            extensions: None,
            start_generation: None,
            end_generation: None,
            role: None,
        },
        append: None,
        last_chunk: Some(true),
        metadata: None,
        generation: 41,
    }
}

#[tokio::test]
async fn artifact_updates_are_refused_unless_the_agent_declares_them() {
    let handler = quick_handler();
    let mut request = send("hi", None);
    request.artifact_updates = vec![artifact_update("data")];

    let error = handler
        .send_message(&ServiceParams::new(), request)
        .await
        .unwrap_err();

    assert_eq!(error.code, error_code::UNSUPPORTED_OPERATION);
}

#[tokio::test]
async fn declared_user_artifacts_are_stored_as_the_clients_and_numbered_by_the_server() {
    let handler = quick_handler().with_capabilities(AgentCapabilities {
        user_artifacts: Some(true),
        ..AgentCapabilities::default()
    });
    let mut request = send("hi", None);
    request.artifact_updates = vec![artifact_update("data")];

    let task = task_of(
        handler
            .send_message(&ServiceParams::new(), request)
            .await
            .unwrap(),
    );

    let artifact = &task.artifacts.expect("stored")[0];
    assert_eq!(artifact.role, Some(Role::User));
    assert_eq!(
        artifact.start_generation,
        Some(2),
        "after the message at generation 1"
    );
    assert!(
        task.generation < 41,
        "the generation the client supplied is ignored: {}",
        task.generation
    );
}

#[tokio::test]
async fn cancelling_advances_the_generation() {
    let release = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        StepAgent {
            release: release.clone(),
        },
        InMemoryTaskStore::new(),
    );
    let params = ServiceParams::new();
    let task = open_step_task(&handler).await;
    let before = handler.get_task(&params, get(&task.id)).await.unwrap();

    let canceled = handler
        .cancel_task(
            &params,
            CancelTaskRequest {
                id: task.id.clone(),
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(canceled.status.state, TaskState::Canceled);
    assert!(canceled.generation > before.generation);
    assert!(matches!(
        &canceled.timeline.unwrap().last().unwrap().content,
        TimelineContent::Status(status) if status.state == TaskState::Canceled
    ));
}

#[tokio::test]
async fn push_configs_remember_the_version_of_the_request_that_created_them() {
    let handler =
        quick_handler().with_push_config_store(a2a_server::InMemoryPushConfigStore::new());
    let task = task_of(
        handler
            .send_message(&ServiceParams::new(), send("needs input", None))
            .await
            .unwrap(),
    );
    let config = |url: &str| TaskPushNotificationConfig {
        url: url.into(),
        id: Some(url.into()),
        task_id: task.id.clone(),
        token: None,
        authentication: None,
        tenant: None,
        protocol_version: None,
    };

    let mut announced = ServiceParams::new();
    announced.insert(SVC_PARAM_VERSION.to_string(), vec!["1.1".to_string()]);
    let saved = handler
        .create_push_config(&announced, config("https://example.com/new"))
        .await
        .unwrap();
    assert_eq!(saved.protocol_version.as_deref(), Some("1.1"));

    let saved = handler
        .create_push_config(&ServiceParams::new(), config("https://example.com/old"))
        .await
        .unwrap();
    assert_eq!(saved.protocol_version.as_deref(), Some("1.0"));
}

// ---------------------------------------------------------------------------
// Regressions found in review
// ---------------------------------------------------------------------------

/// Behaves according to the text of the message it is given.
struct ScriptAgent {
    release: Arc<Notify>,
    cancel: CancelMode,
}

/// How the agent answers a cancel.
#[derive(Clone)]
enum CancelMode {
    /// A status update to `CANCELED`.
    Status,
    /// Nothing at all.
    Silent,
    /// A full `Task` snapshot in the `CANCELED` state.
    Snapshot,
    /// An error.
    Fail,
    /// A message-update event, which only the server may produce.
    ForgedMessage,
    /// A status that would bring the canceled task back to work.
    Revive,
    /// A status that would end the canceled task some other way.
    Complete,
    /// A status to `CANCELED`, but only once the gate is opened.
    Stall(Arc<Notify>),
    /// An artifact and no status at all.
    ArtifactOnly,
}

fn blank_artifact(id: &str, text: &str) -> Artifact {
    Artifact {
        artifact_id: id.into(),
        name: None,
        description: None,
        parts: vec![Part::text(text)],
        metadata: None,
        extensions: None,
        start_generation: None,
        end_generation: None,
        role: None,
    }
}

fn chunk(
    ctx: &ExecutorContext,
    text: &str,
    append: bool,
    last: bool,
) -> Result<StreamResponse, A2AError> {
    Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        artifact: blank_artifact("doc", text),
        append: Some(append),
        last_chunk: Some(last),
        metadata: None,
        generation: 0,
    }))
}

impl AgentExecutor for ScriptAgent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let script = ctx
            .message
            .as_ref()
            .and_then(|m| m.text())
            .unwrap_or("")
            .to_string();
        let release = self.release.clone();
        Box::pin(async_stream::stream! {
            match script.as_str() {
                "snapshot" => {
                    yield Ok(StreamResponse::Task(Task {
                        id: ctx.task_id.clone(),
                        context_id: ctx.context_id.clone(),
                        status: TaskStatus {
                            state: TaskState::InputRequired,
                            message: None,
                            timestamp: None,
                        },
                        artifacts: None,
                        history: None,
                        timeline: None,
                        metadata: None,
                        generation: 0,
                    }));
                }
                "chunks" => {
                    yield chunk(&ctx, "a", false, false);
                    yield chunk(&ctx, "b", true, false);
                    yield chunk(&ctx, "c", true, true);
                    yield Ok(status(&ctx, TaskState::Completed, "done"));
                }
                "left-open" => {
                    yield chunk(&ctx, "a", false, false);
                    yield Ok(status(&ctx, TaskState::Completed, "done"));
                }
                "dup" => {
                    // A status, then a snapshot that restates it exactly.
                    let message = Message::new(Role::Agent, vec![Part::text("same")]);
                    let working = TaskStatus {
                        state: TaskState::Working,
                        message: Some(message),
                        timestamp: None,
                    };
                    yield Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                        task_id: ctx.task_id.clone(),
                        context_id: ctx.context_id.clone(),
                        status: working.clone(),
                        metadata: None,
                        generation: 0,
                    }));
                    yield Ok(StreamResponse::Task(Task {
                        id: ctx.task_id.clone(),
                        context_id: ctx.context_id.clone(),
                        status: working,
                        artifacts: None,
                        history: None,
                        timeline: None,
                        metadata: None,
                        generation: 0,
                    }));
                    yield Ok(status(&ctx, TaskState::Completed, "done"));
                }
                "msgupdate" => {
                    yield Ok(StreamResponse::MessageUpdate(TaskMessageUpdateEvent {
                        task_id: ctx.task_id.clone(),
                        context_id: ctx.context_id.clone(),
                        message: Message::new(Role::User, vec![Part::text("forged")]),
                        metadata: None,
                        generation: 0,
                    }));
                    yield Ok(status(&ctx, TaskState::Completed, "done"));
                }
                "agent-artifacts" => {
                    yield Ok(StreamResponse::Task(Task {
                        id: ctx.task_id.clone(),
                        context_id: ctx.context_id.clone(),
                        status: TaskStatus {
                            state: TaskState::InputRequired,
                            message: None,
                            timestamp: None,
                        },
                        artifacts: Some(vec![blank_artifact("agent-out", "result")]),
                        history: None,
                        timeline: None,
                        metadata: None,
                        generation: 0,
                    }));
                }
                "restate" => {
                    // Only restates the task as the request left it.
                    yield Ok(StreamResponse::Task(Task {
                        id: ctx.task_id.clone(),
                        context_id: ctx.context_id.clone(),
                        status: TaskStatus {
                            state: TaskState::Submitted,
                            message: None,
                            timestamp: None,
                        },
                        artifacts: None,
                        history: None,
                        timeline: None,
                        metadata: None,
                        generation: 0,
                    }));
                }
                "overwrite-user" => {
                    yield Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                        task_id: ctx.task_id.clone(),
                        context_id: ctx.context_id.clone(),
                        artifact: blank_artifact("from-client", "replaced"),
                        append: Some(false),
                        last_chunk: Some(true),
                        metadata: None,
                        generation: 0,
                    }));
                    yield Ok(status(&ctx, TaskState::InputRequired, "need input"));
                }
                "panic" => {
                    yield Ok(status(&ctx, TaskState::Working, "about to fail"));
                    panic!("the agent blew up");
                }
                "hold" => {
                    yield Ok(status(&ctx, TaskState::Working, "holding"));
                    release.notified().await;
                    yield Ok(status(&ctx, TaskState::Completed, "done"));
                }
                _ => {
                    yield Ok(status(&ctx, TaskState::InputRequired, "need input"));
                }
            }
        })
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        match &self.cancel {
            CancelMode::Stall(gate) => {
                let gate = gate.clone();
                Box::pin(stream::once(async move {
                    gate.notified().await;
                    Ok(status(&ctx, TaskState::Canceled, "canceled"))
                }))
            }
            CancelMode::Silent => Box::pin(stream::empty()),
            CancelMode::ArtifactOnly => Box::pin(stream::once(async move {
                Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                    task_id: ctx.task_id,
                    context_id: ctx.context_id,
                    artifact: blank_artifact("partial", "what it had so far"),
                    append: Some(false),
                    last_chunk: Some(true),
                    metadata: None,
                    generation: 0,
                }))
            })),
            CancelMode::Fail => Box::pin(stream::once(async {
                Err(A2AError::internal("the agent could not cancel"))
            })),
            CancelMode::ForgedMessage => Box::pin(stream::once(async move {
                Ok(StreamResponse::MessageUpdate(TaskMessageUpdateEvent {
                    task_id: ctx.task_id,
                    context_id: ctx.context_id,
                    message: Message::new(Role::User, vec![Part::text("forged")]),
                    metadata: None,
                    generation: 0,
                }))
            })),
            CancelMode::Revive => Box::pin(stream::once(async move {
                Ok(status(&ctx, TaskState::Working, "back to work"))
            })),
            CancelMode::Complete => Box::pin(stream::once(async move {
                Ok(status(&ctx, TaskState::Completed, "done after all"))
            })),
            CancelMode::Status => Box::pin(stream::once(async move {
                Ok(status(&ctx, TaskState::Canceled, "canceled"))
            })),
            CancelMode::Snapshot => Box::pin(stream::once(async move {
                // Restates the task exactly as `begin_cancel` left it.
                let status = ctx
                    .stored_task
                    .as_ref()
                    .map(|task| task.status.clone())
                    .unwrap_or(TaskStatus {
                        state: TaskState::Canceled,
                        message: None,
                        timestamp: None,
                    });
                Ok(StreamResponse::Task(Task {
                    id: ctx.task_id,
                    context_id: ctx.context_id,
                    status,
                    artifacts: None,
                    history: None,
                    timeline: None,
                    metadata: None,
                    generation: 0,
                }))
            })),
        }
    }
}

fn script_handler(cancel: CancelMode) -> (DefaultRequestHandler, Arc<Notify>) {
    let release = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        ScriptAgent {
            release: release.clone(),
            cancel,
        },
        InMemoryTaskStore::new(),
    )
    .with_capabilities(AgentCapabilities {
        user_artifacts: Some(true),
        ..AgentCapabilities::default()
    });
    (handler, release)
}

fn timeline_of(task: &Task) -> &[TimelineEntry] {
    task.timeline.as_deref().expect("a timeline")
}

fn canceled_entries(task: &Task) -> usize {
    timeline_of(task)
        .iter()
        .filter(|entry| {
            matches!(&entry.content, TimelineContent::Status(s) if s.state == TaskState::Canceled)
        })
        .count()
}

#[tokio::test]
async fn a_snapshot_keeps_the_artifacts_the_client_wrote() {
    let (handler, _) = script_handler(CancelMode::Status);
    let mut request = send("snapshot", None);
    request.artifact_updates = vec![artifact_update("mine")];

    let task = task_of(
        handler
            .send_message(&ServiceParams::new(), request)
            .await
            .unwrap(),
    );

    let artifacts = task.artifacts.expect("the client's artifact survives");
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].role, Some(Role::User));
    assert!(task.history.is_some(), "so does the history");
}

#[tokio::test]
async fn a_snapshot_that_changes_nothing_is_not_a_mutation() {
    let (handler, _) = script_handler(CancelMode::Status);
    let params = ServiceParams::new();
    let task = task_of(
        handler
            .send_message(&params, send("snapshot", None))
            .await
            .unwrap(),
    );
    let generation = task.generation;

    let again = task_of(
        handler
            .send_message(&params, send("snapshot", Some(&task.id)))
            .await
            .unwrap(),
    );

    // Only the new client message advanced it.
    assert_eq!(again.generation, generation + 1);
}

#[tokio::test]
async fn artifact_generations_follow_the_chunks() {
    let (handler, _) = script_handler(CancelMode::Status);
    let task = task_of(
        handler
            .send_message(&ServiceParams::new(), send("chunks", None))
            .await
            .unwrap(),
    );

    let artifact = &task.artifacts.as_ref().unwrap()[0];
    // 1 message, then the three chunks at 2, 3, 4; the status at 5.
    assert_eq!(artifact.start_generation, Some(2));
    assert_eq!(artifact.end_generation, Some(4), "closed by its last chunk");
    assert_eq!(task.generation, 5);
}

#[tokio::test]
async fn an_artifact_left_open_is_closed_when_the_task_ends() {
    let (handler, _) = script_handler(CancelMode::Status);
    let task = task_of(
        handler
            .send_message(&ServiceParams::new(), send("left-open", None))
            .await
            .unwrap(),
    );

    let artifact = &task.artifacts.as_ref().unwrap()[0];
    assert_eq!(artifact.start_generation, Some(2));
    assert_eq!(
        artifact.end_generation,
        Some(task.generation),
        "a first chunk marked not-last is not complete until the task is"
    );
}

async fn open_held_task(
    handler: &DefaultRequestHandler,
) -> (TaskId, BoxStream<'static, Result<StreamResponse, A2AError>>) {
    let mut held = handler
        .send_streaming_message(&ServiceParams::new(), send("hold", None))
        .await
        .unwrap();
    let task_id = loop {
        if let StreamResponse::StatusUpdate(update) = held.next().await.unwrap().unwrap() {
            break update.task_id;
        }
    };
    (task_id, held)
}

fn cancel(id: &str) -> CancelTaskRequest {
    CancelTaskRequest {
        id: id.into(),
        metadata: None,
        tenant: None,
    }
}

#[tokio::test]
async fn cancelling_records_exactly_one_canceled_entry() {
    let (handler, release) = script_handler(CancelMode::Status);
    let (task_id, mut held) = open_held_task(&handler).await;

    let canceled = handler
        .cancel_task(&ServiceParams::new(), cancel(&task_id))
        .await
        .unwrap();

    assert_eq!(canceled.status.state, TaskState::Canceled);
    assert_eq!(
        canceled_entries(&canceled),
        1,
        "{:?}",
        timeline_of(&canceled)
    );
    assert_eq!(
        canceled.generation,
        timeline_of(&canceled).last().unwrap().generation
    );
    release.notify_one();
    // The subscriber sees the cancel as an event, not as an error.
    let mut generations = Vec::new();
    while let Some(event) = held.next().await {
        if let StreamResponse::StatusUpdate(update) = event.unwrap() {
            generations.push(update.generation);
        }
    }
    assert_eq!(generations.last().copied(), Some(canceled.generation));
}

#[tokio::test]
async fn cancelling_with_a_silent_executor_still_records_the_transition() {
    let (handler, release) = script_handler(CancelMode::Silent);
    let (task_id, _held) = open_held_task(&handler).await;
    let before = handler
        .get_task(&ServiceParams::new(), get(&task_id))
        .await
        .unwrap();

    let canceled = handler
        .cancel_task(&ServiceParams::new(), cancel(&task_id))
        .await
        .unwrap();

    assert_eq!(canceled_entries(&canceled), 1);
    assert_eq!(canceled.generation, before.generation + 1);
    release.notify_one();
}

#[tokio::test]
async fn a_follow_up_is_visible_while_its_turn_is_still_running() {
    let (handler, release) = script_handler(CancelMode::Status);
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("ask", None))
            .await
            .unwrap(),
    );
    assert_eq!(first.status.state, TaskState::InputRequired);

    let mut turn = handler
        .send_streaming_message(&params, send("hold", Some(&first.id)))
        .await
        .unwrap();

    // The executor has not finished (it is held), yet the message is stored.
    let during = handler.get_task(&params, get(&first.id)).await.unwrap();
    assert!(during.generation > first.generation);
    assert!(
        timeline_of(&during).iter().any(|entry| matches!(
            &entry.content,
            TimelineContent::Message(message) if message.text() == Some("hold")
        )),
        "{:?}",
        timeline_of(&during)
    );
    release.notify_one();
    while turn.next().await.is_some() {}
}

/// A store whose next `get` is slow, to hold one request between reading the
/// task and acting on it.
#[derive(Clone)]
struct SlowGetStore {
    inner: Arc<InMemoryTaskStore>,
    slow: Arc<std::sync::atomic::AtomicBool>,
}

impl SlowGetStore {
    fn new() -> Self {
        SlowGetStore {
            inner: Arc::new(InMemoryTaskStore::new()),
            slow: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn slow_next_get(&self) {
        self.slow.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl a2a_server::TaskStore for SlowGetStore {
    async fn create(&self, task: Task) -> Result<a2a_server::task_store::TaskVersion, A2AError> {
        self.inner.create(task).await
    }
    async fn update(&self, task: Task) -> Result<a2a_server::task_store::TaskVersion, A2AError> {
        self.inner.update(task).await
    }
    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        if self.slow.swap(false, std::sync::atomic::Ordering::SeqCst) {
            let read = self.inner.get(task_id).await;
            tokio::time::sleep(Duration::from_millis(200)).await;
            return read;
        }
        self.inner.get(task_id).await
    }
    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.inner.list(req).await
    }
    async fn begin_cancel(&self, task_id: &str) -> Result<Task, A2AError> {
        self.inner.begin_cancel(task_id).await
    }
}

#[tokio::test]
async fn racing_follow_ups_with_the_same_expected_generation_do_not_both_win() {
    let release = Arc::new(Notify::new());
    let store = SlowGetStore::new();
    let handler = Arc::new(DefaultRequestHandler::new(
        ScriptAgent {
            release: release.clone(),
            cancel: CancelMode::Status,
        },
        store.clone(),
    ));
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("ask", None))
            .await
            .unwrap(),
    );
    store.slow_next_get();

    let follow_up = |text: &'static str, delay: u64| {
        let handler = handler.clone();
        let id = first.id.clone();
        let expected = first.generation;
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            handler
                .send_message(
                    &ServiceParams::new(),
                    with_config(send(text, Some(&id)), |c| {
                        c.if_generation_match = Some(expected)
                    }),
                )
                .await
        })
    };
    let slow = follow_up("from B", 0);
    let fast = follow_up("from A", 60);
    let (slow, fast) = (slow.await.unwrap(), fast.await.unwrap());

    assert_eq!(
        [slow.is_ok(), fast.is_ok()]
            .iter()
            .filter(|ok| **ok)
            .count(),
        1,
        "exactly one wins: {slow:?} / {fast:?}"
    );
    // The reservation turns the loser away; the guard on the generation is
    // what refuses a request that gets past it (see the stale-generation test).
    let loser = if slow.is_ok() { &fast } else { &slow };
    assert_eq!(
        loser.as_ref().unwrap_err().code,
        error_code::INVALID_REQUEST
    );
    let stored = handler.get_task(&params, get(&first.id)).await.unwrap();
    let from_clients: Vec<_> = timeline_of(&stored)
        .iter()
        .filter_map(|entry| match &entry.content {
            TimelineContent::Message(message) => message.text(),
            _ => None,
        })
        .collect();
    assert_eq!(
        from_clients.len(),
        2,
        "the first message and the winner: {from_clients:?}"
    );
    let generations: Vec<_> = timeline_of(&stored).iter().map(|e| e.generation).collect();
    assert!(
        generations.windows(2).all(|pair| pair[1] > pair[0]),
        "no generation is reused: {generations:?}"
    );
}

fn artifact_only(task_id: &str, text: &str) -> SendMessageRequest {
    let mut update = artifact_update(text);
    update.task_id = task_id.into();
    let mut request = send("", None);
    request.message.role = Role::Unspecified;
    request.message.message_id = String::new();
    request.message.parts.clear();
    request.artifact_updates = vec![update];
    request
}

#[tokio::test]
async fn artifacts_alone_extend_an_existing_task_instead_of_creating_one() {
    let (handler, _) = script_handler(CancelMode::Status);
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("ask", None))
            .await
            .unwrap(),
    );

    let after = task_of(
        handler
            .send_message(&params, artifact_only(&first.id, "late work"))
            .await
            .unwrap(),
    );

    assert_eq!(after.id, first.id);
    assert!(after.artifacts.as_ref().is_some_and(|a| a.len() == 1));
    let client_messages = timeline_of(&after)
        .iter()
        .filter(|e| matches!(e.content, TimelineContent::Message(_)))
        .count();
    assert_eq!(client_messages, 1, "no empty message was recorded");
    let listed = handler
        .list_tasks(
            &params,
            ListTasksRequest {
                context_id: None,
                status: None,
                page_size: None,
                page_token: None,
                history_length: None,
                timeline_length: None,
                status_timestamp_after: None,
                include_artifacts: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(listed.tasks.len(), 1, "no second task was created");
}

#[tokio::test]
async fn artifacts_alone_must_name_an_existing_task() {
    let (handler, _) = script_handler(CancelMode::Status);
    let error = handler
        .send_message(&ServiceParams::new(), artifact_only("", "orphan"))
        .await
        .unwrap_err();
    assert_eq!(error.code, error_code::INVALID_PARAMS);

    let error = handler
        .send_message(
            &ServiceParams::new(),
            artifact_only("no-such-task", "orphan"),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, error_code::TASK_NOT_FOUND);
}

#[tokio::test]
async fn client_artifact_updates_must_target_their_own_task() {
    let (handler, _) = script_handler(CancelMode::Status);
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("ask", None))
            .await
            .unwrap(),
    );

    let mut request = send("more", Some(&first.id));
    let mut update = artifact_update("x");
    update.task_id = "another-task".into();
    request.artifact_updates = vec![update];

    let error = handler.send_message(&params, request).await.unwrap_err();

    assert_eq!(error.code, error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn a_client_cannot_change_an_artifact_the_agent_wrote() {
    use a2a_server::TaskStore;
    let params = ServiceParams::new();
    // A task, open to follow-ups, that already holds an agent-written artifact.
    let store = InMemoryTaskStore::new();
    store
        .create(Task {
            id: "t-agent".into(),
            context_id: "c-agent".into(),
            status: TaskStatus {
                state: TaskState::InputRequired,
                message: None,
                timestamp: None,
            },
            artifacts: Some(vec![Artifact {
                start_generation: Some(2),
                ..blank_artifact("doc", "agent")
            }]),
            history: None,
            timeline: None,
            metadata: None,
            generation: 3,
        })
        .await
        .unwrap();
    let handler = DefaultRequestHandler::new(
        ScriptAgent {
            release: Arc::new(Notify::new()),
            cancel: CancelMode::Status,
        },
        store,
    )
    .with_capabilities(AgentCapabilities {
        user_artifacts: Some(true),
        ..AgentCapabilities::default()
    });

    let mut request = send("tamper", Some("t-agent"));
    let mut update = artifact_update("overwritten");
    update.artifact.artifact_id = "doc".into();
    request.artifact_updates = vec![update];
    let error = handler.send_message(&params, request).await.unwrap_err();

    assert_eq!(error.code, error_code::INVALID_PARAMS);
}

// ---------------------------------------------------------------------------
// Regressions found in the second review
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_dropped_request_gives_the_task_back() {
    let release = Arc::new(Notify::new());
    let store = SlowGetStore::new();
    let handler = DefaultRequestHandler::new(
        ScriptAgent {
            release: release.clone(),
            cancel: CancelMode::Status,
        },
        store.clone(),
    );
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("ask", None))
            .await
            .unwrap(),
    );
    store.slow_next_get();

    // A client that gives up while the request is still being prepared.
    let abandoned = tokio::time::timeout(
        Duration::from_millis(50),
        handler.send_message(&params, send("hold", Some(&first.id))),
    )
    .await;
    assert!(abandoned.is_err(), "the request was still in flight");

    // The reservation went with the dropped future: the very next request,
    // with no pause, finds the task free.
    let retried = handler
        .send_message(&params, send("ask", Some(&first.id)))
        .await;
    assert!(
        retried.is_ok(),
        "the task stayed reserved after its request was dropped: {retried:?}"
    );
}

#[tokio::test]
async fn a_cancel_during_a_follow_ups_preparation_is_ordered_after_it() {
    let release = Arc::new(Notify::new());
    let store = SlowGetStore::new();
    let handler = Arc::new(DefaultRequestHandler::new(
        ScriptAgent {
            release: release.clone(),
            cancel: CancelMode::Status,
        },
        store.clone(),
    ));
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("ask", None))
            .await
            .unwrap(),
    );
    store.slow_next_get();

    let follow_up = {
        let handler = handler.clone();
        let id = first.id.clone();
        tokio::spawn(async move {
            handler
                .send_message(&ServiceParams::new(), send("hold", Some(&id)))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(40)).await;
    let canceled = handler
        .cancel_task(&params, cancel(&first.id))
        .await
        .expect("the cancel waits for the follow-up and then applies");
    let follow_up = follow_up.await.unwrap();

    assert_eq!(canceled.status.state, TaskState::Canceled);
    if let Err(error) = &follow_up {
        assert_ne!(
            error.code,
            error_code::INVALID_REQUEST,
            "the follow-up must not fail with a store transition error: {error:?}"
        );
    }
    let stored = handler.get_task(&params, get(&first.id)).await.unwrap();
    assert_eq!(stored.status.state, TaskState::Canceled);
    assert_eq!(canceled_entries(&stored), 1, "{:?}", timeline_of(&stored));
    let generations: Vec<_> = timeline_of(&stored).iter().map(|e| e.generation).collect();
    assert!(
        generations.windows(2).all(|pair| pair[1] > pair[0]),
        "{generations:?}"
    );
    release.notify_one();
}

#[tokio::test]
async fn cancelling_with_a_snapshot_still_records_the_transition() {
    let (handler, release) = script_handler(CancelMode::Snapshot);
    let (task_id, _held) = open_held_task(&handler).await;
    let before = handler
        .get_task(&ServiceParams::new(), get(&task_id))
        .await
        .unwrap();

    let canceled = handler
        .cancel_task(&ServiceParams::new(), cancel(&task_id))
        .await
        .unwrap();

    assert_eq!(
        canceled_entries(&canceled),
        1,
        "{:?}",
        timeline_of(&canceled)
    );
    assert!(canceled.generation > before.generation);
    release.notify_one();
}

#[tokio::test]
async fn a_message_with_content_is_never_treated_as_artifact_only() {
    let (handler, _) = script_handler(CancelMode::Status);
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("ask", None))
            .await
            .unwrap(),
    );

    // No messageId, but a body and a task of its own, plus an update for
    // another task: ambiguous, so refused rather than guessed at.
    let mut request = send("real body", Some(&first.id));
    request.message.message_id = String::new();
    request.artifact_updates = vec![{
        let mut update = artifact_update("x");
        update.task_id = "some-other-task".into();
        update
    }];
    let error = handler.send_message(&params, request).await.unwrap_err();

    assert_eq!(error.code, error_code::INVALID_PARAMS);
}

#[tokio::test]
async fn an_executor_snapshot_without_metadata_keeps_the_stored_metadata() {
    use a2a_server::TaskStore;
    let store = InMemoryTaskStore::new();
    store
        .create(Task {
            id: "t-meta".into(),
            context_id: "c-meta".into(),
            status: TaskStatus {
                state: TaskState::InputRequired,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            timeline: None,
            metadata: Some([("owner".to_string(), serde_json::json!("alice"))].into()),
            generation: 1,
        })
        .await
        .unwrap();
    let handler = DefaultRequestHandler::new(
        ScriptAgent {
            release: Arc::new(Notify::new()),
            cancel: CancelMode::Status,
        },
        store,
    );

    // The "snapshot" script answers with a Task that carries no metadata.
    let task = task_of(
        handler
            .send_message(&ServiceParams::new(), send("snapshot", Some("t-meta")))
            .await
            .unwrap(),
    );

    assert_eq!(
        task.metadata.as_ref().and_then(|m| m.get("owner")),
        Some(&serde_json::json!("alice"))
    );
}

// ---------------------------------------------------------------------------
// Regressions found in the final validation
// ---------------------------------------------------------------------------

async fn first_event(
    mut stream: BoxStream<'static, Result<StreamResponse, A2AError>>,
) -> Option<Result<StreamResponse, A2AError>> {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the stream produced something")
}

#[tokio::test]
async fn subscribing_while_a_follow_up_is_prepared_opens_with_the_task() {
    let release = Arc::new(Notify::new());
    let store = SlowGetStore::new();
    let handler = Arc::new(DefaultRequestHandler::new(
        ScriptAgent {
            release: release.clone(),
            cancel: CancelMode::Status,
        },
        store.clone(),
    ));
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("ask", None))
            .await
            .unwrap(),
    );
    store.slow_next_get();
    let follow_up = {
        let handler = handler.clone();
        let id = first.id.clone();
        tokio::spawn(async move {
            handler
                .send_message(&ServiceParams::new(), send("hold", Some(&id)))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(40)).await;

    let stream = handler
        .subscribe_to_task(
            &params,
            SubscribeToTaskRequest {
                id: first.id.clone(),
                tenant: None,
                last_seen_generation: None,
            },
        )
        .await
        .expect("the task is executing");

    match first_event(stream).await.unwrap().unwrap() {
        StreamResponse::Task(task) => {
            assert!(
                task.generation > first.generation,
                "it reflects the follow-up"
            );
        }
        other => panic!("a subscription opens with the task, got {other:?}"),
    }
    release.notify_one();
    follow_up.await.unwrap().unwrap();
}

#[tokio::test]
async fn subscribing_while_a_follow_up_fails_never_yields_an_empty_stream() {
    let store = SlowGetStore::new();
    let handler = Arc::new(DefaultRequestHandler::new(
        ScriptAgent {
            release: Arc::new(Notify::new()),
            cancel: CancelMode::Status,
        },
        store.clone(),
    ));
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("ask", None))
            .await
            .unwrap(),
    );
    store.slow_next_get();
    let doomed = {
        let handler = handler.clone();
        let id = first.id.clone();
        let stale = first.generation - 1;
        tokio::spawn(async move {
            handler
                .send_message(
                    &ServiceParams::new(),
                    with_config(send("late", Some(&id)), |c| {
                        c.if_generation_match = Some(stale)
                    }),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(40)).await;

    let subscribed = handler
        .subscribe_to_task(
            &params,
            SubscribeToTaskRequest {
                id: first.id.clone(),
                tenant: None,
                last_seen_generation: None,
            },
        )
        .await;

    // Either the subscription is refused, or it opens with the task; a stream
    // that is simply empty is the one answer that is wrong.
    if let Ok(stream) = subscribed {
        assert!(matches!(
            first_event(stream).await,
            Some(Ok(StreamResponse::Task(_)))
        ));
    }
    assert_eq!(
        doomed.await.unwrap().unwrap_err().code,
        error_code::TASK_GENERATION_MISMATCH
    );
}

#[tokio::test]
async fn a_failing_cancel_still_records_the_state_change() {
    let (handler, release) = script_handler(CancelMode::Fail);
    let (task_id, _held) = open_held_task(&handler).await;
    let before = handler
        .get_task(&ServiceParams::new(), get(&task_id))
        .await
        .unwrap();

    let error = handler
        .cancel_task(&ServiceParams::new(), cancel(&task_id))
        .await
        .unwrap_err();

    assert_eq!(error.code, error_code::INTERNAL_ERROR);
    let after = handler
        .get_task(&ServiceParams::new(), get(&task_id))
        .await
        .unwrap();
    assert_eq!(after.status.state, TaskState::Canceled);
    assert_eq!(
        after.generation,
        before.generation + 1,
        "the change has a generation"
    );
    assert_eq!(canceled_entries(&after), 1, "{:?}", timeline_of(&after));
    release.notify_one();
}

#[tokio::test]
async fn no_two_events_share_a_generation_and_none_has_zero() {
    let (handler, _) = script_handler(CancelMode::Status);
    let mut stream = handler
        .send_streaming_message(&ServiceParams::new(), send("dup", None))
        .await
        .unwrap();

    let mut generations = Vec::new();
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            StreamResponse::StatusUpdate(update) => generations.push(update.generation),
            StreamResponse::ArtifactUpdate(update) => generations.push(update.generation),
            StreamResponse::Task(task) => generations.push(task.generation),
            _ => {}
        }
    }

    assert!(generations.iter().all(|g| *g > 0), "{generations:?}");
    assert!(
        generations.windows(2).all(|pair| pair[1] > pair[0]),
        "a restated snapshot must not repeat a generation: {generations:?}"
    );
}

#[tokio::test]
async fn an_executor_cannot_forge_a_client_message_event() {
    let (handler, _) = script_handler(CancelMode::Status);
    let mut stream = handler
        .send_streaming_message(&ServiceParams::new(), send("msgupdate", None))
        .await
        .unwrap();

    let mut forwarded = false;
    let mut error = None;
    while let Some(event) = stream.next().await {
        match event {
            Ok(StreamResponse::MessageUpdate(_)) => forwarded = true,
            Ok(_) => {}
            Err(e) => error = Some(e),
        }
    }

    assert!(!forwarded, "a forged event reached a subscriber");
    assert_eq!(
        error.expect("the execution is ended").code,
        error_code::INVALID_AGENT_RESPONSE
    );
}

#[tokio::test]
async fn a_snapshot_listing_only_the_agents_artifacts_keeps_the_clients() {
    let (handler, _) = script_handler(CancelMode::Status);
    let mut request = send("agent-artifacts", None);
    request.artifact_updates = vec![artifact_update("from client")];
    let mut stream = handler
        .send_streaming_message(&ServiceParams::new(), request)
        .await
        .unwrap();
    let mut task_id = None;
    while let Some(event) = stream.next().await {
        if let StreamResponse::Task(task) = event.unwrap() {
            task_id = Some(task.id);
        }
    }

    let stored = handler
        .get_task(&ServiceParams::new(), get(&task_id.unwrap()))
        .await
        .unwrap();

    let mut ids: Vec<_> = stored
        .artifacts
        .unwrap()
        .into_iter()
        .map(|a| a.artifact_id)
        .collect();
    ids.sort();
    assert_eq!(ids, ["agent-out", "from-client"]);
}

#[tokio::test]
async fn a_cancel_and_a_follow_up_on_an_idle_task_are_ordered() {
    let store = SlowGetStore::new();
    let handler = Arc::new(DefaultRequestHandler::new(
        ScriptAgent {
            release: Arc::new(Notify::new()),
            cancel: CancelMode::Status,
        },
        store.clone(),
    ));
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("ask", None))
            .await
            .unwrap(),
    );

    // The cancel holds the task while its read is slow...
    store.slow_next_get();
    let canceling = {
        let handler = handler.clone();
        let id = first.id.clone();
        tokio::spawn(async move {
            handler
                .cancel_task(&ServiceParams::new(), cancel(&id))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(40)).await;
    // ...so a follow-up arriving meanwhile has to wait and then finds the
    // task finished.
    let follow_up = handler
        .send_message(&params, send("too late", Some(&first.id)))
        .await;

    canceling.await.unwrap().unwrap();
    assert_eq!(
        follow_up.unwrap_err().code,
        error_code::UNSUPPORTED_OPERATION,
        "a terminal task is refused as unsupported, not as a store error"
    );
    let stored = handler.get_task(&params, get(&first.id)).await.unwrap();
    assert_eq!(stored.status.state, TaskState::Canceled);
    assert_eq!(canceled_entries(&stored), 1);
}

#[tokio::test]
async fn a_panicking_executor_fails_the_task_and_tells_the_subscribers() {
    let (handler, _) = script_handler(CancelMode::Status);
    let params = ServiceParams::new();
    let mut stream = handler
        .send_streaming_message(&params, send("panic", None))
        .await
        .unwrap();

    let mut task_id = None;
    let mut last_state = None;
    while let Some(event) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the stream ends")
    {
        match event.unwrap() {
            StreamResponse::StatusUpdate(update) => {
                task_id = Some(update.task_id);
                last_state = Some(update.status.state);
            }
            StreamResponse::Task(task) => task_id = Some(task.id),
            _ => {}
        }
    }

    assert_eq!(
        last_state,
        Some(TaskState::Failed),
        "the subscriber is told"
    );
    let stored = handler
        .get_task(&params, get(&task_id.unwrap()))
        .await
        .unwrap();
    assert_eq!(
        stored.status.state,
        TaskState::Failed,
        "and the task is not left working"
    );
}

// ---------------------------------------------------------------------------
// Regressions found in the third validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancel_whose_executor_misbehaves_still_records_the_state_change() {
    for mode in [
        CancelMode::ForgedMessage,
        CancelMode::Revive,
        CancelMode::Complete,
    ] {
        let (handler, release) = script_handler(mode);
        let (task_id, _held) = open_held_task(&handler).await;
        let before = handler
            .get_task(&ServiceParams::new(), get(&task_id))
            .await
            .unwrap();

        let error = handler
            .cancel_task(&ServiceParams::new(), cancel(&task_id))
            .await
            .unwrap_err();

        assert_eq!(error.code, error_code::INVALID_AGENT_RESPONSE);
        let after = handler
            .get_task(&ServiceParams::new(), get(&task_id))
            .await
            .unwrap();
        assert_eq!(
            after.status.state,
            TaskState::Canceled,
            "the task stays canceled"
        );
        assert_eq!(after.generation, before.generation + 1);
        assert_eq!(canceled_entries(&after), 1, "{:?}", timeline_of(&after));
        release.notify_one();
    }
}

#[tokio::test]
async fn a_stream_opens_with_the_servers_task_even_when_the_executor_starts_with_a_status() {
    let (handler, _) = script_handler(CancelMode::Status);
    let mut stream = handler
        .send_streaming_message(&ServiceParams::new(), send("dup", None))
        .await
        .unwrap();

    let first = stream.next().await.unwrap().unwrap();
    let StreamResponse::Task(opening) = first else {
        panic!("a streaming send opens with the task, got {first:?}");
    };
    assert_eq!(opening.generation, 1, "the task as the request left it");
    let mut generations = vec![opening.generation];
    while let Some(event) = stream.next().await {
        match event.unwrap() {
            StreamResponse::StatusUpdate(update) => generations.push(update.generation),
            StreamResponse::Task(task) => generations.push(task.generation),
            _ => {}
        }
    }
    assert!(
        generations.windows(2).all(|pair| pair[1] > pair[0]),
        "{generations:?}"
    );
}

#[tokio::test]
async fn a_message_only_answer_is_not_preceded_by_a_task() {
    struct MessageAgent;
    impl AgentExecutor for MessageAgent {
        fn execute(
            &self,
            _ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            Box::pin(stream::once(async {
                Ok(StreamResponse::Message(Message::new(
                    Role::Agent,
                    vec![Part::text("just a reply")],
                )))
            }))
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            Box::pin(stream::empty())
        }
    }
    let handler = DefaultRequestHandler::new(MessageAgent, InMemoryTaskStore::new());
    let events: Vec<_> = handler
        .send_streaming_message(&ServiceParams::new(), send("hi", None))
        .await
        .unwrap()
        .collect()
        .await;

    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(events[0], Ok(StreamResponse::Message(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscription_that_races_a_follow_up_always_opens_with_the_task() {
    let mut opened_with_task = 0;
    for round in 0..300 {
        let release = Arc::new(Notify::new());
        let handler = Arc::new(DefaultRequestHandler::new(
            ScriptAgent {
                release: release.clone(),
                cancel: CancelMode::Status,
            },
            InMemoryTaskStore::new(),
        ));
        let params = ServiceParams::new();
        let first = task_of(
            handler
                .send_message(&params, send("ask", None))
                .await
                .unwrap(),
        );

        // The follow-up is held, so the execution outlives the subscription.
        let follow_up = {
            let handler = handler.clone();
            let id = first.id.clone();
            tokio::spawn(async move {
                handler
                    .send_message(&ServiceParams::new(), send("hold", Some(&id)))
                    .await
            })
        };
        // Subscribe in a tight loop until the execution is there: the attempts
        // land on every moment of its registration and preparation.
        for _ in 0..2000 {
            let subscribed = handler
                .subscribe_to_task(
                    &params,
                    SubscribeToTaskRequest {
                        id: first.id.clone(),
                        tenant: None,
                        last_seen_generation: None,
                    },
                )
                .await;
            if let Ok(stream) = subscribed {
                let opened = first_event(stream).await;
                assert!(
                    matches!(opened, Some(Ok(StreamResponse::Task(_)))),
                    "round {round}: a subscription opened with {opened:?}"
                );
                opened_with_task += 1;
                break;
            }
            tokio::task::yield_now().await;
        }
        release.notify_one();
        follow_up.await.unwrap().unwrap();
    }
    // A test that never got a subscription proves nothing.
    assert!(
        opened_with_task > 100,
        "only {opened_with_task} rounds subscribed"
    );
}

#[tokio::test]
async fn a_streaming_send_that_only_restates_the_task_still_opens_with_it() {
    let (handler, _) = script_handler(CancelMode::Status);

    let events: Vec<_> = handler
        .send_streaming_message(&ServiceParams::new(), send("restate", None))
        .await
        .unwrap()
        .collect()
        .await;

    assert_eq!(events.len(), 1, "{events:?}");
    assert!(
        matches!(events[0], Ok(StreamResponse::Task(_))),
        "{events:?}"
    );
}

#[tokio::test]
async fn restating_the_agents_artifacts_keeps_the_stored_order_and_is_no_change() {
    use a2a_server::TaskStore;
    let store = InMemoryTaskStore::new();
    store
        .create(Task {
            id: "t-order".into(),
            context_id: "c-order".into(),
            status: TaskStatus {
                state: TaskState::InputRequired,
                message: None,
                timestamp: None,
            },
            artifacts: Some(vec![
                Artifact {
                    role: Some(Role::User),
                    start_generation: Some(2),
                    ..blank_artifact("from-client", "mine")
                },
                Artifact {
                    start_generation: Some(3),
                    ..blank_artifact("agent-out", "result")
                },
            ]),
            history: None,
            timeline: None,
            metadata: None,
            generation: 3,
        })
        .await
        .unwrap();
    let handler = DefaultRequestHandler::new(
        ScriptAgent {
            release: Arc::new(Notify::new()),
            cancel: CancelMode::Status,
        },
        store,
    );

    let task = task_of(
        handler
            .send_message(
                &ServiceParams::new(),
                send("agent-artifacts", Some("t-order")),
            )
            .await
            .unwrap(),
    );

    // Only the client's message advanced the generation; the snapshot that
    // restated the agent's artifact changed nothing.
    assert_eq!(task.generation, 4);
    let ids: Vec<_> = task
        .artifacts
        .unwrap()
        .into_iter()
        .map(|a| a.artifact_id)
        .collect();
    assert_eq!(ids, ["from-client", "agent-out"]);
}

#[test]
fn an_executor_can_be_exercised_with_a_hand_built_context() {
    let ctx = ExecutorContext::new("t-direct", "c-direct")
        .with_message(Message::new(Role::User, vec![Part::text("snapshot")]))
        .with_tenant("acme");
    assert_eq!(
        ctx.task_info(),
        ("t-direct".to_string(), "c-direct".to_string())
    );
    assert_eq!(ctx.tenant.as_deref(), Some("acme"));

    let agent = ScriptAgent {
        release: Arc::new(Notify::new()),
        cancel: CancelMode::Status,
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let first = runtime.block_on(agent.execute(ctx).next());
    assert!(matches!(first, Some(Ok(StreamResponse::Task(_)))));
}

#[tokio::test]
async fn an_executor_replacing_a_client_artifact_keeps_it_the_clients() {
    let (handler, _) = script_handler(CancelMode::Status);
    let params = ServiceParams::new();
    let mut request = send("overwrite-user", None);
    request.artifact_updates = vec![artifact_update("mine")];

    let task = task_of(handler.send_message(&params, request).await.unwrap());

    let artifact = task
        .artifacts
        .unwrap()
        .into_iter()
        .find(|a| a.artifact_id == "from-client")
        .unwrap();
    assert_eq!(artifact.role, Some(Role::User), "who wrote it is not lost");
    assert_eq!(artifact.parts[0].as_text(), Some("replaced"));
}

// ---------------------------------------------------------------------------
// Cancellation is one atomic, generation-bumped transition
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancel_is_visible_at_its_own_generation_from_the_start() {
    let gate = Arc::new(Notify::new());
    let (handler, release) = script_handler(CancelMode::Stall(gate.clone()));
    let handler = Arc::new(handler);
    let (task_id, mut held) = open_held_task(&handler).await;
    let before = handler
        .get_task(&ServiceParams::new(), get(&task_id))
        .await
        .unwrap();

    let canceling = {
        let handler = handler.clone();
        let id = task_id.clone();
        tokio::spawn(async move {
            handler
                .cancel_task(&ServiceParams::new(), cancel(&id))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(60)).await;

    // The executor is still stalled, yet nobody can see the task canceled at
    // the generation it had while it was working.
    let during = handler
        .get_task(&ServiceParams::new(), get(&task_id))
        .await
        .unwrap();
    assert_eq!(during.status.state, TaskState::Canceled);
    assert_eq!(during.generation, before.generation + 1);
    assert_eq!(canceled_entries(&during), 1);

    gate.notify_one();
    let canceled = canceling.await.unwrap().unwrap();
    assert_eq!(
        canceled.generation, during.generation,
        "the executor's status is the same entry, not another mutation"
    );
    assert_eq!(
        canceled_entries(&canceled),
        1,
        "{:?}",
        timeline_of(&canceled)
    );
    let last = timeline_of(&canceled).last().unwrap();
    assert!(matches!(
        &last.content,
        TimelineContent::Status(status)
            if status.message.as_ref().and_then(|m| m.text()) == Some("canceled")
    ));

    // The subscriber is told once, at that generation.
    let mut announced = Vec::new();
    while let Some(event) = held.next().await {
        if let StreamResponse::StatusUpdate(update) = event.unwrap() {
            if update.status.state == TaskState::Canceled {
                announced.push(update.generation);
            }
        }
    }
    assert_eq!(announced, [canceled.generation]);
    release.notify_one();
}

/// A store that predates the generation-bumping `begin_cancel`.
#[derive(Clone)]
struct PlainCancelStore {
    inner: Arc<InMemoryTaskStore>,
}

#[async_trait::async_trait]
impl a2a_server::TaskStore for PlainCancelStore {
    async fn create(&self, task: Task) -> Result<a2a_server::task_store::TaskVersion, A2AError> {
        self.inner.create(task).await
    }
    async fn update(&self, task: Task) -> Result<a2a_server::task_store::TaskVersion, A2AError> {
        self.inner.update(task).await
    }
    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        self.inner.get(task_id).await
    }
    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.inner.list(req).await
    }
    async fn begin_cancel(&self, task_id: &str) -> Result<Task, A2AError> {
        // Moves the state and nothing else, as stores did before 1.1.
        let before = self.inner.get(task_id).await?;
        let mut task = self.inner.begin_cancel(task_id).await?;
        if let Some(before) = before {
            task.generation = before.generation;
            task.timeline = before.timeline;
        }
        self.inner.update(task.clone()).await?;
        Ok(task)
    }
}

#[tokio::test]
async fn a_store_that_does_not_bump_on_cancel_is_recorded_by_the_handler() {
    let release = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        ScriptAgent {
            release: release.clone(),
            cancel: CancelMode::Status,
        },
        PlainCancelStore {
            inner: Arc::new(InMemoryTaskStore::new()),
        },
    );
    let (task_id, _held) = open_held_task(&handler).await;
    let before = handler
        .get_task(&ServiceParams::new(), get(&task_id))
        .await
        .unwrap();

    let canceled = handler
        .cancel_task(&ServiceParams::new(), cancel(&task_id))
        .await
        .unwrap();

    assert_eq!(canceled.status.state, TaskState::Canceled);
    assert_eq!(canceled.generation, before.generation + 1);
    assert_eq!(
        canceled_entries(&canceled),
        1,
        "{:?}",
        timeline_of(&canceled)
    );
    release.notify_one();
}

// ---------------------------------------------------------------------------
// Cancel regressions found in the sixth validation
// ---------------------------------------------------------------------------

/// Every generation announced for a task, in order, from a stream.
async fn announced_generations(
    mut stream: BoxStream<'static, Result<StreamResponse, A2AError>>,
) -> Vec<i64> {
    let mut generations = Vec::new();
    while let Some(event) = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the stream ends")
    {
        match event {
            Ok(StreamResponse::StatusUpdate(update)) => generations.push(update.generation),
            Ok(StreamResponse::ArtifactUpdate(update)) => generations.push(update.generation),
            _ => {}
        }
    }
    generations
}

#[tokio::test]
async fn a_cancel_that_only_produces_an_artifact_never_repeats_a_generation() {
    let (handler, release) = script_handler(CancelMode::ArtifactOnly);
    let (task_id, held) = open_held_task(&handler).await;
    let collecting = tokio::spawn(announced_generations(held));

    let canceled = handler
        .cancel_task(&ServiceParams::new(), cancel(&task_id))
        .await
        .unwrap();
    release.notify_one();

    let generations = collecting.await.unwrap();
    assert!(
        generations.windows(2).all(|pair| pair[1] > pair[0]),
        "{generations:?}"
    );
    assert_eq!(generations.last().copied(), Some(canceled.generation));
    assert_eq!(canceled.status.state, TaskState::Canceled);
    assert!(canceled.artifacts.is_some(), "the artifact was kept");
}

#[tokio::test]
async fn an_idle_cancel_that_only_produces_an_artifact_never_repeats_a_generation() {
    let (handler, _) = script_handler(CancelMode::ArtifactOnly);
    let params = ServiceParams::new();
    let first = task_of(
        handler
            .send_message(&params, send("ask", None))
            .await
            .unwrap(),
    );

    let canceled = handler
        .cancel_task(&params, cancel(&first.id))
        .await
        .unwrap();

    // Store entry, artifact, then the cancellation recorded again: each
    // mutation has a generation of its own.
    let generations: Vec<_> = timeline_of(&canceled)
        .iter()
        .map(|e| e.generation)
        .collect();
    assert!(
        generations.windows(2).all(|pair| pair[1] > pair[0]),
        "{generations:?}"
    );
    assert_eq!(canceled.generation, *generations.last().unwrap());
    assert_eq!(canceled.status.state, TaskState::Canceled);
}

#[tokio::test]
async fn a_long_poll_wakes_when_the_task_is_canceled_not_on_the_next_recheck() {
    let gate = Arc::new(Notify::new());
    let (handler, release) = script_handler(CancelMode::Stall(gate.clone()));
    let handler = Arc::new(handler);
    let (task_id, _held) = open_held_task(&handler).await;
    let before = handler
        .get_task(&ServiceParams::new(), get(&task_id))
        .await
        .unwrap();

    let mut request = get(&task_id);
    request.current_generation = Some(before.generation);
    let polling = {
        let handler = handler.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            let task = handler.get_task(&ServiceParams::new(), request).await;
            (task, started.elapsed())
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let canceling = {
        let handler = handler.clone();
        let id = task_id.clone();
        tokio::spawn(async move {
            handler
                .cancel_task(&ServiceParams::new(), cancel(&id))
                .await
        })
    };

    let (task, waited) = polling.await.unwrap();
    assert_eq!(task.unwrap().generation, before.generation + 1);
    // The cancel executor is still stalled; the poll was woken by the
    // transition itself, well inside the one-second recheck.
    assert!(waited < Duration::from_millis(700), "waited {waited:?}");
    gate.notify_one();
    canceling.await.unwrap().unwrap();
    release.notify_one();
}

#[tokio::test]
async fn a_cancel_without_a_status_does_not_keep_what_the_agent_said_while_working() {
    let (handler, release) = script_handler(CancelMode::Silent);
    let (task_id, _held) = open_held_task(&handler).await;

    let canceled = handler
        .cancel_task(&ServiceParams::new(), cancel(&task_id))
        .await
        .unwrap();

    assert!(
        canceled.status.message.is_none(),
        "{:?}",
        canceled.status.message
    );
    let last = timeline_of(&canceled).last().unwrap();
    assert!(matches!(
        &last.content,
        TimelineContent::Status(status) if status.message.is_none()
    ));
    release.notify_one();
}
