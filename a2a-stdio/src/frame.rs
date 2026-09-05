// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

// Closed set per 3.1 and 15.2. Unknown headers are dropped (RFC 7230).
use bytes::Bytes;
use std::collections::HashMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct FrameHeaders {
    pub content_type: Option<String>,
    pub a2a_kind: Option<String>, // request|response|error|stream|streamEnd|cancel
    pub a2a_id: Option<String>,
    pub a2a_method: Option<String>,
    pub service_params: HashMap<String, Vec<String>>, // from A2A-SP-*
}

#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub headers: FrameHeaders,
    pub body: Bytes, // exactly Content-Length bytes; may be empty
}

pub enum State {
    Headers,
    Body {
        content_length: usize,
        headers: FrameHeaders,
    },
}
