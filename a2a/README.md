# agntcy-a2a

Core Rust types for the A2A v1 protocol.

This crate is published as `agntcy-a2a` and imported in Rust as `a2a`.

## What It Provides

- A2A wire-compatible message, task, artifact, and event types
- JSON-RPC request and response models
- Protocol error types and helpers
- Serde implementations aligned with the A2A protocol shape

## Install

```toml
[dependencies]
a2a = { package = "agntcy-a2a", version = "0.2" }
```

## Example

```rust
use a2a::{Message, Part, Role};

let message = Message::new(Role::User, vec![Part::text("hello")]);
assert_eq!(message.text(), Some("hello"));
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: https://github.com/agntcy/a2a-rs
- Workspace README: https://github.com/agntcy/a2a-rs/blob/main/README.md
