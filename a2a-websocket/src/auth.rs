// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
//! Authentication and authorization for the WebSocket binding
//! (specification Section 9).
//!
//! Credentials are presented once during the HTTP Upgrade handshake and the
//! resulting [`AuthContext`] applies to every request on the connection
//! (Section 9.1–9.2). Because a WebSocket connection outlives a single request,
//! the binding also defines mechanisms to keep authorization fresh
//! (Section 9.3):
//!
//! * **Per-message / interval revalidation** ([`WsAuthenticator::revalidate`],
//!   Section 9.3.1) — the server re-checks the connection's credentials and, if
//!   they have expired or been revoked, closes with code `4001`.
//! * **Server-initiated reconnection** ([`AuthStatus::ReauthRequired`],
//!   Section 9.3.2) — the server emits a `ReauthenticationRequired` control
//!   frame and closes after a short grace period.
//! * **In-band token refresh** ([`WsAuthenticator::refresh`], Section 9.3.3) —
//!   the optional `Authenticate` method lets a client replace its credentials
//!   without reconnecting.
//!
//! Credential validation is inherently deployment-specific, so this crate does
//! not ship a concrete verifier. Applications implement [`WsAuthenticator`] and
//! register it with
//! [`websocket_router_with_auth`](crate::server::websocket_router_with_auth).

use std::time::Duration;

use async_trait::async_trait;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};

pub use a2a_server::{ServiceParams, User};

/// Recommended grace period the server waits, after signalling
/// `ReauthenticationRequired`, before closing the connection (spec Section
/// 9.3.2).
pub const DEFAULT_REAUTH_GRACE: Duration = Duration::from_secs(5);

/// Failure to authenticate a WebSocket upgrade handshake (spec Section 9.1).
///
/// The `status` maps to the HTTP response the server returns instead of the
/// `101 Switching Protocols`: `401 Unauthorized` when credentials are missing
/// or invalid, `403 Forbidden` when the authenticated principal is not allowed.
#[derive(Debug, Clone)]
pub struct AuthError {
    pub status: u16,
    pub message: String,
}

impl AuthError {
    /// Credentials were missing or could not be validated (HTTP 401).
    pub fn unauthorized(message: impl Into<String>) -> Self {
        AuthError {
            status: 401,
            message: message.into(),
        }
    }

    /// The principal is authenticated but not authorized (HTTP 403).
    pub fn forbidden(message: impl Into<String>) -> Self {
        AuthError {
            status: 403,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.status)
    }
}

impl std::error::Error for AuthError {}

/// Authenticated identity and connection-scoped parameters established at the
/// handshake and carried for the lifetime of the connection (spec Section 9.2).
///
/// # Getting the identity to your agent
///
/// The two fields serve different consumers, and the distinction matters:
///
/// * [`user`](Self::user) is used **by the binding**. It scopes the
///   per-identity rate-limit budget (Section 13.3) so that one principal cannot
///   multiply its allowance by opening more connections. It is not handed to
///   the [`RequestHandler`](a2a_server::RequestHandler).
/// * [`service_params`](Self::service_params) is the seam **to your agent**.
///   Every entry is merged into the [`ServiceParams`] of each request on the
///   connection, which is what `RequestHandler` and, through it, your executor
///   receive.
///
/// So publish anything the agent needs to act on — the caller's identity,
/// tenant, scopes — into `service_params`:
///
/// ```ignore
/// AuthContext::for_user(User::authenticated(&subject))
///     .with_param("x-agent-user", &subject)
///     .with_param("x-tenant", &tenant)
/// ```
///
/// These keys are lowercased and become authoritative for the connection: a
/// per-request `serviceParams` entry of the same name is ignored, so a client
/// cannot present itself to the handler as a different principal than the one
/// it authenticated as. Keys the authenticator does *not* set remain
/// client-settable per request.
#[derive(Debug, Clone, Default)]
pub struct AuthContext {
    /// The authenticated principal, if any. Scopes the per-identity rate limit;
    /// see the type-level docs for how to reach the request handler.
    pub user: Option<User>,
    /// Parameters merged into every request handled on this connection (for
    /// example a resolved tenant or scopes). Authoritative: a per-request
    /// `serviceParams` entry cannot override one of these keys.
    pub service_params: ServiceParams,
}

impl AuthContext {
    /// Build a context for an authenticated user with no extra parameters.
    pub fn for_user(user: User) -> Self {
        AuthContext {
            user: Some(user),
            service_params: ServiceParams::new(),
        }
    }

    /// Publish a connection-scoped service parameter, visible to the request
    /// handler on every request and not overridable by the client.
    pub fn with_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.service_params
            .insert(key.into().to_ascii_lowercase(), vec![value.into()]);
        self
    }
}

