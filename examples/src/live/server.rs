// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Run `cargo run -p examples --bin live-server`, then run `live-client`.

use std::{future::IntoFuture, sync::Arc};

use a2a::*;
use a2a_grpc::GrpcHandler;
use a2a_pb::proto::a2a_service_server::A2aServiceServer;
use a2a_server::{DefaultRequestHandler, InMemoryTaskStore, StaticAgentCard};
use examples_lib::{build_agent_card, live::LiveEchoExecutor};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Optional addresses let tests and multiple local examples use other ports.
    let mut args = std::env::args().skip(1);
    let http = TcpListener::bind(args.next().as_deref().unwrap_or("127.0.0.1:3001")).await?;
    let grpc = TcpListener::bind(args.next().as_deref().unwrap_or("127.0.0.1:50053")).await?;
    let http_url = format!("http://{}", http.local_addr()?);
    let grpc_url = format!("http://{}", grpc.local_addr()?);
    let mut card = build_agent_card(vec![
        AgentInterface::new(format!("{http_url}/jsonrpc"), TRANSPORT_PROTOCOL_JSONRPC),
        AgentInterface::new(format!("{http_url}/rest"), TRANSPORT_PROTOCOL_HTTP_JSON),
        AgentInterface::new(grpc_url.clone(), TRANSPORT_PROTOCOL_GRPC),
    ]);
    card.name = "Live Echo Agent".into();
    card.description = "Echo live input; pause and resume the same task, then finish.".into();
    card.skills[0].examples = Some(vec!["hello".into(), "pause".into(), "finish".into()]);
    let handler = Arc::new(
        DefaultRequestHandler::new(LiveEchoExecutor, InMemoryTaskStore::new())
            .with_capabilities(card.capabilities.clone()),
    );
    let app = axum::Router::new()
        .nest(
            "/jsonrpc",
            a2a_server::jsonrpc::jsonrpc_router(handler.clone()),
        )
        .nest("/rest", a2a_server::rest::rest_router(handler.clone()))
        .merge(a2a_server::agent_card::agent_card_router(Arc::new(
            StaticAgentCard::new(card),
        )));
    println!("HTTP={http_url}");
    println!("GRPC={grpc_url}");
    tokio::select! {
        result = axum::serve(http, app).into_future() => result?,
        result = tonic::transport::Server::builder()
            .add_service(A2aServiceServer::new(GrpcHandler::new(handler)))
            .serve_with_incoming(TcpListenerStream::new(grpc)) => result?,
    }
    Ok(())
}
