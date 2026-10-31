// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Input admission survives caller cancellation and rejects configuration before commit.
use a2a::*;
use a2a_server::task_store::TaskVersion;
use a2a_server::*;
use async_trait::async_trait;
use futures::{StreamExt, stream, stream::BoxStream};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Notify;
struct Agent {
    calls: Arc<AtomicUsize>,
    executed: Arc<Notify>,
}
impl AgentExecutor for Agent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.executed.notify_one();
        stream::iter([Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: ctx.task_id,
            context_id: ctx.context_id,
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            generation: 0,
            metadata: None,
        }))])
        .boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}
struct DelayedCommit {
    inner: InMemoryTaskStore,
    committed: Arc<Notify>,
    release: Arc<Notify>,
    delay: AtomicBool,
}
impl DelayedCommit {
    async fn after_commit(&self) {
        if self.delay.swap(false, Ordering::SeqCst) {
            self.committed.notify_one();
            self.release.notified().await;
        }
    }
}
#[async_trait]
impl TaskStore for DelayedCommit {
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        let version = self.inner.create(task).await?;
        self.after_commit().await;
        Ok(version)
    }
    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        let version = self.inner.update(task).await?;
        self.after_commit().await;
        Ok(version)
    }
    async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
        self.inner.get(id).await
    }
    async fn list(&self, request: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.inner.list(request).await
    }
    async fn begin_cancel(&self, id: &str) -> Result<Task, A2AError> {
        self.inner.begin_cancel(id).await
    }
}
fn task() -> Task {
    Task {
        id: "task".into(),
        context_id: "context".into(),
        generation: 1,
        status: TaskStatus {
            state: TaskState::Working,
            message: None,
            timestamp: None,
        },
        artifacts: None,
        history: None,
        timeline: None,
        metadata: None,
    }
}
fn request(existing: bool) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text("accepted input")]);
    message.task_id = existing.then(|| "task".into());
    SendMessageRequest {
        message,
        tenant: None,
        metadata: None,
        configuration: None,
        artifact_updates: vec![],
    }
}
fn config(url: &str) -> TaskPushNotificationConfig {
    TaskPushNotificationConfig {
        task_id: "task".into(),
        url: url.into(),
        id: None,
        tenant: None,
        protocol_version: None,
        token: None,
        authentication: None,
    }
}
fn with_config(mut request: SendMessageRequest, url: &str) -> SendMessageRequest {
    request.configuration = Some(SendMessageConfiguration {
        accepted_output_modes: None,
        task_push_notification_config: Some(config(url)),
        history_length: None,
        timeline_length: None,
        return_immediately: None,
        if_generation_match: None,
    });
    request
}
fn get() -> GetTaskRequest {
    GetTaskRequest {
        id: "task".into(),
        tenant: None,
        history_length: None,
        timeline_length: None,
        current_generation: None,
    }
}
#[derive(Clone, Copy)]
enum Mode {
    Unary,
    Streaming,
    HttpLive,
    NativeLive,
}
async fn send(
    handler: &DefaultRequestHandler,
    input: SendMessageRequest,
    mode: Mode,
) -> Result<(), A2AError> {
    let params = ServiceParams::new();
    match mode {
        Mode::Unary => {
            handler.send_message(&params, input).await?;
        }
        Mode::Streaming => {
            drop(handler.send_streaming_message(&params, input).await?);
        }
        Mode::HttpLive => {
            handler
                .send_live_request(
                    &params,
                    StreamRequest {
                        tenant: None,
                        payload: StreamRequestPayload::SendMessage(input),
                    },
                )
                .await?;
        }
        Mode::NativeLive => {
            let _ = handler
                .send_live_message(
                    &params,
                    stream::iter([StreamRequest {
                        tenant: None,
                        payload: StreamRequestPayload::SendMessage(input),
                    }])
                    .boxed(),
                )
                .await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn disconnect_after_input_commit_still_delivers_once_in_every_send_mode() {
    for mode in [
        Mode::Unary,
        Mode::Streaming,
        Mode::HttpLive,
        Mode::NativeLive,
    ] {
        for existing in [true, false] {
            if !existing && matches!(mode, Mode::Unary | Mode::Streaming) {
                continue;
            }
            let inner = InMemoryTaskStore::new();
            if existing {
                inner.create(task()).await.unwrap();
            }
            let committed = Arc::new(Notify::new());
            let release = Arc::new(Notify::new());
            let calls = Arc::new(AtomicUsize::new(0));
            let executed = Arc::new(Notify::new());
            let handler = DefaultRequestHandler::new(
                Agent {
                    calls: calls.clone(),
                    executed: executed.clone(),
                },
                DelayedCommit {
                    inner,
                    committed: committed.clone(),
                    release: release.clone(),
                    delay: AtomicBool::new(true),
                },
            )
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            });
            let caller = handler.clone();
            let pending = tokio::spawn(async move { send(&caller, request(existing), mode).await });
            tokio::time::timeout(Duration::from_secs(5), committed.notified())
                .await
                .unwrap();
            pending.abort();
            assert!(pending.await.unwrap_err().is_cancelled());
            release.notify_one();
            tokio::time::timeout(Duration::from_secs(5), executed.notified())
                .await
                .unwrap();
            let mut query = get();
            if !existing {
                query.id = handler
                    .list_tasks(
                        &ServiceParams::new(),
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
                    .tasks[0]
                    .id
                    .clone();
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let saved = handler
                        .get_task(&ServiceParams::new(), query.clone())
                        .await
                        .unwrap();
                    if saved.status.state == TaskState::Completed {
                        assert_eq!(saved.history.as_ref().unwrap().len(), 1);
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    }
}

#[tokio::test]
async fn rejected_inline_configuration_never_records_input_or_invokes_executor() {
    for mode in [
        Mode::Unary,
        Mode::Streaming,
        Mode::HttpLive,
        Mode::NativeLive,
    ] {
        for full_store in [false, true] {
            let inner = InMemoryTaskStore::new();
            inner.create(task()).await.unwrap();
            let configs = InMemoryPushConfigStore::new();
            if full_store {
                for _ in 0..50 {
                    configs
                        .save(config("https://example.com/hook"))
                        .await
                        .unwrap();
                }
            }
            let calls = Arc::new(AtomicUsize::new(0));
            let handler = DefaultRequestHandler::new(
                Agent {
                    calls: calls.clone(),
                    executed: Arc::new(Notify::new()),
                },
                inner,
            )
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            })
            .with_push_config_store(configs);
            let url = if full_store {
                "https://example.com/hook"
            } else {
                ""
            };
            let error = send(&handler, with_config(request(true), url), mode)
                .await
                .unwrap_err();
            assert_eq!(error.code, error_code::INVALID_PARAMS);
            assert_eq!(
                handler
                    .get_task(&ServiceParams::new(), get())
                    .await
                    .unwrap(),
                task()
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn invalid_configuration_cannot_create_an_orphan_live_task() {
    for mode in [Mode::HttpLive, Mode::NativeLive] {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = DefaultRequestHandler::new(
            Agent {
                calls: calls.clone(),
                executed: Arc::new(Notify::new()),
            },
            InMemoryTaskStore::new(),
        )
        .with_capabilities(AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        })
        .with_push_config_store(InMemoryPushConfigStore::new());
        assert!(
            send(&handler, with_config(request(false), ""), mode)
                .await
                .is_err()
        );
        let listed = handler
            .list_tasks(
                &ServiceParams::new(),
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
            .unwrap();
        assert!(listed.tasks.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
