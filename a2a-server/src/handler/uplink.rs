// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Adapt duplex streams and HTTP requests into the shared runner uplink.

use super::protocol::{IncomingRequest, ProtocolError};
use crate::middleware::{ServiceParams, User};
use a2a::*;
use futures::{FutureExt, StreamExt, stream::BoxStream};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

// Live (bidirectional) exchanges: `SendLiveMessage` (A2A 1.1 §3.1.12).
//
// A live exchange is a session bound to one task. Its uplink carries the
// client's [`StreamRequest`]s and its downlink the task's events. The session
// decides, per message, whether the task is running (the message is appended
// under the task's writer lock and handed to the executor) or idle (the
// message starts a new turn), and it keeps the downlink open across turns:
// only a terminal task, a client that goes away, or an uplink that ends with
// nothing left to say closes it.
//
// # Known limitations
//
// - Legacy HTTP callers without an SDK exchange identifier can join only an
//   unambiguous legacy exchange for their authenticated caller. Independent
//   callers sharing the same identity must use exchange identifiers.
// - A call lost to a panic is acknowledged when its message is in the task,
//   even if no executor read it; an artifact-only call, or one with no
//   message id, cannot be checked and is acknowledged.
// - A message injected into a turn another client started reaches an idle
//   exchange through the next snapshot, not as events.
// - A stream that falls further behind than the execution buffer holds
//   (`EXECUTION_BUFFER_CAPACITY` events) starts over from the task as it
//   stands, a snapshot with a gap in the generations, as for any subscription.
// - Without replay, `lastSeenGeneration` resumes from a snapshot.
// - Webhook deliveries are queued per task, in publish order. A full task queue
//   applies backpressure to that task; notifications are not dropped. One shared
//   dispatcher rotates ready tasks within its HTTP concurrency bound. With
//   `fail_on_error`, a failed delivery wakes its execution driver with an error;
//   an event that was already published is not rolled back.

/// Normalized input consumed by the runner, with an optional processing receipt.
pub(super) struct Incoming {
    pub(super) request: IncomingRequest,
    /// Completed with the current task after processing, when requested.
    pub(super) receipt: Option<Receipt>,
}

/// An optional processing receipt. Dropping a queued receipt reports that
/// nothing was handled; dropping one after begin reports an unknown outcome.
/// These states let the adapter retry safely without involving the runner.
pub(super) struct Receipt {
    sender: Option<oneshot::Sender<Option<Task>>>,
    state: ReceiptState,
}

#[derive(Clone)]
pub(super) struct ReceiptState(Arc<std::sync::atomic::AtomicU8>);

impl ReceiptState {
    pub(super) fn cancel_pending(&self) -> bool {
        self.0
            .compare_exchange(
                0,
                2,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_ok()
    }
}

impl Receipt {
    pub(super) fn new(sender: oneshot::Sender<Option<Task>>) -> Self {
        Self {
            sender: Some(sender),
            state: ReceiptState(Arc::new(std::sync::atomic::AtomicU8::new(0))),
        }
    }

    pub(super) fn state(&self) -> ReceiptState {
        self.state.clone()
    }

    pub(super) fn begin(self) -> Option<Self> {
        self.state
            .0
            .compare_exchange(
                0,
                1,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()?;
        Some(self)
    }

    pub(super) fn complete(mut self, task: Result<Task, A2AError>) {
        if let (Ok(task), Some(sender)) = (task, self.sender.take()) {
            let _ = sender.send(Some(task));
        }
    }
}

impl Drop for Receipt {
    fn drop(&mut self) {
        if self.state.0.load(std::sync::atomic::Ordering::Acquire) == 0 {
            if let Some(sender) = self.sender.take() {
                let _ = sender.send(None);
            }
        }
    }
}

/// The client's side of an exchange as the session reads it.
pub(super) enum Uplink {
    /// Calls arriving one at a time over HTTP, through a channel the session
    /// can close.
    Http(mpsc::Receiver<Incoming>),
    /// A stream of requests (gRPC, SLIMRPC).
    Stream(BoxStream<'static, Incoming>),
}

impl Uplink {
    pub(super) fn duplex(requests: BoxStream<'static, StreamRequest>) -> Self {
        Self::Stream(
            requests
                .map(|request| Incoming {
                    request: IncomingRequest::from(request),
                    receipt: None,
                })
                .boxed(),
        )
    }

    pub(super) async fn next(&mut self) -> Option<Incoming> {
        match self {
            Uplink::Http(receiver) => receiver.recv().await,
            Uplink::Stream(stream) => stream.next().await,
        }
    }

