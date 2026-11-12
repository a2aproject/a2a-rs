// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use a2a::{A2AError, StreamResponse};
use a2a_pb::pbconv;
use a2a_pb::proto;
use a2a_pb::proto::a2a_service_server::A2aService;
use a2a_server::RequestHandler;
use futures::{FutureExt, StreamExt};
use tokio_stream::Stream;

use crate::errors::a2a_error_to_status;

type BoxStreamResult<T> = Pin<Box<dyn Stream<Item = Result<T, tonic::Status>> + Send + 'static>>;

const DEFAULT_LIVE_RECOVERY_TIMEOUT: Duration = Duration::from_secs(30);

struct UplinkFailure {
    status: tonic::Status,
    had_input: bool,
    deadline: tokio::time::Instant,
}

impl UplinkFailure {
    fn new(status: tonic::Status, had_input: bool, recovery_timeout: Duration) -> Self {
        Self {
            status,
            had_input,
            // Share one grace period between finishing admission and obtaining
            // its first output, even for custom handlers that never complete.
            deadline: tokio::time::Instant::now() + recovery_timeout,
        }
    }
}

/// Tonic decoding failures are fatal transport errors, not a normal half-close.
/// Keep the error alongside the normalized input so it can reach the downlink
/// even when the handler is still waiting for its first request.
fn decode_live_uplink(
    input: impl Stream<Item = Result<proto::StreamRequest, tonic::Status>> + Send + 'static,
    recovery_timeout: Duration,
) -> (
    futures::stream::BoxStream<'static, a2a::StreamRequest>,
    futures::future::BoxFuture<'static, UplinkFailure>,
) {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let input = input
        .scan((Some(sender), false), move |(sender, had_input), item| {
            futures::future::ready(match item {
                Ok(request) => {
                    *had_input = true;
                    Some(pbconv::from_proto_stream_request(&request))
                }
                Err(status) => {
                    if let Some(sender) = sender.take() {
                        let _ =
                            sender.send(UplinkFailure::new(status, *had_input, recovery_timeout));
                    }
                    None
                }
            })
        })
        .fuse()
        .boxed();
    let fault = async move {
        match receiver.await {
            Ok(error) => error,
            Err(_) => futures::future::pending().await,
        }
    }
    .boxed();
    (input, fault)
}

struct LiveDownlinkState {
    events: futures::stream::BoxStream<'static, Result<StreamResponse, A2AError>>,
    fault: futures::future::BoxFuture<'static, UplinkFailure>,
    failure: Option<UplinkFailure>,
    remaining: usize,
    delivered: bool,
}

fn live_downlink(
    events: futures::stream::BoxStream<'static, Result<StreamResponse, A2AError>>,
    fault: futures::future::BoxFuture<'static, UplinkFailure>,
) -> BoxStreamResult<proto::StreamResponse> {
    let state = LiveDownlinkState {
        events,
        fault,
        failure: None,
        remaining: 128,
        delivered: false,
    };
    futures::stream::unfold(Some(state), |state| async move {
        let mut state = state?;
        loop {
            if let Some(failure) = state.failure.take() {
                // A received request may have committed before its opening
                // snapshot is polled. Preserve that response before the fault.
                let event = if state.remaining == 0 {
                    None
                } else if failure.had_input && !state.delivered {
                    tokio::time::timeout_at(failure.deadline, state.events.next())
                        .await
                        .ok()
                        .flatten()
                } else {
                    state.events.next().now_or_never().flatten()
                };
                if let Some(event) = event {
                    state.delivered = true;
                    state.remaining -= 1;
                    state.failure = Some(failure);
                    return Some((map_proto_stream_item(event), Some(state)));
                }
                return Some((Err(failure.status), None));
            }
            tokio::select! {
                biased;
                failure = &mut state.fault => state.failure = Some(failure),
                event = state.events.next() => {
                    let event = event?;
                    state.delivered = true;
                    return Some((map_proto_stream_item(event), Some(state)));
                },
            }
        }
    })
    .boxed()
}

