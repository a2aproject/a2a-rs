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
- Unknown timeline content is represented by `TimelineContent::Unknown`.
  Its generation is retained so a receiver can track progress without knowing
  a future payload kind. Exhaustive matches must handle this variant.
- Send `A2A-Version: 1.1` for 1.1 features. A bare major version is also
  accepted. Bindings use the 1.1 protobuf/ProtoJSON envelope and enum names;
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

## Completion, retries and live exchanges

- A blocking send waits for a relevant interrupt or completion. If the
  invocation that consumed its input returns without changing the task to a
  terminal state, the call returns the current task snapshot. This can still
  have a `WORKING` or `INPUT_REQUIRED` state.
- `with_blocking_response_timeout` can bound waits on an executor that never finishes.
  After input is committed, expiry returns the current task, so a caller is
  not encouraged to repeat a committed input. If no task was created, expiry
  is an error. The timeout does not cancel execution.
- Buffered executor outputs have a correlation limitation: input progress is
  sampled when the server polls the stream, not when a detached producer
  creates an output. Yield an interrupt before consuming the next input, or
  end the invocation, rather than buffering one caller's question across
  consumption of another caller's input. This API does not provide an
  explicit producer-supplied interrupt generation.
- HTTP live follow-ups have a default 30-second acknowledgement deadline,
  configurable with `with_http_followup_timeout`. A queued request revoked
  before processing cannot later mutate the task. Once processing owns it,
  an acknowledgement reports the current committed task; the downlink
  carries any subsequent input rejection.
- The SDK's HTTP live exchange header is an SDK routing convention, not a
  field defined by the A2A specification. Exchange identifiers are scoped to
  the authenticated caller; anonymous calls share the anonymous scope.
- Unsupported live operations consistently report `UNSUPPORTED_OPERATION`.
  A malformed gRPC live input terminates the exchange with its original
  transport status; a normal input half-close still permits output.

## Listing and webhook bounds

In-memory list cursors preserve the initial ordered task IDs for five minutes,
while each continuation reads current contents and rechecks filters and
permissions. New tasks require a new listing. Concurrent updates may change
ordering across page boundaries. Cursor capacity rejects new allocations
rather than evicting active traversals; expired cursors require a fresh list.

Webhook work uses bounded per-task queues and a shared HTTP concurrency pool.
Cancellation can proceed while publication is backpressured; publications
remain ordered. Delivery/configuration retries have deadlines. A single task
cannot occupy every HTTP slot, but many slow tasks can still queue behind the
finite shared pool. Delivery failure is reported through server diagnostics;
webhook delivery is not a durable, exactly-once outbox.
