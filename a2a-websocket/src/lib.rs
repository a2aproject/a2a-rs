// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
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
    DEFAULT_MAX_FRAME_BYTES, JSONRPC_VERSION, JsonRpcError, JsonRpcId,
    ReauthenticationRequiredParams, SERVER_ERROR_CODE, SUBPROTOCOL, StreamControlParams,
    TRANSPORT_PROTOCOL_WEBSOCKET, WsRequestEnvelope, WsResponseEnvelope, binding_errors,
    close_codes, methods,
};
pub use errors::{binding_error, binding_error_reason};
pub use limits::{
    ConnectionLimitPolicy, ConnectionLimits, DEFAULT_CONNECTION_LIMITS, IdentityConnectionCounter,
};
pub use liveness::{DEFAULT_LIVENESS, Liveness, LivenessPolicy};
pub use ratelimit::{DEFAULT_RATE_LIMIT, IdentityRateLimiter, RateLimit, RateLimitPolicy};
pub use reconnect::{Backoff, DEFAULT_BACKOFF};
pub use server::{
    WebSocketConfig, websocket_router, websocket_router_with_auth, websocket_router_with_config,
};
