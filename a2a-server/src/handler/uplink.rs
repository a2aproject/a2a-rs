// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Adapt duplex streams and HTTP requests into the shared runner uplink.

use crate::middleware::{ServiceParams, User};
use a2a::*;
use futures::{FutureExt, StreamExt, stream::BoxStream};
use std::collections::HashMap;
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
// - Webhook deliveries are queued per task, in publish order, and never waited
//   for. A receiver that is down or slow delays only its own task's deliveries,
//   and when more than `PUSH_QUEUE_CAPACITY` are waiting the newest are dropped
//   (a client sees the gap in the generations and reads the task). With
//   `fail_on_error`, a failed delivery wakes its execution driver with an error;
//   an event that was already published is not rolled back.

/// Raw protocol input, or an HTTP message already authorized before routing.
pub(super) enum IncomingRequest {
    Raw(StreamRequest),
    Authorized(super::delivery::AuthorizedMessage),
}

impl IncomingRequest {
    pub(super) fn send_request(&self) -> Option<&SendMessageRequest> {
        match self {
            Self::Authorized(message) => Some(&message.request),
            Self::Raw(StreamRequest {
                payload: StreamRequestPayload::SendMessage(message),
                ..
            }) => Some(message),
            _ => None,
        }
    }
    #[cfg(test)]
    pub(super) fn message(request: SendMessageRequest) -> Self {
        Self::Raw(StreamRequest {
            tenant: request.tenant.clone(),
            payload: StreamRequestPayload::SendMessage(request),
        })
    }

    pub(super) fn reference_id(&self) -> Option<String> {
        let message = match self {
            Self::Authorized(message) => &message.request,
            Self::Raw(StreamRequest {
                payload: StreamRequestPayload::SendMessage(message),
                ..
            }) => message,
            Self::Raw(StreamRequest {
                payload: StreamRequestPayload::Subscribe(subscribe),
                ..
            }) => return Some(subscribe.id.clone()),
            _ => return None,
        };
        Some(message.message.message_id.clone())
            .filter(|id| !id.is_empty())
            .or_else(|| {
                message
                    .artifact_updates
                    .first()
                    .map(|update| update.artifact.artifact_id.clone())
            })
    }
}

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
    processing: bool,
}

impl Receipt {
    pub(super) fn new(sender: oneshot::Sender<Option<Task>>) -> Self {
        Self {
            sender: Some(sender),
            processing: false,
        }
    }

    pub(super) fn begin(mut self) -> Self {
        self.processing = true;
        self
    }

    pub(super) fn complete(mut self, task: Result<Task, A2AError>) {
        if let (Ok(task), Some(sender)) = (task, self.sender.take()) {
            let _ = sender.send(Some(task));
        }
    }
}

impl Drop for Receipt {
    fn drop(&mut self) {
        if !self.processing {
            if let Some(sender) = self.sender.take() {
                let _ = sender.send(None);
            }
        }
    }
}

/// Input to the common send entry point. A single request in Bidi mode is
/// an HTTP call; a stream supplies a native duplex uplink.
pub(super) enum ResponseInput {
    Request(Box<StreamRequest>),
    Stream(BoxStream<'static, StreamRequest>),
}

impl ResponseInput {
    pub(super) fn message(request: SendMessageRequest) -> Self {
        Self::Request(Box::new(StreamRequest {
            tenant: request.tenant.clone(),
            payload: StreamRequestPayload::SendMessage(request),
        }))
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

/// The open live sessions, by task, so a short-lived HTTP call can be routed to
/// the session whose downlink is already open.
#[derive(Default)]
pub(super) struct LiveSessions {
    pub(super) open: std::sync::Mutex<HashMap<TaskId, Vec<LiveSession>>>,
}

pub(super) struct LiveSession {
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
        return Err(A2AError::invalid_params(
            "invalid live exchange routing header",
        ));
    }
    Ok(Some(value.clone()))
}

impl LiveSessions {
    /// Recover the bound task before authorizing an HTTP uplink that omits it.
    pub(super) fn task_for_exchange(&self, id: &str) -> Result<Option<TaskId>, A2AError> {
        let open = self.lock();
        let mut tasks = open.iter().filter_map(|(task, sessions)| {
            sessions
                .iter()
                .any(|session| {
                    !session.sender.is_closed() && session.exchange_id.as_deref() == Some(id)
                })
                .then_some(task)
        });
        let task = tasks.next().cloned();
        if tasks.next().is_some() {
            return Err(A2AError::invalid_request(
                "ambiguous live exchange identifier",
            ));
        }
        Ok(task)
    }
    pub(super) fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<TaskId, Vec<LiveSession>>> {
        self.open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(super) fn register(
        &self,
        task_id: &str,
        sender: mpsc::Sender<Incoming>,
        exchange_id: Option<String>,
        user: Option<User>,
    ) {
        self.lock()
            .entry(task_id.to_string())
            .or_default()
            .push(LiveSession {
                sender,
                exchange_id,
                user,
            });
    }

    /// Route only to one explicitly identified exchange, or one unambiguous
    /// legacy exchange owned by the same trusted caller. Never pick the newest.
    pub(super) fn route(
        &self,
        task_id: &str,
        exchange_id: Option<&str>,
        user: &Option<User>,
    ) -> Result<Option<(TaskId, mpsc::Sender<Incoming>)>, A2AError> {
        let open = self.lock();
        let mut candidates = open.iter().flat_map(|(task, sessions)| {
            sessions
                .iter()
                .filter(move |session| {
                    !session.sender.is_closed()
                        && match exchange_id {
                            Some(id) => session.exchange_id.as_deref() == Some(id),
                            None => {
                                task == task_id
                                    && session.exchange_id.is_none()
                                    && &session.user == user
                            }
                        }
                })
                .map(move |session| (task, session))
        });
        let Some((bound_task, session)) = candidates.next() else {
            return Ok(None);
        };
        if candidates.next().is_some() {
            return Err(A2AError::invalid_request(
                "ambiguous live exchange; use a distinct exchange routing identifier",
            ));
        }
        if &session.user != user {
            return Err(A2AError::invalid_request(
                "live exchange belongs to a different caller",
            ));
        }
        Ok(Some((bound_task.clone(), session.sender.clone())))
    }

    pub(super) fn unregister(&self, task_id: &str, sender: &mpsc::Sender<Incoming>) {
        let mut open = self.lock();
        if let Some(senders) = open.get_mut(task_id) {
            senders.retain(|current| {
                !current.sender.same_channel(sender) && !current.sender.is_closed()
            });
            if senders.is_empty() {
                open.remove(task_id);
            }
        }
    }
}

/// The answer to one `SendLiveMessage` call over a transport that has no
/// native duplex (A2A 1.1 §3.1.12): the call that opens the exchange is the
/// downlink, and a later call for the same task only gets the task state back.
#[non_exhaustive]
pub enum LiveResponse {
    /// This call is the downlink: the events, until the exchange ends.
    Stream(BoxStream<'static, Result<StreamResponse, A2AError>>),
    /// This call was input for an exchange that is already open; here is the
    /// task as it stands. Its events arrive on the downlink, never here.
    Ack(Box<Task>),
}
