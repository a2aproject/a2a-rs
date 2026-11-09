// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Optional typed-agent cleanup must not fail an already-committed cancellation.

use a2a::*;
use a2a_server::*;
use futures::{StreamExt, stream, stream::BoxStream};
use std::{sync::Arc, time::Duration};
use tokio::{sync::Notify, time::timeout};

struct RunningAgent {
    started: Arc<Notify>,
    dropped: Arc<Notify>,
}

struct Dropped(Arc<Notify>);

impl Drop for Dropped {
    fn drop(&mut self) {
        self.0.notify_one();
    }
}

#[async_trait::async_trait]
impl Agent for RunningAgent {
    async fn start(&self, message: NewMessage) -> Result<Start, A2AError> {
        Ok(Start::Task(message.new_task(TaskState::Working)))
    }

    fn execute(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let started = self.started.clone();
        let dropped = self.dropped.clone();
        stream::once(async move {
            let _dropped = Dropped(dropped);
            started.notify_one();
            futures::future::pending().await
        })
        .boxed()
    }
    // The default cancellation hook needs no additional cleanup.
}

fn cancel_request(id: &str) -> CancelTaskRequest {
    CancelTaskRequest {
        id: id.into(),
        metadata: None,
        tenant: None,
    }
}

async fn get(handler: &DefaultRequestHandler, id: &str) -> Task {
    handler
        .get_task(
            &ServiceParams::new(),
            GetTaskRequest {
                id: id.into(),
                history_length: None,
                timeline_length: None,
                current_generation: None,
                tenant: None,
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn default_cleanup_cancels_active_typed_execution_and_notifies_subscribers_once() {
    let started = Arc::new(Notify::new());
    let dropped = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::from_agent(
        RunningAgent {
            started: started.clone(),
            dropped: dropped.clone(),
        },
        InMemoryTaskStore::new(),
    )
    .with_capabilities(AgentCapabilities {
        streaming: Some(true),
        ..Default::default()
    });
    let request = SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text("work")]),
        configuration: Some(
            serde_json::from_value(serde_json::json!({"returnImmediately":true})).unwrap(),
        ),
        metadata: None,
        tenant: None,
        artifact_updates: vec![],
    };
    let response = handler
        .send_message(&ServiceParams::new(), request)
        .await
        .unwrap();
    let SendMessageResponse::Task(task) = response else {
        panic!("expected a task")
    };
    timeout(Duration::from_secs(2), started.notified())
        .await
        .expect("execution started");
    let before = get(&handler, &task.id).await;
    assert_eq!(before.status.state, TaskState::Working);
    let mut events = handler
        .subscribe_to_task(
            &ServiceParams::new(),
            SubscribeToTaskRequest {
                id: task.id.clone(),
                tenant: None,
                last_seen_generation: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(events.next().await, Some(Ok(StreamResponse::Task(task))) if task == before));

    let canceled = timeout(
        Duration::from_secs(2),
        handler.cancel_task(&ServiceParams::new(), cancel_request(&task.id)),
    )
    .await
    .expect("cancellation completed")
    .expect("optional cleanup must succeed");
    assert_eq!(canceled.status.state, TaskState::Canceled);
    assert!(canceled.generation > before.generation);
    assert_eq!(get(&handler, &task.id).await, canceled);
    timeout(Duration::from_secs(2), dropped.notified())
        .await
        .expect("the execution stream was dropped");

    let notifications = timeout(Duration::from_secs(2), events.collect::<Vec<_>>())
        .await
        .unwrap();
    assert_eq!(
        notifications.len(),
        1,
        "one terminal notification closes the subscription"
    );
    assert!(
        matches!(&notifications[0], Ok(StreamResponse::StatusUpdate(update))
        if update.status.state == TaskState::Canceled && update.generation == canceled.generation)
    );
    let again = handler
        .cancel_task(&ServiceParams::new(), cancel_request(&task.id))
        .await
        .unwrap();
    assert_eq!(again, canceled, "repeated cancellation is idempotent");
    handler.shutdown().await;
}

struct OneShot;

#[async_trait::async_trait]
impl Agent for OneShot {
    async fn start(&self, _: NewMessage) -> Result<Start, A2AError> {
        Ok(Start::Message(Message::new(
            Role::Agent,
            vec![Part::text("reply")],
        )))
    }
}

async fn stored_task(state: TaskState) -> InMemoryTaskStore {
    let store = InMemoryTaskStore::new();
    store
        .create(Task {
            id: "task".into(),
            context_id: "context".into(),
            generation: 1,
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            timeline: None,
            metadata: None,
        })
        .await
        .unwrap();
    store
}

#[tokio::test]
async fn default_cleanup_also_cancels_stored_tasks_without_an_active_execution() {
    let handler =
        DefaultRequestHandler::from_agent(OneShot, stored_task(TaskState::InputRequired).await);
    let canceled = handler
        .cancel_task(&ServiceParams::new(), cancel_request("task"))
        .await
        .expect("a stored task does not require an agent cleanup implementation");
    assert_eq!(canceled.status.state, TaskState::Canceled);
    assert_eq!(get(&handler, "task").await, canceled);
    let missing = handler
        .cancel_task(&ServiceParams::new(), cancel_request("missing"))
        .await
        .unwrap_err();
    assert_eq!(missing.code, error_code::TASK_NOT_FOUND);
    handler.shutdown().await;
}

struct FailedCleanup;

#[async_trait::async_trait]
impl Agent for FailedCleanup {
    async fn start(&self, _: NewMessage) -> Result<Start, A2AError> {
        unreachable!("only an existing task is canceled")
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        assert_eq!(ctx.stored_task.unwrap().status.state, TaskState::Canceled);
        stream::once(async { Err(A2AError::internal("cleanup failed")) }).boxed()
    }
}

#[tokio::test]
async fn a_custom_cleanup_error_is_reported_and_cannot_undo_cancellation() {
    let handler =
        DefaultRequestHandler::from_agent(FailedCleanup, stored_task(TaskState::Working).await);
    let error = handler
        .cancel_task(&ServiceParams::new(), cancel_request("task"))
        .await
        .unwrap_err();
    assert_eq!(error.code, error_code::INTERNAL_ERROR);
    assert_eq!(error.message, "cleanup failed");
    assert_eq!(
        get(&handler, "task").await.status.state,
        TaskState::Canceled
    );
    handler.shutdown().await;
}
