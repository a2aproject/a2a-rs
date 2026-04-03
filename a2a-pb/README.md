# agntcy-a2a-pb

Protobuf schema and conversion helpers for A2A v1.

This crate is published as `agntcy-a2a-pb` and imported in Rust as `a2a_pb`.

## What It Provides

- Generated protobuf types for the A2A schema
- Conversion helpers between protobuf and native Rust models
- The bundled A2A proto definition used by the Rust workspace

## Install

```toml
[dependencies]
a2a = { package = "agntcy-a2a", version = "0.2" }
a2a-pb = { package = "agntcy-a2a-pb", version = "0.1" }
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/agntcy/a2a-rs
- Workspace README: https://github.com/agntcy/a2a-rs/blob/main/README.md
