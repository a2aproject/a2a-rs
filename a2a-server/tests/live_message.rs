// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! `SendLiveMessage` (A2A 1.1 §3.1.12) against the default handler.

use std::sync::Arc;
use std::time::Duration;

use a2a::event::{StreamError, TaskMessageUpdateEvent};
use a2a::*;
use a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore, LiveResponse,
    RequestHandler, ServiceParams,
};
use futures::{StreamExt, stream, stream::BoxStream};
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::ReceiverStream;

/// Works until told "finish". With `takes_input` it reads the client's
/// messages while it works and says what it got; without it, it ignores them.
struct LiveAgent {
    takes_input: bool,
    release: Arc<Notify>,
}

fn status(ctx_task: &str, ctx_context: &str, state: TaskState, text: &str) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: ctx_task.into(),
        context_id: ctx_context.into(),
        status: TaskStatus {
            state,
            message: Some(Message::new(Role::Agent, vec![Part::text(text)])),
            timestamp: None,
        },
        metadata: None,
        generation: 0,
    })
}

impl AgentExecutor for LiveAgent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let release = self.release.clone();
        let takes_input = self.takes_input;
        let first = ctx
            .message
            .as_ref()
            .and_then(|m| m.text())
            .map(str::to_owned);
        Box::pin(async_stream::stream! {
            let (task, context) = (ctx.task_id.clone(), ctx.context_id.clone());
            match first.as_deref() {
                Some("ask") => {
                    yield Ok(status(&task, &context, TaskState::InputRequired, "need input"));
                    return;
                }
                Some("quick") => {
                    yield Ok(status(&task, &context, TaskState::Completed, "done"));
                    return;
                }
                Some("whoami") => {
                    // Says who the request was made by, then waits for input.
                    let who = ctx
                        .service_params
                        .get("x-user")
                        .and_then(|values| values.first())
                        .cloned()
                        .unwrap_or_default();
                    yield Ok(status(&task, &context, TaskState::InputRequired, &format!("user: {who}")));
                    return;
                }
                Some("ask-slow") => {
                    // Asks for input, then takes a moment to return.
                    yield Ok(status(&task, &context, TaskState::InputRequired, "need input"));
                    tokio::time::sleep(Duration::from_millis(30)).await;
                    return;
                }
                Some("lazy-done") => {
                    // Like "lazy", but the turn ends with the task completed.
                    let _inputs = &ctx.inputs;
                    yield Ok(status(&task, &context, TaskState::Working, "lazy"));
                    release.notified().await;
                    yield Ok(status(&task, &context, TaskState::Completed, "done"));
                    return;
                }
                Some("chat") => {
                    // Answers each input by asking for the next, and keeps the
                    // turn open to read it.
                    yield Ok(status(&task, &context, TaskState::Working, "working"));
                    let mut inputs = ctx.inputs;
                    while let Some(input) = inputs.recv().await {
                        let text = input.message.text().unwrap_or("").to_string();
                        yield Ok(status(&task, &context, TaskState::InputRequired, &format!("reply: {text}")));
                    }
                    return;
                }
                Some("lazy-err") => {
                    // Like "lazy", but the turn ends with an error.
                    let _inputs = &ctx.inputs;
                    yield Ok(status(&task, &context, TaskState::Working, "lazy"));
                    release.notified().await;
                    yield Err(A2AError::internal("agent failed"));
                    return;
                }
                Some("lazy") => {
                    // Takes input in principle, but never reads it, and ends its
                    // turn when released.
                    let _inputs = &ctx.inputs;
                    yield Ok(status(&task, &context, TaskState::Working, "lazy"));
                    release.notified().await;
                    yield Ok(status(&task, &context, TaskState::InputRequired, "done lazily"));
                    return;
                }
                _ => {}
            }
            yield Ok(status(&task, &context, TaskState::Working, "working"));
            if takes_input {
                let mut inputs = ctx.inputs;
                while let Some(input) = inputs.recv().await {
                    let text = input.message.text().unwrap_or("").to_string();
                    if text == "finish" {
                        yield Ok(status(&task, &context, TaskState::Completed, "done"));
                        return;
                    }
                    yield Ok(status(&task, &context, TaskState::Working, &format!("got: {text}")));
                }
            } else {
                release.notified().await;
                yield Ok(status(&task, &context, TaskState::Completed, "done"));
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

fn handler(takes_input: bool) -> (Arc<DefaultRequestHandler>, Arc<Notify>) {
    let release = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        LiveAgent {
            takes_input,
            release: release.clone(),
        },
        InMemoryTaskStore::new(),
    )
    .with_capabilities(AgentCapabilities {
        streaming: Some(true),
        user_artifacts: Some(true),
        ..AgentCapabilities::default()
    })
    .with_push_config_store(a2a_server::InMemoryPushConfigStore::new());
    (Arc::new(handler), release)
}

fn params(version: &str) -> ServiceParams {
    let mut params = ServiceParams::new();
    params.insert(SVC_PARAM_VERSION.to_string(), vec![version.to_string()]);
    params
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

fn subscribe(task_id: &str, last_seen: Option<i64>) -> StreamRequest {
    StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::Subscribe(SubscribeToTaskRequest {
            id: task_id.into(),
            tenant: None,
            last_seen_generation: last_seen,
        }),
    }
}

/// An uplink the test feeds by hand.
fn new_uplink() -> (
    mpsc::Sender<StreamRequest>,
    BoxStream<'static, StreamRequest>,
) {
    let (sender, receiver) = mpsc::channel(8);
    (sender, ReceiverStream::new(receiver).boxed())
}

type Downlink = BoxStream<'static, Result<StreamResponse, A2AError>>;

async fn next(downlink: &mut Downlink) -> StreamResponse {
    tokio::time::timeout(Duration::from_secs(5), downlink.next())
        .await
        .expect("an event arrives")
        .expect("the stream is open")
        .expect("no transport error")
}

async fn ended(downlink: &mut Downlink) {
    let end = tokio::time::timeout(Duration::from_secs(5), downlink.next())
        .await
        .expect("the stream ends");
    assert!(end.is_none(), "expected the stream to end, got {end:?}");
}

fn task_of(event: StreamResponse) -> Task {
    match event {
        StreamResponse::Task(task) => task,
        other => panic!("expected a task, got {other:?}"),
    }
}

fn state_text(event: &StreamResponse) -> (TaskState, String) {
    match event {
        StreamResponse::StatusUpdate(update) => (
            update.status.state.clone(),
            update
                .status
                .message
                .as_ref()
                .and_then(|m| m.text())
                .unwrap_or("")
                .to_string(),
        ),
        other => panic!("expected a status update, got {other:?}"),
    }
}

fn generation_of(event: &StreamResponse) -> i64 {
    match event {
        StreamResponse::Task(task) => task.generation,
        StreamResponse::StatusUpdate(update) => update.generation,
        StreamResponse::ArtifactUpdate(update) => update.generation,
        StreamResponse::MessageUpdate(update) => update.generation,
        other => panic!("no generation on {other:?}"),
    }
}

#[tokio::test]
async fn opening_with_a_message_opens_with_the_task_that_reflects_it() {
    let (handler, _) = handler(true);
    let (up, uplink) = new_uplink();
    up.send(send("quick", None)).await.unwrap();

    let mut down = handler
        .send_live_message(&params("1.1"), uplink)
        .await
        .unwrap();

    let opening = task_of(next(&mut down).await);
    assert_eq!(opening.generation, 1);
    let timeline = opening.timeline.as_ref().unwrap();
    assert!(matches!(
        &timeline[0].content,
        TimelineContent::Message(m) if m.text() == Some("quick")
    ));
    assert_eq!(
        state_text(&next(&mut down).await),
        (TaskState::Completed, "done".into())
    );
    ended(&mut down).await;
}

#[tokio::test]
async fn a_message_sent_while_the_agent_works_reaches_it_and_is_announced() {
    let (handler, _) = handler(true);
    let (up, uplink) = new_uplink();
    up.send(send("start", None)).await.unwrap();
    let mut down = handler
        .send_live_message(&params("1.1"), uplink)
        .await
        .unwrap();
    let opening = task_of(next(&mut down).await);
    let task_id = opening.id.clone();
    assert_eq!(state_text(&next(&mut down).await).1, "working");

    up.send(send("hello", Some(&task_id))).await.unwrap();

    let announced = next(&mut down).await;
    let StreamResponse::MessageUpdate(TaskMessageUpdateEvent {
        message,
        generation,
        ..
    }) = &announced
    else {
        panic!("expected the message to be announced, got {announced:?}");
    };
    assert_eq!(message.text(), Some("hello"));
    assert!(*generation > opening.generation);
    // The agent read it and answered.
    let answer = next(&mut down).await;
    assert_eq!(state_text(&answer).1, "got: hello");
    assert!(generation_of(&answer) > *generation);

    up.send(send("finish", Some(&task_id))).await.unwrap();
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));
    assert_eq!(
        state_text(&next(&mut down).await),
        (TaskState::Completed, "done".into())
    );
    ended(&mut down).await;

    let stored = handler
        .get_task(
            &ServiceParams::new(),
            GetTaskRequest {
                id: task_id,
                history_length: None,
                tenant: None,
                timeline_length: None,
                current_generation: None,
            },
        )
        .await
        .unwrap();
    let said: Vec<_> = stored
        .timeline
        .unwrap()
        .iter()
        .filter_map(|entry| match &entry.content {
            TimelineContent::Message(m) => m.text().map(str::to_owned),
            _ => None,
        })
        .collect();
    assert_eq!(said, ["start", "hello", "finish"]);
}

