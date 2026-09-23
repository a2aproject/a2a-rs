// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
pub mod client;
pub mod errors;
pub mod server;

pub use client::{GrpcTransport, GrpcTransportFactory};
pub use server::GrpcHandler;

#[cfg(any(feature = "rustls-tls", feature = "rustls-no-provider"))]
pub use rustls;
