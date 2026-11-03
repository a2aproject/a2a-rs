// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use async_trait::async_trait;
use std::collections::{HashMap, VecDeque};
use tokio::sync::RwLock;

use super::apply_history_length;
use super::store::{TaskStore, TaskVersion};
use crate::pagination::resolve_page_size;

struct StoredEntry {
    task: Task,
    version: TaskVersion,
}

#[derive(Default)]
struct Tasks {
    entries: HashMap<TaskId, StoredEntry>,
    /// Terminal task ids, oldest first; tracked only under a limit.
    terminal: VecDeque<TaskId>,
}

impl Tasks {
    /// Records that `id` reached a terminal state, then purges the tasks that
    /// reached one earliest until at most `limit` remain.
    fn retire(&mut self, id: &str, limit: Option<usize>) {
        let Some(limit) = limit else {
            return;
        };
        self.terminal.push_back(id.to_string());
        while self.terminal.len() > limit {
            if let Some(oldest) = self.terminal.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }
}

/// In-memory task store. Contents do not survive restarts.
///
/// Keeps every task for the life of the process unless
/// [`Self::with_terminal_task_limit`] bounds how many finished ones it holds.
pub struct InMemoryTaskStore {
    tasks: RwLock<Tasks>,
    terminal_task_limit: Option<usize>,
}

impl InMemoryTaskStore {
    pub fn new() -> Self {
        InMemoryTaskStore {
            tasks: RwLock::new(Tasks::default()),
            terminal_task_limit: None,
        }
    }

    /// Keep at most `limit` tasks in a terminal state, purging the one that
    /// reached its terminal state earliest first. A purged task is reported
    /// as `TaskNotFoundError`, which §3.3.2 allows for a completed and purged
    /// task. Tasks that are not terminal are never purged.
    pub fn with_terminal_task_limit(mut self, limit: usize) -> Self {
        self.terminal_task_limit = Some(limit);
        self
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
        if store.entries.contains_key(&task.id) {
            return Err(crate::sanitized_internal_error("task already exists"));
        }
        let id = task.id.clone();
        let terminal = task.status.state.is_terminal();
        store
            .entries
            .insert(id.clone(), StoredEntry { task, version: 1 });
        if terminal {
            store.retire(&id, self.terminal_task_limit);
        }
        Ok(1)
    }

    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        let mut store = self.tasks.write().await;
        let entry = store
            .entries
            .get_mut(&task.id)
            .ok_or_else(|| A2AError::task_not_found(&task.id))?;
        // A terminal task's state is immutable. Re-applying the same
        // terminal state is allowed so idempotent re-emission (e.g. cancel
        // flows) keeps working, but a terminal state can never be overwritten
        // by a different state.
        if entry.task.status.state.is_terminal() && entry.task.status.state != task.status.state {
            return Err(A2AError::invalid_request(format!(
                "task {} is in terminal state {:?} and cannot transition to {:?}",
                task.id, entry.task.status.state, task.status.state
            )));
        }
        let retired = !entry.task.status.state.is_terminal() && task.status.state.is_terminal();
        entry.version += 1;
        entry.task = task;
        let version = entry.version;
        if retired {
            let id = entry.task.id.clone();
            store.retire(&id, self.terminal_task_limit);
        }
        Ok(version)
    }

