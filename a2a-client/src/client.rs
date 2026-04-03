// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use async_trait::async_trait;
use futures::stream::BoxStream;
use std::sync::Arc;

use crate::middleware::CallInterceptor;
use crate::transport::{ServiceParams, Transport};

/// High-level A2A client wrapping a transport with middleware.
pub struct A2AClient {
    transport: Box<dyn Transport>,
    interceptors: Vec<Arc<dyn CallInterceptor>>,
    default_params: ServiceParams,
}

impl A2AClient {
    pub fn new(transport: Box<dyn Transport>) -> Self {
        let mut default_params = ServiceParams::new();
        default_params.insert(SVC_PARAM_VERSION.to_string(), vec![VERSION.to_string()]);
        A2AClient {
            transport,
            interceptors: Vec::new(),
            default_params,
        }
    }

    pub fn with_interceptors(mut self, interceptors: Vec<Arc<dyn CallInterceptor>>) -> Self {
        self.interceptors = interceptors;
        self
    }

    fn params(&self) -> ServiceParams {
        self.default_params.clone()
    }

    pub async fn send_message(
        &self,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let params = self.params();
        self.transport.send_message(&params, req).await
    }

    pub async fn send_streaming_message(
        &self,
        req: &SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let params = self.params();
        self.transport.send_streaming_message(&params, req).await
    }

    pub async fn get_task(&self, req: &GetTaskRequest) -> Result<Task, A2AError> {
        let params = self.params();
        self.transport.get_task(&params, req).await
    }

    pub async fn list_tasks(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        let params = self.params();
        self.transport.list_tasks(&params, req).await
    }

    pub async fn cancel_task(&self, req: &CancelTaskRequest) -> Result<Task, A2AError> {
        let params = self.params();
        self.transport.cancel_task(&params, req).await
    }

    pub async fn subscribe_to_task(
        &self,
        req: &SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let params = self.params();
        self.transport.subscribe_to_task(&params, req).await
    }

    pub async fn create_push_config(
        &self,
        req: &CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let params = self.params();
        self.transport.create_push_config(&params, req).await
    }

    pub async fn get_push_config(
        &self,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let params = self.params();
        self.transport.get_push_config(&params, req).await
    }

    pub async fn list_push_configs(
        &self,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let params = self.params();
        self.transport.list_push_configs(&params, req).await
    }

    pub async fn delete_push_config(
        &self,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let params = self.params();
        self.transport.delete_push_config(&params, req).await
    }

    pub async fn get_extended_agent_card(
        &self,
        req: &GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        let params = self.params();
        self.transport.get_extended_agent_card(&params, req).await
    }

    pub async fn destroy(&self) -> Result<(), A2AError> {
        self.transport.destroy().await
    }
}

/// Convenience trait to extract client results.
#[async_trait]
pub trait SendMessageExt {
    async fn send_text(
        &self,
        text: impl Into<String> + Send,
    ) -> Result<SendMessageResponse, A2AError>;
}

