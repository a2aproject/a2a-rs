// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Hello World A2A client over the WebSocket custom protocol binding.
//!
//! Demonstrates the two client-side responsibilities the binding cannot take
//! over: supplying handshake credentials, and reacting when the server asks for
//! reauthentication (spec Section 9.3.2).
//!
//! Run the server first:
//!   cargo run --bin websocket-server --package examples
//! Then run this client:
//!   cargo run --bin websocket-client --package examples

use std::sync::Arc;

use a2a::A2AError;
use a2a_client::A2AClient;
use a2a_websocket::auth::AuthenticateParams;
use a2a_websocket::{ConnectOptions, CredentialProvider, WebSocketTransport};
use async_trait::async_trait;
use examples_lib::exercise_client;

const WS_URL: &str = "ws://localhost:3000/a2a/ws";

/// Supplies fresh credentials when the server signals that the current ones are
/// about to expire.
///
/// A real provider would mint or fetch a token here — refresh an OAuth grant,
/// read a rotated file, call a secrets manager. It is called on the connection's
/// read loop, so it should be quick and must not block.
struct DemoCredentials {
    next_token: String,
}

#[async_trait]
impl CredentialProvider for DemoCredentials {
    async fn fresh_credentials(&self) -> Result<AuthenticateParams, A2AError> {
        tracing::info!("minting fresh credentials for in-band refresh");
        Ok(AuthenticateParams {
            scheme: "Bearer".to_string(),
            credentials: self.next_token.clone(),
        })
    }
}

/// Connect as a caller whose credentials do not expire.
///
/// Note that credentials travel on the upgrade handshake, so this connects
/// directly rather than going through `A2AClientFactory` — the factory has no
/// way to carry per-deployment secrets.
async fn connect_as_alice() -> Result<WebSocketTransport, A2AError> {
    WebSocketTransport::connect_with_options(
        WS_URL,
        ConnectOptions::with_bearer_token("alice-token"),
    )
    .await
}

/// Connect as a caller whose credentials expire, and hand the transport a way
/// to replace them. The server's `ReauthenticationRequired` frame is then
/// answered in-band and the connection survives (Section 9.3.3).
async fn connect_with_refresh() -> Result<WebSocketTransport, A2AError> {
    let options = ConnectOptions::with_bearer_token("expiring-token").with_credential_provider(
        Arc::new(DemoCredentials {
            next_token: "bob-token".to_string(),
        }),
    );
    WebSocketTransport::connect_with_options(WS_URL, options).await
}

/// The alternative to refreshing in-band: be told, and reconnect yourself.
/// Without either policy the server closes the connection with `4001` once its
/// grace period expires.
async fn connect_with_notify() -> Result<WebSocketTransport, A2AError> {
    let options = ConnectOptions::with_bearer_token("expiring-token").on_reauth_required(|req| {
        tracing::warn!(
            reason = ?req.reason,
            retry_after_ms = ?req.retry_after_ms,
            "server asked for reauthentication; reconnect with fresh credentials"
        );
    });
    WebSocketTransport::connect_with_options(WS_URL, options).await
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    match connect_as_alice().await {
        Ok(transport) => {
            exercise_client("WEBSOCKET (static credentials)", &A2AClient::new(transport)).await
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to connect; is websocket-server running?");
            return;
        }
    }

    match connect_with_refresh().await {
        Ok(transport) => {
            exercise_client(
                "WEBSOCKET (in-band credential refresh)",
                &A2AClient::new(transport),
            )
            .await
        }
        Err(e) => tracing::error!(error = %e, "refresh-capable connection failed"),
    }

    // Kept short: this connection is expected to be closed with 4001 once the
    // server's grace period elapses, since nothing replaces the credentials.
    match connect_with_notify().await {
        Ok(transport) => {
            exercise_client(
                "WEBSOCKET (notify-only, expect a 4001 close)",
                &A2AClient::new(transport),
            )
            .await
        }
        Err(e) => tracing::error!(error = %e, "notify-only connection failed"),
    }
}
