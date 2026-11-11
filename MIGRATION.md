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
  Versions must include a minor component; bare `1` is rejected. SLIM RPC
  servers now enforce `A2A-Version` too, so custom SLIM callers must supply it.
  Bindings use the 1.1 protobuf/ProtoJSON envelope and enum names;
  legacy 0.3 stream events are rejected rather than silently skipped.

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
may remain nonterminal after an invocation returns; later input can start a
new invocation. `execution_id` identifies an invocation, not a client or task.
The owned input receiver can move into a spawned Tokio worker.

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
  invocation that consumed its input returns without changing the task to a
  terminal state, the call returns the current task snapshot. This can still
  have a `WORKING` or `INPUT_REQUIRED` state.
- `with_blocking_response_timeout` can bound waits on an executor that never finishes.
  Expiry re-reads storage. A stored interrupt is not presented as a new
  answer: a deadline error includes SDK `ErrorInfo` with `inputCommitted`,
  `taskId` and `generation`. Do not resend that input. Other states return
  the stored snapshot. An uncreated task or uncertain storage outcome is an
  error. The timeout does not cancel execution.
- `AgentExecutor::execute` keeps its signature. An executor can move both
  `ctx.inputs` and the new `ctx.outputs` into owned workers. Use
  `outputs.send_for(&input, event)` to tag output at production time. The
  runner cannot infer the producer of arbitrary custom buffered stream items;
  detached reads no longer advance legacy output correlation. Use the owned
  output sender for prompt per-input blocking replies from detached workers.
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
  SLIM RPC establishes the call asynchronously, so an unsupported operation
  is reported on `connection.events`, rather than by `send_live_message`.
  A malformed gRPC live input terminates the exchange with its original
  transport status; a normal input half-close still permits output.

For versioned REST requests, an unknown task-action path returns HTTP 404.
Malformed requests to recognized operations still return HTTP 400; an unknown
path is no longer reported as an invalid A2A request.

## Listing and webhook bounds

In-memory list cursors preserve the initial ordered task IDs for five minutes
of inactivity. Each continuation reads current contents and rechecks filters
and permissions. New tasks require a new listing. Concurrent updates may
change ordering across page boundaries. Cursor ownership and quotas are
scoped by stable principal and tenant.
Capacity rejects new allocations rather than evicting active traversals;
active continuations refresh expiry. Custom stores can override the additive
`list_for_scope` method without changing existing required trait methods.

Webhook work uses bounded per-task queues and a shared HTTP concurrency pool.
Ordinary output waits for a bounded publication-batch permit outside the
controller. Input admission rejects before persistence when that queue is
full; subscriptions and cancellation still proceed. Finalization has reserved
capacity. Notifications remain ordered, and two slow fan-outs cannot occupy
the whole HTTP pool. Delivery/configuration retries have deadlines. A single task
cannot occupy every HTTP slot, but many slow tasks can still queue behind the
finite shared pool. Delivery failure is reported through server diagnostics;
webhook delivery is not a durable, exactly-once outbox.
