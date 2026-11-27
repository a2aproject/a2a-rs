# The executor

The executor is the agent's logic. You implement the `AgentExecutor` trait, and the server calls it for every message and every cancel request. Both methods return a stream of events. The server turns the events into the task's state and into responses for the client.

```rust title="examples/src/quickstart/agent.rs"
/// The agent's logic.
pub struct Greeter;

impl AgentExecutor for Greeter {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (task_id, context_id) = ctx.task_info();
        let text = ctx
            .message
            .as_ref()
            .and_then(Message::text)
            .unwrap_or_default()
            .trim()
            .to_string();
        // A task the agent left waiting for input is being answered.
        let answering = matches!(
            &ctx.stored_task,
            Some(task) if task.status.state == TaskState::InputRequired
        );

        let events = if text.eq_ignore_ascii_case("hello") && !answering {
            // No name yet: ask for one and wait for the next message.
            vec![status(
                &task_id,
                &context_id,
                TaskState::InputRequired,
                Some("Who should I greet?"),
            )]
        } else {
            // Greet in pieces, so that a streaming client sees them arrive.
            vec![
                status(&task_id, &context_id, TaskState::Working, None),
                chunk(&task_id, &context_id, "Hello, ", false, false),
                chunk(&task_id, &context_id, &text, true, false),
                chunk(&task_id, &context_id, "!", true, true),
                status(&task_id, &context_id, TaskState::Completed, None),
            ]
        };

        Box::pin(stream::iter(events).then(|event| async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(event)
        }))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (task_id, context_id) = ctx.task_info();
        let canceled = status(&task_id, &context_id, TaskState::Canceled, None);
        Box::pin(stream::once(async move { Ok(canceled) }))
    }
}

/// A status update for the task, with an optional message from the agent.
fn status(
    task_id: &str,
    context_id: &str,
    state: TaskState,
    message: Option<&str>,
) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state,
            message: message.map(|text| Message::new(Role::Agent, vec![Part::text(text)])),
            timestamp: None,
        },
        metadata: None,
    })
}

/// One piece of the `greeting` artifact. `append` adds it to the pieces sent
/// before; `last` marks the end of the artifact.
fn chunk(task_id: &str, context_id: &str, text: &str, append: bool, last: bool) -> StreamResponse {
    StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
        task_id: task_id.to_string(),
        context_id: context_id.to_string(),
        artifact: Artifact {
            artifact_id: "greeting".to_string(),
            name: None,
            description: None,
            parts: vec![Part::text(text)],
            metadata: None,
            extensions: None,
        },
        append: Some(append),
        last_chunk: Some(last),
        metadata: None,
    })
}
```

## What the executor gets

`ExecutorContext` describes the request. The greeter uses three of its parts:

- `message` is the message from the client.
- `task_info()` returns the ids of the task and of its context. Every event carries them.
- `stored_task` is the task as the server currently has it. For a new task it may already exist, in `Submitted` state, so check its state rather than whether it is `Some`. The greeter checks for `InputRequired`, which is how it knows that a message answers its question.

## What the executor returns

A stream of events. The greeter sends two kinds:

- A **status update** moves the task to a new state: `Working`, `InputRequired`, `Completed` or `Canceled`. `Completed`, `Failed`, `Canceled` and `Rejected` end the task; `InputRequired` leaves it open.
- An **artifact update** carries output. The greeter sends the artifact `greeting` in three pieces. The first creates it, the others set `append` to add to it, and the last sets `last_chunk`. A client that does not stream receives the three pieces together, as the parts of one artifact.

The greeter takes one of two paths:

1. The message is `hello` and the task is not waiting for an answer. It sets `InputRequired` with the question "Who should I greet?", and the task stays open.
2. Anything else is a name, whether it is the first message or the answer to the question. It works, sends the greeting in pieces and completes.

The `sleep` before each event only makes the streaming visible. Without it, the answer arrives at once.

## Cancel

`cancel` is called when a client cancels a task. The greeter reports the task `Canceled`.

Next, [start the server](start-the-server.md).
