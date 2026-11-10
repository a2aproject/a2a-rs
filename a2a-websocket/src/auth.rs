// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Authentication and authorization for the WebSocket binding
//! (specification Section 9).
//!
//! Credentials are presented once per connection, during the HTTP Upgrade
//! handshake, and the resulting [`AuthContext`] applies to every request on the
//! connection (Sections 9.2 and 9.4). Because a WebSocket connection outlives a
//! single request, the binding also keeps authentication fresh:
//!
//! * **Per-request revalidation** ([`WsAuthenticator::revalidate`], Section
//!   9.4) — before each request the server re-checks the credentials. Expired
//!   or revoked credentials fail the request with `WS_CREDENTIALS_EXPIRED` and
//!   close the connection with `4001`.
//! * **Expiry warning** ([`AuthStatus::ExpiresAt`], Section 9.5) — the server
//!   sends a `ws/ReauthenticationRequired` notification and closes with `4001`
//!   once `expiresAt` passes, unless the client refreshed first.
//! * **In-band refresh** ([`WsAuthenticator::refresh`], Section 9.3) — the
//!   optional `ws/Authenticate` method lets a client replace its credentials
//!   without reconnecting.
//!
//! Credential validation is inherently deployment-specific, so this crate does
//! not ship a concrete verifier. Applications implement [`WsAuthenticator`] and
//! register it with
//! [`websocket_router_with_auth`](crate::server::websocket_router_with_auth).

use std::collections::HashMap;

use async_trait::async_trait;
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use a2a_server::{ServiceParams, User};

/// Failure to authenticate a connection (spec Sections 9.2 and 9.3).
///
/// During the upgrade, `status` is the HTTP response the server returns
/// instead of `101 Switching Protocols`. For `ws/Authenticate` it selects the
/// JSON-RPC error: `401` becomes `WS_UNAUTHENTICATED`, `403` becomes
/// `WS_PERMISSION_DENIED`, and `400` becomes `InvalidParamsError`.
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

    /// `ws/Authenticate` named a scheme that is not in the Agent Card
    /// `securitySchemes`, or one that cannot be presented in-band such as
    /// `MutualTlsSecurityScheme` (Section 9.3).
    pub fn invalid_params(message: impl Into<String>) -> Self {
        AuthError {
            status: 400,
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
/// handshake and carried for the lifetime of the connection (spec Section 9.4).
///
/// # Getting the identity to your agent
///
/// The two fields serve different consumers, and the distinction matters:
///
/// * [`user`](Self::user) is used **by the binding**. It scopes the
///   per-identity rate-limit budget so that one principal cannot multiply its
///   allowance by opening more connections, and it is the principal an
///   in-band refresh must keep (Section 9.3). It is not handed to the
///   [`RequestHandler`](a2a_server::RequestHandler).
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
    /// When the server stops accepting these credentials, if known. Returned
    /// to the client as `expiresAt` in the `ws/Authenticate` result
    /// (Section 9.3).
    pub expires_at: Option<DateTime<Utc>>,
}

impl AuthContext {
    /// Build a context for an authenticated user with no extra parameters.
    pub fn for_user(user: User) -> Self {
        AuthContext {
            user: Some(user),
            ..Default::default()
        }
    }

    /// Publish a connection-scoped service parameter, visible to the request
    /// handler on every request and not overridable by the client.
    pub fn with_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.service_params
            .insert(key.into().to_ascii_lowercase(), vec![value.into()]);
        self
    }

    /// Record when these credentials expire.
    pub fn expiring_at(mut self, expires_at: DateTime<Utc>) -> Self {
        self.expires_at = Some(expires_at);
        self
    }
}

/// Outcome of revalidating an already-established connection (spec Sections
/// 9.4 and 9.5).
#[derive(Debug, Clone)]
pub enum AuthStatus {
    /// Credentials remain valid; continue processing.
    Valid,
    /// Credentials are valid until `expires_at`. The server sends a
    /// `ws/ReauthenticationRequired` notification once and, unless the client
    /// refreshes in-band, closes the connection with `4001` at `expires_at`
    /// (Section 9.5).
    ExpiresAt { expires_at: DateTime<Utc> },
    /// Credentials are expired or revoked: the request fails with
    /// `WS_CREDENTIALS_EXPIRED` and the connection is closed with `4001`
    /// (Section 9.4).
    Expired { reason: String },
}

