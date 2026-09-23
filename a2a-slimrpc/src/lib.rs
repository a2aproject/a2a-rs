// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
pub mod client;
mod common;
pub mod errors;
pub mod server;

pub use client::{SlimApp, SlimRpcTransport, SlimRpcTransportFactory, parse_slimrpc_target};
pub use common::SLIM_SRC_METADATA_KEY;
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
