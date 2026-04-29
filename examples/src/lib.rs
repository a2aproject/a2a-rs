// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

use a2a::event::StreamResponse;
use a2a::*;
use a2a_client::{A2AClient, Transport};
use a2a_server::*;
use futures::stream::{self, BoxStream};
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

// ---------------------------------------------------------------------------
// EchoExecutor — shared across plain and TLS server examples
// ---------------------------------------------------------------------------

pub struct EchoExecutor;

impl AgentExecutor for EchoExecutor {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let message = ctx.message.clone();
        let task_id = ctx.task_id.clone();
        let context_id = ctx.context_id.clone();

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
            let (tx, rx) = tokio::sync::mpsc::channel(1);
            tokio::spawn(async move {
                let _ = tx.send(Ok(working)).await;
                tx.closed().await;
            });
            Box::pin(ReceiverStream::new(rx))
        } else {
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

// ---------------------------------------------------------------------------
// Agent card builder — parameterised on interfaces
// ---------------------------------------------------------------------------

pub fn build_agent_card(interfaces: Vec<AgentInterface>) -> AgentCard {
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
        supported_interfaces: interfaces,
        security_schemes: None,
        security_requirements: None,
        documentation_url: None,
        icon_url: None,
        signatures: None,
    }
}

// ---------------------------------------------------------------------------
// Client helpers — request builders
// ---------------------------------------------------------------------------

pub fn req(text: &str) -> SendMessageRequest {
    SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text(text)]),
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

pub fn wait_req(text: &str) -> SendMessageRequest {
    SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text(format!("wait:{text}"))]),
        configuration: Some(SendMessageConfiguration {
            return_immediately: Some(true),
            accepted_output_modes: None,
            push_notification_config: None,
            history_length: None,
        }),
        metadata: None,
        tenant: None,
    }
}

// ---------------------------------------------------------------------------
// Output formatting
// ---------------------------------------------------------------------------

pub fn short_id(id: &str) -> &str {
    if id.len() > 8 { &id[id.len() - 8..] } else { id }
}

pub fn fmt_state(state: &TaskState) -> &'static str {
    match state {
        TaskState::Working       => "⟳ working",
        TaskState::Completed     => "✓ completed",
        TaskState::Canceled      => "✗ canceled",
        TaskState::Failed        => "! failed",
        TaskState::Submitted     => "· submitted",
        TaskState::Rejected      => "✗ rejected",
        TaskState::AuthRequired  => "? auth-required",
        TaskState::InputRequired => "? input-required",
        TaskState::Unspecified   => "? unspecified",
    }
}

#[macro_export]
macro_rules! row {
    ($method:expr, $id:expr, $state:expr) => {
        println!("{:<26}  …{}  {}", $method,
                 $crate::short_id($id), $crate::fmt_state($state))
    };
}

#[macro_export]
macro_rules! sub_row {
    ($n:expr, $kind:expr, $id:expr, $state:expr) => {
        println!("  [{n:>2}] {:<13}  …{}  {}", $kind,
                 $crate::short_id($id), $crate::fmt_state($state), n = $n)
    };
    ($n:expr, $kind:expr, $id:expr) => {
        println!("  [{n:>2}] {:<13}  …{}", $kind,
                 $crate::short_id($id), n = $n)
    };
}

// ---------------------------------------------------------------------------
// exercise_client — drives every A2A method
// ---------------------------------------------------------------------------

