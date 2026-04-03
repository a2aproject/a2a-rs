// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::A2AError;
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;

use crate::middleware::CallInterceptor;
use crate::transport::ServiceParams;

/// Trait for providing credentials for authentication.
#[async_trait]
pub trait CredentialsStore: Send + Sync {
    /// Get credentials for the given scheme name.
    async fn get(&self, scheme: &str) -> Option<String>;
}

/// Simple in-memory credentials store.
pub struct InMemoryCredentialsStore {
    credentials: RwLock<HashMap<String, String>>,
}

impl InMemoryCredentialsStore {
    pub fn new() -> Self {
        InMemoryCredentialsStore {
            credentials: RwLock::new(HashMap::new()),
        }
    }

    pub fn set(&self, scheme: &str, credential: &str) {
        self.credentials
            .write()
            .unwrap()
            .insert(scheme.to_string(), credential.to_string());
    }
}

impl Default for InMemoryCredentialsStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl CredentialsStore for InMemoryCredentialsStore {
    async fn get(&self, scheme: &str) -> Option<String> {
        self.credentials.read().unwrap().get(scheme).cloned()
    }
}

/// Interceptor that injects authentication credentials into requests.
pub struct AuthInterceptor {
    header_name: String,
    header_value: String,
}

impl AuthInterceptor {
    /// Create an interceptor that adds a Bearer token.
    pub fn bearer(token: impl Into<String>) -> Self {
        AuthInterceptor {
            header_name: "Authorization".to_string(),
            header_value: format!("Bearer {}", token.into()),
        }
    }

    /// Create an interceptor with a custom header.
    pub fn custom(header_name: impl Into<String>, header_value: impl Into<String>) -> Self {
        AuthInterceptor {
            header_name: header_name.into(),
            header_value: header_value.into(),
        }
    }
}

#[async_trait]
impl CallInterceptor for AuthInterceptor {
    async fn before(&self, _method: &str, params: &mut ServiceParams) -> Result<(), A2AError> {
        params
            .entry(self.header_name.clone())
            .or_default()
            .push(self.header_value.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_in_memory_credentials_store_new() {
        let store = InMemoryCredentialsStore::new();
        let creds = store.credentials.read().unwrap();
        assert!(creds.is_empty());
    }

    #[test]
    fn test_in_memory_credentials_store_default() {
        let store = InMemoryCredentialsStore::default();
        let creds = store.credentials.read().unwrap();
        assert!(creds.is_empty());
    }

    #[test]
    fn test_in_memory_credentials_store_set_get() {
        let store = InMemoryCredentialsStore::new();
        store.set("bearer", "token123");
        let creds = store.credentials.read().unwrap();
        assert_eq!(creds.get("bearer").unwrap(), "token123");
    }

    #[tokio::test]
    async fn test_credentials_store_get() {
        let store = InMemoryCredentialsStore::new();
        store.set("api-key", "secret");
        assert_eq!(store.get("api-key").await, Some("secret".to_string()));
        assert_eq!(store.get("nonexistent").await, None);
    }

    #[tokio::test]
    async fn test_auth_interceptor_bearer() {
        let interceptor = AuthInterceptor::bearer("mytoken");
        let mut params = ServiceParams::new();
        interceptor.before("test", &mut params).await.unwrap();
        assert_eq!(
            params.get("Authorization").unwrap(),
            &vec!["Bearer mytoken".to_string()]
        );
    }

    #[tokio::test]
    async fn test_auth_interceptor_custom() {
        let interceptor = AuthInterceptor::custom("X-API-Key", "key123");
        let mut params = ServiceParams::new();
        interceptor.before("test", &mut params).await.unwrap();
        assert_eq!(
            params.get("X-API-Key").unwrap(),
            &vec!["key123".to_string()]
        );
    }
}
