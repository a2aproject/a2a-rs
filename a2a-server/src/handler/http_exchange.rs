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
    let scope_user = handler.authenticated_user(params)?;
    let bound_task = match exchange_id.as_deref() {
        Some(id) => handler
            .http_live_responses
            .task_for_exchange(id, &scope_user),
        None => Ok(None),
    };
    let (request, user) = match IncomingRequest::from(request) {
        IncomingRequest::Message(mut input) => {
            if let Ok(Some(task)) = &bound_task
                && request_task_id(input.request()).is_none()
            {
                if let Some(message) = &mut input.request_mut().message {
                    message.task_id = Some(task.clone());
                }
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
    let deadline = tokio::time::Instant::now() + handler.http_followup_timeout;
    let receipt = Receipt::new(sender);
    let state = receipt.state();
    let permit = tokio::time::timeout_at(deadline, session.reserve_owned())
        .await
        .map_err(|_| {
            A2AError::invalid_request("live input queue timed out; input was not admitted")
        })?;
    let sent = match permit {
        Ok(permit) => {
            permit.send(Incoming {
                request: IncomingRequest::Message(Box::new(MessageInput::Authorized(
                    AuthorizedMessage {
                        receipt: Some(state.clone()),
                        request: authorized.request.clone(),
                        params: authorized.params.clone(),
                        user: authorized.user.clone(),
                        response_id: None,
                    },
                ))),
                receipt: Some(receipt),
            });
            true
        }
        Err(_) => false,
    };
    if !sent {
        return Ok(None);
    }
    match tokio::time::timeout_at(deadline, acknowledged).await {
        Ok(Ok(task)) => Ok(task.map(Box::new)),
        Err(_) => {
            if state.cancel_pending() {
                return Err(A2AError::invalid_request(
                    "live acknowledgement timed out; input was not admitted",
                ));
            }
            match state.committed_task() {
                Some(task) => Ok(Some(Box::new(task))),
                None => Err(A2AError::internal(
                    "live acknowledgement timed out; input outcome is unknown; inspect the task before retrying",
                )),
            }
        }
        Ok(Err(_)) => {
            // Cache publication can trail persistence. Only storage or the
            // receipt written immediately after a successful save proves commit.
            let committed = state.committed_task();
            let task =
                match tokio::time::timeout_at(deadline, handler.tasks.load(&bound_task)).await {
                    Ok(Ok(task)) => task,
                    _ if committed.is_some() => committed.clone().unwrap(),
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        return Err(A2AError::internal(
                            "live receipt recovery timed out; input outcome is unknown",
                        ));
                    }
                };
            let raw = StreamRequest {
                tenant: message.tenant.clone(),
                payload: StreamRequestPayload::SendMessage(message.clone()),
            };
            if committed.is_none() && !records_message(&task, &raw) {
                return Err(A2AError::internal(
                    "the live exchange failed while handling the request; input outcome is unknown",
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

    #[tokio::test]
    async fn an_http_deadline_revokes_a_request_that_has_not_started() {
        let handler = make_handler().with_http_followup_timeout(Duration::from_millis(1));
        seed_task(&handler, "task", "context").await;
        let params = ServiceParams::new();
        let HttpOpening::Attached {
            registration,
            mut uplink,
        } = open(
            &handler,
            &params,
            StreamRequest {
                tenant: None,
                payload: StreamRequestPayload::Subscribe(SubscribeToTaskRequest {
                    id: "task".into(),
                    tenant: None,
                    last_seen_generation: None,
                }),
            },
        )
        .await
        .unwrap()
        else {
            panic!("opening")
        };
        registration.bind("task");
        uplink.next().await.unwrap();
        let error = open(
            &handler,
            &params,
            StreamRequest {
                tenant: None,
                payload: StreamRequestPayload::SendMessage(send_req(Some("task"), Some("context"))),
            },
        )
        .await
        .err()
        .unwrap();
        assert!(error.message.contains("not admitted"));
        let queued = uplink.next().await.unwrap();
        assert!(
            queued.receipt.unwrap().begin().is_none(),
            "a timed-out pending input cannot later be admitted"
        );
        drop(registration);
    }

    #[tokio::test]
    async fn an_owned_http_deadline_requires_evidence_of_commit() {
        for committed in [false, true] {
            let handler = make_handler().with_http_followup_timeout(Duration::from_millis(10));
            seed_task(&handler, "task", "context").await;
            let source = handler.tasks.task("task");
            source.set_snapshot(handler.tasks.load("task").await.unwrap());
            let params = ServiceParams::new();
            let HttpOpening::Attached {
                registration,
                mut uplink,
            } = open(
                &handler,
                &params,
                StreamRequest {
                    tenant: None,
                    payload: StreamRequestPayload::Subscribe(SubscribeToTaskRequest {
                        id: "task".into(),
                        tenant: None,
                        last_seen_generation: None,
                    }),
                },
            )
            .await
            .unwrap()
            else {
                panic!("opening")
            };
            registration.bind("task");
            uplink.next().await.unwrap();
            let request = StreamRequest {
                tenant: None,
                payload: StreamRequestPayload::SendMessage(send_req(Some("task"), Some("context"))),
            };
            let (response, _) = tokio::join!(open(&handler, &params, request), async {
                let queued = uplink.next().await.unwrap();
                let owned = queued.receipt.unwrap().begin().unwrap();
                if committed {
                    owned.state().committed(&source.snapshot().unwrap());
                }
                let _owned = owned;
                tokio::time::sleep(Duration::from_millis(20)).await;
            });
            if committed {
                assert!(
                    matches!(response.unwrap(), HttpOpening::Acknowledged(task) if task.id == "task")
                );
            } else {
                assert!(
                    response
                        .err()
                        .unwrap()
                        .message
                        .contains("outcome is unknown")
                );
            }
            drop(registration);
        }
    }

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
                .task_for_exchange("shared", &None)
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
                .task_for_exchange("shared", &None)
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
    fn routed_input(
        handler: &DefaultRequestHandler,
        capacity: usize,
        exchange: Option<&str>,
    ) -> (AuthorizedMessage, mpsc::Receiver<Incoming>) {
        let (sender, receiver) = mpsc::channel(capacity);
        handler
            .http_live_responses
            .register(sender.clone(), exchange.map(str::to_owned), None)
            .unwrap();
        handler.http_live_responses.bind(&sender, "task");
        (
            AuthorizedMessage {
                receipt: None,
                request: send_req(Some("task"), Some("context")),
                params: ServiceParams::new(),
                user: None,
                response_id: None,
            },
            receiver,
        )
    }

    #[tokio::test]
    async fn a_lost_processing_receipt_recovers_only_recorded_input() {
        for cached in [false, true] {
            for recorded in [false, true] {
                let handler = make_handler();
                seed_task(&handler, "task", "context").await;
                let (input, mut receiver) = routed_input(&handler, 1, None);
                let mut snapshot = handler.tasks.load("task").await.unwrap();
                if recorded {
                    snapshot.history = Some(vec![input.request.message.clone().unwrap()]);
                    handler
                        .tasks
                        .store()
                        .update(snapshot.clone())
                        .await
                        .unwrap();
                }
                let source = handler.tasks.task("task");
                if cached {
                    let mut stale = snapshot.clone();
                    // Deliberately disagree with storage in both directions.
                    stale.history = if recorded {
                        None
                    } else {
                        Some(vec![input.request.message.clone().unwrap()])
                    };
                    source.set_snapshot(stale);
                }
                let (result, ()) = tokio::join!(route(&handler, &input, None), async {
                    // A panic/disconnect after begin loses the receipt; it
                    // cannot make already committed input eligible for replay.
                    drop(
                        receiver
                            .recv()
                            .await
                            .unwrap()
                            .receipt
                            .unwrap()
                            .begin()
                            .unwrap(),
                    );
                });
                if recorded {
                    assert_eq!(*result.unwrap().unwrap(), snapshot);
                } else {
                    assert!(
                        result
                            .unwrap_err()
                            .message
                            .contains("failed while handling")
                    );
                }
                handler.shutdown().await;
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn lost_receipt_storage_recovery_has_a_deadline() {
        let handler = DefaultRequestHandler::new(EchoExecutor, PausedReadStore::default())
            .with_http_followup_timeout(Duration::from_secs(1));
        let (input, mut receiver) = routed_input(&handler, 1, None);
        let (result, ()) = tokio::join!(route(&handler, &input, None), async {
            drop(
                receiver
                    .recv()
                    .await
                    .unwrap()
                    .receipt
                    .unwrap()
                    .begin()
                    .unwrap(),
            );
        });
        assert!(result.unwrap_err().message.contains("recovery timed out"));
        handler.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn explicit_commit_evidence_survives_a_lost_receipt_and_unavailable_storage() {
        let handler = DefaultRequestHandler::new(EchoExecutor, PausedReadStore::default())
            .with_http_followup_timeout(Duration::from_secs(1));
        let (input, mut receiver) = routed_input(&handler, 1, None);
        let snapshot = task_with_history("task", "context", 0);
        let (result, ()) = tokio::join!(route(&handler, &input, None), async {
            let receipt = receiver.recv().await.unwrap().receipt.unwrap();
            receipt.state().committed(&snapshot);
            drop(receipt.begin().unwrap());
        });
        assert_eq!(*result.unwrap().unwrap(), snapshot);
        handler.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_full_http_queue_times_out_without_admitting_another_input() {
        let handler = make_handler().with_http_followup_timeout(Duration::from_secs(1));
        let (input, mut receiver) = routed_input(&handler, 1, None);
        let (_, sender) = handler
            .http_live_responses
            .route("task", None, &None)
            .unwrap()
            .unwrap();
        sender
            .try_send(Incoming {
                request: IncomingRequest::message(input.request.clone()),
                receipt: None,
            })
            .unwrap_or_else(|_| panic!("empty queue"));
        let error = route(&handler, &input, None).await.unwrap_err();
        assert!(error.message.contains("queue timed out"));
        assert!(receiver.try_recv().is_ok());
        assert!(
            receiver.try_recv().is_err(),
            "timed-out input was never enqueued"
        );
        handler.shutdown().await;
    }

    #[tokio::test]
    async fn a_queue_closed_while_reserving_reports_that_processing_never_started() {
        let handler = make_handler();
        let (input, mut receiver) = routed_input(&handler, 1, None);
        let (_, sender) = handler
            .http_live_responses
            .route("task", None, &None)
            .unwrap()
            .unwrap();
        sender
            .try_send(Incoming {
                request: IncomingRequest::message(input.request.clone()),
                receipt: None,
            })
            .unwrap_or_else(|_| panic!("empty queue"));
        let (result, ()) = tokio::join!(route(&handler, &input, None), async {
            receiver.close();
        });
        assert!(
            result.unwrap().is_none(),
            "a new exchange can safely retry this input"
        );
        handler.shutdown().await;
    }
    #[tokio::test(start_paused = true)]
    async fn an_owned_http_timeout_cannot_fabricate_a_missing_snapshot() {
        let handler = make_handler().with_http_followup_timeout(Duration::from_secs(1));
        let (input, mut receiver) = routed_input(&handler, 1, None);
        let (result, ()) = tokio::join!(route(&handler, &input, None), async {
            let _receipt = receiver
                .recv()
                .await
                .unwrap()
                .receipt
                .unwrap()
                .begin()
                .unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await;
        });
        assert!(result.unwrap_err().message.contains("outcome is unknown"));
        handler.shutdown().await;
    }

    struct DenyTask;
    impl crate::RequestAuthorizer for DenyTask {
        fn authorize(&self, _: &ServiceParams, task: Option<&str>) -> Result<(), A2AError> {
            assert!(task.is_none() || task == Some("task"));
            Err(A2AError::invalid_request(
                "credential cannot access bound task",
            ))
        }
    }

    #[tokio::test]
    async fn a_cross_task_route_reauthorizes_the_task_an_ack_would_disclose() {
        let handler = make_handler().with_authorizer(DenyTask);
        let (mut input, _receiver) = routed_input(&handler, 1, Some("bound"));
        input.request.message.as_mut().unwrap().task_id = Some("other".into());
        let error = route(&handler, &input, Some("bound")).await.unwrap_err();
        assert!(error.message.contains("credential"));
        handler.shutdown().await;
    }

    #[tokio::test]
    async fn an_unknown_http_opening_still_requires_authorization() {
        let handler = make_handler().with_authorizer(DenyTask);
        let error = open(
            &handler,
            &ServiceParams::new(),
            StreamRequest {
                tenant: None,
                payload: StreamRequestPayload::Unrecognised,
            },
        )
        .await
        .err()
        .unwrap();
        assert!(error.message.contains("credential"));
        assert!(handler.http_live_responses.open.read().is_empty());
        handler.shutdown().await;
    }
}
