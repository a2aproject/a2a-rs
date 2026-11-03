// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Persist a sanitized terminal status for failures of an existing Task.

use super::{DefaultRequestHandler, events::ActiveExecution, updates::persist_event};
use a2a::*;

/// Called only inside the Task coordinator. A failed lazy start leaves no Task.
pub(super) async fn persist_failure(
    handler: &DefaultRequestHandler,
    active: &ActiveExecution,
    task_id: &str,
    error: &A2AError,
) -> Option<Task> {
    if active.snapshot_is_terminal().await {
        return None;
    }
    let task = match handler.task_store.get(task_id).await {
        Ok(Some(task)) if !task.status.state.is_terminal() => task,
        Ok(_) => return None,
        Err(cause) => {
            tracing::warn!(%task_id, %cause, "could not load the failed Task");
            return None;
        }
    };
    let text = crate::sanitized_internal_error(&error.message).message;
    let mut event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: task.id.clone(),
        context_id: task.context_id.clone(),
        generation: 0,
        metadata: None,
        status: TaskStatus {
            state: TaskState::Failed,
            message: Some(Message {
                task_id: Some(task.id.clone()),
                context_id: Some(task.context_id.clone()),
                ..Message::new(Role::Agent, vec![Part::text(text)])
            }),
            timestamp: Some(chrono::Utc::now()),
        },
    });
    match persist_event(handler.task_store.as_ref(), Some(task), &mut event, false).await {
        Ok((snapshot, _)) => snapshot,
        Err(cause) => {
            tracing::warn!(%task_id, %cause, "could not persist the failed Task");
            None
        }
    }
}
