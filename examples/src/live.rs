// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! A live conversation that spans multiple executor invocations.

use a2a::*;
use a2a_client::{A2AClient, LiveAck, LiveConnection, Transport};
use a2a_server::{AgentExecutor, ExecutorContext};
use futures::{StreamExt, stream::BoxStream};

/// Echoes input while running. `pause` returns from execute with InputRequired;
/// later input starts another invocation for the same task. `finish` completes it.
pub struct LiveEchoExecutor;

impl AgentExecutor for LiveEchoExecutor {
    fn execute(
        &self,
        mut ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(async_stream::stream! {
            // The triggering message belongs to this invocation, not the inbox.
            let mut message = ctx.message;
            loop {
                let text = message.as_ref().and_then(Message::text).unwrap_or("");
                let (state, reply) = match text {
                    "pause" => (TaskState::InputRequired, "Send another message to resume".to_string()),
                    "finish" => (TaskState::Completed, "Conversation complete".to_string()),
                    _ => (TaskState::Working, format!("Echo: {text}")),
                };
                let keep_running = state == TaskState::Working;
                yield Ok(status(&ctx.task_id, &ctx.context_id, state, &reply));
                if !keep_running {
                    return;
                }
                // This owned receiver can also move into a Tokio worker.
                // Awaiting input yields; it does not block other tasks.
                match ctx.inputs.recv().await {
                    Some(input) => message = Some(input.message),
                    None => return,
                }
            }
        })
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(futures::stream::once(async move {
            Ok(status(
                &ctx.task_id,
                &ctx.context_id,
                TaskState::Canceled,
                "Canceled",
            ))
        }))
    }
}

fn status(task: &str, context: &str, state: TaskState, text: &str) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: task.into(),
        context_id: context.into(),
        status: TaskStatus {
            state,
            message: Some(Message::new(Role::Agent, vec![Part::text(text)])),
            timestamp: None,
        },
        metadata: None,
        generation: 0, // The handler assigns persisted generations.
    })
}

/// Opens one exchange and uses both its uplink and downlink through completion.
/// Returns the received events so the example's integration tests can verify them.
pub async fn converse<T: Transport>(
    client: &A2AClient<T>,
) -> Result<Vec<StreamResponse>, A2AError> {
    let first = StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::SendMessage(crate::req("start")),
    };
    let mut live = client.send_live_message(&first).await?;
    let mut events = Vec::new();

    // The opening snapshot supplies the task ID for every subsequent message.
    let opening = live
        .events
        .next()
        .await
        .transpose()?
        .ok_or_else(|| A2AError::internal("live exchange ended before its opening task"))?;
    let StreamResponse::Task(task) = &opening else {
        return Err(A2AError::internal("expected an opening task"));
    };
    let task_id = task.id.clone();
    events.push(opening);
    receive_until(&mut live, TaskState::Working, &mut events).await?;

    for (text, expected) in [
        ("hello", TaskState::Working),
        ("pause", TaskState::InputRequired),
        ("resumed", TaskState::Working),
        ("finish", TaskState::Completed),
    ] {
        let mut message = Message::new(Role::User, vec![Part::text(text)]);
        message.task_id = Some(task_id.clone());
        // HTTP returns a task receipt; gRPC acknowledges the write. In both
        // cases, the executor's response arrives on live.events below.
        if matches!(
            live.sender.send_message(message).await?,
            LiveAck::Reopened(_)
        ) {
            return Err(A2AError::internal("the live exchange unexpectedly closed"));
        }
        receive_until(&mut live, expected, &mut events).await?;
    }
    // A terminal task closes the downlink; InputRequired above did not.
    if let Some(event) = live.events.next().await {
        event?;
        return Err(A2AError::internal("unexpected event after completion"));
    }
    Ok(events)
}

async fn receive_until(
    live: &mut LiveConnection,
    expected: TaskState,
    events: &mut Vec<StreamResponse>,
) -> Result<(), A2AError> {
    while let Some(event) = live.events.next().await {
        let event = event?;
        if let StreamResponse::Error(error) = &event {
            return Err(A2AError::internal(format!(
                "live request rejected: {error:?}"
            )));
        }
        let reached = match &event {
            StreamResponse::StatusUpdate(update) if update.status.state == expected => true,
            StreamResponse::StatusUpdate(update) => {
                return Err(A2AError::internal(format!(
                    "unexpected status: {:?}",
                    update.status.state
                )));
            }
            _ => false, // Client messages are also announced on the downlink.
        };
        events.push(event);
        if reached {
            return Ok(());
        }
    }
    Err(A2AError::internal(
        "live exchange ended before the expected status",
    ))
}
