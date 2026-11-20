// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Programmatic collaborative task client for the e-commerce example.
//!
//! Demonstrates client-side use of the [`a2a_collab`] crate:
//! - [`a2a_collab::require_collaborative_extensions`] pre-flight-checks each
//!   agent's card before opening any stream.
//! - [`a2a_collab::CollaborativeSession`] handles stream merging, session ID
//!   caching, echo suppression, and relay dispatch (spec §2.1 / §5.1).
//!
//! # Running
//!
//! Start each agent server in a separate terminal first:
//! ```text
//! cargo run --bin collaborate-orders    --package examples
//! cargo run --bin collaborate-inventory --package examples
//! cargo run --bin collaborate-shipping  --package examples
//! ```
//!
//! Then run this client:
//! ```text
//! cargo run --bin collaborate-client --package examples
//! ```

use std::time::Duration;

use a2a::*;
use a2a_client::A2AClientFactory;
use a2a_client::agent_card::AgentCardResolver;
use a2a_collab::{CollaborativeSession, require_collaborative_extensions};
use futures::StreamExt;

const AGENTS: &[&str] = &[
    "http://localhost:9001",
    "http://localhost:9002",
    "http://localhost:9003",
];

const PROMPT: &str = "Customer ORD-1001 has not received their order — what is going on?";

const TIMEOUT: Duration = Duration::from_secs(30);

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();
    if let Err(err) = run().await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), A2AError> {
    let resolver = AgentCardResolver::new(None);
    let factory = A2AClientFactory::builder().build();

    // Pre-flight: fetch cards, verify extensions, build clients.
    let mut agents = Vec::with_capacity(AGENTS.len());
    for &url in AGENTS {
        let card = resolver.resolve(url).await?;
        require_collaborative_extensions(&card)?;
        let name = card.name.clone();
        let client = factory.create_from_card(&card).await?;
        println!("connected: {name} ({url})");
        agents.push((name, client));
    }
    println!("\nprompt: {PROMPT}\n");

    let initial = SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text(PROMPT)]),
        configuration: None,
        metadata: None,
        tenant: None,
    };

    let mut stream = CollaborativeSession::new(agents).start(initial).await?;

    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        match tokio::time::timeout_at(deadline, stream.next()).await {
            Err(_elapsed) => {
                println!("\n[session ended: timeout after {TIMEOUT:?}]");
                break;
            }
            Ok(None) => break,
            Ok(Some((label, event))) => {
                for line in render(&event).lines() {
                    println!("[{label}] {line}");
                }
            }
        }
    }

    Ok(())
}

/// Minimal text rendering for a `StreamResponse`.
fn render(event: &StreamResponse) -> String {
    match event {
        StreamResponse::Task(task) => {
            format!(
                "task {} — {:?}",
                &task.id[task.id.len().saturating_sub(8)..],
                task.status.state
            )
        }
        StreamResponse::StatusUpdate(update) => {
            if let Some(msg) = &update.status.message {
                msg.parts
                    .iter()
                    .filter_map(|p| {
                        if let PartContent::Text(t) = &p.content {
                            Some(t.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            } else {
                format!("status: {:?}", update.status.state)
            }
        }
        StreamResponse::ArtifactUpdate(update) => {
            format!("artifact: {}", update.artifact.artifact_id)
        }
        StreamResponse::Message(msg) => msg
            .parts
            .iter()
            .filter_map(|p| {
                if let PartContent::Text(t) = &p.content {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}
