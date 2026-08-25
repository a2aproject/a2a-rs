// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Process lifecycle and startup handshake (2).
//!
//! Roles here are process roles, not network roles: the *client* is the parent
//! that spawns, the *server* is the child that was spawned. The handshake is
//! server-first (2.2) — child emits `handshake`, parent replies `handshakeAck`.
//! Both halves live together so they stay consistent, though only one runs in
//! any given process, and both are pure so negotiation is testable without
//! spawning anything.

use std::collections::HashMap;

use crate::wire::{Handshake, HandshakeAck, Variant};

pub const ENV_SESSION_ID: &str = "A2A_SESSION_ID"; // REQUIRED
pub const ENV_PROTOCOL_VERSION: &str = "A2A_PROTOCOL_VERSION";
pub const ENV_LOG_LEVEL: &str = "A2A_LOG_LEVEL";
pub const ENV_SERVICE_PARAM_PREFIX: &str = "A2A_SP_";
pub const PROTOCOL_NAME: &str = "a2a-stdio";
pub const PROTOCOL_VERSION: &str = "1.0";

/// Values advertised in the handshake `features` array (2.2, 2.3).
pub const FEATURE_STREAMING: &str = "streaming";

/// Reserved. 2.3 makes heartbeats OPTIONAL and nothing here emits or polices
/// them yet, so advertising this would promise liveness the server never keeps.
pub const FEATURE_HEARTBEATS: &str = "heartbeats";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ExitCode {
    Ok = 0,                  // graceful shutdown, or handshake declined
    GenericError = 1,        // unspecified fatal error
    ProtocolError = 2,       // malformed framing or handshake violation
    VersionNotSupported = 3, // client asked for an A2A version we cannot serve
    StartupDenied = 4,       // required startup context missing/invalid
}
impl ExitCode {
    pub fn as_i32(self) -> i32 {
        self as i32
    }
}

pub fn service_params_from_env<I>(vars: I) -> HashMap<String, Vec<String>>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut result = HashMap::new();
    for (name, value) in vars {
        let Some(rest) = name.strip_prefix(ENV_SERVICE_PARAM_PREFIX) else {
            continue;
        };
        let key = rest.to_ascii_lowercase().replace('_', "-");
        let values = split_and_trim(value.split(','));
        result.insert(key, values);
    }
    result
}

fn split_and_trim<'a>(iter: impl Iterator<Item = &'a str>) -> Vec<String> {
    iter.map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn session_id_from_env(get: impl Fn(&str) -> Option<String>) -> Result<String, ExitCode> {
    let Some(session_id) = get(ENV_SESSION_ID) else {
        return Err(ExitCode::StartupDenied);
    };
    if session_id.is_empty() {
        return Err(ExitCode::StartupDenied);
    }
    Ok(session_id)
}

// ---------------------------------------------------------------------------
// Negotiation errors
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, thiserror::Error)]
pub enum NegotiationError {
    #[error("peer reported protocol {0:?}, expected \"a2a-stdio\"")]
    UnknownProtocol(String),

    #[error("session id mismatch: expected {expected:?}, got {actual:?}")]
    SessionIdMismatch { expected: String, actual: String },

    #[error("peer selected variant {0:?} which was not offered")]
    VariantNotOffered(Variant),

    #[error("no serialization variant in common")]
    NoCommonVariant,

    #[error("protocol version {0:?} is not supported")]
    VersionNotSupported(String),
}

impl NegotiationError {
    /// Keeps the 7.2 / 2.4 error-to-exit-code mapping in a single place.
    pub fn exit_code(&self) -> ExitCode {
        match self {
            NegotiationError::VersionNotSupported(_) => ExitCode::VersionNotSupported,
            _ => ExitCode::ProtocolError,
        }
    }
}

// ---------------------------------------------------------------------------
// Server side (runs in the spawned child)
// ---------------------------------------------------------------------------
/// What the client's `handshakeAck` told us to do (2.2 steps 3-5).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckOutcome {
    /// Serve every subsequent A2A frame using this variant.
    Accepted(Variant),
    /// The client refused the session; exit with [`ExitCode::Ok`].
    Declined,
}