    async fn begin_cancel(&self, task_id: &str) -> Result<Task, A2AError> {
        // Hold the write lock across the check and the state transition so two
        // concurrent cancel requests cannot both pass the check-then-act window
        // window.
        let mut store = self.tasks.write().await;
        let entry = store
            .entries
            .get_mut(task_id)
            .ok_or_else(|| A2AError::task_not_found(task_id))?;
        if entry.task.status.state.is_terminal() {
            return Err(A2AError::task_not_cancelable(task_id));
        }
        entry.version += 1;
        entry.task.status.state = TaskState::Canceled;
        entry.task.status.timestamp = Some(chrono::Utc::now());
        let task = entry.task.clone();
        store.retire(task_id, self.terminal_task_limit);
        Ok(task)
    }

    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        let store = self.tasks.read().await;
        Ok(store.entries.get(task_id).map(|e| e.task.clone()))
    }

    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        let store = self.tasks.read().await;
        let mut tasks: Vec<Task> = store
            .entries
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
                // A task without a status timestamp cannot satisfy an
                // "after" filter, and one stamped at the bound does (the
                // proto: "greater than or equal"). Keep this predicate in the
                // store so the result count and pagination are based on the
                // filtered set.
                if let Some(after) = req.status_timestamp_after {
                    if entry
                        .task
                        .status
                        .timestamp
                        .is_none_or(|timestamp| timestamp < after)
                    {
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
        // Defence in depth: DefaultRequestHandler clamps before calling a
        // store, but a store reached by another route must still bound its
        // own page.
        let page_size = resolve_page_size(req.page_size);
        let start = if let Some(ref token) = req.page_token {
            // Simple offset-based pagination
            token
                .parse::<usize>()
                .map_err(|_| A2AError::invalid_params("invalid page token"))?
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

        let page = page
            .into_iter()
            .map(|mut task| {
                apply_history_length(&mut task, req.history_length);
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
            assert!(result.is_err(), "expected {state:?} update to be rejected");
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
    async fn test_terminal_task_limit_purges_the_oldest_finished_task() {
        let store = InMemoryTaskStore::new().with_terminal_task_limit(2);
        store
            .create(make_task("working", "c1", TaskState::Working))
            .await
            .unwrap();
        for id in ["t1", "t2", "t3"] {
            store
                .create(make_task(id, "c1", TaskState::Working))
                .await
                .unwrap();
            store
                .update(make_task(id, "c1", TaskState::Completed))
                .await
                .unwrap();
        }
        // Re-applying a terminal state does not count the task twice.
        store
            .update(make_task("t3", "c1", TaskState::Completed))
            .await
            .unwrap();

        assert!(store.get("t1").await.unwrap().is_none());
        assert!(store.get("t2").await.unwrap().is_some());
        assert!(store.get("t3").await.unwrap().is_some());
        assert!(store.get("working").await.unwrap().is_some());

        store.begin_cancel("working").await.unwrap();
        assert!(store.get("t2").await.unwrap().is_none());
        assert!(store.get("working").await.unwrap().is_some());

        store
            .create(make_task("t4", "c1", TaskState::Failed))
            .await
            .unwrap();
        assert!(store.get("t3").await.unwrap().is_none());
        assert_eq!(store.tasks.read().await.entries.len(), 2);
    }

    #[tokio::test]
    async fn test_unlimited_store_keeps_every_finished_task() {
        let store = InMemoryTaskStore::new();
        for i in 0..100 {
            let id = format!("t{i}");
            store
                .create(make_task(&id, "c1", TaskState::Completed))
                .await
                .unwrap();
        }
        assert_eq!(store.tasks.read().await.entries.len(), 100);
        assert!(store.tasks.read().await.terminal.is_empty());
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
    async fn test_list_filter_by_status_timestamp_after() {
        let store = InMemoryTaskStore::new();
        let cutoff = chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let mut old = make_task("old", "c1", TaskState::Completed);
        old.status.timestamp = Some(cutoff - chrono::Duration::seconds(1));
        store.create(old).await.unwrap();

        let mut recent = make_task("recent", "c1", TaskState::Completed);
        recent.status.timestamp = Some(cutoff + chrono::Duration::seconds(1));
        store.create(recent).await.unwrap();

        // The bound itself is included: "greater than or equal".
        let mut at_cutoff = make_task("at-cutoff", "c1", TaskState::Completed);
        at_cutoff.status.timestamp = Some(cutoff);
        store.create(at_cutoff).await.unwrap();

        // Missing timestamps do not satisfy an "after" filter.
        store
            .create(make_task("missing", "c1", TaskState::Completed))
            .await
            .unwrap();

        let resp = store
            .list(&ListTasksRequest {
                context_id: None,
                status: None,
                page_size: None,
                page_token: None,
                history_length: None,
                status_timestamp_after: Some(cutoff),
                include_artifacts: None,
                tenant: None,
            })
            .await
            .unwrap();

        assert_eq!(resp.total_size, 2);
        assert_eq!(
            resp.tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["at-cutoff", "recent"]
        );
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

        let empty = store
            .list(&ListTasksRequest {
                history_length: Some(-1),
                ..req
            })
            .await
            .unwrap();
        assert!(empty.tasks[0].history.as_ref().unwrap().is_empty());
    }

    fn make_task_with_history(id: &str, messages: Vec<&str>) -> Task {
        let mut task = make_task(id, "c1", TaskState::Working);
        task.history = Some(
            messages
                .into_iter()
                .map(|m| Message::new(Role::User, vec![Part::text(m)]))
                .collect(),
        );
        task
    }

    #[tokio::test]
    async fn test_list_negative_history_length_returns_empty_history() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task_with_history("t1", vec!["1", "2", "3"]))
            .await
            .unwrap();

        let req = ListTasksRequest {
            context_id: None,
            status: None,
            page_size: None,
            page_token: None,
            history_length: Some(-1),
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks.len(), 1);
        assert_eq!(resp.tasks[0].history.as_ref().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_list_page_size_is_capped() {
        let store = InMemoryTaskStore::new();
        for i in 0..150 {
            store
                .create(make_task(&format!("t{i:03}"), "c1", TaskState::Submitted))
                .await
                .unwrap();
        }

        let req = ListTasksRequest {
            context_id: None,
            status: None,
            page_size: Some(1000),
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks.len(), 100);
        assert_eq!(resp.page_size, 100);
        assert_eq!(resp.total_size, 150);
    }

    #[tokio::test]
    async fn test_list_invalid_page_token_errors() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task("t1", "c1", TaskState::Submitted))
            .await
            .unwrap();

        let req = ListTasksRequest {
            context_id: None,
            status: None,
            page_size: None,
            page_token: Some("not-a-number".into()),
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let result = store.list(&req).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::INVALID_PARAMS);
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
