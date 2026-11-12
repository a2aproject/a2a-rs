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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn interrupted_answers_commit_only_after_a_read_claim_or_in_a_fresh_execution() {
    struct Worker {
        count: Arc<std::sync::atomic::AtomicUsize>,
        release: Arc<Notify>,
        behavior: u8,
    }
    impl AgentExecutor for Worker {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            if self.count.fetch_add(1, Ordering::SeqCst) > 0 {
                assert_eq!(ctx.message.as_ref().and_then(Message::text), Some("second"));
                return stream::once(
                    async move { Ok(question(&ctx, TaskState::Completed, "fresh")) },
                )
                .boxed();
            }
            let release = self.release.clone();
            let behavior = self.behavior;
            Box::pin(ctx.spawn_worker(2, move |mut ctx, output| async move {
                output
                    .send(question(&ctx, TaskState::InputRequired, "question"))
                    .await?;
                release.notified().await;
                if behavior == 1 {
                    let input = ctx
                        .inputs
                        .recv()
                        .await
                        .expect("claimed input commits before delivery");
                    assert_eq!(input.message.text(), Some("second"));
                    output
                        .send_for(&input, question(&ctx, TaskState::Completed, "read"))
                        .await?;
                } else if behavior == 3 {
                    assert!(
                        ctx.inputs.try_recv().is_none(),
                        "try_recv must not claim an uncommitted answer"
                    );
                }
                Ok(())
            }))
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    for scenario in 0..4 {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        crate::test_util::install_crypto_provider();
        let handler = DefaultRequestHandler::new(
            Worker {
                count: count.clone(),
                release: release.clone(),
                behavior: scenario,
            },
            InMemoryTaskStore::new(),
        );
        seed_task(&handler, "task", "context").await;
        handler
            .send_message(&ServiceParams::new(), request("first"))
            .await
            .unwrap();
        let active = handler.tasks.task("task").active().unwrap();
        let capacity = active.inputs.sender.capacity();
        let before = handler.tasks.load("task").await.unwrap();
        let mut observers = active.task.sender().subscribe();
        let second = request("second");
        let id = second.message.as_ref().unwrap().message_id.clone();
        let response = tokio::spawn({
            let handler = handler.clone();
            async move { handler.send_message(&ServiceParams::new(), second).await }
        });
        timeout(Duration::from_secs(2), async {
            while active.inputs.sender.capacity() == capacity {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the answer was offered to the interrupted inbox");
        assert_eq!(
            handler.tasks.load("task").await.unwrap(),
            before,
            "an unclaimed offer must not persist anything"
        );
        assert!(
            observers.try_recv().is_err(),
            "an unclaimed answer must not be broadcast"
        );
        assert!(!response.is_finished());
        if scenario == 2 {
            active
                .push_failure
                .send_replace(Some(A2AError::internal("end-of-stream webhook failed")));
            active.mark_stream_ended();
        }
        release.notify_one();
        let reply = timeout(Duration::from_secs(2), response)
            .await
            .unwrap()
            .unwrap();
        let stored = handler.tasks.load("task").await.unwrap();
        let recorded = stored
            .history
            .iter()
            .flatten()
            .filter(|message| message.message_id == id)
            .count();
        if scenario == 2 {
            assert!(reply.is_err());
            assert_eq!(stored.status.state, TaskState::Failed);
            assert_eq!(recorded, 0);
        } else {
            assert!(
                matches!(reply.unwrap(), SendMessageResponse::Task(task) if task.status.state==TaskState::Completed)
            );
            assert_eq!(recorded, 1);
            assert_eq!(
                count.load(Ordering::SeqCst),
                if scenario == 1 { 1 } else { 2 }
            );
        }
        handler.shutdown().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deferred_answers_keep_fifo_generations_when_status_resumes_working() {
    struct Ordered {
        working: Arc<Notify>,
        read: Arc<Notify>,
        received: tokio::sync::mpsc::UnboundedSender<(String, i64)>,
    }
    impl AgentExecutor for Ordered {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let working = self.working.clone();
            let read = self.read.clone();
            let received = self.received.clone();
            Box::pin(ctx.spawn_worker(2, move |mut ctx, output| async move {
                output
                    .send(question(&ctx, TaskState::InputRequired, "question"))
                    .await?;
                working.notified().await;
                output
                    .send(question(&ctx, TaskState::Working, "resumed"))
                    .await?;
                read.notified().await;
                for _ in 0..2 {
                    let input = ctx.inputs.recv().await.unwrap();
                    received
                        .send((input.message.text().unwrap().into(), input.generation))
                        .unwrap();
                }
                output
                    .send(question(&ctx, TaskState::Completed, "done"))
                    .await?;
                Ok(())
            }))
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    let working = Arc::new(Notify::new());
    let read = Arc::new(Notify::new());
    let (received, mut inputs) = tokio::sync::mpsc::unbounded_channel();
    crate::test_util::install_crypto_provider();
    let handler = DefaultRequestHandler::new(
        Ordered {
            working: working.clone(),
            read: read.clone(),
            received,
        },
        InMemoryTaskStore::new(),
    )
    .with_capabilities(caps(Some(true), None));
    seed_task(&handler, "task", "context").await;
    let (uplink, incoming) = tokio::sync::mpsc::unbounded_channel();
    let live = |message| StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::SendMessage(message),
    };
    uplink.send(live(request("first"))).unwrap();
    let incoming = stream::unfold(incoming, |mut receiver| async move {
        receiver.recv().await.map(|request| (request, receiver))
    })
    .boxed();
    let params = ServiceParams::from([(a2a::SVC_PARAM_VERSION.into(), vec!["1.1".into()])]);
    let mut downlink = handler.send_live_message(&params, incoming).await.unwrap();
    timeout(Duration::from_secs(2), async {
        while let Some(event) = downlink.next().await {
            if matches!(event.unwrap(), StreamResponse::StatusUpdate(event) if event.status.state == TaskState::InputRequired) { break; }
        }
    }).await.unwrap();
    let active = handler.tasks.task("task").active().unwrap();
    let capacity = active.inputs.sender.capacity();
    let send = |text| {
        let handler = handler.clone();
        tokio::spawn(async move {
            handler
                .send_message(&ServiceParams::new(), request(text))
                .await
        })
    };
    uplink.send(live(request("second"))).unwrap();
    timeout(Duration::from_secs(2), async {
        while active.inputs.sender.capacity() == capacity {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    working.notify_one();
    timeout(Duration::from_secs(2), async {
        loop {
            let event = downlink.next().await.unwrap().unwrap();
            if matches!(event, StreamResponse::StatusUpdate(event) if event.status.state == TaskState::Working) { break; }
        }
    }).await.expect("a deferred uplink must not stall the live downlink");
    let third = send("third");
    timeout(Duration::from_secs(2), async {
        while active.inputs.sender.capacity() != capacity - 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let stored = handler.tasks.load("task").await.unwrap();
    assert!(
        !stored
            .history
            .iter()
            .flatten()
            .any(|message| matches!(message.text(), Some("second" | "third")))
    );
    read.notify_one();
    assert!(
        matches!(timeout(Duration::from_secs(2), third).await.unwrap().unwrap().unwrap(), SendMessageResponse::Task(task) if task.status.state==TaskState::Completed)
    );
    timeout(Duration::from_secs(2), async {
        loop {
            let event = downlink.next().await.unwrap().unwrap();
            if matches!(event, StreamResponse::StatusUpdate(event) if event.status.state == TaskState::Completed) { break; }
        }
    }).await.unwrap();
    let a = inputs.recv().await.unwrap();
    let b = inputs.recv().await.unwrap();
    assert_eq!((&*a.0, &*b.0), ("second", "third"));
    assert!(a.1 < b.1);
    handler.shutdown().await;
}

#[tokio::test]
async fn a_reader_canceled_during_persistence_cannot_silently_consume_the_answer() {
    #[derive(Clone)]
    struct SavingAnswer {
        inner: Arc<InMemoryTaskStore>,
        entered: Arc<Notify>,
        save: Arc<Notify>,
        block: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait]
    impl TaskStore for SavingAnswer {
        async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.inner.create(task).await
        }
        async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
            if task
                .history
                .iter()
                .flatten()
                .any(|m| m.text() == Some("second"))
                && self.block.swap(false, Ordering::SeqCst)
            {
                self.entered.notify_one();
                self.save.notified().await;
            }
            self.inner.update(task).await
        }
        async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
            self.inner.get(id).await
        }
        async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
            self.inner.list(req).await
        }
        async fn begin_cancel(&self, id: &str) -> Result<Task, A2AError> {
            self.inner.begin_cancel(id).await
        }
    }
    struct Worker {
        read: Arc<Notify>,
        abandon: Arc<Notify>,
        output: Arc<Notify>,
        finish: Arc<Notify>,
        drop_inputs: bool,
    }
    impl AgentExecutor for Worker {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let (read, abandon, sent, finish, drop_inputs) = (
                self.read.clone(),
                self.abandon.clone(),
                self.output.clone(),
                self.finish.clone(),
                self.drop_inputs,
            );
            Box::pin(ctx.spawn_worker(2, move |mut ctx, output| async move {
                output.send(question(&ctx, TaskState::InputRequired, "question")).await?;
                let old_question = question(&ctx, TaskState::InputRequired, "old worker question");
                read.notified().await;
                tokio::select! {
                    input = ctx.inputs.recv() => panic!("the store is gated, so no input may arrive: {input:?}"),
                    _ = abandon.notified() => {},
                }
                if drop_inputs { drop(ctx.inputs); }
                output.send(old_question).await?;
                sent.notify_one();
                finish.notified().await;
                Ok(())
            }))
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    for drop_inputs in [false, true] {
        crate::test_util::install_crypto_provider();
        let store = SavingAnswer {
            inner: Arc::default(),
            entered: Arc::default(),
            save: Arc::default(),
            block: Arc::new(true.into()),
        };
        let (read, abandon, output, finish) = (
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
            Arc::new(Notify::new()),
        );
        let handler = DefaultRequestHandler::new(
            Worker {
                read: read.clone(),
                abandon: abandon.clone(),
                output: output.clone(),
                finish: finish.clone(),
                drop_inputs,
            },
            store.clone(),
        );
        seed_task(&handler, "task", "context").await;
        handler
            .send_message(&ServiceParams::new(), request("first"))
            .await
            .unwrap();
        let active = handler.tasks.task("task").active().unwrap();
        let progress = active.inputs.received_generation();
        let reply = tokio::spawn({
            let handler = handler.clone();
            async move {
                handler
                    .send_message(&ServiceParams::new(), request("second"))
                    .await
            }
        });
        read.notify_one();
        timeout(Duration::from_secs(1), store.entered.notified())
            .await
            .unwrap();
        abandon.notify_one();
        timeout(Duration::from_secs(1), output.notified())
            .await
            .unwrap();
        store.save.notify_one();
        wait_for_input(&handler, "second").await;
        timeout(Duration::from_secs(1), async {
            loop {
                let task = handler.tasks.load("task").await.unwrap();
                if task.status.message.as_ref().and_then(Message::text)
                    == Some("old worker question")
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            active.inputs.received_generation(),
            progress,
            "committing an abandoned claim is not consumption"
        );
        assert!(
            !reply.is_finished(),
            "unrelated output cannot complete the unread answer"
        );
        finish.notify_one();
        let error = timeout(Duration::from_secs(1), reply)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.message.contains("before reading"), "{error:?}");
        let stored = handler.tasks.load("task").await.unwrap();
        assert_eq!(
            stored
                .history
                .iter()
                .flatten()
                .filter(|m| m.text() == Some("second"))
                .count(),
            1
        );
        let canceled = handler
            .cancel_task(
                &ServiceParams::new(),
                CancelTaskRequest {
                    id: "task".into(),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);
        handler.shutdown().await;
        assert_eq!(handler.tasks.background_tasks.count(), 0);
    }
}

#[tokio::test]
async fn successive_interrupts_preserve_the_order_of_uncommitted_requests() {
    struct Successive {
        first: Arc<Notify>,
        second: Arc<Notify>,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl AgentExecutor for Successive {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let first = ctx
                .message
                .as_ref()
                .and_then(Message::text)
                .unwrap()
                .to_owned();
            let wait = if first == "first" {
                self.first.clone()
            } else {
                self.second.clone()
            };
            Box::pin(async_stream::stream! {
                if first == "third" { yield Ok(question(&ctx, TaskState::Completed, "done")); }
                else {
                    yield Ok(question(&ctx, TaskState::InputRequired, &format!("question for {first}")));
                    wait.notified().await;
                }
            })
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    crate::test_util::install_crypto_provider();
    let (first, second) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler = DefaultRequestHandler::new(
        Successive {
            first: first.clone(),
            second: second.clone(),
            calls: calls.clone(),
        },
        InMemoryTaskStore::new(),
    );
    seed_task(&handler, "task", "context").await;
    handler
        .send_message(&ServiceParams::new(), request("first"))
        .await
        .unwrap();
    let active = handler.tasks.task("task").active().unwrap();
    let capacity = active.inputs.sender.capacity();
    let send = |text: &'static str| {
        let handler = handler.clone();
        tokio::spawn(async move {
            handler
                .send_message(&ServiceParams::new(), request(text))
                .await
        })
    };
    let second_response = send("second");
    timeout(Duration::from_secs(1), async {
        while active.inputs.sender.capacity() != capacity - 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let third_response = send("third");
    timeout(Duration::from_secs(1), async {
        while active.inputs.sender.capacity() != capacity - 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    first.notify_one();
    let second_task = timeout(Duration::from_secs(1), second_response)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(
        matches!(second_task, SendMessageResponse::Task(task) if task.status.message.as_ref().and_then(Message::text) == Some("question for second"))
    );
    assert!(!third_response.is_finished());
    assert!(
        !handler
            .tasks
            .load("task")
            .await
            .unwrap()
            .history
            .iter()
            .flatten()
            .any(|m| m.text() == Some("third"))
    );
    second.notify_one();
    assert!(
        matches!(timeout(Duration::from_secs(1), third_response).await.unwrap().unwrap().unwrap(), SendMessageResponse::Task(task) if task.status.state == TaskState::Completed)
    );
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let task = handler.tasks.load("task").await.unwrap();
    for text in ["first", "second", "third"] {
        assert_eq!(
            task.history
                .iter()
                .flatten()
                .filter(|m| m.role == Role::User && m.text() == Some(text))
                .count(),
            1
        );
    }
    handler.shutdown().await;
}

#[tokio::test]
async fn canceling_a_recv_claim_can_be_followed_by_a_real_read_in_the_same_execution() {
    use futures::FutureExt;
    struct Reader {
        withdraw: Arc<Notify>,
        withdrawn: Arc<Notify>,
        read: Arc<Notify>,
    }
    impl AgentExecutor for Reader {
        fn execute(
            &self,
            mut ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let (withdraw, withdrawn, read) = (
                self.withdraw.clone(),
                self.withdrawn.clone(),
                self.read.clone(),
            );
            Box::pin(async_stream::stream! {
                yield Ok(question(&ctx, TaskState::InputRequired, "question"));
                withdraw.notified().await;
                assert!(ctx.inputs.recv().now_or_never().is_none());
                withdrawn.notify_one();
                yield Ok(question(&ctx, TaskState::Working, "still preparing"));
                read.notified().await;
                assert_eq!(ctx.inputs.recv().await.unwrap().message.text(), Some("second"));
                yield Ok(question(&ctx, TaskState::Completed, "read"));
            })
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    crate::test_util::install_crypto_provider();
    let (withdraw, withdrawn, read) = (
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
        Arc::new(Notify::new()),
    );
    let handler = DefaultRequestHandler::new(
        Reader {
            withdraw: withdraw.clone(),
            withdrawn: withdrawn.clone(),
            read: read.clone(),
        },
        InMemoryTaskStore::new(),
    );
    seed_task(&handler, "task", "context").await;
    handler
        .send_message(&ServiceParams::new(), request("first"))
        .await
        .unwrap();
    let task = handler.tasks.task("task");
    let active = task.active().unwrap();
    let capacity = active.inputs.sender.capacity();
    let response = tokio::spawn({
        let handler = handler.clone();
        async move {
            handler
                .send_message(&ServiceParams::new(), request("second"))
                .await
        }
    });
    timeout(Duration::from_secs(1), async {
        while active.inputs.sender.capacity() == capacity {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    withdraw.notify_one();
    timeout(Duration::from_secs(1), withdrawn.notified())
        .await
        .unwrap();
    task.submit(super::task_handle::TaskOperation::Subscribe, |_| {
        Box::pin(async {})
    })
    .await
    .unwrap();
    assert!(
        !handler
            .tasks
            .load("task")
            .await
            .unwrap()
            .history
            .iter()
            .flatten()
            .any(|m| m.text() == Some("second"))
    );
    assert!(!response.is_finished());
    read.notify_one();
    assert!(
        matches!(timeout(Duration::from_secs(1), response).await.unwrap().unwrap().unwrap(), SendMessageResponse::Task(task) if task.status.state == TaskState::Completed)
    );
    handler.shutdown().await;
}

#[tokio::test]
async fn deferred_admission_is_bounded_and_disconnected_callers_return_all_reservations() {
    crate::test_util::install_crypto_provider();
    let handler = DefaultRequestHandler::new(NeverReads, InMemoryTaskStore::new());
    seed_task(&handler, "task", "context").await;
    handler
        .send_message(&ServiceParams::new(), request("first"))
        .await
        .unwrap();
    let active = handler.tasks.task("task").active().unwrap();
    let before = handler.tasks.load("task").await.unwrap();
    let capacity = active.inputs.sender.capacity();
    let mut waiting = Vec::new();
    for _ in 0..capacity {
        let handler = handler.clone();
        waiting.push(tokio::spawn(async move {
            handler
                .send_message(&ServiceParams::new(), request("uncommitted"))
                .await
        }));
    }
    timeout(Duration::from_secs(1), async {
        while active.inputs.sender.capacity() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let busy = timeout(
        Duration::from_secs(1),
        handler.send_message(&ServiceParams::new(), request("overflow")),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(busy.message.contains("busy"), "{busy:?}");
    assert_eq!(handler.tasks.load("task").await.unwrap(), before);
    for caller in waiting {
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
    }
    timeout(Duration::from_secs(1), async {
        while active.inputs.sender.capacity() != capacity {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("every disconnected reservation must be released promptly");
    assert_eq!(handler.tasks.load("task").await.unwrap(), before);
    handler
        .cancel_task(
            &ServiceParams::new(),
            CancelTaskRequest {
                id: "task".into(),
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    handler.shutdown().await;
    assert_eq!(handler.tasks.background_tasks.count(), 0);
}

#[tokio::test(start_paused = true)]
async fn deadline_recovery_cannot_add_a_second_wait_for_an_unresponsive_store() {
    #[derive(Clone)]
    struct SlowRead {
        inner: Arc<InMemoryTaskStore>,
        blocked: Arc<std::sync::atomic::AtomicBool>,
    }
    #[async_trait]
    impl TaskStore for SlowRead {
        async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.inner.create(task).await
        }
        async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.inner.update(task).await
        }
        async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
            if self.blocked.load(Ordering::SeqCst) {
                std::future::pending().await
            } else {
                self.inner.get(id).await
            }
        }
        async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
            self.inner.list(req).await
        }
        async fn begin_cancel(&self, id: &str) -> Result<Task, A2AError> {
            self.inner.begin_cancel(id).await
        }
    }
    struct Working(Arc<std::sync::atomic::AtomicBool>);
    impl AgentExecutor for Working {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let blocked = self.0.clone();
            Box::pin(async_stream::stream! {
                yield Ok(question(&ctx, TaskState::Working, "working"));
                blocked.store(true, Ordering::SeqCst);
                futures::future::pending::<()>().await;
            })
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    crate::test_util::install_crypto_provider();
    let blocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let store = SlowRead {
        inner: Arc::default(),
        blocked: blocked.clone(),
    };
    let handler = DefaultRequestHandler::new(Working(blocked.clone()), store.clone())
        .with_blocking_response_timeout(Duration::from_secs(5));
    seed_task(&handler, "task", "context").await;
    let start = tokio::time::Instant::now();
    let error = handler
        .send_message(&ServiceParams::new(), request("first"))
        .await
        .unwrap_err();
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    let detail = &error.details.as_ref().unwrap()[0].value;
    assert_eq!(detail["reason"], "RESPONSE_DEADLINE_EXCEEDED");
    assert_eq!(detail["metadata"]["inputCommitted"], "true");
    assert_eq!(detail["metadata"]["taskId"], "task");
    blocked.store(false, Ordering::SeqCst);
    assert_eq!(
        store.inner.get("task").await.unwrap().unwrap().status.state,
        TaskState::Working
    );
    handler
        .cancel_task(
            &ServiceParams::new(),
            CancelTaskRequest {
                id: "task".into(),
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    handler.shutdown().await;
}

struct NeverReads;
impl AgentExecutor for NeverReads {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::once(async move { Ok(question(&ctx, TaskState::InputRequired, "question")) })
            .chain(stream::pending())
            .boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

#[tokio::test(start_paused = true)]
async fn an_admission_wait_uses_the_configured_response_deadline() {
    crate::test_util::install_crypto_provider();
    let handler = DefaultRequestHandler::new(NeverReads, InMemoryTaskStore::new())
        .with_blocking_response_timeout(Duration::from_secs(5));
    seed_task(&handler, "task", "context").await;
    handler
        .send_message(&ServiceParams::new(), request("first"))
        .await
        .unwrap();
    let before = handler.tasks.load("task").await.unwrap();
    let start = tokio::time::Instant::now();
    let error = handler
        .send_message(&ServiceParams::new(), request("second"))
        .await
        .unwrap_err();
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    assert!(error.message.contains("deadline"));
    tokio::task::yield_now().await;
    assert_eq!(handler.tasks.load("task").await.unwrap(), before);
    handler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn cancellation_cannot_overtake_an_earlier_uncommitted_answer() {
    crate::test_util::install_crypto_provider();
    let handler = DefaultRequestHandler::new(NeverReads, InMemoryTaskStore::new())
        .with_input_admission_timeout(Duration::from_secs(10));
    seed_task(&handler, "task", "context").await;
    handler
        .send_message(&ServiceParams::new(), request("first"))
        .await
        .unwrap();
    let active = handler.tasks.task("task").active().unwrap();
    let capacity = active.inputs.sender.capacity();
    let answer = tokio::spawn({
        let handler = handler.clone();
        async move {
            handler
                .send_message(&ServiceParams::new(), request("answer"))
                .await
        }
    });
    while active.inputs.sender.capacity() == capacity {
        tokio::task::yield_now().await;
    }
    let start = tokio::time::Instant::now();
    let canceled = handler
        .cancel_task(
            &ServiceParams::new(),
            CancelTaskRequest {
                id: "task".into(),
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(start.elapsed(), Duration::from_secs(10));
    assert_eq!(canceled.status.state, TaskState::Canceled);
    let error = answer.await.unwrap().unwrap_err();
    assert!(
        error.message.contains("input was not committed"),
        "{error:?}"
    );
    assert!(
        !canceled
            .history
            .iter()
            .flatten()
            .any(|message| message.text() == Some("answer"))
    );
    handler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn artifact_only_updates_cannot_overtake_an_earlier_uncommitted_answer() {
    use futures::FutureExt;
    crate::test_util::install_crypto_provider();
    let handler = DefaultRequestHandler::new(NeverReads, InMemoryTaskStore::new())
        .with_input_admission_timeout(Duration::from_secs(10))
        .with_capabilities(AgentCapabilities {
            user_artifacts: Some(true),
            ..Default::default()
        });
    seed_task(&handler, "task", "context").await;
    let params = ServiceParams::new();
    handler
        .send_message(&params, request("first"))
        .await
        .unwrap();
    let task = handler.tasks.task("task");
    let active = task.active().unwrap();
    let capacity = active.inputs.sender.capacity();
    let answer = tokio::spawn({
        let handler = handler.clone();
        async move {
            handler
                .send_message(&ServiceParams::new(), request("answer"))
                .await
        }
    });
    while active.inputs.sender.capacity() == capacity {
        tokio::task::yield_now().await;
    }
    let update = serde_json::from_value(serde_json::json!({
        "artifactUpdates": [{"taskId":"task", "contextId":"context", "artifact":{"artifactId":"client", "parts":[{"text":"data"}]}, "lastChunk":true}],
        "configuration":{"returnImmediately":true}
    })).unwrap();
    let mut artifact = Box::pin(handler.send_message(&params, update));
    assert!(artifact.as_mut().now_or_never().is_none());
    let mut immediate = request("immediate after artifact");
    immediate.configuration =
        Some(serde_json::from_value(serde_json::json!({"returnImmediately":true})).unwrap());
    let mut immediate = Box::pin(handler.send_message(&params, immediate));
    assert!(immediate.as_mut().now_or_never().is_none());
    task.submit(super::task_handle::TaskOperation::Subscribe, |_| {
        Box::pin(async {})
    })
    .await
    .unwrap();
    assert!(
        handler
            .tasks
            .load("task")
            .await
            .unwrap()
            .artifacts
            .is_none(),
        "the artifact cannot persist before the earlier answer is resolved"
    );
    let start = tokio::time::Instant::now();
    let result = artifact.await.unwrap();
    assert_eq!(start.elapsed(), Duration::from_secs(10));
    let SendMessageResponse::Task(artifact_task) = result else {
        panic!("artifact update must return the task");
    };
    assert!(
        artifact_task
            .artifacts
            .as_ref()
            .unwrap()
            .iter()
            .any(|a| a.artifact_id == "client")
    );
    let SendMessageResponse::Task(immediate_task) = immediate.await.unwrap() else {
        panic!("immediate input must return the task");
    };
    assert_eq!(
        start.elapsed(),
        Duration::from_secs(10),
        "an ordered returnImmediately input must not start another read wait"
    );
    assert!(immediate_task.generation > artifact_task.generation);
    assert!(
        immediate_task
            .history
            .iter()
            .flatten()
            .any(|m| m.text() == Some("immediate after artifact"))
    );
    assert!(
        answer
            .await
            .unwrap()
            .unwrap_err()
            .message
            .contains("input was not committed")
    );
    handler.shutdown().await;
}

#[tokio::test]
async fn return_immediately_persists_an_interrupted_answer_without_waiting_for_a_reader() {
    crate::test_util::install_crypto_provider();
    let handler = DefaultRequestHandler::new(NeverReads, InMemoryTaskStore::new());
    seed_task(&handler, "task", "context").await;
    handler
        .send_message(&ServiceParams::new(), request("first"))
        .await
        .unwrap();
    let active = handler.tasks.task("task").active().unwrap();
    let read_generation = active.inputs.received_generation();
    let mut answer = request("immediate");
    let id = answer.message.as_ref().unwrap().message_id.clone();
    answer.configuration = Some(SendMessageConfiguration {
        return_immediately: Some(true),
        accepted_output_modes: None,
        task_push_notification_config: None,
        history_length: None,
        timeline_length: None,
        if_generation_match: None,
    });
    let result = timeout(
        Duration::from_secs(1),
        handler.send_message(&ServiceParams::new(), answer),
    )
    .await
    .expect("returnImmediately cannot wait for the execution to read input")
    .unwrap();
    assert!(
        matches!(result, SendMessageResponse::Task(task) if task.history.iter().flatten().any(|message| message.message_id == id))
    );
    assert_eq!(
        active.inputs.received_generation(),
        read_generation,
        "persistence must not advance consumption progress"
    );
    handler
        .cancel_task(
            &ServiceParams::new(),
            CancelTaskRequest {
                id: "task".into(),
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    handler.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn time_spent_admitting_input_reduces_the_remaining_response_wait() {
    struct Reader(Arc<Notify>);
    impl AgentExecutor for Reader {
        fn execute(
            &self,
            mut ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let read = self.0.clone();
            Box::pin(async_stream::stream! {
                yield Ok(question(&ctx, TaskState::InputRequired, "question"));
                read.notified().await;
                ctx.inputs.recv().await.unwrap();
                yield Ok(question(&ctx, TaskState::Working, "processing"));
                futures::future::pending::<()>().await;
            })
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    crate::test_util::install_crypto_provider();
    let read = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(Reader(read.clone()), InMemoryTaskStore::new())
        .with_blocking_response_timeout(Duration::from_secs(5));
    seed_task(&handler, "task", "context").await;
    handler
        .send_message(&ServiceParams::new(), request("first"))
        .await
        .unwrap();
    let start = tokio::time::Instant::now();
    let release = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(3)).await;
        read.notify_one();
    });
    let response = handler
        .send_message(&ServiceParams::new(), request("second"))
        .await
        .unwrap();
    assert_eq!(start.elapsed(), Duration::from_secs(5));
    assert!(
        matches!(response, SendMessageResponse::Task(task) if task.status.state == TaskState::Working)
    );
    release.await.unwrap();
    handler.shutdown().await;
}

#[tokio::test]
async fn an_unclaimed_answer_times_out_without_persistence_and_releases_its_request() {
    crate::test_util::install_crypto_provider();
    let handler = DefaultRequestHandler::new(NeverReads, InMemoryTaskStore::new());
    seed_task(&handler, "task", "context").await;
    handler
        .send_message(&ServiceParams::new(), request("first"))
        .await
        .unwrap();
    let before = handler.tasks.load("task").await.unwrap();
    let bounded = handler
        .clone()
        .with_input_admission_timeout(Duration::from_millis(10));
    let active = handler.tasks.task("task").active().unwrap();
    let capacity = active.inputs.sender.capacity();
    for _ in 0..20 {
        let error = timeout(
            Duration::from_secs(2),
            bounded.send_message(&ServiceParams::new(), request("answer")),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(error.message.contains("input was not committed"));
        assert_eq!(
            active.inputs.sender.capacity(),
            capacity,
            "expired reservations must release every inbox slot"
        );
    }
    let abandoned = tokio::spawn({
        let handler = handler.clone();
        async move {
            handler
                .send_message(&ServiceParams::new(), request("abandoned"))
                .await
        }
    });
    timeout(Duration::from_secs(1), async {
        while active.inputs.sender.capacity() == capacity {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    abandoned.abort();
    assert!(abandoned.await.unwrap_err().is_cancelled());
    timeout(Duration::from_secs(1), async {
        while active.inputs.sender.capacity() != capacity {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("caller disconnect must release the offer without waiting for its 30s deadline");
    assert_eq!(handler.tasks.load("task").await.unwrap(), before);
    handler.shutdown().await;
    assert_eq!(handler.tasks.background_tasks.count(), 0);
}

#[tokio::test]
async fn a_terminal_downlink_cannot_ack_an_answer_that_was_never_committed() {
    struct Completes(Arc<Notify>);
    impl AgentExecutor for Completes {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let finish = self.0.clone();
            Box::pin(async_stream::stream! {
                yield Ok(question(&ctx, TaskState::InputRequired, "question"));
                finish.notified().await;
                yield Ok(question(&ctx, TaskState::Completed, "done without reading"));
            })
        }
        fn cancel(
            &self,
            _: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            stream::empty().boxed()
        }
    }
    crate::test_util::install_crypto_provider();
    let finish = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(Completes(finish.clone()), InMemoryTaskStore::new())
        .with_capabilities(caps(Some(true), None));
    seed_task(&handler, "task", "context").await;
    handler
        .send_message(&ServiceParams::new(), request("first"))
        .await
        .unwrap();
    let active = handler.tasks.task("task").active().unwrap();
    let capacity = active.inputs.sender.capacity();
    let (acknowledge, acknowledged) = tokio::sync::oneshot::channel();
    let receipt = super::uplink::Receipt::new(acknowledge);
    let state = receipt.state();
    let (uplink, incoming) = tokio::sync::mpsc::channel(1);
    let params = ServiceParams::from([(a2a::SVC_PARAM_VERSION.into(), vec!["1.1".into()])]);
    let mut events = super::response::Run::new(
        handler.clone(),
        params.clone(),
        active.task.clone(),
        super::protocol::ResponsePolicy::new(super::protocol::StartMode::Bidi, true),
    )
    .with_uplink(Some(super::uplink::Uplink::Http(incoming)), None)
    .start(None, None, 0);
    uplink
        .send(super::uplink::Incoming {
            request: super::protocol::IncomingRequest::Message(Box::new(
                super::delivery::MessageInput::Authorized(super::delivery::AuthorizedMessage {
                    request: request("unclaimed"),
                    params,
                    user: None,
                    response_id: None,
                    receipt: Some(state.clone()),
                }),
            )),
            receipt: Some(receipt),
        })
        .await
        .unwrap();
    timeout(Duration::from_secs(2), async {
        while active.inputs.sender.capacity() == capacity {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    finish.notify_one();
    timeout(Duration::from_secs(2), async {
        loop {
            let event = events.next().await.unwrap().unwrap();
            if matches!(event, StreamResponse::StatusUpdate(event) if event.status.state==TaskState::Completed) { break; }
        }
    }).await.unwrap();
    assert!(
        timeout(Duration::from_secs(2), acknowledged)
            .await
            .unwrap()
            .is_err(),
        "an uncommitted input must not receive a Task acknowledgement"
    );
    assert!(state.committed_task().is_none());
    assert!(
        !handler
            .tasks
            .load("task")
            .await
            .unwrap()
            .history
            .iter()
            .flatten()
            .any(|message| message.text() == Some("unclaimed"))
    );
    handler.shutdown().await;
}
