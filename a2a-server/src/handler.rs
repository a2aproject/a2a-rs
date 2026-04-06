// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use async_trait::async_trait;
use futures::stream::BoxStream;

use crate::middleware::ServiceParams;

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
        req: CreateTaskPushNotificationConfigRequest,
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
}

/// Default implementation of [`RequestHandler`] that orchestrates
/// task lifecycle management, storage, and executor dispatch.
pub struct DefaultRequestHandler {
    executor: Box<dyn crate::AgentExecutor>,
    task_store: Box<dyn crate::TaskStore>,
    push_config_store: Option<Box<dyn crate::PushConfigStore>>,
    capabilities: AgentCapabilities,
}

impl DefaultRequestHandler {
    pub fn new(executor: impl crate::AgentExecutor, task_store: impl crate::TaskStore) -> Self {
        DefaultRequestHandler {
            executor: Box::new(executor),
            task_store: Box::new(task_store),
            push_config_store: None,
            capabilities: AgentCapabilities::default(),
        }
    }

    pub fn with_push_config_store(
        mut self,
        push_config_store: impl crate::PushConfigStore,
    ) -> Self {
        self.push_config_store = Some(Box::new(push_config_store));
        self.capabilities.push_notifications = Some(true);
        self
    }

    pub fn with_capabilities(mut self, capabilities: AgentCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    fn push_config_store(&self) -> Result<&dyn crate::PushConfigStore, A2AError> {
        self.push_config_store
            .as_deref()
            .ok_or_else(A2AError::push_notification_not_supported)
    }
}

#[async_trait]
impl RequestHandler for DefaultRequestHandler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        use futures::StreamExt;

        let task_id = req.message.task_id.clone().unwrap_or_else(new_task_id);
        let context_id = req
            .message
            .context_id
            .clone()
            .unwrap_or_else(new_context_id);

        let stored = self.task_store.get(&task_id).await.ok().flatten();

