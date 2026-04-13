// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use thiserror::Error;

/// Errors specific to the STDIO transport.
#[derive(Debug, Error)]
pub enum StdioError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("invalid frame header: {0}")]
    InvalidHeader(String),

    #[error("handshake failed: {0}")]
    HandshakeFailed(String),

    #[error("transport closed")]
    Closed,
}