#[tokio::test]
async fn an_unread_live_input_is_answered_when_the_turn_ends() {
    let (handler, release) = handler(false);
    let (up, uplink) = new_uplink();
    up.send(send("start", None)).await.unwrap();
    let mut down = handler
        .send_live_message(&params("1.1"), uplink)
        .await
        .unwrap();
    let task_id = task_of(next(&mut down).await).id;
    assert_eq!(state_text(&next(&mut down).await).1, "working");

    let rejected = send("hello", Some(&task_id));
    let message_id = match &rejected.payload {
        StreamRequestPayload::SendMessage(r) => r.message.message_id.clone(),
        _ => unreachable!(),
    };
    up.send(rejected).await.unwrap();

    // The input was queued. Its error is published as the turn ends.
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));
    release.notify_one();
    match next(&mut down).await {
        StreamResponse::Error(StreamError {
            code,
            reference_id,
            task_id: id,
            ..
        }) => {
            assert_eq!(code, "UNAVAILABLE");
            assert_eq!(reference_id.as_deref(), Some(message_id.as_str()));
            assert_eq!(id, task_id);
        }
        other => panic!("expected a StreamError, got {other:?}"),
    }
    // The turn completes after the queued message is answered.
    assert_eq!(
        state_text(&next(&mut down).await),
        (TaskState::Completed, "done".into())
    );
    ended(&mut down).await;
}

#[tokio::test]
async fn a_second_subscribe_or_another_task_is_refused_without_ending_the_stream() {
    let (handler, _) = handler(true);
    let (up, uplink) = new_uplink();
    up.send(send("start", None)).await.unwrap();
    let mut down = handler
        .send_live_message(&params("1.1"), uplink)
        .await
        .unwrap();
    let task_id = task_of(next(&mut down).await).id;
    next(&mut down).await; // working

    up.send(subscribe(&task_id, None)).await.unwrap();
    match next(&mut down).await {
        StreamResponse::Error(error) => assert_eq!(error.code, "INVALID_ARGUMENT"),
        other => panic!("expected a StreamError, got {other:?}"),
    }
    up.send(send("elsewhere", Some("some-other-task")))
        .await
        .unwrap();
    match next(&mut down).await {
        StreamResponse::Error(error) => assert_eq!(error.code, "INVALID_ARGUMENT"),
        other => panic!("expected a StreamError, got {other:?}"),
    }

    // Still bound to its task and still working.
    up.send(send("finish", Some(&task_id))).await.unwrap();
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));
    assert_eq!(state_text(&next(&mut down).await).0, TaskState::Completed);
}

#[tokio::test]
async fn the_stream_stays_open_while_the_task_waits_for_input_and_the_next_message_starts_a_turn() {
    let (handler, _) = handler(true);
    let (up, uplink) = new_uplink();
    up.send(send("ask", None)).await.unwrap();
    let mut down = handler
        .send_live_message(&params("1.1"), uplink)
        .await
        .unwrap();
    let task_id = task_of(next(&mut down).await).id;
    assert_eq!(
        state_text(&next(&mut down).await),
        (TaskState::InputRequired, "need input".into())
    );

    // The turn is over, the exchange is not.
    assert!(
        tokio::time::timeout(Duration::from_millis(150), down.next())
            .await
            .is_err(),
        "the downlink must not close at INPUT_REQUIRED"
    );
    up.send(send("quick", Some(&task_id))).await.unwrap();

    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));
    assert_eq!(
        state_text(&next(&mut down).await),
        (TaskState::Completed, "done".into())
    );
    ended(&mut down).await;
}

