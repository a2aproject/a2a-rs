// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;

use a2a::event::StreamResponse;
use a2a::*;
use a2a_client::transport::{ServiceParams, Transport, TransportFactory};
use a2a_pb::pbconv;
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use slim_datapath::api::ProtoName;

/// Native SLIM application type backing the RPC channel.
pub type SlimApp = slim_service::app::App<
    slim_auth::auth_provider::AuthProvider,
    slim_auth::auth_provider::AuthVerifier,
>;

use crate::common::{
    A2A_COLLABORATIVE_CHANNEL_SERVICE, A2A_SLIMRPC_SERVICE, METHOD_CANCEL_TASK, METHOD_COLLABORATE,
    METHOD_CREATE_PUSH_CONFIG, METHOD_DELETE_PUSH_CONFIG, METHOD_GET_EXTENDED_AGENT_CARD,
    METHOD_GET_PUSH_CONFIG, METHOD_GET_TASK, METHOD_LIST_PUSH_CONFIGS, METHOD_LIST_TASKS,
    METHOD_SEND_MESSAGE, METHOD_SEND_STREAMING_MESSAGE, METHOD_SUBSCRIBE_TO_TASK,
    SLIM_SRC_METADATA_KEY, decode_proto_response, encode_proto_message,
    service_params_to_metadata_opt,
};
use crate::errors::rpc_error_to_a2a_error;

/// SLIMRPC transport for A2A clients.
#[derive(Clone)]
pub struct SlimRpcTransport {
    channel: slim_rpc::Channel,
}

impl SlimRpcTransport {
    pub fn new(app: Arc<SlimApp>, remote: Arc<ProtoName>) -> Self {
        Self::new_with_connection(app, remote, None)
    }

    pub fn new_with_connection(
        app: Arc<SlimApp>,
        remote: Arc<ProtoName>,
        connection_id: Option<u64>,
    ) -> Self {
        Self {
            channel: slim_rpc::Channel::new_with_connection(app, remote, connection_id),
        }
    }

    pub fn from_channel(channel: slim_rpc::Channel) -> Self {
        Self { channel }
    }

    /// Build a transport backed by a SLIM **group** channel spanning `members`,
    /// for the `Collaborate` many-to-many operation (see
    /// [`Self::collaborate`]). Unlike the point-to-point [`Self::new`], this does
    /// not correspond to a single `Transport` peer.
    pub fn new_group(
        app: Arc<SlimApp>,
        members: Vec<Arc<ProtoName>>,
    ) -> Result<Self, slim_rpc::RpcError> {
        Self::new_group_with_connection(app, members, None)
    }

    pub fn new_group_with_connection(
        app: Arc<SlimApp>,
        members: Vec<Arc<ProtoName>>,
        connection_id: Option<u64>,
    ) -> Result<Self, slim_rpc::RpcError> {
        Ok(Self {
            channel: slim_rpc::Channel::new_group_with_connection(app, members, connection_id)?,
        })
    }

