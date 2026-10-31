// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use async_trait::async_trait;
use auto_impl::auto_impl;
use futures::stream::BoxStream;
use std::collections::HashMap;
use std::sync::Arc;

pub type ServiceParams = HashMap<String, Vec<String>>;

/// Each HTTP LiveConnection owns a routing identifier so later requests and
/// their rejections stay on that connection even when other clients use the
/// same task and credentials. Servers without this optional SDK convention
/// ignore the header and keep their own live-routing behavior.
pub(crate) fn live_params(params: &ServiceParams) -> ServiceParams {
    let mut params = params.clone();
    params.retain(|name, _| !name.eq_ignore_ascii_case(SDK_LIVE_EXCHANGE_HEADER));
    params.insert(
        SDK_LIVE_EXCHANGE_HEADER.into(),
        vec![uuid::Uuid::now_v7().to_string()],
    );
    params
}

/// The client's half of a live exchange (A2A 1.1 §3.1.12): how it sends further
/// requests once the exchange is open.
///
/// A transport with native duplex (gRPC) writes them to the stream it opened.
/// One without (JSON-RPC, REST) makes a short-lived call to the same operation
/// for each, which the server answers with the task as it stands; that is what
/// `send` returns there.
#[async_trait]
pub trait LiveUplink: Send + Sync {
    async fn send(&self, request: StreamRequest) -> Result<LiveAck, A2AError>;
}

/// What sending a request on a live exchange came back with.
#[non_exhaustive]
pub enum LiveAck {
    /// The request was written to the exchange's stream (gRPC, SLIMRPC). Whether
    /// the server accepted it is told on the event stream: a rejection is a
    /// `StreamError` there.
    Sent,
    /// The server received the call and answered with the task as it stands
    /// (JSON-RPC, REST); its events come on the downlink, not here.
    Task(Box<Task>),
    /// The exchange this was meant for had ended, so the server opened a new one
    /// for the request, which it carried out. Here is that exchange; the old
    /// events stream has ended.
    Reopened(Box<LiveConnection>),
}

/// Sends requests on a live exchange. Cheap to clone; the exchange's uplink
/// ends when the last clone is dropped.
#[derive(Clone)]
pub struct LiveSender {
    uplink: Arc<dyn LiveUplink>,
    tenant: Option<String>,
}

impl LiveSender {
    pub fn new(uplink: Arc<dyn LiveUplink>) -> Self {
        LiveSender {
            uplink,
            tenant: None,
        }
    }

    /// Declares the routing tenant of the selected interface, applied to every
    /// request sent (A2A §8.3.2).
    pub fn with_tenant(mut self, tenant: impl Into<String>) -> Self {
        self.tenant = Some(tenant.into());
        self
    }

    /// Sends one request; see [`LiveAck`] for what comes back. Whether the
    /// server accepted it is told on the event stream: a rejection is a
    /// `StreamError` there.
    ///
    /// On HTTP bindings each request is a call of its own, made with the
    /// service parameters the exchange was opened with; a credential that has
    /// expired since is not renewed.
    pub async fn send(&self, mut request: StreamRequest) -> Result<LiveAck, A2AError> {
        if let Some(tenant) = &self.tenant {
            request.tenant = Some(tenant.clone());
            match &mut request.payload {
                StreamRequestPayload::SendMessage(inner) => inner.tenant = Some(tenant.clone()),
                StreamRequestPayload::Subscribe(inner) => inner.tenant = Some(tenant.clone()),
                _ => {}
            }
        }
        let mut acknowledgement = self.uplink.send(request).await?;
        if let (Some(tenant), LiveAck::Reopened(connection)) = (&self.tenant, &mut acknowledgement)
        {
            // A replacement downlink must retain the interface's routing tenant.
            connection.sender = connection.sender.clone().with_tenant(tenant.clone());
        }
        Ok(acknowledgement)
    }

    /// Sends `message` to the task the exchange is bound to.
    pub async fn send_message(&self, message: Message) -> Result<LiveAck, A2AError> {
        self.send(StreamRequest {
            tenant: None,
            payload: StreamRequestPayload::SendMessage(SendMessageRequest {
                message,
                configuration: None,
                metadata: None,
                tenant: None,
                artifact_updates: Vec::new(),
            }),
        })
        .await
    }
}

/// An open live exchange: the events of the task, and the means to send more.
pub struct LiveConnection {
    /// The downlink. Ends when the task does, or the server closes the
    /// exchange.
    pub events: BoxStream<'static, Result<StreamResponse, A2AError>>,
    /// The uplink.
    pub sender: LiveSender,
}

/// The extension point for all protocol bindings.
///
/// Each protocol binding (JSON-RPC, REST, gRPC, or custom) implements this trait.
/// The [`TransportFactory`] creates instances of `Transport` for a given agent interface.
///
/// A blanket implementation for `Box<dyn Transport>` is automatically derived via
/// [`auto_impl`], enabling `A2AClient<Box<dyn Transport>>` for runtime-selected transports.
#[async_trait]
#[auto_impl(Box)]
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

    /// Opens a live exchange (`SendLiveMessage`, A2A 1.1 §3.1.12) with `first`,
    /// which binds it to a task. Not every binding supports it.
    async fn send_live_message(
        &self,
        params: &ServiceParams,
        first: &StreamRequest,
    ) -> Result<LiveConnection, A2AError> {
        let _ = (params, first);
        Err(A2AError::unsupported_operation(
            "this transport does not support SendLiveMessage",
        ))
    }

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
        req: &TaskPushNotificationConfig,
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
