// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
#![doc = include_str!("../README.md")]

pub mod agent_card;
pub mod errordetails;
pub mod errors;
pub mod event;
pub mod jsonrpc;
pub mod timestamp;
pub mod types;

pub use agent_card::*;
pub use errordetails::*;
pub use errors::*;
pub use event::*;
pub use jsonrpc::*;
pub use timestamp::*;
pub use types::*;

/// The A2A protocol version this SDK implements.
pub const VERSION: &str = "1.0";

/// Service parameter key for the A2A protocol version.
pub const SVC_PARAM_VERSION: &str = "A2A-Version";

/// Service parameter key for extensions the client wants to use.
pub const SVC_PARAM_EXTENSIONS: &str = "A2A-Extensions";

/// Extension URI for the shared-task extension (§4.1 of the collaborative task spec).
pub const EXTENSION_URI_SHARED_TASK: &str = "https://a2a-protocol.org/extensions/shared-task/v1";

/// Extension URI for the collaborative-task extension.
pub const EXTENSION_URI_COLLABORATIVE_TASK: &str =
    "https://a2a-protocol.org/extensions/collaborative-task/v1";

/// Extension URI for the experimental SLIMRPC binding of the collaborative-task extension.
pub const EXTENSION_URI_SLIMRPC_COLLABORATIVE_TASK: &str =
    "https://a2a-protocol.org/bindings/experimental-slimrpc/extensions/collaborative-task/v1";
