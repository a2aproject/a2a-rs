// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! The A2A 1.1 task generation features, driven through each binding with the
//! real clients against a real server: the request fields must reach the
//! handler and the new error must come back as the same A2A error.
use std::sync::Arc;
use std::time::{Duration, Instant};

use a2a::event::TaskStatusUpdateEvent;
use a2a::*;
use a2a_client::Transport;
use a2a_client::jsonrpc::JsonRpcTransport;
use a2a_client::rest::RestTransport;
use a2a_grpc::{GrpcHandler, GrpcTransport};
use a2a_pb::proto::a2a_service_server::A2aServiceServer;
use a2a_server::jsonrpc::jsonrpc_router;
use a2a_server::rest::rest_router;
use a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore, ServiceParams,
};
use axum::Router;
use futures::stream::{self, BoxStream};
use reqwest::Client;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

/// Asks for input until the client sends "second", then completes.
struct TwoTurnAgent;

impl AgentExecutor for TwoTurnAgent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        // Asks for input until it is told "second".
        let state = if ctx.message.as_ref().and_then(|m| m.text()) == Some("second") {
            TaskState::Completed
        } else {
            TaskState::InputRequired
        };
        Box::pin(stream::once(async move {
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: ctx.task_id,
                context_id: ctx.context_id,
                status: TaskStatus {
                    state,
                    message: Some(Message::new(Role::Agent, vec![Part::text("ok")])),
                    timestamp: None,
                },
                metadata: None,
                generation: 0,
            }))
        }))
    }

    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::once(async move {
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: ctx.task_id,
                context_id: ctx.context_id,
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
                generation: 0,
            }))
        }))
    }
}

fn handler() -> Arc<DefaultRequestHandler> {
    Arc::new(
        DefaultRequestHandler::new(TwoTurnAgent, InMemoryTaskStore::new())
            .with_capabilities(AgentCapabilities {
                user_artifacts: Some(true),
                streaming: Some(true),
                ..AgentCapabilities::default()
            })
            .with_long_poll_timeout(Duration::from_millis(600)),
    )
}

