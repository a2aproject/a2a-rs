// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! E-commerce collaborative task example agents.
//!
//! Three specialist agents with hardcoded domain data, mirroring the Go
//! reference at <https://github.com/a2aproject/experimental-cpb-slimrpc/commit/3a82760e147659b891c70e17164040122f560c9e>.
//!
//! Each agent responds only to queries that mention keywords from its domain.
//! No LLM required — deterministic keyword-matching keeps the example simple.

use std::sync::Arc;

use a2a::*;
use a2a_server::{AgentExecutor, ExecutorContext};
use futures::stream::{self, BoxStream};
use tokio_stream::wrappers::ReceiverStream;

use super::fan_in::InboxRegistry;

// ---------------------------------------------------------------------------
// Domain data (hardcoded, no LLM)
// ---------------------------------------------------------------------------

struct DomainEntry {
    keywords: &'static [&'static str],
    response: &'static str,
}

static ORDERS_DATA: &[DomainEntry] = &[
    DomainEntry {
        keywords: &["ord-1001", "order 1001", "order1001"],
        response: "ORD-1001: shipped on 2024-11-15, tracking TRK-9001. \
                   Contains: 1× P-500 Wireless Headphones.",
    },
    DomainEntry {
        keywords: &["ord-1002", "order 1002", "order1002"],
        response: "ORD-1002: delivered on 2024-11-14. Contains: 2× USB-C Cable.",
    },
];

static INVENTORY_DATA: &[DomainEntry] = &[
    DomainEntry {
        keywords: &["p-500", "wireless headphone", "headphone"],
        response: "P-500 Wireless Headphones: oversold by 2 units. \
                   Restock expected 2024-11-22.",
    },
    DomainEntry {
        keywords: &["usb-c cable", "usb cable"],
        response: "USB-C Cable: 143 units in stock.",
    },
];

static SHIPPING_DATA: &[DomainEntry] = &[
    DomainEntry {
        keywords: &["trk-9001", "tracking 9001"],
        response: "TRK-9001: delayed — weather event in Memphis. \
                   Revised delivery estimate: 2024-11-21.",
    },
    DomainEntry {
        keywords: &["trk-9002", "tracking 9002"],
        response: "TRK-9002: out for delivery.",
    },
];

// ---------------------------------------------------------------------------
// CollabExecutor
// ---------------------------------------------------------------------------

/// A single-domain executor that responds to relevant queries and absorbs
/// relay messages from peers via an inbox registered with FanInHandler.
pub struct CollabExecutor {
    domain: &'static str,
    data: &'static [DomainEntry],
    inbox: Arc<InboxRegistry>,
}

impl CollabExecutor {
    pub fn orders(inbox: Arc<InboxRegistry>) -> Self {
        Self {
            domain: "orders",
            data: ORDERS_DATA,
            inbox,
        }
    }

    pub fn inventory(inbox: Arc<InboxRegistry>) -> Self {
        Self {
            domain: "inventory",
            data: INVENTORY_DATA,
            inbox,
        }
    }

    pub fn shipping(inbox: Arc<InboxRegistry>) -> Self {
        Self {
            domain: "shipping",
            data: SHIPPING_DATA,
            inbox,
        }
    }
}

