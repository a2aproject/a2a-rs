// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use crate::middleware::{ServiceParams, User};
use a2a::*;
use futures::stream::BoxStream;
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

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
/// For a new task its first event decides whether a task exists (A2A §3.1.1):
/// a task, status or artifact event creates it, and the server saves it then;
/// a [`Message`] is a direct reply, nothing is saved, and whatever the
/// executor emits afterwards is ignored (it may keep working until it
/// returns). An executor that fails or panics before it has said anything
/// leaves no task; one that ends without an event leaves the task as the
/// request prepared it. Opening a live exchange is the exception: that task
/// is created up front. See [`Agent`] for the same contract as a typed return
/// value.
#[async_trait::async_trait]
pub trait AgentExecutor: Send + Sync + 'static {
    /// Execute a message. Returns an owned stream of events.
    fn execute(&self, ctx: ExecutorContext)
    -> BoxStream<'static, Result<StreamResponse, A2AError>>;

    /// Cancel an in-progress task. Returns a stream of events.
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>>;
}

/// A message with no `taskId`, which an [`Agent`] answers or turns into a task.
#[non_exhaustive]
pub struct NewMessage {
    /// ID shared by this start decision and its subsequent execution.
    pub execution_id: String,
    /// The message.
    pub message: Message,
    /// The ID the task gets if the agent creates one.
    pub task_id: TaskId,
    /// The context the task would belong to.
    pub context_id: String,
    /// Trusted caller resolved by the server's authorizer.
    pub user: Option<User>,
    /// Request metadata.
    pub metadata: Option<HashMap<String, Value>>,
    /// Service parameters (headers/metadata) from the request.
    pub service_params: ServiceParams,
    /// Optional tenant ID.
    pub tenant: Option<String>,
}

impl NewMessage {
    /// A task in `state` with this message's ids, for [`Start::Task`].
    pub fn new_task(&self, state: TaskState) -> Task {
        Task {
            id: self.task_id.clone(),
            context_id: self.context_id.clone(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            timeline: None,
            metadata: None,
            generation: 0,
        }
    }
}

/// What an [`Agent`] makes of a message that names no task (A2A §3.1.1).
// A value built once per request and consumed at once: boxing the variants
// would only make it harder to write.
#[allow(clippy::large_enum_variant)]
pub enum Start {
    /// A one-shot reply. No task exists, and nothing is saved: neither the
    /// client's message nor its artifacts. The call ends with the reply; an
    /// agent that has more to do spawns it.
    Message(Message),
    /// Create this task (its `id` and `context_id` are the ones in the
    /// [`NewMessage`]). The server saves it, with the client's message in its
    /// timeline, and then runs [`Agent::execute`] for the message, as it does
    /// for every message to a task.
    Task(Task),
}

/// An agent that decides, for a message with no `taskId`, whether to answer it
/// at once or create a task, and that works on tasks once they exist.
///
/// [`Agent::start`] makes that one decision. Everything about a task, its first
/// turn included, is [`Agent::execute`]: the task is always there, and the
/// client's messages queue for it only from then on. An agent that never keeps
/// tasks implements `start` alone. [`AgentExecutor`] is the same contract as
/// one stream of events, whose first event decides; it runs on the server
/// unchanged.
#[async_trait::async_trait]
pub trait Agent: Send + Sync + 'static {
    /// A message with no `taskId`: reply, or create the task.
    async fn start(&self, msg: NewMessage) -> Result<Start, A2AError>;

    /// A message to an existing task (`ctx.stored_task` is set): the work of a
    /// turn, as a stream of status and artifact events. The first message of a
    /// task [`Agent::start`] created arrives here too. Fails by default, for an
    /// agent that keeps no tasks.
    fn execute(&self, ctx: ExecutorContext) -> EventStream {
        let _ = ctx;
        Box::pin(futures::stream::once(async {
            Err(A2AError::unsupported_operation("this agent keeps no tasks"))
        }))
    }

    /// Cancels an in-progress task. Returns a stream of events.
    fn cancel(&self, ctx: ExecutorContext) -> EventStream {
        let _ = ctx;
        Box::pin(futures::stream::once(async {
            Err(A2AError::unsupported_operation("this agent keeps no tasks"))
        }))
    }
}

type EventStream = BoxStream<'static, Result<StreamResponse, A2AError>>;

/// An [`Agent`] as the executor the server drives: for a new task the first
/// event is the task, or the reply; the rest is [`Agent::execute`].
pub(crate) struct AgentExecutorAdapter<A: Agent>(pub(crate) Arc<A>);

