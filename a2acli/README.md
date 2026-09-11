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
- Bearer token, API key, and general service-parameter authentication on
  every request, with an ordered `--transport` preference and an `--insecure`
  escape hatch (development only, always warns) for self-signed endpoints
- Human-readable `text` output by default, or the protocol's own JSON
  (`-o/--output json`); every failure is a machine-readable error object
  with a stable code and exit status

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
another deployment and `--transport jsonrpc` or `--transport rest` (repeatable
and ordered, highest preference first) to pin the transport when the agent
card exposes more than one compatible interface. The global `--tenant`,
`--bearer`, `--api-key`, and repeated `--svc-param Name:Value` options also
apply to `task push-config` commands.

### Authentication and transport

```sh
cargo run --bin a2acli -- --bearer "$TOKEN" card get
cargo run --bin a2acli -- --api-key "$KEY" card get
cargo run --bin a2acli -- --svc-param "X-Trace-Id:abc123" send "hello"
cargo run --bin a2acli -- --transport jsonrpc --transport rest card get
cargo run --bin a2acli -- --insecure --bearer "$TOKEN" card get  # dev only; always warns
cargo run --bin a2acli -- --debug send "hello"                  # request/response diagnostics to stderr
```

`--bearer`/`--api-key` (env `A2ACLI_BEARER`/`A2ACLI_API_KEY`) supply credentials;
`--svc-param` is a separate, general-purpose transport-level key-value pair,
never itself a credential flag. `--insecure` disables TLS certificate
verification and always prints a warning naming the risk when a credential is
also configured — it never disables verification silently. `--debug` never
prints credential values, regardless of verbosity. `--tenant` overrides the
routing tenant the selected Agent Card interface may itself declare; omit it
to use the interface's own value, if it has one.

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

### Output and errors

`text` (labeled `Label: value` fields, with a copy-pasteable resume command
whenever a task pauses at `INPUT_REQUIRED`/`AUTH_REQUIRED`) is the default,
human-readable format. Pass `-o json` (or `--output json`) for the protocol's
own JSON types instead — `--stream` then switches its cardinality from one
document to JSON Lines (one object per event); `--compact` only affects the
single-document form.

```sh
cargo run --bin a2acli -- task get task-123            # text (default)
cargo run --bin a2acli -- task get task-123 -o json    # one JSON document
cargo run --bin a2acli -- send "hello" --stream -o json  # JSONL, one event per line
```

A failure — from the agent or from the tool itself — always prints one
compact JSON error object to stderr, in every output mode:

```json
{"error":{"code":"TASK_NOT_FOUND","message":"task not found: t-1","a2aCode":-32001}}
```

`code` is the A2A protocol's own error name for a protocol failure (with the
numeric `a2aCode` alongside it), or an `A2ACLI_ERR_*` symbol for a failure
the protocol never saw (a bad flag, an unreachable agent, a `--timeout`
expiry). The exit status reports only whether the CLI did its job — `0` even
when a task ends `FAILED`/`REJECTED` or pauses at
`INPUT_REQUIRED`/`AUTH_REQUIRED` — while `1`/`2`/`3`/`4`/`5` distinguish a
generic failure, a usage error, an unreachable agent, a rejected credential,
and a timeout respectively.

## Conformance

This CLI is tracked against the
[a2a-cli specification](https://github.com/a2aproject/a2a-cli/blob/main/specification/SPEC.md)
Tier 1 ("Core") requirements; see
[a2aproject/a2a-rs#164](https://github.com/a2aproject/a2a-rs/issues/164) for the
current gap list and in-progress work (blocking-by-default `send`/polling,
human-readable `text` output, message parts, and more).