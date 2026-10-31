// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! A new task is created by the executor: its first event creates it, and a
//! direct reply leaves nothing saved (A2A §3.1.1, §3.1.2, §3.2.1).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use a2a::*;
use a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore, RequestHandler,
    ServiceParams,
};
use futures::{StreamExt, stream, stream::BoxStream};

/// What the executor does depends on the text of the message it is sent.
struct Agent {
    ids: Arc<Mutex<Vec<String>>>,
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
        generation: 0,
    })
}

impl AgentExecutor for Agent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        self.ids.lock().unwrap().push(ctx.task_id.clone());
        let (task, context) = (ctx.task_id.clone(), ctx.context_id.clone());
        let text = ctx
            .message
            .as_ref()
            .and_then(|m| m.text())
            .unwrap_or("")
            .to_string();
        Box::pin(async_stream::stream! {
            match text.as_str() {
                "reply" => {
                    yield Ok(StreamResponse::Message(Message::new(
                        Role::Agent,
                        vec![Part::text("hi")],
                    )));
                }
                "reply-then-more" => {
                    yield Ok(StreamResponse::Message(Message::new(
                        Role::Agent,
                        vec![Part::text("hi")],
                    )));
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    yield Ok(status(&task, &context, TaskState::Working, "too late"));
                }
                "slow-reply" => {
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    yield Ok(StreamResponse::Message(Message::new(
                        Role::Agent,
                        vec![Part::text("hi")],
                    )));
                }
                "panic" => {
                    if text == "panic" {
                        panic!("the executor panicked");
                    }
                }
                "reply-then-panic" => {
                    yield Ok(StreamResponse::Message(Message::new(
                        Role::Agent,
                        vec![Part::text("hi")],
                    )));
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    panic!("the executor panicked");
                }
                "artifact" => {
                    yield Ok(StreamResponse::ArtifactUpdate(
                        a2a::event::TaskArtifactUpdateEvent {
                            task_id: task.clone(),
                            context_id: context.clone(),
                            artifact: Artifact {
                                artifact_id: "a1".into(),
                                name: None,
                                description: None,
                                parts: vec![Part::text("x")],
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
                        },
                    ));
                }
                "silent" => {}
                "error" => {
                    yield Err(A2AError::internal("no"));
                }
                "task" => {
                    yield Ok(StreamResponse::Task(Task {
                        id: task.clone(),
                        context_id: context.clone(),
                        status: TaskStatus {
                            state: TaskState::Working,
                            message: None,
                            timestamp: None,
                        },
                        artifacts: None,
                        history: None,
                        metadata: None,
                        generation: 0,
                        timeline: None,
                    }));
                }
                _ => {
                    yield Ok(status(&task, &context, TaskState::Working, "working"));
                    yield Ok(status(&task, &context, TaskState::Completed, "done"));
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

fn handler() -> (Arc<DefaultRequestHandler>, Arc<Mutex<Vec<String>>>) {
    let ids = Arc::new(Mutex::new(Vec::new()));
    let handler = DefaultRequestHandler::new(Agent { ids: ids.clone() }, InMemoryTaskStore::new())
        .with_capabilities(AgentCapabilities {
            streaming: Some(true),
            ..AgentCapabilities::default()
        });
    (Arc::new(handler), ids)
}

fn params() -> ServiceParams {
    let mut params = ServiceParams::new();
    params.insert(SVC_PARAM_VERSION.to_string(), vec!["1.1".to_string()]);
    params
}

fn send(text: &str) -> SendMessageRequest {
    SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text(text)]),
        configuration: None,
        metadata: None,
        tenant: None,
        artifact_updates: Vec::new(),
    }
}

async fn tasks(handler: &DefaultRequestHandler) -> usize {
    handler
        .list_tasks(
            &params(),
            ListTasksRequest {
                context_id: None,
                status: None,
                page_size: None,
                page_token: None,
                history_length: None,
                timeline_length: None,
                status_timestamp_after: None,
                include_artifacts: None,
                tenant: None,
            },
        )
        .await
        .unwrap()
        .tasks
        .len()
}

#[tokio::test]
async fn a_direct_reply_leaves_no_task() {
    let (handler, ids) = handler();
    let response = handler
        .send_message(&params(), send("reply"))
        .await
        .unwrap();
    assert!(matches!(response, SendMessageResponse::Message(_)));
    assert_eq!(tasks(&handler).await, 0);
    let id = ids.lock().unwrap()[0].clone();
    let error = handler
        .get_task(
            &params(),
            GetTaskRequest {
                id,
                history_length: None,
                tenant: None,
                timeline_length: None,
                current_generation: None,
            },
        )
        .await
        .expect_err("no task was created");
    assert_eq!(error.code, error_code::TASK_NOT_FOUND);
}

#[tokio::test]
async fn a_streamed_direct_reply_is_one_message_and_ends_with_it() {
    let (handler, _) = handler();
    // The executor goes on working for a while after its reply.
    let started = tokio::time::Instant::now();
    let events: Vec<_> = handler
        .send_streaming_message(&params(), send("reply-then-more"))
        .await
        .unwrap()
        .collect()
        .await;
    assert!(
        started.elapsed() < Duration::from_millis(400),
        "the stream waited"
    );
    assert_eq!(events.len(), 1, "{events:?}");
    assert!(matches!(events[0], Ok(StreamResponse::Message(_))));
    // What it says afterwards belongs to no task.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(tasks(&handler).await, 0);
}

#[tokio::test]
async fn an_executor_that_says_nothing_still_has_the_task_the_request_prepared() {
    let (handler, _) = handler();
    let mut request = send("silent");
    request.configuration = Some(SendMessageConfiguration {
        return_immediately: Some(true),
        accepted_output_modes: None,
        task_push_notification_config: None,
        history_length: None,
        timeline_length: None,
        if_generation_match: None,
    });
    let response = handler.send_message(&params(), request).await.unwrap();
    let SendMessageResponse::Task(task) = response else {
        panic!("a task is returned");
    };
    assert_eq!(task.status.state, TaskState::Submitted);
    assert_eq!(tasks(&handler).await, 1);
}

#[tokio::test]
async fn an_executor_error_before_anything_leaves_no_task() {
    let (handler, _) = handler();
    handler
        .send_message(&params(), send("error"))
        .await
        .expect_err("the executor failed");
    assert_eq!(tasks(&handler).await, 0);
}

#[tokio::test]
async fn the_first_task_event_creates_the_task_with_the_clients_message() {
    let (handler, _) = handler();
    for text in ["task", "status"] {
        let mut events = handler
            .send_streaming_message(&params(), send(text))
            .await
            .unwrap();
        let Some(Ok(StreamResponse::Task(first))) = events.next().await else {
            panic!("{text}: the stream begins with the task");
        };
        let stored = handler
            .get_task(
                &params(),
                GetTaskRequest {
                    id: first.id.clone(),
                    history_length: None,
                    tenant: None,
                    timeline_length: None,
                    current_generation: None,
                },
            )
            .await
            .unwrap();
        assert!(
            stored.timeline.iter().flatten().any(|entry| matches!(
                &entry.content,
                TimelineContent::Message(m) if m.text() == Some(text)
            )),
            "{text}: the client's message is in the timeline"
        );
    }
}

#[tokio::test]
async fn a_live_exchange_creates_its_task_up_front() {
    use a2a::{StreamRequest, StreamRequestPayload};
    use tokio_stream::wrappers::ReceiverStream;

    let (handler, ids) = handler();
    let (up, rx) = tokio::sync::mpsc::channel(4);
    up.send(StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::SendMessage(send("reply")),
    })
    .await
    .unwrap();
    let mut down = handler
        .send_live_message(&params(), ReceiverStream::new(rx).boxed())
        .await
        .unwrap();
    // The exchange is bound to one task from its first request, so it opens
    // with that task even though the executor only replied.
    let Some(Ok(StreamResponse::Task(task))) = down.next().await else {
        panic!("the exchange opens with the task");
    };
    assert_eq!(task.id, ids.lock().unwrap()[0]);
    assert_eq!(tasks(&handler).await, 1);
}

// ---------------------------------------------------------------------------
// The typed form: an `Agent` returns a reply or a task
// ---------------------------------------------------------------------------

struct Typed;

#[tokio::test]
async fn typed_start_and_execute_share_the_execution_id() {
    struct Identified(tokio::sync::mpsc::UnboundedSender<(String, String)>);
    #[async_trait::async_trait]
    impl a2a_server::Agent for Identified {
        async fn start(&self, msg: a2a_server::NewMessage) -> Result<a2a_server::Start, A2AError> {
            self.0
                .send((msg.task_id.clone(), msg.execution_id.clone()))
                .unwrap();
            Ok(a2a_server::Start::Task(msg.new_task(TaskState::Working)))
        }
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            self.0
                .send((ctx.task_id.clone(), ctx.execution_id.clone()))
                .unwrap();
            Box::pin(stream::once(async move {
                Ok(status(
                    &ctx.task_id,
                    &ctx.context_id,
                    TaskState::Completed,
                    "done",
                ))
            }))
        }
    }
    let (sender, mut observed) = tokio::sync::mpsc::unbounded_channel();
    let handler = DefaultRequestHandler::from_agent(Identified(sender), InMemoryTaskStore::new());
    let response = handler
        .send_message(&params(), send("start"))
        .await
        .unwrap();
    let SendMessageResponse::Task(task) = response else {
        panic!("a task")
    };
    let start = observed.try_recv().unwrap();
    let execute = observed.try_recv().unwrap();
    assert!(!start.1.is_empty());
    assert_eq!(start, execute);
    assert_eq!(start.0, task.id);
}

