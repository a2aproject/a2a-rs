# a2acli skill

Use this skill to interact with A2A-compatible agents from the command line using the `a2a` binary.

## When to use

- Inspect an agent's capabilities by fetching its agent card
- Send one-shot or streaming messages to an A2A agent
- Retrieve, list, cancel, or subscribe to tasks on an agent
- Manage push notification configs for tasks
- Diagnose A2A agent deployments or script A2A workflows in a shell

## Install

```sh
# macOS
brew tap a2aproject/a2a-rs https://github.com/a2aproject/a2a-rs
brew trust a2aproject/a2a-rs
brew install a2acli

# Linux
curl -fsSL https://raw.githubusercontent.com/a2aproject/a2a-rs/main/install.sh | bash

# From source
cargo install a2a-cli
```

## Usage patterns

### Agent reference (`AGENT_REF`)

Every command takes an `AGENT_REF` as its first positional argument:

| Value | Behaviour |
| --- | --- |
| `http://host` or `https://host` | Card is fetched from `<URL>/.well-known/agent-card.json`. |
| Any `http`/`https` URL ending in `.json` | Used as-is — a direct link to the agent card JSON. |
| Any other value | Treated as a local filesystem path and read directly. |

```sh
a2a discover http://localhost:3000                        # base URL
a2a discover https://example.com/agents/my-agent.json    # direct card URL
a2a discover ./agent-card.json                           # local file
```

### Discover an agent

```sh
a2a discover <AGENT_REF>
a2a discover <AGENT_REF> --extended
```

### Send a message

```sh
# One-shot — prints the completed task
a2a send <AGENT_REF> "<message>"

# With context and task identity
a2a send <AGENT_REF> "<message>" --context-id <ctx> --task-id <task>

# Streaming — prints events as they arrive
a2a stream <AGENT_REF> "<message>"
```

### Manage tasks

```sh
a2a task get    <AGENT_REF> <TASK_ID>
a2a task list   <AGENT_REF> [--status completed] [--context-id <ctx>]
a2a task cancel <AGENT_REF> <TASK_ID>
a2a task subscribe <AGENT_REF> <TASK_ID>   # stream live events
```

### Push notification configs

```sh
a2a push-config create <AGENT_REF> <TASK_ID> <CALLBACK_URL> \
  --auth-scheme Bearer --auth-credentials <secret>

a2a push-config list   <AGENT_REF> <TASK_ID>
a2a push-config get    <AGENT_REF> <TASK_ID> <CONFIG_ID>
a2a push-config delete <AGENT_REF> <TASK_ID> <CONFIG_ID>
```

## Global flags

| Flag | Env var | Description |
| --- | --- | --- |
| `--enabled-binding jsonrpc\|http-json` | | Pin the transport binding. Repeatable. Auto-negotiated if omitted. |
| `--bearer-token <TOKEN>` | `A2A_BEARER_TOKEN` | Bearer token sent with every request. |
| `--header <Name:Value>` | | Custom HTTP header. Repeatable. |
| `-o pretty\|json` | | Output format. `pretty` = indented JSON (default). `json` = compact, one object per line. |

## Config file

Create `.a2a.yaml` in any ancestor directory (up to `$HOME`) to set defaults:

```yaml
enabled_bindings:
  - http-json
bearer_token: my-token
headers:
  - "X-Tenant: acme"
output: json
```

CLI flags always win over config file values. Headers are additive.

## Pipe-friendly output

Use `-o json` with `jq` for scripting:

```sh
# Get the agent name
a2a -o json discover http://localhost:3000 | jq -r '.name'

# Watch task state from a stream
a2a -o json stream http://localhost:3000 "hello" | jq -r '.statusUpdate.status.state // empty'

# List completed task IDs
a2a -o json task list http://localhost:3000 --status completed | jq -r '.tasks[].id'
```

## Reference

Full documentation: [`a2acli/README.md`](../a2acli/README.md)