#[tokio::test]
async fn a_1_0_client_is_not_sent_message_updates() {
    let (handler, _) = handler(true);
    let (up, uplink) = new_uplink();
    up.send(send("start", None)).await.unwrap();
    let mut down = handler
        .send_live_message(&params("1.0"), uplink)
        .await
        .unwrap();
    let task_id = task_of(next(&mut down).await).id;
    let working = generation_of(&next(&mut down).await);

    up.send(send("hello", Some(&task_id))).await.unwrap();

    // The agent's answer comes; the append is not announced, which shows only
    // as a gap in the generations (the message took the one in between).
    let answer = next(&mut down).await;
    assert_eq!(state_text(&answer).1, "got: hello");
    assert_eq!(generation_of(&answer), working + 2);
    up.send(send("finish", Some(&task_id))).await.unwrap();
    assert_eq!(state_text(&next(&mut down).await).0, TaskState::Completed);
}

/// HTTP calls are authorized before routing, once per caller, and a rejected
/// caller must receive neither an acknowledgement nor a persisted message.
#[tokio::test]
async fn routed_http_messages_authorize_once_with_their_own_credentials() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let calls = Arc::new(AtomicUsize::new(0));
    let checked = calls.clone();
    let handler = DefaultRequestHandler::new(
        LiveAgent {
            takes_input: true,
            release: Arc::new(Notify::new()),
        },
        InMemoryTaskStore::new(),
    )
    .with_capabilities(AgentCapabilities {
        streaming: Some(true),
        ..AgentCapabilities::default()
    })
    .with_authorizer(move |params: &ServiceParams, _: Option<&str>| {
        checked.fetch_add(1, Ordering::SeqCst);
        if params
            .get("x-user")
            .and_then(|values| values.first())
            .map(String::as_str)
            == Some("bob")
        {
            Err(A2AError::invalid_request("caller denied"))
        } else {
            Ok(())
        }
    });
    let alice = params("1.1");
    let mut bob = alice.clone();
    bob.insert("x-user".into(), vec!["bob".into()]);
    let LiveResponse::Stream(mut down) = handler
        .send_live_request(&alice, send("start", None))
        .await
        .unwrap()
    else {
        panic!("opening an exchange must return a stream");
    };
    let task_id = task_of(next(&mut down).await).id;
    next(&mut down).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        handler
            .send_live_request(&alice, send("accepted", Some(&task_id)))
            .await
            .unwrap(),
        LiveResponse::Ack(_)
    ));
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the routed request must not authorize again in the runner"
    );
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));
    next(&mut down).await;
    let result = handler
        .send_live_request(&bob, send("denied", Some(&task_id)))
        .await;
    assert!(matches!(result, Err(error) if error.message == "caller denied"));
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let LiveResponse::Ack(task) = handler
        .send_live_request(&alice, send("finish", Some(&task_id)))
        .await
        .unwrap()
    else {
        panic!("expected an acknowledgement");
    };
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert!(
        !task
            .history
            .as_ref()
            .unwrap()
            .iter()
            .any(|message| message.text() == Some("denied"))
    );
    while tokio::time::timeout(Duration::from_secs(5), down.next())
        .await
        .expect("the exchange ends")
        .is_some()
    {}
}

#[tokio::test]
async fn http_style_calls_open_the_exchange_once_and_later_calls_are_acknowledged() {
    let (handler, _) = handler(true);
    let p = params("1.1");

    let LiveResponse::Stream(mut down) = handler
        .send_live_request(&p, send("start", None))
        .await
        .unwrap()
    else {
        panic!("the first call is the downlink");
    };
    let task_id = task_of(next(&mut down).await).id;
    next(&mut down).await; // working

    let ack = handler
        .send_live_request(&p, send("hello", Some(&task_id)))
        .await
        .unwrap();
    let LiveResponse::Ack(task) = ack else {
        panic!("a later call only acknowledges");
    };
    let task = *task;
    assert_eq!(task.id, task_id);
    let messages = task
        .timeline
        .iter()
        .flatten()
        .filter(|entry| matches!(entry.content, TimelineContent::Message(_)))
        .count();
    assert_eq!(
        messages, 2,
        "the acknowledgement already reflects the message"
    );

    // Its events came on the downlink, once.
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));
    assert_eq!(state_text(&next(&mut down).await).1, "got: hello");

    let LiveResponse::Ack(_) = handler
        .send_live_request(&p, send("finish", Some(&task_id)))
        .await
        .unwrap()
    else {
        panic!("still an acknowledgement");
    };
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));
    assert_eq!(state_text(&next(&mut down).await).0, TaskState::Completed);
    ended(&mut down).await;
}

#[tokio::test]
async fn subscribing_first_attaches_to_a_running_task_without_sending_input() {
    let (handler, _) = handler(true);
    let p = params("1.1");
    let (up, uplink_a) = new_uplink();
    up.send(send("start", None)).await.unwrap();
    let mut owner = handler.send_live_message(&p, uplink_a).await.unwrap();
    let opening = task_of(next(&mut owner).await);
    next(&mut owner).await; // working

    let (watch_up, uplink_b) = new_uplink();
    watch_up.send(subscribe(&opening.id, None)).await.unwrap();
    let mut watcher = handler.send_live_message(&p, uplink_b).await.unwrap();
    let snapshot = task_of(next(&mut watcher).await);
    assert_eq!(snapshot.id, opening.id);

    up.send(send("finish", Some(&opening.id))).await.unwrap();
    // Both see the same message and the same end.
    assert!(matches!(
        next(&mut owner).await,
        StreamResponse::MessageUpdate(_)
    ));
    assert!(matches!(
        next(&mut watcher).await,
        StreamResponse::MessageUpdate(_)
    ));
    assert_eq!(state_text(&next(&mut owner).await).0, TaskState::Completed);
    assert_eq!(
        state_text(&next(&mut watcher).await).0,
        TaskState::Completed
    );
}

#[tokio::test]
async fn subscribing_with_the_current_generation_opens_without_a_snapshot() {
    let (handler, _) = handler(true);
    let p = params("1.1");
    let (up, uplink_a) = new_uplink();
    up.send(send("start", None)).await.unwrap();
    let mut owner = handler.send_live_message(&p, uplink_a).await.unwrap();
    let opening = task_of(next(&mut owner).await);
    let working = next(&mut owner).await;
    let current = generation_of(&working);

    let (watch_up, uplink_b) = new_uplink();
    watch_up
        .send(subscribe(&opening.id, Some(current)))
        .await
        .unwrap();
    let mut watcher = handler.send_live_message(&p, uplink_b).await.unwrap();

    up.send(send("hello", Some(&opening.id))).await.unwrap();
    // The first thing the watcher is told is what followed what it had seen.
    match next(&mut watcher).await {
        StreamResponse::MessageUpdate(update) => assert!(update.generation > current),
        other => panic!("expected the message, not a snapshot: {other:?}"),
    }
}

