// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

use rcgen::{CertificateParams, DistinguishedName, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

use crate::error::PluginError;

pub struct LoopbackTls {
    pub cert_pem: String,
    pub server_config: std::sync::Arc<rustls::ServerConfig>,
}

/// Generate a self-signed cert/key for the loopback gRPC proxy.
/// The cert PEM is sent in the handshake so the CLI can pin it.
pub fn generate_loopback_tls() -> Result<LoopbackTls, PluginError> {
    let key_pair = KeyPair::generate().map_err(|e| PluginError::Tls(e.to_string()))?;

    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params.subject_alt_names = vec![rcgen::SanType::IpAddress(
        "127.0.0.1".parse().expect("valid IP"),
    )];

    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| PluginError::Tls(e.to_string()))?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    let certs: Vec<CertificateDer<'static>> =
        rustls_pemfile::certs(&mut cert_pem.as_bytes())
            .collect::<Result<_, _>>()
            .map_err(|e| PluginError::Tls(format!("parse cert: {e}")))?;

    let key: PrivateKeyDer<'static> = rustls_pemfile::private_key(&mut key_pem.as_bytes())
        .map_err(|e| PluginError::Tls(format!("parse key: {e}")))?
        .ok_or_else(|| PluginError::Tls("no private key in PEM".to_string()))?;

    let mut server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| PluginError::Tls(format!("build server TLS config: {e}")))?;

    // gRPC requires ALPN h2
    server_config.alpn_protocols = vec![tonic_tls::ALPN_H2.to_vec()];

    Ok(LoopbackTls {
        cert_pem,
        server_config: std::sync::Arc::new(server_config),
    })
}
