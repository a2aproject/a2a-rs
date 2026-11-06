// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! `SendLiveMessage` through each binding, with the real clients against a real
//! server: the exchange opens, a message sent while the agent works reaches it,
//! and the events come back in order.
use std::sync::Arc;
use std::time::Duration;

use a2a::*;
use a2a_client::jsonrpc::JsonRpcTransport;
use a2a_client::rest::RestTransport;
use a2a_client::{LiveAck, LiveConnection, Transport};
use a2a_grpc::{GrpcHandler, GrpcTransport};
use a2a_pb::proto::a2a_service_server::A2aServiceServer;
use a2a_server::jsonrpc::jsonrpc_router;
use a2a_server::rest::rest_router;
use a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore, ServiceParams,
};
use axum::Router;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use reqwest::Client;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;

/// Works until told "finish", answering each message it is sent while it does.
struct ChattyAgent {
    takes_input: bool,
    /// What an agent that takes no input waits for before it finishes.
    gate: Arc<tokio::sync::Notify>,
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

impl AgentExecutor for ChattyAgent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let gate = self.gate.clone();
        let takes_input = self.takes_input;
        Box::pin(async_stream::stream! {
            let (task, context) = (ctx.task_id.clone(), ctx.context_id.clone());
            yield Ok(status(&task, &context, TaskState::Working, "working"));
            if !takes_input {
                // Leaves input queued: the test ends this turn explicitly.
                gate.notified().await;
                yield Ok(status(&task, &context, TaskState::Completed, "done"));
                return;
            }
            let mut inputs = ctx.inputs;
            while let Some(input) = inputs.recv().await {
                let text = input.message.text().unwrap_or("").to_string();
                if text == "finish" {
                    yield Ok(status(&task, &context, TaskState::Completed, "done"));
                    return;
                }
                yield Ok(status(&task, &context, TaskState::Working, &format!("got: {text}")));
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

fn handler(takes_input: bool) -> (Arc<DefaultRequestHandler>, Arc<tokio::sync::Notify>) {
    let gate = Arc::new(tokio::sync::Notify::new());
    let handler = Arc::new(
        DefaultRequestHandler::new(
            ChattyAgent {
                takes_input,
                gate: gate.clone(),
            },
            InMemoryTaskStore::new(),
        )
        .with_capabilities(AgentCapabilities {
            streaming: Some(true),
            ..AgentCapabilities::default()
        }),
    );
    (handler, gate)
}

async fn spawn_http(takes_input: bool) -> (String, Arc<tokio::sync::Notify>) {
    let (handler, gate) = handler(takes_input);
    let app = Router::new()
        .nest("/rest", rest_router(handler.clone()))
        .nest("/rpc", jsonrpc_router(handler));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), gate)
}

async fn spawn_grpc(takes_input: bool) -> (String, Arc<tokio::sync::Notify>) {
    let (handler, gate) = handler(takes_input);
    let service = A2aServiceServer::new(GrpcHandler::new(handler));
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
    (format!("http://{addr}"), gate)
}

fn params() -> ServiceParams {
    let mut params = ServiceParams::new();
    params.insert(SVC_PARAM_VERSION.to_string(), vec!["1.1".to_string()]);
    params
}

fn send(text: &str, task_id: Option<&str>) -> StreamRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task_id.map(Into::into);
    StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::SendMessage(SendMessageRequest {
            message,
            configuration: None,
            metadata: None,
            tenant: None,
            artifact_updates: Vec::new(),
        }),
    }
}

async fn next(connection: &mut LiveConnection) -> StreamResponse {
    tokio::time::timeout(Duration::from_secs(5), connection.events.next())
        .await
        .expect("an event arrives")
        .expect("the stream is open")
        .expect("no transport error")
}

fn text_of(event: &StreamResponse) -> String {
    match event {
        StreamResponse::StatusUpdate(update) => update
            .status
            .message
            .as_ref()
            .and_then(|m| m.text())
            .unwrap_or("")
            .to_string(),
        other => panic!("expected a status update, got {other:?}"),
    }
}

