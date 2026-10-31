// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Adapt transport calls to an exchange with owned routing and receipt lifetimes.

use super::delivery::{StartMode, request_task_id};
use super::protocol::{Rejection, records_message};
use super::uplink::{
    Incoming, IncomingRequest, LiveSessions, Receipt, ResponseInput, Uplink, exchange_id,
};
use super::{DefaultRequestHandler, LIVE_UPLINK_CAPACITY};
use crate::middleware::ServiceParams;
use a2a::*;
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub(super) enum OpenExchange {
    Attached(Exchange),
    Acknowledged(Box<Task>),
}

/// The runner consumes this interface without knowing the transport shape.
pub(super) struct Exchange {
    // Drop routing before the receiver, so new calls cannot join a dead exchange.
    registration: Option<RegistrationLease>,
    uplink: Uplink,
}

impl Exchange {
    pub(super) async fn next(&mut self) -> Option<Incoming> {
        self.uplink.next().await
    }

    pub(super) fn bind(&mut self, task_id: &str) {
        if let Some(registration) = &mut self.registration {
            registration.bind(task_id);
        }
    }

    pub(super) async fn close(mut self) {
        drop(self.registration.take());
        self.uplink.close().await;
    }
}

/// Registers only once the opening request has established the task. The
/// lease is not cloned: normal exit, cancellation, and unwinding all drop it.
struct RegistrationLease {
    sessions: Arc<LiveSessions>,
    sender: mpsc::Sender<Incoming>,
    task_id: Option<TaskId>,
    exchange_id: Option<String>,
    user: Option<crate::middleware::User>,
}

impl RegistrationLease {
    fn bind(&mut self, task_id: &str) {
        if self.task_id.is_none() {
            self.sessions.register(
                task_id,
                self.sender.clone(),
                self.exchange_id.clone(),
                self.user.clone(),
            );
            self.task_id = Some(task_id.to_string());
        }
    }
}

impl Drop for RegistrationLease {
    fn drop(&mut self) {
        if let Some(task_id) = &self.task_id {
            self.sessions.unregister(task_id, &self.sender);
        }
    }
}

pub(super) struct ExchangeAdapter<'a> {
    handler: &'a DefaultRequestHandler,
    params: &'a ServiceParams,
}

impl<'a> ExchangeAdapter<'a> {
    pub(super) fn new(handler: &'a DefaultRequestHandler, params: &'a ServiceParams) -> Self {
        Self { handler, params }
    }

    pub(super) async fn open(
        self,
        input: ResponseInput,
        mode: StartMode,
    ) -> Result<OpenExchange, A2AError> {
        let exchange = match input {
            ResponseInput::Request(mut request) if mode == StartMode::Bidi => {
                let exchange_id = exchange_id(self.params)?;
                let bound_task = match exchange_id.as_deref() {
                    Some(id) => self.handler.live.task_for_exchange(id),
                    None => Ok(None),
                };
                if let (Ok(Some(task)), StreamRequestPayload::SendMessage(message)) =
                    (&bound_task, &mut request.payload)
                    && request_task_id(message).is_none()
                {
                    message.message.task_id = Some(task.clone());
                }
                let (request, user) = match &request.payload {
                    StreamRequestPayload::SendMessage(_) => {
                        let message = self
                            .handler
                            .authorize_message(self.params, IncomingRequest::Raw(*request), None)
                            .map_err(Rejection::into_error)?;
                        let user = message.user.clone();
                        (IncomingRequest::Authorized(message), user)
                    }
                    _ => {
                        // Subscription authorization precedes identity resolution.
                        let task = match &request.payload {
                            StreamRequestPayload::Subscribe(request) => Some(request.id.as_str()),
                            _ => None,
                        };
                        self.handler.authorize(self.params, task)?;
                        let user = self.handler.authenticated_user(self.params)?;
                        (IncomingRequest::Raw(*request), user)
                    }
                };
                // Report registry conflicts only after checking credentials.
                if bound_task?.is_some()
                    && matches!(
                        &request,
                        IncomingRequest::Raw(StreamRequest {
                            payload: StreamRequestPayload::Subscribe(_),
                            ..
                        })
                    )
                {
                    return Err(A2AError::invalid_request(
                        "live exchange identifier is already in use",
                    ));
                }
                if let Some(task) = self.route(&request, exchange_id.as_deref(), &user).await? {
                    return Ok(OpenExchange::Acknowledged(task));
                }
                let (sender, receiver) = mpsc::channel(LIVE_UPLINK_CAPACITY);
                let _ = sender
                    .send(Incoming {
                        request,
                        receipt: None,
                    })
                    .await;
                Exchange {
                    registration: Some(RegistrationLease {
                        sessions: self.handler.live.clone(),
                        sender,
                        task_id: None,
                        exchange_id,
                        user,
                    }),
                    uplink: Uplink::Http(receiver),
                }
            }
            ResponseInput::Request(request) => {
                let item = Incoming {
                    request: IncomingRequest::Raw(*request),
                    receipt: None,
                };
                Exchange {
                    registration: None,
                    uplink: Uplink::Stream(futures::stream::once(async move { item }).boxed()),
                }
            }
            ResponseInput::Stream(stream) => {
                let input = stream
                    .map(|request| Incoming {
                        request: IncomingRequest::Raw(request),
                        receipt: None,
                    })
                    .boxed();
                Exchange {
                    registration: None,
                    uplink: Uplink::Stream(input),
                }
            }
        };
        Ok(OpenExchange::Attached(exchange))
    }

