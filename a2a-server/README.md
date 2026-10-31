# a2a-server-lf

Async server framework for implementing A2A v1 agents in Rust.

This crate is published as `a2a-server-lf` and imported in Rust as `a2a_server`.

## What It Provides

- REST and JSON-RPC routers built on `axum`
- A transport-agnostic `RequestHandler` abstraction
- A default handler and in-memory task store
- SSE helpers for streaming task responses

## Multiple Live Clients

`ctx.execution_id` is a server-generated identifier for one execution. It stays
the same while that execution reads queued inputs and changes when the same
task starts another execution, including unread-input handoff. Capture it
alongside `ctx.task_id` when spawning workers or recording traces. Typed
`Agent::start` receives the same ID in `NewMessage`; cancellation receives the
active execution's ID, or a fresh ID when no execution is active. This ID is SDK
context and is not added to protocol task objects or events.

`AgentExecutor` keeps the signatures from `main`: an owned `ExecutorContext` and
`BoxStream<'static, Result<StreamResponse, A2AError>>` for both `execute` and
`cancel`. `ctx.inputs` is an owned receiver and may move into a Tokio worker.
The server tracks receipt of queued messages using atomic delivery receipts;
it reuses the existing execution-state lock for bookkeeping and adds neither
a receiver mutex nor an input worker task. At invocation end the old inbox is
revoked: workers wake with `None`, and unread input is handed to the next
invocation or rejected if the task ended. Application workers remain responsible
for finishing work on messages they already received.

Authorized clients can attach independent live exchanges to the same task. Their
messages share the active executor's bounded input queue, and task updates reach
every attached downlink. Request rejections, including unread input when a task
ends, go only to the live response that accepted the input.

Implement `RequestAuthorizer::authenticated_user` to provide trusted caller
information after authorization. Its default returns `None`; the SDK never
constructs a principal from message metadata or an unvalidated user-name header.
`ExecutorContext.user` describes the request that started that invocation.
Each `ClientInput` carries its own `user`, `service_params`, request `metadata`,
and `tenant`. These fields remain attached when unread input starts a later
invocation, so it uses that input's caller rather than the previous invocation's.
Service parameters can contain credentials and should not be logged wholesale.

With a request authorizer, task listing uses `TaskStore::list_scoped`. Custom
stores must implement it and apply the supplied visibility predicate before
counting or paginating. The default rejects authorized listing rather than
exposing unfiltered tasks. The in-memory store orders by descending status
timestamp, with task IDs breaking ties, and returns opaque cursor tokens.
Previous numeric offset tokens are no longer accepted.

Required extensions declared in the handler's capabilities must be present in
the request's `A2A-Extensions` header. Normal client messages require a message
ID, user role and at least one part. Artifact-only input uses an empty
`Role::Unspecified` message so it is omitted on the wire; its updates name the
existing task.

Message preparation runs in an owned Tokio task. A caller disconnecting while
storage is committing input cannot interrupt preparation before executor delivery.
Inline push configuration is validated and admitted before input is persisted.
This does not provide a durable outbox across process crashes or transactions
across independent task and configuration stores.

Executor events must match their execution's task and context before persistence
or publication. Snapshot rebasing preserves recorded message history and the
latest client-owned artifacts. Cancellation completes open artifacts at its
recorded generation. Invalid stored webhooks respect the sender's `fail_on_error`
setting, and a rejected endpoint does not skip later endpoints.

HTTP clients can send the optional `x-a2a-rs-live-exchange` header, exported as
`a2a::SDK_LIVE_EXCHANGE_HEADER`. Use a distinct identifier for each exchange and
reuse it on that exchange's uplink requests. It also binds input that omits a
task ID to the existing exchange's task. The Rust REST and JSON-RPC clients manage
this automatically. The identifier is routing information, not an authentication
credential; authorization still applies to every request, and an identified
exchange cannot be used by a different authenticated caller.

This header is a Rust SDK transport convention, not part of the A2A 1.1 standard.
Without it, an input can join a single unambiguous, untagged exchange for the same
authenticated caller. If several match, the server refuses the call instead of
choosing one arbitrarily. Anonymous callers and authorization-only hooks provide
no caller distinction; independent clients sharing an identity need distinct
exchange identifiers. Opening with `subscribe` creates an observing downlink.
The exchange registry is local to the handler process; deployments with multiple
replicas must route an exchange's HTTP calls to the same process.

## Install

```sh
cargo add a2a-lf --rename a2a
cargo add a2a-server-lf --rename a2a-server
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/a2aproject/a2a-rs
- Workspace README: https://github.com/a2aproject/a2a-rs/blob/main/README.md
