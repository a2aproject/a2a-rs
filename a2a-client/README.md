# agntcy-a2a-client

Async Rust client for A2A v1 agents.

This crate is published as `agntcy-a2a-client` and imported in Rust as `a2a_client`.

## What It Provides

- REST and JSON-RPC transports
- Agent card resolution and transport selection helpers
- A high-level `A2AClient` wrapper
- Streaming response parsing for SSE-based endpoints

## Install

```toml
[dependencies]
a2a = { package = "agntcy-a2a", version = "0.2" }
a2a-client = { package = "agntcy-a2a-client", version = "0.1" }
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/agntcy/a2a-rs
- Workspace README: https://github.com/agntcy/a2a-rs/blob/main/README.md