async fn open_live_downlink(
    opening: impl std::future::Future<
        Output = Result<
            futures::stream::BoxStream<'static, Result<StreamResponse, A2AError>>,
            A2AError,
        >,
    >,
    mut fault: futures::future::BoxFuture<'static, UplinkFailure>,
) -> Result<BoxStreamResult<proto::StreamResponse>, tonic::Status> {
    tokio::pin!(opening);
    let events = tokio::select! {
        biased;
        // Prefer an already-open response so its ready events can drain.
        events = &mut opening => match events {
            Ok(events) => events,
            Err(error) => return Err((&mut fault).now_or_never().map(|failure| failure.status).unwrap_or_else(|| a2a_error_to_status(&error))),
        },
        failure = &mut fault => {
            if !failure.had_input { return Err(failure.status); }
            // Admission owns the first request. A later malformed frame must
            // not drop its opening while persistence is still completing.
            match tokio::time::timeout_at(failure.deadline, opening).await {
                Ok(Ok(events)) => return Ok(live_downlink(events, futures::future::ready(failure).boxed())),
                Ok(Err(_)) | Err(_) => return Err(failure.status),
            }

        },
    };
    Ok(live_downlink(events, fault))
}

/// gRPC server handler — wraps a `RequestHandler` as a tonic gRPC service.
///
/// This bridges the transport-agnostic `RequestHandler` interface to the
/// tonic-generated `A2AService` server trait. Proto ↔ native type conversions
/// are handled through `a2a_pb::pbconv`.
pub struct GrpcHandler<H: RequestHandler> {
    handler: Arc<H>,
    live_recovery_timeout: Duration,
}

impl<H: RequestHandler> GrpcHandler<H> {
    pub fn new(handler: Arc<H>) -> Self {
        GrpcHandler {
            handler,
            live_recovery_timeout: DEFAULT_LIVE_RECOVERY_TIMEOUT,
        }
    }

    /// Bound recovery after malformed live input (30 seconds by default).
    /// Admission and its first response share this grace period. Expiry returns
    /// the original transport fault; healthy exchanges have no such deadline.
    pub fn with_live_recovery_timeout(mut self, timeout: Duration) -> Self {
        self.live_recovery_timeout = timeout;
        self
    }
}

/// Extract service params from gRPC request metadata.
fn extract_service_params(
    metadata: &tonic::metadata::MetadataMap,
) -> a2a_server::middleware::ServiceParams {
    let mut params = std::collections::HashMap::new();
    for kv in metadata.iter() {
        match kv {
            tonic::metadata::KeyAndValueRef::Ascii(key, value) => {
                if let Ok(v) = value.to_str() {
                    params
                        .entry(key.as_str().to_string())
                        .or_insert_with(Vec::new)
                        .push(v.to_string());
                }
            }
            tonic::metadata::KeyAndValueRef::Binary(_, _) => {}
        }
    }
    params
}

/// §3.6.2 for every gRPC method, on the `A2A-Version` read from metadata
/// (§10.2) -- the same check the HTTP bindings apply.
#[allow(clippy::result_large_err)]
fn validate_protocol_version(
    params: &a2a_server::middleware::ServiceParams,
) -> Result<(), tonic::Status> {
    a2a_server::middleware::check_a2a_version(params).map_err(|error| a2a_error_to_status(&error))
}

#[allow(clippy::result_large_err)]
fn map_proto_stream_item(
    item: Result<StreamResponse, A2AError>,
) -> Result<proto::StreamResponse, tonic::Status> {
    match item {
        Ok(response) => Ok(pbconv::to_proto_stream_response(&response)),
        Err(error) => Err(a2a_error_to_status(&error)),
    }
}

#[tonic::async_trait]
impl<H: RequestHandler> A2aService for GrpcHandler<H> {
    async fn send_message(
        &self,
        request: tonic::Request<proto::SendMessageRequest>,
    ) -> Result<tonic::Response<proto::SendMessageResponse>, tonic::Status> {
        let params = extract_service_params(request.metadata());
        validate_protocol_version(&params)?;
        let native_req = pbconv::from_proto_send_message_request(request.get_ref());
        let result = self
            .handler
            .send_message(&params, native_req)
            .await
            .map_err(|e| a2a_error_to_status(&e))?;
        Ok(tonic::Response::new(
            pbconv::to_proto_send_message_response(&result),
        ))
    }

    type SendStreamingMessageStream = BoxStreamResult<proto::StreamResponse>;

