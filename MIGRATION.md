# Migrating to the A2A 1.1 API

This stack changes the public Rust model as well as the protocol binding.
Update custom transports, handlers and struct literals together.

## Requests and wire types

- `SendMessageRequest.message` is `Option<Message>`. Wrap ordinary messages
  in `Some(message)`. Use `None` with `artifact_updates` for an artifact-only
  update to an existing task. Do not substitute an empty message: a present
  message must pass the normal role, ID and content checks.
- Public request and task structs have new generation and timeline fields.
  Existing struct literals must initialize them. Optional request fields can
  normally start as `None`; event and task generations start at `0` until the
  server assigns their committed generation. Use existing constructors where
  available rather than copying wire-model struct literals.
- `history_length` limits message history; `timeline_length` limits timeline
  entries. They affect returned snapshots, not persisted history. A timeline
  includes status transitions and messages, while artifact changes have
  generation bounds on the artifact itself.
- `StreamResponse` and `ExecutorContext` are `#[non_exhaustive]`. Add a
  wildcard arm to external `StreamResponse` matches. Construct executor
  contexts with their constructors rather than external struct literals.
- Unknown timeline content is represented by `TimelineContent::Unknown`.
  Its generation is retained so a receiver can track progress without knowing
  a future payload kind. Exhaustive matches must handle this variant.
- `a2a::VERSION` is now `"1.1"`. Send `A2A-Version: 1.1` for 1.1 features.
  Every wire binding requires a single `A2A-Version` value selecting `1.0`
  or `1.1`; a numeric patch component is tolerated and ignored. Bare `1`,
  `1.2` and later versions, and duplicate version headers are rejected.
  A missing header means legacy `0.3`, which every binding rejects.
  This also applies to custom SLIM RPC callers.
  Bindings use the 1.1 protobuf/ProtoJSON envelope and enum names;
  legacy 0.3 stream events are rejected rather than silently skipped.
  JSON-RPC response envelopes must declare `"jsonrpc": "2.0"`.

## Executors and handlers

`AgentExecutor::execute` keeps the owned-context API:

```rust
fn execute(
    &self,
    ctx: ExecutorContext,
) -> BoxStream<'static, Result<StreamResponse, A2AError>>;
```

Executors may return a standalone agent `Message`, or create/update a task
through their output stream. Once task output begins, subsequent output must
belong to that task. A standalone message closes its response stream. A task
may remain nonterminal after its output stream ends; a fresh input can start
another execution. `execution_id` identifies an invocation, not a client or task.
The owned input receiver can move into a spawned Tokio worker.

Input queues belong to one execution. If that execution ends before reading an
accepted input, a waiting sending call receives an explicit rejection (a
private `StreamError` on a live exchange). The input remains in persisted task
history but is never automatically replayed into another `execute` call. A
fresh request after completion may start another execution. An answer to an
outstanding interrupt waits uncommitted until the current executor claims it.
If the stream ends first, the original request starts a fresh execution and is
persisted once; no accepted inbox message is handed off. Deferred requests
retain bounded inbox reservations and publication capacity. Uncommitted offers
use one replaceable notification slot, so expiry or caller disconnect releases
capacity even if the executor never reads. The read-admission deadline defaults
to 30 seconds and is configured with `with_input_admission_timeout`; expiry
reports that input was not committed. Artifact-only input and cancellation stay
behind earlier deferred requests; a cancellation may wait for their remaining
read-admission budget.

A pending `recv` owns its read claim. Canceling that future withdraws the claim
before persistence starts. If persistence has started, the committed input goes
through the normal tracked inbox: only actually returning it advances consumed
input progress. Ending execution without reading it gives its sender an explicit
unread-input rejection. Committed input is never replayed. `try_recv` only reads
committed messages and never claims an offered answer; use `recv` to accept one.
Task history does not implicitly become its inbox. `returnImmediately` bypasses
the read handshake once earlier admissions are resolved; its response acknowledges
persistence, not executor consumption.

The default handler validates each client input, persists it, and delivers it
through the task's execution input queue. Custom `TaskStore` implementations
retain their existing required methods. Authorized listings require an
implementation of `list_scoped`; its default rejects the operation to prevent
post-pagination filtering from exposing tasks or counts.

Capabilities must be advertised explicitly: an absent capability is treated
as unsupported. Set `streaming: Some(true)` to enable streaming/live responses,
and advertise client artifact support before accepting artifact updates.
`with_capabilities` replaces the entire capability set. Calling it after
`with_push_notifications` or extended-card configuration can remove capabilities
those builders enabled. Set the capability set first, or include every enabled
capability in the final `with_capabilities` call.

## Completion, retries and live exchanges

- A blocking send waits for a relevant interrupt or completion. If the
  execution that consumed its input ends without changing the task to a
  terminal state, the call returns the current task snapshot. This can still
  have a `WORKING` or `INPUT_REQUIRED` state.
- `with_blocking_response_timeout` uses one deadline across admission and the
  blocking response. Time spent waiting for a reader reduces the response budget.
  It also bounds `returnImmediately` admission. Expiry during a storage operation
  can leave an unknown input outcome; inspect the task before retrying. Deadline
  recovery never waits for another full timeout to reload storage.
  Expiry re-reads storage. A stored interrupt is not presented as a new
  answer: a deadline error includes SDK `ErrorInfo` with `inputCommitted`,
  `taskId` and `generation`. Do not resend that input. Other states return
  the stored snapshot. An uncreated task or uncertain storage outcome is an
  error. The timeout does not cancel execution.
