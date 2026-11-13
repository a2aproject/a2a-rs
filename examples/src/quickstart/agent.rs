// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! The agent from the quickstart: a greeter that streams its answer and asks
//! for a name when it is not given one.

// --8<-- [start:imports]
use std::time::Duration;

use a2a::event::{StreamResponse, TaskArtifactUpdateEvent, TaskStatusUpdateEvent};
use a2a::*;
use a2a_server::{AgentExecutor, ExecutorContext};
use futures::StreamExt;
use futures::stream::{self, BoxStream};
// --8<-- [end:imports]

// --8<-- [start:card]
/// The agent card: who the agent is, what it can do and where to reach it.
pub fn agent_card(base_url: &str) -> AgentCard {
    AgentCard {
        name: "Greeter".to_string(),
        description: "Greets you by name. Asks for the name first if it was not given.".to_string(),
        version: "0.1.0".to_string(),
        provider: None,
        capabilities: AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(false),
            extensions: None,
            extended_agent_card: None,
        },
        skills: vec![AgentSkill {
            id: "greet".to_string(),
            name: "Greet".to_string(),
            description: "Greets the name in the message.".to_string(),
            tags: vec!["greeting".to_string()],
            examples: Some(vec!["Ada".to_string()]),
            input_modes: None,
            output_modes: None,
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
// --8<-- [end:card]

// --8<-- [start:executor]
/// The agent's logic.
pub struct Greeter;

impl AgentExecutor for Greeter {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (task_id, context_id) = ctx.task_info();
        let text = ctx
            .message
            .as_ref()
            .and_then(Message::text)
            .unwrap_or_default()
            .trim()
            .to_string();
        // A task the agent left waiting for input is being answered.
        let answering = matches!(
            &ctx.stored_task,
            Some(task) if task.status.state == TaskState::InputRequired
        );

        let events = if text.eq_ignore_ascii_case("hello") && !answering {
            // No name yet: ask for one and wait for the next message.
            vec![status(
                &task_id,
                &context_id,
                TaskState::InputRequired,
                Some("Who should I greet?"),
            )]
        } else {
            // Greet in pieces, so that a streaming client sees them arrive.
            vec![
                status(&task_id, &context_id, TaskState::Working, None),
                chunk(&task_id, &context_id, "Hello, ", false, false),
                chunk(&task_id, &context_id, &text, true, false),
                chunk(&task_id, &context_id, "!", true, true),
                status(&task_id, &context_id, TaskState::Completed, None),
            ]
        };

        Box::pin(stream::iter(events).then(|event| async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Ok(event)
        }))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (task_id, context_id) = ctx.task_info();
        let canceled = status(&task_id, &context_id, TaskState::Canceled, None);
        Box::pin(stream::once(async move { Ok(canceled) }))
    }
}

/// A status update for the task, with an optional message from the agent.
fn status(
    task_id: &str,
    context_id: &str,
    state: TaskState,
    message: Option<&str>,
) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: task_id.to_string(),
        context_id: context_id.to_string(),
        status: TaskStatus {
            state,
            message: message.map(|text| Message::new(Role::Agent, vec![Part::text(text)])),
            timestamp: None,
        },
        metadata: None,
    })
}

/// One piece of the `greeting` artifact. `append` adds it to the pieces sent
/// before; `last` marks the end of the artifact.
fn chunk(task_id: &str, context_id: &str, text: &str, append: bool, last: bool) -> StreamResponse {
    StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
        task_id: task_id.to_string(),
        context_id: context_id.to_string(),
        artifact: Artifact {
            artifact_id: "greeting".to_string(),
            name: None,
            description: None,
            parts: vec![Part::text(text)],
            metadata: None,
            extensions: None,
        },
        append: Some(append),
        last_chunk: Some(last),
        metadata: None,
    })
}
// --8<-- [end:executor]
