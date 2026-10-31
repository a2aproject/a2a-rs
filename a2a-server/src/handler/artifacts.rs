// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Apply client and executor artifact changes, preserving ownership and generations.

use a2a::*;

/// Merges an artifact update into the task, advancing the generation. The
/// stored artifact is stamped with the generations at which it started and
/// completed; the update itself is left as sent apart from the generation it
/// is assigned. Returns the generation after the mutation.
pub(super) fn merge_artifact(
    task: &mut Task,
    update: &mut TaskArtifactUpdateEvent,
) -> Result<i64, A2AError> {
    task.generation += 1;
    let generation = task.generation;
    let append = update.append.unwrap_or(false);
    // The wire cannot tell an unset `lastChunk` from `false`, so only an
    // explicit `true` completes an artifact here; one left open is closed when
    // the task reaches a terminal state.
    let complete = update.last_chunk == Some(true);

    let artifacts = task.artifacts.get_or_insert_with(Vec::new);
    let existing = artifacts
        .iter_mut()
        .find(|artifact| artifact.artifact_id == update.artifact.artifact_id);
    if append {
        let artifact = existing.ok_or_else(A2AError::invalid_agent_response)?;
        artifact.parts.extend(update.artifact.parts.iter().cloned());
        if let Some(metadata) = &update.artifact.metadata {
            artifact
                .metadata
                .get_or_insert_with(Default::default)
                .extend(metadata.clone());
        }
        if complete {
            artifact.end_generation = Some(generation);
        }
    } else {
        let mut replacement = update.artifact.clone();
        // Replacing an artifact keeps who wrote it.
        replacement.role = replacement
            .role
            .clone()
            .or_else(|| existing.as_ref().and_then(|artifact| artifact.role.clone()));
        replacement.start_generation = Some(
            existing
                .as_ref()
                .and_then(|artifact| artifact.start_generation)
                .unwrap_or(generation),
        );
        replacement.end_generation = complete.then_some(generation);
        match existing {
            Some(artifact) => *artifact = replacement,
            None => artifacts.push(replacement),
        }
    }
    Ok(generation)
}

/// Applies the artifacts a client wrote in `artifactUpdates`, in list order,
/// advancing the generation for each. The client does not own `generation`:
/// whatever it sent is ignored and the artifact is attributed to the user.
///
/// When the message creates the task the client cannot know its ids, so they
/// are assigned; on an existing task they must match. A client may only touch
/// artifacts it wrote itself.
pub(super) fn apply_client_artifacts(
    task: &mut Task,
    req: &SendMessageRequest,
    new_task: bool,
) -> Result<Vec<TaskArtifactUpdateEvent>, A2AError> {
    let mut applied = Vec::with_capacity(req.artifact_updates.len());
    for update in &req.artifact_updates {
        if update.artifact.artifact_id.is_empty() {
            return Err(A2AError::invalid_params(
                "an artifact update needs an artifactId",
            ));
        }
        let mut update = update.clone();
        if !new_task {
            if !update.task_id.is_empty() && update.task_id != task.id {
                return Err(A2AError::invalid_params(format!(
                    "artifact update targets task {}, not {}",
                    update.task_id, task.id
                )));
            }
            if !update.context_id.is_empty() && update.context_id != task.context_id {
                return Err(A2AError::invalid_params(format!(
                    "artifact update names context {}, but task {} belongs to {}",
                    update.context_id, task.id, task.context_id
                )));
            }
        }
        let existing_role = task
            .artifacts
            .iter()
            .flatten()
            .find(|artifact| artifact.artifact_id == update.artifact.artifact_id)
            .map(|artifact| artifact.role.clone());
        if existing_role.is_some_and(|role| role != Some(Role::User)) {
            return Err(A2AError::invalid_params(format!(
                "artifact {} was not written by the client and cannot be changed by it",
                update.artifact.artifact_id
            )));
        }
        update.artifact.role = Some(Role::User);
        update.task_id = task.id.clone();
        update.context_id = task.context_id.clone();
        update.generation = merge_artifact(task, &mut update).map_err(|_| {
            A2AError::invalid_params(format!(
                "cannot append to unknown artifact {}",
                update.artifact.artifact_id
            ))
        })?;
        applied.push(update);
    }
    Ok(applied)
}

