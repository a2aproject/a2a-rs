// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! HTTP-only exchange routing, acknowledgements, and caller binding.

use super::delivery::{AuthorizedMessage, MessageInput};
use super::protocol::{
    IncomingRequest, ProtocolError, Rejection, records_message, request_task_id,
};
use super::uplink::{HttpLiveResponses, Incoming, Receipt, Uplink, exchange_id};
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

/// Reserves routing before admission and binds it once the task is established.
/// Normal exit, failed opening, cancellation, and unwinding release the lease.
pub(super) struct RegistrationLease {
    responses: Arc<HttpLiveResponses>,
    sender: mpsc::Sender<Incoming>,
}

impl RegistrationLease {
    pub(super) fn bind(&self, task_id: &str) {
        self.responses.bind(&self.sender, task_id);
    }
}

impl Drop for RegistrationLease {
    fn drop(&mut self) {
        self.responses.unregister(&self.sender);
    }
}

pub(super) async fn open(
    handler: &DefaultRequestHandler,
    params: &ServiceParams,
    request: StreamRequest,
) -> Result<HttpOpening, A2AError> {
    let exchange_id = exchange_id(params)?;
    let bound_task = match exchange_id.as_deref() {
        Some(id) => handler.http_live_responses.task_for_exchange(id),
        None => Ok(None),
    };
    let (request, user) = match IncomingRequest::from(request) {
        IncomingRequest::Message(mut input) => {
            if let Ok(Some(task)) = &bound_task
                && request_task_id(input.request()).is_none()
            {
                input.request_mut().message.task_id = Some(task.clone());
            }
            let message = handler
                .authorize_message(params, *input, None)
                .map_err(Rejection::into_error)?;
            let user = message.user.clone();
            (
                IncomingRequest::Message(Box::new(MessageInput::Authorized(message))),
                user,
            )
        }
        request => {
            // Subscription authorization precedes identity resolution.
            let task = match &request {
                IncomingRequest::Subscribe(request) => Some(request.id.as_str()),
                _ => None,
            };
            handler.authorize(params, task)?;
            (request, handler.authenticated_user(params)?)
        }
    };
    // Report registry conflicts only after checking credentials.
    if bound_task?.is_some() && matches!(request, IncomingRequest::Subscribe(_)) {
        return Err(ProtocolError::ExchangeIdentifierInUse.into());
    }
    if let IncomingRequest::Message(input) = &request
        && let MessageInput::Authorized(message) = input.as_ref()
        && let Some(task) = route(handler, message, exchange_id.as_deref()).await?
    {
        return Ok(HttpOpening::Acknowledged(task));
    }
    let (sender, receiver) = mpsc::channel(LIVE_UPLINK_CAPACITY);
    handler
        .http_live_responses
        .register(sender.clone(), exchange_id, user)?;
    let registration = RegistrationLease {
        responses: handler.http_live_responses.clone(),
        sender: sender.clone(),
    };
    let _ = sender
        .send(Incoming {
            request,
            receipt: None,
        })
        .await;
    Ok(HttpOpening::Attached {
        registration,
        uplink: Uplink::Http(receiver),
    })
}

