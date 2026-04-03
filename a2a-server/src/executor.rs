// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use futures::stream::BoxStream;
use serde_json::Value;
use std::collections::HashMap;

use crate::middleware::{ServiceParams, User};

/// Context passed to the [`AgentExecutor`] for each execution.
pub struct ExecutorContext {
    /// The message that triggered execution (None for cancellation without message).
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
