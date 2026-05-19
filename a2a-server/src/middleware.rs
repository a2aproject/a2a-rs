// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::A2AError;
use async_trait::async_trait;
use axum::http::HeaderMap;
use serde_json::Value;
use std::collections::HashMap;

/// Service parameters — metadata from HTTP headers or gRPC metadata.
pub type ServiceParams = HashMap<String, Vec<String>>;

/// Build [`ServiceParams`] from an axum [`HeaderMap`].
///
/// Header names are normalized to lowercase (axum does this for us), values
/// that aren't valid ASCII are silently skipped, and multi-valued headers
/// produce a `Vec<String>` in insertion order.
pub(crate) fn extract_service_params(headers: &HeaderMap) -> ServiceParams {
    let mut params = ServiceParams::new();
    for (name, value) in headers {
        if let Ok(v) = value.to_str() {
            params
                .entry(name.as_str().to_owned())
                .or_default()
                .push(v.to_owned());
        }
    }
    params
}

/// Authenticated user information.
#[derive(Debug, Clone)]
pub struct User {
    pub name: String,
    pub authenticated: bool,
    pub attributes: HashMap<String, Value>,
}

impl User {
    pub fn authenticated(name: impl Into<String>) -> Self {
        User {
            name: name.into(),
            authenticated: true,
            attributes: HashMap::new(),
        }
    }
}

/// Context for a single request being processed.
pub struct CallContext {
    pub method: String,
    pub service_params: ServiceParams,
    pub tenant: Option<String>,
    pub user: Option<User>,
}

impl CallContext {
    pub fn new(method: impl Into<String>, params: ServiceParams) -> Self {
        CallContext {
            method: method.into(),
            service_params: params,
            tenant: None,
            user: None,
        }
    }
}

/// Server-side interceptor for modifying requests and responses.
#[async_trait]
pub trait CallInterceptor: Send + Sync + 'static {
    async fn before(&self, ctx: &mut CallContext, request: &Value) -> Result<(), A2AError> {
        let _ = (ctx, request);
        Ok(())
    }

    async fn after(
        &self,
        ctx: &CallContext,
        result: &Result<Value, A2AError>,
    ) -> Result<(), A2AError> {
        let _ = (ctx, result);
        Ok(())
    }
}

/// Wraps a [`RequestHandler`](crate::RequestHandler) with interceptors.
pub struct InterceptedHandler<H> {
    pub handler: H,
    pub interceptors: Vec<Box<dyn CallInterceptor>>,
}

impl<H> InterceptedHandler<H> {
    pub fn new(handler: H) -> Self {
        InterceptedHandler {
            handler,
            interceptors: Vec::new(),
        }
    }

    pub fn with_interceptor(mut self, interceptor: impl CallInterceptor) -> Self {
        self.interceptors.push(Box::new(interceptor));
        self
    }
}

/// Server-side logging interceptor.
pub struct LoggingInterceptor;

#[async_trait]
impl CallInterceptor for LoggingInterceptor {
    async fn before(&self, ctx: &mut CallContext, _request: &Value) -> Result<(), A2AError> {
        tracing::info!(method = %ctx.method, "A2A server request");
        Ok(())
    }

