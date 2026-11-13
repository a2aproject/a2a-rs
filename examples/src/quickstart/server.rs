// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! The quickstart agent, served over JSON-RPC and REST.
//!
//! Run:
//!   cargo run --bin quickstart-server --package examples

// --8<-- [start:imports]
use std::sync::Arc;

use a2a_server::agent_card::agent_card_router;
use a2a_server::jsonrpc::jsonrpc_router;
use a2a_server::rest::rest_router;
use a2a_server::{DefaultRequestHandler, InMemoryTaskStore, StaticAgentCard};
use examples_lib::quickstart::{Greeter, agent_card};
// --8<-- [end:imports]

// --8<-- [start:main]
#[tokio::main]
async fn main() {
    // The handler runs the executor and keeps the tasks in the store.
    let handler = Arc::new(DefaultRequestHandler::new(
        Greeter,
        InMemoryTaskStore::new(),
    ));
    let card = Arc::new(StaticAgentCard::new(agent_card("http://localhost:9999")));

    // One route per binding, and the card under /.well-known/.
    let app = axum::Router::new()
        .nest("/jsonrpc", jsonrpc_router(handler.clone()))
        .nest("/rest", rest_router(handler))
        .merge(agent_card_router(card));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:9999")
        .await
        .unwrap();
    println!("Greeter listening on http://localhost:9999");
    axum::serve(listener, app).await.unwrap();
}
// --8<-- [end:main]
