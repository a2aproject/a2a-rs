// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::A2AError;
use async_trait::async_trait;

use crate::transport::ServiceParams;

/// Interceptor for modifying requests and responses at the client level.
///
/// Interceptors are called in order for `before`, and in reverse order for `after`.
#[async_trait]
pub trait CallInterceptor: Send + Sync {
    /// Called before sending a request. Can modify params (e.g., add auth headers).
    async fn before(&self, method: &str, params: &mut ServiceParams) -> Result<(), A2AError> {
        let _ = (method, params);
        Ok(())
    }

    /// Called after receiving a response.
    async fn after(&self, method: &str, result: &Result<(), A2AError>) -> Result<(), A2AError> {
        let _ = (method, result);
        Ok(())
    }
}

/// Logging interceptor using `tracing`.
pub struct LoggingInterceptor;

#[async_trait]
impl CallInterceptor for LoggingInterceptor {
    async fn before(&self, method: &str, _params: &mut ServiceParams) -> Result<(), A2AError> {
        tracing::info!(method = method, "A2A client request");
        Ok(())
    }

    async fn after(&self, method: &str, result: &Result<(), A2AError>) -> Result<(), A2AError> {
        match result {
            Ok(()) => tracing::info!(method = method, "A2A client response"),
            Err(e) => tracing::warn!(method = method, error = %e, "A2A client error"),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoopInterceptor;

    #[async_trait]
    impl CallInterceptor for NoopInterceptor {}

    #[tokio::test]
    async fn test_default_before() {
        let interceptor = NoopInterceptor;
        let mut params = ServiceParams::new();
        let result = interceptor.before("test", &mut params).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_default_after() {
        let interceptor = NoopInterceptor;
        let result = interceptor.after("test", &Ok(())).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_default_after_with_error() {
        let interceptor = NoopInterceptor;
        let err = Err(A2AError::internal("fail"));
        let result = interceptor.after("test", &err).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_logging_interceptor_before() {
        let interceptor = LoggingInterceptor;
        let mut params = ServiceParams::new();
        let result = interceptor
            .before(a2a::jsonrpc::methods::SEND_MESSAGE, &mut params)
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_logging_interceptor_after_ok() {
        let interceptor = LoggingInterceptor;
        let result = interceptor
            .after(a2a::jsonrpc::methods::SEND_MESSAGE, &Ok(()))
            .await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_logging_interceptor_after_err() {
        let interceptor = LoggingInterceptor;
        let err = Err(A2AError::internal("boom"));
        let result = interceptor
            .after(a2a::jsonrpc::methods::SEND_MESSAGE, &err)
            .await;
        assert!(result.is_ok());
    }
}
