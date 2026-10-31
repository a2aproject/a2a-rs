// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use crate::middleware::{ServiceParams, User};
use a2a::*;
use futures::stream::BoxStream;
use serde_json::Value;
use std::collections::HashMap;

mod inputs;
pub use inputs::{ClientInput, ClientInputSender, ClientInputs};
pub(crate) use inputs::{QueuedInput, ReturnedInputs};

/// Context passed to the [`AgentExecutor`] for each execution.
///
/// Non-exhaustive: fields are added as the protocol grows, and only the
/// handler builds one. Inputs are owned and can move into a worker task.
/// Once the execution stream ends the old inbox stops yielding messages; unread
/// inputs remain available to the server for the next invocation.
///
/// ```no_run
/// use a2a_server::ExecutorContext;
/// fn detach(mut ctx: ExecutorContext) {
///     tokio::spawn(async move { ctx.inputs.recv().await });
/// }
/// ```
#[non_exhaustive]
pub struct ExecutorContext {
    /// Server-generated ID of this execution, for tracing and worker correlation.
    /// Inputs read by this execution share its ID; a later execution for the
    /// same task gets a new ID. This is local SDK context, not an A2A wire field.
    /// Cancellation shares the active execution's ID, or gets a fresh ID when idle.
    pub execution_id: String,
    /// The message that triggered execution. `None` for a cancellation, and for
    /// a request that carried only `artifactUpdates` for an existing task.
    pub message: Option<Message>,
    /// The task ID being executed.
    pub task_id: TaskId,
    /// The existing task, if this is a continuation. For a new task it is
    /// `None`, except on a blocking `SendMessage`, where it is the task as the
    /// request prepared it (not saved yet: the executor's first event creates it).
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
    /// Client messages queued for this task while this execution runs, on a
    /// live stream or sent to the working task. The executor can read them
    /// alongside its own work. Messages left unread at the end of a nonterminal
    /// turn are handed to the next execution.
    pub inputs: ClientInputs,
    /// Whether this message creates the task, which has not existed until now.
    pub new_task: bool,
}

impl ExecutorContext {
    /// A context for `task_id` in `context_id`, owning `inputs`. The
    /// handler builds the real ones; this is for exercising an executor on its
    /// own, as the struct is non-exhaustive and cannot be built literally
    /// outside this crate.
    pub fn new(
        task_id: impl Into<TaskId>,
        context_id: impl Into<String>,
        inputs: ClientInputs,
    ) -> Self {
        ExecutorContext {
            execution_id: uuid::Uuid::now_v7().to_string(),
            message: None,
            task_id: task_id.into(),
            stored_task: None,
            context_id: context_id.into(),
            metadata: None,
            user: None,
            service_params: ServiceParams::new(),
            tenant: None,
            inputs,
            new_task: false,
        }
    }

    pub fn with_inputs(mut self, inputs: ClientInputs) -> Self {
        self.inputs = inputs;
        self
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
///
#[async_trait::async_trait]
pub trait AgentExecutor: Send + Sync + 'static {
    /// Execute a message. Returns an owned stream of events.
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
        let inputs = ClientInputs::closed();
        let ctx = ExecutorContext {
            execution_id: uuid::Uuid::now_v7().to_string(),
            message: None,
            task_id: "t1".into(),
            stored_task: None,
            context_id: "c1".into(),
            metadata: None,
            user: None,
            service_params: HashMap::new(),
            tenant: None,
            inputs,
            new_task: false,
        };
        let (tid, cid) = ctx.task_info();
        assert_eq!(tid, "t1");
        assert_eq!(cid, "c1");
    }
}