#[tokio::test]
async fn an_exchange_without_a_first_request_or_with_an_unknown_task_cannot_open() {
    let (handler, _) = handler(true);
    let p = params("1.1");
    let (_up, empty) = {
        let (sender, receiver) = mpsc::channel::<StreamRequest>(1);
        drop(sender);
        ((), ReceiverStream::new(receiver).boxed())
    };
    let error = handler.send_live_message(&p, empty).await.err().unwrap();
    assert_eq!(error.code, error_code::INVALID_REQUEST);

    let (up, uplink) = new_uplink();
    up.send(subscribe("no-such-task", None)).await.unwrap();
    let error = handler.send_live_message(&p, uplink).await.err().unwrap();
    assert_eq!(error.code, error_code::TASK_NOT_FOUND);

    let (up, uplink) = new_uplink();
    up.send(send("hi", Some("no-such-task"))).await.unwrap();
    let error = handler.send_live_message(&p, uplink).await.err().unwrap();
    assert_eq!(error.code, error_code::TASK_NOT_FOUND);
}

#[tokio::test]
async fn a_handler_that_does_not_override_the_live_methods_refuses_them() {
    struct Bare;
    #[async_trait::async_trait]
    impl RequestHandler for Bare {
        async fn send_message(
            &self,
            _: &ServiceParams,
            _: SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            unreachable!()
        }
        async fn send_streaming_message(
            &self,
            _: &ServiceParams,
            _: SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            unreachable!()
        }
        async fn get_task(&self, _: &ServiceParams, _: GetTaskRequest) -> Result<Task, A2AError> {
            unreachable!()
        }
        async fn list_tasks(
            &self,
            _: &ServiceParams,
            _: ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            unreachable!()
        }
        async fn cancel_task(
            &self,
            _: &ServiceParams,
            _: CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            unreachable!()
        }
        async fn subscribe_to_task(
            &self,
            _: &ServiceParams,
            _: SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            unreachable!()
        }
        async fn create_push_config(
            &self,
            _: &ServiceParams,
            _: TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            unreachable!()
        }
        async fn get_push_config(
            &self,
            _: &ServiceParams,
            _: GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            unreachable!()
        }
        async fn list_push_configs(
            &self,
            _: &ServiceParams,
            _: ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            unreachable!()
        }
        async fn delete_push_config(
            &self,
            _: &ServiceParams,
            _: DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            unreachable!()
        }
        async fn get_extended_agent_card(
            &self,
            _: &ServiceParams,
            _: GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            unreachable!()
        }
    }
    let error = Bare
        .send_live_request(&ServiceParams::new(), send("hi", None))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, error_code::UNSUPPORTED_OPERATION);
}

// ---------------------------------------------------------------------------
// Regressions found in the live-message validation
// ---------------------------------------------------------------------------

fn artifact_update(task_id: &str, artifact_id: &str) -> a2a::event::TaskArtifactUpdateEvent {
    a2a::event::TaskArtifactUpdateEvent {
        task_id: task_id.into(),
        context_id: String::new(),
        artifact: Artifact {
            artifact_id: artifact_id.into(),
            name: None,
            description: None,
            parts: vec![Part::text("from the client")],
            metadata: None,
            extensions: None,
            start_generation: None,
            end_generation: None,
            role: None,
        },
        append: None,
        last_chunk: Some(true),
        metadata: None,
        generation: 0,
    }
}

fn cancel(id: &str) -> CancelTaskRequest {
    CancelTaskRequest {
        id: id.into(),
        metadata: None,
        tenant: None,
    }
}

#[tokio::test]
async fn an_idle_exchange_is_told_when_its_task_is_canceled_and_ends() {
    let (handler, _) = handler(true);
    let p = params("1.1");
    let (up, uplink) = new_uplink();
    up.send(send("ask", None)).await.unwrap();
    let mut down = handler.send_live_message(&p, uplink).await.unwrap();
    let task_id = task_of(next(&mut down).await).id;
    assert_eq!(
        state_text(&next(&mut down).await).0,
        TaskState::InputRequired
    );

    // Nothing is running; somebody else cancels the task.
    handler.cancel_task(&p, cancel(&task_id)).await.unwrap();

    let told = task_of(next(&mut down).await);
    assert_eq!(told.status.state, TaskState::Canceled);
    ended(&mut down).await;
}

#[tokio::test]
async fn an_idle_exchange_follows_a_turn_another_client_starts() {
    let (handler, _) = handler(true);
    let p = params("1.1");
    let (up, uplink) = new_uplink();
    up.send(send("ask", None)).await.unwrap();
    let mut owner = handler.send_live_message(&p, uplink).await.unwrap();
    let task_id = task_of(next(&mut owner).await).id;
    next(&mut owner).await; // INPUT_REQUIRED

    // A second client attaches to the idle task...
    let (watch_up, watch_uplink) = new_uplink();
    watch_up.send(subscribe(&task_id, None)).await.unwrap();
    let mut watcher = handler.send_live_message(&p, watch_uplink).await.unwrap();
    assert_eq!(
        task_of(next(&mut watcher).await).status.state,
        TaskState::InputRequired
    );

    // ...and a third continues the task, without a live exchange of its own.
    let mut turn = handler
        .send_streaming_message(
            &p,
            match send("quick", Some(&task_id)).payload {
                StreamRequestPayload::SendMessage(request) => request,
                _ => unreachable!(),
            },
        )
        .await
        .unwrap();
    while turn.next().await.is_some() {}

    // Both exchanges heard about it, and both ended with the task.
    for down in [&mut owner, &mut watcher] {
        let mut ended_task = false;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), down.next())
                .await
                .expect("the exchange ends")
            {
                Some(Ok(StreamResponse::Task(task)))
                    if task.status.state == TaskState::Completed =>
                {
                    ended_task = true;
                }
                Some(Ok(StreamResponse::StatusUpdate(update)))
                    if update.status.state == TaskState::Completed =>
                {
                    ended_task = true;
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => panic!("{error:?}"),
                None => break,
            }
        }
        assert!(ended_task, "the exchange was told the task completed");
    }
}

