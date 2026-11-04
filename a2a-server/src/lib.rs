// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Server framework for implementing A2A v1 agents.
//!
//! Published as `a2a-server-lf` and imported as `a2a_server`. The protocol
//! types come from the [`a2a`] crate.
//!
//! # How a request flows
//!
//! A protocol binding turns a wire request into a call on a
//! [`RequestHandler`]. [`DefaultRequestHandler`] is the implementation most
//! agents use: it manages the task lifecycle, saves tasks in a [`TaskStore`],
//! and hands the work to your [`AgentExecutor`], which returns a stream of
//! events.
//!
//! # Where to start
//!
//! 1. Implement [`AgentExecutor`] with the agent's logic. [`ExecutorContext`]
//!    describes the request being served.
//! 2. Build a [`DefaultRequestHandler`] from the executor and a
//!    [`TaskStore`]. [`InMemoryTaskStore`] is provided.
//! 3. Mount a binding on an axum router with [`jsonrpc::jsonrpc_router`] or
//!    [`rest::rest_router`]. The gRPC and SLIMRPC bindings are in the
//!    `a2a-grpc` and `a2a-slimrpc` crates.
//! 4. Serve the agent card at [`WELL_KNOWN_AGENT_CARD_PATH`] with
//!    [`agent_card::agent_card_router`], from a [`StaticAgentCard`] or your
//!    own [`AgentCardProducer`].
//!
//! # Also in this crate
//!
//! - [`sse`]: streaming responses as server-sent events.
//! - [`push`]: push notification configs ([`PushConfigStore`]) and their
//!   delivery ([`HttpPushSender`]).
//! - [`middleware`]: [`CallInterceptor`]s around a handler, applied with
//!   [`InterceptedHandler`].
//! - [`RequestAuthorizer`]: decides whether a request may proceed; set it
//!   with [`DefaultRequestHandler::with_authorizer`].
//! - [`pagination`]: the page limits the list endpoints share.
//!
//! # Feature flags
//!
//! - `rustls-tls` (default): rustls, with its default crypto provider, for
//!   push notification delivery and for serving over HTTPS through the `tls`
//!   module.
//! - `rustls-no-provider`: the same, with the crypto provider installed by
//!   the application.
//! - `native-tls`: the platform's native TLS stack for push notification
//!   delivery. The `tls` module is not available with this flag alone.
//!
//! `fuzzing` is for the fuzz targets in this repository, not for dependents.

pub mod agent_card;
pub mod executor;
pub mod handler;
pub mod jsonrpc;
pub mod middleware;
pub mod pagination;
pub mod push;
pub mod rest;
pub mod sse;
pub mod task_store;

// Push-URL validation internals reached by the fuzz target in fuzz/ — see
// #238. Thin wrappers, not re-exports: the originals stay crate-private.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzz_support {
    use a2a::A2AError;

    pub fn validate_push_url(url: &str) -> Result<(), A2AError> {
        crate::push::sender::validate_push_url(url)
    }

    pub fn is_blocked_ip(ip: std::net::IpAddr) -> bool {
        crate::push::sender::is_blocked_ip(ip)
    }
}

#[cfg(any(feature = "rustls-tls", feature = "rustls-no-provider"))]
pub mod tls;

pub use agent_card::{AgentCardProducer, StaticAgentCard, WELL_KNOWN_AGENT_CARD_PATH};
pub use executor::{AgentExecutor, ClientInput, ClientInputSender, ClientInputs, ExecutorContext};
pub use handler::{
    DefaultRequestHandler, ExtendedAgentCardResolver, LiveResponse, RequestAuthorizer,
    RequestHandler,
};
pub use middleware::{CallContext, CallInterceptor, InterceptedHandler, ServiceParams, User};
pub use push::{HttpPushSender, HttpPushSenderConfig, InMemoryPushConfigStore, PushConfigStore};
pub use task_store::{InMemoryTaskStore, TaskStore};

