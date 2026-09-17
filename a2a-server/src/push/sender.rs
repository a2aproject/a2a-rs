// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
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
pub(crate) fn validate_push_url(url: &str) -> Result<(), A2AError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| A2AError::invalid_params("invalid push notification URL"))?;

    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(A2AError::invalid_params("push URL must be http or https"));
    }

    if let Some(host) = parsed.host_str() {
        // A trailing dot is an equivalent fully-qualified form: "localhost."
        // resolves exactly as "localhost" does. Normalise it away, or every
        // hostname below is bypassed by appending one character. IP literals
        // are unaffected either way -- Url::parse already strips the dot for
        // those, which is why "127.0.0.1." was blocked and "localhost." was
        // not.
        let host = host.strip_suffix('.').unwrap_or(host);
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
        // private, link-local, ULA, unspecified, multicast). For IPv6 URLs
        // the host serializes WITH brackets ("[fc00::1]"), which never
        // parses as an IpAddr — strip them first or every range check
        // below is silently skipped.
        let host_unbracketed = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if let Ok(ip) = host_unbracketed.parse::<std::net::IpAddr>() {
            if is_blocked_ip(ip) {
                return Err(A2AError::invalid_params(
                    "push URL targets private/loopback/link-local address",
                ));
            }
        }
    }

    Ok(())
}

/// Whether `ip` is loopback, RFC 1918/ULA private, link-local, unspecified,
/// or multicast -- the ranges a push URL must not target.
///
/// A standalone predicate rather than inline in `validate_push_url`, per
/// #224's own scope: the connect-time guard it asks for needs to check the
/// *resolved* address against these same ranges, and must reuse this rather
/// than duplicate it. It is also what makes target 4 of #238's fuzz plan
/// possible: fuzzing "does `validate_push_url` correctly extract an IP from
/// an arbitrary URL and apply this predicate" needs the predicate itself to
/// be callable as independent ground truth, not re-derived by the fuzz
/// target and risking drifting out of sync with the real one.
pub(crate) fn is_blocked_ip(ip: std::net::IpAddr) -> bool {
    match ip {
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
            // IPv4-mapped IPv6 literals (::ffff:a.b.c.d) must be checked
            // against the IPv4 restrictions too, or they bypass the
            // loopback/private/link-local checks above.
            let mapped_v4_blocked = v6.to_ipv4_mapped().is_some_and(|v4| {
                v4.is_loopback()
                    || v4.is_private()
                    || v4.is_link_local()
                    || v4.is_unspecified()
                    || v4.is_multicast()
            });
            v6.is_loopback()
                || is_ula
                || mapped_v4_blocked
                || v6.is_unicast_link_local()
                || v6.is_unspecified()
                || v6.is_multicast()
        }
    }
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
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/hook",
            "gopher://example.com/",
        ] {
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
    fn test_validate_push_url_rejects_trailing_dot_hosts() {
        // "localhost." is the fully-qualified spelling of "localhost" and
        // resolves identically, so it must not slip past the host blocklist.
        for url in [
            "http://localhost./hook",
            "http://LocalHost./hook",
            "https://localhost./hook",
            "http://metadata.google.internal./hook",
            "http://metadata.azure.com./hook",
        ] {
            assert!(
                validate_push_url(url).is_err(),
                "{url} must be blocked, but was allowed"
            );
        }
    }

    #[test]
    fn test_validate_push_url_rejects_alternate_ipv4_spellings() {
        // These are blocked only because Url::parse normalises them to
        // 127.0.0.1 before the range check sees them. Pin that, so the
        // normalisation cannot be regressed away unnoticed.
        for url in [
            "http://2130706433/hook",         // decimal
            "http://0x7f.0.0.1/hook",         // hex octet
            "http://127.1/hook",              // short form
            "http://127.0.0.1./hook",         // trailing dot on a literal
            "http://[::ffff:127.0.0.1]/hook", // IPv4-mapped IPv6
        ] {
            assert!(
                validate_push_url(url).is_err(),
                "{url} must be blocked, but was allowed"
            );
        }
    }

    #[test]
    fn test_is_blocked_ip_checks_every_range_through_an_ipv4_mapped_address() {
        // is_blocked_ip's IPv4-mapped-IPv6 branch applies every IPv4
        // predicate to the unwrapped address, not just is_loopback -- the
        // existing ::ffff:127.0.0.1 test case above only exercises that
        // first one, since a loopback address short-circuits the rest.
        // Each of these is chosen to be false on every earlier predicate,
        // so the run reaches its own line rather than returning early.
        for (ip, why) in [
            ("::ffff:169.254.1.1", "link-local"),
            ("::ffff:0.0.0.0", "unspecified"),
            ("::ffff:224.0.0.1", "multicast"),
        ] {
            let addr: std::net::IpAddr = ip.parse().unwrap();
            assert!(is_blocked_ip(addr), "{ip} ({why}) should be blocked");
        }

        // And the mapped address is not blocked when the underlying IPv4
        // address is none of these -- confirms the closure's result is
        // actually load-bearing, not a predicate that always returns true.
        let public: std::net::IpAddr = "::ffff:93.184.216.34".parse().unwrap();
        assert!(
            !is_blocked_ip(public),
            "a public mapped address was blocked"
        );
    }

    #[test]
    fn test_validate_push_url_rejects_blocked_hosts_regardless_of_case() {
        // Url::parse lowercases the host per the URL Standard before the
        // blocklist ever sees it, so this holds independent of the
        // trailing-dot case above -- pinned on its own rather than only as
        // a side effect of that test.
        for url in [
            "http://LOCALHOST/hook",
            "http://METADATA.GOOGLE.INTERNAL/hook",
            "http://Metadata.Azure.Com/hook",
        ] {
            assert!(
                validate_push_url(url).is_err(),
                "{url} must be blocked, but was allowed"
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

    #[test]
    fn test_validate_push_url_rejects_invalid_url() {
        assert!(validate_push_url("not a url").is_err());
        assert!(validate_push_url("").is_err());
        // Empty authority fails URL parsing outright (EmptyHost).
        assert!(validate_push_url("http://").is_err());
    }

    #[test]
    fn test_validate_push_url_rejects_ipv6_restricted_ranges() {
        for url in [
            "http://[fc00::1]/cb",
            "http://[fd12:3456:789a::1]/cb",
            "http://[fe80::1]/cb",
            "http://[ff02::1]/cb",
            "http://[::]/cb",
        ] {
            let result = validate_push_url(url);
            assert!(result.is_err(), "expected {url} to be rejected");
            assert_eq!(result.unwrap_err().code, error_code::INVALID_PARAMS);
        }
    }

    #[test]
    fn test_validate_push_url_rejects_ipv4_mapped_ipv6() {
        // ::ffff:a.b.c.d literals must inherit the IPv4 restrictions.
        for url in [
            "http://[::ffff:127.0.0.1]/cb",
            "http://[::ffff:10.0.0.1]/cb",
        ] {
            let result = validate_push_url(url);
            assert!(result.is_err(), "expected {url} to be rejected");
            assert_eq!(result.unwrap_err().code, error_code::INVALID_PARAMS);
        }
    }

    #[test]
    fn test_validate_push_url_rejects_ipv4_multicast() {
        let result = validate_push_url("http://224.0.0.1/cb");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::INVALID_PARAMS);
    }

    #[test]
    fn test_validate_push_url_accepts_global_ipv6() {
        assert!(validate_push_url("http://[2606:4700::1111]/cb").is_ok());
    }
}
