// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use chrono::{DateTime, Utc};
use pbjson_types::Timestamp;
use serde::{Serialize, Serializer};

/// A `google.protobuf.Timestamp` as it travels in ProtoJSON (spec §5.6.1):
/// UTC with the `Z` designator.
///
/// `pbjson_types::Timestamp` serializes through chrono's `to_rfc3339`, which
/// ends in `+00:00`. `build.rs` routes every `Timestamp` field of the generated
/// ProtoJSON types through this wrapper instead, so the value is right where
/// it is written and nothing has to be fixed up in the JSON afterwards.
pub(crate) struct WireTimestamp<'a>(pub(crate) &'a Timestamp);

impl Serialize for WireTimestamp<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let timestamp: DateTime<Utc> = (*self.0).try_into().map_err(serde::ser::Error::custom)?;
        serializer.serialize_str(&a2a::format_timestamp(&timestamp))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(seconds: i64, nanos: i32) -> String {
        let timestamp = Timestamp { seconds, nanos };
        serde_json::to_string(&WireTimestamp(&timestamp)).unwrap()
    }

    #[test]
    fn test_wire_timestamp_uses_the_z_designator() {
        assert_eq!(render(1_700_000_000, 0), "\"2023-11-14T22:13:20Z\"");
    }

    #[test]
    fn test_wire_timestamp_keeps_only_the_digits_the_value_needs() {
        assert_eq!(
            render(1_700_000_000, 500_000_000),
            "\"2023-11-14T22:13:20.500Z\""
        );
        assert_eq!(
            render(1_700_000_000, 1_000),
            "\"2023-11-14T22:13:20.000001Z\""
        );
        assert_eq!(
            render(1_700_000_000, 1),
            "\"2023-11-14T22:13:20.000000001Z\""
        );
    }

    #[test]
    fn test_wire_timestamp_rejects_an_out_of_range_value() {
        let timestamp = Timestamp {
            seconds: i64::MAX,
            nanos: 0,
        };
        assert!(serde_json::to_string(&WireTimestamp(&timestamp)).is_err());
    }
}
