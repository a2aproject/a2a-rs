// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Owned task inputs and cancellation-safe receipt tracking.

use crate::middleware::{ServiceParams, User};
use a2a::*;
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU8, Ordering},
    },
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// A client message that reached a running execution: sent on a live stream
/// (`SendLiveMessage`) or with `SendMessage` or `SendStreamingMessage` to a
/// task that is working.
///
/// By the time an executor reads it the server has already appended it to the
/// task's timeline and announced it to the subscribers; the executor decides
/// what the message means for its work.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ClientInput {
    /// The message the client sent.
    pub message: Message,
    /// The task generation at which the message was appended to the timeline.
    pub generation: i64,
    /// Trusted caller resolved by the server's authorizer for this input.
    /// Never inferred from client-controlled message metadata.
    pub user: Option<User>,
    /// Transport parameters belonging to this input, rather than the opener.
    pub service_params: ServiceParams,
    /// Request-level metadata belonging to this input.
    pub metadata: Option<HashMap<String, Value>>,
    /// Routing tenant supplied with this input.
    pub tenant: Option<String>,
    /// The response that accepted this input, for private unread-input errors.
    pub(crate) response_id: Option<uuid::Uuid>,
}

impl ClientInput {
    pub fn new(message: Message, generation: i64) -> Self {
        ClientInput {
            message,
            generation,
            user: None,
            service_params: ServiceParams::new(),
            metadata: None,
            tenant: None,
            response_id: None,
        }
    }

    pub(crate) fn received(
        request: &SendMessageRequest,
        generation: i64,
        user: Option<User>,
        params: &ServiceParams,
        response_id: Option<uuid::Uuid>,
    ) -> Self {
        Self {
            message: request.message.clone(),
            generation,
            user,
            service_params: params.clone(),
            metadata: request.metadata.clone(),
            tenant: request.tenant.clone(),
            response_id,
        }
    }
}

/// The client messages that arrive while an execution runs, on a live stream
/// or sent to the working task (see [`super::ExecutorContext::inputs`]).
///
/// The receiver is owned and can move into a Tokio task. The server tracks
/// delivery receipts: a message still unread at the end of a nonterminal
/// turn is handed to the next execution. If the task ends, its
/// sender receives a `StreamError`. An executor reads with [`recv`](Self::recv),
/// alongside its own work.
pub struct ClientInputs {
    receiver: Option<mpsc::Receiver<QueuedInput>>,
    closed: CancellationToken,
    revoked: CancellationToken,
    returned: Option<oneshot::Sender<mpsc::Receiver<QueuedInput>>>,
}

impl ClientInputs {
    /// A bounded channel for exercising an executor on its own.
    pub fn channel(capacity: usize) -> (ClientInputSender, ClientInputs) {
        let (sender, receiver) = mpsc::channel(capacity);
        let closed = CancellationToken::new();
        let revoked = CancellationToken::new();
        (
            ClientInputSender {
                sender,
                closed: closed.clone(),
                revoked: revoked.clone(),
            },
            ClientInputs {
                receiver: Some(receiver),
                closed,
                revoked,
                returned: None,
            },
        )
    }

    /// Keep the native receiver alive even if an executor ignores its inputs.
    /// The mailbox is owned by the existing execution state, not a new worker.
    pub(crate) fn for_execution(capacity: usize) -> (ClientInputSender, Self, ReturnedInputs) {
        let (sender, mut inputs) = Self::channel(capacity);
        let (returned, receiver) = oneshot::channel();
        inputs.returned = Some(returned);
        (sender, inputs, receiver)
    }

    pub(crate) fn closed() -> Self {
        let (sender, inputs) = Self::channel(1);
        drop(sender);
        inputs
    }

    /// The next message, waiting for one if none is queued. Closing the
    /// sending end preserves buffered messages; after those, returns `None`.
    pub async fn recv(&mut self) -> Option<ClientInput> {
        loop {
            if self.revoked.is_cancelled() {
                return None;
            }
            let receiver = self.receiver.as_mut().expect("receiver exists until drop");
            let input = tokio::select! {
                biased;
                _ = self.revoked.cancelled() => return None,
                _ = self.closed.cancelled() => {
                    receiver.close();
                    receiver.recv().await
                }
                input = receiver.recv() => input,
            }?;
            if self.revoked.is_cancelled() {
                return None;
            }
            if let Some(input) = input.claim() {
                return Some(input);
            }
        }
    }

