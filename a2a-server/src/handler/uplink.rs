// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Adapt duplex streams and HTTP requests into the shared runner uplink.

use super::delivery::MessageInput;
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

/// Protocol payload classified once at the native-stream or HTTP boundary.
pub(super) enum IncomingRequest {
    Message(Box<MessageInput>),
    Subscribe(SubscribeToTaskRequest),
    Invalid,
}

impl From<StreamRequest> for IncomingRequest {
    fn from(request: StreamRequest) -> Self {
        match request.payload {
            StreamRequestPayload::SendMessage(request) => {
                Self::Message(Box::new(MessageInput::Raw(request)))
            }
            StreamRequestPayload::Subscribe(request) => Self::Subscribe(request),
            _ => Self::Invalid,
        }
    }
}

impl IncomingRequest {
    /// Only the opening accepts subscriptions; follow-ups must be messages.
    pub(super) fn into_message(self) -> Result<MessageInput, A2AError> {
        match self {
            Self::Message(message) => Ok(*message),
            Self::Subscribe(_) => Err(A2AError::invalid_request(
                "subscribe is only valid as the first request of a stream",
            )),
            Self::Invalid => Err(A2AError::invalid_request(
                "unrecognised StreamRequest payload",
            )),
        }
    }

    #[cfg(test)]
    pub(super) fn message(request: SendMessageRequest) -> Self {
        Self::Message(Box::new(MessageInput::Raw(request)))
    }

    pub(super) fn reference_id(&self) -> Option<String> {
        let message = match self {
            Self::Message(input) => input.request(),
            Self::Subscribe(subscribe) => return Some(subscribe.id.clone()),
            Self::Invalid => return None,
        };
        message
            .message
            .as_ref()
            .map(|message| message.message_id.clone())
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

/// Open HTTP live responses, grouped by task. Follow-up calls are routed to
/// the input queue of a response whose downlink is already open.
#[derive(Default)]
pub(super) struct HttpLiveResponses {
    pub(super) open: parking_lot::RwLock<HashMap<TaskId, Vec<HttpLiveResponse>>>,
}

pub(super) struct HttpLiveResponse {
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

impl HttpLiveResponses {
    /// Recover the bound task before authorizing an HTTP uplink that omits it.
    pub(super) fn task_for_exchange(&self, id: &str) -> Result<Option<TaskId>, A2AError> {
        let open = self.open.read();
        let mut tasks = open.iter().filter_map(|(task, responses)| {
            responses
                .iter()
                .any(|response| {
                    !response.sender.is_closed() && response.exchange_id.as_deref() == Some(id)
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
    pub(super) fn register(
        &self,
        task_id: &str,
        sender: mpsc::Sender<Incoming>,
        exchange_id: Option<String>,
        user: Option<User>,
    ) {
        self.open
            .write()
            .entry(task_id.to_string())
            .or_default()
            .push(HttpLiveResponse {
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
        let open = self.open.read();
        let mut candidates = open.iter().flat_map(|(task, responses)| {
            responses
                .iter()
                .filter(move |response| {
                    !response.sender.is_closed()
                        && match exchange_id {
                            Some(id) => response.exchange_id.as_deref() == Some(id),
                            None => {
                                task == task_id
                                    && response.exchange_id.is_none()
                                    && &response.user == user
                            }
                        }
                })
                .map(move |response| (task, response))
        });
        let Some((bound_task, response)) = candidates.next() else {
            return Ok(None);
        };
        if candidates.next().is_some() {
            return Err(A2AError::invalid_request(
                "ambiguous live exchange; use a distinct exchange routing identifier",
            ));
        }
        if &response.user != user {
            return Err(A2AError::invalid_request(
                "live exchange belongs to a different caller",
            ));
        }
        Ok(Some((bound_task.clone(), response.sender.clone())))
    }

    pub(super) fn unregister(&self, task_id: &str, sender: &mpsc::Sender<Incoming>) {
        let mut open = self.open.write();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_registrations_cannot_route_an_ambiguous_exchange_identifier() {
        let responses = HttpLiveResponses::default();
        let (first, _rx1) = mpsc::channel(1);
        let (second, _rx2) = mpsc::channel(1);
        responses.register("first", first.clone(), Some("shared".into()), None);
        responses.register("second", second, Some("shared".into()), None);
        let error = responses.task_for_exchange("shared").unwrap_err();
        assert_eq!(error.code, a2a::error_code::INVALID_REQUEST);
        assert!(error.message.contains("ambiguous"));
        responses.unregister("first", &first);
        assert_eq!(
            responses.task_for_exchange("shared").unwrap().as_deref(),
            Some("second")
        );
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
