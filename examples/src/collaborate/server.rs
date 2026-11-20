// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Shared server startup for all three e-commerce collaborative agents.

use std::sync::Arc;

use a2a_server::agent_card::agent_card_router;
use a2a_server::jsonrpc::jsonrpc_router;
use a2a_server::rest::rest_router;
use a2a_server::{DefaultRequestHandler, InMemoryTaskStore, StaticAgentCard};

use super::agent::{agent_card, CollabExecutor};
use super::fan_in::{FanInHandler, InboxRegistry};

/// Start one of the three e-commerce collaborative agents.
///
/// The binary entry points (`collaborate-orders`, `collaborate-inventory`,
/// `collaborate-shipping`) each call this with their own parameters.
pub async fn run_server(role: &str, description: &str, port: u16) {
    let base_url = format!("http://localhost:{port}");
    let card = agent_card(role, description, &base_url);

    // Share the inbox registry between the executor and the handler.
    let inbox = Arc::new(InboxRegistry::new());

    let executor = match role {
        "orders" => CollabExecutor::orders(inbox.clone()),
        "inventory" => CollabExecutor::inventory(inbox.clone()),
        _ => CollabExecutor::shipping(inbox.clone()),
    };

    let inner = DefaultRequestHandler::new(executor, InMemoryTaskStore::new());
    let handler = Arc::new(FanInHandler::new(inner, inbox));
    let card = Arc::new(StaticAgentCard::new(card));

    let app = axum::Router::new()
        .nest("/jsonrpc", jsonrpc_router(handler.clone()))
        .nest("/rest", rest_router(handler))
        .merge(agent_card_router(card));

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .expect("failed to bind");
    println!("[{role}] listening on http://localhost:{port}");
    axum::serve(listener, app).await.unwrap();
}
