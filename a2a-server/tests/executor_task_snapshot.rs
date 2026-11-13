// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Execution snapshots must carry the same admitted input in every response mode.

use a2a::*;
use a2a_server::*;
use futures::{StreamExt, stream, stream::BoxStream};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{Notify, mpsc},
    task::JoinHandle,
    time::timeout,
};

const WAIT: Duration = Duration::from_secs(2);

#[derive(Clone, Default)]
struct Store(Arc<InMemoryTaskStore>);

#[async_trait::async_trait]
impl TaskStore for Store {
    async fn create(&self, task: Task) -> Result<task_store::TaskVersion, A2AError> {
        self.0.create(task).await
    }
    async fn update(&self, task: Task) -> Result<task_store::TaskVersion, A2AError> {
        self.0.update(task).await
    }
    async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
        self.0.get(id).await
    }
    async fn list(&self, request: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.0.list(request).await
    }
    async fn begin_cancel(&self, id: &str) -> Result<Task, A2AError> {
        self.0.begin_cancel(id).await
    }
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Blocking,
    Immediate,
    Streaming,
    Live,
    HttpLive,
}
const MODES: [Mode; 5] = [
    Mode::Blocking,
    Mode::Immediate,
    Mode::Streaming,
    Mode::Live,
    Mode::HttpLive,
];

fn params() -> ServiceParams {
    ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])])
}

fn request(id: Option<&str>) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text("admitted input")]);
    message.task_id = id.map(str::to_owned);
    message.context_id = Some("context".into());
    SendMessageRequest {
        message: Some(message),
        artifact_updates: vec![
            serde_json::from_value(serde_json::json!({
                "taskId": "", "contextId": "", "artifact": {
                    "artifactId": "client", "parts": [{"text": "client data"}]
                }, "lastChunk": true
            }))
            .unwrap(),
        ],
        configuration: Some(SendMessageConfiguration {
            return_immediately: None,
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: Some(0),
            timeline_length: Some(0),
            if_generation_match: None,
        }),
        metadata: None,
        tenant: None,
    }
}

fn run(
    handler: DefaultRequestHandler,
    mode: Mode,
    mut request: SendMessageRequest,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        match mode {
            Mode::Blocking | Mode::Immediate => {
                request.configuration.as_mut().unwrap().return_immediately =
                    Some(matches!(mode, Mode::Immediate));
                assert!(matches!(
                    handler.send_message(&params(), request).await.unwrap(),
                    SendMessageResponse::Task(_)
                ));
            }
            Mode::Streaming | Mode::Live | Mode::HttpLive => {
                let first = StreamRequest {
                    tenant: None,
                    payload: StreamRequestPayload::SendMessage(request.clone()),
                };
                let mut output = match mode {
                    Mode::Streaming => handler
                        .send_streaming_message(&params(), request)
                        .await
                        .unwrap(),
                    Mode::Live => handler
                        .send_live_message(
                            &params(),
                            stream::once(async move { first })
                                .chain(stream::pending())
                                .boxed(),
                        )
                        .await
                        .unwrap(),
                    Mode::HttpLive => {
                        match handler.send_live_request(&params(), first).await.unwrap() {
                            LiveResponse::Stream(output) => output,
                            _ => panic!("expected a new live response"),
                        }
                    }
                    _ => unreachable!(),
                };
                while let Some(event) = output.next().await {
                    event.unwrap();
                }
            }
        }
    })
}

fn configure(handler: DefaultRequestHandler) -> DefaultRequestHandler {
    handler.with_capabilities(AgentCapabilities {
        streaming: Some(true),
        user_artifacts: Some(true),
        ..Default::default()
    })
}

struct Observed {
    snapshot: Option<Task>,
    new_task: bool,
}
struct Executor {
    observed: mpsc::UnboundedSender<Observed>,
    finish: Arc<Notify>,
}
impl AgentExecutor for Executor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        self.observed
            .send(Observed {
                snapshot: ctx.stored_task,
                new_task: ctx.new_task,
            })
            .unwrap();
        let finish = self.finish.clone();
        stream::once(async move {
            finish.notified().await;
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: ctx.task_id,
                context_id: ctx.context_id,
                generation: 0,
                metadata: None,
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: None,
                },
            }))
        })
        .boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

