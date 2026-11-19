// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use std::{fmt, str::FromStr, time::SystemTime};

use jiff::{SignedDuration, civil, fmt::temporal::Pieces};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

const UNIX_EPOCH: civil::DateTime = civil::datetime(1970, 1, 1, 0, 0, 0, 0);

/// A UTC instant in the range of `google.protobuf.Timestamp` (years 1–9999).
///
/// Jiff's civil calendar covers this entire range. Its zoned instant range
/// reserves room for timezone offsets at the upper end of year 9999.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp(SignedDuration);

impl Timestamp {
    const MIN_SECONDS: i64 = -62_135_596_800;
    const MAX_SECONDS: i64 = 253_402_300_799;

    fn checked(elapsed: SignedDuration) -> Option<Self> {
        let bounds = SignedDuration::from_secs(Self::MIN_SECONDS)
            ..=SignedDuration::new(Self::MAX_SECONDS, 999_999_999);
        bounds.contains(&elapsed).then_some(Self(elapsed))
    }

    /// Construct an instant from floor Unix seconds and nanoseconds within that second.
    pub fn from_unix_parts(seconds: i64, nanos: u32) -> Option<Self> {
        if nanos >= 1_000_000_000 {
            return None;
        }
        Self::checked(SignedDuration::new(seconds, nanos as i32))
    }

    /// Read the current UTC instant from the system clock.
    pub fn now() -> Self {
        SignedDuration::system_until(SystemTime::UNIX_EPOCH, SystemTime::now())
            .ok()
            .and_then(Self::checked)
            .expect("system clock outside the protobuf Timestamp range")
    }

    /// Whole seconds since the Unix epoch, rounded down for pre-epoch instants.
    pub fn unix_seconds(self) -> i64 {
        self.0.as_nanos().div_euclid(1_000_000_000) as i64
    }

    /// Nanoseconds elapsed since the start of the second.
    pub fn subsec_nanoseconds(self) -> u32 {
        self.0.subsec_nanos().rem_euclid(1_000_000_000) as u32
    }
}

/// An invalid wire timestamp or an instant outside the protobuf range.
#[derive(Debug, thiserror::Error)]
pub enum ParseTimestampError {
    #[error("expected an RFC 3339 date, time and UTC offset")]
    Syntax,
    #[error(transparent)]
    Invalid(#[from] jiff::Error),
    #[error("protobuf timestamps use smeared time and cannot encode a leap second")]
    LeapSecond,
    #[error("instant outside the protobuf Timestamp range")]
    OutOfRange,
}

impl FromStr for Timestamp {
    type Err = ParseTimestampError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        // Pieces also accepts basic ISO dates, reduced time precision and
        // timezone annotations. Require the RFC 3339 shape before decoding.
        let (clock, tail) = input
            .split_at_checked(19)
            .ok_or(ParseTimestampError::Syntax)?;
        let bytes = clock.as_bytes();
        if bytes[4] != b'-'
            || bytes[7] != b'-'
            || !matches!(bytes[10], b'T' | b't' | b' ')
            || bytes[13] != b':'
            || bytes[16] != b':'
        {
            return Err(ParseTimestampError::Syntax);
        }
        if clock.ends_with(":60") {
            return Err(ParseTimestampError::LeapSecond);
        }
        let zone_at = tail
            .find(['Z', 'z', '+', '-'])
            .ok_or(ParseTimestampError::Syntax)?;
        let (fraction, zone) = tail.split_at(zone_at);
        if !matches!(
            zone.as_bytes(),
            [b'Z' | b'z'] | [b'+' | b'-', _, _, b':', _, _]
        ) {
            return Err(ParseTimestampError::Syntax);
        }
        if !fraction.is_empty()
            && !fraction.strip_prefix('.').is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
            })
        {
            return Err(ParseTimestampError::Syntax);
        }
        // RFC 3339 accepts arbitrary fractional precision. Preserve the first
        // nine digits, as the previous parser did for nanosecond timestamps.
        let shortened;
        let input = if fraction.len() > 10 {
            shortened = format!("{clock}{}{zone}", &fraction[..10]);
            &shortened
        } else {
            input
        };
        let pieces = Pieces::parse(input)?;
        let time = pieces.time().ok_or(ParseTimestampError::Syntax)?;
        let offset = pieces
            .to_numeric_offset()
            .ok_or(ParseTimestampError::Syntax)?;
        if offset.seconds().unsigned_abs() >= 86_400 {
            return Err(ParseTimestampError::Syntax);
        }
        let local = pieces.date().to_datetime(time).duration_since(UNIX_EPOCH);
        Self::checked(local - SignedDuration::from_secs(i64::from(offset.seconds())))
            .ok_or(ParseTimestampError::OutOfRange)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let utc = UNIX_EPOCH.checked_add(self.0).map_err(|_| fmt::Error)?;
        let nanos = self.subsec_nanoseconds();
        let precision = if nanos == 0 {
            0
        } else if nanos % 1_000_000 == 0 {
            3
        } else if nanos % 1_000 == 0 {
            6
        } else {
            9
        };
        write!(f, "{utc:.precision$}Z")
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[cfg(feature = "fuzzing")]
impl<'a> arbitrary::Arbitrary<'a> for Timestamp {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        let seconds = u.int_in_range(Self::MIN_SECONDS..=Self::MAX_SECONDS)?;
        let nanos = u.int_in_range(0..=999_999_999)?;
        Ok(Self::from_unix_parts(seconds, nanos).expect("generated a valid timestamp"))
    }
}

