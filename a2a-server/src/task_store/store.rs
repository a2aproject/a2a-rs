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

/// Trusted caller identity used only for cursor ownership and resource limits.
/// This does not replace the authorization predicate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TaskListScope {
    pub tenant: Option<String>,
    pub principal: Option<(bool, String)>,
}

/// Interface for persisting and retrieving tasks.
///
/// # Concurrency
///
/// `update` takes no expected version: a write replaces what is stored. The
/// default handler wraps the store in a task service which serialises changes
/// per task within a single process. Several server processes sharing one
/// store are therefore not protected from each other:
/// a shared store should make `update` and `begin_cancel` atomic itself, for example by
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

    /// List with caller-scoped cursor accounting. Existing implementations retain
    /// their listing behavior; stores with cursor pools should override this.
    async fn list_for_scope(
        &self,
        req: &ListTasksRequest,
        _scope: &TaskListScope,
        visible: Option<&(dyn for<'id> Fn(&'id str) -> bool + Send + Sync)>,
    ) -> Result<ListTasksResponse, A2AError> {
        match visible {
            Some(visible) => self.list_scoped(req, visible).await,
            None => self.list(req).await,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::task_store::InMemoryTaskStore;

    // Deliberately implements only the original required interface.
    struct LegacyStore(InMemoryTaskStore);
    #[async_trait]
    impl TaskStore for LegacyStore {
        async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.0.create(task).await
        }
        async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.0.update(task).await
        }
        async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
            self.0.get(id).await
        }
        async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
            self.0.list(req).await
        }
        async fn begin_cancel(&self, id: &str) -> Result<Task, A2AError> {
            self.0.begin_cancel(id).await
        }
    }

    #[tokio::test]
    async fn scoped_listing_preserves_custom_store_compatibility_and_fails_closed() {
        let store = LegacyStore(InMemoryTaskStore::new());
        let request = serde_json::from_value(serde_json::json!({})).unwrap();
        let scope = TaskListScope {
            tenant: None,
            principal: Some((true, "alice".into())),
        };
        let task = Task {
            id: "legacy".into(),
            context_id: "context".into(),
            generation: 0,
            timeline: None,
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        store.create(task.clone()).await.unwrap();
        store.update(task.clone()).await.unwrap();
        assert_eq!(store.get("legacy").await.unwrap(), Some(task));
        assert_eq!(
            store
                .list_for_scope(&request, &scope, None)
                .await
                .unwrap()
                .tasks[0]
                .id,
            "legacy"
        );
        assert_eq!(
            store.begin_cancel("legacy").await.unwrap().status.state,
            TaskState::Canceled
        );
        assert_eq!(
            store
                .list_for_scope(&request, &scope, Some(&|_| true))
                .await
                .unwrap_err()
                .code,
            error_code::UNSUPPORTED_OPERATION
        );
    }
}