    /// A message that is already queued, without waiting.
    pub fn try_recv(&mut self) -> Option<ClientInput> {
        if self.revoked.is_cancelled() {
            return None;
        }
        let receiver = self.receiver.as_mut().expect("receiver exists until drop");
        if self.closed.is_cancelled() {
            receiver.close();
        }
        let input = receiver.try_recv().ok()?;
        if self.revoked.is_cancelled() {
            return None;
        }
        input.claim()
    }

    #[cfg(test)]
    pub(crate) fn close_and_drain(&mut self) -> Vec<ClientInput> {
        self.closed.cancel();
        self.revoked.cancel();
        let receiver = self.receiver.as_mut().unwrap();
        receiver.close();
        std::iter::from_fn(|| receiver.try_recv().ok())
            .filter_map(|input| input.reclaim())
            .collect()
    }
}

impl Drop for ClientInputs {
    fn drop(&mut self) {
        if let (Some(returned), Some(receiver)) = (self.returned.take(), self.receiver.take()) {
            let _ = returned.send(receiver);
        }
    }
}

pub(crate) type ReturnedInputs = oneshot::Receiver<mpsc::Receiver<QueuedInput>>;

const UNREAD: u8 = 0;
const RECEIVED: u8 = 1;
const RECLAIMED: u8 = 2;

/// Reading and reclaiming race through one atomic receipt. This lets a worker
/// own the receiver without letting it read messages handed to a later turn.
#[derive(Clone)]
pub(crate) struct QueuedInput {
    input: Arc<ClientInput>,
    state: Arc<AtomicU8>,
}

impl QueuedInput {
    pub(crate) fn new(input: ClientInput) -> Self {
        Self {
            input: Arc::new(input),
            state: Arc::new(AtomicU8::new(UNREAD)),
        }
    }

    pub(crate) fn pending(&self) -> bool {
        self.state.load(Ordering::Acquire) == UNREAD
    }

    fn claim(self) -> Option<ClientInput> {
        self.take(RECEIVED)
    }

    pub(crate) fn reclaim(self) -> Option<ClientInput> {
        self.take(RECLAIMED)
    }

    fn take(self, state: u8) -> Option<ClientInput> {
        self.state
            .compare_exchange(UNREAD, state, Ordering::AcqRel, Ordering::Acquire)
            .ok()?;
        Some(Arc::try_unwrap(self.input).unwrap_or_else(|input| (*input).clone()))
    }
}

impl std::fmt::Debug for ClientInputs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientInputs").finish_non_exhaustive()
    }
}

/// The sending end of a [`ClientInputs`] made with [`ClientInputs::channel`].
/// When the last clone is dropped, buffered messages remain readable and then
/// `recv` returns `None`.
#[derive(Clone)]
pub struct ClientInputSender {
    pub(crate) sender: mpsc::Sender<QueuedInput>,
    pub(crate) closed: CancellationToken,
    revoked: CancellationToken,
}

impl ClientInputSender {
    /// Queues `input`. `Err` gives it back when the channel is full or closed.
    pub fn try_send(&self, input: ClientInput) -> Result<(), Box<ClientInput>> {
        if self.closed.is_cancelled() {
            return Err(Box::new(input));
        }
        self.sender
            .try_send(QueuedInput::new(input))
            .map_err(|error| {
                Box::new(
                    error
                        .into_inner()
                        .reclaim()
                        .expect("unsent input is unread"),
                )
            })
    }

    /// Signals closure; buffered messages remain readable by the executor.
    pub fn close(&self) {
        self.closed.cancel();
    }

