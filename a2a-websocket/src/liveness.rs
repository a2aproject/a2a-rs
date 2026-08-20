//! Connection liveness: keep-alive pings and idle timeouts (spec Section 2.4).
//!
//! Answering a ping is handled by the WebSocket layer itself. This module covers
//! the parts the binding has to drive: sending pings on an interval, closing a
//! connection whose peer stops answering them, and closing one that has carried
//! no application traffic for a long time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::time::Instant;

/// Intervals recommended by spec Section 2.4.
pub const DEFAULT_LIVENESS: Liveness = Liveness {
    ping_interval: Duration::from_secs(30),
    pong_timeout: Duration::from_secs(10),
    idle_timeout: Duration::from_secs(300),
};

/// Keep-alive and idle timings for a connection (spec Section 2.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Liveness {
    /// How often to send a keep-alive ping. Recommended: 30 seconds.
    pub ping_interval: Duration,
    /// How long to wait for the answering pong before giving up on the peer.
    /// Recommended: 10 seconds.
    pub pong_timeout: Duration,
    /// How long a connection may carry no application-level message before it is
    /// closed. Recommended: 5 minutes.
    pub idle_timeout: Duration,
}

/// Whether a server enforces keep-alive and idle timeouts, and with what
/// timings.
///
/// Defaults to [`DEFAULT_LIVENESS`]. Opting out is deliberate rather than the
/// result of leaving a field unset, since Section 2.4 asks servers to ping and
/// to bound idle connections.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LivenessPolicy {
    /// Apply [`DEFAULT_LIVENESS`].
    #[default]
    Default,
    /// Apply explicit timings.
    Custom(Liveness),
    /// Send no pings and never time out an idle connection. Appropriate only
    /// when a proxy or load balancer in front of the agent already does both.
    Disabled,
}

impl LivenessPolicy {
    /// The timings to apply, or `None` when liveness checking is switched off.
    pub fn liveness(&self) -> Option<Liveness> {
        match self {
            LivenessPolicy::Default => Some(DEFAULT_LIVENESS),
            LivenessPolicy::Custom(liveness) => Some(*liveness),
            LivenessPolicy::Disabled => None,
        }
    }
}

/// Records when a connection last carried application traffic and how many
/// pongs it has answered with.
///
/// Shared by the read half, the write half, and the liveness monitor, so it is
/// built from atomics rather than a lock.
#[derive(Debug)]
pub(crate) struct ActivityTracker {
    base: Instant,
    last_message_ms: AtomicU64,
    pongs: AtomicU64,
}

impl ActivityTracker {
    pub(crate) fn new() -> Self {
        ActivityTracker {
            base: Instant::now(),
            last_message_ms: AtomicU64::new(0),
            pongs: AtomicU64::new(0),
        }
    }

    /// Note an application-level message in *either* direction.
    ///
    /// Counting outbound frames matters: a long-running stream can push events
    /// for minutes without the client saying anything, and that connection is
    /// busy rather than idle.
    pub(crate) fn record_message(&self) {
        self.last_message_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    pub(crate) fn record_pong(&self) {
        self.pongs.fetch_add(1, Ordering::Relaxed);
    }

    /// Number of pongs seen so far. Compared before and after a ping rather than
    /// timestamped, so the check does not depend on clock resolution.
    pub(crate) fn pong_count(&self) -> u64 {
        self.pongs.load(Ordering::Relaxed)
    }

    /// How long since the last application-level message.
    pub(crate) fn idle_for(&self) -> Duration {
        Duration::from_millis(
            self.now_ms()
                .saturating_sub(self.last_message_ms.load(Ordering::Relaxed)),
        )
    }

    fn now_ms(&self) -> u64 {
        self.base.elapsed().as_millis() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_uses_the_intervals_the_spec_recommends() {
        let liveness = LivenessPolicy::default().liveness().unwrap();
        assert_eq!(liveness, DEFAULT_LIVENESS);
        assert_eq!(liveness.ping_interval, Duration::from_secs(30));
        assert_eq!(liveness.pong_timeout, Duration::from_secs(10));
        assert_eq!(liveness.idle_timeout, Duration::from_secs(300));
    }

    #[test]
    fn disabled_policy_yields_no_timings() {
        assert!(LivenessPolicy::Disabled.liveness().is_none());
    }

    #[test]
    fn custom_policy_is_returned_verbatim() {
        let custom = Liveness {
            ping_interval: Duration::from_secs(1),
            pong_timeout: Duration::from_millis(250),
            idle_timeout: Duration::from_secs(5),
        };
        assert_eq!(
            LivenessPolicy::Custom(custom).liveness(),
            Some(custom),
            "custom timings must not be silently replaced by the defaults"
        );
    }

    #[tokio::test]
    async fn recording_a_message_resets_the_idle_clock() {
        const QUIET: Duration = Duration::from_millis(30);

        let activity = ActivityTracker::new();
        tokio::time::sleep(QUIET).await;
        assert!(
            activity.idle_for() >= QUIET,
            "idle time must accrue from connection start"
        );

        activity.record_message();
        assert!(
            activity.idle_for() < QUIET,
            "a message must reset the idle clock"
        );
    }

    #[test]
    fn pong_count_increments_per_pong() {
        let activity = ActivityTracker::new();
        assert_eq!(activity.pong_count(), 0);
        activity.record_pong();
        activity.record_pong();
        assert_eq!(activity.pong_count(), 2);
    }
}
