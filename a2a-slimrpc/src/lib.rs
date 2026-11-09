// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! SLIMRPC binding for A2A v1, for clients and servers.
//!
//! Published as `a2a-slimrpc` and imported as `a2a_slimrpc`. It carries the
//! A2A service over SLIM RPC, using the generated messages in [`a2a_pb`].
//!
//! - **Client:** [`SlimRpcTransport`] implements [`a2a_client::Transport`].
//!   Register [`SlimRpcTransportFactory`] with an
//!   [`A2AClientFactory`](a2a_client::A2AClientFactory) and it can pick
//!   SLIMRPC for an agent card that advertises it. The interface URL is read
//!   as a SLIM peer name, `org/namespace/app`, optionally prefixed with
//!   `slim://` or `slimrpc://`; see [`parse_slimrpc_target`].
//! - **Server:** [`SlimRpcHandler`] registers the methods of an
//!   [`a2a_server::RequestHandler`] on a `slim_rpc::Server`.
//!   [`register_collaborate`] adds a listen-only participant for the
//!   collaborative channel, which is not a request/response operation.
//! - **Errors:** [`errors`] converts between [`a2a::A2AError`] and SLIM RPC
//!   errors.
//!
//! # Caller identity
//!
//! The SLIMRPC layer passes the caller's identity to the handler in the
//! service params, under [`SLIM_SRC_METADATA_KEY`],
//! [`SLIM_PEER_SUBJECT_METADATA_KEY`] and [`SLIM_PEER_ISSUER_METADATA_KEY`].
//! Values a caller sends under these keys are dropped. Authorize on the peer
//! subject, which the session verifies; the source is the name the caller
//! claimed.

pub mod client;
mod common;
pub mod errors;
pub mod server;

pub use client::{SlimApp, SlimRpcTransport, SlimRpcTransportFactory, parse_slimrpc_target};
pub use common::{
    SLIM_PEER_ISSUER_METADATA_KEY, SLIM_PEER_SUBJECT_METADATA_KEY, SLIM_SRC_METADATA_KEY,
};
pub use server::{SlimRpcHandler, register_collaborate};

// Wire internals reached by the fuzz target in fuzz/ — see #238. Thin
// wrappers, not re-exports: the originals stay crate-private.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzz_support {
    use a2a::A2AError;
    use prost::Message;

    pub fn encode_proto_message<T: Message>(message: &T) -> Vec<u8> {
        crate::common::encode_proto_message(message)
    }

    pub fn decode_proto_response<T: Message + Default>(
        bytes: Vec<u8>,
        type_name: &str,
    ) -> Result<T, A2AError> {
        crate::common::decode_proto_response(bytes, type_name)
    }
}
