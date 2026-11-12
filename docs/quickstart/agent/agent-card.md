# The agent card

An agent card is a JSON document that describes an agent: who it is, what it can do and where to reach it. A client usually fetches it first, from `/.well-known/agent-card.json` on the agent's host.

The card of the greeter is built by `agent_card`:

```rust title="examples/src/quickstart/agent.rs"
--8<-- "examples/src/quickstart/agent.rs:card"
```

What the fields say:

- `name`, `description` and `version` identify the agent. `version` is the agent's own version, not the protocol's.
- `capabilities.streaming` promises that the agent answers `SendStreamingMessage`. The greeter does, so it is `true`. `push_notifications` is `false`: the greeter does not call a webhook back.
- `skills` lists what the agent does, each with an `id`, a `name`, a `description`, `tags` and optional `examples`. The greeter has one skill, `greet`.
- `default_input_modes` and `default_output_modes` are the media types the agent reads and writes. The greeter works in plain text.
- `supported_interfaces` has one entry per way to reach the agent: a URL and a binding. `agent_card` takes the base URL, so the same card works wherever the agent runs. The greeter offers `JSONRPC` at `/jsonrpc` and `HTTP+JSON` (REST) at `/rest`, both speaking protocol version `1.0`.

`AgentCard` has no default value, so every field is written out, and `None` marks an optional one that the agent leaves unset.

Next, [the executor](executor.md) holds the logic the card advertises.