fn assert_admitted_input(task: &Task) {
    assert_eq!(task.context_id, "context");
    assert_eq!(
        task.history.as_ref().unwrap().last().unwrap().text(),
        Some("admitted input")
    );
    assert!(task.timeline.iter().flatten().any(|entry| matches!(&entry.content, TimelineContent::Message(message) if message.text() == Some("admitted input"))));
    let client = task
        .artifacts
        .iter()
        .flatten()
        .find(|artifact| artifact.artifact_id == "client")
        .unwrap();
    assert_eq!(client.role, Some(Role::User));
    assert!(client.start_generation.is_some());
    assert!(client.end_generation.is_some());
}

#[tokio::test]
async fn new_executions_receive_the_prepared_snapshot_in_every_send_mode() {
    for mode in MODES {
        let store = Store::default();
        let (sent, mut observed) = mpsc::unbounded_channel();
        let finish = Arc::new(Notify::new());
        let handler = configure(DefaultRequestHandler::new(
            Executor {
                observed: sent,
                finish: finish.clone(),
            },
            store.clone(),
        ));
        let response = run(handler.clone(), mode, request(None));
        let observed = timeout(WAIT, observed.recv()).await.unwrap().unwrap();
        assert!(observed.new_task, "{mode:?}");
        let task = observed
            .snapshot
            .unwrap_or_else(|| panic!("{mode:?}: missing prepared snapshot"));
        assert_admitted_input(&task);
        let committed = store.get(&task.id).await.unwrap();
        match mode {
            Mode::Blocking | Mode::Streaming => assert!(
                committed.is_none(),
                "a snapshot must not force deferred creation"
            ),
            _ => assert_eq!(committed.as_ref(), Some(&task)),
        }
        finish.notify_one();
        timeout(WAIT, response).await.unwrap().unwrap();
        handler.shutdown().await;
    }
}

#[tokio::test]
async fn continuations_receive_the_committed_input_snapshot_in_every_send_mode() {
    for mode in MODES {
        let store = Store::default();
        let task: Task = serde_json::from_value(serde_json::json!({
            "id": "task", "contextId": "context", "status": {"state": TaskState::InputRequired}, "generation": 0
        })).unwrap();
        store.create(task).await.unwrap();
        let (sent, mut observed) = mpsc::unbounded_channel();
        let finish = Arc::new(Notify::new());
        let handler = configure(DefaultRequestHandler::new(
            Executor {
                observed: sent,
                finish: finish.clone(),
            },
            store.clone(),
        ));
        let response = run(handler.clone(), mode, request(Some("task")));
        let observed = timeout(WAIT, observed.recv()).await.unwrap().unwrap();
        assert!(!observed.new_task, "{mode:?}");
        let task = observed.snapshot.unwrap();
        assert_admitted_input(&task);
        assert_eq!(
            store.get("task").await.unwrap().as_ref(),
            Some(&task),
            "{mode:?}"
        );
        finish.notify_one();
        timeout(WAIT, response).await.unwrap().unwrap();
        handler.shutdown().await;
    }
}

struct Typed(Executor);
#[async_trait::async_trait]
impl Agent for Typed {
    async fn start(&self, message: NewMessage) -> Result<Start, A2AError> {
        let mut task = message.new_task(TaskState::Working);
        task.metadata = Some(std::collections::HashMap::from([(
            "checkpoint".into(),
            serde_json::json!("agent state"),
        )]));
        Ok(Start::Task(task))
    }
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        self.0.execute(ctx)
    }
}

#[tokio::test]
async fn typed_agents_receive_the_committed_start_snapshot_in_every_send_mode() {
    for mode in MODES {
        let store = Store::default();
        let (sent, mut observed) = mpsc::unbounded_channel();
        let finish = Arc::new(Notify::new());
        let handler = configure(DefaultRequestHandler::from_agent(
            Typed(Executor {
                observed: sent,
                finish: finish.clone(),
            }),
            store.clone(),
        ));
        let response = run(handler.clone(), mode, request(None));
        let observed = timeout(WAIT, observed.recv()).await.unwrap().unwrap();
        assert!(observed.new_task);
        let task = observed.snapshot.unwrap();
        assert_admitted_input(&task);
        assert_eq!(task.status.state, TaskState::Working);
        assert!(task.status.timestamp.is_some());
        assert_eq!(task.metadata.as_ref().unwrap()["checkpoint"], "agent state");
        assert_eq!(
            store.get(&task.id).await.unwrap().as_ref(),
            Some(&task),
            "{mode:?}: the context must reflect the committed generation and input"
        );
        finish.notify_one();
        timeout(WAIT, response).await.unwrap().unwrap();
        handler.shutdown().await;
    }
}