/// None means processing never started, so opening a new exchange is safe.
async fn route(
    handler: &DefaultRequestHandler,
    authorized: &AuthorizedMessage,
    exchange_id: Option<&str>,
) -> Result<Option<Box<Task>>, A2AError> {
    let message = &authorized.request;
    let Some(task_id) = request_task_id(message) else {
        return Ok(None);
    };
    let Some((bound_task, session)) =
        handler
            .http_live_responses
            .route(&task_id, exchange_id, &authorized.user)?
    else {
        return Ok(None);
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
            request: IncomingRequest::Message(Box::new(MessageInput::Authorized(
                AuthorizedMessage {
                    request: authorized.request.clone(),
                    params: authorized.params.clone(),
                    user: authorized.user.clone(),
                    response_id: None,
                },
            ))),
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
    use crate::handler::protocol::{ResponsePolicy, StartMode};
    use crate::handler::response::{ResponseStream, Run};
    use crate::handler::test_support::*;
    use crate::handler::uplink::Receipt;
    use std::sync::atomic::AtomicUsize;
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
            ResponsePolicy::new(StartMode::Bidi, false),
        )
        .with_uplink(Some(uplink), Some(registration))
    }

    #[tokio::test]
    async fn dropping_a_run_unregisters_it_and_releases_queued_callers() {
        let handler = make_handler();
        let run = bound_run(&handler).await;
        let (_, sender) = handler
            .http_live_responses
            .route("task", None, &None)
            .unwrap()
            .unwrap();
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
            handler.http_live_responses.open.read().is_empty(),
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
        let events: ResponseStream =
            futures::stream::once(async { panic!("response stream panicked") }).boxed();
        let mut output = run.start(None, Some(events), 0);
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(2), output.next())
                .await
                .unwrap(),
            Some(Err(_))
        ));
        assert!(
            handler.http_live_responses.open.read().is_empty(),
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
            registration,
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
            handler
                .http_live_responses
                .task_for_exchange("shared")
                .unwrap()
                .as_deref(),
            Some("task")
        );
        drop(registration);
        uplink.close().await;
    }

    #[tokio::test]
    async fn a_pending_opening_reserves_its_exchange_identifier() {
        let handler = make_handler();
        let params =
            ServiceParams::from([(a2a::SDK_LIVE_EXCHANGE_HEADER.into(), vec!["shared".into()])]);
        let request = || StreamRequest {
            tenant: None,
            payload: StreamRequestPayload::SendMessage(send_req(None, None)),
        };
        let first = open(&handler, &params, request()).await.unwrap();
        let denied = handler
            .clone()
            .with_authorizer(|_: &ServiceParams, _: Option<&str>| {
                Err(A2AError::invalid_request("credentials rejected"))
            });
        assert_eq!(
            open(&denied, &params, request())
                .await
                .err()
                .unwrap()
                .message,
            "credentials rejected",
            "authorization must precede reporting a pending ID conflict"
        );
        let error = open(&handler, &params, request())
            .await
            .err()
            .expect("an opening must reserve its ID before task admission");
        assert_eq!(error.code, a2a::error_code::INVALID_REQUEST);
        assert!(
            handler
                .http_live_responses
                .task_for_exchange("shared")
                .is_err(),
            "pending exchanges must not infer a task or open another response"
        );
        drop(first);
        let replacement = open(&handler, &params, request()).await.unwrap();
        drop(replacement);
        assert!(handler.http_live_responses.open.read().is_empty());
    }

    #[derive(Clone, Default)]
    struct PausedReadStore {
        inner: Arc<InMemoryTaskStore>,
        entered: Arc<Notify>,
        release: Arc<Notify>,
        reads: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl TaskStore for PausedReadStore {
        async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.inner.create(task).await
        }
        async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.inner.update(task).await
        }
        async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
            if self.reads.fetch_add(1, Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.get(id).await
        }
        async fn list(&self, request: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
            self.inner.list(request).await
        }
        async fn begin_cancel(&self, id: &str) -> Result<Task, A2AError> {
            self.inner.begin_cancel(id).await
        }
    }

    #[tokio::test]
    async fn a_competing_http_opening_is_rejected_before_task_admission() {
        let store = PausedReadStore::default();
        let handler = DefaultRequestHandler::new(EchoExecutor, store.clone()).with_capabilities(
            AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            },
        );
        let params = ServiceParams::from([
            (SVC_PARAM_VERSION.into(), vec!["1.1".into()]),
            (SDK_LIVE_EXCHANGE_HEADER.into(), vec!["shared".into()]),
        ]);
        let request = || StreamRequest {
            tenant: None,
            payload: StreamRequestPayload::SendMessage(send_req(None, None)),
        };
        let first = tokio::spawn({
            let handler = handler.clone();
            let params = params.clone();
            let request = request();
            async move { handler.send_live_request(&params, request).await }
        });
        timeout(Duration::from_secs(2), store.entered.notified())
            .await
            .unwrap();
        let error = timeout(
            Duration::from_secs(2),
            handler.send_live_request(&params, request()),
        )
        .await
        .unwrap()
        .err()
        .expect("the first opening owns this identifier");
        assert_eq!(error.code, error_code::INVALID_REQUEST);
        assert_eq!(
            store.reads.load(Ordering::SeqCst),
            1,
            "the competing request must not reach admission"
        );
        store.release.notify_one();
        let super::super::LiveResponse::Stream(mut stream) = timeout(Duration::from_secs(2), first)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
        else {
            panic!("the owning opening must succeed")
        };
        assert!(matches!(
            timeout(Duration::from_secs(2), stream.next())
                .await
                .unwrap(),
            Some(Ok(StreamResponse::Task(_)))
        ));
        drop(stream);
        handler.shutdown().await;
        assert!(handler.http_live_responses.open.read().is_empty());
    }

    #[tokio::test]
    async fn a_failed_opening_releases_its_reserved_identifier() {
        let handler = make_handler();
        let params = ServiceParams::from([
            (SVC_PARAM_VERSION.into(), vec!["1.1".into()]),
            (SDK_LIVE_EXCHANGE_HEADER.into(), vec!["shared".into()]),
        ]);
        let error = handler
            .send_live_request(
                &params,
                StreamRequest {
                    tenant: None,
                    payload: StreamRequestPayload::SendMessage(send_req(Some("missing"), None)),
                },
            )
            .await
            .err()
            .expect("the task does not exist");
        assert_eq!(error.code, error_code::TASK_NOT_FOUND);
        assert!(handler.http_live_responses.open.read().is_empty());
        let response = handler
            .send_live_request(
                &params,
                StreamRequest {
                    tenant: None,
                    payload: StreamRequestPayload::SendMessage(send_req(None, None)),
                },
            )
            .await
            .unwrap();
        assert!(matches!(response, super::super::LiveResponse::Stream(_)));
        drop(response);
        handler.shutdown().await;
        assert!(handler.http_live_responses.open.read().is_empty());
    }

    #[tokio::test]
    async fn canceling_an_opening_releases_its_pending_identifier() {
        let store = PausedReadStore::default();
        let handler = DefaultRequestHandler::new(EchoExecutor, store.clone()).with_capabilities(
            AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            },
        );
        let params = ServiceParams::from([
            (SVC_PARAM_VERSION.into(), vec!["1.1".into()]),
            (SDK_LIVE_EXCHANGE_HEADER.into(), vec!["shared".into()]),
        ]);
        let first = tokio::spawn({
            let handler = handler.clone();
            let params = params.clone();
            async move {
                handler
                    .send_live_request(
                        &params,
                        StreamRequest {
                            tenant: None,
                            payload: StreamRequestPayload::SendMessage(send_req(None, None)),
                        },
                    )
                    .await
            }
        });
        timeout(Duration::from_secs(2), store.entered.notified())
            .await
            .unwrap();
        first.abort();
        assert!(first.await.is_err());
        assert!(handler.http_live_responses.open.read().is_empty());
        let replacement = open(
            &handler,
            &params,
            StreamRequest {
                tenant: None,
                payload: StreamRequestPayload::SendMessage(send_req(None, None)),
            },
        )
        .await
        .expect("cancellation must release the ID even while owned admission continues");
        drop(replacement);
        store.release.notify_one();
        handler.shutdown().await;
        assert!(handler.http_live_responses.open.read().is_empty());
    }
}
