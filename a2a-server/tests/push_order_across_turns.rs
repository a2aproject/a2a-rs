// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Push notifications of one task keep the order of the events they announce,
//! across turns and for a cancel, while the last push of a turn is slow.
use std::sync::Arc;
use std::time::Duration;

use a2a::*;
use a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, HttpPushSender, HttpPushSenderConfig,
    InMemoryPushConfigStore, InMemoryTaskStore, RequestHandler, ServiceParams,
};
use futures::{StreamExt, stream, stream::BoxStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

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
        if text == "two" {
            let (t2, c2) = (t.clone(), c.clone());
            return Box::pin(
                stream::once(async move { Ok(status(&t, &c, TaskState::Working, "w-two")) }).chain(
                    stream::once(async move {
                        Ok(status(&t2, &c2, TaskState::InputRequired, "reply-to-two"))
                    }),
                ),
            );
        }
        Box::pin(stream::once(async move {
            Ok(status(
                &t,
                &c,
                TaskState::InputRequired,
                &format!("reply-to-{text}"),
            ))
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

fn req(text: &str, task_id: Option<&str>, push: Option<&str>) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task_id.map(Into::into);
    SendMessageRequest {
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
    }
}

fn handler(timeout: Duration, fail: bool) -> Arc<DefaultRequestHandler> {
    Arc::new(
        DefaultRequestHandler::new(Agent, InMemoryTaskStore::new())
            .with_capabilities(a2a::AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            })
            .with_push_notifications(
                InMemoryPushConfigStore::new(),
                HttpPushSender::new(Some(HttpPushSenderConfig {
                    validate_urls: false,
                    timeout,
                    fail_on_error: fail,
                })),
            )
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                push_notifications: Some(true),
                ..AgentCapabilities::default()
            }),
    )
}

fn params() -> ServiceParams {
    let mut p = ServiceParams::new();
    p.insert(SVC_PARAM_VERSION.to_string(), vec!["1.1".to_string()]);
    p
}

/// Webhook: for each request, passes the body to `on` (awaited before the
/// response), answers with `code`.
async fn webhook(
    code: u16,
    tx: mpsc::UnboundedSender<(String, tokio::sync::oneshot::Sender<()>)>,
) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let (mut s, _) = listener.accept().await.unwrap();
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 65536];
                let len = s.read(&mut buf).await.unwrap_or(0);
                let body = String::from_utf8_lossy(&buf[..len]).to_string();
                let (done, wait) = tokio::sync::oneshot::channel();
                let _ = tx.send((body, done));
                let _ = wait.await;
                let resp =
                    format!("HTTP/1.1 {code} X\r\ncontent-length: 0\r\nconnection: close\r\n\r\n");
                let _ = s.write_all(resp.as_bytes()).await;
            });
        }
    });
    url
}

/// Turn 1's tail push is slow; turn 2 has a non-tail push (w-two) then its
/// tail; then a cancel. Order seen by the webhook (by arrival).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pushes_of_a_later_turn_and_a_cancel_wait_for_the_last_push_of_the_turn_before() {
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, tokio::sync::oneshot::Sender<()>)>();
    let url = webhook(200, tx).await;
    let order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let o2 = order.clone();
    tokio::spawn(async move {
        while let Some((body, d)) = rx.recv().await {
            let tag = ["reply-to-one", "w-two", "reply-to-two", "\"c\""]
                .iter()
                .find(|k| body.contains(*k))
                .map(|s| s.to_string())
                .unwrap_or_else(|| "?".into());
            o2.lock().unwrap().push(format!("arrive:{tag}"));
            let o3 = o2.clone();
            tokio::spawn(async move {
                if tag == "reply-to-one" {
                    tokio::time::sleep(Duration::from_millis(400)).await;
                }
                o3.lock().unwrap().push(format!("done:{tag}"));
                let _ = d.send(());
            });
        }
    });
    let h = handler(Duration::from_secs(3), false);
    let first = h
        .send_message(&params(), req("one", None, Some(&url)))
        .await
        .unwrap();
    let task_id = match first {
        SendMessageResponse::Task(t) => t.id,
        other => panic!("{other:?}"),
    };
    let _ = h
        .send_message(&params(), req("two", Some(&task_id), None))
        .await
        .unwrap();
    // The second turn may start a moment after the call returns (a message
    // that reaches the first turn as it ends is taken up by the next): cancel
    // once it has said its piece.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let task = h
                .get_task(
                    &params(),
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
            let said = task.timeline.iter().flatten().any(|entry| {
                matches!(
                    &entry.content,
                    TimelineContent::Status(status)
                        if status.message.as_ref().and_then(|m| m.text()) == Some("reply-to-two")
                )
            });
            if said {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the second turn runs");
    let _ = h
        .cancel_task(
            &params(),
            CancelTaskRequest {
                id: task_id,
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    // Deliveries are made one at a time: wait for the last, not for a while.
    let order = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let seen = order.lock().unwrap().clone();
            if seen.iter().any(|s| s == "done:\"c\"") {
                return seen;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "the cancel was never delivered: {:?}",
            order.lock().unwrap()
        )
    });
    // One at a time, in the order the events were published.
    let arrivals: Vec<_> = order
        .iter()
        .filter(|s| s.starts_with("arrive:") && !s.ends_with('?'))
        .map(String::as_str)
        .collect();
    assert_eq!(
        arrivals,
        [
            "arrive:reply-to-one",
            "arrive:w-two",
            "arrive:reply-to-two",
            "arrive:\"c\""
        ],
        "{order:?}"
    );
    let pos = |t: &str| order.iter().position(|s| s == t);
    assert!(
        pos("done:reply-to-one") < pos("arrive:w-two"),
        "turn two was pushed before turn one's last delivery finished: {order:?}"
    );
}

/// A receiver that has not answered a push, for example because it is calling
/// back into the task, does not hold up the task: its deliveries wait in their
/// own queue while the executions go on, including the events they push.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_receiver_that_has_not_answered_does_not_hold_up_the_next_turn() {
    let (tx, mut rx) = mpsc::unbounded_channel::<(String, tokio::sync::oneshot::Sender<()>)>();
    let url = webhook(200, tx).await;
    // Takes every push and answers none.
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Some(push) = rx.recv().await {
            held.push(push);
        }
    });
    let h = handler(Duration::from_secs(30), false);
    let first = h
        .send_message(&params(), req("one", None, Some(&url)))
        .await
        .unwrap();
    let SendMessageResponse::Task(task) = first else {
        panic!("a task is returned");
    };
    // Turn two pushes a status and then its reply; neither waits for the
    // receiver, which is still holding turn one's.
    tokio::time::timeout(
        Duration::from_secs(3),
        h.send_message(&params(), req("two", Some(&task.id), None)),
    )
    .await
    .expect("the next turn waited for the receiver")
    .unwrap();
}
