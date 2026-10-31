// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use futures::stream::BoxStream;
use serde_json::Value;
use std::collections::HashMap;

use crate::middleware::{ServiceParams, User};

/// Context passed to the [`AgentExecutor`] for each execution.
///
/// Non-exhaustive: fields are added as the protocol grows (live input is next),
/// and only the handler builds one.
#[non_exhaustive]
pub struct ExecutorContext {
    /// The message that triggered execution. `None` for a cancellation, and for
    /// a request that carried only `artifactUpdates` for an existing task.
    pub message: Option<Message>,
    /// The task ID being executed.
    pub task_id: TaskId,
    /// The existing task, if this is a continuation.
    pub stored_task: Option<Task>,
    /// Context ID grouping related interactions.
    pub context_id: String,
    /// Request metadata.
    pub metadata: Option<HashMap<String, Value>>,
    /// Authenticated user info.
    pub user: Option<User>,
    /// Service parameters (headers/metadata) from the request.
    pub service_params: ServiceParams,
    /// Optional tenant ID.
    pub tenant: Option<String>,
}

impl ExecutorContext {
    /// A context for `task_id` in `context_id` with nothing else set. The
    /// handler builds the real ones; this is for exercising an executor on its
    /// own, as the struct is non-exhaustive and cannot be built literally
    /// outside this crate.
    pub fn new(task_id: impl Into<TaskId>, context_id: impl Into<String>) -> Self {
        ExecutorContext {
            message: None,
            task_id: task_id.into(),
            stored_task: None,
            context_id: context_id.into(),
            metadata: None,
            user: None,
            service_params: ServiceParams::new(),
            tenant: None,
        }
    }

    pub fn with_message(mut self, message: Message) -> Self {
        self.message = Some(message);
        self
    }

    pub fn with_stored_task(mut self, task: Task) -> Self {
        self.stored_task = Some(task);
        self
    }

    pub fn with_metadata(mut self, metadata: HashMap<String, Value>) -> Self {
        self.metadata = Some(metadata);
        self
    }

    pub fn with_user(mut self, user: User) -> Self {
        self.user = Some(user);
        self
    }

    pub fn with_service_params(mut self, service_params: ServiceParams) -> Self {
        self.service_params = service_params;
        self
    }

    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }

    pub fn task_info(&self) -> (TaskId, String) {
        (self.task_id.clone(), self.context_id.clone())
    }
}

/// The user's business logic entry point.
///
/// Implement this trait to handle incoming messages and cancellation requests.
/// The executor returns a stream of events (status updates, artifact updates,
/// messages, or task objects).
#[async_trait::async_trait]
pub trait AgentExecutor: Send + Sync + 'static {
    /// Execute a message. Returns a stream of events.
    fn execute(&self, ctx: ExecutorContext)
    -> BoxStream<'static, Result<StreamResponse, A2AError>>;

    /// Cancel an in-progress task. Returns a stream of events.
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_executor_context_task_info() {
        let ctx = ExecutorContext {
            message: None,
            task_id: "t1".into(),
            stored_task: None,
            context_id: "c1".into(),
            metadata: None,
            user: None,
            service_params: HashMap::new(),
            tenant: None,
        };
        let (tid, cid) = ctx.task_info();
        assert_eq!(tid, "t1");
        assert_eq!(cid, "c1");
    }
}
