// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
pub mod agent_card;
pub mod executor;
pub mod handler;
pub mod jsonrpc;
pub mod middleware;
pub mod push;
pub mod rest;
pub mod sse;
pub mod task_store;
#[cfg(feature = "rustls-tls")]
pub mod tls;

pub use agent_card::{AgentCardProducer, StaticAgentCard, WELL_KNOWN_AGENT_CARD_PATH};
pub use executor::{AgentExecutor, ExecutorContext};
pub use handler::{DefaultRequestHandler, RequestHandler};
pub use middleware::{CallContext, CallInterceptor, InterceptedHandler, ServiceParams, User};
pub use push::{HttpPushSender, InMemoryPushConfigStore, PushConfigStore};
pub use task_store::{InMemoryTaskStore, TaskStore};

#[cfg(test)]
pub(crate) mod test_util {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use a2a::*;
    use async_trait::async_trait;
    use futures::stream::BoxStream;

    use crate::handler::RequestHandler;
    use crate::middleware::ServiceParams;

    pub fn install_crypto_provider() {
        #[cfg(feature = "rustls-tls")]
        {
            let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        }
    }

    pub(crate) type CapturedParams = Arc<Mutex<HashMap<&'static str, ServiceParams>>>;

    /// `RequestHandler` stub that records the `ServiceParams` it sees per method.
    ///
    /// Used by transport-layer tests to assert that incoming request headers
    /// are surfaced through to the `RequestHandler` boundary — which is exactly
    /// what the HTTP / JSON-RPC transports are responsible for. Returns minimal
    /// stub responses so tests don't depend on `DefaultRequestHandler` behavior.
    pub(crate) struct CapturingHandler {
        captured: CapturedParams,
    }

    impl CapturingHandler {
        pub fn new() -> (Arc<Self>, CapturedParams) {
            let captured = Arc::new(Mutex::new(HashMap::new()));
            let handler = Arc::new(CapturingHandler {
                captured: captured.clone(),
            });
            (handler, captured)
        }

        fn record(&self, method: &'static str, params: &ServiceParams) {
            self.captured.lock().unwrap().insert(method, params.clone());
        }
    }

    #[async_trait]
    impl RequestHandler for CapturingHandler {
        async fn send_message(
            &self,
            params: &ServiceParams,
            _req: SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            self.record("send_message", params);
            Err(A2AError::internal("stub"))
        }

        async fn send_streaming_message(
            &self,
            params: &ServiceParams,
            _req: SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            self.record("send_streaming_message", params);
            Ok(Box::pin(futures::stream::empty()))
        }

        async fn get_task(
            &self,
            params: &ServiceParams,
            _req: GetTaskRequest,
        ) -> Result<Task, A2AError> {
            self.record("get_task", params);
            Err(A2AError::task_not_found("stub"))
        }

        async fn list_tasks(
            &self,
            params: &ServiceParams,
            _req: ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            self.record("list_tasks", params);
            Ok(ListTasksResponse {
                tasks: Vec::new(),
                next_page_token: String::new(),
                page_size: 0,
                total_size: 0,
            })
        }

        async fn cancel_task(
            &self,
            params: &ServiceParams,
            _req: CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            self.record("cancel_task", params);
            Err(A2AError::task_not_found("stub"))
        }

        async fn subscribe_to_task(
            &self,
            params: &ServiceParams,
            _req: SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            self.record("subscribe_to_task", params);
            Ok(Box::pin(futures::stream::empty()))
        }

        async fn create_push_config(
            &self,
            params: &ServiceParams,
            req: TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            self.record("create_push_config", params);
            Ok(req)
        }

        async fn get_push_config(
            &self,
            params: &ServiceParams,
            _req: GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            self.record("get_push_config", params);
            Err(A2AError::internal("stub"))
        }

        async fn list_push_configs(
            &self,
            params: &ServiceParams,
            _req: ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            self.record("list_push_configs", params);
            Ok(ListTaskPushNotificationConfigsResponse {
                configs: Vec::new(),
                next_page_token: None,
            })
        }

        async fn delete_push_config(
            &self,
            params: &ServiceParams,
            _req: DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            self.record("delete_push_config", params);
            Ok(())
        }

        async fn get_extended_agent_card(
            &self,
            params: &ServiceParams,
            _req: GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            self.record("get_extended_agent_card", params);
            Err(A2AError::internal("stub"))
        }
    }

    /// Assert that `method` was called with a service-params entry matching `header = expected`.
    #[track_caller]
    pub(crate) fn assert_header_captured(
        captured: &CapturedParams,
        method: &'static str,
        header: &str,
        expected: &str,
    ) {
        let captured = captured.lock().unwrap();
        let params = captured
            .get(method)
            .unwrap_or_else(|| panic!("{method} should have been called; captured={captured:?}"));
        assert_eq!(
            params.get(header),
            Some(&vec![expected.to_string()]),
            "{header} should be propagated to {method}; got {params:?}",
        );
    }
}
