// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Hello World A2A server with TLS (rustls).
//!
//! Run:
//!   cargo run --bin helloworld-tls-server --package examples

use std::sync::Arc;

use a2a::*;
use a2a_grpc::GrpcHandler;
use a2a_pb::proto::a2a_service_server::A2aServiceServer;
use a2a_server::tls::{axum_server, rustls};
use a2a_server::*;
use examples_lib::{EchoExecutor, build_agent_card};
use tonic::transport::Server as TonicServer;
use tonic::transport::server::TcpIncoming;
use tonic_tls::rustls::TlsIncoming;

const SERVER_CERT_PEM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/server.pem"));
const SERVER_KEY_PEM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/server.key"));

fn load_cert_and_key() -> (
    Vec<rustls::pki_types::CertificateDer<'static>>,
    rustls::pki_types::PrivateKeyDer<'static>,
) {
    let certs: Vec<_> = rustls_pemfile::certs(&mut &SERVER_CERT_PEM[..])
        .collect::<Result<_, _>>()
        .expect("parse certs");
    let key = rustls_pemfile::private_key(&mut &SERVER_KEY_PEM[..])
        .expect("parse key")
        .expect("no private key found");
    (certs, key)
}

fn load_rustls_config() -> Arc<rustls::ServerConfig> {
    let (certs, key) = load_cert_and_key();
    Arc::new(
        rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs, key)
            .expect("invalid cert/key"),
    )
}

fn load_grpc_tls_config() -> Arc<tokio_rustls::rustls::ServerConfig> {
    let (certs, key) = load_cert_and_key();
    let mut config = tokio_rustls::rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .expect("invalid cert/key");
    config.alpn_protocols = vec![tonic_tls::ALPN_H2.to_vec()];
    Arc::new(config)
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let handler = Arc::new(DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new()));
    let agent_card = build_agent_card(vec![
        AgentInterface::new("https://localhost:3443/jsonrpc", TRANSPORT_PROTOCOL_JSONRPC),
        AgentInterface::new("https://localhost:3443/rest", TRANSPORT_PROTOCOL_HTTP_JSON),
        AgentInterface::new("https://localhost:50052", TRANSPORT_PROTOCOL_GRPC),
    ]);
    let card_producer = Arc::new(StaticAgentCard::new(agent_card));

    let app = axum::Router::new()
        .nest("/jsonrpc", a2a_server::jsonrpc::jsonrpc_router(handler.clone()))
        .nest("/rest", a2a_server::rest::rest_router(handler.clone()))
        .merge(a2a_server::agent_card::agent_card_router(card_producer));

    let grpc_service = A2aServiceServer::new(GrpcHandler::new(handler));

    let rustls_config = load_rustls_config();
    let https_config = axum_server::tls_rustls::RustlsConfig::from_config(rustls_config);
    let https_addr: std::net::SocketAddr = "0.0.0.0:3443".parse().unwrap();

    let grpc_tls_config = load_grpc_tls_config();
    let grpc_addr: std::net::SocketAddr = "0.0.0.0:50052".parse().unwrap();
    let tcp_incoming = TcpIncoming::bind(grpc_addr).expect("bind gRPC address");
    let tls_incoming = TlsIncoming::new(tcp_incoming, grpc_tls_config);

    tracing::info!("Hello World Agent starting (TLS)");
    tracing::info!("Agent card:  https://localhost:3443/.well-known/agent-card.json");
    tracing::info!("JSON-RPC:    https://localhost:3443/jsonrpc");
    tracing::info!("REST:        https://localhost:3443/rest");
    tracing::info!("gRPC:        https://localhost:50052");

    tokio::select! {
        result = axum_server::bind_rustls(https_addr, https_config)
            .serve(app.into_make_service()) => {
            if let Err(e) = result { tracing::error!(error = %e, "HTTPS server exited"); }
        }
        result = TonicServer::builder()
            .add_service(grpc_service)
            .serve_with_incoming(tls_incoming) => {
            if let Err(e) = result { tracing::error!(error = %e, "gRPC TLS server exited"); }
        }
    }
}