    /// Refuses any further call and answers the ones queued but never reached,
    /// so a caller that got its call in just as the exchange ended is told it
    /// was not carried out (and may open a new exchange with it) instead of
    /// waiting for an answer that will not come.
    pub(super) async fn close(&mut self) {
        match self {
            Uplink::Http(receiver) => {
                // After `close` nothing more can be sent, and what was sent
                // before it is still received here.
                receiver.close();
                // Dropping an item answers its caller "not reached".
                while receiver.recv().await.is_some() {}
            }
            Uplink::Stream(stream) => while let Some(Some(_)) = stream.next().now_or_never() {},
        }
    }
}

/// Reserved HTTP live responses. Exchange IDs are unique per trusted caller;
/// follow-up calls can route only after the response is bound to its task.
#[derive(Default)]
pub(super) struct HttpLiveResponses {
    pub(super) open: parking_lot::RwLock<Vec<HttpLiveResponse>>,
}

pub(super) struct HttpLiveResponse {
    task_id: Option<TaskId>,
    exchange_id: Option<String>,
    user: Option<User>,
    sender: mpsc::Sender<Incoming>,
}

/// Optional SDK correlation, never an authentication credential.
pub(super) fn exchange_id(params: &ServiceParams) -> Result<Option<String>, A2AError> {
    let mut values = params
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(SDK_LIVE_EXCHANGE_HEADER))
        .flat_map(|(_, values)| values);
    let Some(value) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some()
        || value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(ProtocolError::InvalidExchangeRoutingHeader.into());
    }
    Ok(Some(value.clone()))
}

impl HttpLiveResponses {
    /// Recover the bound task before authorizing an HTTP uplink that omits it.
    pub(super) fn task_for_exchange(
        &self,
        id: &str,
        user: &Option<User>,
    ) -> Result<Option<TaskId>, A2AError> {
        let open = self.open.read();
        let Some(response) = open.iter().find(|response| {
            !response.sender.is_closed()
                && response.exchange_id.as_deref() == Some(id)
                && &response.user == user
        }) else {
            return Ok(None);
        };
        response
            .task_id
            .clone()
            .map(Some)
            .ok_or_else(|| ProtocolError::ExchangeIdentifierInUse.into())
    }

    /// Claim the caller's optional identifier before admission mutates a task.
    pub(super) fn register(
        &self,
        sender: mpsc::Sender<Incoming>,
        exchange_id: Option<String>,
        user: Option<User>,
    ) -> Result<(), A2AError> {
        let mut open = self.open.write();
        open.retain(|response| !response.sender.is_closed());
        if exchange_id.is_some()
            && open
                .iter()
                .any(|response| response.exchange_id == exchange_id && response.user == user)
        {
            return Err(ProtocolError::ExchangeIdentifierInUse.into());
        }
        open.push(HttpLiveResponse {
            task_id: None,
            sender,
            exchange_id,
            user,
        });
        Ok(())
    }

    pub(super) fn bind(&self, sender: &mpsc::Sender<Incoming>, task_id: &str) {
        if let Some(response) = self
            .open
            .write()
            .iter_mut()
            .find(|response| response.sender.same_channel(sender))
        {
            response.task_id.get_or_insert_with(|| task_id.to_owned());
        }
    }

    /// Route only to one explicitly identified exchange, or one unambiguous
    /// legacy exchange owned by the same trusted caller. Never pick the newest.
    pub(super) fn route(
        &self,
        task_id: &str,
        exchange_id: Option<&str>,
        user: &Option<User>,
    ) -> Result<Option<(TaskId, mpsc::Sender<Incoming>)>, A2AError> {
        let open = self.open.read();
        let mut candidates = open.iter().filter(|response| {
            !response.sender.is_closed()
                && match exchange_id {
                    Some(id) => {
                        response.exchange_id.as_deref() == Some(id) && &response.user == user
                    }
                    None => {
                        response.task_id.as_deref() == Some(task_id)
                            && response.exchange_id.is_none()
                            && &response.user == user
                    }
                }
        });
        let Some(response) = candidates.next() else {
            return Ok(None);
        };
        if candidates.next().is_some() {
            return Err(ProtocolError::AmbiguousExchange.into());
        }
        if &response.user != user {
            return Err(ProtocolError::ForeignExchangeCaller.into());
        }
        let bound_task = response
            .task_id
            .as_ref()
            .ok_or(ProtocolError::ExchangeIdentifierInUse)?;
        Ok(Some((bound_task.clone(), response.sender.clone())))
    }