    /// End this invocation's right to read, including in detached workers.
    pub(crate) fn revoke(&self) {
        self.revoked.cancel();
        self.closed.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(id: &str) -> ClientInput {
        ClientInput::new(
            Message {
                message_id: id.into(),
                ..Message::new(Role::User, vec![])
            },
            1,
        )
    }

    #[tokio::test]
    async fn closing_wakes_a_pending_receiver() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Wake, Waker};

        #[derive(Default)]
        struct Awoken(AtomicBool);
        impl Wake for Awoken {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let (sender, mut inputs) = ClientInputs::channel(1);
        let awoken = Arc::new(Awoken::default());
        let waker = Waker::from(awoken.clone());
        let mut cx = Context::from_waker(&waker);
        let mut pending = Box::pin(inputs.recv());
        assert!(pending.as_mut().poll(&mut cx).is_pending());
        sender.close();
        assert!(awoken.0.load(Ordering::SeqCst), "close must wake recv");
        assert!(matches!(
            pending.as_mut().poll(&mut cx),
            std::task::Poll::Ready(None)
        ));
    }

    #[tokio::test]
    async fn closing_preserves_buffered_input_and_rejects_new_sends() {
        let (sender, mut inputs) = ClientInputs::channel(2);
        sender.try_send(input("first")).unwrap();
        sender.try_send(input("second")).unwrap();
        sender.close();
        assert!(sender.try_send(input("late")).is_err());
        assert_eq!(inputs.recv().await.unwrap().message.message_id, "first");
        assert_eq!(inputs.recv().await.unwrap().message.message_id, "second");
        assert!(inputs.recv().await.is_none());
    }

    #[tokio::test]
    async fn last_sender_drop_closes_inputs_after_buffered_messages() {
        let (sender, mut inputs) = ClientInputs::channel(2);
        let other = sender.clone();
        sender.try_send(input("first")).unwrap();
        drop(sender);
        assert_eq!(inputs.recv().await.unwrap().message.message_id, "first");
        other.try_send(input("second")).unwrap();
        drop(other);
        assert_eq!(inputs.recv().await.unwrap().message.message_id, "second");
        assert!(inputs.recv().await.is_none());
    }

    #[tokio::test]
    async fn handler_drains_only_unread_inputs_at_turn_end() {
        let (sender, mut inputs) = ClientInputs::channel(3);
        sender.try_send(input("read")).unwrap();
        sender.try_send(input("next-turn-first")).unwrap();
        sender.try_send(input("next-turn-second")).unwrap();
        assert_eq!(inputs.recv().await.unwrap().message.message_id, "read");
        let unread = inputs.close_and_drain();
        assert_eq!(
            unread
                .iter()
                .map(|i| i.message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["next-turn-first", "next-turn-second"]
        );
        assert!(inputs.recv().await.is_none());
        assert!(sender.try_send(input("late")).is_err());
    }

    #[tokio::test]
    async fn abandoned_delivery_releases_reserved_capacity() {
        let (sender, mut inputs) = ClientInputs::channel(1);
        let permit = sender.sender.clone().try_reserve_owned().unwrap();
        assert!(sender.try_send(input("full")).is_err());
        drop(permit);
        sender.try_send(input("accepted")).unwrap();
        assert_eq!(inputs.recv().await.unwrap().message.message_id, "accepted");
    }

    #[test]
    fn a_concurrent_reader_and_handoff_deliver_each_input_once() {
        for _ in 0..64 {
            let queued = QueuedInput::new(input("raced"));
            let reader = queued.clone();
            let start = Arc::new(std::sync::Barrier::new(3));
            std::thread::scope(|scope| {
                let reading = &start;
                let read = scope.spawn(move || {
                    reading.wait();
                    reader.claim()
                });
                let handing_off = &start;
                let reclaim = scope.spawn(move || {
                    handing_off.wait();
                    queued.reclaim()
                });
                start.wait();
                let delivered: Vec<_> = read
                    .join()
                    .unwrap()
                    .into_iter()
                    .chain(reclaim.join().unwrap())
                    .collect();
                assert_eq!(delivered.len(), 1, "one recipient must own the input");
                assert_eq!(delivered[0].message.message_id, "raced");
            });
        }
    }
}
