// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! The push of a turn's last event, which goes on after the turn has ended.
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

static AFTER_TERMINAL_RAN: AtomicBool = AtomicBool::new(false);

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
        let text = ctx
            .message
            .as_ref()
            .and_then(|m| m.text().map(str::to_owned))
            .unwrap_or_default();
        match text.as_str() {
            "working" => Box::pin(stream::once(async move {
                Ok(status(&t, &c, TaskState::Working, "still working"))
            })),
            "complete" => {
                let (t2, c2) = (t.clone(), c.clone());
                Box::pin(
                    stream::once(async move { Ok(status(&t, &c, TaskState::Completed, "done")) })
                        .chain(stream::once(async move {
                            AFTER_TERMINAL_RAN.store(true, Ordering::SeqCst);
                            Ok(status(&t2, &c2, TaskState::Working, "after"))
                        })),
                )
            }
            other => {
                let other = other.to_string();
                Box::pin(stream::once(async move {
                    Ok(status(
                        &t,
                        &c,
                        TaskState::InputRequired,
                        &format!("reply-to-{other}"),
                    ))
                }))
            }
        }
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

fn config(url: Option<&str>) -> Option<SendMessageConfiguration> {
    url.map(|url| SendMessageConfiguration {
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
    })
}

fn req(text: &str, task_id: Option<&str>, push: Option<&str>) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task_id.map(Into::into);
    SendMessageRequest {
        message,
        configuration: config(push),
        metadata: None,
        tenant: None,
        artifact_updates: Vec::new(),
    }
}

/// Webhook that answers slowly for the first request and for the push of the
/// first turn's reply, and records what it was sent in the order it finished.
async fn webhook(first_delay: Duration) -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let count = Arc::new(AtomicUsize::new(0));
    let seen2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let seen = seen2.clone();
            let n = count.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let len = s.read(&mut buf).await.unwrap_or(0);
                let body = String::from_utf8_lossy(&buf[..len]).to_string();
                if body.contains("reply-to-one") || (n == 0 && !body.contains("reply-to-")) {
                    tokio::time::sleep(first_delay).await;
                }
                // record at the time the receiver "processes" it (after delay)
                let tag = ["reply-to-", "still working", "\"c\"", "done"]
                    .iter()
                    .find_map(|k| {
                        body.find(k)
                            .map(|i| body[i..].chars().take(20).collect::<String>())
                    })
                    .unwrap_or_else(|| "?".into());
                seen.lock().unwrap().push(tag);
                let _ = s
                    .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    (url, seen)
}

fn handler() -> DefaultRequestHandler {
    DefaultRequestHandler::new(Agent, InMemoryTaskStore::new())
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
        })
}

fn params() -> ServiceParams {
    let mut p = ServiceParams::new();
    p.insert(SVC_PARAM_VERSION.to_string(), vec!["1.1".to_string()]);
    p
}

/// Push order across turns: turn 1's last push is slow, turn 2 starts at once.
/// Does the webhook see turn 2 before turn 1?
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushes_stay_in_order_across_turns() {
    let (url, seen) = webhook(Duration::from_millis(400)).await;
    let h = handler();
    let p = params();
    let (up, rx) = mpsc::channel(8);
    up.send(StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::SendMessage(req("one", None, Some(&url))),
    })
    .await
    .unwrap();
    let mut down = h
        .send_live_message(&p, ReceiverStream::new(rx).boxed())
        .await
        .unwrap();
    let mut task_id = String::new();
    let mut n = 0;
    while n < 2 {
        let e = tokio::time::timeout(Duration::from_secs(5), down.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        // A turn that began and ended while the exchange was idle reaches it as
        // the task's snapshot, not as events.
        let asks_for_input = match &e {
            StreamResponse::Task(t) => {
                task_id = t.id.clone();
                t.status.state == TaskState::InputRequired
            }
            StreamResponse::StatusUpdate(u) => u.status.state == TaskState::InputRequired,
            _ => false,
        };
        match &e {
            _ if asks_for_input => {
                n += 1;
                if n == 1 {
                    up.send(StreamRequest {
                        tenant: None,
                        payload: StreamRequestPayload::SendMessage(req(
                            "two",
                            Some(&task_id),
                            None,
                        )),
                    })
                    .await
                    .unwrap();
                }
            }
            _ => {}
        }
    }
    // Deliveries are made one at a time: wait for turn two's, not for a while.
    let seen = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let seen = seen.lock().unwrap().clone();
            if seen.iter().any(|s| s.contains("reply-to-two")) {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("turn two's webhook is called");
    let one = seen.iter().position(|s| s.contains("reply-to-one"));
    let two = seen.iter().position(|s| s.contains("reply-to-two"));
    assert!(
        one.is_some() && one < two,
        "turn 2 pushed before turn 1: {seen:?}"
    );
}

/// A nonterminal turn leaves its stream open, and webhook delivery does not
/// delay progress or the cancellation that eventually ends the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_stream_follows_the_task_until_cancel_without_waiting_for_webhooks() {
    let (url, _seen) = webhook(Duration::from_millis(500)).await;
    let h = handler();
    let p = params();
    let mut s = h
        .send_streaming_message(&p, req("working", None, Some(&url)))
        .await
        .unwrap();
    let started = tokio::time::Instant::now();
    let StreamResponse::Task(task) = s.next().await.unwrap().unwrap() else {
        panic!("task opens the stream")
    };
    let task_id = task.id;
    assert!(
        matches!(s.next().await.unwrap().unwrap(), StreamResponse::StatusUpdate(update)
        if update.status.state == TaskState::Working)
    );
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "the stream waited for the webhook"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(30), s.next())
            .await
            .is_err(),
        "the nonterminal stream must stay open"
    );
    // A cancel after the turn ends reaches the same stream.
    let canceled = h
        .cancel_task(
            &p,
            CancelTaskRequest {
                id: task_id,
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(canceled.status.state, TaskState::Canceled);
    let event = tokio::time::timeout(Duration::from_secs(2), s.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(&event, StreamResponse::Task(task) if task.status.state == TaskState::Canceled)
            || matches!(&event, StreamResponse::StatusUpdate(update) if update.status.state == TaskState::Canceled)
    );
    assert!(s.next().await.is_none());
}

/// Code after a terminal yield: run before, or not at all?
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn code_after_a_terminal_event_does_not_run() {
    let h = handler();
    let p = params();
    let _ = h
        .send_message(&p, req("complete", None, None))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !AFTER_TERMINAL_RAN.load(Ordering::SeqCst),
        "executor code after the terminal yield ran"
    );
}
