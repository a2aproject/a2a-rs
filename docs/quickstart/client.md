# Client quickstart

A client reads an agent's card, connects over one of the bindings the card lists, and calls the agent. This page builds one and runs it against the greeter from the [agent quickstart](agent/index.md), so start that server first, as [Start the server](agent/start-the-server.md) shows.

This is the whole client:

```rust title="examples/src/quickstart/client.rs"
use std::io::Write;

use a2a::event::StreamResponse;
use a2a::*;
use a2a_client::A2AClientFactory;
use a2a_client::agent_card::AgentCardResolver;
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<(), A2AError> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://localhost:9999".to_string());

    // Fetch the agent card, then let the factory pick a binding from it.
    let card = AgentCardResolver::new(None).resolve(&url).await?;
    let client = A2AClientFactory::builder()
        .build()
        .create_from_card(&card)
        .await?;
    println!("Connected to {}", card.name);

    // Send a message and wait for the task to finish.
    match client.send_message(&request("Ada")).await? {
        SendMessageResponse::Task(task) => {
            println!("{:?}: {}", task.status.state, greeting(&task));
        }
        SendMessageResponse::Message(message) => {
            println!("{}", message.text().unwrap_or_default());
        }
    }

    // Send another one and print the answer as it arrives.
    let mut events = client.send_streaming_message(&request("Grace")).await?;
    while let Some(event) = events.next().await {
        if let StreamResponse::ArtifactUpdate(update) = event? {
            for part in &update.artifact.parts {
                print!("{}", part.as_text().unwrap_or_default());
            }
            std::io::stdout().flush().ok();
        }
    }
    println!();

    Ok(())
}
```

## Connect

`AgentCardResolver` fetches the card from `/.well-known/agent-card.json` under the URL. `A2AClientFactory` turns the card into a client: it ranks the card's interfaces against the client's preferred bindings and returns a client for the first transport that connects. JSON-RPC and REST are registered by default, and the `a2a-grpc` and `a2a-slimrpc` crates add the gRPC and SLIM RPC bindings. The greeter's card lists JSON-RPC and REST, so that is what the client talks.

## Send a message

`send_message` waits for the task to finish, or to need input, like the `SendMessage` call in [Call the agent](agent/call-the-agent.md). It returns a `SendMessageResponse`, which is either a `Task` or a `Message`, because an agent may answer with either. The greeter answers with a task, and `greeting` joins the parts of its artifact:

```rust title="examples/src/quickstart/client.rs"
/// A request with one text part.
fn request(text: &str) -> SendMessageRequest {
    SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text(text)]),
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

/// The text of the task's first artifact, the pieces joined.
fn greeting(task: &Task) -> String {
    task.artifacts
        .iter()
        .flatten()
        .take(1)
        .flat_map(|artifact| &artifact.parts)
        .filter_map(Part::as_text)
        .collect()
}
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