- `AgentExecutor::execute` keeps its signature. A worker creates `let
  (outputs, events) = ctx.output_channel(capacity)`, owns the sender and
  inputs, and returns `Box::pin(events)` from `execute`. The returned stream
  alone defines execution lifetime: after all senders drop, buffered events
  drain and EOF revokes input. Capturing the context does not extend
  execution. `outputs.send_for(&input, event)` preserves producer-side input
  correlation through the supplied receiver. Return that receiver directly;
  arbitrary buffering of its untagged items cannot retain producer provenance
  implicitly. Synchronous filtering is supported, including filtering followed
  by a pending read. Ambiguous buffering/prefetching produces a diagnostic and
  conservative correlation; it does not fail an otherwise valid execution.
  Transform before `send_for` when exact producer correlation matters.
  Arbitrary untagged buffers cannot preserve that provenance implicitly.
  Prefer `ctx.spawn_worker(capacity, |ctx, outputs| async move { ...; Ok(())
  })` and return its receiver, or attach a manually spawned worker using
  `events.with_worker(handle)`. The monitored worker's panic, cancellation or
  returned error becomes a stream error after its buffered output. Dropping
  the stream aborts that worker. Arbitrary detached application tasks are not
  monitored. `ExecutorOutput` is non-exhaustive; construct outputs through the
  sender API.
- HTTP live follow-ups have a default 30-second acknowledgement deadline,
  configurable with `with_http_followup_timeout`. Pending requests can be
  revoked before admission. Owned requests are ACKed on timeout only when a
  post-save receipt proves commit. Otherwise the error explicitly reports an
  unknown outcome: inspect the task before retrying. Lost receipts re-read
  storage; an artifact-only update needs an explicit commit receipt.
- The SDK's HTTP live exchange header is an SDK routing convention, not a
  field defined by the A2A specification. Exchange identifiers are scoped to
  the authenticated caller; anonymous calls share the anonymous scope.
- HTTP `LiveSender` retains the opening `ServiceParams`, including credentials.
  It does not refresh an expired token for follow-ups. Open a new connection
  with refreshed parameters to change credentials; creating another clone of
  the existing sender does not refresh them.
- Unsupported live operations consistently report `UNSUPPORTED_OPERATION`.
  SLIM RPC establishes the call asynchronously, so an unsupported operation is
  reported on `connection.events`, rather than by `send_live_message`. Any
  malformed gRPC frame reports its original transport status. If an earlier
  request was received, admission and its first response share a recovery
  deadline, defaulting to 30 seconds. Set it with
  `GrpcHandler::with_live_recovery_timeout`; it applies to recovery after
  malformed input. Healthy exchanges have no such deadline. Available output
  is preserved, with a bounded ready-event drain before the fault. If a custom
  handler remains pending beyond the deadline, the fault is returned without
  waiting longer. A normal input half-close still permits output.
- An HTTP live fallback classified as unsupported retains its HTTP status
  and response body in the error message. A 404 can mean a wrong endpoint or
  a lost route; it is not proof of the agent's advertised capabilities.
- The SLIM proxy merges Task ACKs with the downlink using task generations.
  Only positive, reported generations are compared; generation zero is
  forwarded as unknown. Progress is retained per task across reopened
  connections. Stale ACKs and covered non-message events are omitted and
  logged at debug level. Reconnected backends must preserve task generations;
  a lower-generation snapshot does not reset the cursor.
  `MessageUpdate` events are always forwarded because an ACK snapshot can
  have its history and timeline trimmed. Consumers can deduplicate message IDs.

For versioned REST requests, an unknown task-action path returns HTTP 404.
Malformed requests to recognized operations still return HTTP 400; an unknown
path is no longer reported as an invalid A2A request.

## Listing and webhook bounds

Authenticated in-memory listings normally cache their initial ordered task IDs
for five minutes of inactivity. Continuations renew expiry, read current
contents and recheck filters and permissions. Their membership order stays
fixed; newly created tasks require a new listing. Cursor ownership includes
the tenant and stable principal, while cache budgets count a principal across
tenants to prevent request-supplied tenants from multiplying allocations.

Anonymous listings retain no per-list state. Authenticated listings also use
stateless positions when a cache budget is full, so exhausting the cache
cannot deny another caller's listing or evict active traversals. Newly issued
keyset tokens are authenticated and bind the store, query, tenant and
principal. Each page uses current timestamp order: a task moving ahead of the
cursor can be omitted, and a task moving behind it can repeat. Start a new
traversal for a fresh view. Legacy numeric/unsigned sort-key positions remain
readable for compatibility and have no scope binding. Every page still checks
current filters and authorization. For issued keyset cursors, continuation
pages scan O(N) Arc references, select the next page and sort only that page.
Custom stores can override the additive `list_for_scope` method without
changing existing required trait methods.

Webhook work uses bounded per-task queues and a shared HTTP concurrency pool.
Ordinary output waits for a bounded publication-batch permit outside the
controller. The 128 ordinary slots reserve eight for inputs, so waiting
executor outputs cannot starve follow-up admission. Input admission rejects
before persistence when its reserved capacity is full; subscriptions and
cancellation still proceed. Finalization has two additional reserved slots.
Notifications remain ordered, and two slow fan-outs cannot occupy the whole
HTTP pool. Delivery/configuration retries have deadlines. A single task cannot
occupy every HTTP slot, but many slow tasks can still queue behind the finite
shared pool. If direct webhook publication cannot be admitted, the committed
event still reaches client subscribers once and notification failure is
signaled. Delivery failure is reported through server diagnostics; webhook
delivery is not a durable, exactly-once outbox.
