// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::collections::HashMap;

pub type ServiceParams = HashMap<String, Vec<String>>;

/// The extension point for all protocol bindings.
///
/// Each protocol binding (JSON-RPC, REST, gRPC, or custom) implements this trait.
/// The [`TransportFactory`] creates instances of `Transport` for a given agent interface.
#[async_trait]
pub trait Transport: Send + Sync {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError>;

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError>;

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: &GetTaskRequest,
    ) -> Result<Task, A2AError>;

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError>;

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: &CancelTaskRequest,
    ) -> Result<Task, A2AError>;

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: &SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError>;

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: &CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError>;

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError>;

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError>;

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError>;

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: &GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError>;

    async fn destroy(&self) -> Result<(), A2AError>;
}

/// Factory that creates [`Transport`] instances from agent card interface declarations.
///
/// Each protocol binding provides its own `TransportFactory` implementation.
/// Register factories with [`A2AClientFactory`](crate::A2AClientFactory) to enable
/// automatic protocol negotiation.
#[async_trait]
pub trait TransportFactory: Send + Sync {
    /// Returns the protocol identifier this factory handles (e.g., "JSONRPC", "GRPC").
    fn protocol(&self) -> &str;

    /// Create a transport for the given agent interface.
    async fn create(
        &self,
        card: &AgentCard,
        iface: &AgentInterface,
    ) -> Result<Box<dyn Transport>, A2AError>;
}
