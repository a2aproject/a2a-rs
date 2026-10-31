// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Persist task updates, record status changes, and rebase executor snapshots.

use super::artifacts::{artifacts_differ, close_open_artifacts, merge_artifact};
use super::events::ActiveExecution;
use a2a::*;

/// An execution may only publish or mutate its bound task and context.
/// A message's optional task/context IDs may be absent for direct replies, but never name
/// another task. Validate before looking up or saving the event's IDs.
pub(super) fn validate_event_identity(task: &Task, event: &StreamResponse) -> Result<(), A2AError> {
    let (id, context) = match event {
        StreamResponse::Task(value) => (Some(value.id.as_str()), Some(value.context_id.as_str())),
        StreamResponse::StatusUpdate(value) => (
            Some(value.task_id.as_str()),
            Some(value.context_id.as_str()),
        ),
        StreamResponse::ArtifactUpdate(value) => (
            Some(value.task_id.as_str()),
            Some(value.context_id.as_str()),
        ),
        StreamResponse::MessageUpdate(value) => (
            Some(value.task_id.as_str()),
            Some(value.context_id.as_str()),
        ),
        StreamResponse::Error(value) => (Some(value.task_id.as_str()), None),
        StreamResponse::Message(value) => (value.task_id.as_deref(), value.context_id.as_deref()),
        _ => return Ok(()),
    };
    if id.is_some_and(|id| id != task.id)
        || context.is_some_and(|context| context != task.context_id)
    {
        return Err(A2AError::invalid_agent_response());
    }
    Ok(())
}

pub(super) async fn save_task(
    task_store: &dyn crate::TaskStore,
    task: Task,
) -> Result<Task, A2AError> {
    save_task_with(task_store, task, false).await
}

/// Saves `task`: `creating` for the first save of a task, which is created
/// outright; otherwise it is updated, and created if the store does not know it.
pub(super) async fn save_task_with(
    task_store: &dyn crate::TaskStore,
    mut task: Task,
    creating: bool,
) -> Result<Task, A2AError> {
    if task.status.state.is_terminal() {
        let generation = task.generation;
        close_open_artifacts(&mut task, generation);
    }
    // DM-SERIAL-001: a persisted/returned task status must carry a timestamp.
    // This is the single persistence chokepoint for executor events, so
    // backfilling here covers the task-snapshot and status-update paths
    // alike, while preserving any timestamp the executor already supplied.
    if task.status.timestamp.is_none() {
        task.status.timestamp = Some(chrono::Utc::now());
    }
    if creating {
        task_store.create(task.clone()).await?;
        return Ok(task);
    }
    match task_store.update(task.clone()).await {
        Ok(_) => Ok(task),
        Err(error) if error.code == error_code::TASK_NOT_FOUND => {
            task_store.create(task.clone()).await?;
            Ok(task)
        }
        Err(error) => Err(error),
    }
}

/// DM-SERIAL-001: backfills a missing status timestamp on the event itself,
/// before it is both persisted (via `apply_event_to_task`/`save_task`) and
/// broadcast to any live subscriber -- `save_task`'s own backfill only
/// covers what gets persisted; a subscriber watching `send_streaming_message`
/// or `subscribe_to_task` receives the broadcast event directly, never a
/// reload from the store, so backfilling only at persistence time still
/// leaves a live subscriber seeing a timestamp-less status.
pub(super) fn backfill_status_timestamp(event: &mut StreamResponse) {
    let status = match event {
        StreamResponse::Task(task) => &mut task.status,
        StreamResponse::StatusUpdate(update) => &mut update.status,
        _ => return,
    };
    if status.timestamp.is_none() {
        status.timestamp = Some(chrono::Utc::now());
    }
}

/// Records an agent status as the task's next timeline entry, advancing the
/// generation. Returns the generation after the mutation.
pub(super) fn record_status(task: &mut Task, status: TaskStatus) -> i64 {
    task.generation += 1;
    task.timeline
        .get_or_insert_with(Vec::new)
        .push(TimelineEntry {
            content: TimelineContent::Status(status),
            generation: task.generation,
        });
    task.generation
}