pub async fn exercise_client<T: Transport>(protocol: &str, client: &A2AClient<T>) {
    let sep = "─".repeat(50);

    println!();
    println!("{protocol}");
    println!("{sep}");

    // ---- send_message ----------------------------------------------------
    let task_id = match client.send_message(&req("Hello, world!")).await {
        Ok(SendMessageResponse::Task(t)) => {
            row!("send_message", &t.id, &t.status.state);
            t.id
        }
        Ok(SendMessageResponse::Message(m)) => {
            row!("send_message", &m.message_id, &TaskState::Unspecified);
            return;
        }
        Err(e) => {
            eprintln!("send_message               ✗  {e}");
            return;
        }
    };

    // ---- get_task --------------------------------------------------------
    match client
        .get_task(&GetTaskRequest { id: task_id.clone(), history_length: Some(10), tenant: None })
        .await
    {
        Ok(t)  => row!("get_task", &t.id, &t.status.state),
        Err(e) => eprintln!("get_task                   ✗  {e}"),
    }

    // ---- list_tasks ------------------------------------------------------
    match client
        .list_tasks(&ListTasksRequest {
            context_id: None,
            status: None,
            page_size: Some(10),
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        })
        .await
    {
        Ok(l)  => println!("{:<26}  {} task(s)", "list_tasks", l.tasks.len()),
        Err(e) => eprintln!("list_tasks                 ✗  {e}"),
    }

    // ---- cancel_task -----------------------------------------------------
    match client.send_message(&wait_req("cancel demo")).await {
        Ok(SendMessageResponse::Task(t)) => {
            match client
                .cancel_task(&CancelTaskRequest { id: t.id, metadata: None, tenant: None })
                .await
            {
                Ok(t)  => row!("cancel_task", &t.id, &t.status.state),
                Err(e) => eprintln!("cancel_task                ✗  {e}"),
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("cancel_task                ✗  {e}"),
    }

    // ---- send_streaming_message ------------------------------------------
    let stream_req = SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text("Stream me!")]),
        configuration: None,
        metadata: None,
        tenant: None,
    };
    match client.send_streaming_message(&stream_req).await {
        Ok(mut stream) => {
            println!("send_streaming_message");
            let mut n = 0usize;
            while let Some(event) = stream.next().await {
                n += 1;
                match event {
                    Ok(StreamResponse::Task(t)) =>
                        sub_row!(n, "task", &t.id, &t.status.state),
                    Ok(StreamResponse::StatusUpdate(u)) =>
                        sub_row!(n, "status_update", &u.task_id, &u.status.state),
                    Ok(StreamResponse::Message(m)) =>
                        sub_row!(n, "message", &m.message_id),
                    Ok(StreamResponse::ArtifactUpdate(a)) =>
                        sub_row!(n, "artifact", &a.artifact.artifact_id),
                    Err(e) =>
                        eprintln!("  [{n:>2}] ✗  {e}"),
                }
            }
        }
        Err(e) => eprintln!("send_streaming_message     ✗  {e}"),
    }

    // ---- subscribe_to_task -----------------------------------------------
    match client.send_message(&wait_req("subscribe demo")).await {
        Ok(SendMessageResponse::Task(t)) => {
            let sub_id = t.id;
            match client
                .subscribe_to_task(&SubscribeToTaskRequest { id: sub_id.clone(), tenant: None })
                .await
            {
                Ok(mut sub) => {
                    match client
                        .cancel_task(&CancelTaskRequest { id: sub_id.clone(), metadata: None, tenant: None })
                        .await
                    {
                        Ok(t)  => row!("subscribe_to_task (cancel)", &t.id, &t.status.state),
                        Err(e) => eprintln!("subscribe_to_task (cancel) ✗  {e}"),
                    }
                    let mut n = 0usize;
                    while let Some(event) = sub.next().await {
                        n += 1;
                        match event {
                            Ok(StreamResponse::Task(t)) =>
                                sub_row!(n, "task", &t.id, &t.status.state),
                            Ok(StreamResponse::StatusUpdate(u)) =>
                                sub_row!(n, "status_update", &u.task_id, &u.status.state),
                            Ok(_) => {}
                            Err(e) => eprintln!("  [{n:>2}] ✗  {e}"),
                        }
                    }
                }
                Err(e) => eprintln!("subscribe_to_task          ✗  {e}"),
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("subscribe_to_task          ✗  {e}"),
    }

    println!("{sep}");
}