    async fn after(
        &self,
        ctx: &CallContext,
        result: &Result<Value, A2AError>,
    ) -> Result<(), A2AError> {
        match result {
            Ok(_) => tracing::info!(method = %ctx.method, "A2A server response"),
            Err(e) => tracing::warn!(method = %ctx.method, error = %e, "A2A server error"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    #[test]
    fn test_extract_service_params_empty() {
        let headers = HeaderMap::new();
        let params = extract_service_params(&headers);
        assert!(params.is_empty());
    }

    #[test]
    fn test_extract_service_params_basic() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            HeaderValue::from_static("Bearer token-abc"),
        );
        headers.insert("x-tenant-id", HeaderValue::from_static("acme"));

        let params = extract_service_params(&headers);

        assert_eq!(
            params.get("authorization"),
            Some(&vec!["Bearer token-abc".to_string()])
        );
        assert_eq!(params.get("x-tenant-id"), Some(&vec!["acme".to_string()]));
    }

    #[test]
    fn test_extract_service_params_multi_value() {
        let mut headers = HeaderMap::new();
        headers.append("x-forwarded-for", HeaderValue::from_static("10.0.0.1"));
        headers.append("x-forwarded-for", HeaderValue::from_static("10.0.0.2"));

        let params = extract_service_params(&headers);

        assert_eq!(
            params.get("x-forwarded-for"),
            Some(&vec!["10.0.0.1".to_string(), "10.0.0.2".to_string()]),
        );
    }

    #[test]
    fn test_extract_service_params_lowercases_names() {
        let mut headers = HeaderMap::new();
        // HeaderMap normalizes the name to lowercase regardless of casing here.
        headers.insert("Authorization", HeaderValue::from_static("Bearer cased"));
        let params = extract_service_params(&headers);
        assert_eq!(
            params.get("authorization"),
            Some(&vec!["Bearer cased".to_string()])
        );
        assert!(!params.contains_key("Authorization"));
    }

    #[test]
    fn test_extract_service_params_skips_non_ascii_values() {
        let mut headers = HeaderMap::new();
        headers.insert("x-ascii", HeaderValue::from_static("ok"));
        headers.insert(
            "x-binary",
            HeaderValue::from_bytes(&[0xff, 0xfe, 0xfd]).unwrap(),
        );

        let params = extract_service_params(&headers);

        assert_eq!(params.get("x-ascii"), Some(&vec!["ok".to_string()]));
        assert!(
            !params.contains_key("x-binary"),
            "non-ASCII value should be skipped, not produce an empty entry"
        );
    }

    #[test]
    fn test_user_authenticated() {
        let user = User::authenticated("alice");
        assert_eq!(user.name, "alice");
        assert!(user.authenticated);
        assert!(user.attributes.is_empty());
    }

    #[test]
    fn test_call_context_new() {
        let params = ServiceParams::new();
        let ctx = CallContext::new(a2a::jsonrpc::methods::SEND_MESSAGE, params);
        assert_eq!(ctx.method, a2a::jsonrpc::methods::SEND_MESSAGE);
        assert!(ctx.tenant.is_none());
        assert!(ctx.user.is_none());
    }

    #[test]
    fn test_intercepted_handler_new() {
        let handler = "dummy";
        let ih = InterceptedHandler::new(handler);
        assert_eq!(ih.handler, "dummy");
        assert!(ih.interceptors.is_empty());
    }

    #[test]
    fn test_intercepted_handler_with_interceptor() {
        let handler = "dummy";
        let ih = InterceptedHandler::new(handler).with_interceptor(LoggingInterceptor);
        assert_eq!(ih.interceptors.len(), 1);
    }

    struct NoopInterceptor;

    #[async_trait]
    impl CallInterceptor for NoopInterceptor {}

    #[tokio::test]
    async fn test_noop_interceptor_before() {
        let interceptor = NoopInterceptor;
        let mut ctx = CallContext::new("test", ServiceParams::new());
        let result = interceptor.before(&mut ctx, &Value::Null).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_noop_interceptor_after() {
        let interceptor = NoopInterceptor;
        let ctx = CallContext::new("test", ServiceParams::new());
        let result = interceptor.after(&ctx, &Ok(Value::Null)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_logging_interceptor_before() {
        let interceptor = LoggingInterceptor;
        let mut ctx = CallContext::new(a2a::jsonrpc::methods::SEND_MESSAGE, ServiceParams::new());
        let result = interceptor.before(&mut ctx, &Value::Null).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_logging_interceptor_after_ok() {
        let interceptor = LoggingInterceptor;
        let ctx = CallContext::new(a2a::jsonrpc::methods::SEND_MESSAGE, ServiceParams::new());
        let result = interceptor.after(&ctx, &Ok(Value::Null)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_logging_interceptor_after_err() {
        let interceptor = LoggingInterceptor;
        let ctx = CallContext::new(a2a::jsonrpc::methods::SEND_MESSAGE, ServiceParams::new());
        let err: Result<Value, A2AError> = Err(A2AError::internal("boom"));
        let result = interceptor.after(&ctx, &err).await;
        assert!(result.is_ok());
    }
}