/// Format a timestamp for wire payloads and query strings (spec §5.6.1).
pub fn format_timestamp(timestamp: &Timestamp) -> String {
    timestamp.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(rfc3339: &str) -> Timestamp {
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
    fn json_preserves_instants_across_offsets_and_before_the_epoch() {
        for (wire, seconds, nanos, expected) in [
            (
                "0001-01-01T00:00:00Z",
                Timestamp::MIN_SECONDS,
                0,
                "0001-01-01T00:00:00Z",
            ),
            (
                "1970-01-01T00:59:59.5+01:00",
                -1,
                500_000_000,
                "1969-12-31T23:59:59.500Z",
            ),
            (
                "2025-01-01T00:30:00.123456+01:00",
                1_735_687_800,
                123_456_000,
                "2024-12-31T23:30:00.123456Z",
            ),
            (
                "9999-12-31T23:59:59.999999999Z",
                Timestamp::MAX_SECONDS,
                999_999_999,
                "9999-12-31T23:59:59.999999999Z",
            ),
        ] {
            let timestamp: Timestamp = serde_json::from_value(serde_json::json!(wire)).unwrap();
            assert_eq!(timestamp.unix_seconds(), seconds);
            assert_eq!(timestamp.subsec_nanoseconds(), nanos);
            assert_eq!(Timestamp::from_unix_parts(seconds, nanos), Some(timestamp));
            assert_eq!(serde_json::to_value(timestamp).unwrap(), expected);
        }
    }

    #[test]
    fn a_local_time_can_cross_into_the_last_utc_day() {
        let last = at("9999-12-31T23:59:59.999999999Z");
        assert_eq!(at("9999-12-31t20:59:59.9999999999-03:00"), last);
        assert_eq!(
            at("0001-01-01T01:00:00+01:00").unix_seconds(),
            Timestamp::MIN_SECONDS
        );
    }

    #[test]
    fn invalid_dates_and_out_of_range_instants_are_rejected() {
        for input in [
            "2025-02-29T00:00:00Z",
            "2000-01-01T00:00Z",
            "2000-01-01T00:00:00",
            "2000-01-01T00:00:00.Z",
            "2000-01-01T00:00:00,5Z",
            "2000-01-01T00:00:00.123456789xZ",
            "2000-01-01T00:00:00Z[UTC]",
            "2000-01-01T00:00:00+24:00",
            "2016-12-31T23:59:60Z",
            "0000-12-31T23:59:59Z",
            "0001-01-01T00:00:00+01:00",
            "9999-12-31T23:59:59-01:00",
        ] {
            assert!(input.parse::<Timestamp>().is_err(), "{input}");
        }
        assert!(Timestamp::from_unix_parts(-1, 1_000_000_000).is_none());
    }

    #[test]
    fn test_format_timestamp_round_trips() {
        let now = Timestamp::now();
        assert_eq!(at(&format_timestamp(&now)), now);
    }
}