/// Parameters of the optional `ws/Authenticate` method (spec Section 9.3): a
/// map from the name of a scheme in the Agent Card `securitySchemes` to the
/// credential for that scheme.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AuthenticateParams {
    pub credentials: HashMap<String, String>,
}

impl AuthenticateParams {
    /// Credentials for a single scheme.
    pub fn single(scheme: impl Into<String>, credential: impl Into<String>) -> Self {
        AuthenticateParams {
            credentials: HashMap::from([(scheme.into(), credential.into())]),
        }
    }
}

/// Pluggable authenticator for the WebSocket binding.
///
/// Implementations validate handshake credentials and, optionally, keep the
/// connection's authentication fresh over its lifetime. All hooks other than
/// [`authenticate`](Self::authenticate) have sensible defaults, so a minimal
/// authenticator only needs to validate the handshake.
#[async_trait]
pub trait WsAuthenticator: Send + Sync + 'static {
    /// Validate the credentials presented in the upgrade handshake headers
    /// (typically `Authorization`, or a scheme-specific API-key header) and
    /// return the connection's [`AuthContext`]. Returning [`AuthError`] rejects
    /// the upgrade with the corresponding HTTP status (Section 9.2).
    ///
    /// Whatever the agent needs to know about the caller belongs in the
    /// returned context's
    /// [`service_params`](AuthContext::service_params) — see the
    /// [`AuthContext`] docs.
    async fn authenticate(&self, headers: &HeaderMap) -> Result<AuthContext, AuthError>;

    /// Re-check the connection's credentials (Section 9.4). Invoked before
    /// each incoming request. The default implementation treats credentials
    /// as permanently valid.
    async fn revalidate(&self, _ctx: &AuthContext) -> AuthStatus {
        AuthStatus::Valid
    }

    /// Names of the Agent Card `securitySchemes` the server accepts, listed in
    /// the `schemes` metadata of `WS_UNAUTHENTICATED` and
    /// `WS_CREDENTIALS_EXPIRED` errors (Section 9.3).
    fn accepted_schemes(&self) -> Vec<String> {
        Vec::new()
    }

    /// Whether the optional `ws/Authenticate` method is implemented (Section
    /// 9.3). When it is not, the server answers the method with
    /// `MethodNotFoundError`.
    fn supports_in_band_refresh(&self) -> bool {
        false
    }

    /// Validate the credentials of a `ws/Authenticate` request and return the
    /// replacement [`AuthContext`] (Section 9.3). Only called when
    /// [`supports_in_band_refresh`](Self::supports_in_band_refresh) returns
    /// `true`. The server rejects a context whose
    /// [`user`](AuthContext::user) differs from the current one with
    /// `WS_PERMISSION_DENIED`.
    async fn refresh(
        &self,
        _ctx: &AuthContext,
        _credentials: &HashMap<String, String>,
    ) -> Result<AuthContext, AuthError> {
        Err(AuthError::unauthorized(
            "in-band authentication is not supported",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_error_status_codes() {
        assert_eq!(AuthError::unauthorized("no creds").status, 401);
        assert_eq!(AuthError::forbidden("nope").status, 403);
        assert_eq!(AuthError::invalid_params("unknown scheme").status, 400);
    }

    #[test]
    fn auth_context_for_user_sets_user() {
        let ctx = AuthContext::for_user(User::authenticated("alice"));
        assert_eq!(ctx.user.as_ref().unwrap().name, "alice");
        assert!(ctx.service_params.is_empty());
        assert!(ctx.expires_at.is_none());
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
    fn authenticate_params_use_the_credentials_map() {
        let params = AuthenticateParams::single("oauth", "tok");
        let json = serde_json::to_value(&params).unwrap();
        assert_eq!(json, serde_json::json!({"credentials": {"oauth": "tok"}}));
        let back: AuthenticateParams = serde_json::from_value(json).unwrap();
        assert_eq!(back, params);
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
        assert!(auth.accepted_schemes().is_empty());
        let err = auth
            .refresh(&AuthContext::default(), &HashMap::new())
            .await
            .unwrap_err();
        assert_eq!(err.status, 401);
    }
}
