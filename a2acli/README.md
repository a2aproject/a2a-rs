# a2acli

Standalone A2A CLI client built on top of `a2a-client`. Published as `a2a-cli`; installs the `a2a` binary.

## Install

**macOS** — [Homebrew](https://brew.sh):

```sh
brew tap a2aproject/a2a-rs https://github.com/a2aproject/a2a-rs
brew trust a2aproject/a2a-rs
brew install a2acli
```

**Linux** — install script:

```sh
curl -fsSL https://raw.githubusercontent.com/a2aproject/a2a-rs/main/install.sh | bash
```

Installs to `/usr/local/bin` as root or `~/.local/bin` otherwise. Override the destination with `A2A_CLI_INSTALL_DIR`.

**Windows** — [WinGet](https://learn.microsoft.com/en-us/windows/package-manager/winget/):

```sh
winget install a2aproject.a2acli
```

**From source** — via [crates.io](https://crates.io/crates/a2a-cli):

```sh
cargo install a2a-cli
```

**From workspace checkout:**

```sh
cargo install --path a2acli
```

## Quick start

Every command takes an `AGENT_REF` as its first positional argument. This can be:

- A base URL — the card is fetched from `<URL>/.well-known/agent-card.json`
- A direct URL ending in `.json` — used as-is
- A local file path — read from disk

```sh
# Base URL
a2a discover http://localhost:3000

# Direct URL to a card JSON
a2a discover https://example.com/agents/my-agent.json

# Local file
a2a discover ./agent-card.json

# Send a message (same AGENT_REF forms apply to all commands)
a2a send http://localhost:3000 "hello"
a2a stream http://localhost:3000 "hello"
a2a task list http://localhost:3000
a2a task get http://localhost:3000 <task-id>
```

## Global options

These options apply to every subcommand.

| Flag | Env var | Description |
| --- | --- | --- |
| `--enabled-binding <BINDING>` | | Pin the transport. `jsonrpc` or `http-json`. Repeatable. Auto-negotiated from the agent card if omitted. |
| `--bearer-token <TOKEN>` | `A2A_BEARER_TOKEN` | Adds `Authorization: Bearer <TOKEN>` to all requests. |
| `--header <Name:Value>` | | Adds a custom HTTP header to all requests. Repeatable. |
| `-o, --output <FORMAT>` | | Output format: `pretty` (default, indented JSON) or `json` (compact, one object per line). |

## Config file

The CLI searches for `.a2a.yaml` starting from the current directory and walking up to `$HOME`. You can also place one in `$HOME/.a2a.yaml` as a fallback. CLI flags always override config file values; headers are additive (config headers come first, then CLI headers).

```yaml
# .a2a.yaml
enabled_bindings:
  - http-json          # jsonrpc | http-json
bearer_token: my-token
headers:
  - "X-Tenant: acme"
output: json           # pretty | json
```

## Commands

### `discover`

Fetch and print an agent's public card (or extended card with `--extended`).

```sh
a2a discover <AGENT_REF> [--extended]
```

`AGENT_REF` is resolved as follows:

| Value | Behaviour |
| --- | --- |
| `http://host` or `https://host` | Appends `/.well-known/agent-card.json` and fetches it. |
| Any `http`/`https` URL ending in `.json` | Used as-is — a direct link to the agent card JSON. |
| Any other value | Treated as a local filesystem path and read directly. |

```sh
# Base URL — card is fetched from /.well-known/agent-card.json
a2a discover http://localhost:3000

# Direct URL to the card JSON
a2a discover https://example.com/cards/my-agent.json

# Local file
a2a discover ./agent-card.json
a2a discover /etc/a2a/staging-card.json

# Extended card (requires the agent to support it)
a2a discover http://localhost:3000 --extended
```

### `send`

Send a one-shot message and print the resulting task.

```sh
a2a send <AGENT_REF> <TEXT> [OPTIONS]
```

| Option | Description |
| --- | --- |
| `--context-id <ID>` | Attach the message to an existing context. |
| `--task-id <ID>` | Attach the message to an existing task. |
| `--history-length <N>` | Number of history turns to request. |
| `--accept-output <MIME>` | Accepted output modes. Repeatable. |
| `--return-immediately` | Ask the agent to return immediately without waiting for completion. |

```sh
a2a send http://localhost:3000 "summarise this report"
a2a send http://localhost:3000 "continue" --task-id task-abc --context-id ctx-1
a2a send http://localhost:3000 "export" --accept-output text/plain --accept-output application/pdf
```

### `stream`

Send a message and stream the response events. Each event is printed as it arrives.

```sh
a2a stream <AGENT_REF> <TEXT> [OPTIONS]
```

Accepts the same options as `send` except `--return-immediately`.

```sh
a2a stream http://localhost:3000 "write me a blog post"
a2a -o json stream http://localhost:3000 "hello" --task-id task-abc
```

Use `-o json` to get one JSON object per line, which is convenient for piping to `jq`.

### `task`

Manage tasks on an agent.

#### `task get`

```sh
a2a task get <AGENT_REF> <TASK_ID> [--history-length <N>]
```

#### `task list`

```sh
a2a task list <AGENT_REF> [OPTIONS]
```

| Option | Description |
| --- | --- |
| `--context-id <ID>` | Filter by context. |
| `--status <STATE>` | Filter by state. Values: `submitted`, `working`, `completed`, `failed`, `canceled`, `input-required`, `rejected`, `auth-required`. |
| `--page-size <N>` | Maximum number of tasks to return. |
| `--page-token <TOKEN>` | Pagination cursor from a previous response. |
| `--history-length <N>` | Number of history turns to include. |
| `--include-artifacts` | Include task artifacts in the response. |

```sh
a2a task list http://localhost:3000
a2a task list http://localhost:3000 --status completed --context-id ctx-1
```

#### `task cancel`

```sh
a2a task cancel <AGENT_REF> <TASK_ID>
```

#### `task subscribe`

Stream live status-update events for a task until it reaches a terminal state.

```sh
a2a task subscribe <AGENT_REF> <TASK_ID>
```

```sh
a2a -o json task subscribe http://localhost:3000 task-abc | jq '.statusUpdate.status.state'
```

### `push-config`

Manage push notification configs for tasks.

#### `push-config create`

```sh
a2a push-config create <AGENT_REF> <TASK_ID> <CALLBACK_URL> [OPTIONS]
```

| Option | Description |
| --- | --- |
| `--config-id <ID>` | Assign a specific ID to the config (server-generated if omitted). |
| `--token <TOKEN>` | Opaque token forwarded in push notifications. |
| `--auth-scheme <SCHEME>` | Authentication scheme for the callback (e.g. `Bearer`). Requires `--auth-credentials`. |
| `--auth-credentials <CREDS>` | Credentials for the callback endpoint. Requires `--auth-scheme`. |

```sh
a2a push-config create http://localhost:3000 task-abc https://my.service/webhook \
  --config-id cfg-1 \
  --auth-scheme Bearer \
  --auth-credentials my-secret
```

#### `push-config get`

```sh
a2a push-config get <AGENT_REF> <TASK_ID> <CONFIG_ID>
```

#### `push-config list`

```sh
a2a push-config list <AGENT_REF> <TASK_ID> [--page-size <N>] [--page-token <TOKEN>]
```

#### `push-config delete`

```sh
a2a push-config delete <AGENT_REF> <TASK_ID> <CONFIG_ID>
```

## Authentication

### Bearer token

```sh
# via flag
a2a --bearer-token my-token discover http://localhost:3000

# via environment variable
export A2A_BEARER_TOKEN=my-token
a2a discover http://localhost:3000
```

### Custom headers

```sh
a2a --header "X-Api-Key: secret" --header "X-Tenant: acme" send http://localhost:3000 "hello"
```

## Output format

By default responses are printed as indented (pretty-printed) JSON. Pass `-o json` for compact, one-object-per-line output.

```sh
# Pretty (default)
a2a discover http://localhost:3000

# Compact — pipe-friendly
a2a -o json discover http://localhost:3000 | jq '.name'

# Streaming compact — one event per line
a2a -o json stream http://localhost:3000 "hello" | jq '.statusUpdate.status.state'
```

## Transport negotiation

By default the CLI reads the agent card, discovers the supported interfaces, and picks a binding automatically (preferring JSON-RPC). Use `--enabled-binding` to restrict which binding is considered:

```sh
a2a --enabled-binding http-json send http://localhost:3000 "hello"
a2a --enabled-binding jsonrpc   send http://localhost:3000 "hello"
```

`--enabled-binding` may be repeated; the CLI will use whichever of those bindings the agent card also advertises.
