# A2A Rust SDK

`a2a-rs` is the Rust SDK for the [Agent2Agent (A2A) protocol](https://a2a-protocol.org/latest/), an open protocol that lets AI agents discover each other and work together, whatever framework or language they are built with. The SDK implements A2A v1: the protocol types, an async server framework for building agents, an async client for calling them, and a command-line client.

It speaks the protocol over four bindings, and an agent can serve several at once:

- JSON-RPC 2.0 over HTTP
- REST (HTTP+JSON)
- gRPC
- SLIM RPC

Streaming responses use Server-Sent Events on JSON-RPC and REST, and native streams on gRPC.

<div class="grid cards" markdown>

- **Build an agent**

    ---

    Write an agent card and an executor, then serve the agent over JSON-RPC and REST. The agent streams its answer and asks for input.

    [:octicons-arrow-right-24: Agent quickstart](quickstart/agent/index.md)

- **Call an agent**

    ---

    Resolve an agent's card, connect over the best binding it offers, then send a message and stream the reply.

    [:octicons-arrow-right-24: Client quickstart](quickstart/client.md)

- **Use the command line**

    ---

    Inspect cards, send messages and manage tasks with `a2acli`, without writing any code.

    [:octicons-arrow-right-24: `a2acli` quickstart](quickstart/a2acli.md)

</div>

## Add the SDK to a project

The crates are on [crates.io](https://crates.io). The core crates are published under `-lf` names, but their library names are plain, so `a2a-lf` is imported as `a2a`.

=== "Agent"

    ```bash
    cargo add a2a-lf a2a-server-lf
    ```

=== "Client"

    ```bash
    cargo add a2a-lf a2a-client-lf
    ```

Add `a2a-grpc` or `a2a-slimrpc` to serve or call the other bindings. A client created with `a2a-client-lf` handles JSON-RPC and REST by default. The [install page](install.md) covers the TLS features, the development version, the Rust version and how to install `a2acli`.

## The crates

| Crate | Library | What it is for |
|---|---|---|
| [`a2a-lf`](https://docs.rs/a2a-lf) | `a2a` | Protocol types, errors, events and JSON-RPC types, with wire-compatible serde behavior. |
| [`a2a-server-lf`](https://docs.rs/a2a-server-lf) | `a2a_server` | The server framework: request handler, task stores and the REST and JSON-RPC routers, built on `axum`. |
| [`a2a-client-lf`](https://docs.rs/a2a-client-lf) | `a2a_client` | The client: agent card resolution and a factory that picks a binding from the card. |
| [`a2a-grpc`](https://docs.rs/a2a-grpc) | `a2a_grpc` | The gRPC client and server bindings, built on `tonic`. |
| [`a2a-slimrpc`](https://docs.rs/a2a-slimrpc) | `a2a_slimrpc` | The SLIM RPC client and server bindings. |
| [`a2a-pb`](https://docs.rs/a2a-pb) | `a2a_pb` | The protobuf schema, the generated types and conversions to and from the native types. |
| [`a2a-cli`](https://crates.io/crates/a2a-cli) | `a2acli` | The `a2acli` command-line client. |

The crates are versioned independently. The REST and JSON-RPC bindings are meant to stay wire-compatible with the other A2A SDKs, such as Go and C#.

## What an agent looks like

The hello world agent in the `examples` crate mounts its bindings like this:

```rust
--8<-- "examples/src/helloworld/server.rs:routers"
```

The [agent quickstart](quickstart/agent/index.md) builds a complete agent from the ground up.

## Where to read more

- The API reference of each crate is on [docs.rs](https://docs.rs). The table above links to each one.
- [`a2acli` command reference](reference/a2acli.md).
- The protocol itself, with its concepts, is described on [a2a-protocol.org](https://a2a-protocol.org/latest/).
- The source, issues and the [contributing guide](https://github.com/a2aproject/a2a-rs/blob/main/CONTRIBUTING.md) are in the [a2aproject/a2a-rs](https://github.com/a2aproject/a2a-rs) repository.
