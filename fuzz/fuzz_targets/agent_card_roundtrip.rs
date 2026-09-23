// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! `arbitrary`-generated `AgentCard` values, complementing
//! `agent_card_deserialize`'s raw bytes with the "structured values" half
//! of #238 target 3: bytes that already look like a plausible card explore
//! its field combinations far more efficiently than mutating raw JSON text
//! ever finds its way past the outermost `{`.
//!
//! Oracle: serializing and reparsing reproduces the same value exactly --
//! the same identity property `agent_card_deserialize` checks, generated
//! from the other direction.

#![no_main]

use a2a::AgentCard;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|card: AgentCard| {
    let text = serde_json::to_string(&card)
        .unwrap_or_else(|e| panic!("serializing an arbitrary-generated card failed: {e}"));
    let reparsed: AgentCard = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("reparsing this crate's own output failed: {e}\n{text}"));
    assert_eq!(
        card, reparsed,
        "AgentCard did not survive a serde round trip unchanged"
    );
});
