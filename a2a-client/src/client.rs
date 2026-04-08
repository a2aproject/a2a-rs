// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::jsonrpc::methods;
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

    async fn apply_before(&self, method: &str) -> Result<ServiceParams, A2AError> {
        let mut params = self.params();
        for interceptor in &self.interceptors {
            interceptor.before(method, &mut params).await?;
        }
        Ok(params)
    }

    async fn apply_after(
        &self,
        method: &str,
        result: &Result<(), A2AError>,
    ) -> Result<(), A2AError> {
        for interceptor in self.interceptors.iter().rev() {
            interceptor.after(method, result).await?;
        }
        Ok(())
    }

    async fn finish_call<T>(
        &self,
        method: &str,
        result: Result<T, A2AError>,
    ) -> Result<T, A2AError> {
        let status = result.as_ref().map(|_| ()).map_err(Clone::clone);
        let after_result = self.apply_after(method, &status).await;

        match (result, after_result) {
            (Ok(value), Ok(())) => Ok(value),
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
        }
    }

    pub async fn send_message(
        &self,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let params = self.apply_before(methods::SEND_MESSAGE).await?;
        let result = self.transport.send_message(&params, req).await;
        self.finish_call(methods::SEND_MESSAGE, result).await
    }

    pub async fn send_streaming_message(
        &self,
        req: &SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let params = self.apply_before(methods::SEND_STREAMING_MESSAGE).await?;
        let result = self.transport.send_streaming_message(&params, req).await;
        self.finish_call(methods::SEND_STREAMING_MESSAGE, result)
            .await
    }

    pub async fn get_task(&self, req: &GetTaskRequest) -> Result<Task, A2AError> {
        let params = self.apply_before(methods::GET_TASK).await?;
        let result = self.transport.get_task(&params, req).await;
        self.finish_call(methods::GET_TASK, result).await
    }

    pub async fn list_tasks(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        let params = self.apply_before(methods::LIST_TASKS).await?;
        let result = self.transport.list_tasks(&params, req).await;
        self.finish_call(methods::LIST_TASKS, result).await
    }

    pub async fn cancel_task(&self, req: &CancelTaskRequest) -> Result<Task, A2AError> {
        let params = self.apply_before(methods::CANCEL_TASK).await?;
        let result = self.transport.cancel_task(&params, req).await;
        self.finish_call(methods::CANCEL_TASK, result).await
    }

    pub async fn subscribe_to_task(
        &self,
        req: &SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let params = self.apply_before(methods::SUBSCRIBE_TO_TASK).await?;
        let result = self.transport.subscribe_to_task(&params, req).await;
        self.finish_call(methods::SUBSCRIBE_TO_TASK, result).await
    }

    pub async fn create_push_config(
        &self,
        req: &CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let params = self.apply_before(methods::CREATE_PUSH_CONFIG).await?;
        let result = self.transport.create_push_config(&params, req).await;
        self.finish_call(methods::CREATE_PUSH_CONFIG, result).await
    }

    pub async fn get_push_config(
        &self,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let params = self.apply_before(methods::GET_PUSH_CONFIG).await?;
        let result = self.transport.get_push_config(&params, req).await;
        self.finish_call(methods::GET_PUSH_CONFIG, result).await
    }

    pub async fn list_push_configs(
        &self,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let params = self.apply_before(methods::LIST_PUSH_CONFIGS).await?;
        let result = self.transport.list_push_configs(&params, req).await;
        self.finish_call(methods::LIST_PUSH_CONFIGS, result).await
    }

    pub async fn delete_push_config(
        &self,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        let params = self.apply_before(methods::DELETE_PUSH_CONFIG).await?;
        let result = self.transport.delete_push_config(&params, req).await;
        self.finish_call(methods::DELETE_PUSH_CONFIG, result).await
    }

    pub async fn get_extended_agent_card(
        &self,
        req: &GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        let params = self.apply_before(methods::GET_EXTENDED_AGENT_CARD).await?;
        let result = self.transport.get_extended_agent_card(&params, req).await;
        self.finish_call(methods::GET_EXTENDED_AGENT_CARD, result)
            .await
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
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockTransportState {
        calls: Mutex<Vec<(String, ServiceParams)>>,
        send_message_error: Mutex<Option<A2AError>>,
    }

    /// Mock transport that returns canned responses.
    struct MockTransport {
        state: Arc<MockTransportState>,
    }

    impl MockTransport {
        fn new() -> (Self, Arc<MockTransportState>) {
            let state = Arc::new(MockTransportState::default());
            (
                MockTransport {
                    state: state.clone(),
                },
                state,
            )
        }

        fn record(&self, method: &str, params: &ServiceParams) {
            self.state
                .calls
                .lock()
                .unwrap()
                .push((method.to_string(), params.clone()));
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn send_message(
            &self,
            params: &ServiceParams,
            _req: &SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            self.record(methods::SEND_MESSAGE, params);
            if let Some(error) = self.state.send_message_error.lock().unwrap().clone() {
                return Err(error);
            }
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
            params: &ServiceParams,
            _req: &SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            self.record(methods::SEND_STREAMING_MESSAGE, params);
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
            params: &ServiceParams,
            req: &GetTaskRequest,
        ) -> Result<Task, A2AError> {
            self.record(methods::GET_TASK, params);
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
            params: &ServiceParams,
            _req: &ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            self.record(methods::LIST_TASKS, params);
            Ok(ListTasksResponse {
                tasks: vec![],
                next_page_token: String::new(),
                page_size: 0,
                total_size: 0,
            })
        }

        async fn cancel_task(
            &self,
            params: &ServiceParams,
            req: &CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            self.record(methods::CANCEL_TASK, params);
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
            params: &ServiceParams,
            _req: &SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            self.record(methods::SUBSCRIBE_TO_TASK, params);
            Ok(Box::pin(stream::empty()))
        }

        async fn create_push_config(
            &self,
            params: &ServiceParams,
            req: &CreateTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            self.record(methods::CREATE_PUSH_CONFIG, params);
            Ok(TaskPushNotificationConfig {
                task_id: req.task_id.clone(),
                config: req.config.clone(),
                tenant: None,
            })
        }

        async fn get_push_config(
            &self,
            params: &ServiceParams,
            req: &GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            self.record(methods::GET_PUSH_CONFIG, params);
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
            params: &ServiceParams,
            _req: &ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            self.record(methods::LIST_PUSH_CONFIGS, params);
            Ok(ListTaskPushNotificationConfigsResponse {
                configs: vec![],
                next_page_token: None,
            })
        }

        async fn delete_push_config(
            &self,
            params: &ServiceParams,
            _req: &DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            self.record(methods::DELETE_PUSH_CONFIG, params);
            Ok(())
        }

        async fn get_extended_agent_card(
            &self,
            params: &ServiceParams,
            _req: &GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            self.record(methods::GET_EXTENDED_AGENT_CARD, params);
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
        let (transport, _) = MockTransport::new();
        A2AClient::new(Box::new(transport))
    }

    struct RecordingInterceptor {
        name: &'static str,
        events: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait]
    impl CallInterceptor for RecordingInterceptor {
        async fn before(&self, _method: &str, params: &mut ServiceParams) -> Result<(), A2AError> {
            self.events
                .lock()
                .unwrap()
                .push(format!("before:{}", self.name));
            params
                .entry("X-Interceptor".to_string())
                .or_default()
                .push(self.name.to_string());
            Ok(())
        }

        async fn after(
            &self,
            _method: &str,
            result: &Result<(), A2AError>,
        ) -> Result<(), A2AError> {
            let status = if result.is_ok() { "ok" } else { "err" };
            self.events
                .lock()
                .unwrap()
                .push(format!("after:{}:{status}", self.name));
            Ok(())
        }
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
    async fn test_send_message_applies_interceptors_and_reverses_after_order() {
        let (transport, state) = MockTransport::new();
        let events = Arc::new(Mutex::new(Vec::new()));
        let client = A2AClient::new(Box::new(transport)).with_interceptors(vec![
            Arc::new(RecordingInterceptor {
                name: "first",
                events: events.clone(),
            }),
            Arc::new(RecordingInterceptor {
                name: "second",
                events: events.clone(),
            }),
        ]);

        let req = SendMessageRequest {
            message: Message::new(Role::User, vec![Part::text("hi")]),
            configuration: None,
            metadata: None,
            tenant: None,
        };

        client.send_message(&req).await.unwrap();

        let calls = state.calls.lock().unwrap();
        let params = &calls[0].1;
        assert_eq!(
            params.get("X-Interceptor").unwrap(),
            &vec!["first".to_string(), "second".to_string()]
        );

        let events = events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec![
                "before:first".to_string(),
                "before:second".to_string(),
                "after:second:ok".to_string(),
                "after:first:ok".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn test_send_message_preserves_transport_error_after_after_hooks() {
        let (transport, state) = MockTransport::new();
        *state.send_message_error.lock().unwrap() = Some(A2AError::internal("boom"));
        let events = Arc::new(Mutex::new(Vec::new()));
        let client = A2AClient::new(Box::new(transport)).with_interceptors(vec![Arc::new(
            RecordingInterceptor {
                name: "only",
                events: events.clone(),
            },
        )]);

        let req = SendMessageRequest {
            message: Message::new(Role::User, vec![Part::text("hi")]),
            configuration: None,
            metadata: None,
            tenant: None,
        };

        let err = client.send_message(&req).await.unwrap_err();
        assert_eq!(err.message, "boom");

        let events = events.lock().unwrap().clone();
        assert_eq!(
            events,
            vec!["before:only".to_string(), "after:only:err".to_string(),]
        );
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
