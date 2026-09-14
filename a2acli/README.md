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

- Fetch and print the public agent card for an A2A deployment, named by
  `-a/--agent-card` as a host, a full card URL, or a local file — or
  skipped entirely with `-e/--endpoint`
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
- Full configuration precedence (flag > environment variable > local `.env`
  > global `.env` > built-in default) with a read-only `config show` to
  inspect it

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

By default the CLI targets `http://localhost:3000`. Use `--transport jsonrpc`
or `--transport rest` (repeatable and ordered, highest preference first) to pin
the transport when the agent card exposes more than one compatible interface.
The global `--tenant`, `--bearer`, `--api-key`, and repeated
`--svc-param Name:Value` options also apply to `task push-config` commands.

### Naming the agent

`-a/--agent-card <ref>` names the agent as an **Agent Card reference**, in any
of three forms:

```sh
cargo run --bin a2acli -- -a agent.example.com card get      # host or origin
cargo run --bin a2acli -- -a https://agent.example.com/custom/card.json card get
cargo run --bin a2acli -- -a ./fixtures/card.json card get   # local file
cargo run --bin a2acli -- -a file:///etc/a2a/card.json card get
```

A bare host or origin gets the well-known path `/.well-known/agent-card.json`
appended, and takes `http://` when it is loopback (`localhost`, `127.0.0.1`,
`[::1]`) or `https://` otherwise. A reference that already carries a path is a
full card URL and is used as-is. A `file://` URL or a plain filesystem path is
read from disk, so the CLI can be driven with no agent running at all — a
missing file reports `A2ACLI_ERR_CARD_NOT_FOUND` and a file that is not a card
reports `A2ACLI_ERR_CARD_INVALID`, matching how the HTTP path classifies an
unreachable card and an unparseable body.

`-e/--endpoint <url>` skips card resolution entirely and connects straight to
an agent interface. Since no card is fetched, there is nothing to declare the
protocol binding, so it requires exactly one `--transport` and cannot be
combined with `--agent-card`:

```sh
cargo run --bin a2acli -- -e https://agent.example.com/jsonrpc --transport jsonrpc task get task-123
```

`--base-url` is a deprecated alias for the bare-origin form. It still works so
existing invocations keep running, but it warns and is hidden from `--help`;
prefer `--agent-card`, which accepts all three forms.

### Authentication and transport

```sh
cargo run --bin a2acli -- --bearer "$TOKEN" card get
cargo run --bin a2acli -- --api-key "$KEY" card get
cargo run --bin a2acli -- --svc-param "X-Trace-Id:abc123" send "hello"
cargo run --bin a2acli -- --transport jsonrpc --transport rest card get
cargo run --bin a2acli -- --a2a-version 1.0 card get
cargo run --bin a2acli -- --insecure --bearer "$TOKEN" card get  # dev only; always warns
cargo run --bin a2acli -- --debug send "hello"                  # request/response diagnostics to stderr
```

`--a2a-version` pins the protocol version signaled on every request. Absent
it, the version is negotiated down to the highest one both `a2acli` and the
agent's selected card interface declare, bounded to 1.x and never below 1.0 —
A2A reads an empty or pre-1.0 value as 0.3, so a version outside 1.x is
refused as a usage error rather than downgraded, and any effective version
other than the tool's own is named on stderr rather than applied silently.

`--bearer`/`--api-key` (env `A2ACLI_BEARER`/`A2ACLI_API_KEY`) supply credentials;
`--svc-param` is a separate, general-purpose transport-level key-value pair,
never itself a credential flag. `--insecure` disables TLS certificate
verification and always prints a warning naming the risk when a credential is
also configured — it never disables verification silently. `--debug` never
prints credential values, regardless of verbosity. `--tenant` supplies a routing tenant when the
selected Agent Card interface declares none; when the interface does declare
one, A2A §8.3.2 requires that declared value to be sent exactly, so it is
used and `--tenant` has no effect.

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

A failure — from the agent or from the tool itself, including a malformed
flag or a missing subcommand — always prints one compact JSON error object
to stderr, in every output mode:

```json
{"error":{"code":"TASK_NOT_FOUND","message":"task not found: t-1","a2aCode":-32001}}
```

A task that the CLI conducted and reported exits `0` even when the agent did
not succeed, so the exit status alone cannot say that. A `FAILED` or
`REJECTED` outcome, and a task paused at `INPUT_REQUIRED`/`AUTH_REQUIRED`,
is therefore also named in a one-line warning on stderr — in every output
mode, leaving stdout exactly the payload. `CANCELED` is not warned about:
after `task cancel` it is the outcome you asked for.

`code` is the A2A protocol's own error name for a protocol failure (with the
numeric `a2aCode` alongside it), or an `A2ACLI_ERR_*` symbol for a failure
the protocol never saw (a bad flag, an unreachable agent, a `--timeout`
expiry). The exit status reports only whether the CLI did its job — `0` even
when a task ends `FAILED`/`REJECTED` or pauses at
`INPUT_REQUIRED`/`AUTH_REQUIRED` — while `1`/`2`/`3`/`4`/`5` distinguish a
generic failure, a usage error, an unreachable agent, a rejected credential,
and a timeout respectively.

### Configuration

Every global option listed above can also be set from the environment or a
`.env` file, so repeated invocations stay short. Precedence, highest wins:

1. an explicit flag,
2. a real environment variable,
3. a local `.env` (found by walking up from the working directory, or the
   file named by `--config <path>`),
4. the global `.env` at `~/.config/a2a-cli/.env` (`$XDG_CONFIG_HOME` honored),
5. the built-in default.

The environment variable name is `A2ACLI_` followed by the long flag name,
upper-snake-cased — `--bearer` → `A2ACLI_BEARER`, `--transport` →
`A2ACLI_TRANSPORT` (comma-separated: `A2ACLI_TRANSPORT=jsonrpc,rest`),
`--context-id` → `A2ACLI_CONTEXT_ID`. The same names work in a `.env` file,
one `KEY=value` per line; blank lines and `#` comments are ignored, a leading
`export ` is tolerated, and one layer of surrounding quotes is stripped:

```dotenv
A2ACLI_AGENT_CARD=https://agent.example.com
A2ACLI_TRANSPORT=rest,jsonrpc
A2ACLI_TIMEOUT=60s
A2ACLI_A2A_VERSION=1.0
# credentials
A2ACLI_BEARER="Bearer <token>"
```

`--stream`, `-h/--help`, and `-v/--version` are never read from the
environment or a file — they must always be passed explicitly.

```sh
cargo run --bin a2acli -- config show                  # inspect effective settings
cargo run --bin a2acli -- config show -o json
cargo run --bin a2acli -- --config ./prod.env config show
```

`config show` is read-only: it prints each effective setting and the source
it resolved from (redacting credential values), so you can confirm
precedence without guessing. Change settings by exporting the variable or
editing a `.env` file directly — the command never mutates anything. `a2acli`
never writes a `.env` file itself, but warns if one it reads is readable by
users other than its owner (mode should be `0600`).

## Conformance

This CLI is tracked against the
[a2a-cli specification](https://github.com/a2aproject/a2a-cli/blob/main/specification/SPEC.md)
Tier 1 ("Core") requirements; see
[a2aproject/a2a-rs#164](https://github.com/a2aproject/a2a-rs/issues/164) for the
current gap list and in-progress work (blocking-by-default `send`/polling,
human-readable `text` output, message parts, and more).