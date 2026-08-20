// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
#![doc = include_str!("../README.md")]

pub mod auth;
pub mod client;
pub mod common;
pub mod errors;
pub mod limits;
pub mod liveness;
pub mod ratelimit;
pub mod reconnect;
pub mod server;

pub use auth::{AuthContext, AuthError, AuthStatus, AuthenticateParams, WsAuthenticator};
pub use client::{
    ConnectOptions, CredentialProvider, ReauthPolicy, ReauthRequired, TlsOptions,
    WebSocketTransport, WebSocketTransportFactory,
};
pub use common::{
    CONTROL_REAUTH_REQUIRED, DEFAULT_MAX_FRAME_BYTES, JSONRPC_VERSION, JsonRpcError, JsonRpcId,
    SERVER_ERROR_CODE, SUBPROTOCOL, TRANSPORT_PROTOCOL_WEBSOCKET, WsRequestEnvelope,
    WsResponseEnvelope, close_codes, error_types, methods,
};
pub use limits::{
    ConnectionLimitPolicy, ConnectionLimits, DEFAULT_CONNECTION_LIMITS, IdentityConnectionCounter,
};
pub use liveness::{DEFAULT_LIVENESS, Liveness, LivenessPolicy};
pub use ratelimit::{DEFAULT_RATE_LIMIT, IdentityRateLimiter, RateLimit, RateLimitPolicy};
pub use reconnect::{Backoff, DEFAULT_BACKOFF};
pub use server::{
    WebSocketConfig, websocket_router, websocket_router_with_auth, websocket_router_with_config,
};