#[async_trait::async_trait]
impl a2a_server::Agent for Typed {
    async fn start(&self, msg: a2a_server::NewMessage) -> Result<a2a_server::Start, A2AError> {
        if msg.message.text() == Some("reply") {
            return Ok(a2a_server::Start::Message(Message::new(
                Role::Agent,
                vec![Part::text("hi")],
            )));
        }
        Ok(a2a_server::Start::Task(msg.new_task(TaskState::Working)))
    }

    fn execute(
        &self,
        mut ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (task_id, context_id) = ctx.task_info();
        let first = ctx.message.take();
        let mut inputs = ctx.inputs;
        Box::pin(async_stream::stream! {
            // The task is there, whichever turn this is.
            assert!(ctx.stored_task.is_some());
            if let Some(first) = first {
                let text = first.text().unwrap_or("").to_string();
                yield Ok(status(&task_id, &context_id, TaskState::Working, &format!("got: {text}")));
            }
            while let Some(input) = inputs.recv().await {
                let text = input.message.text().unwrap_or("").to_string();
                if text == "finish" {
                    yield Ok(status(&task_id, &context_id, TaskState::Completed, "done"));
                    return;
                }
                yield Ok(status(&task_id, &context_id, TaskState::Working, &format!("got: {text}")));
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

fn typed_handler() -> Arc<DefaultRequestHandler> {
    Arc::new(
        DefaultRequestHandler::from_agent(Typed, InMemoryTaskStore::new()).with_capabilities(
            AgentCapabilities {
                streaming: Some(true),
                ..AgentCapabilities::default()
            },
        ),
    )
}

#[tokio::test]
async fn an_agent_that_replies_creates_no_task() {
    let handler = typed_handler();
    let response = handler
        .send_message(&params(), send("reply"))
        .await
        .unwrap();
    assert!(matches!(response, SendMessageResponse::Message(_)));
    assert_eq!(tasks(&handler).await, 0);
}

#[tokio::test]
async fn an_agent_that_starts_a_task_reads_the_clients_messages_from_then_on() {
    let handler = typed_handler();
    let mut stream = handler
        .send_streaming_message(&params(), send("hello"))
        .await
        .unwrap();
    let Some(Ok(StreamResponse::Task(task))) = stream.next().await else {
        panic!("the stream begins with the task");
    };
    assert_eq!(task.status.state, TaskState::Working);
    assert_eq!(tasks(&handler).await, 1);

    let mut follow_up = send("ping");
    follow_up.message.task_id = Some(task.id.clone());
    let mut second = handler
        .send_streaming_message(&params(), follow_up)
        .await
        .unwrap();
    let _ = second.next().await; // the task, holding the message
    // The first turn may still be answering "hello".
    loop {
        let StreamResponse::StatusUpdate(update) = second.next().await.unwrap().unwrap() else {
            panic!("a status update follows");
        };
        if update
            .status
            .message
            .and_then(|m| m.text().map(str::to_owned))
            == Some("got: ping".to_string())
        {
            break;
        }
    }
}

fn get_request(id: String) -> GetTaskRequest {
    GetTaskRequest {
        id,
        history_length: None,
        tenant: None,
        timeline_length: None,
        current_generation: None,
    }
}

#[tokio::test]
async fn a_panic_before_the_first_event_leaves_no_task() {
    let (handler, _) = handler();
    // The executor panics before saying anything: the call fails, and there is
    // no task to have failed.
    let result = handler.send_message(&params(), send("panic")).await;
    assert!(
        !matches!(result, Ok(SendMessageResponse::Task(_))),
        "{result:?}"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(tasks(&handler).await, 0);
}

#[tokio::test]
async fn a_panic_after_a_direct_reply_creates_no_task() {
    let (handler, _) = handler();
    let response = handler
        .send_message(&params(), send("reply-then-panic"))
        .await
        .unwrap();
    assert!(matches!(response, SendMessageResponse::Message(_)));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(tasks(&handler).await, 0);
}

fn with_push_config(mut request: SendMessageRequest) -> SendMessageRequest {
    request.configuration = Some(SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: Some(TaskPushNotificationConfig {
            url: "http://127.0.0.1:9/hook".into(),
            id: Some("hook".into()),
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
    request
}

fn push_handler() -> (Arc<DefaultRequestHandler>, Arc<Mutex<Vec<String>>>) {
    let ids = Arc::new(Mutex::new(Vec::new()));
    let handler = DefaultRequestHandler::new(Agent { ids: ids.clone() }, InMemoryTaskStore::new())
        .with_capabilities(AgentCapabilities {
            streaming: Some(true),
            push_notifications: Some(true),
            ..AgentCapabilities::default()
        })
        .with_push_notifications(
            a2a_server::InMemoryPushConfigStore::new(),
            a2a_server::HttpPushSender::new(Some(a2a_server::HttpPushSenderConfig {
                // This fixture uses a local callback and tests configuration lifetime.
                validate_urls: false,
                ..Default::default()
            })),
        );
    (Arc::new(handler), ids)
}

async fn push_configs(handler: &DefaultRequestHandler, task_id: &str) -> usize {
    handler
        .list_push_configs(
            &params(),
            ListTaskPushNotificationConfigsRequest {
                task_id: task_id.into(),
                page_size: None,
                page_token: None,
                tenant: None,
            },
        )
        .await
        .unwrap()
        .configs
        .len()
}

#[tokio::test]
async fn a_push_configuration_is_kept_only_for_a_task_that_exists() {
    let (handler, ids) = push_handler();

    // A direct reply, an early error: no task, so no configuration.
    for text in ["reply", "error"] {
        let _ = handler
            .send_message(&params(), with_push_config(send(text)))
            .await;
        let id = ids.lock().unwrap().last().unwrap().clone();
        assert_eq!(push_configs(&handler, &id).await, 0, "{text}");
    }

    // A task the executor creates keeps the one the request carried.
    let mut request = with_push_config(send("task"));
    request.configuration.as_mut().unwrap().return_immediately = Some(true);
    let response = handler.send_message(&params(), request).await.unwrap();
    let SendMessageResponse::Task(task) = response else {
        panic!("a task is returned");
    };
    assert_eq!(push_configs(&handler, &task.id).await, 1);
}

#[tokio::test]
async fn a_task_the_executor_has_not_created_yet_is_not_found() {
    let (handler, ids) = handler();
    let call = {
        let handler = handler.clone();
        tokio::spawn(async move { handler.send_message(&params(), send("slow-reply")).await })
    };
    // The executor is running (it recorded the id) and has yet to reply.
    tokio::time::timeout(Duration::from_secs(5), async {
        while ids.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the executor started");
    let id = ids.lock().unwrap()[0].clone();
    for result in [
        handler
            .get_task(&params(), get_request(id.clone()))
            .await
            .map(|_| ()),
        handler
            .cancel_task(
                &params(),
                CancelTaskRequest {
                    id: id.clone(),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .map(|_| ()),
        handler
            .subscribe_to_task(
                &params(),
                SubscribeToTaskRequest {
                    id: id.clone(),
                    tenant: None,
                    last_seen_generation: None,
                },
            )
            .await
            .map(|_| ()),
    ] {
        assert_eq!(result.unwrap_err().code, error_code::TASK_NOT_FOUND);
    }
    assert!(matches!(
        call.await.unwrap().unwrap(),
        SendMessageResponse::Message(_)
    ));
}

#[tokio::test]
async fn a_first_artifact_event_creates_the_task() {
    let (handler, _) = handler();
    let mut request = send("artifact");
    request.configuration = Some(SendMessageConfiguration {
        return_immediately: Some(true),
        accepted_output_modes: None,
        task_push_notification_config: None,
        history_length: None,
        timeline_length: None,
        if_generation_match: None,
    });
    let response = handler.send_message(&params(), request).await.unwrap();
    let SendMessageResponse::Task(task) = response else {
        panic!("a task is returned");
    };
    assert_eq!(task.artifacts.as_ref().map(Vec::len), Some(1));
    assert_eq!(tasks(&handler).await, 1);
}

struct WrongId;

#[async_trait::async_trait]
impl a2a_server::Agent for WrongId {
    async fn start(&self, msg: a2a_server::NewMessage) -> Result<a2a_server::Start, A2AError> {
        let mut task = msg.new_task(TaskState::Working);
        task.id = "other".into();
        Ok(a2a_server::Start::Task(task))
    }
}

#[tokio::test]
async fn a_task_with_other_ids_than_the_executions_is_refused() {
    let handler = DefaultRequestHandler::from_agent(WrongId, InMemoryTaskStore::new())
        .with_capabilities(a2a::AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        });
    handler
        .send_message(&params(), send("hello"))
        .await
        .expect_err("the agent created a task for another id");
    assert_eq!(tasks(&handler).await, 0);
}

struct OneShot;

#[async_trait::async_trait]
impl a2a_server::Agent for OneShot {
    async fn start(&self, msg: a2a_server::NewMessage) -> Result<a2a_server::Start, A2AError> {
        let text = msg.message.text().unwrap_or("").to_string();
        Ok(a2a_server::Start::Message(Message::new(
            Role::Agent,
            vec![Part::text(format!("echo: {text}"))],
        )))
    }
}

#[tokio::test]
async fn an_agent_that_only_starts_answers_every_message_and_keeps_no_tasks() {
    let handler = DefaultRequestHandler::from_agent(OneShot, InMemoryTaskStore::new())
        .with_capabilities(a2a::AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        });
    let response = handler.send_message(&params(), send("hi")).await.unwrap();
    let SendMessageResponse::Message(reply) = response else {
        panic!("a reply");
    };
    assert_eq!(reply.text(), Some("echo: hi"));
    assert_eq!(tasks(&handler).await, 0);
}
