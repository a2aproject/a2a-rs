// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! An uncommitted request offered to an interrupted execution's reader.

use std::sync::{
    Arc,
    atomic::{AtomicU8, Ordering},
};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

const WAITING: u8 = 0;
const CLAIMED: u8 = 1;
const CANCELED: u8 = 2;
const COMMITTING: u8 = 3;
const COMPLETED: u8 = 4;

pub(crate) struct OfferedInput {
    state: AtomicU8,
    pub(super) ready: CancellationToken,
    changed: Arc<Notify>,
}

impl OfferedInput {
    pub(super) fn claim(self: &Arc<Self>) -> Option<ReadClaim> {
        if self
            .state
            .compare_exchange(WAITING, CLAIMED, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }
        // The task's retiring mailbox may still have a waiter. A read claim
        // must also wake its replacement rather than letting that waiter steal it.
        self.changed.notify_waiters();
        Some(ReadClaim(self.clone()))
    }
}

/// A pending recv owns its claim. Canceling that future withdraws the claim
/// unless persistence has already started; committed input has a normal receipt.
pub(super) struct ReadClaim(Arc<OfferedInput>);

impl ReadClaim {
    pub(super) fn ready(&self) -> CancellationToken {
        self.0.ready.clone()
    }
}

impl Drop for ReadClaim {
    fn drop(&mut self) {
        if self
            .0
            .state
            .compare_exchange(CLAIMED, CANCELED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            self.0.ready.cancel();
            self.0.changed.notify_waiters();
        }
    }
}

/// Only the server owns this lease. Dropping an unfinished request releases a
/// reader waiting for its commit, including during shutdown or a store panic.
pub(crate) struct InputOffer {
    inner: Arc<OfferedInput>,
}

impl InputOffer {
    pub(crate) fn new(changed: Arc<Notify>) -> Self {
        let inner = Arc::new(OfferedInput {
            state: AtomicU8::new(WAITING),
            ready: CancellationToken::new(),
            changed,
        });
        Self { inner }
    }

    pub(crate) fn reader(&self) -> Arc<OfferedInput> {
        self.inner.clone()
    }

    pub(crate) fn canceled(&self) -> bool {
        self.inner.state.load(Ordering::Acquire) == CANCELED
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

    pub(crate) fn complete(self) {
        self.inner.state.store(COMPLETED, Ordering::Release);
        self.inner.ready.cancel();
    }
}

impl Drop for InputOffer {
    fn drop(&mut self) {
        if self.inner.state.load(Ordering::Acquire) != COMPLETED {
            self.inner.state.store(CANCELED, Ordering::Release);
            self.inner.ready.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ClientInput, ClientInputs, executor::QueuedInput};
    use a2a::{Message, Part, Role};
    use futures::FutureExt;

    fn input() -> ClientInput {
        ClientInput::new(Message::new(Role::User, vec![Part::text("committed")]), 7)
    }

    #[tokio::test]
    async fn stale_offer_cleanup_cannot_erase_the_next_callers_notification() {
        let (sender, mut inputs) = ClientInputs::channel(1);
        sender.offer(None);
        let stale = InputOffer::new(Arc::default());
        let next = InputOffer::new(Arc::default());
        sender.offer(Some(stale.reader()));
        sender.offer(Some(next.reader()));
        sender.clear_offer(&stale.reader());
        drop(stale);
        let mut read = Box::pin(inputs.recv());
        assert!(read.as_mut().now_or_never().is_none());
        assert!(next.claimed());
        assert!(next.begin_commit());
        assert_eq!(sender.received_generation(), 0);
        sender.try_send(input()).unwrap();
        next.complete();
        assert_eq!(read.await.unwrap().generation, 7);
        assert_eq!(sender.received_generation(), 7);
    }

    #[tokio::test]
    async fn try_recv_skips_rejected_receipts_without_advancing_input_progress() {
        let (sender, mut inputs) = ClientInputs::channel(2);
        let rejected = QueuedInput::new(input());
        sender.sender.try_send(rejected.clone()).unwrap();
        assert!(rejected.reclaim().is_some());
        assert!(inputs.try_recv().is_none());
        assert_eq!(sender.received_generation(), 0);
        sender.try_send(input()).unwrap();
        assert_eq!(inputs.try_recv().unwrap().generation, 7);
        assert_eq!(sender.received_generation(), 7);
    }

    #[tokio::test]
    async fn read_claim_waits_for_commit_and_canceled_offers_do_not_close_the_inbox() {
        let (sender, mut inputs) = ClientInputs::channel(1);
        for commit in [false, true] {
            let lease = InputOffer::new(Arc::default());
            sender.offer(Some(lease.reader()));
            let mut read = Box::pin(inputs.recv());
            assert!(read.as_mut().now_or_never().is_none());
            assert!(lease.claimed());
            assert!(lease.begin_commit());
            if commit {
                sender.try_send(input()).unwrap();
                lease.complete();
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
    async fn try_recv_never_claims_an_uncommitted_offer() {
        let (sender, mut inputs) = ClientInputs::channel(1);
        let lease = InputOffer::new(Arc::default());
        sender.offer(Some(lease.reader()));
        assert!(inputs.try_recv().is_none());
        assert!(!lease.claimed());
        assert!(!lease.begin_commit());
        sender.try_send(input()).unwrap();
        assert_eq!(inputs.try_recv().unwrap().generation, 7);
        assert!(inputs.try_recv().is_none());
    }

    #[tokio::test]
    async fn a_canceled_unclaimed_offer_never_reaches_executor_code() {
        let (sender, mut inputs) = ClientInputs::channel(1);
        let lease = InputOffer::new(Arc::default());
        sender.offer(Some(lease.reader()));
        drop(lease);
        drop(sender);
        assert!(inputs.recv().await.is_none());
    }

    #[tokio::test]
    async fn canceling_recv_withdraws_a_claim_before_commit_starts() {
        let (sender, mut inputs) = ClientInputs::channel(1);
        let lease = InputOffer::new(Arc::default());
        sender.offer(Some(lease.reader()));
        let mut read = Box::pin(inputs.recv());
        assert!(read.as_mut().now_or_never().is_none());
        assert!(lease.claimed());
        drop(read);
        assert!(lease.canceled());
        assert!(!lease.begin_commit());
        assert_eq!(sender.received_generation(), 0);
    }

    #[tokio::test]
    async fn a_reader_lost_during_commit_leaves_a_reclaimable_unread_receipt() {
        let (sender, mut inputs) = ClientInputs::channel(1);
        let lease = InputOffer::new(Arc::default());
        sender.offer(Some(lease.reader()));
        let mut read = Box::pin(inputs.recv());
        assert!(read.as_mut().now_or_never().is_none());
        assert!(lease.begin_commit());
        drop(read);
        let tracked = QueuedInput::new(input());
        sender.sender.try_send(tracked.clone()).unwrap();
        lease.complete();
        drop(inputs);
        assert_eq!(sender.received_generation(), 0);
        assert_eq!(tracked.reclaim().unwrap().generation, 7);
    }
}