/// Opens an exchange, talks to a running agent, and finishes it.
async fn converse(transport: impl Transport, acknowledges: bool) {
    let mut live = transport
        .send_live_message(&params(), &send("start", None))
        .await
        .expect("the exchange opens");

    let StreamResponse::Task(opening) = next(&mut live).await else {
        panic!("the exchange opens with the task");
    };
    assert_eq!(opening.generation, 1);
    assert_eq!(text_of(&next(&mut live).await), "working");

    // A message sent while the agent works.
    let ack = live
        .sender
        .send(send("hello", Some(&opening.id)))
        .await
        .expect("the request is sent");
    if acknowledges {
        let LiveAck::Task(task) = ack else {
            panic!("an HTTP call is answered with the task");
        };
        let task = *task;
        assert_eq!(task.id, opening.id);
        let messages = task
            .timeline
            .iter()
            .flatten()
            .filter(|entry| matches!(entry.content, TimelineContent::Message(_)))
            .count();
        assert_eq!(messages, 2, "the acknowledgement reflects the message");
    } else {
        assert!(matches!(ack, LiveAck::Sent));
    }
    let StreamResponse::MessageUpdate(announced) = next(&mut live).await else {
        panic!("the message is announced on the downlink");
    };
    assert_eq!(announced.message.text(), Some("hello"));
    assert_eq!(text_of(&next(&mut live).await), "got: hello");

    live.sender
        .send_message({
            let mut message = Message::new(Role::User, vec![Part::text("finish")]);
            message.task_id = Some(opening.id.clone());
            message
        })
        .await
        .expect("the request is sent");
    assert!(matches!(
        next(&mut live).await,
        StreamResponse::MessageUpdate(_)
    ));
    assert_eq!(text_of(&next(&mut live).await), "done");
    let end = tokio::time::timeout(Duration::from_secs(5), live.events.next())
        .await
        .expect("the stream ends");
    assert!(end.is_none(), "{end:?}");
}

/// An unread message is answered when the turn ends, and the exchange goes on.
async fn unread_input(transport: impl Transport, gate: Arc<tokio::sync::Notify>) {
    let mut live = transport
        .send_live_message(&params(), &send("start", None))
        .await
        .expect("the exchange opens");
    let StreamResponse::Task(opening) = next(&mut live).await else {
        panic!("the exchange opens with the task");
    };
    assert_eq!(text_of(&next(&mut live).await), "working");

    let rejected = send("hello", Some(&opening.id));
    let message_id = match &rejected.payload {
        StreamRequestPayload::SendMessage(r) => r.message.message_id.clone(),
        _ => unreachable!(),
    };
    live.sender
        .send(rejected)
        .await
        .expect("the request is sent");
    assert!(matches!(
        next(&mut live).await,
        StreamResponse::MessageUpdate(_)
    ));

    // The message is queued while this turn runs; its unread reply arrives
    // only when the turn ends.
    gate.notify_one();
    match next(&mut live).await {
        StreamResponse::Error(error) => {
            assert_eq!(error.code, "UNAVAILABLE");
            assert_eq!(error.reference_id.as_deref(), Some(message_id.as_str()));
        }
        other => panic!("expected a StreamError, got {other:?}"),
    }
    assert_eq!(text_of(&next(&mut live).await), "done");
}

#[tokio::test]
async fn rest_carries_a_live_exchange() {
    let (base, _) = spawn_http(true).await;
    converse(
        RestTransport::new(Client::new(), format!("{base}/rest")),
        true,
    )
    .await;
}

#[tokio::test]
async fn jsonrpc_carries_a_live_exchange() {
    let (base, _) = spawn_http(true).await;
    converse(
        JsonRpcTransport::new(Client::new(), format!("{base}/rpc")),
        true,
    )
    .await;
}

