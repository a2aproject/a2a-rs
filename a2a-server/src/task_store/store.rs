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
    ///
    /// An implementation must honour `req.page_size` and set
    /// `next_page_token` when more tasks remain. `DefaultRequestHandler`
    /// clamps `page_size` to
    /// [`MAX_PAGE_SIZE`](crate::pagination::MAX_PAGE_SIZE) before calling
    /// this, and truncates the response to that bound if more comes back --
    /// so a store that ignores `page_size` will have its extra tasks
    /// dropped, and the caller loses them, because the handler cannot
    /// synthesise a continuation token for a paging scheme it does not own.
    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError>;
}