#[tokio::test]
async fn input_the_agent_never_read_when_its_turn_ends_is_taken_up_by_the_next_turn() {
    let (handler, release) = handler(true);
    let p = params("1.1");
    let (up, uplink) = new_uplink();
    up.send(send("lazy", None)).await.unwrap();
    let mut down = handler.send_live_message(&p, uplink).await.unwrap();
    let task_id = task_of(next(&mut down).await).id;
    assert_eq!(state_text(&next(&mut down).await).1, "lazy");

    for text in ["x", "y"] {
        up.send(send(text, Some(&task_id))).await.unwrap();
        assert!(matches!(
            next(&mut down).await,
            StreamResponse::MessageUpdate(_)
        ));
    }
    release.notify_one();

    // The turn ends without reading either. They were accepted, so a turn
    // takes them up, "x" as its message and "y" from its input: nobody is
    // told they were not read.
    let handled = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let task = handler
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
            let said = |text: &str| {
                task.timeline.iter().flatten().any(|entry| {
                    matches!(
                        &entry.content,
                        TimelineContent::Status(status)
                            if status.message.as_ref().and_then(|m| m.text()) == Some(text)
                    )
                })
            };
            if said("got: y") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(handled.is_ok(), "the next turn read the queued message");
    while let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(300), down.next()).await {
        assert!(
            !matches!(item, Ok(StreamResponse::Error(_))),
            "a message that was taken up was turned away: {item:?}"
        );
    }
}

#[tokio::test]
async fn an_artifact_only_request_to_an_idle_task_announces_its_artifact_not_an_old_message() {
    let (handler, _) = handler(true);
    let p = params("1.1");
    let (up, uplink) = new_uplink();
    up.send(send("ask", None)).await.unwrap();
    let mut down = handler.send_live_message(&p, uplink).await.unwrap();
    let opening = task_of(next(&mut down).await);
    next(&mut down).await; // INPUT_REQUIRED

    let mut request = send("", None);
    if let StreamRequestPayload::SendMessage(r) = &mut request.payload {
        r.message.role = Role::Unspecified;
        r.message.message_id = String::new();
        r.message.parts.clear();
        r.artifact_updates = vec![artifact_update(&opening.id, "from-client")];
    }
    up.send(request).await.unwrap();

    let mut generations = Vec::new();
    let mut announced_artifact = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), down.next())
            .await
            .expect("an event arrives")
            .expect("open")
            .expect("no error")
        {
            StreamResponse::MessageUpdate(update) => {
                panic!("an artifact-only request has no message to announce: {update:?}")
            }
            StreamResponse::ArtifactUpdate(update) => {
                announced_artifact = true;
                generations.push(update.generation);
            }
            StreamResponse::StatusUpdate(update) => {
                generations.push(update.generation);
                // The turn the request started has begun.
                if announced_artifact {
                    break;
                }
            }
            _ => {}
        }
    }
    assert!(announced_artifact, "the artifact was announced");
    assert!(
        generations.windows(2).all(|pair| pair[1] > pair[0]),
        "no generation repeats: {generations:?}"
    );
}

#[tokio::test]
async fn a_call_routed_into_an_exchange_is_served_as_its_own_caller() {
    let (handler, _) = handler(true);
    let alice = {
        let mut p = params("1.1");
        p.insert("x-user".into(), vec!["alice".into()]);
        p
    };
    let bob = {
        let mut p = params("1.1");
        p.insert("x-user".into(), vec!["bob".into()]);
        p
    };
    let LiveResponse::Stream(mut down) = handler
        .send_live_request(&alice, send("ask", None))
        .await
        .unwrap()
    else {
        panic!("the first call is the downlink");
    };
    let task_id = task_of(next(&mut down).await).id;
    next(&mut down).await; // INPUT_REQUIRED

    let LiveResponse::Ack(_) = handler
        .send_live_request(&bob, send("whoami", Some(&task_id)))
        .await
        .unwrap()
    else {
        panic!("a later call is acknowledged");
    };

    let mut said = String::new();
    while !said.starts_with("user:") {
        if let StreamResponse::StatusUpdate(update) = next(&mut down).await {
            said = update
                .status
                .message
                .as_ref()
                .and_then(|m| m.text())
                .unwrap_or("")
                .to_string();
        }
    }
    assert_eq!(
        said, "user: bob",
        "the turn ran as the caller who started it"
    );
}

#[tokio::test]
async fn a_call_after_the_exchange_ended_opens_a_new_one_and_applies_the_message_once() {
    let (handler, _) = handler(true);
    let p = params("1.1");
    let LiveResponse::Stream(mut down) = handler
        .send_live_request(&p, send("ask", None))
        .await
        .unwrap()
    else {
        panic!("the first call is the downlink");
    };
    let task_id = task_of(next(&mut down).await).id;
    next(&mut down).await; // INPUT_REQUIRED
    drop(down); // the client goes away; the exchange ends

    // The call either reaches the exchange before it has noticed (and is
    // acknowledged) or finds none and opens a new one; the message is applied
    // once either way.
    let reply = handler
        .send_live_request(&p, send("quick", Some(&task_id)))
        .await
        .unwrap();
    match reply {
        LiveResponse::Stream(mut reopened) => {
            assert!(matches!(next(&mut reopened).await, StreamResponse::Task(_)));
        }
        LiveResponse::Ack(_) => {}
        _ => panic!("unexpected reply"),
    }
    let stored = handler
        .get_task(
            &ServiceParams::new(),
            GetTaskRequest {
                id: task_id,
                history_length: None,
                tenant: None,
                timeline_length: None,
                current_generation: None,
            },
        )
        .await
        .unwrap();
    let quick = stored
        .timeline
        .iter()
        .flatten()
        .filter(|entry| matches!(&entry.content, TimelineContent::Message(m) if m.text() == Some("quick")))
        .count();
    assert_eq!(quick, 1, "the message was applied exactly once");
}

