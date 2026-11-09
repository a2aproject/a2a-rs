# a2a-server-lf

Async server framework for implementing A2A v1 agents in Rust.

This crate is published as `a2a-server-lf` and imported in Rust as `a2a_server`.

## What It Provides

- REST and JSON-RPC routers built on `axum`
- A transport-agnostic `RequestHandler` abstraction
- A default handler and in-memory task store
- SSE helpers for streaming task responses

## Task Service

`DefaultRequestHandler` automatically wraps your `TaskStore` in an internal
`TaskService`, shared by handler clones. The service owns task coordination,
subscriptions, executor routing, and change notifications. Unary, streaming,
and live sends admit messages and artifact updates through this service;
subscriptions attach to task events without starting execution.

Unary sends first resolve to a direct message or a task response. A direct
message returns immediately; task responses wait for completion or interruption
when blocking is requested. Streaming and live sends share message admission,
with their transport setup handled separately.

Blocking `SendMessage` has no deadline by default. Use
`with_blocking_response_timeout(Duration::from_secs(60))` to bound waiting after
input admission. Expiry returns `INTERNAL_ERROR`; admitted task execution
continues. The deadline does not apply to `returnImmediately`, streaming, or live
responses. Under A2A 1.1, streaming and live responses remain open across
`INPUT_REQUIRED` and `AUTH_REQUIRED`, and close when the task becomes terminal.

Custom stores keep implementing the existing persistence trait. They do not
need to implement subscriptions or manage channels. Changes made through the
service are persisted before publication; writes made directly to a custom
store outside the handler do not produce local subscription events. The
default event routing remains process-local.

HTTP live follow-up routing and acknowledgements are handled separately from
task operations. The response `Run` owns its input stream and optional HTTP
registration lease, unregistering before releasing the stream on exit or panic.
`AgentExecutor` and its owned `ctx.inputs` API are unchanged.

## Shutdown

After stopping incoming transport requests, call `handler.shutdown().await`.
Shutdown affects every clone, rejects new work, and waits for SDK-owned execution,
response, task-queue, cancellation, and webhook futures to be dropped. It closes
live registrations and pending HTTP deliveries. Persisted tasks retain their last
committed state and can be resumed by a new handler through startup recovery.
Shutdown is permanent and may interrupt an admitted operation; it does not roll
back writes or mark unfinished tasks canceled.

Executors' direct `Message` replies finish their execution: the SDK drops the
output stream immediately. Webhook configuration lookups have a 30-second
deadline. Application-spawned tasks remain the application's responsibility;
shutdown is cooperative and cannot interrupt code blocking a Tokio worker thread.

## Startup Recovery

After configuring a handler backed by a persistent `TaskStore`, call
`handler.restart_tasks().await?` at application startup, before serving requests.
It scans all pages of nonterminal tasks and returns the number whose executors
accepted recovery. Tasks already running in this handler and terminal tasks are
skipped. Nothing runs automatically from the synchronous handler constructor.

Executors opt in by implementing `AgentExecutor::restart(ctx)` and returning
`Some(event_stream)`. Typed agents can implement `Agent::restart(ctx)` instead.
The default returns `None`, leaving the task idle. Recovery uses the normal
executor driver, input delivery, event validation, persistence, and cancellation.

The context contains the stored task, a fresh execution ID and a fresh input
receiver. It has no triggering message, caller identity, credentials, request
metadata, or tenant context. The SDK does not replay task history as new input.
The hook must use application checkpoints stored in task metadata, artifacts,
or another durable store to decide what work remains. In-memory input receipts
and application worker state do not survive a process restart.

This is recovery within one handler process, not a distributed worker lease or
an exactly-once guarantee for external side effects. Multiple servers sharing
a store need application-level ownership. The in-memory store cannot preserve
tasks across a real process restart.

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
the task service owns the bookkeeping without a receiver mutex or an
input worker task. At invocation end the old inbox is
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

Message preparation is owned by its task service after enqueueing. A caller
disconnecting while storage commits input cannot interrupt executor delivery.
Inline push configuration is validated and admitted before input is persisted.
This does not provide a durable outbox across process crashes or transactions
across independent task and configuration stores.

Task broadcast subscribers enqueue webhook deliveries into one handler-wide
dispatcher. One Tokio task polls up to 16 concurrent HTTP request futures and
preserves order per Task/URL. Its ingress queue and pending queue each hold at
most 1024 deliveries; overflow drops the new delivery and follows `fail_on_error`.
There are no per-endpoint worker tasks, and accepted deliveries keep their Task
event source alive until they drain. All webhook redirects must keep the original
scheme, host, and effective port, including when `validate_urls` is disabled for
local testing. Configure the final HTTPS URL directly when an endpoint upgrades
HTTP to HTTPS.

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
