# a2a-lf

Core Rust types for the A2A v1 protocol.

This crate is published as `a2a-lf` and imported in Rust as `a2a`.

## What It Provides

- A2A wire-compatible message, task, artifact, and event types
- JSON-RPC request and response models
- Protocol error types and helpers
- Serde implementations aligned with the A2A protocol shape

## Install

```sh
cargo add a2a-lf --rename a2a
```

## Example

```rust
use a2a::{Message, Part, Role};

let message = Message::new(Role::User, vec![Part::text("hello")]);
assert_eq!(message.text(), Some("hello"));
```

## Timestamps

Task status timestamps and `ListTasksRequest::status_timestamp_after` use
`a2a::Timestamp`. Parse RFC 3339 text with `.parse()`, use `Timestamp::now()`
for the current time, or construct an instant with `Timestamp::from_unix_parts`.
The constructor accepts floor Unix seconds and a nonnegative nanosecond fraction.
Serde writes UTC timestamps with a `Z` suffix and 0, 3, 6 or 9 fractional digits.
The protobuf range is years 1 through 9999 UTC. Explicit leap-second
spellings (`:60`) are rejected because protobuf represents smeared time.

```rust
use a2a::Timestamp;

let instant: Timestamp = "1970-01-01T00:59:59.5+01:00".parse()?;
assert_eq!(instant, Timestamp::from_unix_parts(-1, 500_000_000).unwrap());
assert_eq!(instant.to_string(), "1969-12-31T23:59:59.500Z");
# Ok::<(), a2a::ParseTimestampError>(())
```

## Workspace

This crate is part of the `a2a-rs` workspace.

- Repository: <https://github.com/a2aproject/a2a-rs>
- Workspace README: <https://github.com/a2aproject/a2a-rs/blob/main/README.md>