    async fn send_streaming_message(
        &self,
        request: tonic::Request<proto::SendMessageRequest>,
    ) -> Result<tonic::Response<Self::SendStreamingMessageStream>, tonic::Status> {
        let params = extract_service_params(request.metadata());
        validate_protocol_version(&params)?;
        let native_req = pbconv::from_proto_send_message_request(request.get_ref());
        let stream = self
            .handler
            .send_streaming_message(&params, native_req)
            .await
            .map_err(|e| a2a_error_to_status(&e))?;
        let proto_stream = stream.map(map_proto_stream_item);
        Ok(tonic::Response::new(Box::pin(proto_stream)))
    }

    async fn get_task(
        &self,
        request: tonic::Request<proto::GetTaskRequest>,
    ) -> Result<tonic::Response<proto::Task>, tonic::Status> {
        let params = extract_service_params(request.metadata());
        validate_protocol_version(&params)?;
        let native_req = pbconv::from_proto_get_task_request(request.get_ref());
        let result = self
            .handler
            .get_task(&params, native_req)
            .await
            .map_err(|e| a2a_error_to_status(&e))?;
        Ok(tonic::Response::new(pbconv::to_proto_task(&result)))
    }

    async fn list_tasks(
        &self,
        request: tonic::Request<proto::ListTasksRequest>,
    ) -> Result<tonic::Response<proto::ListTasksResponse>, tonic::Status> {
        let params = extract_service_params(request.metadata());
        validate_protocol_version(&params)?;
        let native_req = pbconv::from_proto_list_tasks_request(request.get_ref());
        let result = self
            .handler
            .list_tasks(&params, native_req)
            .await
            .map_err(|e| a2a_error_to_status(&e))?;
        Ok(tonic::Response::new(pbconv::to_proto_list_tasks_response(
            &result,
        )))
    }

    async fn cancel_task(
        &self,
        request: tonic::Request<proto::CancelTaskRequest>,
    ) -> Result<tonic::Response<proto::Task>, tonic::Status> {
        let params = extract_service_params(request.metadata());
        validate_protocol_version(&params)?;
        let native_req = pbconv::from_proto_cancel_task_request(request.get_ref());
        let result = self
            .handler
            .cancel_task(&params, native_req)
            .await
            .map_err(|e| a2a_error_to_status(&e))?;
        Ok(tonic::Response::new(pbconv::to_proto_task(&result)))
    }

    type SubscribeToTaskStream = BoxStreamResult<proto::StreamResponse>;

    async fn subscribe_to_task(
        &self,
        request: tonic::Request<proto::SubscribeToTaskRequest>,
    ) -> Result<tonic::Response<Self::SubscribeToTaskStream>, tonic::Status> {
        let params = extract_service_params(request.metadata());
        validate_protocol_version(&params)?;
        let native_req = pbconv::from_proto_subscribe_to_task_request(request.get_ref());
        let stream = self
            .handler
            .subscribe_to_task(&params, native_req)
            .await
            .map_err(|e| a2a_error_to_status(&e))?;
        let proto_stream = stream.map(map_proto_stream_item);
        Ok(tonic::Response::new(Box::pin(proto_stream)))
    }

    type SendLiveMessageStream = BoxStreamResult<proto::StreamResponse>;

    async fn send_live_message(
        &self,
        request: tonic::Request<tonic::Streaming<proto::StreamRequest>>,
    ) -> Result<tonic::Response<Self::SendLiveMessageStream>, tonic::Status> {
        let params = extract_service_params(request.metadata());
        validate_protocol_version(&params)?;
        let (uplink, fault) = decode_live_uplink(request.into_inner(), self.live_recovery_timeout);
        let events =
            open_live_downlink(self.handler.send_live_message(&params, uplink), fault).await?;
        Ok(tonic::Response::new(events))
    }

    async fn create_task_push_notification_config(
        &self,
        request: tonic::Request<proto::TaskPushNotificationConfig>,
    ) -> Result<tonic::Response<proto::TaskPushNotificationConfig>, tonic::Status> {
        let params = extract_service_params(request.metadata());
        validate_protocol_version(&params)?;
        let native_req =
            pbconv::from_proto_create_task_push_notification_config_request(request.get_ref());
        let result = self
            .handler
            .create_push_config(&params, native_req)
            .await
            .map_err(|e| a2a_error_to_status(&e))?;
        Ok(tonic::Response::new(
            pbconv::to_proto_task_push_notification_config(&result),
        ))
    }