/// Rebases a snapshot an executor built onto the stored task. The generation,
/// timeline, history and artifact stamps belong to the server: an executor
/// that does not set `artifacts` or `history` leaves what is stored in place,
/// and a snapshot that changes nothing is not a mutation.
pub(super) fn rebase_snapshot(task: &mut Task, previous: Task) {
    let status_changed = task.status.state != previous.status.state
        || task.status.message != previous.status.message;
    // The artifacts the client wrote are the server's to keep: an executor
    // lists what it produced, not what the client sent it.
    let supplied = task.artifacts.take().map(|mut new| {
        for kept in previous.artifacts.iter().flatten() {
            if kept.role == Some(Role::User) {
                match new
                    .iter_mut()
                    .find(|artifact| artifact.artifact_id == kept.artifact_id)
                {
                    Some(stale) => *stale = kept.clone(),
                    None => new.push(kept.clone()),
                }
            }
        }
        new
    });
    let artifacts_changed = supplied
        .as_ref()
        .is_some_and(|new| artifacts_differ(new, previous.artifacts.as_deref().unwrap_or(&[])));

    task.timeline = previous.timeline;
    task.generation = previous.generation;
    // Recorded messages are append-only. A snapshot built before a later
    // client input must not erase that input from the compatibility history.
    task.history = match (previous.history, task.history.take()) {
        (Some(mut current), Some(supplied)) => {
            for message in supplied {
                if !current
                    .iter()
                    .any(|kept| kept.message_id == message.message_id)
                {
                    current.push(message);
                }
            }
            Some(current)
        }
        (current, supplied) => supplied.or(current),
    };
    if task.metadata.is_none() {
        task.metadata = previous.metadata;
    }
    let stored_artifacts = previous.artifacts;
    task.artifacts = match supplied {
        Some(new) if artifacts_changed => Some(new),
        _ => stored_artifacts.clone(),
    };

    if status_changed || artifacts_changed {
        task.generation += 1;
        let generation = task.generation;
        if status_changed {
            task.timeline
                .get_or_insert_with(Vec::new)
                .push(TimelineEntry {
                    content: TimelineContent::Status(task.status.clone()),
                    generation,
                });
        }
        let stored = stored_artifacts.unwrap_or_default();
        for artifact in task.artifacts.iter_mut().flatten() {
            let before = stored
                .iter()
                .find(|stored| stored.artifact_id == artifact.artifact_id);
            artifact.start_generation = artifact
                .start_generation
                .or(before.and_then(|stored| stored.start_generation))
                .or(Some(generation));
            artifact.end_generation = artifact
                .end_generation
                .or(before.and_then(|stored| stored.end_generation));
            artifact.role = artifact
                .role
                .clone()
                .or_else(|| before.and_then(|stored| stored.role.clone()));
        }
    }
    if task.status.state.is_terminal() {
        close_open_artifacts(task, task.generation);
    }
}

/// Applies an executor event to the stored task and stamps the event with the
/// task's generation after the mutation (A2A 1.1 §3.2.7): every status or
/// artifact update advances the generation by exactly one and maps to one
/// event.
pub(super) async fn apply_event_to_task(
    task_store: &dyn crate::TaskStore,
    current_task: Option<Task>,
    event: &mut StreamResponse,
) -> Result<Option<Task>, A2AError> {
    apply_event_to_new_or_stored_task(task_store, current_task, event, false).await
}

/// [`apply_event_to_task`], where `creating` says the event is the first of a
/// task that is not in the store yet.
pub(super) async fn apply_event_to_new_or_stored_task(
    task_store: &dyn crate::TaskStore,
    current_task: Option<Task>,
    event: &mut StreamResponse,
    creating: bool,
) -> Result<Option<Task>, A2AError> {
    if let Some(task) = &current_task {
        validate_event_identity(task, event)?;
    }
    match event {
        StreamResponse::Task(task) => {
            let previous = match current_task {
                Some(previous) => Some(previous),
                None => task_store.get(&task.id).await?,
            };
            match previous {
                Some(previous) => rebase_snapshot(task, previous),
                None => {
                    task.generation = 0;
                    task.timeline = None;
                    let generation = record_status(task, task.status.clone());
                    for artifact in task.artifacts.iter_mut().flatten() {
                        artifact.start_generation.get_or_insert(generation);
                    }
                    if task.status.state.is_terminal() {
                        close_open_artifacts(task, generation);
                    }
                }
            }
            save_task_with(task_store, task.clone(), creating)
                .await
                .map(Some)
        }
        StreamResponse::StatusUpdate(update) => {
            let mut task = match current_task {
                Some(task) => task,
                None => task_store
                    .get(&update.task_id)
                    .await?
                    .ok_or_else(|| A2AError::task_not_found(&update.task_id))?,
            };
            task.status = update.status.clone();
            update.generation = record_status(&mut task, update.status.clone());
            if task.status.state.is_terminal() {
                close_open_artifacts(&mut task, update.generation);
            }
            save_task_with(task_store, task, creating).await.map(Some)
        }
        StreamResponse::ArtifactUpdate(update) => {
            let mut task = match current_task {
                Some(task) => task,
                None => task_store
                    .get(&update.task_id)
                    .await?
                    .ok_or_else(|| A2AError::task_not_found(&update.task_id))?,
            };
            update.generation = merge_artifact(&mut task, update)?;
            save_task_with(task_store, task, creating).await.map(Some)
        }
        // A message appended to the task, and a rejection of a client request,
        // are what the server reports about *clients*. An executor has no
        // business producing either: forwarded, they would carry no
        // generation and reach subscribers that never asked for them.
        StreamResponse::MessageUpdate(_) | StreamResponse::Error(_) => {
            Err(A2AError::invalid_agent_response())
        }
        _ => Ok(current_task),
    }
}

