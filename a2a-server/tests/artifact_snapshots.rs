// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

use a2a::{
    A2AError, Artifact, GetTaskRequest, ListTasksRequest, ListTasksResponse, Message, Part, Role,
    SendMessageRequest, SendMessageResponse, StreamResponse, Task, TaskArtifactUpdateEvent,
    TaskState, TaskStatus, TaskStatusUpdateEvent, error_code,
};
use a2a_server::task_store::TaskVersion;
use a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore, RequestHandler,
    TaskStore,
};
use async_trait::async_trait;
use futures::{
    StreamExt,
    stream::{self, BoxStream},
};
use serde_json::json;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

type Update = (Artifact, Option<bool>);

struct ArtifactAgent {
    updates: Vec<Update>,
}

impl AgentExecutor for ArtifactAgent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let mut events = vec![Ok(StreamResponse::Task(Task {
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: Some(chrono::Utc::now()),
            },
            artifacts: None,
            history: ctx.message.map(|message| vec![message]),
            metadata: None,
        }))];
        events.extend(
            self.updates
                .iter()
                .enumerate()
                .map(|(index, (artifact, append))| {
                    Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                        task_id: ctx.task_id.clone(),
                        context_id: ctx.context_id.clone(),
                        artifact: artifact.clone(),
                        append: *append,
                        last_chunk: Some(index + 1 == self.updates.len()),
                        metadata: Some([("sequence".into(), json!(index))].into()),
                    }))
                }),
        );
        events.push(Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: ctx.task_id,
            context_id: ctx.context_id,
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: Some(chrono::Utc::now()),
            },
            metadata: None,
        })));
        Box::pin(stream::iter(events))
    }

    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::empty())
    }
}

fn artifact(id: &str, texts: &[&str]) -> Artifact {
    Artifact {
        artifact_id: id.into(),
        name: None,
        description: None,
        parts: texts.iter().map(|text| Part::text(*text)).collect(),
        metadata: None,
        extensions: None,
    }
}

async fn snapshot(updates: Vec<Update>, streaming: bool) -> Result<Task, A2AError> {
    let handler = DefaultRequestHandler::new(
        ArtifactAgent {
            updates: updates.clone(),
        },
        InMemoryTaskStore::new(),
    );
    let request = SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text("hi")]),
        configuration: None,
        metadata: None,
        tenant: None,
    };
    let task = if streaming {
        let mut stream = handler
            .send_streaming_message(&Default::default(), request)
            .await?;
        let mut initial_task = None;
        let mut live_updates = vec![];
        while let Some(event) = stream.next().await {
            match event? {
                StreamResponse::Task(task) => initial_task = Some(task),
                StreamResponse::ArtifactUpdate(update) => live_updates.push(update),
                _ => {}
            }
        }
        let initial_task = initial_task.unwrap();
        let expected_updates: Vec<_> = updates
            .iter()
            .enumerate()
            .map(|(index, (artifact, append))| TaskArtifactUpdateEvent {
                task_id: initial_task.id.clone(),
                context_id: initial_task.context_id.clone(),
                artifact: artifact.clone(),
                append: *append,
                last_chunk: Some(index + 1 == updates.len()),
                metadata: Some([("sequence".into(), json!(index))].into()),
            })
            .collect();
        assert_eq!(
            live_updates, expected_updates,
            "Live deltas must stay unchanged"
        );
        handler
            .get_task(
                &Default::default(),
                GetTaskRequest {
                    id: initial_task.id,
                    history_length: None,
                    tenant: None,
                },
            )
            .await?
    } else {
        let SendMessageResponse::Task(task) =
            handler.send_message(&Default::default(), request).await?
        else {
            panic!("Expected a task");
        };
        // Blocking responses and the persisted task must agree too.
        let stored = handler
            .get_task(
                &Default::default(),
                GetTaskRequest {
                    id: task.id.clone(),
                    history_length: None,
                    tenant: None,
                },
            )
            .await?;
        assert_eq!(task, stored);
        task
    };
    assert_eq!(task.status.state, TaskState::Completed);
    Ok(task)
}

async fn assert_snapshot(updates: Vec<Update>, expected: Vec<Artifact>) {
    for streaming in [false, true] {
        let task =
            tokio::time::timeout(Duration::from_secs(5), snapshot(updates.clone(), streaming))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(task.artifacts.unwrap(), expected, "streaming={streaming}");
    }
}

#[tokio::test]
async fn single_artifact_is_preserved() {
    for append in [None, Some(false)] {
        let initial = artifact("answer", &["Hello"]);
        assert_snapshot(vec![(initial.clone(), append)], vec![initial]).await;
    }
}

#[tokio::test]
async fn append_updates_preserve_part_boundaries_and_empty_final_chunk() {
    assert_snapshot(
        vec![
            (artifact("answer", &["Hello"]), Some(false)),
            (artifact("answer", &[" ", "world"]), Some(true)),
            (artifact("answer", &[""]), Some(true)),
        ],
        vec![artifact("answer", &["Hello", " ", "world", ""])],
    )
    .await;
}

#[tokio::test]
async fn replacement_discards_the_draft_before_later_appends() {
    for append in [None, Some(false)] {
        let mut draft = artifact("answer", &["Draft"]);
        draft.name = Some("Draft name".into());
        draft.description = Some("Draft description".into());
        draft.metadata = Some([("draft".into(), json!(true))].into());
        draft.extensions = Some(vec!["https://example.com/draft".into()]);
        assert_snapshot(
            vec![
                (draft, None),
                (artifact("answer", &["Final"]), append),
                (artifact("answer", &["!"]), Some(true)),
            ],
            vec![artifact("answer", &["Final", "!"])],
        )
        .await;
    }
}

