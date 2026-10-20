// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use a2a::{A2AError, AgentCard};
use futures::StreamExt;
use reqwest::Client;
use std::time::Duration;

/// Total per-request timeout for agent-card fetches.
///
/// Bounds the whole fetch (connect + headers + body) so a stalled or endless
/// card endpoint cannot hang the caller indefinitely. Applied per request in
/// [`AgentCardResolver::resolve`] so callers that supply their own client are
/// bounded as well.
const CARD_FETCH_TIMEOUT: Duration = Duration::from_secs(120);

/// Hard cap on the agent-card body size.
///
/// Cards are small JSON documents; a response larger than this is treated as
/// hostile and rejected before it can exhaust memory.
const MAX_CARD_BODY_BYTES: usize = 1024 * 1024;

/// Resolves agent cards from `.well-known/agent-card.json` endpoints.
pub struct AgentCardResolver {
    client: Client,
}

impl AgentCardResolver {
    pub fn new(client: Option<Client>) -> Self {
        AgentCardResolver {
            client: client
                .unwrap_or_else(|| default_card_fetch_client().expect("default card client")),
        }
    }

    /// Resolve an agent card from the given base URL.
    ///
    /// Fetches `{base_url}/.well-known/agent-card.json`.
    ///
    /// The fetch is bounded: a total request timeout applies, the body is
    /// capped at [`MAX_CARD_BODY_BYTES`], and — for the default client —
    /// cross-origin redirects are not followed.
    pub async fn resolve(&self, base_url: &str) -> Result<AgentCard, A2AError> {
        let url = format!(
            "{}/.well-known/agent-card.json",
            base_url.trim_end_matches('/')
        );

        let resp = self
            .client
            .get(&url)
            .timeout(CARD_FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|e| A2AError::internal(format!("failed to fetch agent card: {e}")))?;

        if !resp.status().is_success() {
            return Err(A2AError::internal(format!(
                "agent card fetch returned HTTP {}",
                resp.status()
            )));
        }

        // Bounded read: stream the body and stop as soon as it exceeds the
        // size cap instead of buffering an attacker-controlled response.
        let mut body = Vec::new();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk
                .map_err(|e| A2AError::internal(format!("failed to read agent card body: {e}")))?;
            if body.len() + chunk.len() > MAX_CARD_BODY_BYTES {
                return Err(A2AError::internal(format!(
                    "agent card body exceeds the {MAX_CARD_BODY_BYTES} byte limit"
                )));
            }
            body.extend_from_slice(&chunk);
        }

        serde_json::from_slice(&body)
            .map_err(|e| A2AError::internal(format!("failed to parse agent card: {e}")))
    }
}

/// Default HTTP client for agent-card resolution.
///
/// Unlike a general-purpose client, this one never follows a redirect to a
/// different origin: callers often vet the URL they resolve, and silently
/// following a cross-origin redirect would bypass that vetting. Same-origin
/// hops (including `http` → `https` upgrades) are still followed.
fn default_card_fetch_client() -> Result<Client, A2AError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::custom(card_redirect_policy))
        .connect_timeout(crate::DEFAULT_CONNECT_TIMEOUT)
        .build()
        .map_err(|e| A2AError::internal(format!("failed to build HTTP client: {e}")))
}

/// Redirect policy for agent-card fetches: follow same-origin hops (allowing
/// a scheme upgrade to `https`), stop otherwise. Stopping returns the 3xx
/// response to the caller instead of silently following it.
fn card_redirect_policy(attempt: reqwest::redirect::Attempt) -> reqwest::redirect::Action {
    let Some(first) = attempt.previous().first() else {
        // Fail closed: if the original URL is unknown, do not follow.
        return attempt.stop();
    };
    let next = attempt.url();
    let same_authority = next.host_str() == first.host_str()
        && next.port_or_known_default() == first.port_or_known_default();
    let scheme_ok =
        next.scheme() == first.scheme() || (first.scheme() == "http" && next.scheme() == "https");
    if same_authority && scheme_ok {
        attempt.follow()
    } else {
        attempt.stop()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn spawn_agent_card_server(body: &'static str) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = socket.read(&mut buffer).await;

            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        format!("http://{addr}")
    }

    #[tokio::test]
    async fn test_resolve_accepts_null_skills() {
        let server = spawn_agent_card_server(
            r#"{
                "name": "Test Agent",
                "description": "A test agent",
                "version": "1.0.0",
                "supportedInterfaces": [
                    {
                        "url": "http://127.0.0.1:3000/jsonrpc",
                        "protocolBinding": "JSONRPC",
                        "protocolVersion": "1.0"
                    }
                ],
                "capabilities": { "streaming": true },
                "defaultInputModes": ["text/plain"],
                "defaultOutputModes": ["text/plain"],
                "skills": null
            }"#,
        )
        .await;

        let resolver = AgentCardResolver::new(None);
        let card = resolver.resolve(&server).await.unwrap();

        assert!(card.skills.is_empty());
        assert_eq!(card.supported_interfaces[0].protocol_binding, "JSONRPC");
    }
}