impl AgentExecutor for CollabExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (task_id, context_id) = ctx.task_info();
        let inbox = self.inbox.clone();
        let domain = self.domain;

        let query_text = ctx
            .message
            .as_ref()
            .and_then(Message::text)
            .unwrap_or_default()
            .to_string();

        let data = self.data;

        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            // Register this task's inbox so FanInHandler can route relay messages.
            let mut relay_rx = inbox.register(&task_id);

            let _ = tx
                .send(Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: task_id.clone(),
                    context_id: context_id.clone(),
                    status: TaskStatus {
                        state: TaskState::Working,
                        message: None,
                        timestamp: None,
                    },
                    metadata: None,
                })))
                .await;

            // Helper: check and reply if the text matches our domain.
            let check_and_reply = |text: &str| -> Option<StreamResponse> {
                let lower = text.to_ascii_lowercase();
                for entry in data {
                    if entry.keywords.iter().any(|k| lower.contains(k)) {
                        return Some(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                            task_id: task_id.clone(),
                            context_id: context_id.clone(),
                            status: TaskStatus {
                                state: TaskState::Working,
                                message: Some(Message::new(
                                    Role::Agent,
                                    vec![Part::text(entry.response)],
                                )),
                                timestamp: None,
                            },
                            metadata: None,
                        }));
                    }
                }
                None
            };

            // Respond to the initial query if it touches our domain.
            if let Some(event) = check_and_reply(&query_text) {
                let _ = tx.send(Ok(event)).await;
            }

            // Drain relay messages until the channel closes or we hit a timeout.
            let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_secs(30);
            while let Ok(Some(relay_msg)) = tokio::time::timeout_at(deadline, relay_rx.recv()).await
            {
                let relay_text = relay_msg.text().unwrap_or_default().to_string();
                // Also check Part::data envelopes for text content.
                let combined = format!("{relay_text} {}", {
                    relay_msg
                        .parts
                        .iter()
                        .filter_map(|p| {
                            if let PartContent::Data(v) = &p.content {
                                Some(v.to_string())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                if let Some(event) = check_and_reply(&combined)
                    && tx.send(Ok(event)).await.is_err()
                {
                    break;
                }
            }

            inbox.unregister(&task_id);

            let _ = tx
                .send(Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    task_id: task_id.clone(),
                    context_id: context_id.clone(),
                    status: TaskStatus {
                        state: TaskState::Completed,
                        message: None,
                        timestamp: None,
                    },
                    metadata: None,
                })))
                .await;

            tracing::debug!("[{domain}] task {task_id} complete");
        });

        Box::pin(ReceiverStream::new(rx))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (task_id, context_id) = ctx.task_info();
        self.inbox.unregister(&task_id);
        Box::pin(stream::once(async move {
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id,
                context_id,
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            }))
        }))
    }
}

// ---------------------------------------------------------------------------
// AgentCard builder
// ---------------------------------------------------------------------------

/// Build an AgentCard for a collaborative e-commerce agent.
pub fn agent_card(name: &str, description: &str, base_url: &str) -> AgentCard {
    AgentCard {
        name: name.to_string(),
        description: description.to_string(),
        version: "0.1.0".to_string(),
        provider: Some(AgentProvider {
            organization: "A2A Rust SDK".to_string(),
            url: "https://github.com/a2aproject/a2a-rs".to_string(),
        }),
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            extensions: Some(vec![
                AgentExtension {
                    uri: a2a::EXTENSION_URI_SHARED_TASK.to_string(),
                    description: Some("shared-task attribution".to_string()),
                    required: Some(true),
                    params: None,
                },
                AgentExtension {
                    uri: a2a::EXTENSION_URI_COLLABORATIVE_TASK.to_string(),
                    description: Some("collaborative task relay".to_string()),
                    required: Some(true),
                    params: None,
                },
            ]),
            extended_agent_card: None,
        },
        skills: vec![AgentSkill {
            id: "ecommerce".to_string(),
            name: "E-Commerce Specialist".to_string(),
            description: description.to_string(),
            tags: vec!["ecommerce".to_string(), "collaborative".to_string()],
            examples: None,
            input_modes: Some(vec!["text/plain".to_string()]),
            output_modes: Some(vec!["text/plain".to_string()]),
            security_requirements: None,
        }],
        default_input_modes: vec!["text/plain".to_string()],
        default_output_modes: vec!["text/plain".to_string()],
        supported_interfaces: vec![
            AgentInterface::new(format!("{base_url}/jsonrpc"), TRANSPORT_PROTOCOL_JSONRPC),
            AgentInterface::new(format!("{base_url}/rest"), TRANSPORT_PROTOCOL_HTTP_JSON),
        ],
        security_schemes: None,
        security_requirements: None,
        documentation_url: None,
        icon_url: None,
        signatures: None,
    }
}