    pub(super) fn unregister(&self, sender: &mpsc::Sender<Incoming>) {
        self.open
            .write()
            .retain(|current| !current.sender.same_channel(sender) && !current.sender.is_closed());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn different_callers_can_independently_use_the_same_exchange_identifier() {
        let responses = HttpLiveResponses::default();
        let alice = Some(User::authenticated("alice"));
        let bob = Some(User::authenticated("bob"));
        let (a, _a) = mpsc::channel(1);
        let (b, _b) = mpsc::channel(1);
        responses
            .register(a.clone(), Some("shared".into()), alice.clone())
            .unwrap();
        responses.bind(&a, "alice-task");
        responses
            .register(b.clone(), Some("shared".into()), bob.clone())
            .unwrap();
        responses.bind(&b, "bob-task");
        assert_eq!(
            responses
                .task_for_exchange("shared", &alice)
                .unwrap()
                .as_deref(),
            Some("alice-task")
        );
        assert_eq!(
            responses
                .task_for_exchange("shared", &bob)
                .unwrap()
                .as_deref(),
            Some("bob-task")
        );
        let (_, route) = responses
            .route("bob-task", Some("shared"), &bob)
            .unwrap()
            .unwrap();
        assert!(route.same_channel(&b));
        assert!(
            responses
                .route("alice-task", Some("shared"), &None)
                .unwrap()
                .is_none()
        );
        assert!(responses.register(a, Some("shared".into()), alice).is_err());
    }

    #[test]
    fn duplicate_identifiers_are_rejected_without_displacing_the_owner() {
        let responses = HttpLiveResponses::default();
        let (first, _rx1) = mpsc::channel(1);
        let (second, _rx2) = mpsc::channel(1);
        responses
            .register(first.clone(), Some("shared".into()), None)
            .unwrap();
        responses.bind(&first, "first");
        let error = responses
            .register(second.clone(), Some("shared".into()), None)
            .unwrap_err();
        assert_eq!(error.code, a2a::error_code::INVALID_REQUEST);
        assert_eq!(
            responses
                .task_for_exchange("shared", &None)
                .unwrap()
                .as_deref(),
            Some("first")
        );
        responses.unregister(&first);
        responses
            .register(second.clone(), Some("shared".into()), None)
            .unwrap();
        responses.bind(&second, "second");
        assert_eq!(
            responses
                .task_for_exchange("shared", &None)
                .unwrap()
                .as_deref(),
            Some("second")
        );
    }

    #[test]
    fn simultaneous_registrations_have_only_one_identifier_owner() {
        let responses = std::sync::Arc::new(HttpLiveResponses::default());
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let contenders: Vec<_> = (0..16)
            .map(|_| {
                let responses = responses.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let (sender, receiver) = mpsc::channel(1);
                    barrier.wait();
                    let result = responses.register(sender, Some("shared".into()), None);
                    // Keep the winning receiver open until all contenders finish.
                    (result, receiver)
                })
            })
            .collect();
        let results: Vec<_> = contenders
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();
        assert_eq!(
            results.iter().filter(|(result, _)| result.is_ok()).count(),
            1
        );
        assert_eq!(responses.open.read().len(), 1);
        for error in results
            .iter()
            .filter_map(|(result, _)| result.as_ref().err())
        {
            assert_eq!(error.code, a2a::error_code::INVALID_REQUEST);
        }
    }

    #[test]
    fn releasing_a_closed_owner_does_not_remove_its_identifier_replacement() {
        let responses = HttpLiveResponses::default();
        let (first, receiver) = mpsc::channel(1);
        responses
            .register(first.clone(), Some("shared".into()), None)
            .unwrap();
        responses.bind(&first, "first");
        drop(receiver);
        let (second, _receiver) = mpsc::channel(1);
        responses
            .register(second.clone(), Some("shared".into()), None)
            .unwrap();
        responses.bind(&second, "second");
        responses.unregister(&first);
        let (task, sender) = responses
            .route("second", Some("shared"), &None)
            .unwrap()
            .unwrap();
        assert_eq!(task, "second");
        assert!(sender.same_channel(&second));
    }

    #[test]
    fn exchange_headers_reject_ambiguous_or_invalid_routing_values() {
        for values in [
            vec!["".to_string()],
            vec!["contains spaces".to_string()],
            vec!["x".repeat(129)],
            vec!["non-ascii-☃".to_string()],
            vec!["first".to_string(), "second".to_string()],
        ] {
            let params =
                ServiceParams::from([(a2a::SDK_LIVE_EXCHANGE_HEADER.to_ascii_uppercase(), values)]);
            assert_eq!(
                exchange_id(&params).unwrap_err().code,
                a2a::error_code::INVALID_PARAMS
            );
        }
        let params = ServiceParams::from([(
            a2a::SDK_LIVE_EXCHANGE_HEADER.into(),
            vec!["safe_A-1".into()],
        )]);
        assert_eq!(exchange_id(&params).unwrap().as_deref(), Some("safe_A-1"));
    }
}
