// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Executor output must remain inside its bound task, including cancellation.
use a2a::*;
use a2a_server::*;
use futures::{StreamExt, stream, stream::BoxStream};

fn task(id: &str) -> Task {
    Task {
        id: id.into(),
        context_id: format!("ctx-{id}"),
        generation: 1,
        status: TaskStatus {
            state: TaskState::Working,
            message: None,
            timestamp: None,
        },
        history: None,
        timeline: None,
        artifacts: None,
        metadata: None,
    }
}
fn send(id: Option<&str>) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text("input")]);
    message.task_id = id.map(Into::into);
    SendMessageRequest {
        message,
        tenant: None,
        configuration: None,
        metadata: None,
        artifact_updates: vec![],
    }
}
fn get(id: &str) -> GetTaskRequest {
    GetTaskRequest {
        id: id.into(),
        tenant: None,
        history_length: None,
        timeline_length: None,
        current_generation: None,
    }
}
#[derive(Clone, Copy, Debug)]
enum Kind {
    Snapshot,
    Status,
    Artifact,
    Message,
}
struct WrongOutput {
    kind: Kind,
    context_only: bool,
}
impl WrongOutput {
    fn event(&self, ctx: ExecutorContext, state: TaskState) -> StreamResponse {
        let id = if self.context_only {
            ctx.task_id.clone()
        } else {
            "B".into()
        };
        let context = if self.context_only {
            "ctx-B".into()
        } else {
            ctx.context_id.clone()
        };
        match self.kind {
            Kind::Snapshot => {
                let mut task = ctx.stored_task.unwrap();
                task.id = id;
                task.context_id = context;
                task.status.state = state;
                StreamResponse::Task(task)
            }
            Kind::Status => StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: id,
                context_id: context,
                status: TaskStatus {
                    state,
                    message: None,
                    timestamp: None,
                },
                generation: 0,
                metadata: None,
            }),
            Kind::Artifact => StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: id,
                context_id: context,
                artifact: Artifact {
                    artifact_id: "output".into(),
                    parts: vec![Part::text("wrong task")],
                    name: None,
                    description: None,
                    metadata: None,
                    extensions: None,
                    start_generation: None,
                    end_generation: None,
                    role: None,
                },
                generation: 0,
                metadata: None,
                append: None,
                last_chunk: None,
            }),
            Kind::Message => {
                let mut message = Message::new(Role::Agent, vec![Part::text("wrong task")]);
                message.task_id = Some(id);
                message.context_id = Some(context);
                StreamResponse::Message(message)
            }
        }
    }
}
impl AgentExecutor for WrongOutput {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::iter([Ok(self.event(ctx, TaskState::Completed))]).boxed()
    }
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::iter([Ok(self.event(ctx, TaskState::Canceled))]).boxed()
    }
}
async fn handler(kind: Kind, context_only: bool) -> DefaultRequestHandler {
    let store = InMemoryTaskStore::new();
    for id in ["A", "B"] {
        store.create(task(id)).await.unwrap();
    }
    DefaultRequestHandler::new(WrongOutput { kind, context_only }, store).with_authorizer(
        |params: &ServiceParams, id: Option<&str>| {
            if params.contains_key("admin") || id.is_none_or(|id| id == "A") {
                Ok(())
            } else {
                Err(A2AError::invalid_request("denied"))
            }
        },
    )
}

#[tokio::test]
async fn incorrect_executor_task_or_context_never_changes_another_task() {
    for kind in [Kind::Snapshot, Kind::Status, Kind::Artifact, Kind::Message] {
        for context_only in [false, true] {
            let handler = handler(kind, context_only).await;
            let error = handler
                .send_message(&ServiceParams::new(), send(Some("A")))
                .await
                .unwrap_err();
            assert_eq!(
                error.code,
                error_code::INVALID_AGENT_RESPONSE,
                "{kind:?}, context_only={context_only}"
            );
            let admin = ServiceParams::from([("admin".into(), vec![])]);
            assert_eq!(handler.get_task(&admin, get("B")).await.unwrap(), task("B"));
        }
    }
}

#[tokio::test]
async fn incorrect_cancellation_output_never_changes_another_task() {
    for kind in [Kind::Snapshot, Kind::Status, Kind::Artifact, Kind::Message] {
        for context_only in [false, true] {
            let handler = handler(kind, context_only).await;
            let error = handler
                .cancel_task(
                    &ServiceParams::new(),
                    CancelTaskRequest {
                        id: "A".into(),
                        tenant: None,
                        metadata: None,
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, error_code::INVALID_AGENT_RESPONSE);
            let admin = ServiceParams::from([("admin".into(), vec![])]);
            assert_eq!(handler.get_task(&admin, get("B")).await.unwrap(), task("B"));
            assert_eq!(
                handler
                    .get_task(&admin, get("A"))
                    .await
                    .unwrap()
                    .status
                    .state,
                TaskState::Canceled
            );
        }
    }
}

#[tokio::test]
async fn a_direct_reply_cannot_claim_another_task_before_lazy_creation() {
    let handler = handler(Kind::Message, false).await;
    let error = handler
        .send_message(&ServiceParams::new(), send(None))
        .await
        .unwrap_err();
    assert_eq!(error.code, error_code::INVALID_AGENT_RESPONSE);
    let admin = ServiceParams::from([("admin".into(), vec![])]);
    assert_eq!(handler.get_task(&admin, get("B")).await.unwrap(), task("B"));
}
