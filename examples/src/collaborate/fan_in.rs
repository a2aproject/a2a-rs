// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! FanInHandler: routes relay messages from peer agents into the inbox of
//! the active executor for the addressed task, rather than starting a new
//! execution. All other methods delegate to the wrapped inner handler.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use a2a::*;
use a2a_client::BoxStream;
use a2a_server::{RequestHandler, ServiceParams};
use async_trait::async_trait;
use tokio::sync::mpsc;

/// Registry of per-task message inboxes for active executions.
///
/// An executor calls `register` when it starts to receive relay messages;
/// it calls `unregister` when execution completes.
pub struct InboxRegistry {
    senders: Mutex<HashMap<String, mpsc::Sender<Message>>>,
}

impl Default for InboxRegistry {
    fn default() -> Self {
        Self {
            senders: Mutex::new(HashMap::new()),
        }
    }
}

impl InboxRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register an inbox for `task_id`. Returns the receiver.
    pub fn register(&self, task_id: &str) -> mpsc::Receiver<Message> {
        let (tx, rx) = mpsc::channel(32);
        self.senders.lock().unwrap().insert(task_id.to_string(), tx);
        rx
    }

    /// Remove the inbox entry when the executor is done.
    pub fn unregister(&self, task_id: &str) {
        self.senders.lock().unwrap().remove(task_id);
    }

    /// Deliver a relay message to an active executor.
    ///
    /// Returns `true` if an inbox existed and the message was queued,
    /// `false` if no active execution is registered for `task_id`.
    pub fn try_deliver(&self, task_id: &str, message: Message) -> bool {
        let guard = self.senders.lock().unwrap();
        if let Some(tx) = guard.get(task_id) {
            tx.try_send(message).is_ok()
        } else {
            false
        }
    }
}

/// A `RequestHandler` middleware that intercepts relay messages.
///
/// When `send_message` arrives with a `task_id` that has an active inbox,
/// the message is delivered to that inbox rather than starting a new
/// execution. All other calls and unrecognized `send_message` calls
/// are forwarded to the inner handler.
pub struct FanInHandler<H: RequestHandler> {
    inner: Arc<H>,
    pub inbox: Arc<InboxRegistry>,
}

impl<H: RequestHandler> FanInHandler<H> {
    /// Create a `FanInHandler` sharing `inbox` with the executor.
    ///
    /// Create the `InboxRegistry` first, pass it to the executor, then pass
    /// the same `Arc` here so both sides route through the same registry.
    pub fn new(inner: H, inbox: Arc<InboxRegistry>) -> Self {
        Self {
            inner: Arc::new(inner),
            inbox,
        }
    }
}

#[async_trait]
impl<H: RequestHandler> RequestHandler for FanInHandler<H> {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        if let Some(task_id) = &req.message.task_id
            && self.inbox.try_deliver(task_id, req.message.clone())
        {
            // Relay message delivered to the active executor.
            // Return a minimal in-progress task as acknowledgement.
            return Ok(SendMessageResponse::Task(Task {
                id: task_id.clone(),
                context_id: req.message.context_id.clone().unwrap_or_default(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            }));
        }
        self.inner.send_message(params, req).await
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.inner.send_streaming_message(params, req).await
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        self.inner.get_task(params, req).await
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        self.inner.list_tasks(params, req).await
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        self.inner.cancel_task(params, req).await
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.inner.subscribe_to_task(params, req).await
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.inner.create_push_config(params, req).await
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.inner.get_push_config(params, req).await
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        self.inner.list_push_configs(params, req).await
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        self.inner.delete_push_config(params, req).await
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        self.inner.get_extended_agent_card(params, req).await
    }
}
