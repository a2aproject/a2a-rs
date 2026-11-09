// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Optional request deadlines preserve task ownership and A2A completion rules.

use a2a::*;
use a2a_server::*;
use futures::{StreamExt, stream, stream::BoxStream};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{Notify, mpsc},
    time::{advance, timeout},
};

const DEADLINE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
enum Opening {
    Silent,
    Working,
    Question,
    AuthQuestion,
    Reply,
    Error,
}

struct Controlled {
    opening: Opening,
    started: mpsc::UnboundedSender<TaskId>,
    finish: Arc<Notify>,
}

fn status(ctx: &ExecutorContext, state: TaskState) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        generation: 0,
        metadata: None,
        status: TaskStatus {
            state,
            timestamp: None,
            message: None,
        },
    })
}

impl AgentExecutor for Controlled {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        self.started.send(ctx.task_id.clone()).unwrap();
        let opening = self.opening;
        let finish = self.finish.clone();
        Box::pin(async_stream::stream! {
            match opening {
                Opening::Silent => {},
                Opening::Working => yield Ok(status(&ctx, TaskState::Working)),
                Opening::Question => yield Ok(status(&ctx, TaskState::InputRequired)),
                Opening::AuthQuestion => yield Ok(status(&ctx, TaskState::AuthRequired)),
                Opening::Reply => {
                    yield Ok(StreamResponse::Message(Message::new(Role::Agent, vec![Part::text("reply")])));
                    return;
                },
                Opening::Error => {
                    yield Err(A2AError::invalid_params("executor declined"));
                    return;
                },
            }
            finish.notified().await;
            yield Ok(status(&ctx, TaskState::Completed));
        })
    }

    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

fn handler(
    opening: Opening,
) -> (
    DefaultRequestHandler,
    mpsc::UnboundedReceiver<TaskId>,
    Arc<Notify>,
) {
    let (started, observed) = mpsc::unbounded_channel();
    let finish = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        Controlled {
            opening,
            started,
            finish: finish.clone(),
        },
        InMemoryTaskStore::new(),
    )
    .with_capabilities(AgentCapabilities {
        streaming: Some(true),
        ..Default::default()
    });
    (handler, observed, finish)
}

fn request() -> SendMessageRequest {
    SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text("work")]),
        configuration: None,
        metadata: None,
        tenant: None,
        artifact_updates: vec![],
    }
}

fn params() -> ServiceParams {
    ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])])
}

async fn get(handler: &DefaultRequestHandler, id: &str) -> Result<Task, A2AError> {
    handler
        .get_task(
            &params(),
            GetTaskRequest {
                id: id.into(),
                history_length: None,
                timeline_length: None,
                current_generation: None,
                tenant: None,
            },
        )
        .await
}

