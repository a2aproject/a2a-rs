# Next steps

You wrote an agent with a card, an executor and a server, called it over JSON-RPC and REST, streamed its answer and held a conversation over two messages.

## Try these in the code

- Change the greeting in the executor, restart the server and call it again to see the new greeting.
- Add a second skill to the card and handle it in the executor.
- Replace `InMemoryTaskStore` with an implementation of the `TaskStore` trait that survives a restart.
- Serve a third binding. The hello world example, [`examples/src/helloworld/server.rs`](https://github.com/a2aproject/a2a-rs/blob/main/examples/src/helloworld/server.rs), also serves gRPC.

## Read on

- The [client quickstart](../client.md) calls the agent from Rust.
- The [`a2acli` quickstart](../a2acli.md) calls it from the command line, and the [`a2acli` reference](../../reference/a2acli.md) covers the rest of the CLI.
- The API reference of the crates this page used is on docs.rs: [`a2a-lf`](https://docs.rs/a2a-lf) for the protocol types and [`a2a-server-lf`](https://docs.rs/a2a-server-lf) for the server.
- [a2a-protocol.org](https://a2a-protocol.org/latest/) describes the protocol itself.
