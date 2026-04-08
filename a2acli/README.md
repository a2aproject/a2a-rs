# a2acli

Standalone A2A CLI client built on top of `a2a-client`.

This crate is published as `agntcy-a2acli` and installs the `a2acli` binary.

## Install

From the workspace checkout:

```sh
cargo install --path a2acli
```

From crates.io after release:

```sh
cargo install agntcy-a2acli
```

## What It Provides

- Fetch and print the public agent card for an A2A deployment
- Send one-shot or streaming messages
- Inspect, list, cancel, and subscribe to tasks
- Create, fetch, list, and delete task push notification configs
- Request an extended agent card when the server exposes one
- Add bearer-token or custom-header authentication to all requests

## Run

```sh
cargo run --bin a2acli -- card
cargo run --bin a2acli -- send "hello from rust"
cargo run --bin a2acli -- stream "hello from rust"
cargo run --bin a2acli -- get-task task-123
cargo run --bin a2acli -- push-config list task-123
cargo run --bin a2acli -- push-config create task-123 https://example.com/callback --auth-scheme Bearer --auth-credentials secret
```

By default the CLI targets `http://localhost:3000`. Use `--base-url` to point at
another deployment and `--binding jsonrpc` or `--binding http-json` to pin the
transport when the agent card exposes more than one compatible interface. The
global `--tenant`, `--bearer-token`, and repeated `--header Name:Value` options
also apply to push-config commands.