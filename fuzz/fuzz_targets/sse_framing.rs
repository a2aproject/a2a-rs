// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! SSE framing over arbitrary bytes. See #238; the bug class is #198.

#![cfg_attr(not(test), no_main)]

use a2a_client::fuzz_support::{find_event_boundary, parse_stream_tail};
#[cfg(not(test))]
use libfuzzer_sys::fuzz_target;

fn skipped_unknown_result(text: &str) -> bool {
    let Ok(response) = serde_json::from_str::<a2a::JsonRpcResponse>(text.trim()) else {
        return false;
    };
    if response.error.is_some() {
        return false;
    }
    let Some(serde_json::Value::Object(fields)) = response.result else {
        return false;
    };
    // Presence of a known field must never excuse a dropped event, even when
    // its payload is null. Only entirely unknown payload kinds are skippable.
    !fields.is_empty()
        && fields.keys().all(|field| {
            !matches!(
                field.as_str(),
                "task"
                    | "message"
                    | "statusUpdate"
                    | "status_update"
                    | "artifactUpdate"
                    | "artifact_update"
            )
        })
}

fn check_framing(data: &[u8]) {
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
        // A well-formed JSON-RPC success whose result carries only an event
        // kind this client does not know is skipped on purpose (stream readers
        // MUST ignore unrecognised payloads); everything else must surface.
        if !text.trim().is_empty()
            && !text.lines().any(|l| l.starts_with("data:"))
            && !skipped_unknown_result(text)
        {
            assert!(tail.is_some(), "non-empty tail silently dropped: {text:?}");
        }
    }
}

#[cfg(not(test))]
fuzz_target!(|data: &[u8]| check_framing(data));

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn known_payloads_never_qualify_for_the_unknown_event_exemption() {
        for field in [
            "task",
            "message",
            "statusUpdate",
            "status_update",
            "artifactUpdate",
            "artifact_update",
        ] {
            for value in [json!(null), json!({}), json!(false)] {
                for extra_unknown in [false, true] {
                    let mut result = json!({field: value});
                    if extra_unknown {
                        result["futureEvent"] = json!({});
                    }
                    let envelope = json!({"jsonrpc":"2.0", "id":1, "result":result}).to_string();
                    assert!(!skipped_unknown_result(&envelope), "{envelope}");
                }
            }
        }
    }

    #[test]
    fn only_well_formed_unknown_result_envelopes_are_exempt() {
        for value in [json!(null), json!({}), json!(42)] {
            for null_error in [false, true] {
                let mut envelope = json!({"jsonrpc":"2.0", "id":1, "result":{"futureEvent":value}});
                if null_error {
                    envelope["error"] = json!(null);
                }
                assert!(skipped_unknown_result(&envelope.to_string()), "{envelope}");
            }
        }
        for envelope in [
            json!({"result":{"futureEvent":{}}}),
            json!({"jsonrpc":"2.0", "id":1, "result":{}}),
            json!({"jsonrpc":"2.0", "id":1, "result":null}),
            json!({"jsonrpc":"2.0", "id":1, "result":[]}),
            json!({"jsonrpc":"2.0", "id":1, "result":{"futureEvent":{}}, "error":{"code":-1,"message":"failed"}}),
        ] {
            assert!(!skipped_unknown_result(&envelope.to_string()), "{envelope}");
        }
    }

    #[test]
    fn framing_oracle_accepts_the_regression_corpus() {
        for seed in [
            include_bytes!("../seeds/sse_framing/known-null-task").as_slice(),
            include_bytes!("../seeds/sse_framing/known-null-status"),
            include_bytes!("../seeds/sse_framing/known-null-with-unknown"),
            include_bytes!("../seeds/sse_framing/unknown-result"),
            include_bytes!("../seeds/sse_framing/unknown-result-null-error"),
            include_bytes!("../seeds/sse_framing/empty-result"),
            include_bytes!("../seeds/sse_framing/known-task"),
            include_bytes!("../seeds/sse_framing/invalid-envelope"),
        ] {
            check_framing(seed);
        }
    }
}
