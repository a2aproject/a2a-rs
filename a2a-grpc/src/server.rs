// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use std::pin::Pin;
use std::sync::Arc;

use a2a::{A2AError, StreamResponse};
use a2a_pb::pbconv;
use a2a_pb::proto;
use a2a_pb::proto::a2a_service_server::A2aService;
use a2a_server::RequestHandler;
use futures::{FutureExt, StreamExt};
use tokio_stream::Stream;

use crate::errors::a2a_error_to_status;

type BoxStreamResult<T> = Pin<Box<dyn Stream<Item = Result<T, tonic::Status>> + Send + 'static>>;

/// Tonic decoding failures are fatal transport errors, not a normal half-close.
/// Keep the error alongside the normalized input so it can reach the downlink
/// even when the handler is still waiting for its first request.
fn decode_live_uplink(
    input: impl Stream<Item = Result<proto::StreamRequest, tonic::Status>> + Send + 'static,
) -> (
    futures::stream::BoxStream<'static, a2a::StreamRequest>,
    futures::future::BoxFuture<'static, tonic::Status>,
) {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let input = input
        .scan(Some(sender), |sender, item| {
            futures::future::ready(match item {
                Ok(request) => Some(pbconv::from_proto_stream_request(&request)),
                Err(error) => {
                    if let Some(sender) = sender.take() {
                        let _ = sender.send(error);
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

fn live_downlink(
    events: futures::stream::BoxStream<'static, Result<StreamResponse, A2AError>>,
    fault: futures::future::BoxFuture<'static, tonic::Status>,
) -> BoxStreamResult<proto::StreamResponse> {
    // A transport failure must follow output already ready for earlier valid
    // inputs. Bound the drain so an always-ready executor cannot hide the error.
    futures::stream::unfold(Some((events, fault, None, 128usize)), |state| async move {
        let (mut events, mut fault, mut failed, mut remaining) = state?;
        loop {
            if let Some(error) = failed {
                if remaining > 0 {
                    if let Some(Some(event)) = events.next().now_or_never() {
                        remaining -= 1;
                        return Some((map_proto_stream_item(event), Some((events, fault, Some(error), remaining))));
                    }
                }
                return Some((Err(error), None));
            }
            tokio::select! {
                biased;
                error = &mut fault => failed = Some(error),
                event = events.next() => return event.map(|event| (map_proto_stream_item(event), Some((events, fault, None, remaining)))),
            }
        }
    }).boxed()
}

async fn open_live_downlink(
    opening: impl std::future::Future<
        Output = Result<
            futures::stream::BoxStream<'static, Result<StreamResponse, A2AError>>,
            A2AError,
        >,
    >,
    mut fault: futures::future::BoxFuture<'static, tonic::Status>,
) -> Result<BoxStreamResult<proto::StreamResponse>, tonic::Status> {
    tokio::pin!(opening);
    let events = tokio::select! {
        biased;
        // Prefer an already-open response so its ready events can drain.
        events = &mut opening => match events {
            Ok(events) => events,
            Err(error) => return Err((&mut fault).now_or_never().unwrap_or_else(|| a2a_error_to_status(&error))),
        },
        error = &mut fault => {
            // Poll once after the malformed frame closed the input. Never
            // wait for unrelated executor work just to report bad framing.
            if let Some(Ok(events)) = opening.now_or_never() {
                return Ok(live_downlink(events, futures::future::ready(error).boxed()));
            }
            return Err(error);
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
}

impl<H: RequestHandler> GrpcHandler<H> {
    pub fn new(handler: Arc<H>) -> Self {
        GrpcHandler { handler }
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
        let (uplink, fault) = decode_live_uplink(request.into_inner());
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
        let (mut input, fault) = decode_live_uplink(futures::stream::iter([
            Ok(frame.clone()),
            Err(tonic::Status::data_loss("bad protobuf frame")),
            Ok(frame),
        ]));
        assert!(input.next().await.is_some());
        assert!(input.next().await.is_none());
        assert!(input.next().await.is_none());
        let mut output = live_downlink(futures::stream::pending().boxed(), fault);
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
            let fault =
                futures::future::ready(tonic::Status::data_loss("bad protobuf frame")).boxed();
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
            futures::future::ready(tonic::Status::data_loss("broken input")).boxed(),
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
            futures::future::ready(tonic::Status::data_loss("broken input")).boxed(),
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
    async fn malformed_first_frame_preserves_transport_status_during_opening() {
        let (mut input, fault) = decode_live_uplink(futures::stream::iter([Err(
            tonic::Status::data_loss("bad first frame"),
        )]));
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
        let (mut input, fault) = decode_live_uplink(futures::stream::empty());
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

    #[test]
    fn test_grpc_handler_new() {
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
        let grpc_handler = GrpcHandler::new(handler);
        // Just verify construction
        let _ = grpc_handler;
    }
}
