// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Interrupted snapshots cannot stand in for output from a new executor turn.

use super::protocol::{ResponsePolicy, StartMode};
use super::response::{Run, unary_response};
use super::test_support::*;
use crate::AgentExecutor;

fn answer(text: &str) -> SendMessageRequest {
    let mut request = send_request();
    request.message = Some(Message::new(Role::User, vec![Part::text(text)]));
    request.message.as_mut().unwrap().task_id = Some("task".into());
    request
}

async fn old_question(handler: &DefaultRequestHandler) -> Task {
    seed_task(handler, "task", "context").await;
    let mut task = handler.tasks.load("task").await.unwrap();
    task.status.state = TaskState::InputRequired;
    task.status.message = Some(Message::new(Role::Agent, vec![Part::text("old question")]));
    handler.tasks.store().update(task).await.unwrap();
    handler.tasks.load("task").await.unwrap()
}

async fn input_is_stored(handler: &DefaultRequestHandler, text: &str) {
    timeout(Duration::from_secs(2), async {
        loop {
            let task = handler.tasks.load("task").await.unwrap();
            if task
                .history
                .iter()
                .flatten()
                .any(|message| message.text() == Some(text))
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("answer was admitted");
}

async fn cancel(handler: &DefaultRequestHandler) {
    handler
        .cancel_task(
            &ServiceParams::new(),
            CancelTaskRequest {
                id: "task".into(),
                tenant: None,
                metadata: None,
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn post_turn_reload_does_not_return_a_stale_interrupted_snapshot() {
    for state in [TaskState::InputRequired, TaskState::AuthRequired] {
        for newer_generation in [false, true] {
            check_reloaded_question(state.clone(), newer_generation).await;
        }
    }
}

async fn check_reloaded_question(state: TaskState, newer_generation: bool) {
    let handler = make_handler();
    let mut task = old_question(&handler).await;
    task.status.state = state;
    // An appended answer can advance generation without changing the old status.
    task.generation = 2;
    handler.tasks.store().update(task.clone()).await.unwrap();
    let handle = handler.tasks.task(&task.id);
    let events = Run::new(
        handler.clone(),
        ServiceParams::new(),
        handle.clone(),
        ResponsePolicy::new(StartMode::Blocking, false),
    )
    .start(
        None,
        Some(stream::empty().boxed()),
        task.generation - i64::from(newer_generation),
    );
    let mut response = Box::pin(unary_response(handler.tasks.clone(), handle, events));
    assert!(
        timeout(Duration::from_millis(30), &mut response)
            .await
            .is_err(),
        "reloading the same question after a turn ends cannot complete a new answer"
    );
    cancel(&handler).await;
    let result = timeout(Duration::from_secs(2), response)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(result, SendMessageResponse::Task(task) if task.status.state == TaskState::Canceled)
    );
    handler.shutdown().await;
}

struct Empty;

struct ReadAndEnd;

impl AgentExecutor for ReadAndEnd {
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
    fn execute(
        &self,
        mut ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(async_stream::stream! {
            yield Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: ctx.task_id.clone(), context_id: ctx.context_id.clone(),
                generation: 0, metadata: None,
                status: TaskStatus { state: TaskState::Working, message: None, timestamp: None },
            }));
            let _ = ctx.inputs.recv().await;
        })
    }
}

#[tokio::test]
async fn a_turn_that_reads_an_answer_then_ends_returns_the_working_snapshot() {
    let handler = DefaultRequestHandler::new(ReadAndEnd, InMemoryTaskStore::new())
        .with_capabilities(caps(Some(true), None));
    old_question(&handler).await;
    let mut events = handler
        .send_streaming_message(&ServiceParams::new(), answer("opening"))
        .await
        .unwrap();
    timeout(Duration::from_secs(2), async {
        while let Some(event) = events.next().await {
            if matches!(event.unwrap(), StreamResponse::StatusUpdate(update) if update.status.state == TaskState::Working) {
                break;
            }
        }
    }).await.unwrap();
    let result = timeout(
        Duration::from_secs(2),
        handler.send_message(&ServiceParams::new(), answer("answer")),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        matches!(result, SendMessageResponse::Task(task) if task.status.state == TaskState::Working)
    );
    assert!(
        timeout(Duration::from_millis(30), events.next())
            .await
            .is_err(),
        "streaming follows the nonterminal task"
    );
    handler.shutdown().await;
}

impl AgentExecutor for Empty {
    fn execute(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

#[tokio::test]
async fn an_answering_turn_without_output_returns_its_current_snapshot() {
    let handler = DefaultRequestHandler::new(Empty, InMemoryTaskStore::new());
    old_question(&handler).await;
    let response = tokio::spawn({
        let handler = handler.clone();
        async move {
            handler
                .send_message(&ServiceParams::new(), answer("answer"))
                .await
        }
    });
    input_is_stored(&handler, "answer").await;
    timeout(Duration::from_secs(2), async {
        while handler.tasks.background_tasks.count() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the empty turn finished");
    let result = timeout(Duration::from_secs(2), response)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(result, SendMessageResponse::Task(task) if task.status.state == TaskState::InputRequired)
    );
    handler.shutdown().await;
}

struct Handoff {
    finish_first: Arc<Notify>,
    release_successor: Arc<Notify>,
    started: mpsc::UnboundedSender<(String, String)>,
}

impl AgentExecutor for Handoff {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let message = ctx
            .message
            .as_ref()
            .and_then(Message::text)
            .unwrap()
            .to_owned();
        self.started
            .send((ctx.execution_id.clone(), message.clone()))
            .unwrap();
        if message == "first" {
            let finish = self.finish_first.clone();
            return stream::once(async move {
                finish.notified().await;
                None
            })
            .filter_map(futures::future::ready)
            .boxed();
        }
        let release = self.release_successor.clone();
        stream::once(async move {
            release.notified().await;
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: ctx.task_id,
                context_id: ctx.context_id,
                generation: 0,
                metadata: None,
                status: TaskStatus {
                    state: TaskState::InputRequired,
                    timestamp: None,
                    message: Some(Message::new(Role::Agent, vec![Part::text("new question")])),
                },
            }))
        })
        .chain(stream::pending())
        .boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

#[tokio::test]
async fn a_queued_answer_follows_the_successor_until_it_emits_its_own_question() {
    let finish_first = Arc::new(Notify::new());
    let release_successor = Arc::new(Notify::new());
    let (started, mut invocations) = mpsc::unbounded_channel();
    let handler = DefaultRequestHandler::new(
        Handoff {
            finish_first: finish_first.clone(),
            release_successor: release_successor.clone(),
            started,
        },
        InMemoryTaskStore::new(),
    );
    old_question(&handler).await;
    let first = tokio::spawn({
        let handler = handler.clone();
        async move {
            handler
                .send_message(&ServiceParams::new(), answer("first"))
                .await
        }
    });
    let (first_id, _) = timeout(Duration::from_secs(2), invocations.recv())
        .await
        .unwrap()
        .unwrap();
    let mut response = tokio::spawn({
        let handler = handler.clone();
        async move {
            handler
                .send_message(&ServiceParams::new(), answer("answer"))
                .await
        }
    });
    input_is_stored(&handler, "answer").await;
    finish_first.notify_one();
    let (next_id, next_message) = timeout(Duration::from_secs(2), invocations.recv())
        .await
        .unwrap()
        .unwrap();
    assert_ne!(
        first_id, next_id,
        "the answer was handed to a new execution"
    );
    assert_eq!(next_message, "answer");
    assert!(
        timeout(Duration::from_millis(30), &mut response)
            .await
            .is_err(),
        "the previous execution ending cannot return its old question"
    );
    release_successor.notify_one();
    let result = timeout(Duration::from_secs(2), response)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let SendMessageResponse::Task(task) = result else {
        panic!("task response")
    };
    assert_eq!(
        task.status.message.as_ref().and_then(Message::text),
        Some("new question")
    );
    timeout(Duration::from_secs(2), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    handler.shutdown().await;
}
