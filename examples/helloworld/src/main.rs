// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::sync::Arc;

use a2a::*;
use a2a_server::*;
use futures::stream::{self, BoxStream};

/// A simple echo agent that returns the input message as agent output.
struct EchoExecutor;

impl AgentExecutor for EchoExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let message = ctx.message.clone();
        let task_id = ctx.task_id.clone();
        let context_id = ctx.context_id.clone();

        // Build agent response echoing the input
        let response_text = if let Some(msg) = &message {
            let parts_text: Vec<String> = msg
                .parts
                .iter()
                .map(|p| match &p.content {
                    PartContent::Text(text) => text.clone(),
                    _ => "[non-text content]".to_string(),
                })
                .collect();
            format!("Echo: {}", parts_text.join(", "))
        } else {
            "Echo: (no message)".to_string()
        };

        let response_msg = Message {
            role: Role::Agent,
            message_id: new_message_id(),
            task_id: Some(task_id.clone()),
            context_id: Some(context_id.clone()),
            parts: vec![Part::text(response_text)],
            metadata: None,
            extensions: None,
            reference_task_ids: None,
        };

        // Emit a task with completed status
        let task = Task {
            id: task_id,
            context_id,
            status: TaskStatus {
                state: TaskState::Completed,
                message: Some(response_msg),
                timestamp: Some(chrono::Utc::now()),
            },
            artifacts: None,
            history: None,
            metadata: None,
        };

        Box::pin(stream::once(async { Ok(StreamResponse::Task(task)) }))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let task_id = ctx.task_id.clone();
        let context_id = ctx.context_id.clone();

        Box::pin(stream::once(async {
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                context_id,
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: Some(chrono::Utc::now()),
                },
                metadata: None,
            }))
        }))
    }
}

fn build_agent_card() -> AgentCard {
    AgentCard {
        name: "Hello World Agent".to_string(),
        description: "A simple echo agent that returns the input message.".to_string(),
        version: a2a::VERSION.to_string(),
        provider: Some(AgentProvider {
            organization: "A2A Rust SDK".to_string(),
            url: "https://github.com/agntcy/a2a-rs".to_string(),
        }),
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            extensions: None,
            extended_agent_card: None,
        },
        skills: vec![AgentSkill {
            id: "echo".to_string(),
            name: "Echo".to_string(),
            description: "Echoes back the user's message.".to_string(),
            tags: vec!["echo".to_string()],
            examples: None,
            input_modes: None,
            output_modes: None,
            security_requirements: None,
        }],
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
        supported_interfaces: vec![
            AgentInterface::new("http://localhost:3000/jsonrpc", TRANSPORT_PROTOCOL_JSONRPC),
            AgentInterface::new("http://localhost:3000/rest", TRANSPORT_PROTOCOL_HTTP_JSON),
        ],
        security_schemes: None,
        security_requirements: None,
        documentation_url: None,
        icon_url: None,
        signatures: None,
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let executor = EchoExecutor;
    let task_store = InMemoryTaskStore::new();
    let handler = Arc::new(DefaultRequestHandler::new(executor, task_store));
    let agent_card = build_agent_card();
    let card_producer = Arc::new(StaticAgentCard::new(agent_card));

    // Build the combined router
    let app = axum::Router::new()
        .nest(
            "/jsonrpc",
            a2a_server::jsonrpc::jsonrpc_router(handler.clone()),
        )
        .nest("/rest", a2a_server::rest::rest_router(handler))
        .merge(a2a_server::agent_card::agent_card_router(card_producer));

    let addr = "0.0.0.0:3000";
    tracing::info!("Hello World Agent listening on {addr}");
    tracing::info!("Agent card: http://localhost:3000/.well-known/agent-card.json");
    tracing::info!("JSON-RPC endpoint: http://localhost:3000/jsonrpc");
    tracing::info!("REST endpoint: http://localhost:3000/rest");

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