async fn wait_for_completion(handler: &DefaultRequestHandler, id: &str) -> Task {
    timeout(DEADLINE, async {
        loop {
            if let Ok(task) = get(handler, id).await
                && task.status.state == TaskState::Completed
            {
                return task;
            }
            // Polling also covers a lazy Task that has not been created yet.
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap()
}

async fn check_timeout(opening: Opening) {
    let (handler, mut started, finish) = handler(opening);
    let handler = handler.with_blocking_response_timeout(DEADLINE);
    let error = timeout(DEADLINE * 2, handler.send_message(&params(), request()))
        .await
        .expect("the configured deadline bounds the response")
        .unwrap_err();
    assert_eq!(error.code, error_code::INTERNAL_ERROR);
    assert_eq!(error.message, "blocking response timed out");
    let id = started.recv().await.unwrap();
    match opening {
        Opening::Silent => assert_eq!(
            get(&handler, &id).await.unwrap_err().code,
            error_code::TASK_NOT_FOUND
        ),
        Opening::Working => assert_eq!(
            get(&handler, &id).await.unwrap().status.state,
            TaskState::Working
        ),
        _ => unreachable!(),
    }
    // Deadline expiry drops the response, not the owned execution.
    finish.notify_one();
    assert_eq!(
        wait_for_completion(&handler, &id).await.status.state,
        TaskState::Completed
    );
    handler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_silent_lazy_executor_cannot_hold_a_configured_blocking_response_forever() {
    check_timeout(Opening::Silent).await;
}

#[tokio::test(start_paused = true)]
async fn a_working_task_times_out_without_being_returned_as_success_or_canceled() {
    check_timeout(Opening::Working).await;
}

#[tokio::test(start_paused = true)]
async fn blocking_waits_are_unbounded_by_default_and_can_finish_later() {
    let (handler, mut started, finish) = handler(Opening::Working);
    let mut response = tokio::spawn({
        let handler = handler.clone();
        async move { handler.send_message(&params(), request()).await }
    });
    started.recv().await.unwrap();
    assert!(
        timeout(Duration::from_secs(3600), &mut response)
            .await
            .is_err()
    );
    finish.notify_one();
    let response = timeout(DEADLINE, response).await.unwrap().unwrap().unwrap();
    assert!(
        matches!(response, SendMessageResponse::Task(task) if task.status.state == TaskState::Completed)
    );
    handler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn replies_interruptions_and_executor_errors_preserve_their_outcomes() {
    for opening in [
        Opening::Reply,
        Opening::Question,
        Opening::AuthQuestion,
        Opening::Error,
    ] {
        let (handler, _started, _finish) = handler(opening);
        let handler = handler.with_blocking_response_timeout(DEADLINE);
        let response = handler.send_message(&params(), request()).await;
        match opening {
            Opening::Reply => assert!(
                matches!(response, Ok(SendMessageResponse::Message(message)) if message.text() == Some("reply"))
            ),
            Opening::Question => assert!(
                matches!(response, Ok(SendMessageResponse::Task(task)) if task.status.state == TaskState::InputRequired)
            ),
            Opening::AuthQuestion => assert!(
                matches!(response, Ok(SendMessageResponse::Task(task)) if task.status.state == TaskState::AuthRequired)
            ),
            Opening::Error => assert!(
                matches!(response, Err(error) if error.code == error_code::INVALID_PARAMS && error.message == "executor declined")
            ),
            _ => unreachable!(),
        }
        handler.shutdown().await;
    }
}

#[tokio::test(start_paused = true)]
async fn streaming_and_live_responses_remain_open_across_input_required() {
    for live in [false, true] {
        for opening in [Opening::Question, Opening::AuthQuestion] {
            let (handler, mut started, finish) = handler(opening);
            let handler = handler.with_blocking_response_timeout(DEADLINE);
            let mut events = if live {
                handler
                    .send_live_message(
                        &params(),
                        stream::once(async {
                            StreamRequest {
                                tenant: None,
                                payload: StreamRequestPayload::SendMessage(request()),
                            }
                        })
                        .chain(stream::pending())
                        .boxed(),
                    )
                    .await
                    .unwrap()
            } else {
                handler
                    .send_streaming_message(&params(), request())
                    .await
                    .unwrap()
            };
            timeout(DEADLINE, async {
                loop {
                    match events.next().await.unwrap().unwrap() {
                        StreamResponse::Task(task)
                            if matches!(
                                task.status.state,
                                TaskState::InputRequired | TaskState::AuthRequired
                            ) =>
                        {
                            break;
                        }
                        StreamResponse::StatusUpdate(update)
                            if matches!(
                                update.status.state,
                                TaskState::InputRequired | TaskState::AuthRequired
                            ) =>
                        {
                            break;
                        }
                        _ => {}
                    }
                }
            })
            .await
            .unwrap();
            let id = started.recv().await.unwrap();
            advance(DEADLINE * 10).await;
            assert!(
                timeout(Duration::from_millis(1), events.next())
                    .await
                    .is_err(),
                "an interrupted task keeps its streaming response open"
            );
            finish.notify_one();
            timeout(DEADLINE, events.collect::<Vec<_>>()).await.unwrap();
            assert_eq!(
                wait_for_completion(&handler, &id).await.status.state,
                TaskState::Completed
            );
            handler.shutdown().await;
        }
    }
}

#[tokio::test(start_paused = true)]
async fn return_immediately_returns_a_task_without_waiting_for_silent_execution() {
    let (handler, mut started, finish) = handler(Opening::Silent);
    let handler = handler.with_blocking_response_timeout(DEADLINE);
    let mut request = request();
    request.configuration =
        Some(serde_json::from_value(serde_json::json!({"returnImmediately":true})).unwrap());
    let response = handler.send_message(&params(), request).await.unwrap();
    let SendMessageResponse::Task(task) = response else {
        panic!("task response")
    };
    let id = started.recv().await.unwrap();
    advance(DEADLINE * 10).await;
    assert_eq!(
        get(&handler, &id).await.unwrap().status.state,
        TaskState::Submitted
    );
    assert_eq!(id, task.id);
    finish.notify_one();
    wait_for_completion(&handler, &id).await;
    handler.shutdown().await;
}
