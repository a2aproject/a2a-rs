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

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{
        AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
        GetExtendedAgentCardRequest, GetTaskPushNotificationConfigRequest, GetTaskRequest,
        ListTaskPushNotificationConfigsRequest, ListTaskPushNotificationConfigsResponse,
        ListTasksRequest, ListTasksResponse, Message, Part, Role, SendMessageRequest,
        SendMessageResponse, StreamResponse, SubscribeToTaskRequest, Task,
        TaskPushNotificationConfig, TaskState, TaskStatus, TaskStatusUpdateEvent,
    };
    use a2a_client::{A2AClient, transport::ServiceParams};
    use async_trait::async_trait;
    use futures::StreamExt;
    use futures::stream::BoxStream;
    use std::sync::{Arc, Mutex};

    struct MockTransport {
        events: Vec<Result<StreamResponse, A2AError>>,
        relayed: Arc<Mutex<Vec<SendMessageRequest>>>,
        fail_start: bool,
    }

    impl MockTransport {
        fn new(events: Vec<StreamResponse>, relayed: Arc<Mutex<Vec<SendMessageRequest>>>) -> Self {
            Self {
                events: events.into_iter().map(Ok).collect(),
                relayed,
                fail_start: false,
            }
        }
    }

    #[async_trait]
    impl Transport for MockTransport {
        async fn send_message(
            &self,
            _params: &ServiceParams,
            req: &SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            self.relayed.lock().unwrap().push(req.clone());
            Ok(SendMessageResponse::Task(make_task("relay", "relay-ctx")))
        }

        async fn send_streaming_message(
            &self,
            _params: &ServiceParams,
            _req: &SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            if self.fail_start {
                return Err(A2AError::internal("mock failure"));
            }
            Ok(Box::pin(futures::stream::iter(self.events.clone())))
        }

        async fn get_task(&self, _: &ServiceParams, _: &GetTaskRequest) -> Result<Task, A2AError> {
            Err(A2AError::internal("not implemented"))
        }
        async fn list_tasks(
            &self,
            _: &ServiceParams,
            _: &ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            Err(A2AError::internal("not implemented"))
        }
        async fn cancel_task(
            &self,
            _: &ServiceParams,
            _: &CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            Err(A2AError::internal("not implemented"))
        }
        async fn subscribe_to_task(
            &self,
            _: &ServiceParams,
            _: &SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            Err(A2AError::internal("not implemented"))
        }
        async fn create_push_config(
            &self,
            _: &ServiceParams,
            _: &TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            Err(A2AError::internal("not implemented"))
        }
        async fn get_push_config(
            &self,
            _: &ServiceParams,
            _: &GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            Err(A2AError::internal("not implemented"))
        }
        async fn list_push_configs(
            &self,
            _: &ServiceParams,
            _: &ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            Err(A2AError::internal("not implemented"))
        }
        async fn delete_push_config(
            &self,
            _: &ServiceParams,
            _: &DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            Err(A2AError::internal("not implemented"))
        }
        async fn get_extended_agent_card(
            &self,
            _: &ServiceParams,
            _: &GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            Err(A2AError::internal("not implemented"))
        }
        async fn destroy(&self) -> Result<(), A2AError> {
            Ok(())
        }
    }

    fn make_task(id: &str, ctx: &str) -> Task {
        Task {
            id: id.into(),
            context_id: ctx.into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    fn initial_request() -> SendMessageRequest {
        SendMessageRequest {
            message: Message::new(Role::User, vec![Part::text("hello")]),
            configuration: None,
            metadata: None,
            tenant: None,
        }
    }

    fn make_status_update(task_id: &str, ctx: &str) -> StreamResponse {
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.into(),
            context_id: ctx.into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: Some(Message::new(Role::Agent, vec![Part::text("done")])),
                timestamp: None,
            },
            metadata: None,
        })
    }

    #[tokio::test]
    async fn start_returns_error_when_transport_fails() {
        let session = CollaborativeSession::new(vec![
            (
                "a".into(),
                A2AClient::new(MockTransport {
                    events: vec![],
                    relayed: Arc::new(Mutex::new(vec![])),
                    fail_start: true,
                }),
            ),
            (
                "b".into(),
                A2AClient::new(MockTransport::new(vec![], Arc::new(Mutex::new(vec![])))),
            ),
        ]);
        assert!(session.start(initial_request()).await.is_err());
    }

    #[tokio::test]
    async fn stream_yields_events_from_all_agents() {
        let session = CollaborativeSession::new(vec![
            (
                "a".into(),
                A2AClient::new(MockTransport::new(
                    vec![StreamResponse::Task(make_task("ta", "ca"))],
                    Arc::new(Mutex::new(vec![])),
                )),
            ),
            (
                "b".into(),
                A2AClient::new(MockTransport::new(
                    vec![StreamResponse::Task(make_task("tb", "cb"))],
                    Arc::new(Mutex::new(vec![])),
                )),
            ),
        ]);

        let mut stream = session.start(initial_request()).await.unwrap();
        let mut labels = vec![];
        while let Some((label, _)) = stream.next().await {
            labels.push(label);
        }

        assert_eq!(labels.len(), 2);
        assert!(labels.contains(&"a".to_string()));
        assert!(labels.contains(&"b".to_string()));
    }

    #[tokio::test]
    async fn stream_errors_are_skipped() {
        let session = CollaborativeSession::new(vec![
            (
                "a".into(),
                A2AClient::new(MockTransport {
                    events: vec![
                        Err(A2AError::internal("transient")),
                        Ok(StreamResponse::Task(make_task("ta", "ca"))),
                    ],
                    relayed: Arc::new(Mutex::new(vec![])),
                    fail_start: false,
                }),
            ),
            (
                "b".into(),
                A2AClient::new(MockTransport::new(
                    vec![StreamResponse::Task(make_task("tb", "cb"))],
                    Arc::new(Mutex::new(vec![])),
                )),
            ),
        ]);

        let mut stream = session.start(initial_request()).await.unwrap();
        let mut count = 0;
        while stream.next().await.is_some() {
            count += 1;
        }
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn relay_forwards_event_to_peer_once_session_established() {
        let relayed_b = Arc::new(Mutex::new(vec![]));

        // Agent b is at index 0 so its Task event is processed first by
        // select_all, establishing session IDs before agent a's StatusUpdate.
        let session = CollaborativeSession::new(vec![
            (
                "b".into(),
                A2AClient::new(MockTransport::new(
                    vec![StreamResponse::Task(make_task("tb", "cb"))],
                    Arc::clone(&relayed_b),
                )),
            ),
            (
                "a".into(),
                A2AClient::new(MockTransport::new(
                    vec![
                        StreamResponse::Task(make_task("ta", "ca")),
                        make_status_update("ta", "ca"),
                    ],
                    Arc::new(Mutex::new(vec![])),
                )),
            ),
        ]);

        let mut stream = session.start(initial_request()).await.unwrap();
        while stream.next().await.is_some() {}

        // B receives A's Task relay then A's StatusUpdate relay — 2 total,
        // since all StreamResponse variants are relayable.
        assert_eq!(relayed_b.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn no_relay_before_peer_establishes_session() {
        let relayed_b = Arc::new(Mutex::new(vec![]));

        // Agent b yields nothing, so it never establishes a session.
        // Agent a's StatusUpdate must not be forwarded to b.
        let session = CollaborativeSession::new(vec![
            (
                "a".into(),
                A2AClient::new(MockTransport::new(
                    vec![
                        StreamResponse::Task(make_task("ta", "ca")),
                        make_status_update("ta", "ca"),
                    ],
                    Arc::new(Mutex::new(vec![])),
                )),
            ),
            (
                "b".into(),
                A2AClient::new(MockTransport::new(vec![], Arc::clone(&relayed_b))),
            ),
        ]);

        let mut stream = session.start(initial_request()).await.unwrap();
        while stream.next().await.is_some() {}

        assert!(relayed_b.lock().unwrap().is_empty());
    }
}