        // Create or update task in store
        let task = if let Some(existing) = stored {
            existing
        } else {
            let task = Task {
                id: task_id.clone(),
                context_id: context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Submitted,
                    message: None,
                    timestamp: Some(chrono::Utc::now()),
                },
                artifacts: None,
                history: Some(vec![req.message.clone()]),
                metadata: None,
            };
            self.task_store.create(task.clone()).await?;
            task
        };

        let exec_ctx = crate::ExecutorContext {
            message: Some(req.message.clone()),
            task_id: task_id.clone(),
            stored_task: Some(task.clone()),
            context_id,
            metadata: req.metadata.clone(),
            user: None,
            service_params: params.clone(),
            tenant: req.tenant.clone(),
        };

        let mut stream = self.executor.execute(exec_ctx);
        let mut final_task = task;

        // Collect events until terminal state
        while let Some(event) = stream.next().await {
            match event? {
                StreamResponse::Task(t) => {
                    final_task = t;
                }
                StreamResponse::StatusUpdate(su) => {
                    final_task.status = su.status;
                }
                StreamResponse::ArtifactUpdate(au) => {
                    let artifacts = final_task.artifacts.get_or_insert_with(Vec::new);
                    artifacts.push(au.artifact);
                }
                StreamResponse::Message(msg) => {
                    return Ok(SendMessageResponse::Message(msg));
                }
            }
            if final_task.status.state.is_terminal() {
                break;
            }
        }

        self.task_store.update(final_task.clone()).await?;
        Ok(SendMessageResponse::Task(final_task))
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let task_id = req.message.task_id.clone().unwrap_or_else(new_task_id);
        let context_id = req
            .message
            .context_id
            .clone()
            .unwrap_or_else(new_context_id);

        let stored = self.task_store.get(&task_id).await.ok().flatten();

        if stored.is_none() {
            let task = Task {
                id: task_id.clone(),
                context_id: context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Submitted,
                    message: None,
                    timestamp: Some(chrono::Utc::now()),
                },
                artifacts: None,
                history: Some(vec![req.message.clone()]),
                metadata: None,
            };
            self.task_store.create(task).await?;
        }

        let exec_ctx = crate::ExecutorContext {
            message: Some(req.message),
            task_id,
            stored_task: stored,
            context_id,
            metadata: req.metadata,
            user: None,
            service_params: params.clone(),
            tenant: req.tenant,
        };

        Ok(self.executor.execute(exec_ctx))
    }

    async fn get_task(
        &self,
        _params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        self.task_store
            .get(&req.id)
            .await?
            .ok_or_else(|| A2AError::task_not_found(&req.id))
    }

    async fn list_tasks(
        &self,
        _params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        self.task_store.list(&req).await
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let task = self
            .task_store
            .get(&req.id)
            .await?
            .ok_or_else(|| A2AError::task_not_found(&req.id))?;

        if task.status.state.is_terminal() {
            return Err(A2AError::task_not_cancelable(&req.id));
        }

        use futures::StreamExt;

        let exec_ctx = crate::ExecutorContext {
            message: None,
            task_id: req.id.clone(),
            stored_task: Some(task.clone()),
            context_id: task.context_id.clone(),
            metadata: req.metadata,
            user: None,
            service_params: params.clone(),
            tenant: req.tenant,
        };

        let mut stream = self.executor.cancel(exec_ctx);
        let mut final_task = task;

        while let Some(event) = stream.next().await {
            match event? {
                StreamResponse::Task(t) => final_task = t,
                StreamResponse::StatusUpdate(su) => final_task.status = su.status,
                _ => {}
            }
        }

        self.task_store.update(final_task.clone()).await?;
        Ok(final_task)
    }

    async fn subscribe_to_task(
        &self,
        _params: &ServiceParams,
        _req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        Err(A2AError::unsupported_operation(
            "subscribe_to_task not yet implemented",
        ))
    }

    async fn create_push_config(
        &self,
        _params: &ServiceParams,
        req: CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let saved = self
            .push_config_store()?
            .save(&req.task_id, req.config)
            .await?;
        Ok(TaskPushNotificationConfig {
            task_id: req.task_id,
            config: saved,
            tenant: req.tenant,
        })
    }

    async fn get_push_config(
        &self,
        _params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let config = self.push_config_store()?.get(&req.task_id, &req.id).await?;
        Ok(TaskPushNotificationConfig {
            task_id: req.task_id,
            config,
            tenant: req.tenant,
        })
    }

    async fn list_push_configs(
        &self,
        _params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let mut configs = self.push_config_store()?.list(&req.task_id).await?;
        configs.sort_by(|left, right| left.id.cmp(&right.id));

        let page_size = match req.page_size {
            Some(size) if size > 0 => size as usize,
            _ => 50,
        };
        let start = req
            .page_token
            .as_deref()
            .and_then(|token| token.parse::<usize>().ok())
            .unwrap_or(0)
            .min(configs.len());
        let end = (start + page_size).min(configs.len());
        let next_page_token = (end < configs.len()).then(|| end.to_string());

        Ok(ListTaskPushNotificationConfigsResponse {
            configs: configs[start..end]
                .iter()
                .cloned()
                .map(|config| TaskPushNotificationConfig {
                    task_id: req.task_id.clone(),
                    config,
                    tenant: req.tenant.clone(),
                })
                .collect(),
            next_page_token,
        })
    }

    async fn delete_push_config(
        &self,
        _params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        self.push_config_store()?
            .delete(&req.task_id, &req.id)
            .await
    }

    async fn get_extended_agent_card(
        &self,
        _params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        Err(A2AError::unsupported_operation(
            "extended agent card not configured",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::ExecutorContext;
    use crate::push::InMemoryPushConfigStore;
    use crate::task_store::InMemoryTaskStore;
    use futures::stream;

    struct EchoExecutor;

    impl crate::AgentExecutor for EchoExecutor {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let task = Task {
                id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: ctx.message,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            };
            Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
        }

        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let task = Task {
                id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            };
            Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
        }
    }

    fn make_handler() -> DefaultRequestHandler {
        DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
    }

    fn make_handler_with_push_configs() -> DefaultRequestHandler {
        DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
            .with_push_config_store(InMemoryPushConfigStore::new())
    }

    fn make_message() -> Message {
        Message::new(Role::User, vec![Part::text("hello")])
    }

    #[tokio::test]
    async fn test_send_message_creates_task() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = SendMessageRequest {
            message: make_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let result = handler.send_message(&params, req).await.unwrap();
        match result {
            SendMessageResponse::Task(task) => {
                assert_eq!(task.status.state, TaskState::Completed);
            }
            _ => panic!("expected Task response"),
        }
    }

    #[tokio::test]
    async fn test_send_streaming_message() {
        use futures::StreamExt;
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = SendMessageRequest {
            message: make_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let mut stream = handler.send_streaming_message(&params, req).await.unwrap();
        let event = stream.next().await.unwrap().unwrap();
        assert!(matches!(event, StreamResponse::Task(_)));
    }

    #[tokio::test]
    async fn test_get_task_not_found() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = GetTaskRequest {
            id: "nonexistent".into(),
            history_length: None,
            tenant: None,
        };
        let result = handler.get_task(&params, req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_task_after_send() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let mut msg = make_message();
        msg.task_id = Some("t1".into());
        msg.context_id = Some("c1".into());
        let req = SendMessageRequest {
            message: msg,
            configuration: None,
            metadata: None,
            tenant: None,
        };
        handler.send_message(&params, req).await.unwrap();
        let task = handler
            .get_task(
                &params,
                GetTaskRequest {
                    id: "t1".into(),
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(task.id, "t1");
    }

    #[tokio::test]
    async fn test_list_tasks() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = ListTasksRequest {
            context_id: None,
            status: None,
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = handler.list_tasks(&params, req).await.unwrap();
        assert!(resp.tasks.is_empty());
    }

    #[tokio::test]
    async fn test_cancel_task_not_found() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = CancelTaskRequest {
            id: "nonexistent".into(),
            metadata: None,
            tenant: None,
        };
        let result = handler.cancel_task(&params, req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_cancel_task_working() {
        let handler = make_handler();
        let params = ServiceParams::new();
        // First create a task via the store directly
        let task = Task {
            id: "t2".into(),
            context_id: "c2".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        handler.task_store.create(task).await.unwrap();
        let req = CancelTaskRequest {
            id: "t2".into(),
            metadata: None,
            tenant: None,
        };
        let result = handler.cancel_task(&params, req).await.unwrap();
        assert_eq!(result.status.state, TaskState::Canceled);
    }

    #[tokio::test]
    async fn test_cancel_task_already_completed() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let task = Task {
            id: "t3".into(),
            context_id: "c3".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        handler.task_store.create(task).await.unwrap();
        let req = CancelTaskRequest {
            id: "t3".into(),
            metadata: None,
            tenant: None,
        };
        let result = handler.cancel_task(&params, req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_subscribe_not_implemented() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = SubscribeToTaskRequest {
            id: "t1".into(),
            tenant: None,
        };
        let result = handler.subscribe_to_task(&params, req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_push_config_not_supported_without_store() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let result = handler
            .create_push_config(
                &params,
                CreateTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    config: PushNotificationConfig {
                        url: "http://example.com".into(),
                        id: None,
                        token: None,
                        authentication: None,
                    },
                    tenant: None,
                },
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_push_config_crud_with_store() {
        let handler = make_handler_with_push_configs();
        let params = ServiceParams::new();

        let created = handler
            .create_push_config(
                &params,
                CreateTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    config: PushNotificationConfig {
                        url: "https://example.com/first".into(),
                        id: Some("cfg-1".into()),
                        token: None,
                        authentication: None,
                    },
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(created.config.id.as_deref(), Some("cfg-1"));

        handler
            .create_push_config(
                &params,
                CreateTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    config: PushNotificationConfig {
                        url: "https://example.com/second".into(),
                        id: Some("cfg-2".into()),
                        token: None,
                        authentication: None,
                    },
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();

        let fetched = handler
            .get_push_config(
                &params,
                GetTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    id: "cfg-1".into(),
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(fetched.config.url, "https://example.com/first");

        let first_page = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: Some(1),
                    page_token: Some("0".into()),
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(first_page.configs.len(), 1);
        assert_eq!(first_page.configs[0].config.id.as_deref(), Some("cfg-1"));
        assert_eq!(first_page.next_page_token.as_deref(), Some("1"));

        let default_page = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: Some(0),
                    page_token: None,
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(default_page.configs.len(), 2);
        assert!(default_page.next_page_token.is_none());

        handler
            .delete_push_config(
                &params,
                DeleteTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    id: "cfg-1".into(),
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();

        let remaining = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: None,
                    page_token: None,
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(remaining.configs.len(), 1);
        assert_eq!(remaining.configs[0].config.id.as_deref(), Some("cfg-2"));
    }

    #[tokio::test]
    async fn test_extended_agent_card_not_configured() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let result = handler
            .get_extended_agent_card(&params, GetExtendedAgentCardRequest { tenant: None })
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn test_with_capabilities() {
        let handler = make_handler().with_capabilities(AgentCapabilities {
            streaming: Some(true),
            push_notifications: None,
            extensions: None,
            extended_agent_card: None,
        });
        assert_eq!(handler.capabilities.streaming, Some(true));
    }

    #[test]
    fn test_with_push_config_store_enables_push_capability() {
        let handler = make_handler_with_push_configs();
        assert_eq!(handler.capabilities.push_notifications, Some(true));
    }
}
