# a2a-stdio

STDIO transport binding for A2A v1 client and server implementations.

This crate is published as `agntcy-a2a-stdio` and imported in Rust as `a2a_stdio`.

## What It Provides

- `StdioTransport` — `Transport` implementation that spawns an agent
  subprocess and speaks A2A JSON-RPC over its stdin/stdout
- `StdioTransportFactory` — `TransportFactory` integration for agent cards
  that advertise the `STDIO` protocol
- `StdioServer` — server-side dispatcher that wraps an
  `a2a_server::RequestHandler` and serves requests over a pair of
  asynchronous reader/writer streams

## Wire Format

Messages on the wire use LSP-style framing followed by a JSON-RPC 2.0 body:

```text
Content-Length: <N>\r\n
Content-Type: application/json\r\n
\r\n
<N bytes of JSON>
```

Method names use the slash-separated A2A convention
(`message/send`, `tasks/get`, `tasks/subscribe`, ...).

## Handshake

Before serving requests, the server emits a `handshake` message advertising
its supported variants and features. The client replies with a
`handshakeAck` that either accepts (selecting a variant) or rejects the
session.

A session identifier may be passed via the `A2A_SESSION_ID` environment
variable; otherwise the server generates a UUIDv7.

## Agent Card Target Format

`StdioTransportFactory` interprets `supportedInterfaces[].url` as one of:

- `stdio:///path/to/binary` — spawn the binary with no arguments
- `stdio:///path/to/binary?arg1&arg2` — spawn with the given arguments
- `/path/to/binary arg1 arg2` — plain command line, split on whitespace

Arguments in the `stdio://` form are split on `&` and the path is split on
the first `?`, so individual arguments cannot contain `&` or `?`. Use the
plain command-line form when those characters are required.

## Install

```toml
[dependencies]
a2a = { package = "a2a-lf", version = "0.2" }
a2a-stdio = { package = "agntcy-a2a-stdio", version = "0.1" }
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/a2aproject/a2a-rs
- Workspace README: https://github.com/a2aproject/a2a-rs/blob/main/README.md