#[tokio::test]
async fn grpc_carries_a_live_exchange() {
    let (endpoint, _) = spawn_grpc(true).await;
    converse(GrpcTransport::connect(endpoint).await.unwrap(), false).await;
}

#[tokio::test]
async fn rest_reports_unread_input_as_a_stream_error_when_the_turn_ends() {
    let (base, gate) = spawn_http(false).await;
    unread_input(
        RestTransport::new(Client::new(), format!("{base}/rest")),
        gate,
    )
    .await;
}

#[tokio::test]
async fn jsonrpc_reports_unread_input_as_a_stream_error_when_the_turn_ends() {
    let (base, gate) = spawn_http(false).await;
    unread_input(
        JsonRpcTransport::new(Client::new(), format!("{base}/rpc")),
        gate,
    )
    .await;
}

#[tokio::test]
async fn grpc_reports_unread_input_as_a_stream_error_when_the_turn_ends() {
    let (endpoint, gate) = spawn_grpc(false).await;
    unread_input(GrpcTransport::connect(endpoint).await.unwrap(), gate).await;
}

#[tokio::test]
async fn a_handler_that_cannot_do_live_exchanges_says_so_over_every_binding() {
    // slimrpc has no live binding yet: the default says so.
    struct NoLive;
    #[async_trait::async_trait]
    impl Transport for NoLive {
        async fn send_message(
            &self,
            _: &ServiceParams,
            _: &SendMessageRequest,
        ) -> Result<SendMessageResponse, A2AError> {
            unreachable!()
        }
        async fn send_streaming_message(
            &self,
            _: &ServiceParams,
            _: &SendMessageRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            unreachable!()
        }
        async fn get_task(&self, _: &ServiceParams, _: &GetTaskRequest) -> Result<Task, A2AError> {
            unreachable!()
        }
        async fn list_tasks(
            &self,
            _: &ServiceParams,
            _: &ListTasksRequest,
        ) -> Result<ListTasksResponse, A2AError> {
            unreachable!()
        }
        async fn cancel_task(
            &self,
            _: &ServiceParams,
            _: &CancelTaskRequest,
        ) -> Result<Task, A2AError> {
            unreachable!()
        }
        async fn subscribe_to_task(
            &self,
            _: &ServiceParams,
            _: &SubscribeToTaskRequest,
        ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
            unreachable!()
        }
        async fn create_push_config(
            &self,
            _: &ServiceParams,
            _: &TaskPushNotificationConfig,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            unreachable!()
        }
        async fn get_push_config(
            &self,
            _: &ServiceParams,
            _: &GetTaskPushNotificationConfigRequest,
        ) -> Result<TaskPushNotificationConfig, A2AError> {
            unreachable!()
        }
        async fn list_push_configs(
            &self,
            _: &ServiceParams,
            _: &ListTaskPushNotificationConfigsRequest,
        ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
            unreachable!()
        }
        async fn delete_push_config(
            &self,
            _: &ServiceParams,
            _: &DeleteTaskPushNotificationConfigRequest,
        ) -> Result<(), A2AError> {
            unreachable!()
        }
        async fn get_extended_agent_card(
            &self,
            _: &ServiceParams,
            _: &GetExtendedAgentCardRequest,
        ) -> Result<AgentCard, A2AError> {
            unreachable!()
        }
        async fn destroy(&self) -> Result<(), A2AError> {
            Ok(())
        }
    }
    let error = NoLive
        .send_live_message(&params(), &send("hi", None))
        .await
        .err()
        .unwrap();
    assert_eq!(error.code, error_code::UNSUPPORTED_OPERATION);
}

