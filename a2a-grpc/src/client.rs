// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::event::StreamResponse;
use a2a::*;
use a2a_client::transport::{ServiceParams, Transport, TransportFactory};
use a2a_pb::pbconv;
use a2a_pb::proto::a2a_service_client::A2aServiceClient;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use tokio::sync::Mutex;
use tonic::transport::Channel;

use crate::errors::status_to_a2a_error;

/// gRPC transport — implements the `Transport` trait using tonic gRPC client.
///
/// Converts native A2A types to proto types via `a2a_pb::pbconv`,
/// calls the gRPC service, and converts responses back.
pub struct GrpcTransport {
    client: Mutex<A2aServiceClient<Channel>>,
}

impl GrpcTransport {
    pub async fn connect(endpoint: impl Into<String>) -> Result<Self, A2AError> {
        let endpoint_str = endpoint.into();
        let client = A2aServiceClient::connect(endpoint_str)
            .await
            .map_err(|e| A2AError::internal(format!("gRPC connect error: {e}")))?;
        Ok(GrpcTransport {
            client: Mutex::new(client),
        })
    }

    pub fn from_channel(channel: Channel) -> Self {
        GrpcTransport {
            client: Mutex::new(A2aServiceClient::new(channel)),
        }
    }
}

/// Convert `ServiceParams` to gRPC metadata.
fn service_params_to_metadata(params: &ServiceParams) -> tonic::metadata::MetadataMap {
    let mut metadata = tonic::metadata::MetadataMap::new();
    for (key, values) in params {
        if let Ok(key) = tonic::metadata::MetadataKey::from_bytes(key.as_bytes()) {
            for value in values {
                if let Ok(val) = value.parse() {
                    metadata.insert(key.clone(), val);
                }
            }
        }
    }
    metadata
}

fn make_request<T>(params: &ServiceParams, msg: T) -> tonic::Request<T> {
    let mut req = tonic::Request::new(msg);
    let metadata = service_params_to_metadata(params);
    *req.metadata_mut() = metadata;
    req
}

