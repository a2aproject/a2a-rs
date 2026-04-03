// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use std::time::Duration;

/// HTTP push notification sender.
///
/// Sends A2A streaming events to a push notification endpoint over HTTP POST.
pub struct HttpPushSender {
    client: reqwest::Client,
    fail_on_error: bool,
}

/// Configuration for [`HttpPushSender`].
pub struct HttpPushSenderConfig {
    /// HTTP request timeout (default: 30s).
    pub timeout: Duration,
    /// If true, push sending errors abort execution.
    pub fail_on_error: bool,
}

impl Default for HttpPushSenderConfig {
    fn default() -> Self {
        HttpPushSenderConfig {
            timeout: Duration::from_secs(30),
            fail_on_error: false,
        }
    }
}

impl HttpPushSender {
    pub fn new(config: Option<HttpPushSenderConfig>) -> Self {
        let config = config.unwrap_or_default();
        let client = reqwest::Client::builder()
            .timeout(config.timeout)
            .build()
            .expect("failed to create HTTP client");
        HttpPushSender {
            client,
            fail_on_error: config.fail_on_error,
        }
    }

    /// Send an event to the push notification endpoint.
    pub async fn send_push(
        &self,
        config: &PushNotificationConfig,
        event: StreamResponse,
    ) -> Result<(), A2AError> {
        let body = match serde_json::to_vec(&event) {
            Ok(b) => b,
            Err(e) => return self.handle_error(format!("failed to serialize event: {e}")),
        };

        let mut request = self
            .client
            .post(&config.url)
            .header("Content-Type", "application/json")
            .body(body);

        if let Some(ref token) = config.token {
            request = request.header("A2A-Notification-Token", token);
        }

        if let Some(ref auth) = config.authentication {
            if let Some(ref creds) = auth.credentials {
                match auth.scheme.to_lowercase().as_str() {
                    "bearer" => {
                        request = request.header("Authorization", format!("Bearer {creds}"));
                    }
                    "basic" => {
                        request = request.header("Authorization", format!("Basic {creds}"));
                    }
                    _ => {}
                }
            }
        }

        match request.send().await {
            Ok(resp) => {
                if !resp.status().is_success() {
                    return self
                        .handle_error(format!("push endpoint returned status: {}", resp.status()));
                }
                Ok(())
            }
            Err(e) => self.handle_error(format!("failed to send push notification: {e}")),
        }
    }

    fn handle_error(&self, msg: String) -> Result<(), A2AError> {
        if self.fail_on_error {
            Err(A2AError::internal(&msg))
        } else {
            tracing::error!("{}", msg);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = HttpPushSenderConfig::default();
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert!(!config.fail_on_error);
    }

    #[test]
    fn test_sender_new_default() {
        let sender = HttpPushSender::new(None);
        assert!(!sender.fail_on_error);
    }

    #[test]
    fn test_sender_new_custom() {
        let config = HttpPushSenderConfig {
            timeout: Duration::from_secs(10),
            fail_on_error: true,
        };
        let sender = HttpPushSender::new(Some(config));
        assert!(sender.fail_on_error);
    }

    #[test]
    fn test_handle_error_fail_on_error() {
        let sender = HttpPushSender::new(Some(HttpPushSenderConfig {
            timeout: Duration::from_secs(5),
            fail_on_error: true,
        }));
        let result = sender.handle_error("test error".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_handle_error_ignore() {
        let sender = HttpPushSender::new(None);
        let result = sender.handle_error("test error".to_string());
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_push_connection_refused_no_fail() {
        use a2a::event::*;
        // With fail_on_error=false, network errors are logged but Ok
        let sender = HttpPushSender::new(None);
        let config = PushNotificationConfig {
            url: "http://127.0.0.1:1/callback".to_string(),
            id: None,
            token: Some("tok".to_string()),
            authentication: Some(AuthenticationInfo {
                scheme: "bearer".to_string(),
                credentials: Some("secret".to_string()),
            }),
        };
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        let result = sender.send_push(&config, event).await;
        // Should be Ok because fail_on_error is false
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_push_connection_refused_fail() {
        use a2a::event::*;
        let sender = HttpPushSender::new(Some(HttpPushSenderConfig {
            timeout: std::time::Duration::from_millis(100),
            fail_on_error: true,
        }));
        let config = PushNotificationConfig {
            url: "http://127.0.0.1:1/callback".to_string(),
            id: None,
            token: None,
            authentication: None,
        };
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        let result = sender.send_push(&config, event).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_push_with_basic_auth() {
        use a2a::event::*;
        let sender = HttpPushSender::new(None);
        let config = PushNotificationConfig {
            url: "http://127.0.0.1:1/callback".to_string(),
            id: None,
            token: None,
            authentication: Some(AuthenticationInfo {
                scheme: "basic".to_string(),
                credentials: Some("dXNlcjpwYXNz".to_string()),
            }),
        };
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        // Will fail to connect but exercises basic auth header path
        let result = sender.send_push(&config, event).await;
        assert!(result.is_ok()); // fail_on_error=false
    }

    #[tokio::test]
    async fn test_send_push_with_unknown_auth_scheme() {
        use a2a::event::*;
        let sender = HttpPushSender::new(None);
        let config = PushNotificationConfig {
            url: "http://127.0.0.1:1/callback".to_string(),
            id: None,
            token: None,
            authentication: Some(AuthenticationInfo {
                scheme: "custom".to_string(),
                credentials: Some("cred".to_string()),
            }),
        };
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        });
        let result = sender.send_push(&config, event).await;
        assert!(result.is_ok());
    }
}
