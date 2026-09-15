// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! SSE framing over arbitrary bytes. See #238; the bug class is #198.

#![no_main]

use a2a_client::fuzz_support::{find_event_boundary, parse_stream_tail};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Oracle 1: the boundary the caller slices on must be in range and
    // ordered. `parse_sse_bytes` does `buf.drain(..end)` then
    // `&event_bytes[..start]`, so a violation here is a panic there.
    if let Some((start, end)) = find_event_boundary(data) {
        assert!(start <= end, "start {start} > end {end}");
        assert!(end <= data.len(), "end {end} past len {}", data.len());
        let delimiter = &data[start..end];
        assert!(
            delimiter == b"\n\n" || delimiter == b"\r\r" || delimiter == b"\r\n\r\n",
            "framed on {delimiter:?}, which is not an SSE event delimiter"
        );
        // The event body must be decodable or explicitly rejected, never
        // sliced out of bounds.
        let _ = std::str::from_utf8(&data[..start]);
    }

    // Oracle 2: the #198 property — a tail carrying data must never be
    // silently dropped. Returning None here means the bytes are discarded
    // and the caller never learns the stream was cut mid-message.
    let parse_event = |_text: &str| None;
    let tail = parse_stream_tail(data, &parse_event);
    if let Ok(text) = std::str::from_utf8(data) {
        if !text.trim().is_empty() && !text.lines().any(|l| l.starts_with("data:")) {
            assert!(
                tail.is_some(),
                "non-empty tail silently dropped: {text:?}"
            );
        }
    }
});
