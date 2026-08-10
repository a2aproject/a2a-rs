// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use async_trait::async_trait;

/// Version counter for optimistic concurrency control.
pub type TaskVersion = u64;

/// A task stored with version metadata.
#[derive(Debug, Clone)]
pub struct StoredTask {
    pub task: Task,
    pub version: TaskVersion,
}

/// Interface for persisting and retrieving tasks.
#[async_trait]
pub trait TaskStore: Send + Sync + 'static {
    /// Create a new task. Returns the initial version.
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError>;

    /// Update an existing task. Returns the new version.
    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError>;

    /// Get a task by ID.
    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError>;

    /// List tasks matching the request criteria.
    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError>;

    /// Atomically check that a task is not in a terminal state and transition
    /// it to `CANCELED`, returning the updated task.
    ///
    /// Used to eliminate the check-then-act TOCTOU race between concurrent
    /// cancel requests (BUG-44). Fails with `TASK_NOT_CANCELABLE` if the task
    /// is already terminal, and `TASK_NOT_FOUND` if it does not exist.
    ///
    /// The default implementation performs a non-atomic read + update; stores
    /// with locking should override it so the check and the transition are
    /// serialized.
    async fn begin_cancel(&self, task_id: &str) -> Result<Task, A2AError> {
        let task = self
            .get(task_id)
            .await?
            .ok_or_else(|| A2AError::task_not_found(task_id))?;
        if task.status.state.is_terminal() {
            return Err(A2AError::task_not_cancelable(task_id));
        }
        let mut task = task;
        task.status.state = TaskState::Canceled;
        task.status.timestamp = Some(chrono::Utc::now());
        self.update(task.clone()).await?;
        Ok(task)
    }
}
