# a2a-rs

`a2a-rs` is a Rust workspace for the A2A v1 protocol. It includes core protocol
types, async client and server libraries, protobuf definitions, and gRPC
bindings for building interoperable A2A agents and clients.

The workspace supports:

- JSON-RPC 2.0 over HTTP
- REST / HTTP+JSON
- gRPC via `tonic`
- Server-Sent Events for streaming responses
- Protobuf-based interop with other SDKs and runtimes

## Workspace

| Crate | Purpose |
| --- | --- |
| `a2a` | Core A2A types, errors, events, JSON-RPC types, and wire-compatible serde behavior |
| `a2a-client` | Async A2A client with transport abstraction and protocol negotiation from agent cards |
| `a2a-server` | Async server framework with REST and JSON-RPC bindings built on `axum` |
| `a2a-pb` | Protobuf schema, generated types, and native <-> protobuf conversion helpers |
| `a2a-grpc` | gRPC client and server bindings built on `tonic` |
| `examples/helloworld` | Minimal runnable example agent |

## Supported Bindings

| Binding | Client | Server |
| --- | --- | --- |
| JSON-RPC | `a2a-client` | `a2a-server` |
| HTTP+JSON / REST | `a2a-client` | `a2a-server` |
| gRPC | `a2a-grpc` | `a2a-grpc` |

The gRPC support uses the schema in `a2a-pb/proto/a2a.proto`. The REST and
JSON-RPC bindings are intended to stay wire-compatible with other A2A SDKs,
including Go and C# implementations.

## Requirements

- Rust 1.85 or newer
- A stable `rustup` toolchain is recommended
- `just` is optional but useful for common development commands
- `cargo-llvm-cov` is optional if you want HTML coverage reports

## Build And Test

```sh
cargo build --workspace
cargo test --workspace
```

Common project commands are also available through `just`:

```sh
just build
just test
just lint
just fmt-check
just ci
```

## Coverage

Install `cargo-llvm-cov` once:

```sh
cargo install cargo-llvm-cov
```

Then run:

```sh
just coverage
```

This writes the HTML report to `target/llvm-cov/html/index.html`.

## Running The Example Agent

The hello world example exposes an echo-style agent over REST and JSON-RPC.

```sh
cargo run -p helloworld
```

When the example starts, the following endpoints are available:

- Agent card: `http://localhost:3000/.well-known/agent-card.json`
- JSON-RPC endpoint: `http://localhost:3000/jsonrpc`
- REST endpoint: `http://localhost:3000/rest`

The example does not start a gRPC server, but the `a2a-grpc` crate provides the
client and server bindings needed to add one.

## Depending On The Workspace

Until the crates are published, depend on them directly from Git:

```toml
[dependencies]
a2a = { package = "agntcy-a2a", git = "https://github.com/agntcy/a2a-rs.git" }
a2a-client = { package = "agntcy-a2a-client", git = "https://github.com/agntcy/a2a-rs.git" }
a2a-server = { package = "agntcy-a2a-server", git = "https://github.com/agntcy/a2a-rs.git" }
a2a-pb = { package = "agntcy-a2a-pb", git = "https://github.com/agntcy/a2a-rs.git" }
a2a-grpc = { package = "agntcy-a2a-grpc", git = "https://github.com/agntcy/a2a-rs.git" }
```

Typical usage is:

- `a2a` for protocol types and serde models
- `a2a-client` for clients that negotiate REST or JSON-RPC from an agent card
- `a2a-server` for agent implementations on `axum`
- `a2a-grpc` when you need gRPC transport support
- `a2a-pb` when you need direct access to protobuf messages or conversion helpers

## Repository Layout

- `a2a/`: core protocol crate
- `a2a-client/`: client transports and factory
- `a2a-server/`: request handler, routers, streaming, and stores
- `a2a-pb/`: protobuf schema and conversion layer
- `a2a-grpc/`: tonic-based bindings
- `examples/helloworld/`: runnable sample agent

## Contributing

See `CONTRIBUTING.md` for contribution guidelines, `SECURITY.md` for security
reporting, and `CODE_OF_CONDUCT.md` for community expectations.

## License

Apache-2.0. See `LICENSE.md`.