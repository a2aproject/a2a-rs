# a2a-pb

Protobuf schema and conversion helpers for A2A v1.

This crate is published as `a2a-pb` and imported in Rust as `a2a_pb`.

## What It Provides

- Generated protobuf types for the A2A schema
- ProtoJSON-capable generated types in `a2a_pb::protojson`
- Conversion helpers between protobuf and native Rust models
- The bundled A2A proto definition used by the Rust workspace

## Install

```sh
cargo add a2a-lf --rename a2a
cargo add a2a-pb
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/a2aproject/a2a-rs
- Workspace README: https://github.com/a2aproject/a2a-rs/blob/main/README.md