#[tokio::test]
async fn a_push_configuration_sent_with_a_message_to_a_running_task_is_registered() {
    let (handler, _) = handler(true);
    let handler = handler.as_ref().clone().with_push_notifications(
        a2a_server::InMemoryPushConfigStore::new(),
        a2a_server::HttpPushSender::new(Some(a2a_server::HttpPushSenderConfig {
            // The callback is deliberately local; this test covers registration.
            validate_urls: false,
            ..Default::default()
        })),
    );
    let p = params("1.1");
    let (up, uplink) = new_uplink();
    up.send(send("start", None)).await.unwrap();
    let mut down = handler.send_live_message(&p, uplink).await.unwrap();
    let task_id = task_of(next(&mut down).await).id;
    next(&mut down).await; // working

    let mut request = send("hello", Some(&task_id));
    if let StreamRequestPayload::SendMessage(r) = &mut request.payload {
        r.configuration = Some(SendMessageConfiguration {
            accepted_output_modes: None,
            task_push_notification_config: Some(TaskPushNotificationConfig {
                url: "http://127.0.0.1:9/hook".into(),
                id: Some("hook-1".into()),
                task_id: String::new(),
                token: None,
                authentication: None,
                tenant: None,
                protocol_version: None,
            }),
            history_length: None,
            timeline_length: None,
            return_immediately: None,
            if_generation_match: None,
        });
    }
    up.send(request).await.unwrap();
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));

    let configs = handler
        .list_push_configs(
            &p,
            ListTaskPushNotificationConfigsRequest {
                task_id,
                page_size: None,
                page_token: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(configs.configs.len(), 1);
    assert_eq!(configs.configs[0].url, "http://127.0.0.1:9/hook");
}

#[tokio::test]
async fn subscribing_to_an_idle_task_at_its_current_generation_opens_without_a_snapshot() {
    let (handler, _) = handler(true);
    let p = params("1.1");
    let (up, uplink) = new_uplink();
    up.send(send("ask", None)).await.unwrap();
    let mut owner = handler.send_live_message(&p, uplink).await.unwrap();
    let task_id = task_of(next(&mut owner).await).id;
    let idle = next(&mut owner).await;
    let current = generation_of(&idle);

    let (watch_up, watch_uplink) = new_uplink();
    watch_up
        .send(subscribe(&task_id, Some(current)))
        .await
        .unwrap();
    let mut watcher = handler.send_live_message(&p, watch_uplink).await.unwrap();

    assert!(
        tokio::time::timeout(Duration::from_millis(300), watcher.next())
            .await
            .is_err(),
        "a client that holds the current generation is sent nothing"
    );
}

#[tokio::test]
async fn an_artifact_update_without_an_id_is_refused() {
    let (handler, _) = handler(true);
    let (up, uplink) = new_uplink();
    up.send(send("start", None)).await.unwrap();
    let mut down = handler
        .send_live_message(&params("1.1"), uplink)
        .await
        .unwrap();
    let task_id = task_of(next(&mut down).await).id;
    next(&mut down).await; // working

    let mut request = send("hello", Some(&task_id));
    if let StreamRequestPayload::SendMessage(r) = &mut request.payload {
        r.artifact_updates = vec![artifact_update(&task_id, "")];
    }
    up.send(request).await.unwrap();

    match next(&mut down).await {
        StreamResponse::Error(error) => assert_eq!(error.code, "INVALID_ARGUMENT"),
        other => panic!("expected a StreamError, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Regressions found in the second live-message validation
// ---------------------------------------------------------------------------

fn text_message_id(request: &StreamRequest) -> String {
    match &request.payload {
        StreamRequestPayload::SendMessage(r) => r.message.message_id.clone(),
        _ => unreachable!(),
    }
}

fn applied(task: &Task, text: &str) -> usize {
    task.timeline
        .iter()
        .flatten()
        .filter(
            |entry| matches!(&entry.content, TimelineContent::Message(m) if m.text() == Some(text)),
        )
        .count()
}

/// An HTTP call that races the end of its exchange is either carried out (and
/// then acknowledged with the message in the task) or told it was not reached
/// and so opens a new exchange that carries it out; it never hangs, and is
/// never acknowledged without having been carried out.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_call_racing_the_end_of_its_exchange_is_never_lost_or_left_waiting() {
    for round in 0..200 {
        let (handler, _) = handler(true);
        let p = params("1.1");
        let LiveResponse::Stream(mut down) = handler
            .send_live_request(&p, send("ask", None))
            .await
            .unwrap()
        else {
            panic!("the first call is the downlink");
        };
        let task_id = task_of(next(&mut down).await).id;
        next(&mut down).await; // INPUT_REQUIRED

        let marker = format!("race-{round}");
        let caller = {
            let handler = handler.clone();
            let p = p.clone();
            let task_id = task_id.clone();
            let marker = marker.clone();
            tokio::spawn(async move {
                handler
                    .send_live_request(&p, send(&marker, Some(&task_id)))
                    .await
            })
        };
        // The exchange ends as the call is on its way.
        if round % 2 == 0 {
            tokio::task::yield_now().await;
        }
        drop(down);

        let reply = tokio::time::timeout(Duration::from_secs(5), caller)
            .await
            .unwrap_or_else(|_| panic!("round {round}: the call hung"))
            .unwrap()
            .unwrap_or_else(|error| panic!("round {round}: {error:?}"));
        // Whichever way it went, the message was applied exactly once.
        match reply {
            LiveResponse::Ack(task) => {
                assert_eq!(
                    applied(&task, &marker),
                    1,
                    "round {round}: acked, not applied"
                );
            }
            LiveResponse::Stream(mut reopened) => {
                assert!(matches!(next(&mut reopened).await, StreamResponse::Task(_)));
            }
            _ => panic!("unexpected reply"),
        }
        let stored = handler
            .get_task(
                &ServiceParams::new(),
                GetTaskRequest {
                    id: task_id,
                    history_length: None,
                    tenant: None,
                    timeline_length: None,
                    current_generation: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(applied(&stored, &marker), 1, "round {round}");
    }
}

#[tokio::test]
async fn an_accepted_call_is_acknowledged_when_the_downlink_is_gone() {
    // The message is queued for the task; the HTTP caller gets its ack even
    // after the exchange's downlink is dropped.
    let (handler, release) = handler(false);
    let p = params("1.1");
    let LiveResponse::Stream(mut down) = handler
        .send_live_request(&p, send("start", None))
        .await
        .unwrap()
    else {
        panic!("the first call is the downlink");
    };
    let task_id = task_of(next(&mut down).await).id;
    next(&mut down).await; // working

    let caller = {
        let handler = handler.clone();
        let p = p.clone();
        let task_id = task_id.clone();
        tokio::spawn(async move {
            handler
                .send_live_request(&p, send("queued", Some(&task_id)))
                .await
        })
    };
    drop(down);

    let reply = tokio::time::timeout(Duration::from_secs(5), caller)
        .await
        .expect("the call is answered")
        .unwrap();
    // If the old exchange is still registered, the request gets an ACK.
    // Otherwise it opens a new downlink, whose opening task includes the input.
    let task = match reply.expect("the task accepts the message") {
        LiveResponse::Ack(task) => *task,
        LiveResponse::Stream(mut reopened) => task_of(next(&mut reopened).await),
        _ => panic!("unexpected live response"),
    };
    assert_eq!(applied(&task, "queued"), 1);
    release.notify_one();
}

#[tokio::test]
async fn input_unread_when_the_turn_ends_terminally_is_answered_before_the_end() {
    let (handler, release) = handler(true);
    let (up, uplink) = new_uplink();
    up.send(send("lazy-done", None)).await.unwrap();
    let mut down = handler
        .send_live_message(&params("1.1"), uplink)
        .await
        .unwrap();
    let task_id = task_of(next(&mut down).await).id;
    assert_eq!(state_text(&next(&mut down).await).1, "lazy");

    let request = send("unread", Some(&task_id));
    let message_id = text_message_id(&request);
    up.send(request).await.unwrap();
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));
    release.notify_one();

    // The turn completes without reading it: the error comes first, then the end.
    match next(&mut down).await {
        StreamResponse::Error(error) => {
            assert_eq!(error.code, "UNAVAILABLE");
            assert_eq!(error.reference_id.as_deref(), Some(message_id.as_str()));
        }
        other => panic!("expected the unread message to be answered, got {other:?}"),
    }
    assert_eq!(state_text(&next(&mut down).await).0, TaskState::Completed);
    ended(&mut down).await;
}

#[tokio::test]
async fn input_unread_when_the_task_is_canceled_is_answered_before_the_end() {
    let (handler, _) = handler(true);
    let p = params("1.1");
    let (up, uplink) = new_uplink();
    up.send(send("lazy", None)).await.unwrap();
    let mut down = handler.send_live_message(&p, uplink).await.unwrap();
    let task_id = task_of(next(&mut down).await).id;
    assert_eq!(state_text(&next(&mut down).await).1, "lazy");

    let request = send("unread", Some(&task_id));
    let message_id = text_message_id(&request);
    up.send(request).await.unwrap();
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));

    handler.cancel_task(&p, cancel(&task_id)).await.unwrap();

    let mut answered = false;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), down.next())
            .await
            .expect("the exchange ends")
        {
            Some(Ok(StreamResponse::Error(error))) => {
                assert_eq!(error.reference_id.as_deref(), Some(message_id.as_str()));
                answered = true;
            }
            Some(Ok(_)) => {}
            Some(Err(error)) => panic!("{error:?}"),
            None => break,
        }
    }
    assert!(answered, "the unread message was answered before the end");
}

