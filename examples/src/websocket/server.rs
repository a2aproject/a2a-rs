// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Hello World A2A server over the WebSocket custom protocol binding.
//!
//! This example shows the application's side of the binding — the parts the
//! `a2a-websocket` crate deliberately does not decide for you:
//!
//! * validating credentials and deciding who the caller is ([`DemoAuth`]),
//! * getting that identity to your agent's business logic
//!   ([`IdentityAwareEcho`]),
//! * choosing the connection limits appropriate for your deployment.
//!
//! Run:
//!   cargo run --bin websocket-server --package examples

use std::collections::HashMap;
use std::future::IntoFuture;
use std::sync::Arc;
use std::time::Duration;

use a2a::event::StreamResponse;
use a2a::*;
use a2a_server::*;
use a2a_websocket::auth::{AuthContext, AuthError, AuthStatus, User, WsAuthenticator};
use a2a_websocket::server::{WebSocketConfig, websocket_router_with_config};
use a2a_websocket::{
    ConnectionLimitPolicy, ConnectionLimits, Liveness, LivenessPolicy, RateLimit, RateLimitPolicy,
    TRANSPORT_PROTOCOL_WEBSOCKET,
};
use async_trait::async_trait;
use axum::http::HeaderMap;
use examples_lib::{EchoExecutor, build_agent_card};
use futures::stream::BoxStream;

const HTTP_ADDR: &str = "0.0.0.0:3000";
const WS_URL: &str = "ws://localhost:3000/a2a/ws";

/// Service parameter carrying the caller established by the handshake. The name
/// is this application's choice; the binding only guarantees that whatever the
/// authenticator publishes reaches the handler and cannot be overridden by the
/// client.
const PARAM_USER: &str = "x-agent-user";
const PARAM_TENANT: &str = "x-tenant";

// ---------------------------------------------------------------------------
// Authentication — the application's responsibility (spec Section 9)
// ---------------------------------------------------------------------------

/// How long `expiring-token` stays valid after the handshake.
const EXPIRING_TOKEN_LIFETIME: Duration = Duration::from_secs(5);

/// A stand-in for a real credential check.
///
/// A production authenticator would verify a JWT signature, call an
/// introspection endpoint, or check an API key against a store. What matters
/// for the binding is the shape: map credentials to an [`AuthContext`], and
/// publish everything the agent needs into its service parameters.
struct DemoAuth {
    /// token -> (subject, tenant)
    tokens: HashMap<String, (String, String)>,
    /// Accepted, but expires shortly after use. This drives the
    /// `ws/ReauthenticationRequired` flow (Section 9.5) in the example.
    expiring: String,
}

impl DemoAuth {
    fn new() -> Self {
        let mut tokens = HashMap::new();
        tokens.insert(
            "alice-token".to_string(),
            ("alice".to_string(), "acme".to_string()),
        );
        tokens.insert(
            "bob-token".to_string(),
            ("bob".to_string(), "globex".to_string()),
        );
        tokens.insert(
            "expiring-token".to_string(),
            ("carol".to_string(), "initech".to_string()),
        );
        tokens.insert(
            "carol-renewed-token".to_string(),
            ("carol".to_string(), "initech".to_string()),
        );
        DemoAuth {
            tokens,
            expiring: "expiring-token".to_string(),
        }
    }

    fn context_for(&self, token: &str) -> Option<AuthContext> {
        let (subject, tenant) = self.tokens.get(token)?;
        // `user` scopes the per-identity rate limit and is the principal an
        // in-band refresh must keep; the service params are what the request
        // handler — and therefore the executor — actually sees.
        let ctx = AuthContext::for_user(User::authenticated(subject))
            .with_param(PARAM_USER, subject)
            .with_param(PARAM_TENANT, tenant);
        if token == self.expiring {
            let lifetime = chrono::Duration::from_std(EXPIRING_TOKEN_LIFETIME).ok()?;
            Some(ctx.expiring_at(chrono::Utc::now() + lifetime))
        } else {
            Some(ctx)
        }
    }

    fn bearer_token(headers: &HeaderMap) -> Option<&str> {
        headers
            .get("authorization")?
            .to_str()
            .ok()?
            .strip_prefix("Bearer ")
    }
}

#[async_trait]
impl WsAuthenticator for DemoAuth {
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError> {
        let Some(token) = Self::bearer_token(headers) else {
            return Err(AuthError::unauthorized("missing Bearer credentials"));
        };
        self.context_for(token)
            .ok_or_else(|| AuthError::forbidden("unknown token"))
    }