/// Whether the artifacts an executor supplied differ in content from the
/// stored ones. The generations the server stamps are not part of the content.
pub(super) fn artifacts_differ(supplied: &[Artifact], stored: &[Artifact]) -> bool {
    supplied.len() != stored.len()
        || supplied.iter().any(|a| {
            stored
                .iter()
                .find(|b| b.artifact_id == a.artifact_id)
                .is_none_or(|b| {
                    a.name != b.name
                        || a.description != b.description
                        || a.parts != b.parts
                        || a.metadata != b.metadata
                        || a.extensions != b.extensions
                })
        })
}

/// A terminal task has no more output coming, so any artifact still open is
/// completed at the generation that ended it.
pub(super) fn close_open_artifacts(task: &mut Task, generation: i64) {
    for artifact in task.artifacts.iter_mut().flatten() {
        artifact.end_generation.get_or_insert(generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    /// Reported in #161 against a different crate (`a2a-protocol-server`),
    /// but worth pinning here: two tasks sharing a `contextId` must each
    /// carry only their own artifacts. The task store is keyed by task id,
    /// so there is no path by which a context could aggregate them — this
    /// test exists so that stays true.
    #[tokio::test]
    async fn test_artifacts_do_not_aggregate_across_tasks_in_one_context() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let context_id = "ctx-shared";

        for (task_id, artifact_id) in [("t-first", "a-1"), ("t-second", "a-2")] {
            handler
                .task_store
                .create(Task {
                    generation: 0,
                    timeline: None,
                    id: task_id.into(),
                    context_id: context_id.into(),
                    status: TaskStatus {
                        state: TaskState::Working,
                        message: None,
                        timestamp: None,
                    },
                    artifacts: None,
                    history: None,
                    metadata: None,
                })
                .await
                .unwrap();

            apply_event_to_task(
                handler.task_store.as_ref(),
                None,
                &mut StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                    generation: 0,
                    task_id: task_id.into(),
                    context_id: context_id.into(),
                    artifact: Artifact {
                        end_generation: None,
                        role: None,
                        start_generation: None,
                        artifact_id: artifact_id.into(),
                        name: None,
                        description: None,
                        parts: vec![Part::text("payload")],
                        metadata: None,
                        extensions: None,
                    },
                    append: None,
                    last_chunk: None,
                    metadata: None,
                }),
            )
            .await
            .unwrap();
        }

        for (task_id, expected) in [("t-first", "a-1"), ("t-second", "a-2")] {
            let task = handler
                .get_task(
                    &params,
                    GetTaskRequest {
                        current_generation: None,
                        timeline_length: None,
                        id: task_id.into(),
                        history_length: None,
                        tenant: None,
                    },
                )
                .await
                .unwrap();
            let ids: Vec<&str> = task
                .artifacts
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|a| a.artifact_id.as_str())
                .collect();
            assert_eq!(
                ids,
                vec![expected],
                "{task_id} must carry only its own artifact"
            );
        }
    }
    #[tokio::test]
    async fn rejected_artifact_contexts_and_unknown_appends_do_not_change_storage() {
        let handler = make_handler().with_capabilities(AgentCapabilities {
            user_artifacts: Some(true),
            ..AgentCapabilities::default()
        });
        seed_task(&handler, "artifact-validation", "context").await;
        let before = handler
            .task_store
            .get("artifact-validation")
            .await
            .unwrap()
            .unwrap();
        for (context, append) in [("other-context", false), ("context", true)] {
            let mut request = send_req(Some("artifact-validation"), Some("context"));
            request.artifact_updates = vec![TaskArtifactUpdateEvent {
                task_id: "artifact-validation".into(),
                context_id: context.into(),
                artifact: serde_json::from_value(serde_json::json!({
                    "artifactId": "unknown", "parts": [{"text": "client payload"}]
                }))
                .unwrap(),
                append: Some(append),
                last_chunk: Some(true),
                metadata: None,
                generation: 0,
            }];
            let error = handler
                .send_message(&ServiceParams::new(), request)
                .await
                .unwrap_err();
            assert_eq!(error.code, error_code::INVALID_PARAMS);
            assert_eq!(
                handler
                    .task_store
                    .get("artifact-validation")
                    .await
                    .unwrap()
                    .unwrap(),
                before
            );
        }
    }
}
