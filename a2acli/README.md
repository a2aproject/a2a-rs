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
- Send one-shot or streaming messages, with multi-part content
  (`--text-part`/`--file-part`/`--data-part`/`--media-type`)
- Blocks by default until a task settles (terminal or interrupted state);
  `--async` returns immediately instead
- Inspect (optionally waiting on it with `--wait`), list, cancel, and
  subscribe to tasks
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
cargo run --bin a2acli -- send "hello from rust" --async
cargo run --bin a2acli -- task get task-123
cargo run --bin a2acli -- task get task-123 --wait
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

### Blocking and polling

`send` blocks by default until the resulting task reaches a terminal
(`COMPLETED`/`FAILED`/`CANCELED`/`REJECTED`) or interrupted
(`INPUT_REQUIRED`/`AUTH_REQUIRED`) state, polling `task get` under the hood;
pass `--async` to get the task identifiers back immediately instead. `task
get` is one-shot by default — add `--wait` to poll it the same way. Tune the
loop with `--poll-interval` (default `2s`) and `--timeout` (default `30s`,
after which the command exits with a timeout error).

### Message parts

A message can carry more than one part, built from repeatable,
order-preserving flags:

```sh
cargo run --bin a2acli -- send \
  --text-part "Review this" \
  --file-part report.pdf --media-type application/pdf \
  --file-part https://example.com/spec.pdf \
  --data-part '{"priority":"high"}'
```

`--text-part` adds a text part, `--file-part <path|url>` a file part (a local
path is inlined as base64 bytes, a URL is carried by reference and never
fetched by the CLI), and `--data-part <path|->` a structured JSON part read
from a file, or from stdin when the value is `-`; anything else is parsed as
an inline JSON string. `--media-type` sets the media type of the part flag
immediately preceding it. The plain positional form (`send "hello"`) is
shorthand for a single `--text-part` and cannot be combined with the part
flags above.

## Conformance

This CLI is tracked against the
[a2a-cli specification](https://github.com/a2aproject/a2a-cli/blob/main/specification/SPEC.md)
Tier 1 ("Core") requirements; see
[a2aproject/a2a-rs#164](https://github.com/a2aproject/a2a-rs/issues/164) for the
current gap list and in-progress work (blocking-by-default `send`/polling,
human-readable `text` output, message parts, and more).