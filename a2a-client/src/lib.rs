// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
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
        crate::jsonrpc::parse_stream_tail(buf, parse_event)
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
