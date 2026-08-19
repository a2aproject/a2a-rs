// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
//! Inbound message rate limiting (specification Section 13.3).
//!
//! The spec requires servers to rate limit incoming WebSocket messages, and
//! recommends applying the limit both per-connection and per-authenticated
//! identity. Both scopes are enforced here with a token bucket, which allows a
//! short burst up to [`RateLimit::max_messages`] and then settles at the
//! configured sustained rate.
//!
//! A connection that exceeds its limit receives a JSON-RPC error with code
//! `-32000` and message `"Rate limit exceeded"`, after which the server closes
//! with code `1008` (Policy Violation).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Rate limit applied when [`WebSocketConfig::rate_limit`] is left at its
/// default: 100 inbound messages per second, per connection and per identity.
///
/// Section 13.3 requires a limit but does not prescribe a value. 100 messages
/// per second is far above what A2A request traffic needs — each message is a
/// complete RPC — while still capping a flood.
///
/// [`WebSocketConfig::rate_limit`]: crate::server::WebSocketConfig::rate_limit
pub const DEFAULT_RATE_LIMIT: RateLimit = RateLimit {
    max_messages: 100,
    window: Duration::from_secs(1),
};

/// Whether, and how, inbound messages are rate limited (spec Section 13.3).
///
/// The default is [`RateLimitPolicy::Default`] rather than "off", because
/// Section 13.3 makes rate limiting a **MUST** for servers: a server built from
/// [`WebSocketConfig::default()`](crate::server::WebSocketConfig) has to be
/// conformant. Turning the limit off is therefore a deliberate, greppable
/// choice rather than an omission.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum RateLimitPolicy {
    /// Apply [`DEFAULT_RATE_LIMIT`].
    #[default]
    Default,
    /// Apply a specific limit.
    Custom(RateLimit),
    /// Admit every message. Only appropriate when a trusted layer in front of
    /// the agent enforces its own limit, since it opts out of a MUST.
    Disabled,
}

impl RateLimitPolicy {
    /// The limit to enforce, or `None` when rate limiting is switched off.
    pub fn limit(&self) -> Option<RateLimit> {
        match self {
            RateLimitPolicy::Default => Some(DEFAULT_RATE_LIMIT),
            RateLimitPolicy::Custom(limit) => Some(*limit),
            RateLimitPolicy::Disabled => None,
        }
    }
}

/// Message rate allowed on a connection, and for a single authenticated
/// identity across all of its connections (spec Section 13.3).
///
/// `max_messages` doubles as the burst capacity: a freshly opened connection
/// may send that many messages immediately, after which messages are admitted
/// at the sustained rate of `max_messages` per `window`.
///
/// A `max_messages` of `0` rejects every message; use
/// [`RateLimitPolicy::Disabled`] to switch rate limiting off instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateLimit {
    pub max_messages: u32,
    pub window: Duration,
}

impl RateLimit {
    pub fn new(max_messages: u32, window: Duration) -> Self {
        RateLimit {
            max_messages,
            window,
        }
    }

    /// Allow `max_messages` per second.
    pub fn per_second(max_messages: u32) -> Self {
        RateLimit::new(max_messages, Duration::from_secs(1))
    }

    fn capacity(&self) -> f64 {
        f64::from(self.max_messages)
    }

    /// Sustained refill rate. A zero-length window degenerates to "refill
    /// instantly", which keeps the arithmetic finite and simply means the
    /// limiter never rejects.
    fn tokens_per_second(&self) -> f64 {
        let seconds = self.window.as_secs_f64();
        if seconds <= 0.0 {
            f64::MAX
        } else {
            self.capacity() / seconds
        }
    }
}

/// A single token bucket. Starts full so a new connection is not penalised for
/// an empty history.
#[derive(Debug)]
struct TokenBucket {
    tokens: f64,
    updated: Instant,
}

