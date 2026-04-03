// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
pub mod client;
pub mod errors;
pub mod server;

pub use client::{GrpcTransport, GrpcTransportFactory};
pub use server::GrpcHandler;
