// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Returning an old executor snapshot must retain acknowledged client input.
use a2a::*;
use a2a_server::*;
use futures::{StreamExt, stream, stream::BoxStream};
use std::{sync::Arc, time::Duration};
use tokio::sync::Notify;
struct Agent {
    finish: Arc<Notify>,
}
impl AgentExecutor for Agent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let mut snapshot = ctx.stored_task.unwrap();
        let opening = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: ctx.task_id,
            context_id: ctx.context_id,
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            generation: 0,
            metadata: None,
        });
        let finish = self.finish.clone();
        stream::iter([Ok(opening)])
            .chain(stream::once(async move {
                finish.notified().await;
                snapshot.status.state = TaskState::Completed;
                Ok(StreamResponse::Task(snapshot))
            }))
            .boxed()
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}
fn request(text: &str) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = Some("task".into());
    SendMessageRequest {
        message,
        configuration: None,
        metadata: None,
        tenant: None,
        artifact_updates: vec![],
    }
}
fn live(request: SendMessageRequest) -> StreamRequest {
    StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::SendMessage(request),
    }
}
#[tokio::test]
async fn stale_snapshot_preserves_latest_user_artifact_and_recorded_history() {
    let store = InMemoryTaskStore::new();
    let artifact = Artifact {
        artifact_id: "draft".into(),
        parts: vec![Part::text("old draft")],
        role: Some(Role::User),
        start_generation: Some(1),
        end_generation: None,
        name: None,
        description: None,
        metadata: None,
        extensions: None,
    };
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
            history: None,
            timeline: None,
            artifacts: Some(vec![artifact.clone()]),
            metadata: None,
        })
        .await
        .unwrap();
    let finish = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        Agent {
            finish: finish.clone(),
        },
        store,
    )
    .with_capabilities(AgentCapabilities {
        streaming: Some(true),
        user_artifacts: Some(true),
        ..Default::default()
    });
    let params = ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])]);
    let LiveResponse::Stream(mut output) = handler
        .send_live_request(&params, live(request("start")))
        .await
        .unwrap()
    else {
        panic!("expected downlink")
    };
    output.next().await.unwrap().unwrap();
    output.next().await.unwrap().unwrap();
    let mut input = request("accepted while running");
    let mut replacement = artifact;
    replacement.parts = vec![Part::text("new draft")];
    input.artifact_updates = vec![TaskArtifactUpdateEvent {
        task_id: "task".into(),
        context_id: "context".into(),
        artifact: replacement,
        generation: 0,
        metadata: None,
        append: Some(false),
        last_chunk: Some(true),
    }];
    let LiveResponse::Ack(ack) = handler
        .send_live_request(&params, live(input))
        .await
        .unwrap()
    else {
        panic!("expected acknowledgement")
    };
    assert_eq!(
        ack.artifacts.as_ref().unwrap()[0].parts,
        [Part::text("new draft")]
    );
    let accepted_generation = ack.artifacts.as_ref().unwrap()[0].end_generation;
    finish.notify_one();
    let final_task = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = output.next().await {
            if let StreamResponse::Task(task) = event.unwrap() {
                if task.status.state.is_terminal() {
                    return task;
                }
            }
        }
        panic!("expected terminal snapshot")
    })
    .await
    .unwrap();
    let final_artifact = &final_task.artifacts.as_ref().unwrap()[0];
    assert_eq!(final_artifact.parts, [Part::text("new draft")]);
    assert_eq!(final_artifact.role, Some(Role::User));
    assert_eq!(final_artifact.end_generation, accepted_generation);
    let history: Vec<_> = final_task
        .history
        .as_ref()
        .unwrap()
        .iter()
        .filter_map(Message::text)
        .collect();
    assert_eq!(history, ["start", "accepted while running"]);
}
