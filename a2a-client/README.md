# a2a-client-lf

Async Rust client for A2A v1 agents.

This crate is published as `a2a-client-lf` and imported in Rust as `a2a_client`.

## What It Provides

- REST and JSON-RPC transports
- Agent card resolution and transport selection helpers
- A high-level `A2AClient` wrapper
- Streaming response parsing for SSE-based endpoints

## Install

```sh
cargo add a2a-lf --rename a2a
cargo add a2a-client-lf --rename a2a-client
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/a2aproject/a2a-rs
- Workspace README: https://github.com/a2aproject/a2a-rs/blob/main/README.md