    /// Called before every request, so a revoked or expiring credential stops
    /// being honoured without waiting for the client to reconnect.
    async fn revalidate(&self, ctx: &AuthContext) -> AuthStatus {
        match ctx.expires_at {
            Some(expires_at) if expires_at <= chrono::Utc::now() => AuthStatus::Expired {
                reason: "credentials expired".to_string(),
            },
            Some(expires_at) => AuthStatus::ExpiresAt { expires_at },
            None => AuthStatus::Valid,
        }
    }

    /// Names of the Agent Card `securitySchemes` this authenticator accepts,
    /// reported to clients in authentication errors.
    fn accepted_schemes(&self) -> Vec<String> {
        vec!["bearer".to_string()]
    }

    /// Accepting `ws/Authenticate` lets a client replace its credentials
    /// without dropping the connection.
    fn supports_in_band_refresh(&self) -> bool {
        true
    }

    async fn refresh(
        &self,
        _ctx: &AuthContext,
        credentials: &HashMap<String, String>,
    ) -> Result<AuthContext, AuthError> {
        let Some(token) = credentials.get("bearer") else {
            return Err(AuthError::invalid_params(
                "only the bearer scheme is accepted",
            ));
        };
        // The refreshed credential is validated exactly like a handshake one.
        // The binding rejects it if it names a different principal.
        self.context_for(token)
            .ok_or_else(|| AuthError::unauthorized("unknown token"))
    }
}

// ---------------------------------------------------------------------------
// Business logic — reads the identity the authenticator established
// ---------------------------------------------------------------------------

/// Wraps the shared echo executor to show the authenticated caller reaching the
/// agent. `ctx.service_params` is populated from the connection's
/// `AuthContext`, so the executor can authorize per tenant without knowing
/// anything about WebSockets.
struct IdentityAwareEcho;

impl IdentityAwareEcho {
    fn caller(ctx: &ExecutorContext) -> (&str, &str) {
        let read = |key: &str| {
            ctx.service_params
                .get(key)
                .and_then(|values| values.first())
                .map(String::as_str)
                .unwrap_or("anonymous")
        };
        (read(PARAM_USER), read(PARAM_TENANT))
    }
}

impl AgentExecutor for IdentityAwareEcho {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (user, tenant) = Self::caller(&ctx);
        tracing::info!(user, tenant, task = %ctx.task_id, "executing for authenticated caller");
        EchoExecutor.execute(ctx)
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (user, tenant) = Self::caller(&ctx);
        tracing::info!(user, tenant, task = %ctx.task_id, "cancelling for authenticated caller");
        EchoExecutor.cancel(ctx)
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let handler = Arc::new(DefaultRequestHandler::new(
        IdentityAwareEcho,
        InMemoryTaskStore::new(),
    ));

    let agent_card = build_agent_card(vec![AgentInterface::new(
        WS_URL,
        TRANSPORT_PROTOCOL_WEBSOCKET,
    )]);
    let card_producer = Arc::new(StaticAgentCard::new(agent_card));

    let config = WebSocketConfig {
        authenticator: Some(Arc::new(DemoAuth::new())),
        // Browser clients can be made to open a WebSocket to any origin, so an
        // agent reachable from a browser should pin the origins it trusts.
        allowed_origins: None,
        // `None` applies the spec's recommended 1 MiB cap.
        max_frame_bytes: None,
        // Rate limiting is on by default; this only tightens it. The budget is
        // shared across every connection of the same identity, so opening more
        // sockets does not buy more throughput.
        rate_limit: RateLimitPolicy::Custom(RateLimit::new(50, Duration::from_secs(1))),
        // Keep-alive is on by default with the spec's recommended intervals
        // (ping every 30s, pong within 10s, close after 5 minutes idle). Shortened
        // here so the behaviour is visible while playing with the example.
        liveness: LivenessPolicy::Custom(Liveness {
            ping_interval: Duration::from_secs(10),
            pong_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(60),
        }),
        // Also on by default, with far more generous values. Tightened here so
        // that running the client twice at once shows the cap taking effect.
        connection_limits: ConnectionLimitPolicy::Custom(ConnectionLimits {
            max_connections_per_identity: 4,
            max_streams_per_connection: 8,
        }),
    };

    let app = axum::Router::new()
        .nest("/a2a/ws", websocket_router_with_config(handler, config))
        .merge(a2a_server::agent_card::agent_card_router(card_producer));

    tracing::info!("Hello World Agent (WebSocket binding) starting");
    tracing::info!("Agent card:  http://localhost:3000/.well-known/agent-card.json");
    tracing::info!("WebSocket:   {WS_URL}");
    tracing::info!("Tokens:      alice-token, bob-token, expiring-token, carol-renewed-token");

    let listener = tokio::net::TcpListener::bind(HTTP_ADDR).await.unwrap();
    if let Err(e) = axum::serve(listener, app).into_future().await {
        tracing::error!(error = %e, "server exited");
    }
}
