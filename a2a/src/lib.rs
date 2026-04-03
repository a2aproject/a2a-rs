// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
pub mod agent_card;
pub mod errors;
pub mod event;
pub mod jsonrpc;
pub mod types;

pub use agent_card::*;
pub use errors::*;
pub use event::*;
pub use jsonrpc::*;
pub use types::*;

/// The A2A protocol version this SDK implements.
pub const VERSION: &str = "1.0";

/// Service parameter key for the A2A protocol version.
pub const SVC_PARAM_VERSION: &str = "A2A-Version";

/// Service parameter key for extensions the client wants to use.
pub const SVC_PARAM_EXTENSIONS: &str = "A2A-Extensions";
