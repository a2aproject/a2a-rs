// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Races and failures around `SendLiveMessage` sessions: events of a finished
//! turn, a panic mid-call, a call from another client, a message at turn end.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use a2a::*;
use a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore, LiveResponse,
    RequestHandler, ServiceParams,
};
use futures::{StreamExt, stream, stream::BoxStream};
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

/// "burst": 45 WORKING updates then INPUT_REQUIRED "end of burst", turn ends.
/// "ask": INPUT_REQUIRED. "reads": WORKING, then reads input forever.
struct Agent;

impl AgentExecutor for Agent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let first = ctx
            .message
            .as_ref()
            .and_then(|m| m.text())
            .map(str::to_owned);
        Box::pin(async_stream::stream! {
            let (t, c) = (ctx.task_id.clone(), ctx.context_id.clone());
            match first.as_deref() {
                Some("overrun") => {
                    // More than the execution's buffer holds, with nobody
                    // draining it: a session that follows it falls behind.
                    for i in 0..200 {
                        yield Ok(status(&t, &c, TaskState::Working, &format!("w{i}")));
                    }
                    yield Ok(status(&t, &c, TaskState::InputRequired, "end of overrun"));
                }
                Some("burst") => {
                    for i in 0..45 {
                        yield Ok(status(&t, &c, TaskState::Working, &format!("w{i}")));
                        if i == 0 {
                            // The session is following by now: the rest is a
                            // burst it keeps up with, not one that overruns the
                            // execution's buffer before it first polls.
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                    yield Ok(status(&t, &c, TaskState::InputRequired, "end of burst"));
                }
                Some("reads") => {
                    yield Ok(status(&t, &c, TaskState::Working, "reading"));
                    let mut inputs = ctx.inputs;
                    while let Some(input) = inputs.recv().await {
                        yield Ok(status(&t, &c, TaskState::Working, &format!("got: {}", input.message.text().unwrap_or(""))));
                    }
                }
                _ => {
                    yield Ok(status(&t, &c, TaskState::InputRequired, "need input"));
                }
            }
        })
    }
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::once(async move {
            Ok(status(
                &ctx.task_id,
                &ctx.context_id,
                TaskState::Canceled,
                "canceled",
            ))
        }))
    }
}

#[derive(Clone, Default)]
struct Flaky {
    inner: Arc<InMemoryTaskStore>,
    panic_after_update: Arc<AtomicBool>,
    panic_get: Arc<AtomicBool>,
    panic_on_canceled: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl a2a_server::TaskStore for Flaky {
    async fn create(&self, task: Task) -> Result<a2a_server::task_store::TaskVersion, A2AError> {
        self.inner.create(task).await
    }
    async fn update(&self, task: Task) -> Result<a2a_server::task_store::TaskVersion, A2AError> {
        let r = self.inner.update(task).await;
        if self.panic_after_update.swap(false, Ordering::SeqCst) {
            panic!("store panicked after saving");
        }
        r
    }
    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        if self.panic_get.swap(false, Ordering::SeqCst) {
            panic!("store panicked in get");
        }
        let task = self.inner.get(task_id).await;
        // Armed, it panics on every read of a task that has been canceled.
        if self.panic_on_canceled.load(Ordering::SeqCst)
            && matches!(&task, Ok(Some(t)) if t.status.state == TaskState::Canceled)
        {
            panic!("store panicked reading a canceled task");
        }
        task
    }
    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.inner.list(req).await
    }
    async fn begin_cancel(&self, task_id: &str) -> Result<Task, A2AError> {
        self.inner.begin_cancel(task_id).await
    }
}

fn handler_with() -> (Arc<DefaultRequestHandler>, Flaky) {
    let store = Flaky::default();
    let h = DefaultRequestHandler::new(Agent, store.clone()).with_capabilities(AgentCapabilities {
        streaming: Some(true),
        user_artifacts: Some(true),
        ..AgentCapabilities::default()
    });
    (Arc::new(h), store)
}

fn params(version: &str, user: &str) -> ServiceParams {
    let mut p = ServiceParams::new();
    p.insert(SVC_PARAM_VERSION.to_string(), vec![version.to_string()]);
    p.insert("x-user".to_string(), vec![user.to_string()]);
    p
}

fn send(text: &str, task_id: Option<&str>) -> StreamRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task_id.map(Into::into);
    StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::SendMessage(SendMessageRequest {
            message,
            configuration: None,
            metadata: None,
            tenant: None,
            artifact_updates: Vec::new(),
        }),
    }
}

type Downlink = BoxStream<'static, Result<StreamResponse, A2AError>>;

async fn next(d: &mut Downlink) -> StreamResponse {
    tokio::time::timeout(Duration::from_secs(5), d.next())
        .await
        .expect("an event arrives")
        .expect("the stream is open")
        .expect("no transport error")
}