#[tokio::test]
async fn interleaved_ids_keep_their_order_and_independent_contents() {
    assert_snapshot(
        vec![
            (artifact("answer", &["Draft"]), None),
            (artifact("reasoning", &["Think"]), None),
            (artifact("answer", &["Final"]), Some(false)),
            (artifact("reasoning", &[" more"]), Some(true)),
            (artifact("answer", &["!"]), Some(true)),
        ],
        vec![
            artifact("answer", &["Final", "!"]),
            artifact("reasoning", &["Think", " more"]),
        ],
    )
    .await;
}

#[tokio::test]
async fn append_merges_artifact_metadata_and_preserves_structured_parts() {
    let mut initial = artifact("answer", &["Hello"]);
    initial.name = Some("Answer".into());
    initial.description = Some("Description".into());
    initial.extensions = Some(vec!["https://example.com/extension".into()]);
    initial.metadata = Some(
        [
            ("keep".into(), json!(true)),
            ("state".into(), json!("draft")),
        ]
        .into(),
    );
    let mut chunk = artifact("answer", &[]);
    chunk.name = Some("Chunk name".into());
    chunk.description = Some("Chunk description".into());
    chunk.extensions = Some(vec!["https://example.com/chunk".into()]);
    chunk.metadata = Some([("state".into(), json!("done")), ("new".into(), json!(42))].into());
    let mut data = Part::data(json!({"result": [1, 2]}));
    data.metadata = Some([("source".into(), json!("tool"))].into());
    let mut file = Part::raw(vec![0, 1, 255]);
    file.filename = Some("result.bin".into());
    file.media_type = Some("application/octet-stream".into());
    chunk.parts = vec![data.clone(), file.clone()];
    let mut expected = initial.clone();
    expected.parts = vec![Part::text("Hello"), data, file];
    expected.metadata = Some(
        [
            ("keep".into(), json!(true)),
            ("state".into(), json!("done")),
            ("new".into(), json!(42)),
        ]
        .into(),
    );
    assert_snapshot(vec![(initial, None), (chunk, Some(true))], vec![expected]).await;
}

#[tokio::test]
async fn append_can_introduce_metadata() {
    let initial = artifact("answer", &["Hello"]);
    let mut chunk = artifact("answer", &[" world"]);
    chunk.metadata = Some([("source".into(), json!("model"))].into());
    let mut expected = artifact("answer", &["Hello", " world"]);
    expected.metadata = chunk.metadata.clone();
    assert_snapshot(vec![(initial, None), (chunk, Some(true))], vec![expected]).await;
}

#[tokio::test]
async fn append_to_an_unknown_artifact_is_an_invalid_agent_response() {
    for streaming in [false, true] {
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            snapshot(
                vec![(artifact("missing", &["orphan chunk"]), Some(true))],
                streaming,
            ),
        )
        .await
        .unwrap();
        assert_eq!(result.unwrap_err().code, error_code::INVALID_AGENT_RESPONSE);
    }
}

/// An `InMemoryTaskStore` that counts its `get` calls. `TaskStore` is public and `get` is not
/// obliged to be cheap -- a store may rebuild the task from an event log -- so how often the
/// handler calls it is observable behaviour.
struct CountingTaskStore {
    inner: InMemoryTaskStore,
    gets: Arc<AtomicUsize>,
}

#[async_trait]
impl TaskStore for CountingTaskStore {
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.inner.create(task).await
    }

    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        self.inner.update(task).await
    }

    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        self.inner.get(task_id).await
    }

    async fn begin_cancel(&self, task_id: &str) -> Result<Task, A2AError> {
        self.inner.begin_cancel(task_id).await
    }

    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.inner.list(req).await
    }
}

/// Events for a task the execution already holds must not read the store: one `get` per
/// streamed event makes the cost of a turn grow with its length.
#[tokio::test]
async fn applying_events_to_a_known_task_does_not_read_the_store() {
    const FRAGMENTS: usize = 8;
    let gets = Arc::new(AtomicUsize::new(0));
    let handler = DefaultRequestHandler::new(
        ArtifactAgent {
            updates: (0..FRAGMENTS)
                .map(|i| {
                    let fragment = format!("fragment {i}");
                    (artifact("answer", &[fragment.as_str()]), Some(i > 0))
                })
                .collect(),
        },
        CountingTaskStore {
            inner: InMemoryTaskStore::new(),
            gets: Arc::clone(&gets),
        },
    );
    let request = SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text("hi")]),
        configuration: None,
        metadata: None,
        tenant: None,
    };

    let mut stream = handler
        .send_streaming_message(&Default::default(), request)
        .await
        .unwrap();
    let mut task_id = None;
    while let Some(event) = stream.next().await {
        if let StreamResponse::Task(task) = event.unwrap() {
            task_id = Some(task.id);
        }
    }

    // The one read is the handler's own lookup before it starts the executor; none of the
    // FRAGMENTS + 2 events adds one.
    assert_eq!(
        gets.load(Ordering::SeqCst),
        1,
        "the store must not be read once per applied event"
    );

    // The events were all applied: the fix must not change the result.
    let task = handler
        .get_task(
            &Default::default(),
            GetTaskRequest {
                id: task_id.unwrap(),
                history_length: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(task.status.state, TaskState::Completed);
    let artifacts = task.artifacts.unwrap();
    assert_eq!(artifacts.len(), 1);
    assert_eq!(artifacts[0].parts.len(), FRAGMENTS);
}