/// Build the `handshake` frame the server writes to `STDOUT` at startup (2.2 step 1).
///
/// `supported` is ordered by server preference, most preferred first, and must
/// contain [`Variant::Json`] because 1 makes `stdio-json` mandatory for servers.
/// `session_id` must be the value received in `A2A_SESSION_ID` (2.2 step 2).
pub fn build_handshake(
    session_id: String,
    supported: Vec<Variant>,
    features: Vec<String>,
    pid: Option<u32>,
) -> Handshake {
    debug_assert!(
        supported.contains(&Variant::Json),
        "servers must support stdio-json (1)"
    );
    Handshake {
        protocol: PROTOCOL_NAME.to_string(),
        protocol_version: PROTOCOL_VERSION.to_string(),
        session_id,
        supported_variants: supported,
        features,
        pid,
    }
}

/// Validate the client's reply against what we offered (2.2 steps 3-5).
///
/// `offered` is the frame previously produced by [`build_handshake`].
pub fn accept_ack(offered: &Handshake, ack: &HandshakeAck) -> Result<AckOutcome, NegotiationError> {
    // Identity before intent: a frame we cannot attribute to this session is not
    // one whose `accept` flag we should act on.
    if ack.session_id != offered.session_id {
        return Err(NegotiationError::SessionIdMismatch {
            expected: offered.session_id.clone(),
            actual: ack.session_id.clone(),
        });
    }

    // 2.2 step 4: a refusal is a clean shutdown, not a protocol violation.
    if !ack.accept {
        return Ok(AckOutcome::Declined);
    }

    // 2.2 step 5: selecting a variant we never advertised is fatal.
    if !offered.supported_variants.contains(&ack.select_variant) {
        return Err(NegotiationError::VariantNotOffered(ack.select_variant));
    }

    Ok(AckOutcome::Accepted(ack.select_variant))
}

// ---------------------------------------------------------------------------
// Client side (runs in the spawning parent)
// ---------------------------------------------------------------------------

/// Validate the server's `handshake` and choose a variant (2.2 steps 2-3).
///
/// `expected_session_id` is the value the parent passed as `A2A_SESSION_ID`;
/// `client_supported` is what this client can actually speak. Returns the ack to
/// write to the child's `STDIN` together with the agreed variant.
pub fn respond_to_handshake(
    hs: &Handshake,
    expected_session_id: &str,
    client_supported: &[Variant],
) -> Result<(HandshakeAck, Variant), NegotiationError> {
    if hs.protocol != PROTOCOL_NAME {
        return Err(NegotiationError::UnknownProtocol(hs.protocol.clone()));
    }

    // 2.2 step 2: the client MUST reject a session id that is not its own.
    if hs.session_id != expected_session_id {
        return Err(NegotiationError::SessionIdMismatch {
            expected: expected_session_id.into(),
            actual: hs.session_id.clone(),
        });
    }

    if !version_compatible(&hs.protocol_version) {
        return Err(NegotiationError::VersionNotSupported(
            hs.protocol_version.clone(),
        ));
    }

    // Walk the server's list in its own order: that ordering is the server's
    // preference (2.2 step 1), and ours is not consulted.
    let chosen = *hs
        .supported_variants
        .iter()
        .find(|v| client_supported.contains(v))
        .ok_or(NegotiationError::NoCommonVariant)?;

    Ok((
        HandshakeAck {
            session_id: hs.session_id.clone(),
            select_variant: chosen,
            accept: true,
        },
        chosen,
    ))
}

/// Refuse the session (2.2 step 4). The server exits with [`ExitCode::Ok`], so
/// this is a polite goodbye rather than an error; `select_variant` is ignored.
pub fn decline(session_id: &str) -> HandshakeAck {
    HandshakeAck {
        session_id: session_id.into(),
        select_variant: Variant::Json,
        accept: false,
    }
}

