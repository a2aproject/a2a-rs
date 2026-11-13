# Start the server

`DefaultRequestHandler` does the protocol work: it runs the executor, keeps the task up to date from the events and builds the responses. The server mounts it on routes.

```rust title="examples/src/quickstart/server.rs"
--8<-- "examples/src/quickstart/server.rs:imports"

--8<-- "examples/src/quickstart/server.rs:main"
```

- `InMemoryTaskStore` keeps the tasks in memory, so they are lost when the server stops. Any implementation of the `TaskStore` trait can replace it.
- `jsonrpc_router` and `rest_router` each serve one binding of the same handler, under their own prefix. The prefixes match the URLs in the agent card.
- `agent_card_router` serves the card at `/.well-known/agent-card.json`.
- The server listens on `127.0.0.1`, so only your machine can reach it. The base URL in the card is written in `main`; pass the real one to `agent_card` when you serve the agent elsewhere.

## Run it

```bash
cargo run --bin quickstart-server --package examples
```

Wait for this line:

```text
Greeter listening on http://localhost:9999
```

Leave the server running and open a second terminal. Fetch the card:

```bash
curl -s http://localhost:9999/.well-known/agent-card.json | jq .
```

```json
{
  "name": "Greeter",
  "description": "Greets you by name. Asks for the name first if it was not given.",
  "version": "0.1.0",
  "supportedInterfaces": [
    {
      "url": "http://localhost:9999/jsonrpc",
      "protocolBinding": "JSONRPC",
      "protocolVersion": "1.0"
    },
    {
      "url": "http://localhost:9999/rest",
      "protocolBinding": "HTTP+JSON",
      "protocolVersion": "1.0"
    }
  ],
  "capabilities": {
    "streaming": true,
    "pushNotifications": false
  },
  "defaultInputModes": [
    "text/plain"
  ],
  "defaultOutputModes": [
    "text/plain"
  ],
  "skills": [
    {
      "id": "greet",
      "name": "Greet",
      "description": "Greets the name in the message.",
      "tags": [
        "greeting"
      ],
      "examples": [
        "Ada"
      ]
    }
  ]
}
```

The card is the one `agent_card` built, in the JSON the protocol defines: the Rust field names become `camelCase`, and the fields left `None` are omitted.

Next, [call the agent](call-the-agent.md).
