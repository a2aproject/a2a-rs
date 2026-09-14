// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Control frames and errors shared by both serialization variants.
//!
//! Per 2.2, `handshake`, `handshakeAck` and `heartbeat` are always UTF-8 JSON
//! regardless of the negotiated variant, because the variant is not yet chosen
//! when they are exchanged.

use serde::{Deserialize, Serialize};

/// Serialization variant negotiated during the handshake (2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Variant {
    #[serde(rename = "stdio-json")]
    Json,
    #[serde(rename = "stdio-proto")]
    Proto,
}

impl Variant {
    /// The token used on the wire; must match the serde renames above.
    pub fn as_str(self) -> &'static str {
        match self {
            Variant::Json => "stdio-json",
            Variant::Proto => "stdio-proto",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handshake {
    pub protocol: String,
    pub protocol_version: String,
    pub session_id: String,
    /// Ordered by server preference, most preferred first (2.2).
    pub supported_variants: Vec<Variant>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandshakeAck {
    pub session_id: String,
    pub select_variant: Variant,
    pub accept: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Heartbeat {
    pub session_id: String,
    /// Unix epoch milliseconds.
    pub ts: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlFrame {
    #[serde(rename = "handshake")]
    Handshake(Handshake),
    #[serde(rename = "handshakeAck")]
    HandshakeAck(HandshakeAck),
    #[serde(rename = "heartbeat")]
    Heartbeat(Heartbeat),
}

/// Errors raised while decoding a frame body into a typed message.
#[derive(Debug, thiserror::Error)]
pub enum WireError {
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("message is not a JSON object")]
    NotAnObject,

    #[error("bad or missing jsonrpc version: {0:?}")]
    BadVersion(String),

    #[error("unrecognized message shape")]
    UnrecognizedMessage,
}