/// Applies `event` to the task under the writer lock the caller holds and
/// publishes it to the subscribers.
///
/// A `Task` snapshot that changed nothing keeps the generation it found. It
/// is published marked as a repeat, which client-facing streams drop: the
/// generation was already announced, and a second event at it would break
/// A2A 1.1 §3.2.7. Returns whether the event is one clients should see (and
/// be pushed).
pub(super) async fn apply_and_publish(
    task_store: &dyn crate::TaskStore,
    active: &ActiveExecution,
    base: Option<Task>,
    event: &mut StreamResponse,
) -> Result<bool, A2AError> {
    apply_and_publish_with(task_store, active, base, event, false).await
}

/// [`apply_and_publish`], where `creating` says the event is the first of a
/// task that is not in the store yet.
pub(super) async fn apply_and_publish_with(
    task_store: &dyn crate::TaskStore,
    active: &ActiveExecution,
    base: Option<Task>,
    event: &mut StreamResponse,
    creating: bool,
) -> Result<bool, A2AError> {
    let generation_before = base.as_ref().map(|task| task.generation);
    let updated_task = apply_event_to_new_or_stored_task(task_store, base, event, creating).await?;
    let repeated = matches!(
        &*event,
        StreamResponse::Task(task) if Some(task.generation) == generation_before
    );
    active
        .publish_marked(Ok(&*event), updated_task, repeated)
        .await;
    Ok(!repeated)
}

/// Where a cancellation stands while its executor's events are applied.
pub(super) struct CancelEntry {
    /// The store gave the transition its generation and timeline entry.
    pub(super) recorded_by_store: bool,
    /// The generation it gave.
    pub(super) generation: i64,
    /// An event announcing the cancellation has been published.
    pub(super) announced: bool,
}

impl CancelEntry {
    /// Whether `event` is the executor's own status for the cancellation, which
    /// belongs in the store's entry rather than in one of its own: the task has
    /// not moved on since (nothing else advanced the generation).
    pub(super) fn can_amend(&self, base: &Task, event: &StreamResponse) -> bool {
        self.recorded_by_store
            && !self.announced
            && base.generation == self.generation
            && matches!(
                event,
                StreamResponse::StatusUpdate(update) if update.status.state == TaskState::Canceled
            )
    }

