// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Message layer for the `stdio-json` variant (3, 4.2, 8.5).
//!
//! The frames here are JSON-RPC 2.0 objects plus three A2A extension members:
//! `serviceParams`, `streamEnd` and `cancelStream`. Per 3.1, unrecognized
//! members must be ignored rather than rejected, so nothing in this module uses
//! `deny_unknown_fields`.

use std::collections::HashMap;

use a2a::jsonrpc::{JsonRpcError, JsonRpcId};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::wire::{ControlFrame, WireError};

pub const JSONRPC_VERSION: &str = "2.0";

/// Shutdown request defined by this binding only; absent from `a2a::methods` (6.9).
pub const SYSTEM_SHUTDOWN: &str = "system/shutdown";

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// A JSON-RPC request carrying the optional `serviceParams` extension (3.2, 4.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StdioRequest {
    pub jsonrpc: String,
    pub id: JsonRpcId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
    /// 4.2 defines this as a flat map; multi-value params are comma separated (4.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_params: Option<HashMap<String, String>>,
}

impl StdioRequest {
    pub fn new(id: JsonRpcId, method: impl Into<String>, params: Option<Value>) -> Self {
        StdioRequest {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.into(),
            params,
            service_params: None,
        }
    }

    pub fn with_service_params(mut self, sp: &HashMap<String, Vec<String>>) -> Self {
        self.service_params = if sp.is_empty() {
            None
        } else {
            Some(service_params_to_wire(sp))
        };
        self
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientMessage {
    Request(StdioRequest),
    CancelStream { id: JsonRpcId },
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerMessage {
    /// A unary response and a stream chunk are byte-identical here; the reader
    /// tells them apart from the method it issued for this id (3.5).
    Result {
        id: JsonRpcId,
        result: Value,
    },
    Error {
        id: JsonRpcId,
        error: JsonRpcError,
    },
    StreamEnd {
        id: JsonRpcId,
    },
}

/// What a server may read off `STDIN`.
#[derive(Debug, Clone, PartialEq)]
pub enum ServerInbound {
    Control(ControlFrame),
    Message(ClientMessage),
}

/// What a client may read off the child's `STDOUT`.
#[derive(Debug, Clone, PartialEq)]
pub enum ClientInbound {
    Control(ControlFrame),
    Message(ServerMessage),
}

// ---------------------------------------------------------------------------
// Wire shapes
//
// One struct per distinguishable frame layout, used for both directions so that
// serde performs the member extraction instead of hand-indexing a `Value`.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResultFrame {
    jsonrpc: String,
    id: JsonRpcId,
    result: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorFrame {
    jsonrpc: String,
    id: JsonRpcId,
    error: JsonRpcError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StreamEndFrame {
    jsonrpc: String,
    id: JsonRpcId,
    stream_end: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CancelStreamFrame {
    jsonrpc: String,
    id: JsonRpcId,
    cancel_stream: bool,
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

impl ClientMessage {
    pub fn to_value(&self) -> Result<Value, serde_json::Error> {
        match self {
            ClientMessage::Request(req) => serde_json::to_value(req),
            ClientMessage::CancelStream { id } => serde_json::to_value(CancelStreamFrame {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: id.clone(),
                cancel_stream: true,
            }),
        }
    }

    pub fn to_vec(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self.to_value()?)
    }
}

impl ServerMessage {
    pub fn to_value(&self) -> Result<Value, serde_json::Error> {
        match self {
            ServerMessage::Result { id, result } => serde_json::to_value(ResultFrame {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: id.clone(),
                result: result.clone(),
            }),
            ServerMessage::Error { id, error } => serde_json::to_value(ErrorFrame {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: id.clone(),
                error: error.clone(),
            }),
            ServerMessage::StreamEnd { id } => serde_json::to_value(StreamEndFrame {
                jsonrpc: JSONRPC_VERSION.to_string(),
                id: id.clone(),
                stream_end: true,
            }),
        }
    }

    pub fn to_vec(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(&self.to_value()?)
    }
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

/// Cheap read-only classification of a frame body.
///
/// Everything captured here is owned, so the borrow of the parsed `Value` ends
/// when `Peek::new` returns and the caller stays free to move that `Value` into
/// `serde_json::from_value`.
struct Peek {
    is_control: bool,
    version: Option<String>,
    has_method: bool,
    has_result: bool,
    has_error: bool,
    cancel_stream: bool,
    stream_end: bool,
}

impl Peek {
    fn new(v: &Value) -> Result<Self, WireError> {
        let obj = v.as_object().ok_or(WireError::NotAnObject)?;
        Ok(Peek {
            is_control: obj.contains_key("type"),
            version: obj
                .get("jsonrpc")
                .and_then(Value::as_str)
                .map(str::to_owned),
            has_method: obj.contains_key("method"),
            has_result: obj.contains_key("result"),
            has_error: obj.contains_key("error"),
            cancel_stream: flag(obj, "cancelStream"),
            stream_end: flag(obj, "streamEnd"),
        })
    }

    fn require_jsonrpc_2(&self) -> Result<(), WireError> {
        if self.version.as_deref() == Some(JSONRPC_VERSION) {
            Ok(())
        } else {
            Err(WireError::BadVersion(
                self.version.clone().unwrap_or_default(),
            ))
        }
    }
}

fn flag(obj: &Map<String, Value>, key: &str) -> bool {
    obj.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Best-effort request id for a body whose shape was rejected. 3.4 reserves the
/// null id for frames whose id genuinely could not be determined.
pub fn peek_id(body: &[u8]) -> JsonRpcId {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return JsonRpcId::Null;
    };
    match v.get("id") {
        Some(id) => serde_json::from_value(id.clone()).unwrap_or(JsonRpcId::Null),
        None => JsonRpcId::Null,
    }
}

/// Classify a frame body sent by a client, i.e. what a server reads.
pub fn parse_from_client(body: &[u8]) -> Result<ServerInbound, WireError> {
    let v: Value = serde_json::from_slice(body)?;
    let peek = Peek::new(&v)?;

    // Control frames carry no `jsonrpc` member, so they must be routed before
    // the version check (2.2).
    if peek.is_control {
        return Ok(ServerInbound::Control(serde_json::from_value(v)?));
    }
    peek.require_jsonrpc_2()?;

    if peek.has_method {
        let req: StdioRequest = serde_json::from_value(v)?;
        return Ok(ServerInbound::Message(ClientMessage::Request(req)));
    }
    if peek.cancel_stream {
        let f: CancelStreamFrame = serde_json::from_value(v)?;
        return Ok(ServerInbound::Message(ClientMessage::CancelStream {
            id: f.id,
        }));
    }
    Err(WireError::UnrecognizedMessage)
}

/// Classify a frame body sent by a server, i.e. what a client reads.
pub fn parse_from_server(body: &[u8]) -> Result<ClientInbound, WireError> {
    let v: Value = serde_json::from_slice(body)?;
    let peek = Peek::new(&v)?;

    if peek.is_control {
        return Ok(ClientInbound::Control(serde_json::from_value(v)?));
    }
    peek.require_jsonrpc_2()?;

    // Error wins over result, and streamEnd over both: a peer that sends more
    // than one of them is read the conservative way.
    if peek.has_error {
        let f: ErrorFrame = serde_json::from_value(v)?;
        return Ok(ClientInbound::Message(ServerMessage::Error {
            id: f.id,
            error: f.error,
        }));
    }
    if peek.stream_end {
        let f: StreamEndFrame = serde_json::from_value(v)?;
        return Ok(ClientInbound::Message(ServerMessage::StreamEnd {
            id: f.id,
        }));
    }
    if peek.has_result {
        let f: ResultFrame = serde_json::from_value(v)?;
        return Ok(ClientInbound::Message(ServerMessage::Result {
            id: f.id,
            result: f.result,
        }));
    }
    Err(WireError::UnrecognizedMessage)
}

// ---------------------------------------------------------------------------
// Service parameters
//
// 4.2 carries them as a flat map in the body, while `ServiceParams` elsewhere
// in the SDK is multi-valued. 4.1 makes comma the separator.
// ---------------------------------------------------------------------------

pub fn wire_to_service_params(m: &HashMap<String, String>) -> HashMap<String, Vec<String>> {
    let mut result = HashMap::new();
    for (k, v) in m {
        let values: Vec<String> = v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        result.insert(k.to_ascii_lowercase(), values);
    }
    result
}

pub fn service_params_to_wire(sp: &HashMap<String, Vec<String>>) -> HashMap<String, String> {
    let mut result = HashMap::new();
    for (k, vs) in sp {
        result.insert(k.to_ascii_lowercase(), vs.join(","));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::{Handshake, Variant};
    use serde_json::json;

    fn bytes(v: Value) -> Vec<u8> {
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn unknown_members_are_ignored() {
        // 3.1: unrecognized extension members must not fail the message.
        let body = bytes(json!({
            "jsonrpc": "2.0",
            "id": "req-1",
            "method": "SendMessage",
            "futureExtension": {"anything": true},
        }));
        match parse_from_client(&body).unwrap() {
            ServerInbound::Message(ClientMessage::Request(r)) => {
                assert_eq!(r.method, "SendMessage");
            }
            other => panic!("expected request, got {other:?}"),
        }
    }

    #[test]
    fn absent_service_params_are_not_serialized() {
        let req = StdioRequest::new(JsonRpcId::Number(1), "GetTask", None);
        let s = serde_json::to_string(&req).unwrap();
        assert!(!s.contains("serviceParams"), "got {s}");
        assert!(!s.contains("params"), "got {s}");
    }

    #[test]
    fn service_params_use_camel_case_on_the_wire() {
        let mut sp = HashMap::new();
        sp.insert("a2a-version".to_string(), vec!["1.0".to_string()]);
        let req = StdioRequest::new(JsonRpcId::Number(1), "GetTask", None).with_service_params(&sp);
        let v = serde_json::to_value(&req).unwrap();
        assert_eq!(v["serviceParams"]["a2a-version"], json!("1.0"));
    }

    #[test]
    fn variant_uses_hyphenated_names() {
        assert_eq!(
            serde_json::to_value(Variant::Json).unwrap(),
            json!("stdio-json")
        );
        assert_eq!(
            serde_json::to_value(Variant::Proto).unwrap(),
            json!("stdio-proto")
        );
    }

    #[test]
    fn handshake_round_trips_with_camel_case_keys() {
        let hs = Handshake {
            protocol: "a2a-stdio".into(),
            protocol_version: "1.0".into(),
            session_id: "s-1".into(),
            supported_variants: vec![Variant::Json, Variant::Proto],
            features: vec!["streaming".into()],
            pid: Some(42137),
        };
        let v = serde_json::to_value(ControlFrame::Handshake(hs.clone())).unwrap();
        assert_eq!(v["type"], json!("handshake"));
        assert_eq!(v["protocolVersion"], json!("1.0"));
        assert_eq!(v["sessionId"], json!("s-1"));
        assert_eq!(v["supportedVariants"], json!(["stdio-json", "stdio-proto"]));

        let body = serde_json::to_vec(&v).unwrap();
        match parse_from_server(&body).unwrap() {
            ClientInbound::Control(ControlFrame::Handshake(back)) => assert_eq!(back, hs),
            other => panic!("expected handshake, got {other:?}"),
        }
    }

    #[test]
    fn stream_end_is_not_read_as_a_result() {
        let body = bytes(json!({"jsonrpc": "2.0", "id": "req-1", "streamEnd": true}));
        match parse_from_server(&body).unwrap() {
            ClientInbound::Message(ServerMessage::StreamEnd { id }) => {
                assert_eq!(id, JsonRpcId::String("req-1".into()));
            }
            other => panic!("expected streamEnd, got {other:?}"),
        }
    }

    #[test]
    fn error_wins_over_result() {
        let body = bytes(json!({
            "jsonrpc": "2.0",
            "id": 7,
            "result": {"ignored": true},
            "error": {"code": -32603, "message": "boom"},
        }));
        match parse_from_server(&body).unwrap() {
            ClientInbound::Message(ServerMessage::Error { error, .. }) => {
                assert_eq!(error.code, -32603);
            }
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn cancel_stream_round_trips() {
        let msg = ClientMessage::CancelStream {
            id: JsonRpcId::String("req-1".into()),
        };
        let v = msg.to_value().unwrap();
        assert_eq!(v["cancelStream"], json!(true));

        let body = serde_json::to_vec(&v).unwrap();
        match parse_from_client(&body).unwrap() {
            ServerInbound::Message(ClientMessage::CancelStream { id }) => {
                assert_eq!(id, JsonRpcId::String("req-1".into()));
            }
            other => panic!("expected cancelStream, got {other:?}"),
        }
    }

    #[test]
    fn empty_object_is_an_error_not_a_panic() {
        let err = parse_from_client(&bytes(json!({}))).unwrap_err();
        assert!(matches!(err, WireError::BadVersion(_)), "got {err:?}");
    }

    #[test]
    fn non_object_bodies_are_rejected() {
        assert!(matches!(
            parse_from_client(&bytes(json!([1, 2, 3]))).unwrap_err(),
            WireError::NotAnObject
        ));
    }

    #[test]
    fn unknown_control_frame_type_is_rejected() {
        let body = bytes(json!({"type": "somethingElse", "sessionId": "s-1"}));
        assert!(matches!(
            parse_from_client(&body).unwrap_err(),
            WireError::Json(_)
        ));
    }

    #[test]
    fn wrong_jsonrpc_version_is_rejected() {
        let body = bytes(json!({"jsonrpc": "1.0", "id": 1, "method": "GetTask"}));
        match parse_from_client(&body).unwrap_err() {
            WireError::BadVersion(v) => assert_eq!(v, "1.0"),
            other => panic!("expected BadVersion, got {other:?}"),
        }
    }

    #[test]
    fn well_formed_but_unclassifiable_message_is_rejected() {
        let body = bytes(json!({"jsonrpc": "2.0", "id": 1}));
        assert!(matches!(
            parse_from_client(&body).unwrap_err(),
            WireError::UnrecognizedMessage
        ));
    }

    #[test]
    fn service_params_split_and_join_round_trip() {
        let mut wire = HashMap::new();
        wire.insert("a2a-extensions".to_string(), "a, b ,c".to_string());
        let multi = wire_to_service_params(&wire);
        assert_eq!(multi["a2a-extensions"], vec!["a", "b", "c"]);
        assert_eq!(service_params_to_wire(&multi)["a2a-extensions"], "a,b,c");
    }

    #[test]
    fn service_param_keys_are_lowercased() {
        let mut wire = HashMap::new();
        wire.insert("A2A-Version".to_string(), "1.0".to_string());
        assert!(wire_to_service_params(&wire).contains_key("a2a-version"));
    }

    #[test]
    fn empty_service_param_values_are_dropped() {
        let mut wire = HashMap::new();
        wire.insert("k".to_string(), " , ,".to_string());
        assert!(wire_to_service_params(&wire)["k"].is_empty());
    }
}