/// Build an `INTERNAL_ERROR` whose client-visible message is generic, while
/// the real cause is logged server-side (BUG-12 / CWE-209).
///
/// Call this where the **server itself** failed — a serialization fault, a
/// broken invariant — and never at a response boundary. A2A gives an
/// `AgentExecutor` no code other than `INTERNAL_ERROR` to report its own
/// failure with (`error_code` has nothing between `INVALID_AGENT_RESPONSE`
/// and the JSON-RPC generics), and `A2AError` carries no marker for where an
/// error came from. Sanitizing by code at the boundary therefore cannot tell
/// a serialization fault from an agent saying why it failed, and would
/// replace both with "Internal error".
pub(crate) fn sanitized_internal_error(detail: impl std::fmt::Display) -> a2a::A2AError {
    tracing::error!(error = %detail, "internal error; returning a generic message to the client");
    a2a::A2AError::internal("Internal error")
}

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

    pub(crate) async fn seeded_task_store(id: &str) -> crate::InMemoryTaskStore {
        use crate::TaskStore;
        let store = crate::InMemoryTaskStore::new();
        store
            .create(Task {
                id: id.into(),
                context_id: "c1".into(),
                generation: 0,
                status: TaskStatus {
                    state: TaskState::Submitted,
                    message: None,
                    timestamp: None,
                },
                history: None,
                artifacts: None,
                timeline: None,
                metadata: None,
            })
            .await
            .unwrap();
        store
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
        tenants: Mutex<HashMap<&'static str, Option<String>>>,
    }

    impl CapturingHandler {
        pub fn new() -> (Arc<Self>, CapturedParams) {
            let captured = Arc::new(Mutex::new(HashMap::new()));
            let handler = Arc::new(CapturingHandler {
                captured: captured.clone(),
                tenants: Mutex::new(HashMap::new()),
            });
            (handler, captured)
        }

        fn record(&self, method: &'static str, params: &ServiceParams, tenant: Option<&str>) {
            self.captured.lock().unwrap().insert(method, params.clone());
            self.tenants
                .lock()
                .unwrap()
                .insert(method, tenant.map(str::to_owned));
        }

        /// The tenant of `method`'s last call; `None` if it was not called since.
        pub fn take_tenant(&self, method: &str) -> Option<Option<String>> {
            self.tenants.lock().unwrap().remove(method)
        }
    }

    #[async_trait]
    impl RequestHandler for CapturingHandler {
        async fn send_message(
            &self,
            params: &ServiceParams,
            req: SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            self.record("send_message", params, req.tenant.as_deref());
            Err(A2AError::internal("stub"))
        }

        async fn send_streaming_message(
            &self,
            params: &ServiceParams,
            req: SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            self.record("send_streaming_message", params, req.tenant.as_deref());
            Ok(Box::pin(futures::stream::empty()))
        }

        async fn send_live_request(
            &self,
            params: &ServiceParams,
            request: StreamRequest,
        ) -> Result<crate::handler::LiveResponse, A2AError> {
            self.record("send_live_request", params, request.tenant.as_deref());
            let tenant = match &request.payload {
                StreamRequestPayload::SendMessage(message) => message.tenant.as_deref(),
                StreamRequestPayload::Subscribe(request) => request.tenant.as_deref(),
                _ => None,
            };
            self.record("live_payload", params, tenant);
            Ok(crate::handler::LiveResponse::Stream(Box::pin(
                futures::stream::empty(),
            )))
        }

        async fn get_task(
            &self,
            params: &ServiceParams,
            req: GetTaskRequest,
        ) -> Result<Task, A2AError> {
            self.record("get_task", params, req.tenant.as_deref());
            Err(A2AError::task_not_found("stub"))
        }

        async fn list_tasks(
            &self,
            params: &ServiceParams,
            req: ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            self.record("list_tasks", params, req.tenant.as_deref());
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
            req: CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            self.record("cancel_task", params, req.tenant.as_deref());
            Err(A2AError::task_not_found("stub"))
        }

        async fn subscribe_to_task(
            &self,
            params: &ServiceParams,
            req: SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            self.record("subscribe_to_task", params, req.tenant.as_deref());
            Ok(Box::pin(futures::stream::empty()))
        }

        async fn create_push_config(
            &self,
            params: &ServiceParams,
            req: TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            self.record("create_push_config", params, req.tenant.as_deref());
            Ok(req)
        }

        async fn get_push_config(
            &self,
            params: &ServiceParams,
            req: GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            self.record("get_push_config", params, req.tenant.as_deref());
            Err(A2AError::internal("stub"))
        }

        async fn list_push_configs(
            &self,
            params: &ServiceParams,
            req: ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            self.record("list_push_configs", params, req.tenant.as_deref());
            Ok(ListTaskPushNotificationConfigsResponse {
                configs: Vec::new(),
                next_page_token: None,
            })
        }

        async fn delete_push_config(
            &self,
            params: &ServiceParams,
            req: DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            self.record("delete_push_config", params, req.tenant.as_deref());
            Ok(())
        }

        async fn get_extended_agent_card(
            &self,
            params: &ServiceParams,
            req: GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            self.record("get_extended_agent_card", params, req.tenant.as_deref());
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
