// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use super::{ActiveExecution, artifacts_differ, close_open_artifacts, merge_artifact};
use a2a::*;

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
    mut task: Task,
) -> Result<Task, A2AError> {
    if task.status.state.is_terminal() {
        for artifact in task.artifacts.iter_mut().flatten() {
            artifact.end_generation.get_or_insert(task.generation);
        }
    }
    // DM-SERIAL-001: a persisted/returned task status must carry a timestamp.
    // This is the single persistence chokepoint for executor events, so
    // backfilling here covers the task-snapshot and status-update paths
    // alike, while preserving any timestamp the executor already supplied.
    if task.status.timestamp.is_none() {
        task.status.timestamp = Some(chrono::Utc::now());
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
            save_task(task_store, task.clone()).await.map(Some)
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
            save_task(task_store, task).await.map(Some)
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
            save_task(task_store, task).await.map(Some)
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
    let generation_before = base.as_ref().map(|task| task.generation);
    let updated_task = apply_event_to_task(task_store, base, event).await?;
    let repeated = matches!(
        &*event,
        StreamResponse::Task(task) if Some(task.generation) == generation_before
    );
    active
        .publish_marked(Ok(&*event), updated_task, repeated)
        .await;
    Ok(!repeated)
}
