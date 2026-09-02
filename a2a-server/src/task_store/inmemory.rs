// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::RwLock;

use super::store::{TaskStore, TaskVersion};

struct StoredEntry {
    task: Task,
    version: TaskVersion,
}

/// In-memory task store. Contents do not survive restarts.
pub struct InMemoryTaskStore {
    tasks: RwLock<HashMap<TaskId, StoredEntry>>,
}

impl InMemoryTaskStore {
    pub fn new() -> Self {
        InMemoryTaskStore {
            tasks: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TaskStore for InMemoryTaskStore {
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        let mut store = self.tasks.write().await;
        if store.contains_key(&task.id) {
            return Err(A2AError::internal("task already exists"));
        }
        let id = task.id.clone();
        store.insert(id, StoredEntry { task, version: 1 });
        Ok(1)
    }

    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        let mut store = self.tasks.write().await;
        let entry = store
            .get_mut(&task.id)
            .ok_or_else(|| A2AError::task_not_found(&task.id))?;
        // BUG-43: a terminal task's state is immutable. Re-applying the same
        // terminal state is allowed so idempotent re-emission (e.g. cancel
        // flows) keeps working, but a terminal state can never be overwritten
        // by a different state.
        if entry.task.status.state.is_terminal() && entry.task.status.state != task.status.state {
            return Err(A2AError::invalid_request(format!(
                "task {} is in terminal state {:?} and cannot transition to {:?}",
                task.id, entry.task.status.state, task.status.state
            )));
        }
        entry.version += 1;
        entry.task = task;
        Ok(entry.version)
    }

    async fn begin_cancel(&self, task_id: &str) -> Result<Task, A2AError> {
        // Hold the write lock across the check and the state transition so two
        // concurrent cancel requests cannot both pass the check-then-act window
        // (BUG-44).
        let mut store = self.tasks.write().await;
        let entry = store
            .get_mut(task_id)
            .ok_or_else(|| A2AError::task_not_found(task_id))?;
        if entry.task.status.state.is_terminal() {
            return Err(A2AError::task_not_cancelable(task_id));
        }
        entry.version += 1;
        entry.task.status.state = TaskState::Canceled;
        entry.task.status.timestamp = Some(chrono::Utc::now());
        Ok(entry.task.clone())
    }

    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        let store = self.tasks.read().await;
        Ok(store.get(task_id).map(|e| e.task.clone()))
    }

    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        let store = self.tasks.read().await;
        let mut tasks: Vec<Task> = store
            .values()
            .filter(|entry| {
                // Filter by context_id if provided
                if let Some(ref ctx_id) = req.context_id {
                    if entry.task.context_id != *ctx_id {
                        return false;
                    }
                }
                // Filter by status if provided
                if let Some(ref status) = req.status {
                    if entry.task.status.state != *status {
                        return false;
                    }
                }
                true
            })
            .map(|e| e.task.clone())
            .collect();

        // Sort by ID for deterministic output
        tasks.sort_by(|a, b| a.id.cmp(&b.id));

        // Apply pagination
        let page_size = match req.page_size {
            Some(size) if size > 0 => size as usize,
            _ => 50,
        };
        let start = if let Some(ref token) = req.page_token {
            // Simple offset-based pagination
            token.parse::<usize>().unwrap_or(0)
        } else {
            0
        };

        let total_size = tasks.len();
        // Make sure start is within bounds
        let start = start.min(total_size);
        let end = start.saturating_add(page_size).min(total_size);
        let page = tasks[start..end].to_vec();

        let next_page_token = if end < total_size {
            Some(end.to_string())
        } else {
            None
        };

        // Apply history length truncation
        let page = page
            .into_iter()
            .map(|mut task| {
                if let Some(ref hl) = req.history_length {
                    let hl = *hl as usize;
                    if let Some(ref mut history) = task.history {
                        if hl == 0 {
                            *history = Vec::new();
                        } else if history.len() > hl {
                            let start = history.len() - hl;
                            *history = history[start..].to_vec();
                        }
                    }
                }
                task
            })
            .collect();

