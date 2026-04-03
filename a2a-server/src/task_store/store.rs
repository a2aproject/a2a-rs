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
}
