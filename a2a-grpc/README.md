# a2a-grpc

gRPC bindings for A2A v1 client and server implementations.

This crate is published as `a2a-grpc` and imported in Rust as `a2a_grpc`.

## What It Provides

- Tonic-based client transport bindings
- Tonic service adapters for A2A request handlers
- Conversion glue between native models and protobuf messages

## Endpoint Format

`GrpcTransport::connect` and `GrpcTransportFactory` accept both `http://host:port`
and bare `host:port` endpoints. Bare endpoints are normalized to `http://...`
before connecting so agent cards emitted by other SDKs remain usable.

## Install

```toml
[dependencies]
a2a = { package = "a2a-lf", version = "0.2" }
a2a-grpc = { package = "a2a-grpc", version = "0.1" }
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/a2aproject/a2a-rs
- Workspace README: https://github.com/a2aproject/a2a-rs/blob/main/README.md
