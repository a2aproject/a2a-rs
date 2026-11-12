// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! An uncommitted request offered to an interrupted execution's reader.

use super::{ClientInput, QueuedInput};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU8, Ordering},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const WAITING: u8 = 0;
const CLAIMED: u8 = 1;
const CANCELED: u8 = 2;
const COMMITTING: u8 = 3;

pub(crate) struct OfferedInput {
    state: AtomicU8,
    value: OnceLock<ClientInput>,
    pub(super) ready: CancellationToken,
    changed: Arc<Notify>,
}

impl OfferedInput {
    pub(super) fn claim(&self) -> bool {
        if self
            .state
            .compare_exchange(WAITING, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        // The task's retiring mailbox may still have a waiter. A read claim
        // must also wake its replacement rather than letting that waiter steal it.
        self.changed.notify_waiters();
        true
    }

    pub(super) fn value(&self) -> Option<ClientInput> {
        self.value.get().cloned()
    }
}

/// Only the server owns this lease. Dropping an unfinished request releases a
/// reader waiting for its commit, including during shutdown or a store panic.
pub(crate) struct InputOffer {
    inner: Arc<OfferedInput>,
}

impl InputOffer {
    pub(crate) fn new(changed: Arc<Notify>) -> (QueuedInput, Self) {
        let inner = Arc::new(OfferedInput {
            state: AtomicU8::new(WAITING),
            value: OnceLock::new(),
            ready: CancellationToken::new(),
            changed,
        });
        (QueuedInput::Offered(inner.clone()), Self { inner })
    }

    pub(crate) fn claimed(&self) -> bool {
        self.inner.state.load(Ordering::Acquire) == CLAIMED
    }

    pub(crate) fn begin_commit(&self) -> bool {
        self.inner
            .state
            .compare_exchange(CLAIMED, COMMITTING, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn complete(self, input: ClientInput) {
        let _ = self.inner.value.set(input);
        self.inner.ready.cancel();
    }
}

impl Drop for InputOffer {
    fn drop(&mut self) {
        if self.inner.value.get().is_none() {
            self.inner.state.store(CANCELED, Ordering::Release);
            self.inner.ready.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ClientInputs;
    use a2a::{Message, Part, Role};
    use futures::FutureExt;

    fn input() -> ClientInput {
        ClientInput::new(Message::new(Role::User, vec![Part::text("committed")]), 7)
    }

    #[tokio::test]
    async fn read_claim_waits_for_commit_and_canceled_offers_do_not_close_the_inbox() {
        let (sender, mut inputs) = ClientInputs::channel(1);
        for commit in [false, true] {
            let (queued, lease) = InputOffer::new(Arc::default());
            sender.sender.try_send(queued).ok().unwrap();
            let mut read = Box::pin(inputs.recv());
            assert!(read.as_mut().now_or_never().is_none());
            assert!(lease.claimed());
            assert!(lease.begin_commit());
            if commit {
                lease.complete(input());
            } else {
                drop(lease);
                sender.try_send(input()).unwrap();
            }
            let received = read.await.unwrap();
            assert_eq!(received.generation, 7);
            assert_eq!(received.message.text(), Some("committed"));
        }
        drop(sender);
        assert!(inputs.recv().await.is_none());
    }

    #[tokio::test]
    async fn try_recv_claims_an_offer_and_returns_only_the_committed_input() {
        let (sender, mut inputs) = ClientInputs::channel(1);
        let (queued, lease) = InputOffer::new(Arc::default());
        sender.sender.try_send(queued).ok().unwrap();
        assert!(inputs.try_recv().is_none());
        assert!(lease.claimed());
        assert!(lease.begin_commit());
        lease.complete(input());
        assert_eq!(inputs.try_recv().unwrap().generation, 7);
        assert!(inputs.try_recv().is_none());
    }

    #[tokio::test]
    async fn a_canceled_unclaimed_offer_never_reaches_executor_code() {
        let (sender, mut inputs) = ClientInputs::channel(1);
        let (queued, lease) = InputOffer::new(Arc::default());
        sender.sender.try_send(queued).ok().unwrap();
        drop(lease);
        drop(sender);
        assert!(inputs.recv().await.is_none());
    }
}
