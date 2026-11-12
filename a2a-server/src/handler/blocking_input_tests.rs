// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! A blocking response cannot complete on an interruption predating its input.

use super::test_support::*;
use crate::AgentExecutor;

struct Questions {
    ask_first: Arc<Notify>,
    read_second: Arc<Notify>,
    published: Arc<Notify>,
    repetitions: usize,
}

impl AgentExecutor for Questions {
    fn execute(
        &self,
        mut ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let ask_first = self.ask_first.clone();
        let read_second = self.read_second.clone();
        let published = self.published.clone();
        let repetitions = self.repetitions;
        Box::pin(async_stream::stream! {
            ask_first.notified().await;
            for _ in 0..repetitions {
                yield Ok(question(&ctx, TaskState::InputRequired, "first question"));
            }
            published.notify_one();
            read_second.notified().await;
            let input = ctx.inputs.recv().await.expect("the second caller's input");
            assert_eq!(input.message.text(), Some("second"));
            yield Ok(question(&ctx, TaskState::AuthRequired, "second question"));
            futures::future::pending::<()>().await;
        })
    }

    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

fn question(ctx: &ExecutorContext, state: TaskState, text: &str) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        status: TaskStatus {
            state,
            message: Some(Message::new(Role::Agent, vec![Part::text(text)])),
            timestamp: None,
        },
        generation: 0,
        metadata: None,
    })
}

fn request(text: &str) -> SendMessageRequest {
    let mut request = send_request();
    request.message = Some(Message::new(Role::User, vec![Part::text(text)]));
    request.message.as_mut().unwrap().task_id = Some("task".into());
    request
}