        Ok(ListTasksResponse {
            tasks: page,
            next_page_token: next_page_token.unwrap_or_default(),
            page_size: page_size as i32,
            total_size: total_size as i32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(id: &str, ctx: &str, state: TaskState) -> Task {
        Task {
            id: id.to_string(),
            context_id: ctx.to_string(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    #[tokio::test]
    async fn test_create_and_get() {
        let store = InMemoryTaskStore::new();
        let task = make_task("t1", "c1", TaskState::Submitted);
        let ver = store.create(task.clone()).await.unwrap();
        assert_eq!(ver, 1);

        let got = store.get("t1").await.unwrap().unwrap();
        assert_eq!(got.id, "t1");
        assert_eq!(got.context_id, "c1");
    }

    #[tokio::test]
    async fn test_create_duplicate() {
        let store = InMemoryTaskStore::new();
        let task = make_task("t1", "c1", TaskState::Submitted);
        store.create(task.clone()).await.unwrap();

        let result = store.create(task).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_update() {
        let store = InMemoryTaskStore::new();
        let task = make_task("t1", "c1", TaskState::Submitted);
        store.create(task).await.unwrap();

        let updated = make_task("t1", "c1", TaskState::Working);
        let ver = store.update(updated).await.unwrap();
        assert_eq!(ver, 2);

        let got = store.get("t1").await.unwrap().unwrap();
        assert_eq!(got.status.state, TaskState::Working);
    }

    #[tokio::test]
    async fn test_update_not_found() {
        let store = InMemoryTaskStore::new();
        let task = make_task("t1", "c1", TaskState::Working);
        let result = store.update(task).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_update_terminal_state_rejected() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task("t1", "c1", TaskState::Completed))
            .await
            .unwrap();

        // A terminal task cannot transition to a different state.
        for state in [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::Canceled,
            TaskState::Failed,
        ] {
            let result = store.update(make_task("t1", "c1", state.clone())).await;
            assert!(
                result.is_err(),
                "expected {state:?} update to be rejected"
            );
            assert_eq!(
                result.unwrap_err().code,
                error_code::INVALID_REQUEST,
                "unexpected code for {state:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_update_terminal_same_state_allowed() {
        let store = InMemoryTaskStore::new();
        let mut task = make_task("t1", "c1", TaskState::Canceled);
        store.create(task.clone()).await.unwrap();

        // Re-applying the same terminal state (idempotent re-emission) is fine.
        task.status.message = Some(Message::new(Role::Agent, vec![Part::text("after cancel")]));
        let ver = store.update(task).await.unwrap();
        assert_eq!(ver, 2);
        let got = store.get("t1").await.unwrap().unwrap();
        assert_eq!(got.status.state, TaskState::Canceled);
        assert!(got.status.message.is_some());
    }

    #[tokio::test]
    async fn test_begin_cancel_transitions_non_terminal() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task("t1", "c1", TaskState::Working))
            .await
            .unwrap();

        let task = store.begin_cancel("t1").await.unwrap();
        assert_eq!(task.status.state, TaskState::Canceled);

        // A second cancel on the now-terminal task fails.
        let result = store.begin_cancel("t1").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::TASK_NOT_CANCELABLE);
    }

    #[tokio::test]
    async fn test_begin_cancel_terminal_rejected() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task("t1", "c1", TaskState::Completed))
            .await
            .unwrap();
        let result = store.begin_cancel("t1").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::TASK_NOT_CANCELABLE);
    }

    #[tokio::test]
    async fn test_begin_cancel_not_found() {
        let store = InMemoryTaskStore::new();
        let result = store.begin_cancel("nonexistent").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::TASK_NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_not_found() {
        let store = InMemoryTaskStore::new();
        let got = store.get("nonexistent").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn test_list_filter_by_context() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task("t1", "c1", TaskState::Submitted))
            .await
            .unwrap();
        store
            .create(make_task("t2", "c2", TaskState::Working))
            .await
            .unwrap();
        store
            .create(make_task("t3", "c1", TaskState::Completed))
            .await
            .unwrap();

        let req = ListTasksRequest {
            context_id: Some("c1".to_string()),
            status: None,
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks.len(), 2);
        assert_eq!(resp.total_size, 2);
    }

    #[tokio::test]
    async fn test_list_filter_by_status() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task("t1", "c1", TaskState::Submitted))
            .await
            .unwrap();
        store
            .create(make_task("t2", "c1", TaskState::Working))
            .await
            .unwrap();

        let req = ListTasksRequest {
            context_id: None,
            status: Some(TaskState::Working),
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks.len(), 1);
        assert_eq!(resp.tasks[0].id, "t2");
    }

    #[tokio::test]
    async fn test_list_pagination() {
        let store = InMemoryTaskStore::new();
        for i in 0..5 {
            store
                .create(make_task(&format!("t{i}"), "c1", TaskState::Submitted))
                .await
                .unwrap();
        }

        let req = ListTasksRequest {
            context_id: None,
            status: None,
            page_size: Some(2),
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks.len(), 2);
        assert_eq!(resp.total_size, 5);
        assert!(!resp.next_page_token.is_empty());

        // Get second page
        let req2 = ListTasksRequest {
            page_token: Some(resp.next_page_token),
            page_size: Some(2),
            ..req
        };
        let resp2 = store.list(&req2).await.unwrap();
        assert_eq!(resp2.tasks.len(), 2);
    }

    #[tokio::test]
    async fn test_list_zero_page_size_uses_default_window() {
        let store = InMemoryTaskStore::new();
        for i in 0..3 {
            store
                .create(make_task(&format!("t{i}"), "c1", TaskState::Submitted))
                .await
                .unwrap();
        }

        let req = ListTasksRequest {
            context_id: None,
            status: None,
            page_size: Some(0),
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks.len(), 3);
        assert_eq!(resp.page_size, 50);
    }

    #[tokio::test]
    async fn test_list_history_truncation() {
        let store = InMemoryTaskStore::new();
        let mut task = make_task("t1", "c1", TaskState::Working);
        task.history = Some(vec![
            Message::new(Role::User, vec![Part::text("1")]),
            Message::new(Role::Agent, vec![Part::text("2")]),
            Message::new(Role::User, vec![Part::text("3")]),
        ]);
        store.create(task).await.unwrap();

        let req = ListTasksRequest {
            context_id: None,
            status: None,
            page_size: None,
            page_token: None,
            history_length: Some(1),
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks[0].history.as_ref().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn test_list_out_of_range_page_token_returns_empty_page() {
        let store = InMemoryTaskStore::new();
        for i in 0..3 {
            store
                .create(make_task(&format!("t{i}"), "c1", TaskState::Submitted))
                .await
                .unwrap();
        }

        let req = ListTasksRequest {
            context_id: None,
            status: None,
            page_size: None,
            page_token: Some("999".to_string()),
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();

        assert!(resp.tasks.is_empty());
        assert!(
            resp.next_page_token.is_empty(),
            "must not hand back another token"
        );
        assert_eq!(resp.total_size, 3);
    }
}
