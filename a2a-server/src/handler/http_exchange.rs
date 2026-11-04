// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! HTTP-only exchange routing, acknowledgements, and caller binding.

use super::delivery::request_task_id;
use super::protocol::{Rejection, records_message};
use super::uplink::{Incoming, IncomingRequest, LiveSessions, Receipt, Uplink, exchange_id};
use super::{DefaultRequestHandler, LIVE_UPLINK_CAPACITY};
use crate::middleware::ServiceParams;
use a2a::*;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

pub(super) enum HttpOpening {
    Attached {
        registration: RegistrationLease,
        uplink: Uplink,
    },
    Acknowledged(Box<Task>),
}

/// Registers only once the opening request has established the task. The
/// lease is not cloned: normal exit, cancellation, and unwinding all drop it.
pub(super) struct RegistrationLease {
    sessions: Arc<LiveSessions>,
    sender: mpsc::Sender<Incoming>,
    task_id: Option<TaskId>,
    exchange_id: Option<String>,
    user: Option<crate::middleware::User>,
}

impl RegistrationLease {
    pub(super) fn bind(&mut self, task_id: &str) {
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

pub(super) async fn open(
    handler: &DefaultRequestHandler,
    params: &ServiceParams,
    mut request: StreamRequest,
) -> Result<HttpOpening, A2AError> {
    let exchange_id = exchange_id(params)?;
    let bound_task = match exchange_id.as_deref() {
        Some(id) => handler.live.task_for_exchange(id),
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
            let message = handler
                .authorize_message(params, IncomingRequest::Raw(request), None)
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
            handler.authorize(params, task)?;
            let user = handler.authenticated_user(params)?;
            (IncomingRequest::Raw(request), user)
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
    if let Some(task) = route(handler, &request, exchange_id.as_deref(), &user).await? {
        return Ok(HttpOpening::Acknowledged(task));
    }
    let (sender, receiver) = mpsc::channel(LIVE_UPLINK_CAPACITY);
    let _ = sender
        .send(Incoming {
            request,
            receipt: None,
        })
        .await;
    Ok(HttpOpening::Attached {
        registration: RegistrationLease {
            sessions: handler.live.clone(),
            sender,
            task_id: None,
            exchange_id,
            user,
        },
        uplink: Uplink::Http(receiver),
    })
}

/// None means processing never started, so opening a new exchange is safe.
async fn route(
    handler: &DefaultRequestHandler,
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
    let Some((bound_task, session)) = handler.live.route(&task_id, exchange_id, user)? else {
        return Ok(None);
    };
    let IncomingRequest::Authorized(authorized) = request else {
        unreachable!("HTTP input was authorized before routing")
    };
    // An exchange identifier selects its bound task, independently of
    // the task named in this input. Its ACK must never disclose a task
    // authorized only by the credentials of an earlier request.
    if bound_task != task_id {
        handler.authorize(&authorized.params, Some(&bound_task))?;
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
            let task = handler.tasks.load(&bound_task).await?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::response::{Events, Run, RunCompletion};
    use crate::handler::test_support::*;
    use crate::handler::uplink::Receipt;
    use crate::handler::{delivery::StartMode, response_preparation::SubscriptionPolicy};
    use tokio::sync::oneshot;

    async fn bound_run(handler: &DefaultRequestHandler) -> Run {
        let params = ServiceParams::new();
        let HttpOpening::Attached {
            mut uplink,
            registration,
        } = open(
            handler,
            &params,
            StreamRequest {
                tenant: None,
                payload: StreamRequestPayload::SendMessage(send_req(None, None)),
            },
        )
        .await
        .unwrap()
        else {
            panic!("expected a new HTTP stream");
        };
        uplink.next().await.unwrap();
        Run::new(
            handler.clone(),
            params,
            handler.tasks.task("task"),
            RunCompletion::Exchange,
            SubscriptionPolicy::new(StartMode::Bidi, false),
        )
        .with_uplink(Some(uplink), Some(registration))
    }

    #[tokio::test]
    async fn dropping_a_run_unregisters_it_and_releases_queued_callers() {
        let handler = make_handler();
        let run = bound_run(&handler).await;
        let (_, sender) = handler.live.route("task", None, &None).unwrap().unwrap();
        let (receipt, result) = oneshot::channel();
        sender
            .send(Incoming {
                request: IncomingRequest::message(send_req(Some("task"), None)),
                receipt: Some(Receipt::new(receipt)),
            })
            .await
            .unwrap_or_else(|_| panic!("exchange must accept queued input"));
        drop(run);
        assert!(
            handler.live.open.read().is_empty(),
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
        let run = bound_run(&handler).await;
        let events: Events =
            futures::stream::once(async { panic!("response stream panicked") }).boxed();
        let mut output = run.start(None, Some(events), 0);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), output.next())
                .await
                .unwrap(),
            Some(Err(_))
        ));
        assert!(
            handler.live.open.read().is_empty(),
            "panic cleanup must unregister before sending an error"
        );
        assert!(output.next().await.is_none());
    }

    #[tokio::test]
    async fn a_second_subscription_cannot_reuse_an_open_http_exchange_identifier() {
        let handler = make_handler();
        seed_task(&handler, "task", "context").await;
        let params =
            ServiceParams::from([(a2a::SDK_LIVE_EXCHANGE_HEADER.into(), vec!["shared".into()])]);
        let subscribe = || StreamRequest {
            tenant: None,
            payload: StreamRequestPayload::Subscribe(SubscribeToTaskRequest {
                id: "task".into(),
                tenant: None,
                last_seen_generation: None,
            }),
        };
        let HttpOpening::Attached {
            mut registration,
            mut uplink,
        } = open(&handler, &params, subscribe()).await.unwrap()
        else {
            panic!("expected exchange");
        };
        registration.bind("task");
        let error = open(&handler, &params, subscribe())
            .await
            .err()
            .expect("identifier is already bound");
        assert_eq!(error.code, a2a::error_code::INVALID_REQUEST);
        assert_eq!(
            handler.live.task_for_exchange("shared").unwrap().as_deref(),
            Some("task")
        );
        drop(registration);
        uplink.close().await;
    }
}
