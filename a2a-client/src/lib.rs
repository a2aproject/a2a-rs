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
