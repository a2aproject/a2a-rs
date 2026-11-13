# The executor

The executor is the agent's logic. You implement the `AgentExecutor` trait, and the server calls it for every message and every cancel request. Both methods return a stream of events. The server turns the events into the task's state and into responses for the client.

```rust title="examples/src/quickstart/agent.rs"
--8<-- "examples/src/quickstart/agent.rs:executor"
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