    /// Open a `Collaborate` session on this (group) channel: broadcast every
    /// `Message` produced by `outbound` to the group, and yield every `Message`
    /// broadcast by other members — each attributed via
    /// `metadata["slim-src"]` (the sender's SLIM name), per the SLIMRPC
    /// collaborative channel extension spec. Additive: not part of the
    /// point-to-point [`Transport`] trait, whose methods assume exactly one
    /// response per call.
    pub fn collaborate(
        &self,
        outbound: impl futures::Stream<Item = Message> + Send + 'static,
        timeout: Option<std::time::Duration>,
    ) -> impl futures::Stream<Item = Result<Message, A2AError>> {
        let request_stream =
            outbound.map(|message| encode_proto_message(&pbconv::to_proto_message(&message)));
        let stream = self.channel.multicast_stream_stream::<Vec<u8>, Vec<u8>>(
            A2A_COLLABORATIVE_CHANNEL_SERVICE,
            METHOD_COLLABORATE,
            request_stream,
            timeout,
            None,
        );
        stream.map(|item| {
            let item = item.map_err(|error| rpc_error_to_a2a_error(&error))?;
            let proto_message =
                decode_proto_response::<a2a_pb::proto::Message>(item.message, "Message")?;
            let mut message = pbconv::from_proto_message(&proto_message);
            message
                .metadata
                .get_or_insert_with(std::collections::HashMap::new)
                .insert(
                    SLIM_SRC_METADATA_KEY.to_string(),
                    serde_json::Value::String(item.context.source.to_string()),
                );
            Ok(message)
        })
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
            .unary::<Vec<u8>, Vec<u8>>(
                A2A_SLIMRPC_SERVICE,
                method_name,
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
        // `Channel::unary_stream` borrows `&self` (its `impl Stream` return captures the
        // channel's lifetime), but the `Transport` trait requires a `'static` stream.
        // Own a channel clone inside an `async_stream` generator so the driven inner
        // stream lives as long as the returned stream. Errors are delivered as stream
        // items, so there is no fallible setup step to await up front.
        let channel = self.channel.clone();
        let payload = encode_proto_message(request);
        let metadata = service_params_to_metadata_opt(params);

        let stream = async_stream::stream! {
            let inner = channel.unary_stream::<Vec<u8>, Vec<u8>>(
                A2A_SLIMRPC_SERVICE,
                method_name,
                payload,
                None,
                metadata,
            );
            futures::pin_mut!(inner);
            while let Some(item) = inner.next().await {
                yield match item {
                    Ok(data) => decode_proto_response::<Res>(data, response_name),
                    Err(error) => Err(rpc_error_to_a2a_error(&error)),
                };
            }
        };

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
        req: &TaskPushNotificationConfig,
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
        let _response: Vec<u8> = self
            .channel
            .unary(
                A2A_SLIMRPC_SERVICE,
                METHOD_DELETE_PUSH_CONFIG,
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
    app: Arc<SlimApp>,
    connection_id: Option<u64>,
}

impl SlimRpcTransportFactory {
    pub fn new(app: Arc<SlimApp>) -> Self {
        Self {
            app,
            connection_id: None,
        }
    }

    pub fn new_with_connection(app: Arc<SlimApp>, connection_id: Option<u64>) -> Self {
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
pub fn parse_slimrpc_target(target: &str) -> Result<Arc<ProtoName>, A2AError> {
    let normalized = target
        .trim()
        .strip_prefix("slimrpc://")
        .or_else(|| target.trim().strip_prefix("slim://"))
        .unwrap_or(target.trim())
        .trim_start_matches('/');

    let components: Vec<&str> = normalized.split('/').collect();
    let [org, namespace, agent] = components.as_slice() else {
        return Err(A2AError::invalid_params(format!(
            "invalid SLIMRPC target '{target}': expected 'org/namespace/agent'"
        )));
    };
    if org.is_empty() || namespace.is_empty() || agent.is_empty() {
        return Err(A2AError::invalid_params(format!(
            "invalid SLIMRPC target '{target}': components must be non-empty"
        )));
    }

    Ok(Arc::new(ProtoName::from_strings([
        *org, *namespace, *agent,
    ])))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_slimrpc_target_plain_name() {
        let name = parse_slimrpc_target("org/namespace/agent").unwrap();
        assert_eq!(name.str_components(), ("org", "namespace", "agent"));
    }

    #[test]
    fn test_parse_slimrpc_target_scheme() {
        let name = parse_slimrpc_target("slimrpc://org/namespace/agent").unwrap();
        assert_eq!(name.str_components(), ("org", "namespace", "agent"));
    }

    #[test]
    fn test_parse_slimrpc_target_slim_scheme_and_leading_slash() {
        let name = parse_slimrpc_target("slim://org/namespace/agent").unwrap();
        assert_eq!(name.str_components(), ("org", "namespace", "agent"));

        let name = parse_slimrpc_target("/org/namespace/agent").unwrap();
        assert_eq!(name.str_components(), ("org", "namespace", "agent"));
    }

    #[test]
    fn test_parse_slimrpc_target_invalid() {
        let error = parse_slimrpc_target("not-a-valid-target").unwrap_err();
        assert_eq!(error.code, a2a::error_code::INVALID_PARAMS);
    }
}
