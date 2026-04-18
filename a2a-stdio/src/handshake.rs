// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Handshake types and logic for the STDIO transport.
//!
//! After the server process starts, it sends a `Handshake` message to the
//! client. The client replies with a `HandshakeAck` to accept or reject
//! the connection and select a protocol variant.

use crate::errors::StdioError;
use crate::framing;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tokio::io::{AsyncBufRead, AsyncWrite};

/// Features the server advertises during the handshake.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HandshakeFeatures {
    /// Whether the server supports heartbeat pings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat: Option<bool>,

    /// Heartbeat interval in seconds (default 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub heartbeat_interval_secs: Option<u32>,

    /// Additional feature flags for forward compatibility.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Server → Client handshake message, sent immediately after startup.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Handshake {
    /// Must be `"handshake"`.
    #[serde(rename = "type")]
    pub msg_type: String,

    /// Session identifier (matches `A2A_SESSION_ID` env var if set).
    pub session_id: String,

    /// Protocol variants the server supports (e.g. `["a2a/v1"]`).
    pub supported_variants: Vec<String>,

    /// Server process ID.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,

    /// Optional feature negotiation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub features: Option<HandshakeFeatures>,
}

/// Client → Server handshake acknowledgment.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HandshakeAck {
    /// Must be `"handshakeAck"`.
    #[serde(rename = "type")]
    pub msg_type: String,

    /// The variant the client selected from `supported_variants`.
    pub selected_variant: String,

    /// Whether the client accepts the connection.
    pub accept: bool,
}

impl Handshake {
    /// Create a new handshake message.
    pub fn new(session_id: String, supported_variants: Vec<String>) -> Self {
        Handshake {
            msg_type: "handshake".to_string(),
            session_id,
            supported_variants,
            pid: None,
            features: None,
        }
    }
}

impl HandshakeAck {
    /// Create an accepting handshake ack.
    pub fn accept(selected_variant: String) -> Self {
        HandshakeAck {
            msg_type: "handshakeAck".to_string(),
            selected_variant,
            accept: true,
        }
    }

    /// Create a rejecting handshake ack.
    pub fn reject() -> Self {
        HandshakeAck {
            msg_type: "handshakeAck".to_string(),
            selected_variant: String::new(),
            accept: false,
        }
    }
}

/// Read a `Handshake` message from the server's stdout.
pub async fn read_handshake<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<Handshake, StdioError> {
    let frame = framing::read_frame(reader)
        .await?
        .ok_or_else(|| StdioError::HandshakeFailed("EOF before handshake".to_string()))?;

    // Parse as generic JSON first for better error messages.
    let value: serde_json::Value = serde_json::from_slice(&frame)
        .map_err(|e| StdioError::HandshakeFailed(format!("invalid JSON: {e}")))?;

    let msg_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

    if msg_type != "handshake" {
        return Err(StdioError::HandshakeFailed(format!(
            "expected type \"handshake\", got \"{}\"",
            msg_type
        )));
    }

    let handshake: Handshake = serde_json::from_value(value)
        .map_err(|e| StdioError::HandshakeFailed(format!("invalid handshake: {e}")))?;

    if handshake.supported_variants.is_empty() {
        return Err(StdioError::HandshakeFailed(
            "server offered no supported variants".to_string(),
        ));
    }

    Ok(handshake)
}

/// Write a `Handshake` message (server side).
pub async fn write_handshake<W: AsyncWrite + Unpin>(
    writer: &mut W,
    handshake: &Handshake,
) -> Result<(), StdioError> {
    let body = serde_json::to_vec(handshake)?;
    framing::write_frame(writer, &body).await
}

/// Read a `HandshakeAck` from the client's response.
pub async fn read_handshake_ack<R: AsyncBufRead + Unpin>(
    reader: &mut R,
) -> Result<HandshakeAck, StdioError> {
    let frame = framing::read_frame(reader)
        .await?
        .ok_or_else(|| StdioError::HandshakeFailed("EOF before handshake ack".to_string()))?;

    let value: serde_json::Value = serde_json::from_slice(&frame)
        .map_err(|e| StdioError::HandshakeFailed(format!("invalid JSON: {e}")))?;

    let msg_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");

    if msg_type != "handshakeAck" {
        return Err(StdioError::HandshakeFailed(format!(
            "expected type \"handshakeAck\", got \"{}\"",
            msg_type
        )));
    }

    let ack: HandshakeAck = serde_json::from_value(value)
        .map_err(|e| StdioError::HandshakeFailed(format!("invalid handshake ack: {e}")))?;

    Ok(ack)
}

