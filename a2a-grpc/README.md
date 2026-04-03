# agntcy-a2a-grpc

gRPC bindings for A2A v1 client and server implementations.

This crate is published as `agntcy-a2a-grpc` and imported in Rust as `a2a_grpc`.

## What It Provides

- Tonic-based client transport bindings
- Tonic service adapters for A2A request handlers
- Conversion glue between native models and protobuf messages

## Install

```toml
[dependencies]
a2a = { package = "agntcy-a2a", version = "0.2" }
a2a-grpc = { package = "agntcy-a2a-grpc", version = "0.1" }
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/agntcy/a2a-rs
- Workspace README: https://github.com/agntcy/a2a-rs/blob/main/README.md
