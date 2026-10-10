# a2a-slimrpc

SLIMRPC bindings for A2A v1 client and server implementations.

This crate is published as `a2a-slimrpc` and imported in Rust as `a2a_slimrpc`.

## What It Provides

- `Transport` implementation for A2A over SLIMRPC
- `TransportFactory` integration for agent cards that advertise `SLIMRPC`
- Registration helpers that expose an `a2a_server::RequestHandler` through `slim_bindings::Server`

## Agent Card Target Format

The existing `AgentInterface` model only carries a string target, so the SLIMRPC
binding interprets `supportedInterfaces[].url` as a SLIM peer name.

Accepted forms are:

- `org/namespace/app`
- `slim://org/namespace/app`
- `slimrpc://org/namespace/app`

## Install

```toml
[dependencies]
a2a = { package = "a2a-lf", version = "0.2" }
a2a-slimrpc = { package = "a2a-slimrpc", version = "0.3" }
```

Built on the SLIM 3.2 release line (`agntcy-slim-rpc` 3.2, `agntcy-slim-auth` and
`agntcy-slim-config` 0.17). `SlimApp` and the `slim_rpc` types in the public API
come from those crates, so depend on the same versions.

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/a2aproject/a2a-rs
- Workspace README: https://github.com/a2aproject/a2a-rs/blob/main/README.md