impl TokenBucket {
    fn full(limit: &RateLimit, now: Instant) -> Self {
        TokenBucket {
            tokens: limit.capacity(),
            updated: now,
        }
    }

    fn refilled(&self, limit: &RateLimit, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        (self.tokens + elapsed * limit.tokens_per_second()).min(limit.capacity())
    }

    /// Admit one message if a token is available.
    fn try_consume(&mut self, limit: &RateLimit, now: Instant) -> bool {
        self.tokens = self.refilled(limit, now);
        self.updated = now;
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Whether the bucket has fully recovered. Such a bucket carries no state
    /// worth keeping, because a newly created bucket also starts full.
    fn is_recovered(&self, limit: &RateLimit, now: Instant) -> bool {
        self.refilled(limit, now) >= limit.capacity()
    }
}

/// Upper bound on tracked identities. Exceeding it triggers a prune of
/// recovered buckets, which is lossless because a recovered bucket is
/// indistinguishable from a newly created one.
const MAX_TRACKED_IDENTITIES: usize = 10_000;

/// Rate limiter shared by every connection belonging to the same authenticated
/// identity (spec Section 13.3, "per-authenticated-identity").
#[derive(Debug)]
pub struct IdentityRateLimiter {
    limit: RateLimit,
    buckets: Mutex<HashMap<String, TokenBucket>>,
}

impl IdentityRateLimiter {
    pub fn new(limit: RateLimit) -> Self {
        IdentityRateLimiter {
            limit,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    fn allow(&self, identity: &str, now: Instant) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(|err| err.into_inner());
        if buckets.len() >= MAX_TRACKED_IDENTITIES {
            buckets.retain(|_, bucket| !bucket.is_recovered(&self.limit, now));
        }
        buckets
            .entry(identity.to_string())
            .or_insert_with(|| TokenBucket::full(&self.limit, now))
            .try_consume(&self.limit, now)
    }

    #[cfg(test)]
    fn tracked(&self) -> usize {
        self.buckets.lock().unwrap().len()
    }
}

/// Per-connection view of the rate limit, combining the connection's own bucket
/// with the shared bucket for its authenticated identity.
pub(crate) struct ConnectionRateLimiter {
    limit: RateLimit,
    bucket: TokenBucket,
    identity: Option<String>,
    shared: Option<std::sync::Arc<IdentityRateLimiter>>,
}

impl ConnectionRateLimiter {
    pub(crate) fn new(
        limit: RateLimit,
        identity: Option<String>,
        shared: Option<std::sync::Arc<IdentityRateLimiter>>,
    ) -> Self {
        let now = Instant::now();
        ConnectionRateLimiter {
            bucket: TokenBucket::full(&limit, now),
            limit,
            identity,
            shared,
        }
    }