    async fn get_task_push_notification_config(
        &self,
        request: tonic::Request<proto::GetTaskPushNotificationConfigRequest>,
    ) -> Result<tonic::Response<proto::TaskPushNotificationConfig>, tonic::Status> {
        let params = extract_service_params(request.metadata());
        validate_protocol_version(&params)?;
        let native_req =
            pbconv::from_proto_get_task_push_notification_config_request(request.get_ref());
        let result = self
            .handler
            .get_push_config(&params, native_req)
            .await
            .map_err(|e| a2a_error_to_status(&e))?;
        Ok(tonic::Response::new(
            pbconv::to_proto_task_push_notification_config(&result),
        ))
    }

    async fn list_task_push_notification_configs(
        &self,
        request: tonic::Request<proto::ListTaskPushNotificationConfigsRequest>,
    ) -> Result<tonic::Response<proto::ListTaskPushNotificationConfigsResponse>, tonic::Status>
    {
        let params = extract_service_params(request.metadata());
        validate_protocol_version(&params)?;
        let native_req =
            pbconv::from_proto_list_task_push_notification_configs_request(request.get_ref());
        let result = self
            .handler
            .list_push_configs(&params, native_req)
            .await
            .map_err(|e| a2a_error_to_status(&e))?;
        Ok(tonic::Response::new(
            pbconv::to_proto_list_task_push_notification_configs_response(&result),
        ))
    }

    async fn get_extended_agent_card(
        &self,
        request: tonic::Request<proto::GetExtendedAgentCardRequest>,
    ) -> Result<tonic::Response<proto::AgentCard>, tonic::Status> {
        let params = extract_service_params(request.metadata());
        validate_protocol_version(&params)?;
        let native_req = pbconv::from_proto_get_extended_agent_card_request(request.get_ref());
        let result = self
            .handler
            .get_extended_agent_card(&params, native_req)
            .await
            .map_err(|e| a2a_error_to_status(&e))?;
        Ok(tonic::Response::new(pbconv::to_proto_agent_card(&result)))
    }

