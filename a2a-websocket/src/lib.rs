// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
#![doc = include_str!("../README.md")]

pub mod client;
pub mod common;
pub mod errors;
pub mod server;

pub use client::{WebSocketTransport, WebSocketTransportFactory};
pub use common::{
    SUBPROTOCOL, TRANSPORT_PROTOCOL_WEBSOCKET, WsErrorObject, WsRequestEnvelope,
    WsResponseEnvelope, close_codes, error_types, methods,
};
pub use server::websocket_router;