async fn wait_for_input(handler: &DefaultRequestHandler, text: &str) {
    timeout(Duration::from_secs(2), async {
        loop {
            let task = handler.tasks.load("task").await.unwrap();
            if task
                .history
                .iter()
                .flatten()
                .any(|message| message.text() == Some(text))
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("input was persisted");
}

async fn check_joiner(repetitions: usize, lag: bool) {
    let ask_first = Arc::new(Notify::new());
    let read_second = Arc::new(Notify::new());
    let published = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        Questions {
            ask_first: ask_first.clone(),
            read_second: read_second.clone(),
            published: published.clone(),
            repetitions,
        },
        InMemoryTaskStore::new(),
    );
    seed_task(&handler, "task", "context").await;
    let params = ServiceParams::new();
    let first = tokio::spawn({
        let handler = handler.clone();
        let params = params.clone();
        async move { handler.send_message(&params, request("first")).await }
    });
    wait_for_input(&handler, "first").await;

    // Hold the second response unpolled when exercising broadcast overflow.
    let second_request = request("second");
    let settings = super::protocol::ResponsePolicy::message(
        super::protocol::StartMode::Blocking,
        &params,
        &second_request,
    );
    let message = handler
        .authorize_message(
            &params,
            super::delivery::MessageInput::Raw(second_request),
            None,
        )
        .map_err(|r| r.into_error())
        .unwrap();
    let delivery = handler
        .tasks
        .append_input(handler.clone(), message, settings.mode())
        .await
        .map_err(|r| r.into_error())
        .unwrap();
    let (task, events) = super::response::message_response(delivery, settings);
    let response =
        super::response::Run::new(handler.clone(), params.clone(), task.clone(), settings).start(
            None,
            Some(events),
            0,
        );
    let mut second = Box::pin(super::response::unary_response(
        handler.tasks.clone(),
        task,
        response,
    ));

    if !lag {
        assert!(
            timeout(Duration::from_millis(20), &mut second)
                .await
                .is_err()
        );
    }
    ask_first.notify_one();
    timeout(Duration::from_secs(2), published.notified())
        .await
        .unwrap();
    let first = timeout(Duration::from_secs(2), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(first, SendMessageResponse::Task(task) if task.status.state == TaskState::InputRequired)
    );
    assert!(
        timeout(Duration::from_millis(20), &mut second)
            .await
            .is_err(),
        "the first caller's question cannot answer unread input"
    );

    read_second.notify_one();
    let second = timeout(Duration::from_secs(2), second)
        .await
        .unwrap()
        .unwrap();
    let SendMessageResponse::Task(task) = second else {
        panic!("task response")
    };
    assert_eq!(task.status.state, TaskState::AuthRequired);
    assert_eq!(
        task.status.message.as_ref().and_then(Message::text),
        Some("second question")
    );
    handler.shutdown().await;
}

#[tokio::test]
async fn a_blocking_joiner_waits_for_its_own_input_before_returning_a_question() {
    check_joiner(1, false).await;
}

#[tokio::test]
async fn a_lagging_blocking_joiner_does_not_resynchronize_to_an_older_question() {
    check_joiner(super::EXECUTION_BUFFER_CAPACITY + 2, true).await;
}

#[tokio::test]
async fn output_records_input_progress_before_persistence_can_suspend() {
    use super::events::ActiveExecution;
    use super::task_handle::TaskHandle;
    let task = task_with_history("task", "context", 0);
    let (active, mut inputs, _) =
        ActiveExecution::with_task(TaskHandle::new(task.id.clone(), Some(task)));
    active.inputs.received(1);
    active.emitted_output();
    active
        .inputs
        .try_send(crate::ClientInput::new(
            Message::new(Role::User, vec![Part::text("later")]),
            2,
        ))
        .unwrap();
    inputs.recv().await.unwrap();
    let mut receiver = active.subscribe();
    active
        .publish(
            Ok(&StreamResponse::Task(task_with_history(
                "task", "context", 0,
            ))),
            None,
        )
        .await;
    assert_eq!(
        receiver.recv().await.unwrap().input_generation,
        1,
        "reading during persistence cannot change an already-emitted output's input progress"
    );
}

/// A detached producer buffers A's question, then reads B before the runner
/// polls that question. B must wait for a question explicitly tagged for B.
#[tokio::test]
async fn detached_outputs_keep_the_producing_inputs_generation() {
    struct Detached {
        read: Arc<Notify>,
        release: Arc<Notify>,
    }
    impl AgentExecutor for Detached {
        fn execute(
            &self,
            mut ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let read = self.read.clone();
            let release = self.release.clone();
            let (sender, mut receiver) = ctx.output_channel(2);
            let ready = Arc::new(Notify::new());
            let producer_ready = ready.clone();
            tokio::spawn(async move {
                let first = question(&ctx, TaskState::InputRequired, "first question");
                // Both reads and buffering happen outside the polled stream.
                sender.send(first).await.unwrap();
                let input = ctx.inputs.recv().await.unwrap();
                read.notify_one();
                producer_ready.notify_one();
                release.notified().await;
                sender
                    .send_for(
                        &input,
                        question(&ctx, TaskState::AuthRequired, "second question"),
                    )
                    .await
                    .unwrap();
            });
            Box::pin(async_stream::stream! {
                ready.notified().await;
                while let Some(event) = receiver.next().await { yield event; }
            })
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    let read = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        Detached {
            read: read.clone(),
            release: release.clone(),
        },
        InMemoryTaskStore::new(),
    );
    seed_task(&handler, "task", "context").await;
    let params = ServiceParams::new();
    let first = tokio::spawn({
        let handler = handler.clone();
        let params = params.clone();
        async move { handler.send_message(&params, request("first")).await }
    });
    wait_for_input(&handler, "first").await;
    let mut second = tokio::spawn({
        let handler = handler.clone();
        let params = params.clone();
        async move { handler.send_message(&params, request("second")).await }
    });
    timeout(Duration::from_secs(2), read.notified())
        .await
        .unwrap();
    let first = timeout(Duration::from_secs(2), first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(first, SendMessageResponse::Task(task) if task.status.state == TaskState::InputRequired)
    );
    assert!(
        timeout(Duration::from_millis(20), &mut second)
            .await
            .is_err()
    );
    release.notify_one();
    let SendMessageResponse::Task(task) = timeout(Duration::from_secs(2), second)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
    else {
        panic!("Task expected")
    };
    assert_eq!(
        task.status.message.as_ref().and_then(Message::text),
        Some("second question")
    );
    handler.shutdown().await;
}

#[tokio::test]
async fn deadline_does_not_return_an_old_question_as_a_new_answer() {
    struct Silent;
    impl AgentExecutor for Silent {
        fn execute(
            &self,
            _ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::pending().boxed()
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    for state in [TaskState::InputRequired, TaskState::Working] {
        let handler = DefaultRequestHandler::new(Silent, InMemoryTaskStore::new())
            .with_blocking_response_timeout(Duration::from_millis(10));
        seed_task(&handler, "task", "context").await;
        let mut task = handler.tasks.load("task").await.unwrap();
        task.status.state = state.clone();
        task.status.message = Some(Message::new(Role::Agent, vec![Part::text("old question")]));
        handler.tasks.store().update(task).await.unwrap();
        let response = handler
            .send_message(&ServiceParams::new(), request("answer"))
            .await;
        if state == TaskState::InputRequired {
            let error = response.unwrap_err();
            assert!(error.message.contains("do not resend"));
            let details = error.details.unwrap();
            assert_eq!(details[0].value["metadata"]["inputCommitted"], "true");
            assert_eq!(details[0].value["metadata"]["taskId"], "task");
        } else {
            assert!(
                matches!(response.unwrap(), SendMessageResponse::Task(task) if task.status.state == TaskState::Working)
            );
        }
        assert_eq!(
            handler
                .tasks
                .load("task")
                .await
                .unwrap()
                .history
                .unwrap()
                .iter()
                .filter(|message| message.text() == Some("answer"))
                .count(),
            1
        );
        handler.shutdown().await;
    }
}

#[tokio::test]
async fn output_cannot_claim_an_input_the_executor_has_not_read() {
    struct InvalidTag;
    impl AgentExecutor for InvalidTag {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let (outputs, events) = ctx.output_channel(1);
            tokio::spawn(async move {
                let unread = crate::ClientInput::new(Message::new(Role::User, vec![]), i64::MAX);
                outputs
                    .send_for(
                        &unread,
                        question(&ctx, TaskState::InputRequired, "not an answer"),
                    )
                    .await
                    .unwrap();
            });
            Box::pin(events)
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    let handler = DefaultRequestHandler::new(InvalidTag, InMemoryTaskStore::new());
    seed_task(&handler, "task", "context").await;
    let error = timeout(
        Duration::from_secs(2),
        handler.send_message(&ServiceParams::new(), request("first")),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.code, error_code::INVALID_AGENT_RESPONSE);
    assert!(error.message.contains("not consumed"));
    handler.shutdown().await;
}

#[tokio::test]
async fn a_detached_context_cannot_keep_an_ended_turn_or_claim_its_successors_input() {
    struct DetachedContext {
        turns: Arc<std::sync::atomic::AtomicUsize>,
        read_gate: Arc<Notify>,
        stopped: Arc<Notify>,
        read_input: Arc<std::sync::atomic::AtomicBool>,
    }
    impl AgentExecutor for DetachedContext {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            if self.turns.fetch_add(1, Ordering::SeqCst) == 0 {
                let read_gate = self.read_gate.clone();
                let stopped = self.stopped.clone();
                let read_input = self.read_input.clone();
                let opening = question(&ctx, TaskState::InputRequired, "first question");
                tokio::spawn(async move {
                    // Force ownership of the whole context, not just its inbox.
                    let mut ctx = ctx;
                    read_gate.notified().await;
                    read_input.store(ctx.inputs.recv().await.is_some(), Ordering::SeqCst);
                    drop(ctx);
                    stopped.notify_one();
                });
                stream::once(async move { Ok(opening) }).boxed()
            } else {
                assert_eq!(ctx.message.as_ref().and_then(Message::text), Some("second"));
                stream::once(async move {
                    Ok(question(&ctx, TaskState::InputRequired, "second question"))
                })
                .boxed()
            }
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    let turns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::test_util::install_crypto_provider();
    let read_gate = Arc::new(Notify::new());
    let stopped = Arc::new(Notify::new());
    let read_input = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler = DefaultRequestHandler::new(
        DetachedContext {
            turns: turns.clone(),
            read_gate: read_gate.clone(),
            stopped: stopped.clone(),
            read_input: read_input.clone(),
        },
        InMemoryTaskStore::new(),
    )
    .with_push_notifications(
        crate::InMemoryPushConfigStore::new(),
        crate::HttpPushSender::new(None),
    );
    seed_task(&handler, "task", "context").await;
    let source = handler.tasks.task("task");
    handler.attach_pushes(&source);
    // Hold the old driver's Finished command outside the mailbox. Admission
    // must detect EOF itself while the old worker still owns the whole context.
    let mut finalization = Vec::new();
    for _ in 0..2 {
        finalization.push(
            source
                .publications()
                .reserve(&source, super::task_handle::TaskOperation::Finished)
                .await
                .unwrap()
                .unwrap(),
        );
    }
    timeout(
        Duration::from_secs(2),
        handler.send_message(&ServiceParams::new(), request("first")),
    )
    .await
    .unwrap()
    .unwrap();
    let reply = timeout(
        Duration::from_secs(2),
        handler.send_message(&ServiceParams::new(), request("second")),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(reply, SendMessageResponse::Task(task)
        if task.status.message.as_ref().and_then(Message::text) == Some("second question")));
    assert_eq!(
        turns.load(Ordering::SeqCst),
        2,
        "fresh execution began while the old receiver was still owned"
    );
    read_gate.notify_one();
    timeout(Duration::from_secs(2), stopped.notified())
        .await
        .unwrap();
    drop(finalization);
    assert_eq!(turns.load(Ordering::SeqCst), 2);
    assert!(!read_input.load(Ordering::SeqCst));
    handler.shutdown().await;
}

#[tokio::test]
async fn a_workers_returned_output_stream_defines_execution_lifetime() {
    struct OwnedWorker(TaskState);
    impl AgentExecutor for OwnedWorker {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let final_state = self.0.clone();
            let events = ctx.spawn_worker(2, move |mut ctx, outputs| async move {
                outputs
                    .send(question(&ctx, TaskState::InputRequired, "waiting"))
                    .await
                    .unwrap();
                let input = ctx.inputs.recv().await.unwrap();
                assert_eq!(input.message.text(), Some("second"));
                outputs
                    .send_for(&input, question(&ctx, final_state, "done"))
                    .await?;
                Ok(())
            });
            Box::pin(events)
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    // WORKING completes the blocking response only after channel EOF; a
    // terminal output also closes normally without a separate worker lease.
    for final_state in [TaskState::Working, TaskState::Completed] {
        let handler =
            DefaultRequestHandler::new(OwnedWorker(final_state.clone()), InMemoryTaskStore::new());
        seed_task(&handler, "task", "context").await;
        let first = timeout(
            Duration::from_secs(2),
            handler.send_message(&ServiceParams::new(), request("first")),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(first, SendMessageResponse::Task(task) if task.status.state == TaskState::InputRequired)
        );
        let second = timeout(
            Duration::from_secs(2),
            handler.send_message(&ServiceParams::new(), request("second")),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            matches!(second, SendMessageResponse::Task(task) if task.status.state == final_state)
        );
        handler.shutdown().await;
    }
}

#[tokio::test]
async fn worker_channel_preserves_buffered_output_before_a_terminal_event() {
    struct Buffered;
    impl AgentExecutor for Buffered {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let (outputs, events) = ctx.output_channel(2);
            tokio::spawn(async move {
                outputs.send(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                    task_id: ctx.task_id.clone(), context_id: ctx.context_id.clone(),
                    artifact: serde_json::from_value(serde_json::json!({"artifactId":"buffered", "parts":[{"text":"queued"}]})).unwrap(),
                    append: None, last_chunk: Some(true), metadata: None, generation: 0,
                })).await.unwrap();
                outputs
                    .send(question(&ctx, TaskState::Completed, "done"))
                    .await
                    .unwrap();
            });
            Box::pin(events)
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    let handler = DefaultRequestHandler::new(Buffered, InMemoryTaskStore::new());
    seed_task(&handler, "task", "context").await;
    let SendMessageResponse::Task(task) = timeout(
        Duration::from_secs(2),
        handler.send_message(&ServiceParams::new(), request("first")),
    )
    .await
    .unwrap()
    .unwrap() else {
        panic!("Task expected")
    };
    assert_eq!(task.status.state, TaskState::Completed);
    assert_eq!(task.artifacts.unwrap()[0].parts, vec![Part::text("queued")]);
    handler.shutdown().await;
}

#[tokio::test]
async fn a_panicking_monitored_worker_fails_the_task_and_notifies_the_blocking_caller() {
    struct Panics;
    impl AgentExecutor for Panics {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            async fn worker(
                ctx: ExecutorContext,
                outputs: crate::ExecutorOutputSender,
            ) -> Result<(), A2AError> {
                outputs
                    .send(question(&ctx, TaskState::Working, "working"))
                    .await?;
                panic!("worker panicked");
            }
            Box::pin(ctx.spawn_worker(2, worker))
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    let handler = DefaultRequestHandler::new(Panics, InMemoryTaskStore::new());
    seed_task(&handler, "task", "context").await;
    let error = timeout(
        Duration::from_secs(2),
        handler.send_message(&ServiceParams::new(), request("first")),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(error.message.contains("panicked"));
    assert_eq!(
        handler.tasks.load("task").await.unwrap().status.state,
        TaskState::Failed
    );
    handler.shutdown().await;
}