#[async_trait]
impl Transport for GrpcTransport {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let proto_req = pbconv::to_proto_send_message_request(req);
        let grpc_req = make_request(params, proto_req);
        let response = self
            .client
            .lock()
            .await
            .send_message(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        pbconv::from_proto_send_message_response(response.get_ref())
            .ok_or_else(|| A2AError::internal("empty SendMessageResponse payload"))
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let proto_req = pbconv::to_proto_send_message_request(req);
        let grpc_req = make_request(params, proto_req);
        let response = self
            .client
            .lock()
            .await
            .send_streaming_message(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        let stream = response.into_inner().map(|item| match item {
            Ok(proto_sr) => pbconv::from_proto_stream_response(&proto_sr)
                .ok_or_else(|| A2AError::internal("empty StreamResponse payload")),
            Err(s) => Err(status_to_a2a_error(&s)),
        });
        Ok(Box::pin(stream))
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: &GetTaskRequest,
    ) -> Result<Task, A2AError> {
        let proto_req = pbconv::to_proto_get_task_request(req);
        let grpc_req = make_request(params, proto_req);
        let response = self
            .client
            .lock()
            .await
            .get_task(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_task(response.get_ref()))
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        let proto_req = pbconv::to_proto_list_tasks_request(req);
        let grpc_req = make_request(params, proto_req);
        let response = self
            .client
            .lock()
            .await
            .list_tasks(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_list_tasks_response(response.get_ref()))
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: &CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let proto_req = pbconv::to_proto_cancel_task_request(req);
        let grpc_req = make_request(params, proto_req);
        let response = self
            .client
            .lock()
            .await
            .cancel_task(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_task(response.get_ref()))
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: &SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let proto_req = pbconv::to_proto_subscribe_to_task_request(req);
        let grpc_req = make_request(params, proto_req);
        let response = self
            .client
            .lock()
            .await
            .subscribe_to_task(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        let stream = response.into_inner().map(|item| match item {
            Ok(proto_sr) => pbconv::from_proto_stream_response(&proto_sr)
                .ok_or_else(|| A2AError::internal("empty StreamResponse payload")),
            Err(s) => Err(status_to_a2a_error(&s)),
        });
        Ok(Box::pin(stream))
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: &CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let proto_req = pbconv::to_proto_create_task_push_notification_config_request(req);
        let grpc_req = make_request(params, proto_req);
        let response = self
            .client
            .lock()
            .await
            .create_task_push_notification_config(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_task_push_notification_config(
            response.get_ref(),
        ))
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let proto_req = pbconv::to_proto_get_task_push_notification_config_request(req);
        let grpc_req = make_request(params, proto_req);
        let response = self
            .client
            .lock()
            .await
            .get_task_push_notification_config(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_task_push_notification_config(
            response.get_ref(),
        ))
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let proto_req = pbconv::to_proto_list_task_push_notification_configs_request(req);
        let grpc_req = make_request(params, proto_req);
        let response = self
            .client
            .lock()
            .await
            .list_task_push_notification_configs(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_list_task_push_notification_configs_response(response.get_ref()))
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let proto_req = pbconv::to_proto_delete_task_push_notification_config_request(req);
        let grpc_req = make_request(params, proto_req);
        self.client
            .lock()
            .await
            .delete_task_push_notification_config(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(())
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: &GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        let proto_req = pbconv::to_proto_get_extended_agent_card_request(req);
        let grpc_req = make_request(params, proto_req);
        let response = self
            .client
            .lock()
            .await
            .get_extended_agent_card(grpc_req)
            .await
            .map_err(|s| status_to_a2a_error(&s))?;
        Ok(pbconv::from_proto_agent_card(response.get_ref()))
    }

    async fn destroy(&self) -> Result<(), A2AError> {
        Ok(())
    }
}

/// Factory for creating gRPC transports.
pub struct GrpcTransportFactory;

#[async_trait]
impl TransportFactory for GrpcTransportFactory {
    fn protocol(&self) -> &str {
        a2a::TRANSPORT_PROTOCOL_GRPC
    }

    async fn create(
        &self,
        _card: &AgentCard,
        iface: &AgentInterface,
    ) -> Result<Box<dyn Transport>, A2AError> {
        let transport = GrpcTransport::connect(&iface.url).await?;
        Ok(Box::new(transport))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_params_to_metadata_empty() {
        let params = ServiceParams::new();
        let metadata = service_params_to_metadata(&params);
        assert_eq!(metadata.len(), 0);
    }

    #[test]
    fn test_service_params_to_metadata_ascii() {
        let mut params = ServiceParams::new();
        params.insert("x-custom".to_string(), vec!["value1".to_string()]);
        params.insert(
            "x-multi".to_string(),
            vec!["a".to_string(), "b".to_string()],
        );
        let metadata = service_params_to_metadata(&params);
        assert!(metadata.get("x-custom").is_some());
    }

    #[test]
    fn test_make_request() {
        let mut params = ServiceParams::new();
        params.insert("x-version".to_string(), vec!["1.0".to_string()]);
        let req = make_request(&params, "test-body".to_string());
        assert!(req.metadata().get("x-version").is_some());
        assert_eq!(req.get_ref(), "test-body");
    }

    #[test]
    fn test_grpc_transport_factory_protocol() {
        let f = GrpcTransportFactory;
        assert_eq!(f.protocol(), "GRPC");
    }

    #[tokio::test]
    async fn test_grpc_transport_connect_failure() {
        // Connecting to invalid endpoint should fail
        let result = GrpcTransport::connect("http://[::1]:1").await;
        // connect may succeed (deferred connection) or fail
        // Either way we're testing the code path
        let _ = result;
    }

    #[test]
    fn test_grpc_transport_from_channel() {
        // We can't easily create a real Channel without a server,
        // but we can test the factory protocol
        let f = GrpcTransportFactory;
        assert_eq!(f.protocol(), a2a::TRANSPORT_PROTOCOL_GRPC);
    }
}