impl<A: Agent> AgentExecutor for AgentExecutorAdapter<A> {
    fn execute(&self, ctx: ExecutorContext) -> EventStream {
        use futures::StreamExt;

        let agent = Arc::clone(&self.0);
        if !ctx.new_task {
            return agent.execute(ctx);
        }
        Box::pin(
            futures::stream::once(async move {
                let Some(message) = ctx.message.clone() else {
                    return futures::stream::once(async {
                        Err(A2AError::invalid_agent_response())
                    })
                    .boxed();
                };
                let msg = NewMessage {
                    execution_id: ctx.execution_id.clone(),
                    message,
                    task_id: ctx.task_id.clone(),
                    context_id: ctx.context_id.clone(),
                    user: ctx.user.clone(),
                    metadata: ctx.metadata.clone(),
                    service_params: ctx.service_params.clone(),
                    tenant: ctx.tenant.clone(),
                };
                match agent.start(msg).await {
                    Err(error) => futures::stream::once(async move { Err(error) }).boxed(),
                    Ok(Start::Message(message)) => {
                        futures::stream::once(async move { Ok(StreamResponse::Message(message)) })
                            .boxed()
                    }
                    // The task is the one this execution is for.
                    Ok(Start::Task(task))
                        if task.id != ctx.task_id || task.context_id != ctx.context_id =>
                    {
                        futures::stream::once(async { Err(A2AError::invalid_agent_response()) })
                            .boxed()
                    }
                    Ok(Start::Task(task)) => {
                        let mut ctx = ctx;
                        ctx.stored_task = Some(task.clone());
                        futures::stream::once(async move { Ok(StreamResponse::Task(task)) })
                            // The turn starts once the task has been created.
                            .chain(
                                futures::stream::once(async move { agent.execute(ctx) }).flatten(),
                            )
                            .boxed()
                    }
                }
            })
            .flatten(),
        )
    }

    fn cancel(&self, ctx: ExecutorContext) -> EventStream {
        self.0.cancel(ctx)
    }
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
    struct StartOnly;
    #[async_trait::async_trait]
    impl Agent for StartOnly {
        async fn start(&self, message: NewMessage) -> Result<Start, A2AError> {
            Ok(Start::Task(message.new_task(TaskState::Working)))
        }
    }

    #[tokio::test]
    async fn typed_agents_report_unsupported_execution_and_cancellation_by_default() {
        use futures::StreamExt;
        let adapter = AgentExecutorAdapter(Arc::new(StartOnly));
        let mut context = ExecutorContext::new("task", "context", ClientInputs::closed())
            .with_message(Message::new(Role::User, vec![Part::text("input")]));
        context.new_task = true;
        let mut events = adapter.execute(context);
        assert!(
            matches!(events.next().await.unwrap().unwrap(), StreamResponse::Task(task) if task.id == "task")
        );
        assert_eq!(
            events.next().await.unwrap().unwrap_err().code,
            error_code::UNSUPPORTED_OPERATION
        );
        let mut events = adapter.cancel(ExecutorContext::new(
            "task",
            "context",
            ClientInputs::closed(),
        ));
        assert_eq!(
            events.next().await.unwrap().unwrap_err().code,
            error_code::UNSUPPORTED_OPERATION
        );
    }

    struct InvalidStart(bool);
    #[async_trait::async_trait]
    impl Agent for InvalidStart {
        async fn start(&self, message: NewMessage) -> Result<Start, A2AError> {
            if self.0 {
                return Err(A2AError::internal("start refused"));
            }
            let mut task = message.new_task(TaskState::Working);
            task.context_id = "unrelated-context".into();
            Ok(Start::Task(task))
        }
    }

    #[tokio::test]
    async fn typed_start_rejects_missing_messages_foreign_tasks_and_start_failures() {
        use futures::StreamExt;
        for (message, failure) in [(false, false), (true, false), (true, true)] {
            let adapter = AgentExecutorAdapter(Arc::new(InvalidStart(failure)));
            let mut context = ExecutorContext::new("task", "context", ClientInputs::closed());
            context.new_task = true;
            if message {
                context.message = Some(Message::new(Role::User, vec![Part::text("input")]));
            }
            let mut events = adapter.execute(context);
            let error = events.next().await.unwrap().unwrap_err();
            assert_eq!(
                error.code,
                if failure {
                    error_code::INTERNAL_ERROR
                } else {
                    error_code::INVALID_AGENT_RESPONSE
                }
            );
            assert!(events.next().await.is_none());
        }
    }
}
