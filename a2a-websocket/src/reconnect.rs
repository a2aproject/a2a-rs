//! Reconnection backoff (spec Section 8.4).
//!
//! Section 8.4 asks clients to re-establish an interrupted connection, to
//! resubscribe to in-progress tasks, and to tolerate the duplicate events that
//! overlap can produce. This module covers the first part — when to try again —
//! because it is purely mechanical.
//!
//! Resubscribing is left to the application. Only it knows which tasks still
//! matter and whether duplicate events are safe to apply twice, and a transport
//! that guessed would either resubscribe to tasks nobody is watching or
//! silently replay events into application state.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The schedule recommended by spec Section 8.4: one second, doubling, capped at
/// thirty.
pub const DEFAULT_BACKOFF: Backoff = Backoff {
    initial: Duration::from_secs(1),
    max: Duration::from_secs(30),
    multiplier: 2,
    jitter: 0.5,
    max_attempts: Some(10),
};

/// An exponential backoff schedule with jitter (spec Section 8.4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Backoff {
    /// Delay before the first retry.
    pub initial: Duration,
    /// Ceiling for the delay, however many attempts have failed.
    pub max: Duration,
    /// Factor the delay grows by after each failure.
    pub multiplier: u32,
    /// Fraction of the delay left to chance, in `0.0..=1.0`. At `0.5` a delay
    /// falls somewhere between half and all of the schedule's value, so a fleet
    /// of clients that dropped together does not return in lockstep. `0.0`
    /// disables jitter, which makes tests reproducible but is not what the spec
    /// asks for in production.
    pub jitter: f64,
    /// Give up after this many attempts. `None` retries indefinitely.
    pub max_attempts: Option<u32>,
}

impl Default for Backoff {
    fn default() -> Self {
        DEFAULT_BACKOFF
    }
}

impl Backoff {
    /// The delay to wait before `attempt`, counting the first retry as 1.
    ///
    /// Grows geometrically from `initial`, saturates at `max`, and then has
    /// jitter subtracted.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        let base = self.base_delay(attempt);
        if self.jitter <= 0.0 {
            return base;
        }
        let fraction = self.jitter.clamp(0.0, 1.0);
        // Scale down by a random slice of `fraction`, so the result lands in
        // `[base * (1 - fraction), base]`.
        let scale = 1.0 - fraction * random_unit();
        base.mul_f64(scale)
    }

    /// The un-jittered delay for `attempt`.
    fn base_delay(&self, attempt: u32) -> Duration {
        let steps = attempt.saturating_sub(1);
        let multiplier = u64::from(self.multiplier.max(1));
        // Bail out to the ceiling rather than overflowing on a long outage.
        let mut delay = self.initial;
        for _ in 0..steps {
            match delay.checked_mul(multiplier as u32) {
                Some(next) if next < self.max => delay = next,
                _ => return self.max,
            }
        }
        delay.min(self.max)
    }

    /// Whether `attempt` (1-based) is still within the allowance.
    pub fn allows(&self, attempt: u32) -> bool {
        match self.max_attempts {
            Some(max) => attempt <= max,
            None => true,
        }
    }
}

/// A pseudo-random number in `[0, 1)`, taken from the clock.
///
/// Spreading retries does not need unpredictability, only variety, so this
/// avoids pulling in a random number generator for it. Deliberately not used
/// for anything security-sensitive.
fn random_unit() -> f64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.subsec_nanos())
        .unwrap_or(0);
    f64::from(nanos % 1_000_000) / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Jitter off, so the schedule itself can be asserted.
    fn exact(initial_ms: u64, max_ms: u64) -> Backoff {
        Backoff {
            initial: Duration::from_millis(initial_ms),
            max: Duration::from_millis(max_ms),
            multiplier: 2,
            jitter: 0.0,
            max_attempts: None,
        }
    }

    #[test]
    fn the_default_schedule_matches_the_spec_recommendation() {
        assert_eq!(DEFAULT_BACKOFF.initial, Duration::from_secs(1));
        assert_eq!(DEFAULT_BACKOFF.max, Duration::from_secs(30));
    }

    #[test]
    fn delays_grow_geometrically() {
        let backoff = exact(100, 10_000);
        assert_eq!(backoff.delay_for(1), Duration::from_millis(100));
        assert_eq!(backoff.delay_for(2), Duration::from_millis(200));
        assert_eq!(backoff.delay_for(3), Duration::from_millis(400));
        assert_eq!(backoff.delay_for(4), Duration::from_millis(800));
    }

    #[test]
    fn delays_are_capped() {
        let backoff = exact(1_000, 3_000);
        assert_eq!(backoff.delay_for(1), Duration::from_millis(1_000));
        assert_eq!(backoff.delay_for(2), Duration::from_millis(2_000));
        assert_eq!(
            backoff.delay_for(3),
            Duration::from_millis(3_000),
            "the delay must saturate at `max`"
        );
        assert_eq!(backoff.delay_for(50), Duration::from_millis(3_000));
    }

    #[test]
    fn a_long_outage_does_not_overflow_the_delay() {
        let backoff = Backoff {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(30),
            multiplier: 1_000,
            jitter: 0.0,
            max_attempts: None,
        };
        assert_eq!(backoff.delay_for(u32::MAX), Duration::from_secs(30));
    }

    #[test]
    fn jitter_keeps_the_delay_within_its_band() {
        let backoff = Backoff {
            jitter: 0.5,
            ..exact(1_000, 60_000)
        };
        for attempt in 1..=6 {
            let base = backoff.base_delay(attempt);
            for _ in 0..64 {
                let delay = backoff.delay_for(attempt);
                assert!(
                    delay <= base && delay >= base.mul_f64(0.5),
                    "attempt {attempt}: {delay:?} outside half of {base:?}"
                );
            }
        }
    }

    #[test]
    fn full_jitter_can_reach_zero_but_never_exceeds_the_base() {
        let backoff = Backoff {
            jitter: 1.0,
            ..exact(1_000, 60_000)
        };
        let base = backoff.base_delay(1);
        for _ in 0..64 {
            assert!(backoff.delay_for(1) <= base);
        }
    }

    #[test]
    fn an_out_of_range_jitter_is_clamped_rather_than_inverting_the_delay() {
        let backoff = Backoff {
            jitter: 4.0,
            ..exact(1_000, 60_000)
        };
        let base = backoff.base_delay(1);
        for _ in 0..64 {
            let delay = backoff.delay_for(1);
            assert!(delay <= base, "{delay:?} must not exceed {base:?}");
        }
    }

    #[test]
    fn attempts_are_bounded_when_a_maximum_is_set() {
        let backoff = Backoff {
            max_attempts: Some(3),
            ..exact(1, 1)
        };
        assert!(backoff.allows(1));
        assert!(backoff.allows(3));
        assert!(!backoff.allows(4));
    }

    #[test]
    fn attempts_are_unbounded_without_a_maximum() {
        let backoff = exact(1, 1);
        assert!(backoff.allows(u32::MAX));
    }
}
