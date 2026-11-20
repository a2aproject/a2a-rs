// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
#![doc = include_str!("../README.md")]

mod check;
mod translate;

pub use check::require_collaborative_extensions;
pub use translate::translate_peer_response;