/// Cancellation must drop the executor's stream even when it is waiting
/// on unrelated work, then reject unread messages before publishing CANCELED.
#[tokio::test]
async fn cancellation_drops_the_output_stream_before_running_cancel() {
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Dropped(Arc<AtomicBool>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }
    struct WaitingAgent(Arc<AtomicBool>);
    impl AgentExecutor for WaitingAgent {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let dropped = self.0.clone();
            Box::pin(async_stream::stream! {
                let _dropped = Dropped(dropped);
                let mut inputs = ctx.inputs;
                yield Ok(status(&ctx.task_id, &ctx.context_id, TaskState::Working, "waiting"));
                std::future::pending::<()>().await;
                let _ = inputs.recv().await;
            })
        }
        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            assert!(
                self.0.load(Ordering::SeqCst),
                "execution must drop before cancel runs"
            );
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
    let dropped = Arc::new(AtomicBool::new(false));
    let handler =
        DefaultRequestHandler::new(WaitingAgent(dropped.clone()), InMemoryTaskStore::new())
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..AgentCapabilities::default()
            });
    let p = params("1.1");
    let (up, uplink) = new_uplink();
    up.send(send("start", None)).await.unwrap();
    let mut down = handler.send_live_message(&p, uplink).await.unwrap();
    let task_id = task_of(next(&mut down).await).id;
    assert_eq!(state_text(&next(&mut down).await).0, TaskState::Working);
    let request = send("unread", Some(&task_id));
    let message_id = text_message_id(&request);
    up.send(request).await.unwrap();
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));
    tokio::time::timeout(
        Duration::from_secs(5),
        handler.cancel_task(&p, cancel(&task_id)),
    )
    .await
    .expect("cancel must not wait for the pending executor")
    .unwrap();
    assert!(dropped.load(Ordering::SeqCst));
    match next(&mut down).await {
        StreamResponse::Error(error) => {
            assert_eq!(error.reference_id.as_deref(), Some(message_id.as_str()))
        }
        other => panic!("unread input must be rejected before CANCELED: {other:?}"),
    }
    assert_eq!(state_text(&next(&mut down).await).0, TaskState::Canceled);
    ended(&mut down).await;
}

#[tokio::test]
async fn an_ordinary_streaming_subscriber_is_not_told_about_a_live_clients_unread_message() {
    let (handler, release) = handler(true);
    let p = params("1.1");
    let (up, uplink) = new_uplink();
    up.send(send("lazy-done", None)).await.unwrap();
    let mut down = handler.send_live_message(&p, uplink).await.unwrap();
    let task_id = task_of(next(&mut down).await).id;
    next(&mut down).await; // lazy

    // An ordinary 1.1 subscription to the same task.
    let mut plain = handler
        .subscribe_to_task(
            &p,
            SubscribeToTaskRequest {
                id: task_id.clone(),
                tenant: None,
                last_seen_generation: None,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        plain.next().await,
        Some(Ok(StreamResponse::Task(_)))
    ));

    up.send(send("unread", Some(&task_id))).await.unwrap();
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));
    release.notify_one();
    loop {
        // The turn's last status first, then what it never read.
        if matches!(next(&mut down).await, StreamResponse::Error(_)) {
            break;
        }
    }

    // The live exchange heard the error; the plain stream heard only events.
    let mut seen_error = false;
    while let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(500), plain.next()).await
    {
        if matches!(item, Ok(StreamResponse::Error(_))) {
            seen_error = true;
        }
    }
    assert!(!seen_error, "a live-only error reached an ordinary stream");
}

#[tokio::test]
async fn a_rejection_does_not_overtake_the_events_of_an_earlier_request() {
    let (handler, _) = handler(true);
    let (up, uplink) = new_uplink();
    up.send(send("start", None)).await.unwrap();
    let mut down = handler
        .send_live_message(&params("1.1"), uplink)
        .await
        .unwrap();
    let task_id = task_of(next(&mut down).await).id;
    next(&mut down).await; // working

    // An accepted message and, straight after, one that is refused.
    up.send(send("hello", Some(&task_id))).await.unwrap();
    up.send(send("elsewhere", Some("some-other-task")))
        .await
        .unwrap();

    let first = next(&mut down).await;
    assert!(
        matches!(first, StreamResponse::MessageUpdate(_)),
        "the accepted message is announced first: {first:?}"
    );
    // Then its answer and the rejection, in whichever order they happen, but
    // the rejection never before the announcement.
    let mut saw_error = false;
    for _ in 0..2 {
        if let StreamResponse::Error(error) = next(&mut down).await {
            assert_eq!(error.code, "INVALID_ARGUMENT");
            saw_error = true;
        }
    }
    assert!(saw_error);
}

