# a2a-server-lf

Async server framework for implementing A2A v1 agents in Rust.

This crate is published as `a2a-server-lf` and imported in Rust as `a2a_server`.

## What It Provides

- REST and JSON-RPC routers built on `axum`
- A transport-agnostic `RequestHandler` abstraction
- A default handler and in-memory task store
- SSE helpers for streaming task responses

## Install

```toml
[dependencies]
a2a = { package = "a2a-lf", version = "0.2" }
a2a-server = { package = "a2a-server-lf", version = "0.1" }
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/a2aproject/a2a-rs
- Workspace README: https://github.com/a2aproject/a2a-rs/blob/main/README.md