/// The exchange ends (the client's stream goes away) while the task keeps
/// running; the next request is then a call of its own that opens a new
/// exchange. The client is handed it, not told the request failed.
async fn reopened_after_the_exchange_ended(transport: impl Transport) {
    let mut live = transport
        .send_live_message(&params(), &send("start", None))
        .await
        .expect("the exchange opens");
    let StreamResponse::Task(opening) = next(&mut live).await else {
        panic!("the exchange opens with the task");
    };
    assert_eq!(text_of(&next(&mut live).await), "working");
    let sender = live.sender.clone();
    drop(live); // the stream goes away; the server ends the exchange

    // The request reaches the exchange before it has noticed (a plain
    // acknowledgement) or finds none and opens a new one, which the client is
    // handed; the message is applied once either way.
    let task = match sender
        .send(send("hello", Some(&opening.id)))
        .await
        .expect("the request is sent")
    {
        LiveAck::Reopened(mut again) => {
            let StreamResponse::Task(task) = next(&mut again).await else {
                panic!("a new exchange opens with the task");
            };
            task
        }
        LiveAck::Task(task) => *task,
        _ => panic!("unexpected acknowledgement"),
    };
    let said: Vec<_> = task
        .timeline
        .iter()
        .flatten()
        .filter_map(|entry| match &entry.content {
            TimelineContent::Message(m) => m.text().map(str::to_owned),
            _ => None,
        })
        .collect();
    assert_eq!(said, ["start", "hello"], "the message was applied once");
}

#[tokio::test]
async fn rest_hands_over_the_exchange_a_request_had_to_reopen() {
    let (base, _) = spawn_http(true).await;
    reopened_after_the_exchange_ended(RestTransport::new(Client::new(), format!("{base}/rest")))
        .await;
}

#[tokio::test]
async fn jsonrpc_hands_over_the_exchange_a_request_had_to_reopen() {
    let (base, _) = spawn_http(true).await;
    reopened_after_the_exchange_ended(JsonRpcTransport::new(Client::new(), format!("{base}/rpc")))
        .await;
}

/// A second client that sends to a task whose exchange is open is input to
/// that exchange: it is carried out, and it is given the task, not an error.
async fn joining_another_clients_exchange(first: impl Transport, second: impl Transport) {
    let mut owner = first
        .send_live_message(&params(), &send("start", None))
        .await
        .expect("the exchange opens");
    let StreamResponse::Task(opening) = next(&mut owner).await else {
        panic!("the exchange opens with the task");
    };
    assert_eq!(text_of(&next(&mut owner).await), "working");

    let mut joined = second
        .send_live_message(&params(), &send("hello", Some(&opening.id)))
        .await
        .expect("the second client is not told it failed");
    let StreamResponse::Task(task) = next(&mut joined).await else {
        panic!("it is given the task");
    };
    assert_eq!(task.id, opening.id);

    // The owner's exchange heard the message, and the agent answered.
    assert!(matches!(
        next(&mut owner).await,
        StreamResponse::MessageUpdate(_)
    ));
    assert_eq!(text_of(&next(&mut owner).await), "got: hello");
}

#[tokio::test]
async fn rest_lets_a_second_client_join_an_open_exchange() {
    let (base, _) = spawn_http(true).await;
    joining_another_clients_exchange(
        RestTransport::new(Client::new(), format!("{base}/rest")),
        RestTransport::new(Client::new(), format!("{base}/rest")),
    )
    .await;
}

#[tokio::test]
async fn jsonrpc_lets_a_second_client_join_an_open_exchange() {
    let (base, _) = spawn_http(true).await;
    joining_another_clients_exchange(
        JsonRpcTransport::new(Client::new(), format!("{base}/rpc")),
        JsonRpcTransport::new(Client::new(), format!("{base}/rpc")),
    )
    .await;
}

