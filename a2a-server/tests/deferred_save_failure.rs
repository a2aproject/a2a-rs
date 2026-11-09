// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Deferred task creation failures are responses, not provisional Task snapshots.

use a2a::*;
use a2a_server::{task_store::TaskVersion, *};
use futures::{StreamExt, stream, stream::BoxStream};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{sync::mpsc, time::timeout};

const WAIT: Duration = Duration::from_secs(2);
const SAVE_ERROR: &str = "deferred task creation failed";

#[derive(Clone, Copy)]
enum Failure {
    BeforeWrite,
    AfterWrite,
}
#[derive(Clone, Copy)]
enum Mode {
    Unary,
    Streaming,
}

#[derive(Clone)]
struct FailingStore {
    inner: Arc<InMemoryTaskStore>,
    creates: Arc<AtomicUsize>,
    updates: Arc<AtomicUsize>,
    failure: Failure,
}

#[async_trait::async_trait]
impl TaskStore for FailingStore {
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        if matches!(self.failure, Failure::AfterWrite) {
            self.inner.create(task).await?;
        }
        Err(A2AError::internal(SAVE_ERROR))
    }
    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.updates.fetch_add(1, Ordering::SeqCst);
        self.inner.update(task).await
    }
    async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
        self.inner.get(id).await
    }
    async fn list(&self, request: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.inner.list(request).await
    }
    async fn begin_cancel(&self, id: &str) -> Result<Task, A2AError> {
        self.inner.begin_cancel(id).await
    }
}

struct Executor {
    contexts: mpsc::UnboundedSender<(TaskId, ClientInputs)>,
    calls: Arc<AtomicUsize>,
    reply: bool,
}

impl AgentExecutor for Executor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.contexts.send((ctx.task_id, ctx.inputs)).unwrap();
        if self.reply {
            stream::once(async {
                Ok(StreamResponse::Message(Message::new(
                    Role::Agent,
                    vec![Part::text("reply")],
                )))
            })
            .boxed()
        } else {
            stream::empty().boxed()
        }
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

fn params() -> ServiceParams {
    ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])])
}
fn request() -> SendMessageRequest {
    SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text("work")]),
        configuration: None,
        metadata: None,
        tenant: None,
        artifact_updates: vec![],
    }
}
async fn get(handler: &DefaultRequestHandler, id: &str) -> Result<Task, A2AError> {
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
}

async fn check(mode: Mode, failure: Failure, reply: bool) {
    let store = FailingStore {
        inner: Arc::new(InMemoryTaskStore::new()),
        creates: Arc::new(AtomicUsize::new(0)),
        updates: Arc::new(AtomicUsize::new(0)),
        failure,
    };
    let (contexts, mut observed) = mpsc::unbounded_channel();
    let calls = Arc::new(AtomicUsize::new(0));
    let handler = DefaultRequestHandler::new(
        Executor {
            contexts,
            calls: calls.clone(),
            reply,
        },
        store.clone(),
    )
    .with_capabilities(AgentCapabilities {
        streaming: Some(true),
        ..Default::default()
    });
    match mode {
        Mode::Unary => {
            let response = timeout(WAIT, handler.send_message(&params(), request()))
                .await
                .expect("deferred failure must finish the blocking response");
            if reply {
                assert!(
                    matches!(response, Ok(SendMessageResponse::Message(message)) if message.text() == Some("reply"))
                );
            } else {
                let error = response.unwrap_err();
                assert_eq!(error.code, error_code::INTERNAL_ERROR);
                assert_eq!(error.message, SAVE_ERROR);
            }
        }
        Mode::Streaming => {
            let mut events = handler
                .send_streaming_message(&params(), request())
                .await
                .unwrap();
            let first = timeout(WAIT, events.next())
                .await
                .unwrap()
                .expect("a response");
            if reply {
                assert!(
                    matches!(first, Ok(StreamResponse::Message(message)) if message.text() == Some("reply"))
                );
            } else {
                let error = first.expect_err("an uncommitted Task must never open this stream");
                assert_eq!(error.code, error_code::INTERNAL_ERROR);
                assert_eq!(error.message, SAVE_ERROR);
            }
            assert!(timeout(WAIT, events.next()).await.unwrap().is_none());
        }
    }
    let (id, mut inputs) = observed.recv().await.unwrap();
    assert!(
        timeout(WAIT, inputs.recv())
            .await
            .expect("the ended execution releases its inbox")
            .is_none()
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a failed save must not start a successor"
    );
    assert_eq!(store.creates.load(Ordering::SeqCst), usize::from(!reply));
    if reply || matches!(failure, Failure::BeforeWrite) {
        assert_eq!(
            get(&handler, &id).await.unwrap_err().code,
            error_code::TASK_NOT_FOUND
        );
        assert_eq!(
            store.updates.load(Ordering::SeqCst),
            0,
            "do not fabricate a failed Task in an empty store"
        );
    } else {
        // A backend error after writing has an uncertain outcome. Report the
        // error and finalize the Task that actually exists through the failure path.
        let task = get(&handler, &id).await.unwrap();
        assert_eq!(task.status.state, TaskState::Failed);
        assert_eq!(store.updates.load(Ordering::SeqCst), 1);
    }
    timeout(WAIT, handler.shutdown())
        .await
        .expect("the failed runtime shuts down cleanly");
}

#[tokio::test(start_paused = true)]
async fn a_failed_deferred_save_is_reported_and_never_streams_a_provisional_task() {
    for mode in [Mode::Unary, Mode::Streaming] {
        for failure in [Failure::BeforeWrite, Failure::AfterWrite] {
            check(mode, failure, false).await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn direct_messages_do_not_attempt_deferred_persistence() {
    for mode in [Mode::Unary, Mode::Streaming] {
        for failure in [Failure::BeforeWrite, Failure::AfterWrite] {
            check(mode, failure, true).await;
        }
    }
}
