# a2aproj-a2a-rs-client

Async Rust client for A2A v1 agents.

This crate is published as `a2aproj-a2a-rs-client` and imported in Rust as `a2a_client`.

## What It Provides

- REST and JSON-RPC transports
- Agent card resolution and transport selection helpers
- A high-level `A2AClient` wrapper
- Streaming response parsing for SSE-based endpoints

## Install

```toml
[dependencies]
a2a = { package = "a2aproj-a2a-rs", version = "0.2" }
a2a-client = { package = "a2aproj-a2a-rs-client", version = "0.1" }
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/a2aproject/a2a-rs
- Workspace README: https://github.com/a2aproject/a2a-rs/blob/main/README.md