    /// Notes that a published event announced the canceled state.
    pub(super) fn note_published(&mut self, published: bool, event: &StreamResponse) {
        let canceled = match event {
            StreamResponse::StatusUpdate(update) => update.status.state == TaskState::Canceled,
            StreamResponse::Task(task) => task.status.state == TaskState::Canceled,
            _ => false,
        };
        self.announced |= published && canceled;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    /// DM-SERIAL-001: `save_task`'s backfill only covers what gets persisted
    /// -- a live subscriber receives the broadcast event directly, never a
    /// reload from the store, so the timestamp must also be backfilled on
    /// the event itself before it is published.
    #[tokio::test]
    async fn test_streamed_task_event_has_a_backfilled_timestamp() {
        use futures::StreamExt;
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = SendMessageRequest {
            artifact_updates: Vec::new(),
            message: make_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let mut stream = handler.send_streaming_message(&params, req).await.unwrap();
        let event = stream.next().await.unwrap().unwrap();
        match event {
            StreamResponse::Task(task) => assert!(
                task.status.timestamp.is_some(),
                "a live-streamed task event must carry a backfilled timestamp"
            ),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    /// `ArtifactUpdate` and `Message` carry no status of their own, so the
    /// backfill must leave them alone rather than panic on the missing arm.
    #[test]
    fn test_backfill_status_timestamp_ignores_events_without_a_status() {
        let mut artifact_update = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
            generation: 0,
            task_id: "t1".into(),
            context_id: "c1".into(),
            artifact: Artifact {
                end_generation: None,
                role: None,
                start_generation: None,
                artifact_id: "a1".into(),
                name: None,
                description: None,
                parts: vec![Part::text("payload")],
                metadata: None,
                extensions: None,
            },
            append: None,
            last_chunk: None,
            metadata: None,
        });
        let before = artifact_update.clone();
        backfill_status_timestamp(&mut artifact_update);
        assert_eq!(artifact_update, before);

        let mut message = StreamResponse::Message(make_message());
        let before = message.clone();
        backfill_status_timestamp(&mut message);
        assert_eq!(message, before);
    }

    /// DM-SERIAL-001: a status update whose status omits a timestamp must be
    /// backfilled before the task is persisted, so a returned task never
    /// carries a timestamp-less status.
    #[tokio::test]
    async fn test_status_update_backfills_missing_timestamp() {
        let store = InMemoryTaskStore::new();
        store
            .create(Task {
                generation: 0,
                timeline: None,
                id: "t-backfill".into(),
                context_id: "ctx".into(),
                status: TaskStatus {
                    state: TaskState::Submitted,
                    message: None,
                    timestamp: Some(chrono::Utc::now()),
                },
                artifacts: None,
                history: None,
                metadata: None,
            })
            .await
            .unwrap();

        let updated = apply_event_to_task(
            &store,
            None,
            &mut StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                generation: 0,
                task_id: "t-backfill".into(),
                context_id: "ctx".into(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            }),
        )
        .await
        .unwrap()
        .expect("status update yields a task");

        assert_eq!(updated.status.state, TaskState::Working);
        assert!(
            updated.status.timestamp.is_some(),
            "missing status timestamp must be backfilled"
        );
    }

    /// A timestamp supplied by the executor is authoritative and must be kept
    /// verbatim, not overwritten by the backfill.
    #[tokio::test]
    async fn test_status_update_preserves_supplied_timestamp() {
        let store = InMemoryTaskStore::new();
        store
            .create(Task {
                generation: 0,
                timeline: None,
                id: "t-preserve".into(),
                context_id: "ctx".into(),
                status: TaskStatus {
                    state: TaskState::Submitted,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            })
            .await
            .unwrap();

        let supplied = chrono::DateTime::parse_from_rfc3339("2020-01-02T03:04:05Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let updated = apply_event_to_task(
            &store,
            None,
            &mut StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                generation: 0,
                task_id: "t-preserve".into(),
                context_id: "ctx".into(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: Some(supplied),
                },
                metadata: None,
            }),
        )
        .await
        .unwrap()
        .expect("status update yields a task");

        assert_eq!(updated.status.timestamp, Some(supplied));
    }

    /// The task-snapshot arm persists through the same `save_task` chokepoint,
    /// so an executor `Task` event whose status omits a timestamp must also be
    /// backfilled (DM-SERIAL-001).
    #[tokio::test]
    async fn test_task_snapshot_backfills_missing_timestamp() {
        let store = InMemoryTaskStore::new();

        let returned = apply_event_to_task(
            &store,
            None,
            &mut StreamResponse::Task(Task {
                generation: 0,
                timeline: None,
                id: "t-snapshot".into(),
                context_id: "ctx".into(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            }),
        )
        .await
        .unwrap()
        .expect("task event yields a task");

        assert!(
            returned.status.timestamp.is_some(),
            "missing status timestamp must be backfilled on the task-snapshot path"
        );
        let persisted = store.get("t-snapshot").await.unwrap().unwrap();
        assert!(
            persisted.status.timestamp.is_some(),
            "the persisted task must carry a timestamp"
        );
    }

    /// A timestamp already present on a task snapshot is authoritative and
    /// must survive the persistence chokepoint unchanged.
    #[tokio::test]
    async fn test_task_snapshot_preserves_supplied_timestamp() {
        let store = InMemoryTaskStore::new();

        let supplied = chrono::DateTime::parse_from_rfc3339("2021-06-07T08:09:10Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let returned = apply_event_to_task(
            &store,
            None,
            &mut StreamResponse::Task(Task {
                generation: 0,
                timeline: None,
                id: "t-snapshot-kept".into(),
                context_id: "ctx".into(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: Some(supplied),
                },
                artifacts: None,
                history: None,
                metadata: None,
            }),
        )
        .await
        .unwrap()
        .expect("task event yields a task");

        assert_eq!(returned.status.timestamp, Some(supplied));
        let persisted = store.get("t-snapshot-kept").await.unwrap().unwrap();
        assert_eq!(persisted.status.timestamp, Some(supplied));
    }
}
