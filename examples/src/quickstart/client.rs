// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! A client for the quickstart agent.
//!
//! Start the agent first:
//!   cargo run --bin quickstart-server --package examples
//! Then run this client:
//!   cargo run --bin quickstart-client --package examples
//! It takes the agent's URL as an optional argument.

// --8<-- [start:imports]
use std::io::Write;

use a2a::event::StreamResponse;
use a2a::*;
use a2a_client::A2AClientFactory;
use a2a_client::agent_card::AgentCardResolver;
use futures::StreamExt;
// --8<-- [end:imports]

// --8<-- [start:main]
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
// --8<-- [end:main]

// --8<-- [start:helpers]
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
// --8<-- [end:helpers]
