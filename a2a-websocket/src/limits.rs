//! Connection and stream limits (spec Section 13.5).
//!
//! Two caps, both asked for by Section 13.5: how many connections one
//! authenticated identity may hold open against a server, and how many streams
//! may be active on a single connection.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Caps applied when no explicit ones are given.
///
/// Section 13.5 asks for limits without recommending values, unlike the
/// intervals in Section 2.4. These are deliberately generous: they exist to stop
/// one identity from exhausting a server, not to shape normal traffic. Deployments
/// with a known client population should set their own.
pub const DEFAULT_CONNECTION_LIMITS: ConnectionLimits = ConnectionLimits {
    max_connections_per_identity: 256,
    max_streams_per_connection: 128,
};

/// Resource caps for a WebSocket server (spec Section 13.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConnectionLimits {
    /// Maximum concurrent connections held by one authenticated identity.
    /// Anonymous connections are not counted, since they share no identity to
    /// attribute them to.
    pub max_connections_per_identity: usize,
    /// Maximum concurrent active streams on one connection.
    pub max_streams_per_connection: usize,
}

/// Whether a server enforces the Section 13.5 caps, and with what values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionLimitPolicy {
    /// Apply [`DEFAULT_CONNECTION_LIMITS`].
    #[default]
    Default,
    /// Apply explicit caps.
    Custom(ConnectionLimits),
    /// Enforce neither cap. Appropriate only behind a layer that bounds these
    /// itself.
    Disabled,
}

impl ConnectionLimitPolicy {
    /// The caps to apply, or `None` when they are switched off.
    pub fn limits(&self) -> Option<ConnectionLimits> {
        match self {
            ConnectionLimitPolicy::Default => Some(DEFAULT_CONNECTION_LIMITS),
            ConnectionLimitPolicy::Custom(limits) => Some(*limits),
            ConnectionLimitPolicy::Disabled => None,
        }
    }
}

/// Counts live connections per authenticated identity across one server.
///
/// Shared by every connection the router serves, in the same way as the
/// per-identity rate limiter.
#[derive(Debug)]
pub struct IdentityConnectionCounter {
    max: usize,
    counts: Mutex<HashMap<String, usize>>,
}

impl IdentityConnectionCounter {
    pub fn new(max: usize) -> Self {
        IdentityConnectionCounter {
            max,
            counts: Mutex::new(HashMap::new()),
        }
    }

    /// Claim a slot for `identity`, or return `None` when it already holds the
    /// maximum. The returned guard releases the slot when dropped, so a
    /// connection that ends for any reason — including a panic — gives its slot
    /// back.
    pub fn try_acquire(self: &Arc<Self>, identity: &str) -> Option<ConnectionSlot> {
        let mut counts = self.counts.lock().unwrap();
        let count = counts.entry(identity.to_string()).or_insert(0);
        if *count >= self.max {
            // Leaves a zero entry behind when the identity had none; the
            // release path prunes those.
            if *count == 0 {
                counts.remove(identity);
            }
            return None;
        }
        *count += 1;
        Some(ConnectionSlot {
            counter: self.clone(),
            identity: identity.to_string(),
        })
    }

    fn release(&self, identity: &str) {
        let mut counts = self.counts.lock().unwrap();
        if let Some(count) = counts.get_mut(identity) {
            *count = count.saturating_sub(1);
            // Drop identities back to nothing so a server that sees many
            // short-lived identities does not accumulate entries forever.
            if *count == 0 {
                counts.remove(identity);
            }
        }
    }

    #[cfg(test)]
    fn count_for(&self, identity: &str) -> usize {
        self.counts
            .lock()
            .unwrap()
            .get(identity)
            .copied()
            .unwrap_or(0)
    }
}

/// Holds one connection slot for an identity, releasing it on drop.
#[derive(Debug)]
pub struct ConnectionSlot {
    counter: Arc<IdentityConnectionCounter>,
    identity: String,
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.counter.release(&self.identity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_applies_the_default_caps() {
        assert_eq!(
            ConnectionLimitPolicy::default().limits(),
            Some(DEFAULT_CONNECTION_LIMITS)
        );
    }

    #[test]
    fn disabled_policy_yields_no_caps() {
        assert!(ConnectionLimitPolicy::Disabled.limits().is_none());
    }

    #[test]
    fn connections_are_admitted_up_to_the_cap() {
        let counter = Arc::new(IdentityConnectionCounter::new(2));
        let first = counter.try_acquire("alice");
        let second = counter.try_acquire("alice");
        assert!(first.is_some());
        assert!(second.is_some());
        assert!(
            counter.try_acquire("alice").is_none(),
            "a third connection must be refused"
        );
    }

    #[test]
    fn dropping_a_slot_frees_capacity() {
        let counter = Arc::new(IdentityConnectionCounter::new(1));
        let slot = counter.try_acquire("alice").unwrap();
        assert!(counter.try_acquire("alice").is_none());

        drop(slot);
        assert!(
            counter.try_acquire("alice").is_some(),
            "capacity must return when a connection ends"
        );
    }

    #[test]
    fn the_cap_is_per_identity() {
        let counter = Arc::new(IdentityConnectionCounter::new(1));
        let _alice = counter.try_acquire("alice").unwrap();
        assert!(
            counter.try_acquire("bob").is_some(),
            "one identity's connections must not exhaust another's budget"
        );
    }

    #[test]
    fn released_identities_are_pruned() {
        let counter = Arc::new(IdentityConnectionCounter::new(4));
        let slot = counter.try_acquire("alice").unwrap();
        assert_eq!(counter.count_for("alice"), 1);
        drop(slot);
        assert_eq!(
            counter.count_for("alice"),
            0,
            "an identity with no connections must not be retained"
        );
        assert!(counter.counts.lock().unwrap().is_empty());
    }

    #[test]
    fn a_refused_acquire_does_not_leave_an_entry_behind() {
        let counter = Arc::new(IdentityConnectionCounter::new(0));
        assert!(counter.try_acquire("alice").is_none());
        assert!(
            counter.counts.lock().unwrap().is_empty(),
            "refusing a connection must not accumulate state"
        );
    }
}
