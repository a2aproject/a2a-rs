// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Every cancellation path must finish artifacts at a recorded generation.
use a2a::*;
use a2a_server::*;
use futures::{StreamExt, stream, stream::BoxStream};
#[derive(Clone, Copy)]
enum Hook {
    Empty,
    Status,
    Snapshot,
    Artifact,
}
struct Agent(Hook);
impl AgentExecutor for Agent {
    fn execute(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::pending().boxed()
    }
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let event = match self.0 {
            Hook::Empty => return stream::empty().boxed(),
            Hook::Status => StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: ctx.task_id,
                context_id: ctx.context_id,
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                generation: 0,
                metadata: None,
            }),
            Hook::Snapshot => StreamResponse::Task(ctx.stored_task.unwrap()),
            Hook::Artifact => StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                task_id: ctx.task_id,
                context_id: ctx.context_id,
                artifact: artifact("during-cancel", Role::Agent),
                append: None,
                last_chunk: Some(false),
                metadata: None,
                generation: 0,
            }),
        };
        stream::iter([Ok(event)]).boxed()
    }
}
fn artifact(id: &str, role: Role) -> Artifact {
    Artifact {
        artifact_id: id.into(),
        parts: vec![Part::text("partial")],
        role: Some(role),
        start_generation: Some(1),
        end_generation: None,
        name: None,
        description: None,
        metadata: None,
        extensions: None,
    }
}
#[tokio::test]
async fn cancellation_finishes_existing_and_hook_created_artifacts() {
    for hook in [Hook::Empty, Hook::Status, Hook::Snapshot, Hook::Artifact] {
        let store = InMemoryTaskStore::new();
        let mut already_closed = artifact("complete", Role::Agent);
        already_closed.end_generation = Some(1);
        store
            .create(Task {
                id: "task".into(),
                context_id: "context".into(),
                generation: 1,
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                artifacts: Some(vec![
                    artifact("agent", Role::Agent),
                    artifact("user", Role::User),
                    already_closed,
                ]),
                history: None,
                timeline: None,
                metadata: None,
            })
            .await
            .unwrap();
        let handler = DefaultRequestHandler::new(Agent(hook), store);
        let canceled = handler
            .cancel_task(
                &ServiceParams::new(),
                CancelTaskRequest {
                    id: "task".into(),
                    tenant: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);
        for artifact in canceled.artifacts.as_ref().unwrap() {
            let end = artifact
                .end_generation
                .expect("terminal artifacts must be complete");
            if artifact.artifact_id == "complete" {
                assert_eq!(end, 1);
            } else {
                assert!((2..=canceled.generation).contains(&end));
            }
        }
        // Idempotent cancellation preserves both generation and completion stamps.
        let again = handler
            .cancel_task(
                &ServiceParams::new(),
                CancelTaskRequest {
                    id: "task".into(),
                    tenant: None,
                    metadata: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(again, canceled);
    }
}
