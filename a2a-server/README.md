# a2a-server-lf

Async server framework for implementing A2A v1 agents in Rust.

This crate is published as `a2a-server-lf` and imported in Rust as `a2a_server`.

## What It Provides

- REST and JSON-RPC routers built on `axum`
- A transport-agnostic `RequestHandler` abstraction
- A default handler and in-memory task store
- SSE helpers for streaming task responses

## Install

```sh
cargo add a2a-lf --rename a2a
cargo add a2a-server-lf --rename a2a-server
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/a2aproject/a2a-rs
- Workspace README: https://github.com/a2aproject/a2a-rs/blob/main/README.md
