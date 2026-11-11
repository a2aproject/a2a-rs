// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Unread input failures belong to the sending response, including after lag.

use a2a::*;
use a2a_server::*;
use futures::{StreamExt, stream, stream::BoxStream};
use std::{sync::Arc, time::Duration};
use tokio::{sync::Notify, time::timeout};

const WAIT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy)]
enum End {
    Completed,
    Failed,
    Canceled,
    Handoff,
}
#[derive(Clone, Copy)]
enum Mode {
    Unary,
    Streaming,
    Live,
}

struct Gates {
    finish: Notify,
    read: Notify,
    finish_successor: Notify,
}
struct Gated {
    gates: Arc<Gates>,
    end: End,
    burst: usize,
}

fn status(ctx: &ExecutorContext, state: TaskState) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        generation: 0,
        metadata: None,
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
    })
}

impl AgentExecutor for Gated {
    fn execute(
        &self,
        mut ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let gates = self.gates.clone();
        let end = self.end;
        let burst = self.burst;
        Box::pin(async_stream::stream! {
            if ctx.message.as_ref().and_then(Message::text) == Some("start") {
                yield Ok(status(&ctx, TaskState::Working));
                gates.finish.notified().await;
                if matches!(end, End::Handoff) {return;}
                let input = ctx.inputs.recv().await.expect("the first queued input is read");
                assert_eq!(input.message.text(), Some("read"));
            } else {
                assert_eq!(ctx.message.as_ref().and_then(Message::text), Some("read"));
            }
            gates.read.notify_one();
            if matches!(end, End::Handoff) {gates.finish_successor.notified().await;}
            for _ in 0..burst {yield Ok(status(&ctx, TaskState::Working));}
            match end {
                End::Completed | End::Handoff => yield Ok(status(&ctx, TaskState::Completed)),
                End::Failed => yield Err(A2AError::internal("execution failed")),
                End::Canceled => futures::future::pending::<()>().await,
            }
        })
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

fn params() -> ServiceParams {
    ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])])
}
fn request(text: &str, id: Option<&str>) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = id.map(str::to_owned);
    SendMessageRequest {
        message: Some(message),
        configuration: None,
        metadata: None,
        tenant: None,
        artifact_updates: vec![],
    }
}
async fn next(events: &mut BoxStream<'static, Result<StreamResponse, A2AError>>) -> StreamResponse {
    timeout(WAIT, events.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
async fn live(
    handler: &DefaultRequestHandler,
    request: SendMessageRequest,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    handler
        .send_live_message(
            &params(),
            stream::once(async move {
                StreamRequest {
                    tenant: None,
                    payload: StreamRequestPayload::SendMessage(request),
                }
            })
            .chain(stream::pending())
            .boxed(),
        )
        .await
        .unwrap()
}
async fn input_arrived(
    events: &mut BoxStream<'static, Result<StreamResponse, A2AError>>,
    id: &str,
) {
    timeout(WAIT,async {loop {if matches!(events.next().await.unwrap().unwrap(), StreamResponse::MessageUpdate(update) if update.message.message_id == id) {break;}}}).await.unwrap();
}
async fn collect(
    events: BoxStream<'static, Result<StreamResponse, A2AError>>,
) -> Vec<Result<StreamResponse, A2AError>> {
    timeout(WAIT, events.collect()).await.unwrap()
}
fn shared_outcome(events: &[Result<StreamResponse, A2AError>], end: End) {
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, Ok(StreamResponse::Error(_)))),
        "another caller's private rejection must not appear"
    );
    let errors: Vec<_> = events
        .iter()
        .filter_map(|event| event.as_ref().err())
        .collect();
    if matches!(end, End::Failed) {
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].message, "execution failed");
    } else {
        assert!(errors.is_empty(), "shared task completion succeeds");
    }
}

