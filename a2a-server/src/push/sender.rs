// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use std::net::SocketAddr;
use std::sync::Arc;
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
    /// If true (default), guard push URLs against SSRF. A string pre-check
    /// (scheme must be http/https; host must not be a loopback, private,
    /// link-local, multicast, unspecified, or cloud-metadata literal) fails
    /// before any network activity. A connect-time resolver then drops any
    /// resolved address in those ranges before dialing, so a hostname cannot
    /// reach a blocked address through DNS (split-horizon, attacker-controlled
    /// records, or rebinding). Redirects are re-screened per hop with the same
    /// string check, and the sender uses no system proxy, since either path
    /// would otherwise reach an address the guard never saw. Disable only when
    /// pushing to local webhooks in trusted test environments; that turns off
    /// the whole guard.
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
        // When URL validation is on, the guard needs three things reqwest does
        // not do by default, because each one is a way a request reaches an
        // address the string check never saw:
        //   - a resolver that screens every resolved address at connect time
        //     (see SsrfGuardedResolver), since the string check cannot see
        //     where a hostname actually resolves;
        //   - a redirect policy that re-screens each hop with validate_push_url,
        //     since a redirect to a blocked literal IP skips DNS entirely and so
        //     never reaches the resolver, and the string check only ran on the
        //     original URL;
        //   - no system proxy, since a proxy would resolve and reach the target
        //     itself, leaving only the proxy address screened.
        // When validation is off (the single documented opt-out for local test
        // webhooks), keep reqwest's defaults so loopback targets still work.
        let mut builder = reqwest::Client::builder().timeout(config.timeout);
        if config.validate_urls {
            builder = builder
                .dns_resolver(Arc::new(SsrfGuardedResolver))
                .no_proxy()
                .redirect(ssrf_guarded_redirect_policy());
        }
        let client = builder.build().expect("failed to create HTTP client");
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
            Err(crate::sanitized_internal_error(&msg))
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

/// Retain only the socket addresses safe to dial, dropping any whose IP
/// [`is_blocked_ip`] rejects. Split out from [`SsrfGuardedResolver`] so the
/// connect-time filter is unit-testable without performing real DNS.
fn connectable_addrs(addrs: impl Iterator<Item = SocketAddr>) -> Vec<SocketAddr> {
    addrs.filter(|addr| !is_blocked_ip(addr.ip())).collect()
}

/// A `reqwest` resolver that screens resolved addresses against
/// [`is_blocked_ip`] before the socket is dialed (#224).
///
/// `validate_push_url` screens the URL *string*, but a hostname can still
/// resolve to a loopback, private, or link-local address through split-horizon
/// DNS, an attacker-controlled A record, or DNS rebinding. This resolver is the
/// connect-time half of the guard: reqwest dials exactly the addresses returned
/// here, so filtering them is not subject to a resolve-then-reconnect TOCTOU.
/// Installed only when `validate_urls` is set.
///
/// This resolver only sees addresses reqwest resolves from a hostname. Two
/// paths reach an address without hitting it, so [`HttpPushSender::new`] closes
/// them alongside installing this resolver: a redirect to a blocked literal IP
/// skips DNS entirely (handled by re-screening every redirect hop with
/// `validate_push_url`), and a system proxy would resolve the target itself
/// (handled by disabling the proxy). All three are gated on the same
/// `validate_urls` flag.
struct SsrfGuardedResolver;

impl Resolve for SsrfGuardedResolver {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let host = name.as_str().to_owned();
            // Port 0: reqwest replaces it with the request's port before
            // dialing; only the resolved IP matters for the range check.
            let resolved = tokio::net::lookup_host((host.as_str(), 0)).await?;
            let allowed = connectable_addrs(resolved);
            if allowed.is_empty() {
                return Err(Box::<dyn std::error::Error + Send + Sync>::from(
                    "push URL host resolves only to blocked (loopback, private, \
                     link-local, or metadata) addresses",
                ));
            }
            Ok(Box::new(allowed.into_iter()) as Addrs)
        })
    }
}

