// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Hello World A2A client (plain TCP).
//!
//! Run the server first:
//!   cargo run --bin helloworld-server --package examples
//! Then run this client:
//!   cargo run --bin helloworld-client --package examples

use a2a::*;
use a2a_client::A2AClientFactory;
use a2a_client::agent_card::AgentCardResolver;
use a2a_grpc::GrpcTransportFactory;
use examples_lib::exercise_client;
use std::sync::Arc;

const SERVER_URLS: &[&str] = &["http://localhost:3000"];

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let resolver = AgentCardResolver::new(None);

    let protocols: &[&str] = &[
        TRANSPORT_PROTOCOL_GRPC,
        TRANSPORT_PROTOCOL_JSONRPC,
        TRANSPORT_PROTOCOL_HTTP_JSON,
    ];

    for &url in SERVER_URLS {
        tracing::info!(url, "fetching agent card");
        let card = match resolver.resolve(url).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(url, error = %e, "failed to fetch agent card");
                continue;
            }
        };
        tracing::info!(name = %card.name, version = %card.version, "agent card fetched");

        for &protocol in protocols {
            let factory = A2AClientFactory::builder()
                .register(Arc::new(GrpcTransportFactory::new()))
                .preferred_bindings(vec![protocol.to_string()])
                .build();

            let client = match factory.create_from_card(&card).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[{protocol}] skipped: {e}");
                    continue;
                }
            };

            exercise_client(protocol, &client).await;
        }
    }
}
