// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Transport-agnostic request API and the default handler.

use crate::middleware::{ServiceParams, User};
use a2a::*;
use async_trait::async_trait;
use futures::stream::BoxStream;

mod admission;
mod background_tasks;
#[cfg(test)]
mod blocking_completion_tests;
#[cfg(test)]
mod blocking_input_tests;
mod cancellation;
#[cfg(test)]
mod conformance_tests;
mod default_handler;
mod delivery;
mod events;
mod execution;
mod http_exchange;
pub(crate) mod protocol;
mod push_delivery;
mod push_worker;
mod response;
mod task_handle;
mod task_service;
mod updates;
mod uplink;

pub use default_handler::DefaultRequestHandler;
pub(crate) use task_handle::WeakTaskHandle;

const EXECUTION_BUFFER_CAPACITY: usize = 128;

/// How many requests may queue on a live exchange's uplink.
const LIVE_UPLINK_CAPACITY: usize = 32;

/// How many events may wait before a response runner waits for its consumer.
const RESPONSE_BUFFER_CAPACITY: usize = 32;

/// How many client messages may wait for the executor before it reports busy.
const TASK_INPUT_CAPACITY: usize = 16;

/// Transport-agnostic request handler interface.
///
/// This is the core interface consumed by all protocol bindings
/// (JSON-RPC, REST, gRPC). The [`DefaultRequestHandler`] provides a
/// standard implementation that orchestrates task lifecycle, storage,
/// and executor dispatch.
#[async_trait]
pub trait RequestHandler: Send + Sync + 'static {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError>;

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError>;

    /// `SendLiveMessage` over a transport with native duplex (gRPC): `uplink`
    /// carries the client's requests and the returned stream the task's events,
    /// for as long as the exchange lasts. A request that cannot even open the
    /// exchange is an `Err`; one rejected later is a `StreamError` on the
    /// stream, which stays open.
    ///
    /// Not supported unless the handler overrides it.
    async fn send_live_message(
        &self,
        params: &ServiceParams,
        uplink: BoxStream<'static, StreamRequest>,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let _ = (params, uplink);
        Err(A2AError::unsupported_operation(
            "SendLiveMessage is not supported",
        ))
    }

    async fn get_task(&self, params: &ServiceParams, req: GetTaskRequest)
    -> Result<Task, A2AError>;

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError>;

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError>;

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError>;

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError>;

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError>;

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError>;

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError>;

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError>;

    /// `SendLiveMessage` over a transport that has none (JSON-RPC, REST): each
    /// call carries one request. The call that opens an exchange gets its events
    /// back; a later call for the same task is input to that exchange and gets
    /// only the task as it stands (see [`LiveResponse`]).
    ///
    /// Not supported unless the handler overrides it.
    async fn send_live_request(
        &self,
        params: &ServiceParams,
        request: StreamRequest,
    ) -> Result<LiveResponse, A2AError> {
        let _ = (params, request);
        Err(A2AError::unsupported_operation(
            "SendLiveMessage is not supported",
        ))
    }
}

/// The answer to one `SendLiveMessage` call over a transport that has no
/// native duplex (A2A 1.1 §3.1.12): the call that opens the exchange is the
/// downlink, and a later call for the same task only gets the task state back.
#[non_exhaustive]
pub enum LiveResponse {
    /// This call is the downlink: the events, until the exchange ends.
    Stream(BoxStream<'static, Result<StreamResponse, A2AError>>),
    /// This call was input for an exchange that is already open; here is the
    /// task as it stands. Its events arrive on the downlink, never here.
    /// This acknowledges the call, not acceptance of its input: a request-level
    /// rejection is a `StreamError` with `referenceId` on the downlink (§3.1.12).
    /// Authorization and binding errors before routing still return `Err`.
    Ack(Box<Task>),
}
/// Decides whether a request may proceed.
///
/// The handler never treats a non-empty header as a principal. An
/// implementation must validate whatever credential it accepts (token, DID,
/// session) and return [`A2AError`] when that check fails. `task_id` is set
/// when the method addresses one task and is reserved for owner checks.
pub trait RequestAuthorizer: Send + Sync {
    fn authorize(&self, params: &ServiceParams, task_id: Option<&str>) -> Result<(), A2AError>;

    /// Resolve the trusted caller after authorization. Implementations must
    /// validate credentials or use trusted server state; a header or message
    /// claiming a user name is not an authenticated identity.
    /// The default preserves compatibility with authorization-only hooks.
    fn authenticated_user(&self, _params: &ServiceParams) -> Result<Option<User>, A2AError> {
        Ok(None)
    }
}

impl<F> RequestAuthorizer for F
where
    F: Fn(&ServiceParams, Option<&str>) -> Result<(), A2AError> + Send + Sync,
{
    fn authorize(&self, params: &ServiceParams, task_id: Option<&str>) -> Result<(), A2AError> {
        self(params, task_id)
    }
}

/// Supplies the authenticated extended Agent Card (§12.4, §10.1).
///
/// A resolver rather than a stored card because §12.4 has the extended card
/// fetched *under a security scheme*: which skills an agent reveals can
/// legitimately differ by caller, and `ServiceParams` is where the caller's
/// credentials arrive. An agent serving one card to everyone can ignore the
/// argument — [`DefaultRequestHandler::with_extended_agent_card`] wraps a
/// plain card for exactly that case.
pub trait ExtendedAgentCardResolver: Send + Sync {
    fn resolve(&self, params: &ServiceParams) -> Result<AgentCard, A2AError>;
}

impl<F> ExtendedAgentCardResolver for F
where
    F: Fn(&ServiceParams) -> Result<AgentCard, A2AError> + Send + Sync,
{
    fn resolve(&self, params: &ServiceParams) -> Result<AgentCard, A2AError> {
        self(params)
    }
}

#[cfg(test)]
mod test_support;
