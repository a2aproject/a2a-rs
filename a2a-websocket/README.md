# a2a-websocket

WebSocket custom protocol bindings for A2A v1 client and server
implementations.

This crate is published as `a2a-websocket` and imported in Rust as
`a2a_websocket`.

## What It Provides

- `Transport` implementation for A2A over a single multiplexed WebSocket
  connection (`WebSocketTransport`).
- `TransportFactory` integration for agent cards that advertise
  `WEBSOCKET` (`WebSocketTransportFactory`).
- An `axum::Router` builder (`websocket_router`) that adapts an
  `a2a_server::RequestHandler` to serve A2A operations over a
  persistent WebSocket connection with bidirectional streaming.
- Mapping between `a2a::A2AError` codes and the canonical WebSocket
  binding error type strings, including close-code selection for
  fatal failures.

The wire format follows the A2A WebSocket Custom Protocol Binding
specification:

- Sub-protocol: `a2a.v1` (negotiated via `Sec-WebSocket-Protocol`).
- All A2A messages travel as UTF-8 JSON envelopes inside text frames.
- Streaming methods deliver `event` frames terminated by a
  `streamEnd: true` sentinel; clients can cancel an in-progress stream
  by sending a `cancelStream: true` envelope.

## Agent Card Endpoint Format

The existing `AgentInterface` model only carries a string target, so the
WebSocket binding interprets `supportedInterfaces[].url` as an absolute
WebSocket endpoint. Accepted forms are:

- `ws://host:port[/path]`
- `host:port[/path]` (normalized to `ws://`)

`wss://` is reserved for a future TLS-enabled feature flag — for now,
terminate TLS at a reverse proxy in front of the agent.

The transport identifier in agent cards is `WEBSOCKET`, exposed as the
`a2a::TRANSPORT_PROTOCOL_WEBSOCKET` constant.

## Example: server

```rust,ignore
use std::sync::Arc;
use a2a_server::handler::DefaultRequestHandler;
use a2a_server::task_store::InMemoryTaskStore;
use a2a_websocket::server::websocket_router;

let handler = Arc::new(DefaultRequestHandler::new(my_executor, InMemoryTaskStore::new()));
let app = axum::Router::new().nest("/a2a/ws", websocket_router(handler));
```

## Example: client

```rust,ignore
use a2a_client::transport::{ServiceParams, Transport};
use a2a_websocket::WebSocketTransport;

let transport = WebSocketTransport::connect("ws://127.0.0.1:9000/a2a/ws").await?;
let response = transport.send_message(&ServiceParams::new(), &request).await?;
```

## Install

```toml
[dependencies]
a2a = { package = "a2a-lf", version = "0.2" }
a2a-websocket = { package = "a2a-websocket", version = "0.1" }
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/a2aproject/a2a-rs
- Workspace README: https://github.com/a2aproject/a2a-rs/blob/main/README.md