/// Write a `HandshakeAck` message (client side).
pub async fn write_handshake_ack<W: AsyncWrite + Unpin>(
    writer: &mut W,
    ack: &HandshakeAck,
) -> Result<(), StdioError> {
    let body = serde_json::to_vec(ack)?;
    framing::write_frame(writer, &body).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use tokio::io::BufReader;

    #[test]
    fn test_handshake_serde() {
        let hs = Handshake::new("sess-1".into(), vec!["a2a/v1".into()]);
        let json = serde_json::to_string(&hs).unwrap();
        let back: Handshake = serde_json::from_str(&json).unwrap();
        assert_eq!(hs, back);
        assert_eq!(back.msg_type, "handshake");
    }

    #[test]
    fn test_handshake_ack_accept_serde() {
        let ack = HandshakeAck::accept("a2a/v1".into());
        let json = serde_json::to_string(&ack).unwrap();
        let back: HandshakeAck = serde_json::from_str(&json).unwrap();
        assert!(back.accept);
        assert_eq!(back.selected_variant, "a2a/v1");
    }

    #[test]
    fn test_handshake_ack_reject_serde() {
        let ack = HandshakeAck::reject();
        assert!(!ack.accept);
    }

    #[test]
    fn test_handshake_with_features() {
        let hs = Handshake {
            msg_type: "handshake".into(),
            session_id: "s1".into(),
            supported_variants: vec!["a2a/v1".into()],
            pid: Some(1234),
            features: Some(HandshakeFeatures {
                heartbeat: Some(true),
                heartbeat_interval_secs: Some(30),
                extra: HashMap::new(),
            }),
        };
        let json = serde_json::to_string(&hs).unwrap();
        let back: Handshake = serde_json::from_str(&json).unwrap();
        assert_eq!(back.pid, Some(1234));
        assert_eq!(back.features.unwrap().heartbeat, Some(true));
    }

    #[tokio::test]
    async fn test_handshake_roundtrip_over_frames() {
        let hs = Handshake::new("sess-1".into(), vec!["a2a/v1".into()]);

        // Server writes handshake.
        let mut buf = Vec::new();
        write_handshake(&mut buf, &hs).await.unwrap();

        // Client reads handshake.
        let mut reader = BufReader::new(Cursor::new(buf));
        let received = read_handshake(&mut reader).await.unwrap();
        assert_eq!(received, hs);
    }

    #[tokio::test]
    async fn test_handshake_ack_roundtrip_over_frames() {
        let ack = HandshakeAck::accept("a2a/v1".into());

        // Client writes ack.
        let mut buf = Vec::new();
        write_handshake_ack(&mut buf, &ack).await.unwrap();

        // Server reads ack.
        let mut reader = BufReader::new(Cursor::new(buf));
        let received = read_handshake_ack(&mut reader).await.unwrap();
        assert_eq!(received, ack);
    }

    #[tokio::test]
    async fn test_read_handshake_rejects_wrong_type() {
        // Send a handshakeAck where a handshake is expected.
        let ack = HandshakeAck::accept("a2a/v1".into());
        let mut buf = Vec::new();
        write_handshake_ack(&mut buf, &ack).await.unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_handshake(&mut reader).await.unwrap_err();
        assert!(matches!(err, StdioError::HandshakeFailed(_)));
        assert!(err.to_string().contains("expected type \"handshake\""));
    }

    #[tokio::test]
    async fn test_read_handshake_rejects_missing_fields() {
        // Send JSON with correct type but missing required fields.
        let json = br#"{"type":"handshake"}"#;
        let mut buf = Vec::new();
        framing::write_frame(&mut buf, json).await.unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_handshake(&mut reader).await.unwrap_err();
        assert!(matches!(err, StdioError::HandshakeFailed(_)));
        assert!(err.to_string().contains("invalid handshake"));
    }

    #[tokio::test]
    async fn test_read_handshake_eof() {
        let mut reader = BufReader::new(Cursor::new(Vec::<u8>::new()));
        let err = read_handshake(&mut reader).await.unwrap_err();
        assert!(matches!(err, StdioError::HandshakeFailed(_)));
    }

    #[tokio::test]
    async fn test_read_handshake_rejects_invalid_json() {
        let raw = b"not-json-at-all";
        let mut buf = Vec::new();
        framing::write_frame(&mut buf, raw).await.unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_handshake(&mut reader).await.unwrap_err();
        assert!(matches!(err, StdioError::HandshakeFailed(_)));
        assert!(err.to_string().contains("invalid JSON"));
    }

    #[tokio::test]
    async fn test_read_handshake_rejects_empty_supported_variants() {
        let json = br#"{"type":"handshake","sessionId":"s","supportedVariants":[]}"#;
        let mut buf = Vec::new();
        framing::write_frame(&mut buf, json).await.unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_handshake(&mut reader).await.unwrap_err();
        match err {
            StdioError::HandshakeFailed(msg) => {
                assert!(msg.contains("no supported variants"), "got: {msg}");
            }
            other => panic!("expected HandshakeFailed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_read_handshake_ack_eof() {
        let mut reader = BufReader::new(Cursor::new(Vec::<u8>::new()));
        let err = read_handshake_ack(&mut reader).await.unwrap_err();
        assert!(matches!(err, StdioError::HandshakeFailed(_)));
        assert!(err.to_string().contains("EOF"));
    }

    #[tokio::test]
    async fn test_read_handshake_ack_rejects_invalid_json() {
        let raw = b"\xff\xfe garbage";
        let mut buf = Vec::new();
        framing::write_frame(&mut buf, raw).await.unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_handshake_ack(&mut reader).await.unwrap_err();
        assert!(matches!(err, StdioError::HandshakeFailed(_)));
        assert!(err.to_string().contains("invalid JSON"));
    }

    #[tokio::test]
    async fn test_read_handshake_ack_rejects_wrong_type() {
        // Send a handshake where an ack is expected.
        let hs = Handshake::new("s".into(), vec!["a2a/v1".into()]);
        let mut buf = Vec::new();
        write_handshake(&mut buf, &hs).await.unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_handshake_ack(&mut reader).await.unwrap_err();
        assert!(matches!(err, StdioError::HandshakeFailed(_)));
        assert!(err.to_string().contains("expected type \"handshakeAck\""));
    }

    #[tokio::test]
    async fn test_read_handshake_ack_rejects_missing_fields() {
        let json = br#"{"type":"handshakeAck"}"#;
        let mut buf = Vec::new();
        framing::write_frame(&mut buf, json).await.unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let err = read_handshake_ack(&mut reader).await.unwrap_err();
        assert!(matches!(err, StdioError::HandshakeFailed(_)));
        assert!(err.to_string().contains("invalid handshake ack"));
    }
}
