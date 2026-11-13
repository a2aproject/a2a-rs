# Agent quickstart

This quickstart builds an A2A agent in Rust and calls it. The agent, `Greeter`, greets the name it is sent. It does two things that most real agents do as well:

- It **streams** its answer: the greeting is sent in pieces, as it is produced.
- It **asks for input** when it needs it: told only `hello`, it replies "Who should I greet?" and waits for the answer.

The steps follow the files in the repository:

1. [The agent card](agent-card.md) describes the agent to clients.
2. [The executor](executor.md) holds the agent's logic.
3. [Start the server](start-the-server.md) serves the agent over JSON-RPC and REST.
4. [Call the agent](call-the-agent.md) with `curl`.
5. [Streaming and multi-turn](streaming-and-multi-turn.md) shows the streamed answer and a conversation over two messages.
6. [Next steps](next-steps.md).

## Prerequisites

- Rust 1.89 or later, with `cargo`. The locked dependencies need it, although the workspace declares 1.85.
- `git`, `curl` and [`jq`](https://jqlang.github.io/jq/).

## Get the code

The agent is in the `examples` crate of the repository:

```bash
git clone https://github.com/a2aproject/a2a-rs.git
cd a2a-rs
```

| File | What it holds |
|---|---|
| `examples/src/quickstart/agent.rs` | The agent card and the executor. |
| `examples/src/quickstart/server.rs` | The server that serves the agent. |

Check that it builds:

```bash
cargo build --package examples --bin quickstart-server
```

The first build compiles the dependencies, so it takes a while.
