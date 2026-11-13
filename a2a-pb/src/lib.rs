// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Protobuf types for A2A v1, and conversions to the native types of the
//! [`a2a`] crate.
//!
//! Published as `a2a-pb` and imported as `a2a_pb`.
//!
//! - [`proto`]: types generated from `proto/a2a.proto` with prost and tonic,
//!   including the gRPC service. They are also re-exported at the crate root.
//! - [`protojson`]: the same messages, with serde implementations that follow
//!   the ProtoJSON mapping.
//! - [`pbconv`]: functions that convert between the native types and
//!   [`proto`]. The gRPC and SLIMRPC bindings use them.
//! - [`protojson_conv`]: conversion between the native types and ProtoJSON
//!   values.

/// Generated protobuf types for the A2A v1 protocol.
pub mod proto {
    // The generated `oneof` enums wrap whole messages (`Task`, `SendMessageRequest`),
    // which differ widely in size.
    #![allow(clippy::large_enum_variant)]

    include!(concat!(env!("OUT_DIR"), "/grpc/lf.a2a.v1.rs"));
}

/// ProtoJSON-capable generated protobuf types for the A2A v1 protocol.
pub mod protojson {
    // Generated serde code emits `write!(f, "...", &FIELDS)`, which newer
    // clippy flags as a useless borrow. Suppress it for the generated module.
    // `unknown_lints` keeps this compiling on clippy versions predating the lint.
    #![allow(unknown_lints)]
    #![allow(clippy::useless_borrows_in_formatting)]
    #![allow(clippy::large_enum_variant)]

    include!(concat!(env!("OUT_DIR"), "/lf.a2a.v1.rs"));
    include!(concat!(env!("OUT_DIR"), "/lf.a2a.v1.serde.rs"));
}

pub mod pbconv;
pub mod protojson_conv;
mod timestamp;

pub use proto::*;