/// The redirect policy installed when `validate_urls` is set: re-screens
/// each hop with [`validate_push_url`], since a redirect to a blocked
/// literal IP skips DNS entirely and never reaches [`SsrfGuardedResolver`].
///
/// Split out from [`HttpPushSender::new`] so a test can attach it to a
/// client on its own, without the resolver -- `reqwest::redirect::Attempt`
/// has no public constructor, so driving a real HTTP redirect through this
/// policy is the only way to exercise it.
fn ssrf_guarded_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() >= 10 {
            return attempt.stop();
        }
        if validate_push_url(attempt.url().as_str()).is_err() {
            attempt.error("push URL redirect target is blocked")
        } else {
            attempt.follow()
        }
    })
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

    #[test]
    fn test_connectable_addrs_drops_blocked_keeps_public() {
        // The connect-time filter (#224) must drop every restricted resolved
        // address and keep the routable ones, so a hostname that resolves to a
        // mix cannot reach the blocked members.
        let addrs: Vec<SocketAddr> = vec![
            "127.0.0.1:443".parse().unwrap(),         // loopback
            "10.0.0.5:443".parse().unwrap(),          // RFC 1918
            "169.254.169.254:80".parse().unwrap(),    // link-local metadata
            "93.184.216.34:443".parse().unwrap(),     // public v4
            "[fc00::1]:443".parse().unwrap(),         // IPv6 ULA
            "[2606:4700::1111]:443".parse().unwrap(), // public v6
        ];
        let ips: Vec<std::net::IpAddr> = connectable_addrs(addrs.into_iter())
            .iter()
            .map(|a| a.ip())
            .collect();
        assert_eq!(
            ips,
            vec![
                "93.184.216.34".parse::<std::net::IpAddr>().unwrap(),
                "2606:4700::1111".parse::<std::net::IpAddr>().unwrap(),
            ]
        );
    }

    #[test]
    fn test_connectable_addrs_all_blocked_is_empty() {
        let addrs: Vec<SocketAddr> = vec![
            "127.0.0.1:443".parse().unwrap(),
            "192.168.1.1:443".parse().unwrap(),
        ];
        assert!(connectable_addrs(addrs.into_iter()).is_empty());
    }

    #[tokio::test]
    async fn test_ssrf_resolver_rejects_host_resolving_to_loopback() {
        // A bare hostname that resolves only to loopback must be refused at
        // connect time, even though the resolver never sees an IP literal.
        // "localhost" resolves to 127.0.0.1 / ::1 without leaving the machine,
        // so this is deterministic.
        let name: Name = "localhost".parse().expect("localhost is a valid dns name");
        let result = SsrfGuardedResolver.resolve(name).await;
        assert!(
            result.is_err(),
            "a host resolving only to loopback must be refused at connect time"
        );
    }

    #[test]
    fn test_redirect_hop_check_rejects_blocked_targets() {
        // The custom redirect policy re-runs validate_push_url on each hop, so a
        // redirect to a blocked literal IP (which skips DNS and never reaches
        // SsrfGuardedResolver) is refused, while a public hop follows. This pins
        // the per-hop check the policy relies on.
        assert!(validate_push_url("http://127.0.0.1/").is_err());
        assert!(validate_push_url("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(validate_push_url("http://[::1]/").is_err());
        assert!(validate_push_url("https://example.com/next").is_ok());
    }

    /// Runs a local server that redirects `/` to `location`, so a real
    /// `reqwest::redirect::Attempt` reaches [`ssrf_guarded_redirect_policy`]
    /// -- the type has no public constructor, so this is the only way to
    /// drive it without faking reqwest's internals.
    async fn spawn_redirect_server(location: String) -> String {
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                let location = location.clone();
                async move { axum::response::Redirect::temporary(&location) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}/")
    }

    #[tokio::test]
    async fn test_redirect_policy_blocks_a_redirect_to_a_disallowed_target() {
        let server_url =
            spawn_redirect_server("http://169.254.169.254/latest/meta-data/".into()).await;
        let client = reqwest::Client::builder()
            .redirect(ssrf_guarded_redirect_policy())
            .build()
            .unwrap();
        let err = client
            .get(&server_url)
            .send()
            .await
            .expect_err("a redirect to a blocked target must fail");
        assert!(err.is_redirect(), "expected a redirect-policy error: {err}");
    }

    #[tokio::test]
    async fn test_redirect_policy_rejects_a_self_redirect_loop_immediately() {
        // A server that redirects to itself is a loopback target, so the
        // per-hop block check (not the separate 10-hop cap, which nothing
        // here can safely reach without a real allowed target to bounce
        // through) is what stops it -- on the very first hop.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let self_url = format!("http://{addr}/");
        let app = axum::Router::new().route(
            "/",
            axum::routing::get(move || {
                let self_url = self_url.clone();
                async move { axum::response::Redirect::temporary(&self_url) }
            }),
        );
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = reqwest::Client::builder()
            .redirect(ssrf_guarded_redirect_policy())
            .build()
            .unwrap();
        let err = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .expect_err("a loopback self-redirect must be refused, not followed");
        assert!(err.is_redirect(), "expected a redirect-policy error: {err}");
    }
}
