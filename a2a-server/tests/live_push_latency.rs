// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! A client that answers at once, while the final push of the turn is still in
//! flight, starts the next turn instead of being declined.
use std::time::Duration;

use a2a::*;
use a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, HttpPushSender, HttpPushSenderConfig,
    InMemoryPushConfigStore, InMemoryTaskStore, RequestHandler, ServiceParams,
};
use futures::{StreamExt, stream, stream::BoxStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

fn status(t: &str, c: &str, state: TaskState, text: &str) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: t.into(),
        context_id: c.into(),
        status: TaskStatus {
            state,
            message: Some(Message::new(Role::Agent, vec![Part::text(text)])),
            timestamp: None,
        },
        metadata: None,
        generation: 0,
    })
}

struct Agent;
impl AgentExecutor for Agent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (t, c) = (ctx.task_id.clone(), ctx.context_id.clone());
        Box::pin(stream::once(async move {
            Ok(status(&t, &c, TaskState::InputRequired, "need input"))
        }))
    }
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::once(async move {
            Ok(status(
                &ctx.task_id,
                &ctx.context_id,
                TaskState::Canceled,
                "c",
            ))
        }))
    }
}

fn send(text: &str, task_id: Option<&str>, push: Option<&str>) -> StreamRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task_id.map(Into::into);
    StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::SendMessage(SendMessageRequest {
            message,
            configuration: push.map(|url| SendMessageConfiguration {
                task_push_notification_config: Some(TaskPushNotificationConfig {
                    url: url.to_string(),
                    id: None,
                    task_id: String::new(),
                    token: None,
                    authentication: None,
                    tenant: None,
                    protocol_version: None,
                }),
                accepted_output_modes: None,
                history_length: None,
                timeline_length: None,
                return_immediately: None,
                if_generation_match: None,
            }),
            metadata: None,
            tenant: None,
            artifact_updates: Vec::new(),
        }),
    }
}

/// F2 window 2 with a webhook that takes 300 ms: the client answers the
/// INPUT_REQUIRED at once, as a client would.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r5_answer_input_required_while_push_in_flight() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let _ = s.read(&mut buf).await;
                tokio::time::sleep(Duration::from_millis(300)).await;
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    let h = DefaultRequestHandler::new(Agent, InMemoryTaskStore::new())
        .with_capabilities(a2a::AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        })
        .with_push_notifications(
            InMemoryPushConfigStore::new(),
            HttpPushSender::new(Some(HttpPushSenderConfig {
                validate_urls: false,
                ..Default::default()
            })),
        )
        .with_capabilities(AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(true),
            ..AgentCapabilities::default()
        });
    let mut p = ServiceParams::new();
    p.insert(SVC_PARAM_VERSION.to_string(), vec!["1.1".to_string()]);
    let (up, rx) = mpsc::channel(8);
    up.send(send("ask", None, Some(&url))).await.unwrap();
    let mut down = h
        .send_live_message(&p, ReceiverStream::new(rx).boxed())
        .await
        .unwrap();
    let mut task_id = String::new();
    let mut declined = 0;
    let mut answered = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while answered < 5 && tokio::time::Instant::now() < deadline {
        let e = tokio::time::timeout(Duration::from_secs(5), down.next())
            .await
            .expect("event")
            .expect("open")
            .expect("ok");
        match &e {
            StreamResponse::Task(t) => task_id = t.id.clone(),
            StreamResponse::StatusUpdate(u) if u.status.state == TaskState::InputRequired => {
                answered += 1;
                up.send(send(&format!("answer{answered}"), Some(&task_id), None))
                    .await
                    .unwrap();
            }
            StreamResponse::Error(_) => {
                declined += 1;
                // the client retries, as the spec allows
                answered += 1;
                up.send(send(&format!("retry{answered}"), Some(&task_id), None))
                    .await
                    .unwrap();
            }
            _ => {}
        }
    }
    let task = h
        .get_task(
            &p,
            GetTaskRequest {
                id: task_id.clone(),
                history_length: None,
                tenant: None,
                timeline_length: None,
                current_generation: None,
            },
        )
        .await
        .unwrap();
    let hist: Vec<_> = task
        .history
        .iter()
        .flatten()
        .filter_map(|m| m.text().map(str::to_owned))
        .collect();
    assert_eq!(
        declined, 0,
        "answers declined while the final push was in flight: {hist:?}"
    );
}