/// Two connections using the same credentials still own independent downlinks.
/// Rejected requests are private, while accepted task updates reach both.
async fn independent_clients(first: impl Transport, second: impl Transport) {
    let mut owner = first
        .send_live_message(&params(), &send("start", None))
        .await
        .unwrap();
    let StreamResponse::Task(task) = next(&mut owner).await else {
        panic!("opening task")
    };
    next(&mut owner).await; // Working.
    let subscription = StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::Subscribe(SubscribeToTaskRequest {
            id: task.id.clone(),
            tenant: None,
            last_seen_generation: None,
        }),
    };
    let mut other = second
        .send_live_message(&params(), &subscription)
        .await
        .unwrap();
    assert!(matches!(next(&mut other).await, StreamResponse::Task(_)));
    let mut stale = send("stale", Some(&task.id));
    let StreamRequestPayload::SendMessage(request) = &mut stale.payload else {
        unreachable!()
    };
    request.configuration = Some(SendMessageConfiguration {
        if_generation_match: Some(-1),
        return_immediately: None,
        timeline_length: None,
        accepted_output_modes: None,
        task_push_notification_config: None,
        history_length: None,
    });
    let reference = request.message.message_id.clone();
    other.sender.send(stale).await.unwrap();
    let StreamResponse::Error(error) = next(&mut other).await else {
        panic!("private rejection")
    };
    assert_eq!(error.reference_id.as_deref(), Some(reference.as_str()));

    other
        .sender
        .send(send("hello", Some(&task.id)))
        .await
        .unwrap();
    for stream in [&mut owner, &mut other] {
        assert!(
            matches!(next(stream).await, StreamResponse::MessageUpdate(_)),
            "another client's rejection must not appear on this stream"
        );
        assert_eq!(text_of(&next(stream).await), "got: hello");
    }
    owner
        .sender
        .send(send("finish", Some(&task.id)))
        .await
        .unwrap();
    for stream in [&mut owner, &mut other] {
        assert!(matches!(
            next(stream).await,
            StreamResponse::MessageUpdate(_)
        ));
        assert_eq!(text_of(&next(stream).await), "done");
        assert!(
            tokio::time::timeout(Duration::from_secs(5), stream.events.next())
                .await
                .unwrap()
                .is_none()
        );
    }
}

#[tokio::test]
async fn rest_clients_keep_rejections_private_on_a_shared_task() {
    let (base, _) = spawn_http(true).await;
    independent_clients(
        RestTransport::new(Client::new(), format!("{base}/rest")),
        RestTransport::new(Client::new(), format!("{base}/rest")),
    )
    .await;
}

#[tokio::test]
async fn jsonrpc_clients_keep_rejections_private_on_a_shared_task() {
    let (base, _) = spawn_http(true).await;
    independent_clients(
        JsonRpcTransport::new(Client::new(), format!("{base}/rpc")),
        JsonRpcTransport::new(Client::new(), format!("{base}/rpc")),
    )
    .await;
}

#[tokio::test]
async fn grpc_clients_keep_rejections_private_on_a_shared_task() {
    let (base, _) = spawn_grpc(true).await;
    independent_clients(
        GrpcTransport::connect(base.clone()).await.unwrap(),
        GrpcTransport::connect(base).await.unwrap(),
    )
    .await;
}

#[tokio::test]
async fn the_client_opens_an_exchange_through_its_interceptors_and_applies_the_tenant() {
    let (base, _) = spawn_http(true).await;
    let client =
        a2a_client::A2AClient::new(RestTransport::new(Client::new(), format!("{base}/rest")))
            .with_tenant("acme");

    let mut live = client
        .send_live_message(&send("start", None))
        .await
        .expect("the exchange opens");
    let StreamResponse::Task(opening) = next(&mut live).await else {
        panic!("the exchange opens with the task");
    };
    assert_eq!(text_of(&next(&mut live).await), "working");
    live.sender
        .send_message({
            let mut message = Message::new(Role::User, vec![Part::text("finish")]);
            message.task_id = Some(opening.id.clone());
            message
        })
        .await
        .expect("the request is sent");
    // The client signals the SDK's own version, 1.1, so the message it sent is
    // announced to it before the outcome.
    assert!(matches!(
        next(&mut live).await,
        StreamResponse::MessageUpdate(_)
    ));
    assert_eq!(text_of(&next(&mut live).await), "done");
}
