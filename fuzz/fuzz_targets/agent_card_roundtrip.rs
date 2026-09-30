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

    // Not an AgentCard bug: this exact string is serde_json's own internal
    // sentinel key for RawValue, reserved process-wide the moment any
    // dependency enables the "raw_value" feature (as pbjson-types does
    // here). A JSON object with this literal key, anywhere in the document,
    // makes serde_json's parser try to read it as a raw-value wrapper (a
    // string) instead of whatever type actually owns that key, failing with
    // "invalid type: ..., expected raw value" -- independent of AgentCard's
    // own (de)serialization logic, and reproducible with a bare
    // `serde_json::Value` on any object shaped like it. `arbitrary` has no
    // way to know this string is off limits, so skip it here instead.
    if text.contains("$serde_json::private::RawValue") {
        return;
    }

    let reparsed: AgentCard = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("reparsing this crate's own output failed: {e}\n{text}"));
    assert_eq!(
        card, reparsed,
        "AgentCard did not survive a serde round trip unchanged"
    );
});
