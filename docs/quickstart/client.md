# Client quickstart

A client reads an agent's card, connects over one of the bindings the card lists, and calls the agent. This page builds one and runs it against the greeter from the [agent quickstart](agent/index.md), so start that server first, as [Start the server](agent/start-the-server.md) shows.

This is the whole client:

```rust title="examples/src/quickstart/client.rs"
--8<-- "examples/src/quickstart/client.rs:imports"

--8<-- "examples/src/quickstart/client.rs:main"
```

## Connect

`AgentCardResolver` fetches the card from `/.well-known/agent-card.json` under the URL. `A2AClientFactory` turns the card into a client: it ranks the card's interfaces against the client's preferred bindings and returns a client for the first transport that connects. JSON-RPC and REST are registered by default, and the `a2a-grpc` and `a2a-slimrpc` crates add the gRPC and SLIM RPC bindings. The greeter's card lists JSON-RPC and REST, so that is what the client talks.

## Send a message

`send_message` waits for the task to finish, or to need input, like the `SendMessage` call in [Call the agent](agent/call-the-agent.md). It returns a `SendMessageResponse`, which is either a `Task` or a `Message`, because an agent may answer with either. The greeter answers with a task, and `greeting` joins the parts of its artifact:

```rust title="examples/src/quickstart/client.rs"
--8<-- "examples/src/quickstart/client.rs:helpers"
```

## Stream a response

`send_streaming_message` returns a stream of events: the task, messages, status updates and artifact updates. The client prints each piece of the greeting as its artifact update arrives, which is the streaming the agent quickstart showed with `curl`.

## Run it

With the server running, in a second terminal:

```bash
cargo run --bin quickstart-client --package examples
```

```text
Connected to Greeter
Completed: Hello, Ada!
Hello, Grace!
```

`Hello, Grace!` appears piece by piece, over about a second and a half. The client takes the agent's URL as an optional argument, for an agent that is not on `localhost:9999`:

```bash
cargo run --bin quickstart-client --package examples -- http://localhost:9999
```

Next, [`a2acli`](a2acli.md) does the same from the command line.
