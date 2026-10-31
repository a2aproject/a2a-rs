// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
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
///
/// # Concurrency
///
/// `update` takes no expected version: a write replaces what is stored. The
/// default handler serialises the changes to a task with one writer lock per
/// task, and that lock is per process. Several server processes sharing one
/// store are therefore not protected from each other: a store that is shared
/// should make `update` and `begin_cancel` atomic itself, for example by
/// checking the version of the task it replaces.
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

    /// List only tasks visible to this caller. Apply the predicate before
    /// counting, ordering and pagination. Tokens must refer only to visible tasks.
    ///
    /// Stores used with a request authorizer must override this method. The
    /// default fails closed so existing stores cannot leak tasks through lists.
    async fn list_scoped(
        &self,
        _req: &ListTasksRequest,
        _visible: &(dyn for<'id> Fn(&'id str) -> bool + Send + Sync),
    ) -> Result<ListTasksResponse, A2AError> {
        Err(A2AError::unsupported_operation(
            "store does not support authorized task listing",
        ))
    }

    /// Atomically check that a task is not in a terminal state and transition
    /// it to `CANCELED`, returning the updated task.
    ///
    /// `TASK_NOT_CANCELABLE` if the task is already terminal,
    /// `TASK_NOT_FOUND` if it does not exist.
    ///
    /// A store that tracks generations should advance the generation and
    /// append the `CANCELED` status to the timeline and close open artifacts
    /// at that generation in the same write, so no
    /// reader sees the task canceled at its old generation. `DefaultRequestHandler`
    /// then announces that entry as the one status event of the new generation.
    /// A store that does not (the generation comes back unchanged) is
    /// recorded by the handler instead, a moment later.
    ///
    /// Deliberately has no default implementation. A default could only read
    /// and then write, which is the check-then-act race this method exists to
    /// close, and an implementor who did not notice would inherit it silently.
    /// Serialise the check and the transition -- see `InMemoryTaskStore`, which
    /// holds its write lock across both.
    async fn begin_cancel(&self, task_id: &str) -> Result<Task, A2AError>;
}
