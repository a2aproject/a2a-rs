// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Runs the same live conversation over every advertised example binding.

use std::{sync::Arc, time::Duration};

use a2a::*;
use a2a_client::{A2AClientFactory, agent_card::AgentCardResolver};
use a2a_grpc::GrpcTransportFactory;
use examples_lib::live::converse;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:3001".into());
    let card = AgentCardResolver::new(None).resolve(&url).await?;
    for protocol in [
        TRANSPORT_PROTOCOL_JSONRPC,
        TRANSPORT_PROTOCOL_HTTP_JSON,
        TRANSPORT_PROTOCOL_GRPC,
    ] {
        let client = A2AClientFactory::builder()
            .register(Arc::new(GrpcTransportFactory::new()))
            .preferred_bindings(vec![protocol.into()])
            .build()
            .create_from_card(&card)
            .await?;
        let events = tokio::time::timeout(Duration::from_secs(30), converse(&client)).await??;
        println!("{protocol}");
        for event in events {
            match event {
                StreamResponse::Task(task) => println!("  task {}", task.id),
                StreamResponse::StatusUpdate(update) => println!(
                    "  {:?}: {}",
                    update.status.state,
                    update
                        .status
                        .message
                        .as_ref()
                        .and_then(Message::text)
                        .unwrap_or("")
                ),
                StreamResponse::MessageUpdate(update) => {
                    println!("  accepted: {}", update.message.text().unwrap_or(""))
                }
                _ => {}
            }
        }
    }
    Ok(())
}
