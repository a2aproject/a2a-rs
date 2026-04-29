// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
pub mod agent_card;
pub mod auth;
pub mod client;
pub mod factory;
pub mod jsonrpc;
pub mod middleware;
mod push_config_compat;
pub mod rest;
pub mod transport;

pub use client::A2AClient;
pub use factory::A2AClientFactory;
pub use futures::stream::BoxStream;
pub use transport::{ServiceParams, Transport, TransportFactory};

pub(crate) fn build_reqwest_client_with_root_pem(
    pem: &[u8],
) -> Result<reqwest::Client, a2a::A2AError> {
    let cert = reqwest::Certificate::from_pem(pem)
        .map_err(|e| a2a::A2AError::internal(format!("invalid PEM certificate: {e}")))?;
    reqwest::Client::builder()
        .add_root_certificate(cert)
        .build()
        .map_err(|e| a2a::A2AError::internal(format!("failed to build HTTP client: {e}")))
}
