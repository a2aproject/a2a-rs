// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Hello World A2A server (plain TCP).
//!
//! Run:
//!   cargo run --bin helloworld-server --package examples

use std::future::IntoFuture;
use std::sync::Arc;

use a2a::*;
use a2a_grpc::GrpcHandler;
use a2a_pb::proto::a2a_service_server::A2aServiceServer;
use a2a_server::*;
use examples_lib::{EchoExecutor, build_agent_card};
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server as TonicServer;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let handler = Arc::new(DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new()));
    let agent_card = build_agent_card(vec![
        AgentInterface::new("http://localhost:3000/jsonrpc", TRANSPORT_PROTOCOL_JSONRPC),
        AgentInterface::new("http://localhost:3000/rest", TRANSPORT_PROTOCOL_HTTP_JSON),
        AgentInterface::new("http://localhost:50051", TRANSPORT_PROTOCOL_GRPC),
    ]);
    let card_producer = Arc::new(StaticAgentCard::new(agent_card));

    let app = axum::Router::new()
        .nest("/jsonrpc", a2a_server::jsonrpc::jsonrpc_router(handler.clone()))
        .nest("/rest", a2a_server::rest::rest_router(handler.clone()))
        .merge(a2a_server::agent_card::agent_card_router(card_producer));

    let grpc_service = A2aServiceServer::new(GrpcHandler::new(handler));

    tracing::info!("Hello World Agent starting");
    tracing::info!("Agent card:  http://localhost:3000/.well-known/agent-card.json");
    tracing::info!("JSON-RPC:    http://localhost:3000/jsonrpc");
    tracing::info!("REST:        http://localhost:3000/rest");
    tracing::info!("gRPC:        http://localhost:50051");

    let http_listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    let grpc_listener = tokio::net::TcpListener::bind("0.0.0.0:50051").await.unwrap();

    tokio::select! {
        result = axum::serve(http_listener, app).into_future() => {
            if let Err(e) = result { tracing::error!(error = %e, "HTTP server exited"); }
        }
        result = TonicServer::builder()
            .add_service(grpc_service)
            .serve_with_incoming(TcpListenerStream::new(grpc_listener)) => {
            if let Err(e) = result { tracing::error!(error = %e, "gRPC server exited"); }
        }
    }
}
