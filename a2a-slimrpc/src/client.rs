// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;

use a2a::event::StreamResponse;
use a2a::*;
use a2a_client::transport::{ServiceParams, Transport, TransportFactory};
use a2a_pb::pbconv;
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};

use crate::common::{
    A2A_SLIMRPC_SERVICE, METHOD_CANCEL_TASK, METHOD_CREATE_PUSH_CONFIG, METHOD_DELETE_PUSH_CONFIG,
    METHOD_GET_EXTENDED_AGENT_CARD, METHOD_GET_PUSH_CONFIG, METHOD_GET_TASK,
    METHOD_LIST_PUSH_CONFIGS, METHOD_LIST_TASKS, METHOD_SEND_MESSAGE,
    METHOD_SEND_STREAMING_MESSAGE, METHOD_SUBSCRIBE_TO_TASK, decode_proto_response,
    encode_proto_message, service_params_to_metadata_opt,
};
use crate::errors::rpc_error_to_a2a_error;

/// SLIMRPC transport for A2A clients.
pub struct SlimRpcTransport {
    channel: slim_bindings::Channel,
}

impl SlimRpcTransport {
    pub fn new(app: Arc<slim_bindings::App>, remote: Arc<slim_bindings::Name>) -> Self {
        Self::new_with_connection(app, remote, None)
    }

    pub fn new_with_connection(
        app: Arc<slim_bindings::App>,
        remote: Arc<slim_bindings::Name>,
        connection_id: Option<u64>,
    ) -> Self {
        Self {
            channel: slim_bindings::Channel::new_with_connection(app, remote, connection_id),
        }
    }

    pub fn from_channel(channel: slim_bindings::Channel) -> Self {
        Self { channel }
    }

    async fn call_unary<Req, Res>(
        &self,
        params: &ServiceParams,
        method_name: &'static str,
        request: &Req,
        response_name: &str,
    ) -> Result<Res, A2AError>
    where
        Req: prost::Message,
        Res: prost::Message + Default,
    {
        let response = self
            .channel
            .call_unary_async(
                A2A_SLIMRPC_SERVICE.to_string(),
                method_name.to_string(),
                encode_proto_message(request),
                None,
                service_params_to_metadata_opt(params),
            )
            .await
            .map_err(|error| rpc_error_to_a2a_error(&error))?;

        decode_proto_response(response, response_name)
    }

    async fn call_unary_stream<Req, Res>(
        &self,
        params: &ServiceParams,
        method_name: &'static str,
        request: &Req,
        response_name: &'static str,
    ) -> Result<BoxStream<'static, Result<Res, A2AError>>, A2AError>
    where
        Req: prost::Message,
        Res: prost::Message + Default + Send + 'static,
    {
        let reader = self
            .channel
            .call_unary_stream_async(
                A2A_SLIMRPC_SERVICE.to_string(),
                method_name.to_string(),
                encode_proto_message(request),
                None,
                service_params_to_metadata_opt(params),
            )
            .await
            .map_err(|error| rpc_error_to_a2a_error(&error))?;

        let stream = futures::stream::unfold(reader, move |reader| async move {
            match reader.next_async().await {
                slim_bindings::StreamMessage::Data(data) => {
                    Some((decode_proto_response::<Res>(data, response_name), reader))
                }
                slim_bindings::StreamMessage::Error(error) => {
                    Some((Err(rpc_error_to_a2a_error(&error)), reader))
                }
                slim_bindings::StreamMessage::End => None,
            }
        });

        Ok(Box::pin(stream))
    }
}

#[async_trait]
impl Transport for SlimRpcTransport {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let request = pbconv::to_proto_send_message_request(req);
        let response = self
            .call_unary::<_, a2a_pb::proto::SendMessageResponse>(
                params,
                METHOD_SEND_MESSAGE,
                &request,
                "SendMessageResponse",
            )
            .await?;

        pbconv::from_proto_send_message_response(&response)
            .ok_or_else(|| A2AError::internal("empty SendMessageResponse payload"))
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let request = pbconv::to_proto_send_message_request(req);
        let stream = self
            .call_unary_stream::<_, a2a_pb::proto::StreamResponse>(
                params,
                METHOD_SEND_STREAMING_MESSAGE,
                &request,
                "StreamResponse",
            )
            .await?;

