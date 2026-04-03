// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;

use a2a::AgentCard;
use axum::{
    Json,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
};

/// Well-known path for the public agent card.
pub const WELL_KNOWN_AGENT_CARD_PATH: &str = "/.well-known/agent-card.json";

/// Trait for producing agent cards dynamically.
pub trait AgentCardProducer: Send + Sync + 'static {
    fn card(&self) -> AgentCard;
}

/// A static agent card producer.
pub struct StaticAgentCard {
    card: AgentCard,
}

impl StaticAgentCard {
    pub fn new(card: AgentCard) -> Self {
        StaticAgentCard { card }
    }
}

impl AgentCardProducer for StaticAgentCard {
    fn card(&self) -> AgentCard {
        self.card.clone()
    }
}

/// Create an axum router serving the agent card at `/.well-known/agent-card.json`
/// with CORS headers for public discovery.
pub fn agent_card_router<P: AgentCardProducer>(producer: Arc<P>) -> axum::Router {
    axum::Router::new()
        .route(
            WELL_KNOWN_AGENT_CARD_PATH,
            axum::routing::get(handle_agent_card::<P>),
        )
        .with_state(producer)
}

async fn handle_agent_card<P: AgentCardProducer>(
    State(producer): State<Arc<P>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let card = producer.card();
    let mut resp_headers = HeaderMap::new();

    // CORS headers for public discovery
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*");

    if origin != "*" {
        resp_headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            origin.parse().unwrap_or_else(|_| "*".parse().unwrap()),
        );
        resp_headers.insert(
            header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
            "true".parse().unwrap(),
        );
        resp_headers.insert(header::VARY, "Origin".parse().unwrap());
    } else {
        resp_headers.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*".parse().unwrap());
    }

    (StatusCode::OK, resp_headers, Json(card))
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::AgentCapabilities;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_card() -> AgentCard {
        AgentCard {
            name: "TestAgent".into(),
            description: "A test agent".into(),
            version: "1.0".into(),
            supported_interfaces: vec![],
            capabilities: AgentCapabilities::default(),
            default_input_modes: vec!["text".into()],
            default_output_modes: vec!["text".into()],
            skills: vec![],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        }
    }

    #[test]
    fn test_static_agent_card() {
        let card = test_card();
        let sac = StaticAgentCard::new(card.clone());
        assert_eq!(sac.card().name, "TestAgent");
    }

    #[test]
    fn test_well_known_path() {
        assert_eq!(WELL_KNOWN_AGENT_CARD_PATH, "/.well-known/agent-card.json");
    }

    #[tokio::test]
    async fn test_agent_card_router_no_origin() {
        let card = test_card();
        let producer = Arc::new(StaticAgentCard::new(card));
        let app = agent_card_router(producer);

        let req = Request::builder()
            .uri("/.well-known/agent-card.json")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .unwrap()
                .to_str()
                .unwrap(),
            "*"
        );
    }

    #[tokio::test]
    async fn test_agent_card_router_with_origin() {
        let card = test_card();
        let producer = Arc::new(StaticAgentCard::new(card));
        let app = agent_card_router(producer);

        let req = Request::builder()
            .uri("/.well-known/agent-card.json")
            .header("origin", "https://example.com")
            .body(Body::empty())
            .unwrap();

        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get("access-control-allow-origin")
                .unwrap()
                .to_str()
                .unwrap(),
            "https://example.com"
        );
        assert_eq!(
            resp.headers()
                .get("access-control-allow-credentials")
                .unwrap()
                .to_str()
                .unwrap(),
            "true"
        );
    }
}
