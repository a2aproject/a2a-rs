// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Programmatic collaborative task client for the e-commerce example.
//!
//! Demonstrates client-side use of the [`a2a_collab`] crate:
//! - [`a2a_collab::require_collaborative_extensions`] pre-flight-checks each
//!   agent's card before opening any stream.
//! - [`a2a_collab::translate_peer_response`] translates every `StreamResponse`
//!   from one agent into a `SendMessageRequest` delivered to every other agent
//!   (the relay loop, spec §2.1 / §5.1).
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
use a2a_collab::{require_collaborative_extensions, translate_peer_response};
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

    struct Session {
        name: String,
        client: a2a_client::A2AClient<Box<dyn a2a_client::Transport>>,
    }

    // --- pre-flight: fetch cards, verify extensions, build clients -----------
    let mut sessions: Vec<Session> = Vec::with_capacity(AGENTS.len());
    for &url in AGENTS {
        let card = resolver.resolve(url).await?;
        // Fails here if the card does not declare both collaborative-task and
        // shared-task extension URIs — prevents a half-open session later.
        require_collaborative_extensions(&card)?;
        let name = card.name.clone();
        let client = factory.create_from_card(&card).await?;
        println!("connected: {name} ({url})");
        sessions.push(Session { name, client });
    }
    println!("\nprompt: {PROMPT}\n");

    // --- open one streaming connection per agent -----------------------------
    let initial = SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text(PROMPT)]),
        configuration: None,
        metadata: None,
        tenant: None,
    };

    type TaggedEvent = (usize, Result<StreamResponse, A2AError>);
    let mut all_streams: Vec<futures::stream::BoxStream<'static, TaggedEvent>> =
        Vec::with_capacity(sessions.len());

    for (idx, session) in sessions.iter().enumerate() {
        let stream = session.client.send_streaming_message(&initial).await?;
        all_streams.push(Box::pin(stream.map(move |ev| (idx, ev))));
    }

    // --- relay loop ----------------------------------------------------------
    // Per-agent session IDs: populated from the first Task event each agent
    // sends back. We need them before we can relay peer items to that agent.
    let mut context_ids: Vec<Option<String>> = vec![None; sessions.len()];
    let mut task_ids: Vec<Option<String>> = vec![None; sessions.len()];

    let deadline = tokio::time::Instant::now() + TIMEOUT;
    let mut merged = futures::stream::select_all(all_streams);

    loop {
        match tokio::time::timeout_at(deadline, merged.next()).await {
            Err(_elapsed) => {
                println!("\n[session ended: timeout after {TIMEOUT:?}]");
                break;
            }
            Ok(None) => break,
            Ok(Some((src, Err(err)))) => {
                eprintln!("[{}] stream error: {err}", sessions[src].name);
                break;
            }
            Ok(Some((src, Ok(event)))) => {
                // Cache session IDs from the first Task event.
                if let StreamResponse::Task(task) = &event
                    && context_ids[src].is_none()
                {
                    context_ids[src] = Some(task.context_id.clone());
                    task_ids[src] = Some(task.id.clone());
                }

                // Print each line prefixed with the originating agent's name.
                let label = &sessions[src].name;
                for line in render(&event).lines() {
                    println!("[{label}] {line}");
                }

                // Relay to every other agent that has an established session.
                let sender = sessions[src].name.clone();
                for dst in 0..sessions.len() {
                    if dst == src {
                        continue; // Echo suppression: never route back to origin.
                    }
                    let (Some(ctx), Some(tid)) = (&context_ids[dst], &task_ids[dst]) else {
                        continue; // No session ID yet — skip until first Task arrives.
                    };
                    let Some(mut relay) = translate_peer_response(&event, &sender, ctx, tid) else {
                        continue;
                    };
                    // return_immediately=true: the receiving agent absorbs the
                    // relay message inline while its own stream is still running.
                    relay.configuration = Some(SendMessageConfiguration {
                        return_immediately: Some(true),
                        accepted_output_modes: None,
                        task_push_notification_config: None,
                        history_length: None,
                    });
                    if let Err(err) = sessions[dst].client.send_message(&relay).await {
                        eprintln!("warning: relay to [{}] failed: {err}", sessions[dst].name);
                    }
                }
            }
        }
    }

    for session in sessions {
        let _ = session.client.destroy().await;
    }
    Ok(())
}

/// Minimal text rendering for a `StreamResponse` — avoids pulling in the
/// full a2acli formatting machinery.
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
