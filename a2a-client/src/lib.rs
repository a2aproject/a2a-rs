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

/// Build a `reqwest::Client` whose TLS layer matches this crate's feature
/// selection, optionally adding extra root certificates from a PEM bundle.
///
/// - When `rustls-tls-aws-lc-rs` or `rustls-tls-ring` is enabled, a
///   `rustls::ClientConfig` is constructed with the corresponding provider
///   and handed to `reqwest::ClientBuilder::use_preconfigured_tls`. Custom
///   PEM certificates are parsed and added to the rustls root store
///   alongside the webpki roots. No process-global `CryptoProvider` is
///   installed.
/// - When only `rustls-tls` is enabled (no provider variant), reqwest
///   falls back to whatever `CryptoProvider` the application installed via
///   `install_default()`. If none is installed, `reqwest::Client::new()`
///   panics; that is the contract the consumer accepted by disabling the
///   provider variants.
/// - When the rustls features are off (e.g. only `native-tls`), extra
///   certificates are added via `reqwest::ClientBuilder::add_root_certificate`.
pub fn default_reqwest_client(
    extra_root_pem: Option<&[u8]>,
) -> Result<reqwest::Client, a2a::A2AError> {
    let builder = reqwest::Client::builder();

    #[cfg(any(feature = "rustls-tls-aws-lc-rs", feature = "rustls-tls-ring"))]
    let builder = builder.use_preconfigured_tls(rustls_client_config(extra_root_pem)?);

    #[cfg(not(any(feature = "rustls-tls-aws-lc-rs", feature = "rustls-tls-ring")))]
    let builder = match extra_root_pem {
        Some(pem) => {
            let cert = reqwest::Certificate::from_pem(pem)
                .map_err(|e| a2a::A2AError::internal(format!("invalid PEM certificate: {e}")))?;
            builder.add_root_certificate(cert)
        }
        None => builder,
    };

    builder
        .build()
        .map_err(|e| a2a::A2AError::internal(format!("failed to build HTTP client: {e}")))
}

#[cfg(any(feature = "rustls-tls-aws-lc-rs", feature = "rustls-tls-ring"))]
fn rustls_client_config(
    extra_root_pem: Option<&[u8]>,
) -> Result<rustls::ClientConfig, a2a::A2AError> {
    let provider = std::sync::Arc::new(selected_crypto_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if let Some(pem) = extra_root_pem {
        for cert in rustls_pemfile::certs(&mut std::io::Cursor::new(pem)) {
            let der =
                cert.map_err(|e| a2a::A2AError::internal(format!("invalid PEM certificate: {e}")))?;
            roots
                .add(der)
                .map_err(|e| a2a::A2AError::internal(format!("failed to add CA: {e}")))?;
        }
    }
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("safe default protocol versions are supported")
        .with_root_certificates(roots)
        .with_no_client_auth())
}

#[cfg(feature = "rustls-tls-aws-lc-rs")]
fn selected_crypto_provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::aws_lc_rs::default_provider()
}

#[cfg(all(feature = "rustls-tls-ring", not(feature = "rustls-tls-aws-lc-rs")))]
fn selected_crypto_provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::ring::default_provider()
}

pub(crate) fn a2a_error_from_details(
    code: i32,
    message: String,
    details: Vec<a2a::TypedDetail>,
) -> a2a::A2AError {
    use a2a::{error_code, errordetails, reason_to_error_code};
    use serde_json::Value;

    let mut code = code;
    let mut message = message;

    for detail in &details {
        match detail.type_url.as_str() {
            errordetails::BAD_REQUEST_TYPE => {
                if let Some(Value::Array(violations)) = detail.value.get("fieldViolations") {
                    let violation_strs: Vec<String> = violations
                        .iter()
                        .filter_map(|v| {
                            let field = v.get("field")?.as_str()?;
                            let desc = v.get("description")?.as_str()?;
                            if field.is_empty() {
                                Some(desc.to_string())
                            } else {
                                Some(format!("{field}: {desc}"))
                            }
                        })
                        .collect();
                    if !violation_strs.is_empty() {
                        message = format!("{}: {}", message, violation_strs.join("; "));
                    }
                }
                if code == error_code::INTERNAL_ERROR {
                    code = error_code::INVALID_PARAMS;
                }
            }
            errordetails::ERROR_INFO_TYPE => {
                if let Some(Value::String(domain)) = detail.value.get("domain") {
                    if domain == errordetails::PROTOCOL_DOMAIN {
                        if let Some(Value::String(reason)) = detail.value.get("reason") {
                            if let Some(c) = reason_to_error_code(reason) {
                                code = c;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    a2a::A2AError {
        code,
        message,
        details: (!details.is_empty()).then_some(details),
    }
}

#[cfg(test)]
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
