// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

use a2a::*;
use a2a_client::{A2AClient, Transport};
use futures::channel::mpsc;
use futures::stream::BoxStream;
use futures::{SinkExt, StreamExt};

use crate::translate::translate_peer_response;

/// A collaborative task session connecting multiple A2A agents.
///
/// Implements the Basic Relay (spec §2.1): streams from all agents are merged,
/// and each event is translated and forwarded to every other agent.
///
/// # Example
///
/// ```no_run
/// # use a2a_collab::CollaborativeSession;
/// # use a2a::*;
/// # use a2a_client::{A2AClient, Transport};
/// # async fn example() -> Result<(), A2AError> {
/// # let agents: Vec<(String, A2AClient<Box<dyn Transport>>)> = vec![];
/// use futures::StreamExt;
///
/// let request = SendMessageRequest {
///     message: Message::new(Role::User, vec![Part::text("hello")]),
///     configuration: None,
///     metadata: None,
///     tenant: None,
/// };
///
/// let mut stream = CollaborativeSession::new(agents)
///     .start(request)
///     .await?;
///
/// while let Some((agent, event)) = stream.next().await {
///     println!("[{agent}] {event:?}");
/// }
/// # Ok(())
/// # }
/// ```
pub struct CollaborativeSession<T: Transport> {
    agents: Vec<(String, A2AClient<T>)>,
}

impl<T: Transport + Send + 'static> CollaborativeSession<T> {
    /// Create a new session from labelled agent clients.
    ///
    /// Pass at least two `(label, client)` pairs. Labels are used for attribution
    /// in the shared-task `message-sender` metadata of relayed messages, and for
    /// identifying the source agent in the returned stream items.
    pub fn new(agents: Vec<(String, A2AClient<T>)>) -> Self {
        Self { agents }
    }

    /// Open streams to all agents, send the initial message, and start the relay loop.
    ///
    /// All `send_streaming_message` calls are made before this returns — if any
    /// agent is unreachable the error is returned immediately and no background
    /// task is started.
    ///
    /// Returns a stream of `(agent_label, StreamResponse)` for the caller to render.
    /// Dropping the returned stream terminates the background relay task.
    pub async fn start(
        self,
        initial_request: SendMessageRequest,
    ) -> Result<BoxStream<'static, (String, StreamResponse)>, A2AError> {
        let n = self.agents.len();
        let labels: Vec<String> = self.agents.iter().map(|(l, _)| l.clone()).collect();

        // Open all streams first — fail fast if any agent is unreachable.
        let mut tagged = Vec::with_capacity(n);
        for (i, (_, client)) in self.agents.iter().enumerate() {
            let stream = client.send_streaming_message(&initial_request).await?;
            tagged.push(stream.map(move |ev| (i, ev)).boxed());
        }

        let clients: Vec<A2AClient<T>> = self.agents.into_iter().map(|(_, c)| c).collect();

        let (mut tx, rx) = mpsc::channel::<(String, StreamResponse)>(64);

        tokio::spawn(async move {
            let mut context_ids: Vec<Option<String>> = vec![None; n];
            let mut task_ids: Vec<Option<String>> = vec![None; n];
            let mut merged = futures::stream::select_all(tagged);

            while let Some((src, result)) = merged.next().await {
                let event = match result {
                    Ok(e) => e,
                    Err(err) => {
                        tracing::warn!("stream error from [{}]: {err}", labels[src]);
                        continue;
                    }
                };

                // Cache session IDs from the first Task event per agent.
                if let StreamResponse::Task(task) = &event
                    && context_ids[src].is_none()
                {
                    context_ids[src] = Some(task.context_id.clone());
                    task_ids[src] = Some(task.id.clone());
                }

                // Relay to every other agent that has an established session.
                let sender = labels[src].clone();
                for dst in 0..n {
                    if dst == src {
                        continue; // Echo suppression.
                    }
                    let (Some(ctx), Some(tid)) = (&context_ids[dst], &task_ids[dst]) else {
                        continue; // No session ID yet — skip until first Task arrives.
                    };
                    let Some(mut relay) = translate_peer_response(&event, &sender, ctx, tid) else {
                        continue;
                    };
                    relay.configuration = Some(SendMessageConfiguration {
                        return_immediately: Some(true),
                        accepted_output_modes: None,
                        task_push_notification_config: None,
                        history_length: None,
                    });
                    if let Err(err) = clients[dst].send_message(&relay).await {
                        tracing::warn!("relay to [{}] failed: {err}", labels[dst]);
                    }
                }

                // Yield to caller; stop if the stream was dropped.
                if tx.send((labels[src].clone(), event)).await.is_err() {
                    break;
                }
            }
        });

        Ok(Box::pin(rx))
    }
}