    /// Admit one inbound message, or return `false` when either the
    /// per-connection or the per-identity limit is exhausted.
    pub(crate) fn allow(&mut self) -> bool {
        let now = Instant::now();
        if !self.bucket.try_consume(&self.limit, now) {
            return false;
        }
        match (self.shared.as_ref(), self.identity.as_deref()) {
            // Anonymous connections have no identity to aggregate over, so the
            // per-connection bucket is the only applicable scope.
            (Some(shared), Some(identity)) => shared.allow(identity, now),
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn limiter(max: u32) -> ConnectionRateLimiter {
        ConnectionRateLimiter::new(RateLimit::per_second(max), None, None)
    }

    #[test]
    fn burst_up_to_capacity_is_admitted_then_rejected() {
        let mut limiter = limiter(3);
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(limiter.allow());
        assert!(!limiter.allow(), "fourth message exceeds the burst");
    }

    #[test]
    fn the_default_policy_enforces_a_limit() {
        // Section 13.3 makes rate limiting a MUST, so a default-constructed
        // server has to come with one rather than silently opting out.
        assert_eq!(RateLimitPolicy::default().limit(), Some(DEFAULT_RATE_LIMIT));
        assert_eq!(RateLimitPolicy::Disabled.limit(), None);
    }

    #[test]
    fn zero_capacity_rejects_everything() {
        assert!(!limiter(0).allow());
    }

    #[test]
    fn tokens_refill_over_time() {
        let limit = RateLimit::per_second(10);
        let start = Instant::now();
        let mut bucket = TokenBucket::full(&limit, start);

        for _ in 0..10 {
            assert!(bucket.try_consume(&limit, start));
        }
        assert!(!bucket.try_consume(&limit, start), "bucket is drained");

        // Half a window later, half the capacity is back.
        let later = start + Duration::from_millis(500);
        for _ in 0..5 {
            assert!(bucket.try_consume(&limit, later), "should have refilled");
        }
        assert!(!bucket.try_consume(&limit, later));
    }

    #[test]
    fn refill_is_capped_at_capacity() {
        let limit = RateLimit::per_second(2);
        let start = Instant::now();
        let mut bucket = TokenBucket::full(&limit, start);
        // A long idle period must not accumulate more than the burst.
        let much_later = start + Duration::from_secs(3600);
        assert!(bucket.try_consume(&limit, much_later));
        assert!(bucket.try_consume(&limit, much_later));
        assert!(!bucket.try_consume(&limit, much_later));
    }

    #[test]
    fn identity_limit_is_shared_across_connections() {
        let shared = Arc::new(IdentityRateLimiter::new(RateLimit::per_second(3)));
        let mut first = ConnectionRateLimiter::new(
            RateLimit::per_second(100),
            Some("alice".into()),
            Some(shared.clone()),
        );
        let mut second = ConnectionRateLimiter::new(
            RateLimit::per_second(100),
            Some("alice".into()),
            Some(shared.clone()),
        );

        // The shared budget of 3 is consumed across both connections even
        // though neither exhausts its own generous per-connection bucket.
        assert!(first.allow());
        assert!(second.allow());
        assert!(first.allow());
        assert!(!second.allow(), "identity budget is exhausted");
    }

    #[test]
    fn identities_are_limited_independently() {
        let shared = Arc::new(IdentityRateLimiter::new(RateLimit::per_second(1)));
        let mut alice = ConnectionRateLimiter::new(
            RateLimit::per_second(100),
            Some("alice".into()),
            Some(shared.clone()),
        );
        let mut bob = ConnectionRateLimiter::new(
            RateLimit::per_second(100),
            Some("bob".into()),
            Some(shared.clone()),
        );

        assert!(alice.allow());
        assert!(!alice.allow());
        assert!(bob.allow(), "bob has his own budget");
    }

    #[test]
    fn anonymous_connections_fall_back_to_the_connection_bucket() {
        let shared = Arc::new(IdentityRateLimiter::new(RateLimit::per_second(1)));
        let mut anon =
            ConnectionRateLimiter::new(RateLimit::per_second(2), None, Some(shared.clone()));

        assert!(anon.allow());
        assert!(anon.allow());
        assert!(!anon.allow(), "per-connection bucket still applies");
        assert_eq!(shared.tracked(), 0, "no identity bucket was created");
    }

    #[test]
    fn recovered_identity_buckets_are_pruned() {
        let limiter = IdentityRateLimiter::new(RateLimit::per_second(1));
        let start = Instant::now();
        for i in 0..MAX_TRACKED_IDENTITIES {
            assert!(limiter.allow(&format!("user-{i}"), start));
        }
        assert_eq!(limiter.tracked(), MAX_TRACKED_IDENTITIES);

        // A later request finds every existing bucket recovered, so the map is
        // pruned back rather than growing without bound.
        let later = start + Duration::from_secs(60);
        assert!(limiter.allow("newcomer", later));
        assert!(
            limiter.tracked() < MAX_TRACKED_IDENTITIES,
            "recovered buckets should have been pruned"
        );
    }
}
