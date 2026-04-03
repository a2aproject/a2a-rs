// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::pin::Pin;
use std::sync::Arc;

use a2a::{A2AError, StreamResponse};
use a2a_pb::pbconv;
use a2a_pb::proto;
use a2a_pb::proto::a2a_service_server::A2aService;
use a2a_server::RequestHandler;
use futures::StreamExt;
use tokio_stream::Stream;

use crate::errors::a2a_error_to_status;

type BoxStreamResult<T> = Pin<Box<dyn Stream<Item = Result<T, tonic::Status>> + Send + 'static>>;

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
        let native_req = pbconv::from_proto_subscribe_to_task_request(request.get_ref());
        let stream = self
            .handler
            .subscribe_to_task(&params, native_req)
            .await
            .map_err(|e| a2a_error_to_status(&e))?;
        let proto_stream = stream.map(map_proto_stream_item);
        Ok(tonic::Response::new(Box::pin(proto_stream)))
    }

    async fn create_task_push_notification_config(
        &self,
        request: tonic::Request<proto::TaskPushNotificationConfig>,
    ) -> Result<tonic::Response<proto::TaskPushNotificationConfig>, tonic::Status> {
        let params = extract_service_params(request.metadata());
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

        let handler = Arc::new(DefaultRequestHandler::new(
            NoopExecutor,
            InMemoryTaskStore::new(),
        ));
        let grpc_handler = GrpcHandler::new(handler);
        // Just verify construction
        let _ = grpc_handler;
    }
}