    async fn delete_task_push_notification_config(
        &self,
        request: tonic::Request<proto::DeleteTaskPushNotificationConfigRequest>,
    ) -> Result<tonic::Response<()>, tonic::Status> {
        let params = extract_service_params(request.metadata());
        validate_protocol_version(&params)?;
        let native_req =
            pbconv::from_proto_delete_task_push_notification_config_request(request.get_ref());
        self.handler
            .delete_push_config(&params, native_req)
            .await
            .map_err(|e| a2a_error_to_status(&e))?;
        Ok(tonic::Response::new(()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn malformed_live_uplink_stops_input_and_reports_original_status() {
        let frame = proto::StreamRequest::default();
        let (mut input, fault) = decode_live_uplink(
            futures::stream::iter([
                Ok(frame.clone()),
                Err(tonic::Status::data_loss("bad protobuf frame")),
                Ok(frame),
            ]),
            DEFAULT_LIVE_RECOVERY_TIMEOUT,
        );
        assert!(input.next().await.is_some());
        assert!(input.next().await.is_none());
        assert!(input.next().await.is_none());
        let mut output = live_downlink(futures::stream::empty().boxed(), fault);
        let error = output.next().await.unwrap().unwrap_err();
        assert_eq!(error.code(), tonic::Code::DataLoss);
        assert_eq!(error.message(), "bad protobuf frame");
        assert!(output.next().await.is_none());
    }

    #[tokio::test]
    async fn malformed_frame_drains_ready_output_before_original_transport_error() {
        let event = StreamResponse::Message(a2a::Message::new(
            a2a::Role::Agent,
            vec![a2a::Part::text("earlier output")],
        ));
        for infinite in [false, true] {
            let events = if infinite {
                futures::stream::repeat(Ok(event.clone())).boxed()
            } else {
                futures::stream::iter([Ok(event.clone())]).boxed()
            };
            let fault = futures::future::ready(UplinkFailure::new(
                tonic::Status::data_loss("bad protobuf frame"),
                false,
                DEFAULT_LIVE_RECOVERY_TIMEOUT,
            ))
            .boxed();
            let mut output = live_downlink(events, fault);
            let mut received = 0;
            loop {
                match output.next().await.unwrap() {
                    Ok(_) => received += 1,
                    Err(error) => {
                        assert_eq!(error.code(), tonic::Code::DataLoss);
                        assert_eq!(error.message(), "bad protobuf frame");
                        break;
                    }
                }
            }
            assert_eq!(received, if infinite { 128 } else { 1 });
            assert!(output.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn opening_fault_does_not_wait_for_unrelated_executor_work() {
        let error = open_live_downlink(
            futures::future::pending(),
            futures::future::ready(UplinkFailure::new(
                tonic::Status::data_loss("broken input"),
                false,
                DEFAULT_LIVE_RECOVERY_TIMEOUT,
            ))
            .boxed(),
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.code(), tonic::Code::DataLoss);
        let opening = futures::future::ready(Err(A2AError::invalid_request("empty clean uplink")));
        let error = open_live_downlink(opening, futures::future::pending().boxed())
            .await
            .err()
            .unwrap();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }

    #[tokio::test]
    async fn an_opening_ready_after_the_fault_still_drains_its_ready_output() {
        let mut polled = false;
        let opening = futures::future::poll_fn(move |_| {
            if !polled {
                polled = true;
                return std::task::Poll::Pending;
            }
            let events = futures::stream::iter([Ok(StreamResponse::Message(a2a::Message::new(
                a2a::Role::Agent,
                vec![a2a::Part::text("ready reply")],
            )))])
            .boxed();
            std::task::Poll::Ready(Ok(events))
        });
        let mut events = open_live_downlink(
            opening,
            futures::future::ready(UplinkFailure::new(
                tonic::Status::data_loss("broken input"),
                true,
                DEFAULT_LIVE_RECOVERY_TIMEOUT,
            ))
            .boxed(),
        )
        .await
        .unwrap();
        assert!(events.next().await.unwrap().is_ok());
        assert_eq!(
            events.next().await.unwrap().unwrap_err().code(),
            tonic::Code::DataLoss
        );
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn a_later_malformed_frame_preserves_a_pending_admitted_opening() {
        let frame = proto::StreamRequest::default();
        let (mut input, fault) = decode_live_uplink(
            futures::stream::iter([Ok(frame), Err(tonic::Status::data_loss("bad later frame"))]),
            DEFAULT_LIVE_RECOVERY_TIMEOUT,
        );
        let opening = async move {
            assert!(input.next().await.is_some());
            assert!(input.next().await.is_none());
            // Admission and then its first response need additional polls.
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            Ok(futures::stream::once(async {
                tokio::task::yield_now().await;
                Ok(StreamResponse::Task(a2a::Task {
                    id: "committed".into(),
                    context_id: "context".into(),
                    generation: 1,
                    timeline: None,
                    status: a2a::TaskStatus {
                        state: a2a::TaskState::Working,
                        message: None,
                        timestamp: None,
                    },
                    artifacts: None,
                    history: None,
                    metadata: None,
                }))
            })
            .boxed())
        };
        let mut events = open_live_downlink(opening, fault).await.unwrap();
        assert!(events.next().await.unwrap().is_ok());
        assert_eq!(
            events.next().await.unwrap().unwrap_err().code(),
            tonic::Code::DataLoss
        );
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn malformed_input_bounds_pending_opening_and_first_output() {
        fn expired_failure() -> UplinkFailure {
            let mut failure = UplinkFailure::new(
                tonic::Status::data_loss("bad later frame"),
                true,
                DEFAULT_LIVE_RECOVERY_TIMEOUT,
            );
            failure.deadline = tokio::time::Instant::now();
            failure
        }
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            open_live_downlink(
                futures::future::pending(),
                futures::future::ready(expired_failure()).boxed(),
            ),
        )
        .await
        .unwrap()
        .err()
        .unwrap();
        assert_eq!(error.code(), tonic::Code::DataLoss);
        assert_eq!(error.message(), "bad later frame");

        let mut events = live_downlink(
            futures::stream::pending().boxed(),
            futures::future::ready(expired_failure()).boxed(),
        );
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::DataLoss);
        assert_eq!(error.message(), "bad later frame");
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn malformed_first_frame_preserves_transport_status_during_opening() {
        let (mut input, fault) = decode_live_uplink(
            futures::stream::iter([Err(tonic::Status::data_loss("bad first frame"))]),
            DEFAULT_LIVE_RECOVERY_TIMEOUT,
        );
        let opening = async move {
            assert!(input.next().await.is_none());
            Err(A2AError::invalid_request(
                "a response needs a first request",
            ))
        };
        let error = open_live_downlink(opening, fault).await.err().unwrap();
        assert_eq!(error.code(), tonic::Code::DataLoss);
        assert_eq!(error.message(), "bad first frame");
    }

    #[tokio::test]
    async fn normal_live_half_close_keeps_downlink_available() {
        let (mut input, fault) =
            decode_live_uplink(futures::stream::empty(), DEFAULT_LIVE_RECOVERY_TIMEOUT);
        assert!(input.next().await.is_none());
        let events = futures::stream::iter([Ok(StreamResponse::Message(a2a::Message::new(
            a2a::Role::Agent,
            vec![a2a::Part::text("done")],
        )))])
        .boxed();
        let mut output = live_downlink(events, fault);
        assert!(output.next().await.unwrap().is_ok());
        assert!(output.next().await.is_none());
    }

    #[test]
    fn test_extract_service_params_empty() {
        let metadata = tonic::metadata::MetadataMap::new();
        let params = extract_service_params(&metadata);
        assert!(params.is_empty());
    }

    #[test]
    fn test_extract_service_params_ascii() {
        let mut metadata = tonic::metadata::MetadataMap::new();
        metadata.insert("x-custom", "value1".parse().unwrap());
        metadata.insert("x-another", "value2".parse().unwrap());
        let params = extract_service_params(&metadata);
        assert_eq!(params.get("x-custom").unwrap(), &vec!["value1".to_string()]);
        assert_eq!(
            params.get("x-another").unwrap(),
            &vec!["value2".to_string()]
        );
    }

    /// A client that sends one entry per value, as the Go client does.
    #[test]
    fn test_extract_service_params_repeated_entries() {
        let mut metadata = tonic::metadata::MetadataMap::new();
        metadata.append("a2a-extensions", "urn:a".parse().unwrap());
        metadata.append("a2a-extensions", "urn:b".parse().unwrap());
        let params = extract_service_params(&metadata);
        assert_eq!(
            params.get("a2a-extensions").unwrap(),
            &vec!["urn:a".to_string(), "urn:b".to_string()]
        );
    }

    #[test]
    fn test_grpc_protocol_version_validation() {
        let mut params = a2a_server::middleware::ServiceParams::new();
        assert_eq!(
            validate_protocol_version(&params).unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );

        params.insert("a2a-version".into(), vec!["1.1".into()]);
        assert!(validate_protocol_version(&params).is_ok());

        params.insert("a2a-version".into(), vec!["2.0".into()]);
        assert_eq!(
            validate_protocol_version(&params).unwrap_err().code(),
            tonic::Code::FailedPrecondition
        );
    }

    #[tokio::test]
    async fn configured_recovery_timeout_bounds_an_admitted_opening() {
        use a2a::*;
        use a2a_server::handler::DefaultRequestHandler;
        use a2a_server::task_store::InMemoryTaskStore;

        struct NoopExecutor;
        impl a2a_server::AgentExecutor for NoopExecutor {
            fn execute(
                &self,
                _ctx: a2a_server::executor::ExecutorContext,
            ) -> futures::stream::BoxStream<'static, Result<a2a::event::StreamResponse, A2AError>>
            {
                Box::pin(futures::stream::empty())
            }
            fn cancel(
                &self,
                _ctx: a2a_server::executor::ExecutorContext,
            ) -> futures::stream::BoxStream<'static, Result<a2a::event::StreamResponse, A2AError>>
            {
                Box::pin(futures::stream::empty())
            }
        }

        let handler = Arc::new(
            DefaultRequestHandler::new(NoopExecutor, InMemoryTaskStore::new()).with_capabilities(
                a2a::AgentCapabilities {
                    streaming: Some(true),
                    ..Default::default()
                },
            ),
        );
        let grpc_handler = GrpcHandler::new(handler).with_live_recovery_timeout(Duration::ZERO);
        let (mut input, fault) = decode_live_uplink(
            futures::stream::iter([
                Ok(proto::StreamRequest::default()),
                Err(tonic::Status::data_loss("bad input after admission")),
            ]),
            grpc_handler.live_recovery_timeout,
        );
        let opening = async move {
            assert!(input.next().await.is_some());
            assert!(input.next().await.is_none());
            futures::future::pending().await
        };
        let error =
            tokio::time::timeout(Duration::from_secs(1), open_live_downlink(opening, fault))
                .await
                .unwrap()
                .err()
                .unwrap();
        assert_eq!(error.code(), tonic::Code::DataLoss);
        assert_eq!(error.message(), "bad input after admission");
    }
}
