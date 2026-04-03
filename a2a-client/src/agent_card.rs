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
            client: client.unwrap_or_default(),
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