async fn spawn_http() -> String {
    let handler = handler();
    let app = Router::new()
        .nest("/rest", rest_router(handler.clone()))
        .nest("/rpc", jsonrpc_router(handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

async fn spawn_grpc() -> String {
    let service = A2aServiceServer::new(GrpcHandler::new(handler()));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = TcpListenerStream::new(listener);
    tokio::spawn(async move {
        Server::builder()
            .add_service(service)
            .serve_with_incoming(incoming)
            .await
            .unwrap()
    });
    format!("http://{addr}")
}

fn params() -> ServiceParams {
    let mut params = ServiceParams::new();
    params.insert(SVC_PARAM_VERSION.to_string(), vec![VERSION.to_string()]);
    params
}

fn send(text: &str, task_id: Option<&str>, if_generation_match: Option<i64>) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task_id.map(Into::into);
    SendMessageRequest {
        message,
        configuration: Some(SendMessageConfiguration {
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: None,
            timeline_length: None,
            return_immediately: None,
            if_generation_match,
        }),
        metadata: None,
        tenant: None,
        artifact_updates: Vec::new(),
    }
}

fn get(id: &str) -> GetTaskRequest {
    GetTaskRequest {
        id: id.into(),
        history_length: None,
        tenant: None,
        timeline_length: None,
        current_generation: None,
    }
}

async fn exercise(transport: impl Transport) {
    let params = params();

    let SendMessageResponse::Task(first) = transport
        .send_message(&params, &send("first", None, None))
        .await
        .unwrap()
    else {
        panic!("expected a task");
    };
    assert_eq!(first.status.state, TaskState::InputRequired);
    assert!(first.generation >= 2, "generation: {}", first.generation);
    assert!(first.timeline.as_ref().unwrap().len() >= 2);

    // timelineLength reaches the server.
    let mut request = get(&first.id);
    request.timeline_length = Some(1);
    let trimmed = transport.get_task(&params, &request).await.unwrap();
    assert_eq!(trimmed.timeline.as_ref().map(Vec::len), Some(1));
    assert_eq!(trimmed.generation, first.generation);

    // A stale ifGenerationMatch comes back as the same A2A error.
    let stale = transport
        .send_message(
            &params,
            &send("late", Some(&first.id), Some(first.generation - 1)),
        )
        .await
        .unwrap_err();
    assert_eq!(
        stale.code,
        error_code::TASK_GENERATION_MISMATCH,
        "{stale:?}"
    );

    // currentGeneration reaches the server: a task that has not moved is
    // held until the long-poll timeout, one that is ahead answers at once.
    let mut request = get(&first.id);
    request.current_generation = Some(first.generation);
    let started = Instant::now();
    let held = transport.get_task(&params, &request).await.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert_eq!(held.generation, first.generation);

    request.current_generation = Some(first.generation - 1);
    let started = Instant::now();
    let ahead = transport.get_task(&params, &request).await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(400));
    assert_eq!(ahead.generation, first.generation);

    // A matching ifGenerationMatch goes through.
    let SendMessageResponse::Task(second) = transport
        .send_message(
            &params,
            &send("second", Some(&first.id), Some(first.generation)),
        )
        .await
        .unwrap()
    else {
        panic!("expected a task");
    };
    assert_eq!(second.status.state, TaskState::Completed);
    assert!(second.generation > first.generation);
}

fn client_artifact(task_id: &str, text: &str) -> a2a::event::TaskArtifactUpdateEvent {
    a2a::event::TaskArtifactUpdateEvent {
        task_id: task_id.into(),
        context_id: String::new(),
        artifact: Artifact {
            artifact_id: "from-client".into(),
            name: None,
            description: None,
            parts: vec![Part::text(text)],
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
    }
}

/// Client-written artifacts reach the handler through each binding, with and
/// without a message.
async fn exercise_artifacts(transport: impl Transport) {
    let params = params();

    // Alongside the message that creates the task.
    let mut creating = send("first", None, None);
    creating.artifact_updates = vec![client_artifact("", "with the message")];
    let SendMessageResponse::Task(first) =
        transport.send_message(&params, &creating).await.unwrap()
    else {
        panic!("expected a task");
    };
    let artifact = &first.artifacts.as_ref().expect("the artifact was stored")[0];
    assert_eq!(artifact.role, Some(Role::User));
    assert!(artifact.start_generation.is_some());

    // On its own, for the task that exists: no message on the wire.
    let mut alone = send("", None, None);
    alone.message.role = Role::Unspecified;
    alone.message.message_id = String::new();
    alone.message.parts.clear();
    alone.artifact_updates = vec![client_artifact(&first.id, "on its own")];
    let SendMessageResponse::Task(after) = transport.send_message(&params, &alone).await.unwrap()
    else {
        panic!("expected a task");
    };

    assert_eq!(after.id, first.id, "it extended the task, not a new one");
    let listed = transport
        .list_tasks(
            &params,
            &ListTasksRequest {
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
    assert_eq!(listed.tasks.len(), 1);
}

#[tokio::test]
async fn rest_carries_client_artifacts() {
    let base = spawn_http().await;
    exercise_artifacts(RestTransport::new(Client::new(), format!("{base}/rest"))).await;
}

#[tokio::test]
async fn jsonrpc_carries_client_artifacts() {
    let base = spawn_http().await;
    exercise_artifacts(JsonRpcTransport::new(Client::new(), format!("{base}/rpc"))).await;
}

#[tokio::test]
async fn grpc_carries_client_artifacts() {
    let endpoint = spawn_grpc().await;
    exercise_artifacts(GrpcTransport::connect(endpoint).await.unwrap()).await;
}

#[tokio::test]
async fn rest_carries_the_generation_features() {
    let base = spawn_http().await;
    exercise(RestTransport::new(Client::new(), format!("{base}/rest"))).await;
}

#[tokio::test]
async fn jsonrpc_carries_the_generation_features() {
    let base = spawn_http().await;
    exercise(JsonRpcTransport::new(Client::new(), format!("{base}/rpc"))).await;
}

#[tokio::test]
async fn grpc_carries_the_generation_features() {
    let endpoint = spawn_grpc().await;
    exercise(GrpcTransport::connect(endpoint).await.unwrap()).await;
}
