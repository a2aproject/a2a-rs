# a2acli

Standalone A2A CLI client built on top of `a2a-client`.

This crate is published as `a2a-cli` and installs the `a2acli` binary.

## Install

From the workspace checkout:

```sh
cargo install --path a2acli
```

From crates.io after release:

```sh
cargo install a2a-cli
```

## What It Provides

- Fetch and print the public agent card for an A2A deployment
- Send one-shot or streaming messages
- Inspect, list, cancel, and subscribe to tasks
- Create, fetch, list, and delete task push notification configs
- Request an extended agent card when the server exposes one
- Add bearer-token or custom-header authentication to all requests

## Run

Commands are namespaced by the resource they act on (`card get`, `task get`,
`task push-config create`, …), matching the command taxonomy in the
[a2a-cli specification](https://github.com/a2aproject/a2a-cli/blob/main/specification/SPEC.md#7-command-surface--global-options):

```sh
cargo run --bin a2acli -- card get
cargo run --bin a2acli -- card get --extended
cargo run --bin a2acli -- send "hello from rust"
cargo run --bin a2acli -- send "hello from rust" --stream
cargo run --bin a2acli -- task get task-123
cargo run --bin a2acli -- task list
cargo run --bin a2acli -- task cancel task-123
cargo run --bin a2acli -- task subscribe task-123
cargo run --bin a2acli -- task push-config list task-123
cargo run --bin a2acli -- task push-config create task-123 https://example.com/callback --auth-scheme Bearer --auth-credentials secret
```

By default the CLI targets `http://localhost:3000`. Use `--base-url` to point at
another deployment and `--binding jsonrpc` or `--binding http-json` to pin the
transport when the agent card exposes more than one compatible interface. The
global `--tenant`, `--bearer-token`, and repeated `--header Name:Value` options
also apply to `task push-config` commands.

## Conformance

This CLI is tracked against the
[a2a-cli specification](https://github.com/a2aproject/a2a-cli/blob/main/specification/SPEC.md)
Tier 1 ("Core") requirements; see
[a2aproject/a2a-rs#164](https://github.com/a2aproject/a2a-rs/issues/164) for the
current gap list and in-progress work (blocking-by-default `send`/polling,
human-readable `text` output, message parts, and more).