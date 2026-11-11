// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Streaming joins open from committed input, independently of executor output.

use a2a::*;
use a2a_server::*;
use futures::{StreamExt, stream, stream::BoxStream};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::Notify, time::timeout};

const WAIT: Duration = Duration::from_secs(1);

struct Gates {
    calls: AtomicUsize,
    finish: Notify,
    successor: Notify,
    finish_successor: Notify,
}

struct Gated {
    gates: Arc<Gates>,
    handoff: bool,
}

fn status(ctx: &ExecutorContext, state: TaskState) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        generation: 0,
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        metadata: None,
    })
}

impl AgentExecutor for Gated {
    fn execute(
        &self,
        mut ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        self.gates.calls.fetch_add(1, Ordering::SeqCst);
        let gates = self.gates.clone();
        let handoff = self.handoff;
        Box::pin(async_stream::stream! {
            if ctx.message.as_ref().and_then(Message::text) == Some("start") {
                yield Ok(status(&ctx, TaskState::Working));
                gates.finish.notified().await;
                if handoff {return;}
                let input = ctx.inputs.recv().await.expect("the accepted join input");
                assert_eq!(input.message.text(), Some("join"));
            } else {
                gates.successor.notify_one();
                gates.finish_successor.notified().await;
            }
            yield Ok(status(&ctx, TaskState::Completed));
        })
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

fn handler(handoff: bool) -> (DefaultRequestHandler, Arc<Gates>) {
    let gates = Arc::new(Gates {
        calls: AtomicUsize::new(0),
        finish: Notify::new(),
        successor: Notify::new(),
        finish_successor: Notify::new(),
    });
    let handler = DefaultRequestHandler::new(
        Gated {
            gates: gates.clone(),
            handoff,
        },
        InMemoryTaskStore::new(),
    )
    .with_capabilities(AgentCapabilities {
        streaming: Some(true),
        user_artifacts: Some(true),
        ..Default::default()
    });
    (handler, gates)
}

fn params() -> ServiceParams {
    ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])])
}

fn request(text: &str, task: Option<&Task>) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task.map(|task| task.id.clone());
    SendMessageRequest {
        message: Some(message),
        configuration: None,
        metadata: None,
        tenant: None,
        artifact_updates: vec![],
    }
}

async fn next(events: &mut BoxStream<'static, Result<StreamResponse, A2AError>>) -> StreamResponse {
    timeout(WAIT, events.next())
        .await
        .expect("response progress without waiting for the agent")
        .expect("the response is open")
        .unwrap()
}

async fn get(handler: &DefaultRequestHandler, id: &str) -> Task {
    handler
        .get_task(
            &params(),
            GetTaskRequest {
                id: id.into(),
                tenant: None,
                history_length: None,
                timeline_length: None,
                current_generation: None,
            },
        )
        .await
        .unwrap()
}

async fn owner(
    handler: &DefaultRequestHandler,
) -> (Task, BoxStream<'static, Result<StreamResponse, A2AError>>) {
    let mut events = handler
        .send_streaming_message(&params(), request("start", None))
        .await
        .unwrap();
    let StreamResponse::Task(task) = next(&mut events).await else {
        panic!("opening task")
    };
    assert!(
        matches!(next(&mut events).await, StreamResponse::StatusUpdate(update) if update.status.state == TaskState::Working)
    );
    (get(handler, &task.id).await, events)
}

async fn check_join(with_artifact: bool) {
    let (handler, gates) = handler(false);
    let (before, owner) = owner(&handler).await;
    let mut input = request("join", Some(&before));
    let message_id = input.message.as_ref().unwrap().message_id.clone();
    if with_artifact {
        input.configuration = Some(
            serde_json::from_value(serde_json::json!({"historyLength":1,"timelineLength":1}))
                .unwrap(),
        );
        input.artifact_updates.push(TaskArtifactUpdateEvent {
            task_id: before.id.clone(),
            context_id: before.context_id.clone(),
            generation: 0,
            metadata: None,
            artifact: serde_json::from_value(
                serde_json::json!({"artifactId":"input","parts":[{"text":"work item"}]}),
            )
            .unwrap(),
            append: None,
            last_chunk: Some(true),
        });
    }
    let mut events = handler
        .send_streaming_message(&params(), input)
        .await
        .unwrap();
    let StreamResponse::Task(opening) = next(&mut events).await else {
        panic!("a join must open with a task")
    };
    assert_eq!(opening.id, before.id);
    assert!(opening.generation > before.generation);
    assert_eq!(opening.status.state, TaskState::Working);
    assert_eq!(
        opening
            .history
            .iter()
            .flatten()
            .filter(|m| m.message_id == message_id)
            .count(),
        1
    );
    assert!(opening.timeline.iter().flatten().any(
        |entry| matches!(&entry.content, TimelineContent::Message(m) if m.message_id == message_id)
    ));
    if with_artifact {
        assert_eq!(opening.history.as_ref().unwrap().len(), 1);
        assert_eq!(opening.timeline.as_ref().unwrap().len(), 1);
        let artifact = &opening.artifacts.as_ref().unwrap()[0];
        assert_eq!(artifact.parts, vec![Part::text("work item")]);
        assert_eq!(artifact.role, Some(Role::User));
        assert_eq!(opening.generation, before.generation + 2);
    } else {
        assert_eq!(opening, get(&handler, &before.id).await);
    }
    assert_eq!(
        gates.calls.load(Ordering::SeqCst),
        1,
        "a join reuses the running executor"
    );
    assert!(
        timeout(Duration::from_millis(1), events.next())
            .await
            .is_err(),
        "input already present in the opening is not replayed"
    );
    gates.finish.notify_one();
    let tail = timeout(WAIT, events.collect::<Vec<_>>()).await.unwrap();
    assert_eq!(tail.len(), 1);
    assert!(
        matches!(&tail[0], Ok(StreamResponse::StatusUpdate(update)) if update.status.state == TaskState::Completed)
    );
    timeout(WAIT, owner.collect::<Vec<_>>()).await.unwrap();
    handler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_busy_streaming_join_opens_with_the_task_after_its_input_is_committed() {
    check_join(false).await;
}

#[tokio::test(start_paused = true)]
async fn the_opening_includes_client_artifacts_and_applies_response_lengths() {
    check_join(true).await;
}

#[tokio::test(start_paused = true)]
async fn a_streaming_join_follows_a_silent_handoff_without_repeating_its_opening() {
    let (handler, gates) = handler(true);
    let (task, owner) = owner(&handler).await;
    let mut events = handler
        .send_streaming_message(&params(), request("join", Some(&task)))
        .await
        .unwrap();
    assert!(matches!(next(&mut events).await, StreamResponse::Task(_)));
    gates.finish.notify_one();
    timeout(WAIT, gates.successor.notified())
        .await
        .expect("unread input starts a successor");
    assert_eq!(gates.calls.load(Ordering::SeqCst), 2);
    assert!(
        timeout(Duration::from_millis(1), events.next())
            .await
            .is_err(),
        "the old invocation ending does not repeat the opening snapshot"
    );
    gates.finish_successor.notify_one();
    assert!(
        matches!(next(&mut events).await, StreamResponse::StatusUpdate(update) if update.status.state == TaskState::Completed)
    );
    assert!(timeout(WAIT, events.next()).await.unwrap().is_none());
    timeout(WAIT, owner.collect::<Vec<_>>()).await.unwrap();
    handler.shutdown().await;
}