async fn check(end: End, mode: Mode, burst: usize) {
    let gates = Arc::new(Gates {
        finish: Notify::new(),
        read: Notify::new(),
        finish_successor: Notify::new(),
    });
    let handler = DefaultRequestHandler::new(
        Gated {
            gates: gates.clone(),
            end,
            burst,
        },
        InMemoryTaskStore::new(),
    )
    .with_capabilities(AgentCapabilities {
        streaming: Some(true),
        ..Default::default()
    });
    let mut owner = handler
        .send_streaming_message(&params(), request("start", None))
        .await
        .unwrap();
    let StreamResponse::Task(task) = next(&mut owner).await else {
        panic!("opening")
    };
    assert!(
        matches!(next(&mut owner).await,StreamResponse::StatusUpdate(update) if update.status.state == TaskState::Working)
    );
    let mut observer = handler
        .subscribe_to_task(
            &params(),
            SubscribeToTaskRequest {
                id: task.id.clone(),
                tenant: None,
                last_seen_generation: None,
            },
        )
        .await
        .unwrap();
    next(&mut observer).await;
    let read_request = request("read", Some(&task.id));
    let read_id = read_request.message.as_ref().unwrap().message_id.clone();
    let peer = live(&handler, read_request).await;
    input_arrived(&mut observer, &read_id).await;
    let unread = request("unread", Some(&task.id));
    let unread_id = unread.message.as_ref().unwrap().message_id.clone();
    let (mut unary, events) = match mode {
        Mode::Unary => (
            Some(Box::pin({
                let handler = handler.clone();
                async move { handler.send_message(&params(), unread).await }
            })),
            None,
        ),
        Mode::Streaming => (
            None,
            Some(
                handler
                    .send_streaming_message(&params(), unread)
                    .await
                    .unwrap(),
            ),
        ),
        Mode::Live => (None, Some(live(&handler, unread).await)),
    };
    if let Some(call) = unary.as_mut() {
        tokio::select! {
            () = input_arrived(&mut observer, &unread_id) => {},
            _ = call => panic!("blocking response completed before execution"),
        }
    } else {
        input_arrived(&mut observer, &unread_id).await;
    }
    // Both inputs are accepted, but only the peer's input will be consumed.
    gates.finish.notify_one();
    timeout(WAIT, gates.read.notified()).await.unwrap();
    if matches!(end, End::Handoff) {
        gates.finish_successor.notify_one();
    }
    if matches!(end, End::Canceled) {
        handler
            .cancel_task(
                &params(),
                CancelTaskRequest {
                    id: task.id.clone(),
                    tenant: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();
    }
    // Drain a task subscriber first, so the sender can be deliberately late.
    shared_outcome(&collect(observer).await, end);
    match mode {
        Mode::Unary => {
            let error = timeout(WAIT, unary.unwrap()).await.unwrap().unwrap_err();
            assert_eq!(error.code, error_code::UNSUPPORTED_OPERATION);
            assert!(error.message.contains("before reading"));
        }
        Mode::Streaming => {
            let events = collect(events.unwrap()).await;
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Ok(StreamResponse::Error(_)))),
                "ordinary streaming uses a request error"
            );
            let errors: Vec<_> = events
                .iter()
                .filter_map(|event| event.as_ref().err())
                .collect();
            assert_eq!(
                errors.len(),
                1,
                "the unread sender receives one request error"
            );
            assert_eq!(errors[0].code, error_code::UNSUPPORTED_OPERATION);
            assert!(errors[0].message.contains("before reading"));
        }
        Mode::Live => {
            let events = collect(events.unwrap()).await;
            let private: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    Ok(StreamResponse::Error(error)) => Some(error),
                    _ => None,
                })
                .collect();
            assert_eq!(
                private.len(),
                1,
                "live input rejection remains a StreamError"
            );
            assert_eq!(private[0].code, "UNAVAILABLE");
            assert_eq!(private[0].reference_id.as_deref(), Some(unread_id.as_str()));
        }
    }
    shared_outcome(&collect(owner).await, end);
    shared_outcome(&collect(peer).await, end);
    let stored = handler
        .get_task(
            &params(),
            GetTaskRequest {
                id: task.id,
                tenant: None,
                history_length: None,
                timeline_length: None,
                current_generation: None,
            },
        )
        .await
        .unwrap();
    let expected = match end {
        End::Completed | End::Handoff => TaskState::Completed,
        End::Failed => TaskState::Failed,
        End::Canceled => TaskState::Canceled,
    };
    assert_eq!(stored.status.state, expected);
    assert!(
        stored
            .history
            .unwrap()
            .iter()
            .any(|message| message.message_id == unread_id),
        "the committed input is not rolled back"
    );
    handler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn unread_input_is_reported_only_to_its_sender_on_completion_failure_cancellation_or_handoff()
{
    for end in [End::Completed, End::Failed, End::Canceled, End::Handoff] {
        for mode in [Mode::Unary, Mode::Streaming, Mode::Live] {
            check(end, mode, 0).await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn unread_input_rejections_survive_a_lagging_sending_response() {
    for mode in [Mode::Unary, Mode::Streaming, Mode::Live] {
        check(End::Completed, mode, 200).await;
    }
}
