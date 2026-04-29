// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Hello World A2A client with TLS (rustls).
//!
//! Run the TLS server first:
//!   cargo run --bin helloworld-tls-server --package examples
//! Then run this client:
//!   cargo run --bin helloworld-tls-client --package examples

use std::sync::Arc;

use a2a::*;
use a2a_client::A2AClientFactory;
use a2a_client::agent_card::AgentCardResolver;
use a2a_client::jsonrpc::JsonRpcTransportFactory;
use a2a_client::rest::RestTransportFactory;
use a2a_grpc::GrpcTransportFactory;
use a2a_grpc::rustls;
use examples_lib::exercise_client;

const SERVER_URLS: &[&str] = &["https://localhost:3443"];
const CA_PEM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ca.pem"));

fn build_grpc_tls_config() -> Arc<rustls::ClientConfig> {
    let ca_cert = rustls_pemfile::certs(&mut &CA_PEM[..])
        .next()
        .expect("no cert in ca.pem")
        .expect("parse CA cert");

    let mut root_store = rustls::RootCertStore::empty();
    root_store.add(ca_cert).expect("add CA cert");

    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec()];
    Arc::new(config)
}

fn build_tls_reqwest_client() -> reqwest::Client {
    let cert = reqwest::Certificate::from_pem(CA_PEM).expect("parse CA cert for reqwest");
    reqwest::Client::builder()
        .add_root_certificate(cert)
        .build()
        .expect("build reqwest client")
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let grpc_tls_config = build_grpc_tls_config();
    let tls_reqwest = build_tls_reqwest_client();
    let resolver = AgentCardResolver::new(Some(tls_reqwest));

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
                .register(Arc::new(GrpcTransportFactory::with_rustls_config(
                    grpc_tls_config.clone(),
                )))
                .register(Arc::new(
                    RestTransportFactory::with_root_certificates_pem(CA_PEM).unwrap(),
                ))
                .register(Arc::new(
                    JsonRpcTransportFactory::with_root_certificates_pem(CA_PEM).unwrap(),
                ))
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