    /// None means processing never started, so opening a new exchange is safe.
    async fn route(
        &self,
        request: &IncomingRequest,
        exchange_id: Option<&str>,
        user: &Option<crate::middleware::User>,
    ) -> Result<Option<Box<Task>>, A2AError> {
        let Some(message) = request.send_request() else {
            return Ok(None);
        };
        let Some(task_id) = request_task_id(message) else {
            return Ok(None);
        };
        let Some((bound_task, session)) = self.handler.live.route(&task_id, exchange_id, user)?
        else {
            return Ok(None);
        };
        let IncomingRequest::Authorized(authorized) = request else {
            unreachable!("HTTP input was authorized before routing")
        };
        // An exchange identifier selects its bound task, independently of
        // the task named in this input. Its ACK must never disclose a task
        // authorized only by the credentials of an earlier request.
        if bound_task != task_id {
            self.handler
                .authorize(&authorized.params, Some(&bound_task))?;
        }
        let (sender, acknowledged) = oneshot::channel();
        let sent = session
            .send(Incoming {
                request: IncomingRequest::Authorized(super::delivery::AuthorizedMessage {
                    request: authorized.request.clone(),
                    params: authorized.params.clone(),
                    user: authorized.user.clone(),
                    response_id: None,
                }),
                receipt: Some(Receipt::new(sender)),
            })
            .await
            .is_ok();
        drop(session);
        if !sent {
            return Ok(None);
        }
        match acknowledged.await {
            Ok(task) => Ok(task.map(Box::new)),
            Err(_) => {
                // Processing began but its receipt was lost. Recover from
                // storage instead of delivering possibly recorded input twice.
                let task = self.handler.load_task(&bound_task).await?;
                let raw = StreamRequest {
                    tenant: message.tenant.clone(),
                    payload: StreamRequestPayload::SendMessage(message.clone()),
                };
                if !records_message(&task, &raw) {
                    return Err(A2AError::internal(
                        "the live exchange failed while handling the request",
                    ));
                }
                Ok(Some(Box::new(task)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::response::{Events, Run, RunCompletion};
    use crate::handler::test_support::*;

    async fn bound_exchange(handler: &DefaultRequestHandler) -> Exchange {
        let params = ServiceParams::new();
        let OpenExchange::Attached(mut exchange) = ExchangeAdapter::new(handler, &params)
            .open(
                ResponseInput::message(send_req(None, None)),
                StartMode::Bidi,
            )
            .await
            .unwrap()
        else {
            panic!("expected a new exchange")
        };
        exchange.next().await.unwrap();
        exchange.bind("task");
        exchange
    }

    #[tokio::test]
    async fn dropping_an_exchange_unregisters_it_and_releases_queued_callers() {
        let handler = make_handler();
        let exchange = bound_exchange(&handler).await;
        let (_, sender) = handler.live.route("task", None, &None).unwrap().unwrap();
        let (receipt, result) = oneshot::channel();
        sender
            .send(Incoming {
                request: IncomingRequest::message(send_req(Some("task"), None)),
                receipt: Some(Receipt::new(receipt)),
            })
            .await
            .unwrap_or_else(|_| panic!("exchange must accept queued input"));
        drop(exchange);
        assert!(
            handler.live.lock().is_empty(),
            "the lease must remove the registry entry"
        );
        assert!(
            tokio::time::timeout(Duration::from_secs(2), result)
                .await
                .unwrap()
                .unwrap()
                .is_none(),
            "a queued caller must be told processing never started"
        );
    }

    #[tokio::test]
    async fn runner_panic_releases_the_lease_before_reporting_the_error() {
        let handler = make_handler();
        let exchange = bound_exchange(&handler).await;
        let events: Events =
            futures::stream::once(async { panic!("response stream panicked") }).boxed();
        let mut output = Run::new(
            handler.clone(),
            ServiceParams::new(),
            "task".into(),
            RunCompletion::Exchange,
        )
        .start(None, Some(events), 0, Some(exchange));
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), output.next())
                .await
                .unwrap(),
            Some(Err(_))
        ));
        assert!(
            handler.live.lock().is_empty(),
            "panic cleanup must unregister before sending an error"
        );
        assert!(output.next().await.is_none());
    }
}
