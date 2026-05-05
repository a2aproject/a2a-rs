# a2a-websocket

WebSocket bindings for A2A v1 client and server implementations.

This crate is published as `a2a-websocket` and imported in Rust as `a2a_websocket`.

## What It Provides

- A `WebSocketTransport` implementing the [`a2a_client::Transport`] trait,
  multiplexing requests and streaming responses over a single WebSocket
  connection.
- A `WebSocketTransportFactory` for use with `A2AClientFactory`.
- An `axum::Router` builder that adapts an `a2a_server::RequestHandler` to
  serve A2A operations over a persistent WebSocket connection, including
  full bidirectional streaming and multiplexing.
- Mapping between A2A error types/codes and the canonical
  [WebSocket binding error type strings](../../websocket/websocket-spec.md).

The wire format follows the [A2A WebSocket Custom Protocol Binding
specification](../../websocket/websocket-spec.md):

- Sub-protocol: `a2a.v1` (negotiated via `Sec-WebSocket-Protocol`).
- All A2A messages travel as UTF-8 JSON envelopes inside text frames.
- Streaming methods deliver `event` frames terminated by a `streamEnd: true`
  sentinel; clients can cancel an in-progress stream by sending a
  `cancelStream: true` envelope.

## Endpoint Format

`WebSocketTransport::connect` and `WebSocketTransportFactory` accept
`ws://host:port[/path]` endpoints. URLs that omit the scheme are normalized to
`ws://`. `wss://` is reserved for a future TLS-enabled feature flag — for now,
terminate TLS at a reverse proxy in front of the agent.

The transport identifier in agent cards is `WEBSOCKET`.

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