fn text(e: &StreamResponse) -> Option<String> {
    match e {
        StreamResponse::StatusUpdate(u) => u
            .status
            .message
            .as_ref()
            .and_then(|m| m.text())
            .map(str::to_owned),
        _ => None,
    }
}

fn gen_of(e: &StreamResponse) -> Option<i64> {
    match e {
        StreamResponse::Task(t) => Some(t.generation),
        StreamResponse::StatusUpdate(u) => Some(u.generation),
        StreamResponse::ArtifactUpdate(u) => Some(u.generation),
        StreamResponse::MessageUpdate(u) => Some(u.generation),
        _ => None,
    }
}

fn applied(task: &Task, t: &str) -> usize {
    task.timeline
        .iter()
        .flatten()
        .filter(|e| matches!(&e.content, TimelineContent::Message(m) if m.text() == Some(t)))
        .count()
}

async fn get(h: &DefaultRequestHandler, id: &str) -> Task {
    h.get_task(
        &ServiceParams::new(),
        GetTaskRequest {
            id: id.into(),
            history_length: None,
            tenant: None,
            timeline_length: None,
            current_generation: None,
        },
    )
    .await
    .unwrap()
}

/// a message that starts a new turn while the exchange still has the
/// previous turn's events buffered must not drop those events.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p1_starting_a_turn_does_not_drop_the_previous_turns_buffered_events() {
    let mut lostrounds = 0;
    let mut skipped = 0;
    for _ in 0..20 {
        let (h, _) = handler_with();
        let p = params("1.1", "a");
        let (up, rx) = mpsc::channel(8);
        up.send(send("burst", None)).await.unwrap();
        let mut down: Downlink = h
            .send_live_message(&p, ReceiverStream::new(rx).boxed())
            .await
            .unwrap();
        let task = match next(&mut down).await {
            StreamResponse::Task(t) => t,
            other => panic!("{other:?}"),
        };
        // Wait (without reading the downlink) until the burst turn is over.
        loop {
            let t = get(&h, &task.id).await;
            if t.status.state == TaskState::InputRequired {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // A loaded machine can make the session lag behind the burst, which ends
        // the exchange before the next message; that round proves nothing.
        if up.send(send("ask", Some(&task.id))).await.is_err() {
            skipped += 1;
            continue;
        }
        let mut seen = Vec::new();
        let mut last_gen = task.generation;
        let mut gap = false;
        loop {
            let e = next(&mut down).await;
            if let Some(g) = gen_of(&e) {
                if g != last_gen + 1 && !matches!(e, StreamResponse::Task(_)) {
                    gap = true;
                }
                last_gen = last_gen.max(g);
            }
            let t = text(&e);
            seen.push(t.clone());
            if t.as_deref() == Some("need input") {
                break;
            }
        }
        let got_end = seen.iter().any(|t| t.as_deref() == Some("end of burst"));
        if !got_end || gap {
            lostrounds += 1;
        }
    }
    assert_eq!(
        lostrounds, 0,
        "rounds where the previous turn's events were lost"
    );
    assert!(skipped < 10, "too many rounds ended early: {skipped}");
}

/// a panic after the message is saved (outcome unknown) must not let the
/// caller deliver it a second time.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p2_panic_after_saving_is_not_delivered_twice() {
    let (h, store) = handler_with();
    let p = params("1.1", "a");
    let LiveResponse::Stream(mut down) = h.send_live_request(&p, send("ask", None)).await.unwrap()
    else {
        panic!()
    };
    let task_id = match next(&mut down).await {
        StreamResponse::Task(t) => t.id,
        other => panic!("{other:?}"),
    };
    next(&mut down).await; // INPUT_REQUIRED

    store.panic_after_update.store(true, Ordering::SeqCst);
    let reply = tokio::time::timeout(
        Duration::from_secs(5),
        h.send_live_request(&p, send("once", Some(&task_id))),
    )
    .await
    .expect("no hang");
    let kind = match &reply {
        Ok(LiveResponse::Ack(_)) => "ack",
        Ok(LiveResponse::Stream(_)) => "reopened",
        Err(_) => "error",
        Ok(_) => "other",
    };
    let stored = get(&h, &task_id).await;
    assert_eq!(applied(&stored, "once"), 1, "reply was {kind}");
}

