// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! gRPC binding for A2A v1, for clients and servers.
//!
//! Published as `a2a-grpc` and imported as `a2a_grpc`. It is built on tonic
//! and the generated types in [`a2a_pb`], whose `proto/a2a.proto` is the
//! service definition.
//!
//! - **Client:** [`GrpcTransport`] implements [`a2a_client::Transport`].
//!   Register [`GrpcTransportFactory`] with an
//!   [`A2AClientFactory`](a2a_client::A2AClientFactory) and it can pick gRPC
//!   for an agent card that advertises it. An endpoint is either
//!   `http://host:port` or a bare `host:port`.
//! - **Server:** [`GrpcHandler`] adapts any [`a2a_server::RequestHandler`] to
//!   the generated tonic service. Wrap it in
//!   [`A2aServiceServer`](a2a_pb::proto::a2a_service_server::A2aServiceServer)
//!   and add that to a tonic server.
//! - **Errors:** [`errors`] converts between [`a2a::A2AError`] and
//!   [`tonic::Status`].
//!
//! # Feature flags
//!
//! `rustls-tls` (default), `rustls-no-provider` and `native-tls` select the
//! TLS stack, as in `a2a-client` and `a2a-server`, and turn on the same flag
//! in both.

pub mod client;
pub mod errors;
pub mod server;

pub use client::{GrpcTransport, GrpcTransportFactory};
pub use server::GrpcHandler;

#[cfg(any(feature = "rustls-tls", feature = "rustls-no-provider"))]
pub use rustls;