#[async_trait]
impl SendMessageExt for A2AClient {
    async fn send_text(
        &self,
        text: impl Into<String> + Send,
    ) -> Result<SendMessageResponse, A2AError> {
        let msg = Message::new(Role::User, vec![Part::text(text)]);
        let req = SendMessageRequest {
            message: msg,
            configuration: None,
            metadata: None,
            tenant: None,
        };
        self.send_message(&req).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::event::StreamResponse;
    use futures::stream;

    /// Mock transport that returns canned responses.
    struct MockTransport;

    #[async_trait]
    impl Transport for MockTransport {
        async fn send_message(
            &self,
            _params: &ServiceParams,
            _req: &SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            Ok(SendMessageResponse::Task(Task {
                id: "t1".into(),
                context_id: "c1".into(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            }))
        }

        async fn send_streaming_message(
            &self,
            _params: &ServiceParams,
            _req: &SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            Ok(Box::pin(stream::once(async {
                Ok(StreamResponse::StatusUpdate(
                    a2a::event::TaskStatusUpdateEvent {
                        task_id: "t1".into(),
                        context_id: "c1".into(),
                        status: TaskStatus {
                            state: TaskState::Working,
                            message: None,
                            timestamp: None,
                        },
                        metadata: None,
                    },
                ))
            })))
        }

        async fn get_task(
            &self,
            _params: &ServiceParams,
            req: &GetTaskRequest,
        ) -> Result<Task, A2AError> {
            Ok(Task {
                id: req.id.clone(),
                context_id: "c1".into(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            })
        }

        async fn list_tasks(
            &self,
            _params: &ServiceParams,
            _req: &ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            Ok(ListTasksResponse {
                tasks: vec![],
                next_page_token: String::new(),
                page_size: 0,
                total_size: 0,
            })
        }

        async fn cancel_task(
            &self,
            _params: &ServiceParams,
            req: &CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            Ok(Task {
                id: req.id.clone(),
                context_id: "c1".into(),
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            })
        }

        async fn subscribe_to_task(
            &self,
            _params: &ServiceParams,
            _req: &SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            Ok(Box::pin(stream::empty()))
        }

        async fn create_push_config(
            &self,
            _params: &ServiceParams,
            req: &CreateTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            Ok(TaskPushNotificationConfig {
                task_id: req.task_id.clone(),
                config: req.config.clone(),
                tenant: None,
            })
        }

        async fn get_push_config(
            &self,
            _params: &ServiceParams,
            req: &GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            Ok(TaskPushNotificationConfig {
                task_id: req.task_id.clone(),
                config: PushNotificationConfig {
                    url: "http://example.com".into(),
                    id: Some(req.id.clone()),
                    token: None,
                    authentication: None,
                },
                tenant: None,
            })
        }

        async fn list_push_configs(
            &self,
            _params: &ServiceParams,
            _req: &ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            Ok(ListTaskPushNotificationConfigsResponse {
                configs: vec![],
                next_page_token: None,
            })
        }

        async fn delete_push_config(
            &self,
            _params: &ServiceParams,
            _req: &DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            Ok(())
        }

        async fn get_extended_agent_card(
            &self,
            _params: &ServiceParams,
            _req: &GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            Ok(AgentCard {
                name: "Test".into(),
                description: "Test agent".into(),
                version: "1.0".into(),
                supported_interfaces: vec![],
                capabilities: AgentCapabilities::default(),
                default_input_modes: vec!["text/plain".into()],
                default_output_modes: vec!["text/plain".into()],
                skills: vec![],
                provider: None,
                documentation_url: None,
                icon_url: None,
                security_schemes: None,
                security_requirements: None,
                signatures: None,
            })
        }

        async fn destroy(&self) -> Result<(), A2AError> {
            Ok(())
        }
    }

    fn make_client() -> A2AClient {
        A2AClient::new(Box::new(MockTransport))
    }

    #[test]
    fn test_new_sets_default_params() {
        let client = make_client();
        let params = client.params();
        assert!(params.contains_key(SVC_PARAM_VERSION));
    }

    #[test]
    fn test_with_interceptors() {
        let client = make_client().with_interceptors(vec![]);
        assert!(client.interceptors.is_empty());
    }

    #[tokio::test]
    async fn test_send_message() {
        let client = make_client();
        let req = SendMessageRequest {
            message: Message::new(Role::User, vec![Part::text("hi")]),
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let resp = client.send_message(&req).await.unwrap();
        assert!(matches!(resp, SendMessageResponse::Task(_)));
    }

    #[tokio::test]
    async fn test_send_streaming_message() {
        use futures::StreamExt;
        let client = make_client();
        let req = SendMessageRequest {
            message: Message::new(Role::User, vec![Part::text("hi")]),
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let mut stream = client.send_streaming_message(&req).await.unwrap();
        let item = stream.next().await.unwrap().unwrap();
        assert!(matches!(item, StreamResponse::StatusUpdate(_)));
    }

    #[tokio::test]
    async fn test_get_task() {
        let client = make_client();
        let req = GetTaskRequest {
            id: "t1".into(),
            history_length: None,
            tenant: None,
        };
        let task = client.get_task(&req).await.unwrap();
        assert_eq!(task.id, "t1");
    }

    #[tokio::test]
    async fn test_list_tasks() {
        let client = make_client();
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
        let resp = client.list_tasks(&req).await.unwrap();
        assert!(resp.tasks.is_empty());
    }

    #[tokio::test]
    async fn test_cancel_task() {
        let client = make_client();
        let req = CancelTaskRequest {
            id: "t1".into(),
            metadata: None,
            tenant: None,
        };
        let task = client.cancel_task(&req).await.unwrap();
        assert_eq!(task.status.state, TaskState::Canceled);
    }

    #[tokio::test]
    async fn test_subscribe_to_task() {
        let client = make_client();
        let req = SubscribeToTaskRequest {
            id: "t1".into(),
            tenant: None,
        };
        let _stream = client.subscribe_to_task(&req).await.unwrap();
    }

    #[tokio::test]
    async fn test_create_push_config() {
        let client = make_client();
        let req = CreateTaskPushNotificationConfigRequest {
            task_id: "t1".into(),
            config: PushNotificationConfig {
                url: "http://example.com".into(),
                id: None,
                token: None,
                authentication: None,
            },
            tenant: None,
        };
        let resp = client.create_push_config(&req).await.unwrap();
        assert_eq!(resp.task_id, "t1");
    }

    #[tokio::test]
    async fn test_get_push_config() {
        let client = make_client();
        let req = GetTaskPushNotificationConfigRequest {
            task_id: "t1".into(),
            id: "cfg1".into(),
            tenant: None,
        };
        let resp = client.get_push_config(&req).await.unwrap();
        assert_eq!(resp.config.id, Some("cfg1".into()));
    }

    #[tokio::test]
    async fn test_list_push_configs() {
        let client = make_client();
        let req = ListTaskPushNotificationConfigsRequest {
            task_id: "t1".into(),
            page_size: None,
            page_token: None,
            tenant: None,
        };
        let resp = client.list_push_configs(&req).await.unwrap();
        assert!(resp.configs.is_empty());
    }

    #[tokio::test]
    async fn test_delete_push_config() {
        let client = make_client();
        let req = DeleteTaskPushNotificationConfigRequest {
            task_id: "t1".into(),
            id: "cfg1".into(),
            tenant: None,
        };
        client.delete_push_config(&req).await.unwrap();
    }

    #[tokio::test]
    async fn test_get_extended_agent_card() {
        let client = make_client();
        let req = GetExtendedAgentCardRequest { tenant: None };
        let card = client.get_extended_agent_card(&req).await.unwrap();
        assert_eq!(card.name, "Test");
    }

    #[tokio::test]
    async fn test_destroy() {
        let client = make_client();
        client.destroy().await.unwrap();
    }
}