/// Major versions must agree; a newer minor is assumed backward compatible.
fn version_compatible(peer: &str) -> bool {
    fn major(v: &str) -> Option<&str> {
        v.split('.').next().filter(|s| !s.is_empty())
    }
    match (major(peer), major(PROTOCOL_VERSION)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offered(supported: Vec<Variant>) -> Handshake {
        build_handshake("s-1".into(), supported, vec![], Some(42137))
    }

    fn ack(session_id: &str, select_variant: Variant, accept: bool) -> HandshakeAck {
        HandshakeAck {
            session_id: session_id.into(),
            select_variant,
            accept,
        }
    }

    #[test]
    fn handshake_carries_the_binding_identity() {
        let hs = offered(vec![Variant::Json]);
        assert_eq!(hs.protocol, "a2a-stdio");
        assert_eq!(hs.protocol_version, "1.0");
        assert_eq!(hs.session_id, "s-1");
        assert_eq!(hs.pid, Some(42137));
    }

    #[test]
    fn handshake_preserves_preference_order() {
        // 2.2 step 1: the client walks this list in order, so it must not be reordered.
        let hs = offered(vec![Variant::Proto, Variant::Json]);
        assert_eq!(hs.supported_variants, vec![Variant::Proto, Variant::Json]);
    }

    #[test]
    fn accepted_ack_selects_the_variant() {
        let hs = offered(vec![Variant::Json, Variant::Proto]);
        let out = accept_ack(&hs, &ack("s-1", Variant::Proto, true)).unwrap();
        assert_eq!(out, AckOutcome::Accepted(Variant::Proto));
    }

    #[test]
    fn declined_ack_is_a_clean_shutdown_not_an_error() {
        let hs = offered(vec![Variant::Json]);
        let out = accept_ack(&hs, &ack("s-1", Variant::Json, false)).unwrap();
        assert_eq!(out, AckOutcome::Declined);
    }

    #[test]
    fn selecting_an_unoffered_variant_is_a_protocol_error() {
        let hs = offered(vec![Variant::Json]);
        let err = accept_ack(&hs, &ack("s-1", Variant::Proto, true)).unwrap_err();
        assert!(
            matches!(err, NegotiationError::VariantNotOffered(Variant::Proto)),
            "got {err:?}"
        );
        assert_eq!(err.exit_code(), ExitCode::ProtocolError);
    }

    #[test]
    fn mismatched_session_id_is_rejected_even_when_declining() {
        let hs = offered(vec![Variant::Json]);
        let err = accept_ack(&hs, &ack("other", Variant::Json, false)).unwrap_err();
        assert!(
            matches!(err, NegotiationError::SessionIdMismatch { .. }),
            "got {err:?}"
        );
        assert_eq!(err.exit_code(), ExitCode::ProtocolError);
    }

    #[test]
    fn version_failures_use_their_own_exit_code() {
        let err = NegotiationError::VersionNotSupported("9.9".into());
        assert_eq!(err.exit_code(), ExitCode::VersionNotSupported);
        assert_eq!(err.exit_code().as_i32(), 3);
    }

    #[test]
    fn client_honours_the_servers_preference_order() {
        // Server prefers json; our own list prefers proto. The server wins.
        let hs = offered(vec![Variant::Json, Variant::Proto]);
        let (ack, chosen) =
            respond_to_handshake(&hs, "s-1", &[Variant::Proto, Variant::Json]).unwrap();
        assert_eq!(chosen, Variant::Json);
        assert_eq!(ack.select_variant, Variant::Json);
        assert!(ack.accept);
        assert_eq!(ack.session_id, "s-1");
    }

    #[test]
    fn client_skips_variants_it_cannot_speak() {
        let hs = offered(vec![Variant::Proto, Variant::Json]);
        let (_, chosen) = respond_to_handshake(&hs, "s-1", &[Variant::Json]).unwrap();
        assert_eq!(chosen, Variant::Json);
    }

    #[test]
    fn no_overlap_is_reported_as_no_common_variant() {
        let hs = offered(vec![Variant::Json]);
        let err = respond_to_handshake(&hs, "s-1", &[Variant::Proto]).unwrap_err();
        assert!(
            matches!(err, NegotiationError::NoCommonVariant),
            "got {err:?}"
        );
        assert_eq!(err.exit_code(), ExitCode::ProtocolError);
    }

    #[test]
    fn empty_variant_list_does_not_panic() {
        // Built by hand: a conformant server never sends this, but a buggy or
        // hostile one can, and it must not take the client process down.
        let hs = Handshake {
            protocol: PROTOCOL_NAME.into(),
            protocol_version: PROTOCOL_VERSION.into(),
            session_id: "s-1".into(),
            supported_variants: vec![],
            features: vec![],
            pid: None,
        };
        assert!(matches!(
            respond_to_handshake(&hs, "s-1", &[Variant::Json]).unwrap_err(),
            NegotiationError::NoCommonVariant
        ));
    }

    #[test]
    fn client_rejects_a_foreign_session_id() {
        let hs = offered(vec![Variant::Json]);
        match respond_to_handshake(&hs, "s-2", &[Variant::Json]).unwrap_err() {
            NegotiationError::SessionIdMismatch { expected, actual } => {
                assert_eq!(expected, "s-2");
                assert_eq!(actual, "s-1");
            }
            other => panic!("expected SessionIdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn client_rejects_a_foreign_protocol() {
        let mut hs = offered(vec![Variant::Json]);
        hs.protocol = "not-a2a".into();
        let err = respond_to_handshake(&hs, "s-1", &[Variant::Json]).unwrap_err();
        assert!(
            matches!(err, NegotiationError::UnknownProtocol(_)),
            "got {err:?}"
        );
    }

    #[test]
    fn incompatible_major_version_exits_with_code_three() {
        let mut hs = offered(vec![Variant::Json]);
        hs.protocol_version = "2.0".into();
        let err = respond_to_handshake(&hs, "s-1", &[Variant::Json]).unwrap_err();
        assert!(
            matches!(err, NegotiationError::VersionNotSupported(ref v) if v == "2.0"),
            "got {err:?}"
        );
        assert_eq!(err.exit_code().as_i32(), 3);
    }

    #[test]
    fn newer_minor_version_is_accepted() {
        let mut hs = offered(vec![Variant::Json]);
        hs.protocol_version = "1.7".into();
        assert!(respond_to_handshake(&hs, "s-1", &[Variant::Json]).is_ok());
    }

    #[test]
    fn decline_refuses_without_selecting() {
        let ack = decline("s-1");
        assert!(!ack.accept);
        assert_eq!(ack.session_id, "s-1");
    }

    #[test]
    fn the_two_halves_agree_on_a_full_exchange() {
        // Server offers, client responds, server validates the response.
        let hs = offered(vec![Variant::Json, Variant::Proto]);
        let (ack, client_choice) =
            respond_to_handshake(&hs, "s-1", &[Variant::Json, Variant::Proto]).unwrap();
        assert_eq!(
            accept_ack(&hs, &ack).unwrap(),
            AckOutcome::Accepted(client_choice)
        );
    }

    #[test]
    fn a_declining_client_is_seen_as_a_clean_shutdown() {
        let hs = offered(vec![Variant::Json]);
        assert_eq!(
            accept_ack(&hs, &decline("s-1")).unwrap(),
            AckOutcome::Declined
        );
    }

    #[test]
    fn missing_or_empty_session_id_denies_startup() {
        assert_eq!(
            session_id_from_env(|_| None).unwrap_err(),
            ExitCode::StartupDenied
        );
        assert_eq!(
            session_id_from_env(|_| Some(String::new())).unwrap_err(),
            ExitCode::StartupDenied
        );
        assert_eq!(session_id_from_env(|_| Some("s-1".into())).unwrap(), "s-1");
    }

    #[test]
    fn env_service_params_are_normalized() {
        // 4.1: strip the prefix, lowercase, underscores become hyphens.
        let vars = vec![
            ("A2A_SP_A2A_VERSION".to_string(), "1.0".to_string()),
            ("A2A_SP_A2A_EXTENSIONS".to_string(), "a, b ,c".to_string()),
            ("PATH".to_string(), "/usr/bin".to_string()),
        ];
        let sp = service_params_from_env(vars);
        assert_eq!(sp["a2a-version"], vec!["1.0"]);
        assert_eq!(sp["a2a-extensions"], vec!["a", "b", "c"]);
        assert!(!sp.contains_key("path"), "unprefixed vars must be ignored");
    }
}
