# The server

A server is three pieces: your executor (the agent's logic), the handler that
orchestrates it, and a store that keeps the tasks. The [quickstart](../quickstart/agent/index.md)
walks through writing an executor; this page covers the other two and the
lifecycle they implement.

## The executor and the handler

The executor is the agent's logic: an
[`AgentExecutor`](https://docs.rs/a2a-server-lf/latest/a2a_server/executor/trait.AgentExecutor.html)
has two methods, `execute` and `cancel`, and both return a stream of events over
an
[`ExecutorContext`](https://docs.rs/a2a-server-lf/latest/a2a_server/executor/struct.ExecutorContext.html)
describing the message, the task and context ids, the stored task, and the
caller. The [quickstart](../quickstart/agent/executor.md) writes one.

[`DefaultRequestHandler`](https://docs.rs/a2a-server-lf/latest/a2a_server/handler/struct.DefaultRequestHandler.html)
runs the executor and applies what it emits to the store. Construct it once with
the two pieces and share it:

```rust title="examples/src/quickstart/server.rs"
--8<-- "examples/src/quickstart/server.rs:main"
```

Both routes take a clone of the same handler, so one task table serves every
binding you mount. Nothing in the executor knows which binding a request arrived
on.

## The task lifecycle

[`TaskState`](https://docs.rs/a2a-lf/latest/a2a/types/enum.TaskState.html) has
nine values. Four are terminal: the handler refuses to change a task once it
reaches one.

| State | Terminal | Meaning |
| --- | --- | --- |
| `Submitted` | no | The task exists and has not started. |
| `Working` | no | The executor is running. |
| `InputRequired` | no | The task is waiting for the client to send more. |
| `AuthRequired` | no | The task needs the caller to authenticate. |
| `Completed` | yes | The task finished and its output is final. |
| `Failed` | yes | The executor reported a failure. |
| `Canceled` | yes | A cancel request was honoured. |
| `Rejected` | yes | The agent declined the task. |
| `Unspecified` | no | No state was set. |

`TaskState::is_terminal` is the check the server uses; it returns true for
`Completed`, `Failed`, `Canceled` and `Rejected`. For the protocol-level
description of how a task moves between these states, including what a client
sees at each step, see
[Life of a task](https://a2a-protocol.org/latest/topics/life-of-a-task/) on the
A2A site.

### How the handler drives a task

The table is the protocol's model. What the handler does with an executor's
events is narrower, and worth knowing before you write one:

- **The task starts as `Submitted`.** The handler creates and persists it before
  `execute` is called, then hands the executor that snapshot in the context.
- **The first terminal event ends the run.** When an emitted event is terminal,
  the handler stops reading the stream, so a later event from the same execution
  is discarded rather than applied.
- **A failure makes the task `Failed`.** An error from the executor's stream, a
  failed store write, or a stream that mixes a `Message` with task events all end
  the same way: the handler persists `Failed` with a sanitised message and
  publishes it to subscribers. A streaming response is one `Message` or a `Task`
  and its updates, never both (section 3.1.2 of the specification).
- **A terminal task takes no new message.** `SendMessage` for a task that has
  already reached a terminal state is refused with "has already reached a
  terminal state" instead of restarting it.

[`InMemoryTaskStore`](https://docs.rs/a2a-server-lf/latest/a2a_server/task_store/struct.InMemoryTaskStore.html)
keeps every task it is given for the life of the process.
`InMemoryTaskStore::with_terminal_task_limit` bounds how many finished ones it
retains; that is retention, not a limit on work in flight.

## The store

Every task the server keeps lives in a
[`TaskStore`](https://docs.rs/a2a-server-lf/latest/a2a_server/task_store/trait.TaskStore.html).
The trait is small, and a custom implementation must provide all of it:

```rust title="a2a-server/src/task_store/store.rs"
--8<-- "a2a-server/src/task_store/store.rs:task-store"
```

Three things in that signature are worth more than a skim:

- **`create` and `update` return a `TaskVersion`.** Nothing in the SDK reads it
  today: `save_task` and `start_execution` discard the value, and `StoredTask` is
  used only inside its own module. It is the hook a store that needs optimistic
  concurrency would build on, not a counter the handler enforces.
- **`begin_cancel` exists to close a race.** Checking that a task is not
  terminal and then writing the transition is a check-then-act race between two
  concurrent cancels. The method must perform both under one lock or one
  transaction; `InMemoryTaskStore` holds its write lock across both.
- **`get` returns `Option`, not an error, for a missing task.** Absence is a
  normal answer; an error means the store itself failed.

[`InMemoryTaskStore`](https://docs.rs/a2a-server-lf/latest/a2a_server/task_store/struct.InMemoryTaskStore.html)
is the one to start with: it is correct, and it loses every task when the process
ends. Reach for a custom store when tasks must outlive a restart, or when more
than one instance must see the same tasks.

## Task listing and pagination

Task listing is the part of the trait easiest to get subtly wrong. The server
bounds every page before it reaches the store:

```rust title="a2a-server/src/pagination.rs"
--8<-- "a2a-server/src/pagination.rs:page-size"
```

A store that ignores `req.page_size` does not merely return a large page: it
**loses tasks**. The handler clamps the request to
[`MAX_PAGE_SIZE`](https://docs.rs/a2a-server-lf/latest/a2a_server/pagination/constant.MAX_PAGE_SIZE.html)
and truncates whatever comes back to that bound, because it cannot synthesise a
continuation token for a paging scheme it does not own. Honour `page_size`, and
set `next_page_token` whenever more tasks remain.

## API reference

| Type | Crate | Reference |
| --- | --- | --- |
| `AgentExecutor`, `ExecutorContext` | `a2a-server-lf` | [`a2a_server::executor`](https://docs.rs/a2a-server-lf/latest/a2a_server/executor/index.html) |
| `DefaultRequestHandler` | `a2a-server-lf` | [`a2a_server::handler`](https://docs.rs/a2a-server-lf/latest/a2a_server/handler/index.html) |
| `TaskStore`, `InMemoryTaskStore` | `a2a-server-lf` | [`a2a_server::task_store`](https://docs.rs/a2a-server-lf/latest/a2a_server/task_store/index.html) |
| `TaskState` | `a2a-lf` | [`a2a::types`](https://docs.rs/a2a-lf/latest/a2a/types/index.html) |

## What this page leaves out

A worked custom store (a SQLite or Postgres `TaskStore`, including a `list`
that honours `page_size`) is not here; [#530](https://github.com/a2aproject/a2a-rs/issues/530)
stays open for it. The executor's own walkthrough, including the events it
returns, is in the [quickstart](../quickstart/agent/executor.md).
