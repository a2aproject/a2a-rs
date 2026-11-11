// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! SSE framing helpers and an oracle testable without linking libFuzzer.

use a2a::{A2AError, StreamResponse};

pub fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    crate::jsonrpc::find_event_boundary(buf)
}

pub fn parse_stream_tail(
    buf: &[u8],
    parse_event: &dyn Fn(&str) -> Option<Result<StreamResponse, A2AError>>,
) -> Option<Result<StreamResponse, A2AError>> {
    crate::jsonrpc::parse_stream_tail(buf, parse_event)
}

pub fn check_framing(data: &[u8]) {
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

    // Oracle 2: adding an SSE boundary cannot turn an RPC error into a
    // successful empty stream. This compares independent framed/tail paths,
    // without duplicating the implementation's future-payload skip rule.
    let tail = parse_stream_tail(data, &crate::jsonrpc::parse_jsonrpc_sse_event);
    if let Ok(text) = std::str::from_utf8(data) {
        if !text.trim().is_empty() && !text.lines().any(|line| line.starts_with("data:")) {
            if let Ok(envelope) = serde_json::from_str::<a2a::JsonRpcResponse>(text) {
                // An RPC failure, or an absent/null result with no failure,
                // cannot represent an ignorable future event.
                if envelope.jsonrpc != "2.0"
                    || envelope.error.is_some()
                    || envelope.result.is_none()
                {
                    assert!(
                        tail.as_ref().is_some_and(Result::is_err),
                        "malformed RPC envelope was ignored"
                    );
                }
                // Encode every SSE line, including bare-CR JSON whitespace.
                let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
                let framed = normalized
                    .lines()
                    .map(|line| format!("data:{line}\n"))
                    .collect::<String>()
                    + "\n";
                let events = futures::executor::block_on(async {
                    use futures::StreamExt;
                    crate::jsonrpc::parse_sse_stream(futures::stream::iter([Ok(
                        bytes::Bytes::from(framed),
                    )]))
                    .collect::<Vec<_>>()
                    .await
                });
                match tail {
                    None => assert!(events.is_empty(), "framing changed an ignored RPC payload"),
                    Some(Ok(expected)) => {
                        assert_eq!(events.len(), 1);
                        assert_eq!(events.into_iter().next().unwrap().unwrap(), expected);
                    }
                    Some(Err(_)) => {
                        assert_eq!(
                            events.len(),
                            1,
                            "framing discarded a malformed RPC envelope"
                        );
                        assert!(
                            events[0].is_err(),
                            "framing accepted a malformed RPC envelope"
                        );
                    }
                }
            } else {
                assert!(tail.is_some(), "nonempty malformed tail silently dropped");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn oracle_preserves_errors_for_known_null_payloads_and_empty_envelopes() {
        for payload in [
            json!({"task":null}),
            json!({"statusUpdate":null}),
            json!({"messageUpdate":null}),
            json!({"message_update":null}),
            json!({"error":null}),
            json!({"task":null,"futureEvent":{}}),
            json!({}),
            json!(null),
        ] {
            let envelope = json!({"jsonrpc":"2.0","id":1,"result":payload}).to_string();
            check_framing(envelope.as_bytes());
            assert!(
                parse_stream_tail(
                    envelope.as_bytes(),
                    &crate::jsonrpc::parse_jsonrpc_sse_event
                )
                .unwrap()
                .is_err()
            );
        }
        check_framing(br#"{"jsonrpc":"2.0","id":1}"#);
    }

    #[test]
    fn oracle_allows_future_payloads_without_a_copy_of_the_decoder_rules() {
        for payload in [json!({"futureEvent":{}}), json!({"futureOther":null})] {
            check_framing(
                json!({"jsonrpc":"2.0","id":1,"result":payload})
                    .to_string()
                    .as_bytes(),
            );
        }
    }

    #[test]
    fn oracle_encodes_json_whitespace_without_creating_sse_boundaries() {
        for newline in ["\n", "\r", "\r\n"] {
            let envelope = format!(
                "{{\"jsonrpc\":\"2.0\",{newline}{newline}\"id\":1,\"result\":{{\"futureEvent\":{{}}}}}}"
            );
            assert!(serde_json::from_str::<a2a::JsonRpcResponse>(&envelope).is_ok());
            check_framing(envelope.as_bytes());
        }
    }

    #[test]
    fn framing_oracle_accepts_the_regression_corpus() {
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../fuzz/seeds/sse_framing");
        let mut count = 0;
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                check_framing(&std::fs::read(entry.path()).unwrap());
                count += 1;
            }
        }
        assert!(
            count >= 16,
            "the complete framing regression corpus must be present"
        );
    }
}