#[tokio::test]
async fn closing_the_input_sender_keeps_what_was_queued_and_dropping_it_ends_the_queue() {
    use a2a_server::{ClientInput, ClientInputs};
    let message =
        |text: &str| ClientInput::new(Message::new(Role::User, vec![Part::text(text)]), 1);

    let (sender, mut inputs) = ClientInputs::channel(4);
    sender.try_send(message("a")).map_err(|_| ()).unwrap();
    sender.try_send(message("b")).map_err(|_| ()).unwrap();
    sender.close();
    assert!(
        sender.try_send(message("late")).is_err(),
        "closed queues refuse"
    );
    assert_eq!(inputs.recv().await.unwrap().message.text(), Some("a"));
    assert_eq!(inputs.recv().await.unwrap().message.text(), Some("b"));
    assert!(inputs.recv().await.is_none(), "then it ends");

    let (sender, mut inputs) = ClientInputs::channel(4);
    sender.try_send(message("only")).map_err(|_| ()).unwrap();
    drop(sender);
    assert_eq!(inputs.recv().await.unwrap().message.text(), Some("only"));
    let end = tokio::time::timeout(Duration::from_secs(2), inputs.recv()).await;
    assert!(
        matches!(end, Ok(None)),
        "dropping every sender ends the queue"
    );
}

// ---------------------------------------------------------------------------
// Regressions found in the third live-message validation
// ---------------------------------------------------------------------------

/// A store that can be told to fail the next `begin_cancel` or to panic in the
/// next `get`.
#[derive(Clone, Default)]
struct FlakyStore {
    inner: Arc<InMemoryTaskStore>,
    fail_cancel: Arc<std::sync::atomic::AtomicBool>,
    panic_get: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait::async_trait]
impl a2a_server::TaskStore for FlakyStore {
    async fn create(&self, task: Task) -> Result<a2a_server::task_store::TaskVersion, A2AError> {
        self.inner.create(task).await
    }
    async fn update(&self, task: Task) -> Result<a2a_server::task_store::TaskVersion, A2AError> {
        self.inner.update(task).await
    }
    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        if self
            .panic_get
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            panic!("the store panicked");
        }
        self.inner.get(task_id).await
    }
    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.inner.list(req).await
    }
    async fn begin_cancel(&self, task_id: &str) -> Result<Task, A2AError> {
        if self
            .fail_cancel
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(A2AError::internal("store is down"));
        }
        self.inner.begin_cancel(task_id).await
    }
}

fn flaky_handler() -> (Arc<DefaultRequestHandler>, FlakyStore) {
    let store = FlakyStore::default();
    let handler = DefaultRequestHandler::new(
        LiveAgent {
            takes_input: true,
            release: Arc::new(Notify::new()),
        },
        store.clone(),
    )
    .with_capabilities(AgentCapabilities {
        streaming: Some(true),
        user_artifacts: Some(true),
        ..AgentCapabilities::default()
    });
    (Arc::new(handler), store)
}

/// A cancel the store refuses changes nothing: the agent still takes input.
#[tokio::test]
async fn a_cancel_the_store_refuses_does_not_close_the_agents_input() {
    let (handler, store) = flaky_handler();
    let p = params("1.1");
    let (up, uplink) = new_uplink();
    up.send(send("work", None)).await.unwrap();
    let mut down = handler.send_live_message(&p, uplink).await.unwrap();
    let task_id = task_of(next(&mut down).await).id;
    assert_eq!(state_text(&next(&mut down).await).1, "working");

    store
        .fail_cancel
        .store(true, std::sync::atomic::Ordering::SeqCst);
    handler
        .cancel_task(&p, cancel(&task_id))
        .await
        .expect_err("the store refused the cancel");

    up.send(send("ping", Some(&task_id))).await.unwrap();
    loop {
        match next(&mut down).await {
            StreamResponse::Error(error) => panic!("the input was refused: {error:?}"),
            event => {
                if state_text_opt(&event).is_some_and(|(_, text)| text == "got: ping") {
                    break;
                }
            }
        }
    }
}

fn state_text_opt(event: &StreamResponse) -> Option<(TaskState, String)> {
    match event {
        StreamResponse::StatusUpdate(update) => Some((
            update.status.state.clone(),
            update
                .status
                .message
                .as_ref()
                .and_then(|m| m.text())
                .unwrap_or("")
                .to_owned(),
        )),
        _ => None,
    }
}

/// When the agent's stream ends with an error, what it never read is answered
/// first, then the error ends the exchange.
#[tokio::test]
async fn input_unread_when_the_agent_fails_is_answered_before_the_error() {
    let (handler, release) = handler(true);
    let (up, uplink) = new_uplink();
    up.send(send("lazy-err", None)).await.unwrap();
    let mut down = handler
        .send_live_message(&params("1.1"), uplink)
        .await
        .unwrap();
    let task_id = task_of(next(&mut down).await).id;
    assert_eq!(state_text(&next(&mut down).await).1, "lazy");

    let request = send("unread", Some(&task_id));
    let message_id = text_message_id(&request);
    up.send(request).await.unwrap();
    assert!(matches!(
        next(&mut down).await,
        StreamResponse::MessageUpdate(_)
    ));
    release.notify_one();

    match next(&mut down).await {
        StreamResponse::Error(error) => {
            assert_eq!(error.reference_id.as_deref(), Some(message_id.as_str()));
        }
        other => panic!("expected the unread message to be answered, got {other:?}"),
    }
}

/// A panic while a call is being handled never leaves a call acknowledged
/// without having been carried out, or waiting.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_panic_while_handling_a_call_never_loses_or_hangs_the_calls_queued_behind_it() {
    for round in 0..30 {
        let (handler, store) = flaky_handler();
        let p = params("1.1");
        let LiveResponse::Stream(mut down) = handler
            .send_live_request(&p, send("ask", None))
            .await
            .unwrap()
        else {
            panic!("the first call is the downlink");
        };
        let task_id = task_of(next(&mut down).await).id;
        next(&mut down).await; // INPUT_REQUIRED

        store
            .panic_get
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let mut callers = Vec::new();
        for i in 0..6 {
            let handler = handler.clone();
            let p = p.clone();
            let task_id = task_id.clone();
            let marker = format!("panic-{round}-{i}");
            callers.push((
                marker.clone(),
                tokio::spawn(async move {
                    handler
                        .send_live_request(&p, send(&marker, Some(&task_id)))
                        .await
                }),
            ));
        }
        for (marker, caller) in callers {
            let reply = tokio::time::timeout(Duration::from_secs(5), caller)
                .await
                .unwrap_or_else(|_| panic!("round {round}: {marker} hung"));
            // The call that hit the panic itself may fail; the others may not
            // be acknowledged unless they were carried out.
            if let Ok(Ok(LiveResponse::Ack(task))) = reply {
                assert_eq!(
                    applied(&task, &marker),
                    1,
                    "round {round}: {marker} acked, not applied"
                );
            }
        }
    }
}
