// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use async_trait::async_trait;
use std::collections::HashMap;
use tokio::sync::RwLock;

/// Interface for persisting push notification configurations.
#[async_trait]
pub trait PushConfigStore: Send + Sync + 'static {
    /// Save a push config for a task. Generates an ID if none provided.
    async fn save(
        &self,
        task_id: &str,
        config: PushNotificationConfig,
    ) -> Result<PushNotificationConfig, A2AError>;

    /// Get a specific push config by task and config ID.
    async fn get(&self, task_id: &str, config_id: &str)
    -> Result<PushNotificationConfig, A2AError>;

    /// List all push configs for a task.
    async fn list(&self, task_id: &str) -> Result<Vec<PushNotificationConfig>, A2AError>;

    /// Delete a specific push config.
    async fn delete(&self, task_id: &str, config_id: &str) -> Result<(), A2AError>;

    /// Delete all push configs for a task.
    async fn delete_all(&self, task_id: &str) -> Result<(), A2AError>;
}

/// In-memory push config store.
pub struct InMemoryPushConfigStore {
    configs: RwLock<HashMap<String, HashMap<String, PushNotificationConfig>>>,
}

impl InMemoryPushConfigStore {
    pub fn new() -> Self {
        InMemoryPushConfigStore {
            configs: RwLock::new(HashMap::new()),
        }
    }
}

impl Default for InMemoryPushConfigStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl PushConfigStore for InMemoryPushConfigStore {
    async fn save(
        &self,
        task_id: &str,
        mut config: PushNotificationConfig,
    ) -> Result<PushNotificationConfig, A2AError> {
        if config.url.is_empty() {
            return Err(A2AError::invalid_params("push config URL cannot be empty"));
        }

        if config.id.is_none() {
            config.id = Some(uuid::Uuid::now_v7().to_string());
        }

        let mut store = self.configs.write().await;
        let task_configs = store.entry(task_id.to_string()).or_default();
        let config_id = config.id.clone().unwrap();
        task_configs.insert(config_id, config.clone());
        Ok(config)
    }

    async fn get(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> Result<PushNotificationConfig, A2AError> {
        let store = self.configs.read().await;
        store
            .get(task_id)
            .and_then(|configs| configs.get(config_id))
            .cloned()
            .ok_or_else(A2AError::push_notification_not_supported)
    }

    async fn list(&self, task_id: &str) -> Result<Vec<PushNotificationConfig>, A2AError> {
        let store = self.configs.read().await;
        Ok(store
            .get(task_id)
            .map(|configs| configs.values().cloned().collect())
            .unwrap_or_default())
    }

    async fn delete(&self, task_id: &str, config_id: &str) -> Result<(), A2AError> {
        let mut store = self.configs.write().await;
        if let Some(configs) = store.get_mut(task_id) {
            configs.remove(config_id);
        }
        Ok(())
    }

    async fn delete_all(&self, task_id: &str) -> Result<(), A2AError> {
        let mut store = self.configs.write().await;
        store.remove(task_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(url: &str) -> PushNotificationConfig {
        PushNotificationConfig {
            url: url.to_string(),
            id: None,
            token: None,
            authentication: None,
        }
    }

    #[tokio::test]
    async fn test_save_and_get() {
        let store = InMemoryPushConfigStore::new();
        let config = store
            .save("t1", make_config("https://example.com/hook"))
            .await
            .unwrap();
        assert!(config.id.is_some());

        let got = store.get("t1", config.id.as_ref().unwrap()).await.unwrap();
        assert_eq!(got.url, "https://example.com/hook");
    }

    #[tokio::test]
    async fn test_save_empty_url() {
        let store = InMemoryPushConfigStore::new();
        let result = store.save("t1", make_config("")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_save_with_existing_id() {
        let store = InMemoryPushConfigStore::new();
        let mut config = make_config("https://example.com/hook");
        config.id = Some("my-id".to_string());
        let saved = store.save("t1", config).await.unwrap();
        assert_eq!(saved.id.as_deref(), Some("my-id"));
    }

    #[tokio::test]
    async fn test_get_not_found() {
        let store = InMemoryPushConfigStore::new();
        let result = store.get("t0", "nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list() {
        let store = InMemoryPushConfigStore::new();
        store
            .save("t1", make_config("https://a.com"))
            .await
            .unwrap();
        store
            .save("t1", make_config("https://b.com"))
            .await
            .unwrap();
        store
            .save("t2", make_config("https://c.com"))
            .await
            .unwrap();

        let configs = store.list("t1").await.unwrap();
        assert_eq!(configs.len(), 2);

        let configs = store.list("t2").await.unwrap();
        assert_eq!(configs.len(), 1);

        let configs = store.list("t3").await.unwrap();
        assert_eq!(configs.len(), 0);
    }

    #[tokio::test]
    async fn test_delete() {
        let store = InMemoryPushConfigStore::new();
        let config = store
            .save("t1", make_config("https://a.com"))
            .await
            .unwrap();
        let config_id = config.id.unwrap();

        store.delete("t1", &config_id).await.unwrap();
        let result = store.get("t1", &config_id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_delete_all() {
        let store = InMemoryPushConfigStore::new();
        store
            .save("t1", make_config("https://a.com"))
            .await
            .unwrap();
        store
            .save("t1", make_config("https://b.com"))
            .await
            .unwrap();

        store.delete_all("t1").await.unwrap();
        let configs = store.list("t1").await.unwrap();
        assert_eq!(configs.len(), 0);
    }
}
