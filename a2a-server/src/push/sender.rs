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
    validate_urls: bool,
}

/// Configuration for [`HttpPushSender`].
pub struct HttpPushSenderConfig {
    /// HTTP request timeout (default: 30s).
    pub timeout: Duration,
    /// If true, push sending errors abort execution.
    pub fail_on_error: bool,
    /// If true (default), reject push URLs that target loopback, private,
    /// link-local, multicast, unspecified, or cloud-metadata addresses, and
    /// URLs whose scheme is not http/https. Disable only when pushing to
    /// local webhooks in trusted test environments.
    pub validate_urls: bool,
}

impl Default for HttpPushSenderConfig {
    fn default() -> Self {
        HttpPushSenderConfig {
            timeout: Duration::from_secs(30),
            fail_on_error: false,
            validate_urls: true,
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
            validate_urls: config.validate_urls,
        }
    }

    /// Send an event to the push notification endpoint.
    pub async fn send_push(
        &self,
        config: &TaskPushNotificationConfig,
        event: StreamResponse,
    ) -> Result<(), A2AError> {
        if self.validate_urls {
            validate_push_url(&config.url)?;
        }

        // Reject credentials containing CR/LF so they cannot inject
        // additional headers into the outgoing request (BUG-34).
        if let Some(ref token) = config.token {
            if token.contains('\r') || token.contains('\n') {
                return Err(A2AError::invalid_params(
                    "push notification token must not contain CR/LF",
                ));
            }
        }
        if let Some(ref auth) = config.authentication {
            if let Some(ref creds) = auth.credentials {
                if creds.contains('\r') || creds.contains('\n') {
                    return Err(A2AError::invalid_params(
                        "push credentials must not contain CR/LF",
                    ));
                }
            }
        }

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

/// Validate a push notification URL against SSRF targets (BUG-54).
///
/// The URL scheme must be http/https and the host must not resolve to a
/// loopback, private, link-local, multicast, or unspecified address, nor to a
/// well-known cloud metadata endpoint.
fn validate_push_url(url: &str) -> Result<(), A2AError> {
    let parsed =
        reqwest::Url::parse(url).map_err(|_| A2AError::invalid_params("invalid push notification URL"))?;

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(A2AError::invalid_params(
            "push URL must be http or https",
        ));
    }

    if let Some(host) = parsed.host_str() {
        // Block well-known loopback, metadata, and unspecified hosts.
        let blocked = [
            "127.0.0.1",
            "localhost",
            "::1",
            "[::1]",
            "169.254.169.254",
            "metadata.google.internal",
            "metadata.azure.com",
            "metadata.goog",
            "0.0.0.0",
        ];
        if blocked.contains(&host) {
            return Err(A2AError::invalid_params("push URL targets blocked host"));
        }
        // Block IP literals in restricted ranges (loopback, RFC 1918
        // private, link-local, ULA, unspecified, multicast).
        if let Ok(ip) = host.parse::<std::net::IpAddr>() {
            let blocked = match ip {
                std::net::IpAddr::V4(v4) => {
                    v4.is_loopback()
                        || v4.is_private()
                        || v4.is_link_local()
                        || v4.is_unspecified()
                        || v4.is_multicast()
                }
                std::net::IpAddr::V6(v6) => {
                    // fc00::/7 unique local addresses (IPv6 "private").
                    let is_ula = (v6.segments()[0] & 0xfe00) == 0xfc00;
                    v6.is_loopback()
                        || is_ula
                        || v6.is_unicast_link_local()
                        || v6.is_unspecified()
                        || v6.is_multicast()
                }
            };
            if blocked {
                return Err(A2AError::invalid_params(
                    "push URL targets private/loopback/link-local address",
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::test_util::install_crypto_provider;

    #[test]
    fn test_default_config() {
        let config = HttpPushSenderConfig::default();
        assert_eq!(config.timeout, Duration::from_secs(30));
        assert!(!config.fail_on_error);
        assert!(config.validate_urls);
    }

    #[test]
    fn test_sender_new_default() {
        install_crypto_provider();
        let sender = HttpPushSender::new(None);
        assert!(!sender.fail_on_error);
        assert!(sender.validate_urls);
    }

    #[test]
    fn test_sender_new_custom() {
        install_crypto_provider();
        let config = HttpPushSenderConfig {
            timeout: Duration::from_secs(10),
            fail_on_error: true,
            validate_urls: false,
        };
        let sender = HttpPushSender::new(Some(config));
        assert!(sender.fail_on_error);
        assert!(!sender.validate_urls);
    }

    #[test]
    fn test_handle_error_fail_on_error() {
        install_crypto_provider();
        let sender = HttpPushSender::new(Some(HttpPushSenderConfig {
            timeout: Duration::from_secs(5),
            fail_on_error: true,
            validate_urls: true,
        }));
        let result = sender.handle_error("test error".to_string());
        assert!(result.is_err());
    }

    #[test]
    fn test_handle_error_ignore() {
        install_crypto_provider();
        let sender = HttpPushSender::new(None);
        let result = sender.handle_error("test error".to_string());
        assert!(result.is_ok());
    }

    fn sample_status_update() -> StreamResponse {
        use a2a::event::*;
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "t1".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            metadata: None,
        })
    }

    fn sample_config(url: &str) -> TaskPushNotificationConfig {
        TaskPushNotificationConfig {
            task_id: String::new(),
            url: url.to_string(),
            id: None,
            token: None,
            authentication: None,
            tenant: None,
        }
    }

    #[tokio::test]
    async fn test_send_push_connection_refused_no_fail() {
        install_crypto_provider();
        let sender = HttpPushSender::new(Some(HttpPushSenderConfig {
            validate_urls: false,
            ..Default::default()
        }));
        let config = TaskPushNotificationConfig {
            task_id: String::new(),
            url: "http://127.0.0.1:1/callback".to_string(),
            id: None,
            token: Some("tok".to_string()),
            authentication: Some(AuthenticationInfo {
                scheme: "bearer".to_string(),
                credentials: Some("secret".to_string()),
            }),
            tenant: None,
        };
        let result = sender.send_push(&config, sample_status_update()).await;
        // Should be Ok because fail_on_error is false
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_send_push_connection_refused_fail() {
        install_crypto_provider();
        let sender = HttpPushSender::new(Some(HttpPushSenderConfig {
            timeout: std::time::Duration::from_millis(100),
            fail_on_error: true,
            validate_urls: false,
        }));
        let config = sample_config("http://127.0.0.1:1/callback");
        let result = sender.send_push(&config, sample_status_update()).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_push_with_basic_auth() {
        install_crypto_provider();
        let sender = HttpPushSender::new(Some(HttpPushSenderConfig {
            validate_urls: false,
            ..Default::default()
        }));
        let config = TaskPushNotificationConfig {
            task_id: String::new(),
            url: "http://127.0.0.1:1/callback".to_string(),
            id: None,
            token: None,
            authentication: Some(AuthenticationInfo {
                scheme: "basic".to_string(),
                credentials: Some("dXNlcjpwYXNz".to_string()),
            }),
            tenant: None,
        };
        let result = sender.send_push(&config, sample_status_update()).await;
        // Will fail to connect but exercises basic auth header path
        assert!(result.is_ok()); // fail_on_error=false
    }

    #[tokio::test]
    async fn test_send_push_with_unknown_auth_scheme() {
        install_crypto_provider();
        let sender = HttpPushSender::new(Some(HttpPushSenderConfig {
            validate_urls: false,
            ..Default::default()
        }));
        let config = TaskPushNotificationConfig {
            task_id: String::new(),
            url: "http://127.0.0.1:1/callback".to_string(),
            id: None,
            token: None,
            authentication: Some(AuthenticationInfo {
                scheme: "custom".to_string(),
                credentials: Some("cred".to_string()),
            }),
            tenant: None,
        };
        let result = sender.send_push(&config, sample_status_update()).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_push_url_rejects_non_http_schemes() {
        for url in ["file:///etc/passwd", "ftp://example.com/hook", "gopher://example.com/"] {
            let err = validate_push_url(url).unwrap_err();
            assert_eq!(err.code, error_code::INVALID_PARAMS);
        }
    }

    #[test]
    fn test_validate_push_url_rejects_loopback() {
        for url in [
            "http://127.0.0.1:1/callback",
            "http://localhost/callback",
            "http://[::1]/callback",
            "http://0.0.0.0/callback",
        ] {
            assert!(
                validate_push_url(url).is_err(),
                "expected {url} to be rejected"
            );
        }
    }

    #[test]
    fn test_validate_push_url_rejects_private_and_link_local() {
        for url in [
            "http://10.0.0.1/callback",
            "http://192.168.1.1/callback",
            "http://172.16.0.1/callback",
            "http://169.254.169.254/latest/meta-data/",
            "http://169.254.1.1/callback",
            "http://metadata.google.internal/callback",
        ] {
            assert!(
                validate_push_url(url).is_err(),
                "expected {url} to be rejected"
            );
        }
    }

    #[test]
    fn test_validate_push_url_accepts_public_urls() {
        for url in [
            "http://example.com/callback",
            "https://example.com/callback",
            "https://hooks.example.com/abc?x=1",
        ] {
            assert!(
                validate_push_url(url).is_ok(),
                "expected {url} to be accepted"
            );
        }
    }

    #[tokio::test]
    async fn test_send_push_rejects_blocked_url_even_without_fail_on_error() {
        install_crypto_provider();
        let sender = HttpPushSender::new(None); // validate_urls = true
        let config = sample_config("http://127.0.0.1:1/callback");
        let result = sender.send_push(&config, sample_status_update()).await;
        // Validation errors are always returned, regardless of fail_on_error.
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn test_send_push_rejects_crlf_credentials() {
        install_crypto_provider();
        let sender = HttpPushSender::new(None);
        let config = TaskPushNotificationConfig {
            task_id: String::new(),
            url: "https://example.com/callback".to_string(),
            id: None,
            token: None,
            authentication: Some(AuthenticationInfo {
                scheme: "bearer".to_string(),
                credentials: Some("secret\r\nX-Injected: 1".to_string()),
            }),
            tenant: None,
        };
        let result = sender.send_push(&config, sample_status_update()).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn test_send_push_rejects_crlf_token() {
        install_crypto_provider();
        let sender = HttpPushSender::new(None);
        let config = TaskPushNotificationConfig {
            task_id: String::new(),
            url: "https://example.com/callback".to_string(),
            id: None,
            token: Some("tok\nX-Injected: 1".to_string()),
            authentication: None,
            tenant: None,
        };
        let result = sender.send_push(&config, sample_status_update()).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::INVALID_PARAMS);
    }
}
