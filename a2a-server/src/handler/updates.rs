// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Persist protocol-approved task mutations. Domain rules live in protocol.rs.

use super::protocol::{self, TaskMutation};
use a2a::*;

/// Apply persistence invariants, then write through the caller's TaskStore.
pub(super) async fn save_task_with(
    task_store: &dyn crate::TaskStore,
    mut task: Task,
    creating: bool,
) -> Result<Task, A2AError> {
    protocol::prepare_task_for_storage(&mut task);
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

#[cfg(test)]
pub(super) async fn apply_event_to_task(
    task_store: &dyn crate::TaskStore,
    current_task: Option<Task>,
    event: &mut StreamResponse,
) -> Result<Option<Task>, A2AError> {
    apply_event_to_new_or_stored_task(task_store, current_task, event, false).await
}

/// Resolve the authoritative snapshot before applying the pure mutation rules.
pub(super) async fn apply_event_to_new_or_stored_task(
    task_store: &dyn crate::TaskStore,
    current_task: Option<Task>,
    event: &mut StreamResponse,
    creating: bool,
) -> Result<Option<Task>, A2AError> {
    if let Some(task) = &current_task {
        protocol::validate_event_identity(task, event)?;
    }
    let current_task = match (current_task, protocol::mutation_task_id(event)) {
        (None, Some(id)) => task_store.get(id).await?,
        (current, _) => current,
    };
    match protocol::apply_task_mutation(current_task, event)? {
        TaskMutation::Save(task) => save_task_with(task_store, task, creating).await.map(Some),
        TaskMutation::Unchanged(task) => Ok(task),
    }
}

/// Publication remains the caller's responsibility after persistence succeeds.
pub(super) async fn persist_event(
    task_store: &dyn crate::TaskStore,
    base: Option<Task>,
    event: &mut StreamResponse,
    creating: bool,
) -> Result<(Option<Task>, bool), A2AError> {
    let generation_before = base.as_ref().map(|task| task.generation);
    let snapshot = apply_event_to_new_or_stored_task(task_store, base, event, creating).await?;
    let repeated = protocol::repeats_generation(event, generation_before);
    Ok((snapshot, repeated))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::protocol::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn initial_snapshot_stamps_artifacts_with_the_first_committed_generation() {
        for state in [TaskState::Working, TaskState::Completed] {
            let store = InMemoryTaskStore::new();
            let mut task = task_with_history("new-task", "context", 0);
            task.status.state = state.clone();
            task.status.timestamp = Some(chrono::Utc::now());
            task.generation = 99;
            task.artifacts = Some(vec![
                serde_json::from_value(serde_json::json!({
                    "artifactId":"result", "parts":[{"text":"answer"}], "role":"ROLE_AGENT"
                }))
                .unwrap(),
            ]);
            let mut event = StreamResponse::Task(task);
            let saved = apply_event_to_new_or_stored_task(&store, None, &mut event, true)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(saved.generation, 1);
            let artifact = &saved.artifacts.as_ref().unwrap()[0];
            assert_eq!(artifact.start_generation, Some(1));
            assert_eq!(artifact.end_generation, state.is_terminal().then_some(1));
            assert_eq!(store.get("new-task").await.unwrap().unwrap(), saved);
            assert_eq!(event, StreamResponse::Task(saved));
        }
    }

    /// DM-SERIAL-001: `save_task_with`'s backfill only covers what gets persisted
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

    /// The task-snapshot arm persists through the same `save_task_with` chokepoint,
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

#[cfg(test)]
mod regression_tests {
    use super::*;
    use crate::handler::protocol::{rebase_snapshot, validate_event_identity};
    use crate::{InMemoryTaskStore, TaskStore};

    fn task() -> Task {
        serde_json::from_value(serde_json::json!({
            "id":"task", "contextId":"context", "status":{"state":"TASK_STATE_WORKING"}
        }))
        .unwrap()
    }

    #[test]
    fn error_frames_cannot_claim_another_task() {
        let event = StreamResponse::Error(
            serde_json::from_value(serde_json::json!({
                "taskId":"other", "code":"UNAVAILABLE"
            }))
            .unwrap(),
        );
        assert_eq!(
            validate_event_identity(&task(), &event).unwrap_err().code,
            error_code::INVALID_AGENT_RESPONSE
        );
    }

    #[test]
    fn rebasing_a_snapshot_appends_new_history_without_duplicating_recorded_messages() {
        let first = Message::new(Role::User, vec![Part::text("first")]);
        let next = Message::new(Role::Agent, vec![Part::text("next")]);
        let mut stored = task();
        stored.history = Some(vec![first.clone()]);
        let mut snapshot = stored.clone();
        snapshot.history = Some(vec![first.clone(), next.clone()]);
        rebase_snapshot(&mut snapshot, stored);
        assert_eq!(snapshot.history, Some(vec![first, next]));
    }

    #[tokio::test]
    async fn saving_a_snapshot_cannot_overwrite_a_terminal_task() {
        let store = InMemoryTaskStore::new();
        let mut canceled = task();
        canceled.status.state = TaskState::Canceled;
        store.create(canceled.clone()).await.unwrap();
        let error = save_task_with(&store, task(), false).await.unwrap_err();
        assert_eq!(error.code, error_code::INVALID_REQUEST);
        assert_eq!(store.get("task").await.unwrap(), Some(canceled));
    }
}
