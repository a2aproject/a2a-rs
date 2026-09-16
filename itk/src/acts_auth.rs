// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! ACTS authentication support.
//!
//! Five ACTS tests assert that an agent requiring a credential rejects a
//! request that lacks one. A2A conditions that obligation on the agent's own
//! declared requirements, so those tests gate on a card declaring
//! `securitySchemes` and `securityRequirements` — an agent declaring neither is
//! not violating anything by serving an unauthenticated request. Every ITK
//! agent declares neither by default, because traversal peers dial it with no
//! credential.
//!
//! Enforcement is therefore opt-in, and the ACTS runner turns it on for a
//! separate pass over just those tests. It cannot be on for the main pass: raw
//! steps are sent exactly as written, so an absent `Authorization` header means
//! "reject me" in `SEC-AUTH-001` and "serve me" in `JSONRPC-ENV-001`, and no
//! server can tell those two requests apart.
//!
//! The extended card is the exception, guarded in either mode: A2A §13.3 makes
//! its authentication unconditional, and no traversal scenario fetches one.

use std::collections::HashMap;

use a2a::{HttpAuthSecurityScheme, SecurityRequirement, SecurityScheme};
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;
use tracing::info;

/// The environment variable that asks the agent to require a credential.
pub const AUTH_ENFORCED_ENV: &str = "ITK_ACTS_AUTH";

/// Credentials the ACTS runner presents. Not secrets: the runner attaches the
/// valid one to every abstract operation and offers the insufficient one from
/// `SEC-AUTH-002` and `SEC-EXTCARD-002`, so a fixture has to recognise both to
/// answer 200 / 403 / 401 as those tests require.
const VALID_TOKEN: &str = "itk-valid-token";
const INSUFFICIENT_TOKEN: &str = "itk-insufficient-token";
const SCHEME_ID: &str = "bearerAuth";

/// Suffix rather than a whole path: the card is served at the root and under
/// each binding's prefix, and requiring a credential to read it would be
/// circular — A2A §8.2 makes the well-known URL the discovery mechanism and
/// §7.3 has the client learn its schemes from that card. The ITK readiness
/// probe fetches it too.
const CARD_PATH_SUFFIX: &str = ".well-known/agent-card.json";

/// Suffix for the same reason: the REST router is nested under a prefix.
const EXTENDED_CARD_SUFFIX: &str = "/extendedAgentCard";

pub fn auth_enforced() -> bool {
    std::env::var_os(AUTH_ENFORCED_ENV).is_some()
}

/// What the card advertises, declared only when the agent actually enforces
/// it: a card claiming a scheme it does not check would be a lie, and this is
/// what the ACTS `authentication` precondition reads to decide whether the
/// `SEC-AUTH` tests apply at all.
pub fn security_schemes() -> Option<HashMap<String, SecurityScheme>> {
    if !auth_enforced() {
        return None;
    }
    info!("Requiring a bearer credential (ACTS auth pass)");
    Some(HashMap::from([(
        SCHEME_ID.to_string(),
        SecurityScheme::HttpAuth(HttpAuthSecurityScheme {
            scheme: "Bearer".to_string(),
            description: Some("Bearer token presented by the ACTS runner.".to_string()),
            bearer_format: Some("opaque".to_string()),
        }),
    )]))
}

/// Separate from the schemes because the two mean different things: schemes
/// are what a client *may* use, requirements are what it *must*. An agent
/// publishing the first and not the second requires nothing.
pub fn security_requirements() -> Option<Vec<SecurityRequirement>> {
    if !auth_enforced() {
        return None;
    }
    Some(vec![SecurityRequirement::from([(
        SCHEME_ID.to_string(),
        Vec::new(),
    )])])
}

/// Three outcomes, because the tests distinguish them: the valid token
/// authorizes, the insufficient one authenticates but does not, and anything
/// else — including nothing at all — fails authentication.
enum Credential {
    Valid,
    Insufficient,
    Unusable,
}

fn presented(header: Option<&str>) -> Credential {
    let token = header
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .map(|(_, token)| token.trim())
        .unwrap_or_default();

    match token {
        VALID_TOKEN => Credential::Valid,
        INSUFFICIENT_TOKEN => Credential::Insufficient,
        _ => Credential::Unusable,
    }
}

/// A `google.rpc.Status` body, the shape A2A §11.6 requires of an error.
fn rejection(status: StatusCode, reason: &str, message: &str) -> Response {
    let body = json!({
        "error": {
            "code": status.as_u16(),
            "status": reason,
            "message": message,
            "details": [{
                "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                "reason": reason,
                "domain": "a2a-protocol.org",
            }],
        }
    });

    let mut response = (status, axum::Json(body)).into_response();
    if status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(r#"Bearer realm="a2a", scheme="bearerAuth""#),
        );
    }
    response
}

/// Guards the operation endpoints when enforcement is on, and the extended
/// card always. The public agent card stays reachable without a credential in
/// either mode.
pub async fn credential_middleware(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    if path.ends_with(CARD_PATH_SUFFIX) {
        return next.run(request).await;
    }
    if !auth_enforced() && !path.ends_with(EXTENDED_CARD_SUFFIX) {
        return next.run(request).await;
    }

    let header = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());

    match presented(header) {
        Credential::Valid => next.run(request).await,
        Credential::Insufficient => rejection(
            StatusCode::FORBIDDEN,
            "PERMISSION_DENIED",
            "Token lacks the required scope.",
        ),
        Credential::Unusable => rejection(
            StatusCode::UNAUTHORIZED,
            "UNAUTHENTICATED",
            "A bearer token is required.",
        ),
    }
}

/// The same rule over gRPC metadata, so that a card claiming an agent-wide
/// requirement is not contradicted by one binding that serves anyone. gRPC
/// keys are lowercase by protocol.
pub fn grpc_interceptor(request: tonic::Request<()>) -> Result<tonic::Request<()>, tonic::Status> {
    if !auth_enforced() {
        return Ok(request);
    }

    let header = request
        .metadata()
        .get("authorization")
        .and_then(|value| value.to_str().ok());

    match presented(header) {
        Credential::Valid => Ok(request),
        Credential::Insufficient => Err(tonic::Status::permission_denied(
            "Token lacks the required scope.",
        )),
        Credential::Unusable => Err(tonic::Status::unauthenticated(
            "A bearer token is required.",
        )),
    }
}