        Ok(Box::pin(stream.map(|item| {
            item.and_then(|response| {
                pbconv::from_proto_stream_response(&response)
                    .ok_or_else(|| A2AError::internal("empty StreamResponse payload"))
            })
        })))
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: &GetTaskRequest,
    ) -> Result<Task, A2AError> {
        let request = pbconv::to_proto_get_task_request(req);
        let response = self
            .call_unary::<_, a2a_pb::proto::Task>(params, METHOD_GET_TASK, &request, "Task")
            .await?;
        Ok(pbconv::from_proto_task(&response))
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        let request = pbconv::to_proto_list_tasks_request(req);
        let response = self
            .call_unary::<_, a2a_pb::proto::ListTasksResponse>(
                params,
                METHOD_LIST_TASKS,
                &request,
                "ListTasksResponse",
            )
            .await?;
        Ok(pbconv::from_proto_list_tasks_response(&response))
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: &CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let request = pbconv::to_proto_cancel_task_request(req);
        let response = self
            .call_unary::<_, a2a_pb::proto::Task>(params, METHOD_CANCEL_TASK, &request, "Task")
            .await?;
        Ok(pbconv::from_proto_task(&response))
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: &SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let request = pbconv::to_proto_subscribe_to_task_request(req);
        let stream = self
            .call_unary_stream::<_, a2a_pb::proto::StreamResponse>(
                params,
                METHOD_SUBSCRIBE_TO_TASK,
                &request,
                "StreamResponse",
            )
            .await?;

        Ok(Box::pin(stream.map(|item| {
            item.and_then(|response| {
                pbconv::from_proto_stream_response(&response)
                    .ok_or_else(|| A2AError::internal("empty StreamResponse payload"))
            })
        })))
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: &CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let request = pbconv::to_proto_create_task_push_notification_config_request(req);
        let response = self
            .call_unary::<_, a2a_pb::proto::TaskPushNotificationConfig>(
                params,
                METHOD_CREATE_PUSH_CONFIG,
                &request,
                "TaskPushNotificationConfig",
            )
            .await?;
        Ok(pbconv::from_proto_task_push_notification_config(&response))
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let request = pbconv::to_proto_get_task_push_notification_config_request(req);
        let response = self
            .call_unary::<_, a2a_pb::proto::TaskPushNotificationConfig>(
                params,
                METHOD_GET_PUSH_CONFIG,
                &request,
                "TaskPushNotificationConfig",
            )
            .await?;
        Ok(pbconv::from_proto_task_push_notification_config(&response))
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let request = pbconv::to_proto_list_task_push_notification_configs_request(req);
        let response = self
            .call_unary::<_, a2a_pb::proto::ListTaskPushNotificationConfigsResponse>(
                params,
                METHOD_LIST_PUSH_CONFIGS,
                &request,
                "ListTaskPushNotificationConfigsResponse",
            )
            .await?;
        Ok(pbconv::from_proto_list_task_push_notification_configs_response(&response))
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let request = pbconv::to_proto_delete_task_push_notification_config_request(req);
        let _response = self
            .channel
            .call_unary_async(
                A2A_SLIMRPC_SERVICE.to_string(),
                METHOD_DELETE_PUSH_CONFIG.to_string(),
                encode_proto_message(&request),
                None,
                service_params_to_metadata_opt(params),
            )
            .await
            .map_err(|error| rpc_error_to_a2a_error(&error))?;
        Ok(())
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: &GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        let request = pbconv::to_proto_get_extended_agent_card_request(req);
        let response = self
            .call_unary::<_, a2a_pb::proto::AgentCard>(
                params,
                METHOD_GET_EXTENDED_AGENT_CARD,
                &request,
                "AgentCard",
            )
            .await?;
        Ok(pbconv::from_proto_agent_card(&response))
    }

    async fn destroy(&self) -> Result<(), A2AError> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct SlimRpcTransportFactory {
    app: Arc<slim_bindings::App>,
    connection_id: Option<u64>,
}

impl SlimRpcTransportFactory {
    pub fn new(app: Arc<slim_bindings::App>) -> Self {
        Self {
            app,
            connection_id: None,
        }
    }

    pub fn new_with_connection(app: Arc<slim_bindings::App>, connection_id: Option<u64>) -> Self {
        Self { app, connection_id }
    }
}

#[async_trait]
impl TransportFactory for SlimRpcTransportFactory {
    fn protocol(&self) -> &str {
        a2a::TRANSPORT_PROTOCOL_SLIMRPC
    }

    async fn create(
        &self,
        _card: &AgentCard,
        iface: &AgentInterface,
    ) -> Result<Box<dyn Transport>, A2AError> {
        let remote = parse_slimrpc_target(&iface.url)?;
        let transport =
            SlimRpcTransport::new_with_connection(self.app.clone(), remote, self.connection_id);
        Ok(Box::new(transport))
    }
}

/// Parse a SLIMRPC target from agent card interface data.
pub fn parse_slimrpc_target(target: &str) -> Result<Arc<slim_bindings::Name>, A2AError> {
    let normalized = target
        .trim()
        .strip_prefix("slimrpc://")
        .or_else(|| target.trim().strip_prefix("slim://"))
        .unwrap_or(target.trim())
        .trim_start_matches('/');

    let name = slim_bindings::Name::from_string(normalized.to_string()).map_err(|error| {
        A2AError::invalid_params(format!("invalid SLIMRPC target '{target}': {error}"))
    })?;

    Ok(Arc::new(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_slimrpc_target_plain_name() {
        let name = parse_slimrpc_target("org/namespace/agent").unwrap();
        assert_eq!(name.components(), vec!["org", "namespace", "agent"]);
    }

    #[test]
    fn test_parse_slimrpc_target_scheme() {
        let name = parse_slimrpc_target("slimrpc://org/namespace/agent").unwrap();
        assert_eq!(name.components(), vec!["org", "namespace", "agent"]);
    }

    #[test]
    fn test_parse_slimrpc_target_slim_scheme_and_leading_slash() {
        let name = parse_slimrpc_target("slim://org/namespace/agent").unwrap();
        assert_eq!(name.components(), vec!["org", "namespace", "agent"]);

        let name = parse_slimrpc_target("/org/namespace/agent").unwrap();
        assert_eq!(name.components(), vec!["org", "namespace", "agent"]);
    }

    #[test]
    fn test_parse_slimrpc_target_invalid() {
        let error = parse_slimrpc_target("not-a-valid-target").unwrap_err();
        assert_eq!(error.code, a2a::error_code::INVALID_PARAMS);
    }
}