/// Outcome of revalidating an already-established connection (spec Section
/// 9.2 / 9.3.1).
#[derive(Debug, Clone)]
pub enum AuthStatus {
    /// Credentials remain valid; continue processing.
    Valid,
    /// Credentials are approaching expiry or have been rotated: the server
    /// signals `ReauthenticationRequired` and closes after a grace period
    /// (Section 9.3.2).
    ReauthRequired { reason: String, retry_after_ms: u64 },
    /// Credentials are expired or revoked: the server MUST close with `4001`
    /// (Section 9.2 / 9.3.1).
    Expired { reason: String },
}

/// Parameters of the binding-specific in-band `Authenticate` refresh method
/// (spec Section 9.3.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthenticateParams {
    pub scheme: String,
    pub credentials: String,
}

/// Pluggable authenticator for the WebSocket binding.
///
/// Implementations validate handshake credentials and, optionally, keep the
/// connection's authorization fresh over its lifetime. All hooks other than
/// [`authenticate`](Self::authenticate) have sensible defaults, so a minimal
/// authenticator only needs to validate the handshake.
#[async_trait]
pub trait WsAuthenticator: Send + Sync + 'static {
    /// Validate the credentials presented in the upgrade handshake headers
    /// (typically `Authorization`, or a scheme-specific API-key header) and
    /// return the connection's [`AuthContext`]. Returning [`AuthError`] rejects
    /// the upgrade with the corresponding HTTP status (Section 9.1).
    ///
    /// Whatever the agent needs to know about the caller belongs in the
    /// returned context's
    /// [`service_params`](AuthContext::service_params) — see the
    /// [`AuthContext`] docs.
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError>;

    /// Re-check the connection's credentials (Section 9.3.1). Invoked before
    /// each incoming request (and therefore also usable as an interval check).
    /// The default implementation treats credentials as permanently valid.
    async fn revalidate(&self, _ctx: &AuthContext) -> AuthStatus {
        AuthStatus::Valid
    }

    /// Whether the optional in-band `Authenticate` refresh method is supported
    /// (Section 9.3.3). Support MUST also be advertised in the agent's
    /// binding-specific capabilities.
    fn supports_in_band_refresh(&self) -> bool {
        false
    }

    /// Handle an in-band `Authenticate` request, replacing the connection's
    /// credentials without reconnecting (Section 9.3.3). Only called when
    /// [`supports_in_band_refresh`](Self::supports_in_band_refresh) returns
    /// `true`.
    async fn refresh(
        &self,
        _ctx: &AuthContext,
        _scheme: &str,
        _credentials: &str,
    ) -> Result<AuthContext, AuthError> {
        Err(AuthError::forbidden(
            "in-band token refresh is not supported",
        ))
    }

    /// Grace period to wait after signalling `ReauthenticationRequired` before
    /// closing the connection (Section 9.3.2).
    fn reauth_grace(&self) -> Duration {
        DEFAULT_REAUTH_GRACE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_error_status_codes() {
        assert_eq!(AuthError::unauthorized("no creds").status, 401);
        assert_eq!(AuthError::forbidden("nope").status, 403);
    }

    #[test]
    fn auth_context_for_user_sets_user() {
        let ctx = AuthContext::for_user(User::authenticated("alice"));
        assert_eq!(ctx.user.as_ref().unwrap().name, "alice");
        assert!(ctx.service_params.is_empty());
    }

    #[test]
    fn auth_context_with_param_lowercases_the_key() {
        let ctx =
            AuthContext::for_user(User::authenticated("alice")).with_param("X-Tenant", "acme");
        assert_eq!(
            ctx.service_params.get("x-tenant").unwrap(),
            &vec!["acme".to_string()]
        );
    }

    #[test]
    fn authenticate_params_round_trip() {
        let params = AuthenticateParams {
            scheme: "Bearer".into(),
            credentials: "tok".into(),
        };
        let json = serde_json::to_string(&params).unwrap();
        let back: AuthenticateParams = serde_json::from_str(&json).unwrap();
        assert_eq!(back.scheme, "Bearer");
        assert_eq!(back.credentials, "tok");
    }

    struct DummyAuth;

    #[async_trait]
    impl WsAuthenticator for DummyAuth {
        async fn authenticate(&self, _headers: &HeaderMap) -> Result<AuthContext, AuthError> {
            Ok(AuthContext::default())
        }
    }

    #[tokio::test]
    async fn default_hooks_have_expected_behavior() {
        let auth = DummyAuth;
        assert!(matches!(
            auth.revalidate(&AuthContext::default()).await,
            AuthStatus::Valid
        ));
        assert!(!auth.supports_in_band_refresh());
        assert_eq!(auth.reauth_grace(), DEFAULT_REAUTH_GRACE);
        let err = auth
            .refresh(&AuthContext::default(), "Bearer", "x")
            .await
            .unwrap_err();
        assert_eq!(err.status, 403);
    }
}