/// a call another client routes into this exchange: it is acknowledged and
/// read by the same task execution.
#[tokio::test]
async fn p3_call_from_another_client_reaches_the_task_input_queue() {
    let (h, _) = handler_with();
    let pa = params("1.1", "a");
    let pb = params("1.1", "b");
    let LiveResponse::Stream(mut down) =
        h.send_live_request(&pa, send("reads", None)).await.unwrap()
    else {
        panic!()
    };
    let task_id = match next(&mut down).await {
        StreamResponse::Task(t) => t.id,
        other => panic!("{other:?}"),
    };
    next(&mut down).await; // working
    let reply = h
        .send_live_request(&pb, send("from-b", Some(&task_id)))
        .await
        .unwrap();
    // The caller, which has no downlink of its own, gets the updated task.
    let LiveResponse::Ack(task) = reply else {
        panic!("a call routed into an exchange is acknowledged")
    };
    assert_eq!(applied(&task, "from-b"), 1);
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));
    let StreamResponse::StatusUpdate(update) = next(&mut down).await else {
        panic!("the task reports handling the queued message")
    };
    assert_eq!(
        update.status.message.as_ref().and_then(Message::text),
        Some("got: from-b")
    );
}

/// a session that dies (panic) ends the opener's downlink: is it told?
#[tokio::test]
async fn p4_session_panic_tells_the_downlink() {
    let (h, store) = handler_with();
    let p = params("1.1", "a");
    let (up, rx) = mpsc::channel(8);
    up.send(send("ask", None)).await.unwrap();
    let mut down: Downlink = h
        .send_live_message(&p, ReceiverStream::new(rx).boxed())
        .await
        .unwrap();
    let task_id = match next(&mut down).await {
        StreamResponse::Task(task) => task.id,
        other => panic!("{other:?}"),
    };
    next(&mut down).await; // INPUT_REQUIRED
    // Another client cancels the task: the idle session wakes and reads it, and
    // the store panics on that read.
    store.panic_on_canceled.store(true, Ordering::SeqCst);
    let canceller = h.clone();
    tokio::spawn(async move {
        let _ = canceller
            .cancel_task(
                &ServiceParams::new(),
                CancelTaskRequest {
                    id: task_id,
                    metadata: None,
                    tenant: None,
                },
            )
            .await;
    });
    let end = tokio::time::timeout(Duration::from_secs(3), down.next())
        .await
        .expect("the downlink ends");
    assert!(
        matches!(end, Some(Err(_))),
        "the downlink ended silently: {end:?}"
    );
}

/// A message sent as one turn asks for input starts the next turn rather than
/// being declined.
async fn p5_run() -> (usize, usize) {
    let (h, _) = handler_with();
    let p = params("1.1", "a");
    let (up, rx) = mpsc::channel(8);
    up.send(send("ask", None)).await.unwrap();
    let mut down: Downlink = h
        .send_live_message(&p, ReceiverStream::new(rx).boxed())
        .await
        .unwrap();
    let task_id = match next(&mut down).await {
        StreamResponse::Task(t) => t.id,
        other => panic!("{other:?}"),
    };
    let mut declined = 0;
    let rounds = 50;
    for _ in 0..rounds {
        // wait for the turn's INPUT_REQUIRED, then answer at once
        loop {
            let e = next(&mut down).await;
            match &e {
                StreamResponse::StatusUpdate(u) if u.status.state == TaskState::InputRequired => {
                    break;
                }
                StreamResponse::Error(err) => {
                    declined += 1;
                    let _ = err;
                    break;
                }
                _ => {}
            }
        }
        up.send(send("ask", Some(&task_id))).await.unwrap();
    }
    (declined, rounds)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn p5_message_at_turn_end_is_not_declined() {
    let (declined, rounds) = p5_run().await;
    assert_eq!(declined, 0);
    assert_eq!(rounds, 50);
}

/// A session that fell behind the turn that just ended starts over from the
/// task, and the exchange carries on: the next message gets its turn, and no
/// error ends the stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_exchange_that_fell_behind_carries_on_from_the_task() {
    for round in 0..20 {
        let (h, _) = handler_with();
        let p = params("1.1", "a");
        let (up, rx) = mpsc::channel(8);
        up.send(send("overrun", None)).await.unwrap();
        let mut down: Downlink = h
            .send_live_message(&p, ReceiverStream::new(rx).boxed())
            .await
            .unwrap();
        let task = match next(&mut down).await {
            StreamResponse::Task(t) => t,
            other => panic!("{other:?}"),
        };
        // The turn publishes far more than the buffer holds while nobody reads.
        loop {
            if get(&h, &task.id).await.status.state == TaskState::InputRequired {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        up.send(send("ask", Some(&task.id)))
            .await
            .unwrap_or_else(|_| panic!("round {round}: the exchange ended"));
        // The exchange goes on to the next turn, with no error on the way.
        let reached = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match down.next().await {
                    Some(Ok(event)) => {
                        if text(&event).as_deref() == Some("need input") {
                            return;
                        }
                    }
                    Some(Err(error)) => panic!("round {round}: {error:?}"),
                    None => panic!("round {round}: the exchange ended"),
                }
            }
        })
        .await;
        assert!(reached.is_ok(), "round {round}: the next turn was not seen");
    }
}
