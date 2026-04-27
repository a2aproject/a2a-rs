// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::future::IntoFuture;
use std::sync::Arc;

use a2a::*;
use a2a_grpc::GrpcHandler;
use a2a_pb::proto::a2a_service_server::A2aServiceServer;
use a2a_server::*;
use futures::stream::{self, BoxStream};
use tokio_stream::wrappers::{ReceiverStream, TcpListenerStream};
use tonic::transport::Server as TonicServer;

/// Echo agent with two execution modes driven by the message content:
///
/// * **Normal** (`"wait:"` prefix absent): emits one `Working` status update
///   followed immediately by a `Completed` task containing the echo reply.
///
/// * **Wait** (message starts with `"wait:"`): emits one `Working` status
///   update and then parks until the receiver is dropped (i.e. until the
///   framework cancels the task).  This lets the client demonstrate proper
///   cancellation and live subscription without any timing races.
struct EchoExecutor;

impl AgentExecutor for EchoExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let message = ctx.message.clone();
        let task_id = ctx.task_id.clone();
        let context_id = ctx.context_id.clone();

        // Decide the execution mode from the message text.
        let should_wait = message
            .as_ref()
            .and_then(|m| m.parts.first())
            .and_then(|p| {
                if let PartContent::Text(t) = &p.content {
                    Some(t.as_str())
                } else {
                    None
                }
            })
            .map(|t| t.starts_with("wait:"))
            .unwrap_or(false);

        let working = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.clone(),
            context_id: context_id.clone(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: Some(chrono::Utc::now()),
            },
            metadata: None,
        });

        if should_wait {
            // Emit Working, then park.  When drive_execution stops consuming
            // (because cancel_task was called), the ReceiverStream is dropped,
            // closing the channel, and tx.closed() resolves cleanly.
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tokio::spawn(async move {
                let _ = tx.send(Ok(working)).await;
                tx.closed().await;
            });
            Box::pin(ReceiverStream::new(rx))
        } else {
            // Build the echo reply and complete in one shot.
            let response_text = match &message {
                Some(msg) => {
                    let parts: Vec<String> = msg
                        .parts
                        .iter()
                        .map(|p| match &p.content {
                            PartContent::Text(t) => t.clone(),
                            _ => "[non-text]".to_string(),
                        })
                        .collect();
                    format!("Echo: {}", parts.join(", "))
                }
                None => "Echo: (no message)".to_string(),
            };

            let completed = StreamResponse::Task(Task {
                id: task_id.clone(),
                context_id: context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: Some(Message {
                        role: Role::Agent,
                        message_id: new_message_id(),
                        task_id: Some(task_id),
                        context_id: Some(context_id),
                        parts: vec![Part::text(response_text)],
                        metadata: None,
                        extensions: None,
                        reference_task_ids: None,
                    }),
                    timestamp: Some(chrono::Utc::now()),
                },
                artifacts: None,
                history: None,
                metadata: None,
            });

            Box::pin(stream::iter([Ok(working), Ok(completed)]))
        }
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
            url: "https://github.com/a2aproject/a2a-rs".to_string(),
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
            AgentInterface::new("http://localhost:50051", TRANSPORT_PROTOCOL_GRPC),
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

    // Build the HTTP router (JSON-RPC + REST + agent card)
    let app = axum::Router::new()
        .nest(
            "/jsonrpc",
            a2a_server::jsonrpc::jsonrpc_router(handler.clone()),
        )
        .nest("/rest", a2a_server::rest::rest_router(handler.clone()))
        .merge(a2a_server::agent_card::agent_card_router(card_producer));

    // Build the gRPC service
    let grpc_service = A2aServiceServer::new(GrpcHandler::new(handler));

    let http_addr = "0.0.0.0:3000";
    let grpc_addr = "0.0.0.0:50051";

    tracing::info!("Hello World Agent starting");
    tracing::info!("Agent card:      http://localhost:3000/.well-known/agent-card.json");
    tracing::info!("JSON-RPC:        http://localhost:3000/jsonrpc");
    tracing::info!("REST:            http://localhost:3000/rest");
    tracing::info!("gRPC:            http://localhost:50051");

    let http_listener = tokio::net::TcpListener::bind(http_addr).await.unwrap();
    let grpc_listener = tokio::net::TcpListener::bind(grpc_addr).await.unwrap();

    tokio::select! {
        result = axum::serve(http_listener, app).into_future() => {
            if let Err(e) = result { tracing::error!(error = %e, "HTTP server exited"); }
        }
        result = TonicServer::builder()
            .add_service(grpc_service)
            .serve_with_incoming(TcpListenerStream::new(grpc_listener)) => {
            if let Err(e) = result { tracing::error!(error = %e, "gRPC server exited"); }
        }
    }
}
