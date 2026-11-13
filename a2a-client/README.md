# a2a-client-lf

Async Rust client for A2A v1 agents.

This crate is published as `a2a-client-lf` and imported in Rust as `a2a_client`.

## What It Provides

- REST and JSON-RPC transports
- Agent card resolution and transport selection helpers
- A high-level `A2AClient` wrapper
- Streaming response parsing for SSE-based endpoints

## Live Connections

Each REST or JSON-RPC `send_live_message` call creates its own exchange identifier
and sends it in the optional `x-a2a-rs-live-exchange` header. Its `LiveSender`
reuses that identifier so inputs and their rejections stay with the intended
downlink, including when several connections share a task and credentials.
Task updates remain visible to every subscribed client.

The header is a Rust SDK routing convention, not a standard A2A field or an
authentication credential. Servers without this convention may ignore it and
apply their own HTTP live-routing policy. gRPC already associates input with its
native duplex stream and does not use this header.

## Install

```sh
cargo add a2a-lf --rename a2a
cargo add a2a-client-lf --rename a2a-client
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/a2aproject/a2a-rs
- Workspace README: https://github.com/a2aproject/a2a-rs/blob/main/README.md
