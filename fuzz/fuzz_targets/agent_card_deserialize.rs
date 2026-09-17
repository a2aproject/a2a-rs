// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Raw bytes into `a2a::AgentCard`, the way every client reads one off the
//! network -- unauthenticated, at `/.well-known/agent-card.json`, before any
//! other check runs. See #238 target 3.
//!
//! Oracle: no panic, and a card that parses is stable under one more
//! serialize/deserialize round trip. Unlike the protojson conversion in
//! `protojson_roundtrip`, this is the *same* `Serialize`/`Deserialize` pair
//! both ways, so exact identity is the right assertion rather than
//! idempotence -- there is no lossy proto3-presence step in between.

#![no_main]

use a2a::AgentCard;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(card) = serde_json::from_slice::<AgentCard>(data) else {
        return;
    };
    let text = serde_json::to_string(&card)
        .unwrap_or_else(|e| panic!("re-serializing a card this crate just parsed failed: {e}"));
    let reparsed: AgentCard = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("re-parsing this crate's own output failed: {e}\n{text}"));
    assert_eq!(
        card, reparsed,
        "AgentCard is not stable under a second serde round trip"
    );
});
