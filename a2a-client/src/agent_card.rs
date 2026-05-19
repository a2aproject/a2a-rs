// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::{A2AError, AgentCard};
use reqwest::Client;

/// Resolves agent cards from `.well-known/agent-card.json` endpoints.
pub struct AgentCardResolver {
    client: Client,
}

impl AgentCardResolver {
    pub fn new(client: Option<Client>) -> Self {
        AgentCardResolver {
            client: client
                .unwrap_or_else(|| crate::default_reqwest_client(None).expect("default client")),
        }
    }

    /// Resolve an agent card from the given base URL.
    ///
    /// Fetches `{base_url}/.well-known/agent-card.json`.
    pub async fn resolve(&self, base_url: &str) -> Result<AgentCard, A2AError> {
        let url = format!(
            "{}/.well-known/agent-card.json",
            base_url.trim_end_matches('/')
        );

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| A2AError::internal(format!("failed to fetch agent card: {e}")))?;

        if !resp.status().is_success() {
            return Err(A2AError::internal(format!(
                "agent card fetch returned HTTP {}",
                resp.status()
            )));
        }

        resp.json::<AgentCard>()
            .await
            .map_err(|e| A2AError::internal(format!("failed to parse agent card: {e}")))
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
