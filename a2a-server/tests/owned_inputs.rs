// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Main-compatible executor signatures and owned inboxes in Tokio workers.

use a2a::*;
use a2a_server::{
    AgentExecutor, ClientInput, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore,
    LiveResponse, RequestHandler, ServiceParams,
};
use futures::{StreamExt, stream::BoxStream};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Notify, mpsc};
use tokio_stream::wrappers::ReceiverStream;

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

struct WorkerExecutor {
    end_turn: Arc<Notify>,
    read_old_inbox: Arc<Notify>,
    old_result: mpsc::UnboundedSender<Option<ClientInput>>,
    processed: mpsc::UnboundedSender<String>,
}

impl AgentExecutor for WorkerExecutor {
    // These are the exact method signatures from main, with no named lifetimes.
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let (task, context) = (ctx.task_id, ctx.context_id);
        let text = ctx
            .message
            .as_ref()
            .and_then(Message::text)
            .unwrap_or("")
            .to_string();
        let mut inputs = ctx.inputs;
        let processed = self.processed.clone();
        if text == "hold" {
            let read = self.read_old_inbox.clone();
            let result = self.old_result.clone();
            // Deliberately retains ownership beyond the output stream's EOF.
            tokio::spawn(async move {
                read.notified().await;
                result.send(inputs.recv().await).unwrap();
            });
            let end = self.end_turn.clone();
            return Box::pin(async_stream::stream! {
                yield Ok(status(&task, &context, TaskState::Working, "waiting"));
                end.notified().await;
            });
        }
        if text == "finish" {
            processed.send(text).unwrap();
            return Box::pin(futures::stream::once(async move {
                Ok(status(&task, &context, TaskState::Completed, "done"))
            }));
        }
        let (output, events) = mpsc::channel(8);
        // Input processing and event production live in an independent task.
        tokio::spawn(async move {
            if output
                .send(Ok(status(&task, &context, TaskState::Working, "ready")))
                .await
                .is_err()
            {
                return;
            }
            while let Some(input) = inputs.recv().await {
                let text = input.message.text().unwrap_or("").to_string();
                let finished = text == "finish";
                processed.send(text.clone()).unwrap();
                let event = status(
                    &task,
                    &context,
                    if finished {
                        TaskState::Completed
                    } else {
                        TaskState::Working
                    },
                    &text,
                );
                if output.send(Ok(event)).await.is_err() || finished {
                    break;
                }
            }
        });
        ReceiverStream::new(events).boxed()
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(futures::stream::once(async move {
            Ok(status(
                &ctx.task_id,
                &ctx.context_id,
                TaskState::Canceled,
                "canceled",
            ))
        }))
    }
}

fn send(text: &str, task: Option<&str>) -> StreamRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task.map(Into::into);
    StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::SendMessage(SendMessageRequest {
            message,
            tenant: None,
            configuration: None,
            metadata: None,
            artifact_updates: vec![],
        }),
    }
}
fn params() -> ServiceParams {
    [(SVC_PARAM_VERSION.into(), vec!["1.1".into()])].into()
}
async fn next(stream: &mut BoxStream<'static, Result<StreamResponse, A2AError>>) -> StreamResponse {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
async fn received<T>(receiver: &mut mpsc::UnboundedReceiver<T>) -> T {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn detached_receiver_is_revoked_without_waiting_for_its_worker_to_finish() {
    let end_turn = Arc::new(Notify::new());
    let read_old_inbox = Arc::new(Notify::new());
    let (old_result, mut old_results) = mpsc::unbounded_channel();
    let (processed, mut messages) = mpsc::unbounded_channel();
    let handler = DefaultRequestHandler::new(
        WorkerExecutor {
            end_turn: end_turn.clone(),
            read_old_inbox: read_old_inbox.clone(),
            old_result,
            processed,
        },
        InMemoryTaskStore::new(),
    )
    .with_capabilities(AgentCapabilities {
        streaming: Some(true),
        ..Default::default()
    });
    let p = params();
    let LiveResponse::Stream(mut stream) = handler
        .send_live_request(&p, send("hold", None))
        .await
        .unwrap()
    else {
        panic!("stream")
    };
    let StreamResponse::Task(task) = next(&mut stream).await else {
        panic!("task")
    };
    next(&mut stream).await; // Working.
    assert!(matches!(
        handler
            .send_live_request(&p, send("finish", Some(&task.id)))
            .await
            .unwrap(),
        LiveResponse::Ack(_)
    ));
    assert!(matches!(
        next(&mut stream).await,
        StreamResponse::MessageUpdate(_)
    ));
    end_turn.notify_one();
    // The next invocation must complete while the old worker still owns its receiver.
    assert_eq!(received(&mut messages).await, "finish");
    loop {
        let event = next(&mut stream).await;
        if match event {
            StreamResponse::StatusUpdate(update) => update.status.state == TaskState::Completed,
            StreamResponse::Task(task) => task.status.state == TaskState::Completed,
            _ => false,
        } {
            break;
        }
    }
    read_old_inbox.notify_one();
    assert!(
        received(&mut old_results).await.is_none(),
        "the old worker cannot steal handed-off input"
    );
    assert!(messages.try_recv().is_err(), "the input is processed once");
}

#[tokio::test]
async fn independent_worker_receives_inputs_and_sends_events_with_main_signatures() {
    let (old_result, _old_results) = mpsc::unbounded_channel();
    let (processed, mut messages) = mpsc::unbounded_channel();
    let handler = DefaultRequestHandler::new(
        WorkerExecutor {
            end_turn: Arc::new(Notify::new()),
            read_old_inbox: Arc::new(Notify::new()),
            old_result,
            processed,
        },
        InMemoryTaskStore::new(),
    )
    .with_capabilities(AgentCapabilities {
        streaming: Some(true),
        ..Default::default()
    });
    let p = params();
    let LiveResponse::Stream(mut stream) = handler
        .send_live_request(&p, send("start", None))
        .await
        .unwrap()
    else {
        panic!("stream")
    };
    let StreamResponse::Task(task) = next(&mut stream).await else {
        panic!("task")
    };
    next(&mut stream).await;
    for text in ["hello", "finish"] {
        assert!(matches!(
            handler
                .send_live_request(&p, send(text, Some(&task.id)))
                .await
                .unwrap(),
            LiveResponse::Ack(_)
        ));
        assert_eq!(received(&mut messages).await, text);
        assert!(matches!(
            next(&mut stream).await,
            StreamResponse::MessageUpdate(_)
        ));
        let StreamResponse::StatusUpdate(update) = next(&mut stream).await else {
            panic!("worker output")
        };
        assert_eq!(update.status.message.unwrap().text(), Some(text));
    }
    assert!(
        tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
            .is_none()
    );
}
