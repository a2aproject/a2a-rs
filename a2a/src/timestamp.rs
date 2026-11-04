// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use chrono::{DateTime, SecondsFormat, Utc};

/// Format a timestamp the way it travels on the wire (spec §5.6.1).
///
/// RFC 3339 in UTC with the `Z` designator and no numeric offset, and 0, 3, 6
/// or 9 fractional digits as the value needs. chrono's own `to_rfc3339` writes
/// `+00:00` instead, so every place that puts a timestamp into a payload or a
/// query string formats it here.
pub fn format_timestamp(timestamp: &DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        rfc3339.parse().unwrap()
    }

    #[test]
    fn test_format_timestamp_uses_the_z_designator() {
        let formatted = format_timestamp(&at("2025-01-01T00:00:00+00:00"));
        assert_eq!(formatted, "2025-01-01T00:00:00Z");
    }

    #[test]
    fn test_format_timestamp_keeps_only_the_digits_the_value_needs() {
        for (input, expected) in [
            ("2025-01-01T00:00:00Z", "2025-01-01T00:00:00Z"),
            ("2025-01-01T00:00:00.5Z", "2025-01-01T00:00:00.500Z"),
            ("2025-01-01T00:00:00.123Z", "2025-01-01T00:00:00.123Z"),
            ("2025-01-01T00:00:00.000123Z", "2025-01-01T00:00:00.000123Z"),
            (
                "2025-01-01T00:00:00.000000123Z",
                "2025-01-01T00:00:00.000000123Z",
            ),
        ] {
            assert_eq!(format_timestamp(&at(input)), expected, "{input}");
        }
    }

    #[test]
    fn test_format_timestamp_round_trips() {
        let now = Utc::now();
        assert_eq!(at(&format_timestamp(&now)), now);
    }
}
