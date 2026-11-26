// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Async client for A2A v1 agents.
//!
//! Published as `a2a-client-lf` and imported as `a2a_client`. The protocol
//! types come from the [`a2a`] crate.
//!
//! # Where to start
//!
//! - [`A2AClientFactory`] turns an [`AgentCard`](a2a::AgentCard) into a
//!   client. It ranks the card's interfaces against the client's preferred
//!   bindings and returns an [`A2AClient`] for the first transport that
//!   connects. JSON-RPC and REST are registered unless
//!   [`no_defaults`](factory::A2AClientFactoryBuilder::no_defaults) is called.
//! - [`A2AClient`] sends messages, streams responses, and manages tasks and
//!   push notification configs over any [`Transport`].
//! - [`agent_card::AgentCardResolver`] fetches a card from a base URL's
//!   `/.well-known/agent-card.json`.
//!
//! # Extension points
//!
//! - [`Transport`] and [`TransportFactory`] are what a protocol binding
//!   implements. This crate provides [`jsonrpc`] and [`rest`]; the gRPC and
//!   SLIMRPC bindings are in the `a2a-grpc` and `a2a-slimrpc` crates.
//! - [`middleware::CallInterceptor`] runs around every call: `before` in
//!   order, `after` in reverse order. [`auth::AuthInterceptor`] uses it to add
//!   a bearer token or a custom header.
//!
//! # Feature flags
//!
//! - `rustls-tls` (default): TLS through rustls, with its default crypto
//!   provider.
//! - `rustls-no-provider`: TLS through rustls; the application installs the
//!   crypto provider.
//! - `native-tls`: TLS through the platform's native stack.
//!
//! `fuzzing` is for the fuzz targets in this repository, not for dependents.

// Framing internals reached by the fuzz targets in fuzz/ — see #238.
// Thin wrappers, not re-exports: the originals stay crate-private.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzz_support {
    use a2a::{A2AError, StreamResponse};

    pub fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
        crate::jsonrpc::find_event_boundary(buf)
    }

    pub fn parse_stream_tail(
        buf: &[u8],
        parse_event: &dyn Fn(&str) -> Option<Result<StreamResponse, A2AError>>,
    ) -> Option<Result<StreamResponse, A2AError>> {
        crate::jsonrpc::parse_stream_tail(buf, parse_event, None)
    }
}

pub mod agent_card;
pub mod auth;
pub mod client;
pub mod factory;
pub mod jsonrpc;
pub mod middleware;
mod push_config_compat;
pub mod rest;
pub mod transport;
mod wire;

pub use client::A2AClient;
pub use factory::A2AClientFactory;
pub use futures::stream::BoxStream;
pub use transport::{ServiceParams, Transport, TransportFactory};

#[cfg(any(feature = "rustls-tls", feature = "rustls-no-provider"))]
pub use rustls;

/// Build a `reqwest::Client` whose TLS layer matches this crate's feature
/// selection, optionally adding extra root certificates from a PEM bundle.
pub fn default_reqwest_client(
    #[allow(unused_variables)] extra_root_pem: Option<&[u8]>,
) -> Result<reqwest::Client, a2a::A2AError> {
    let builder = reqwest::Client::builder();

    #[cfg(any(
        feature = "rustls-tls",
        feature = "rustls-no-provider",
        feature = "native-tls"
    ))]
    let builder = match extra_root_pem {
        Some(pem) => {
            let certs = reqwest::Certificate::from_pem_bundle(pem)
                .map_err(|e| a2a::A2AError::internal(format!("invalid PEM certificate: {e}")))?;
            builder.tls_certs_merge(certs)
        }
        None => builder,
    };

    builder
        .build()
        .map_err(|e| a2a::A2AError::internal(format!("failed to build HTTP client: {e}")))
}

#[cfg(all(test, any(feature = "rustls-tls", feature = "native-tls")))]
pub(crate) mod test_utils {
    pub fn rcgen_self_signed_ca_pem() -> Vec<u8> {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "Test CA");
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        cert.pem().into_bytes()
    }
}
