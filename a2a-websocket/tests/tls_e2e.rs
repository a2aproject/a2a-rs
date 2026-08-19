// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
//! End-to-end tests for the `wss://` (TLS) transport path. These tests boot a
//! real TLS-terminating server (rustls + `ring`, self-signed via `rcgen`),
//! serve the standard `websocket_router(...)` over it through hyper, and drive
//! a real `WebSocketTransport` over `wss://`.

#![cfg(feature = "tls")]

use std::sync::Arc;

use a2a::*;
use a2a_client::transport::{ServiceParams, Transport};
use a2a_server::AgentExecutor;
use a2a_server::executor::ExecutorContext;
use a2a_server::handler::DefaultRequestHandler;
use a2a_server::task_store::InMemoryTaskStore;
use a2a_websocket::{ConnectOptions, TlsOptions, WebSocketTransport, server::websocket_router};
use futures::stream::{self, BoxStream};
use hyper::Request;
use hyper::body::Incoming;
use hyper_util::rt::{TokioExecutor, TokioIo};
use hyper_util::server::conn::auto::Builder as AutoBuilder;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_rustls::TlsAcceptor;
use tower::ServiceExt;

struct EchoExecutor;

impl AgentExecutor for EchoExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let task = Task {
            id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: ctx.message,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
    }

    fn cancel(
        &self,
        _ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::empty())
    }
}

/// Freshly generated self-signed certificate for `localhost`, plus the rustls
/// server config that presents it.
fn self_signed_localhost() -> (String, rustls::ServerConfig) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("generate self-signed cert");
    let cert_pem = certified.cert.pem();
    let cert_der: CertificateDer<'static> = certified.cert.der().clone();
    let key_der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
        certified.signing_key.serialize_der(),
    ));

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("safe protocol versions")
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("valid cert/key");

    (cert_pem, config)
}

/// Start a TLS WebSocket server on loopback. Returns the `wss://` URL, the
/// server's certificate in PEM form (for the client to trust), and a shutdown
/// handle.
async fn start_tls_server<E: AgentExecutor>(executor: E) -> (String, String, oneshot::Sender<()>) {
    let handler = Arc::new(DefaultRequestHandler::new(
        executor,
        InMemoryTaskStore::new(),
    ));
    let app = axum::Router::new().nest("/a2a/ws", websocket_router(handler));

    let (cert_pem, server_config) = self_signed_localhost();
    let acceptor = TlsAcceptor::from(Arc::new(server_config));

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();

    let (shutdown_tx, mut shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let (tcp, _) = match accepted {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    let acceptor = acceptor.clone();
                    let app = app.clone();
                    tokio::spawn(async move {
                        let tls = match acceptor.accept(tcp).await {
                            Ok(t) => t,
                            Err(_) => return,
                        };
                        let io = TokioIo::new(tls);
                        let service = hyper::service::service_fn(move |req: Request<Incoming>| {
                            app.clone().oneshot(req)
                        });
                        let _ = AutoBuilder::new(TokioExecutor::new())
                            .serve_connection_with_upgrades(io, service)
                            .await;
                    });
                }
            }
        }
    });

    // Connect to "localhost" so the SNI/hostname matches the certificate SAN.
    (
        format!("wss://localhost:{port}/a2a/ws"),
        cert_pem,
        shutdown_tx,
    )
}

fn send_message_request() -> SendMessageRequest {
    SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text("hello over tls")]),
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

#[tokio::test]
async fn wss_round_trips_when_certificate_is_trusted() {
    let (url, cert_pem, shutdown) = start_tls_server(EchoExecutor).await;

    let options = ConnectOptions::default().with_tls(TlsOptions::default().trust_pem(cert_pem));
    let transport = WebSocketTransport::connect_with_options(&url, options)
        .await
        .expect("wss connect with trusted cert should succeed");

    let response = transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();
    assert!(matches!(response, SendMessageResponse::Task(_)));

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn wss_rejects_untrusted_self_signed_certificate() {
    let (url, _cert_pem, shutdown) = start_tls_server(EchoExecutor).await;

    // No trust anchor for the self-signed cert -> the TLS handshake must fail.
    let result = WebSocketTransport::connect(&url).await;
    assert!(
        result.is_err(),
        "untrusted self-signed cert must be rejected"
    );
    let message = result.err().unwrap().message;
    assert!(
        message.contains("TLS handshake failed"),
        "unexpected error: {message}"
    );

    shutdown.send(()).unwrap();
}

#[tokio::test]
async fn wss_round_trips_with_danger_accept_invalid_certs() {
    let (url, _cert_pem, shutdown) = start_tls_server(EchoExecutor).await;

    let options =
        ConnectOptions::default().with_tls(TlsOptions::default().danger_accept_invalid_certs());
    let transport = WebSocketTransport::connect_with_options(&url, options)
        .await
        .expect("wss connect with verification disabled should succeed");

    let response = transport
        .send_message(&ServiceParams::new(), &send_message_request())
        .await
        .unwrap();
    assert!(matches!(response, SendMessageResponse::Task(_)));

    transport.destroy().await.unwrap();
    shutdown.send(()).unwrap();
}
