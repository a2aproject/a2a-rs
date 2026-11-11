// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Trusted input context and independent live exchanges on a shared task.

use a2a::*;
use a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore, LiveResponse,
    RequestAuthorizer, RequestHandler, ServiceParams, User,
};
use futures::{StreamExt, stream::BoxStream};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Notify, mpsc};

type Downlink = BoxStream<'static, Result<StreamResponse, A2AError>>;

struct Tokens;
impl Tokens {
    fn user(params: &ServiceParams) -> Result<User, A2AError> {
        match params
            .get("authorization")
            .and_then(|v| v.first())
            .map(String::as_str)
        {
            Some("Bearer alice-secret") => Ok(User::authenticated("alice")),
            Some("Bearer bob-secret") => Ok(User::authenticated("bob")),
            _ => Err(A2AError::invalid_request("invalid credential")),
        }
    }
}
impl RequestAuthorizer for Tokens {
    fn authorize(&self, params: &ServiceParams, _: Option<&str>) -> Result<(), A2AError> {
        Self::user(params).map(|_| ())
    }
    fn authenticated_user(&self, params: &ServiceParams) -> Result<Option<User>, A2AError> {
        Self::user(params).map(Some)
    }
}

struct Observed {
    execution_id: String,
    text: String,
    user: Option<User>,
    params: ServiceParams,
    metadata: Option<std::collections::HashMap<String, serde_json::Value>>,
    tenant: Option<String>,
    initial: bool,
}
struct Agent {
    seen: mpsc::UnboundedSender<Observed>,
    release: Arc<Notify>,
}
fn status(ctx: &ExecutorContext, state: TaskState) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: ctx.task_id.clone(),
        context_id: ctx.context_id.clone(),
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
        metadata: None,
        generation: 0,
    })
}
impl AgentExecutor for Agent {
    fn execute(
        &self,
        mut ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let seen = self.seen.clone();
        let release = self.release.clone();
        Box::pin(async_stream::stream! {
            let text = ctx.message.as_ref().and_then(Message::text).unwrap_or("").to_string();
            seen.send(Observed { execution_id: ctx.execution_id.clone(), text: text.clone(), user: ctx.user.clone(),
                params: ctx.service_params.clone(), metadata: ctx.metadata.clone(),
                tenant: ctx.tenant.clone(), initial: true }).unwrap();
            yield Ok(status(&ctx, TaskState::Working));
            if text.starts_with("hold") {
                release.notified().await;
                if text == "hold-finish" { yield Ok(status(&ctx, TaskState::Completed)); }
                return;
            }
            while let Some(input) = ctx.inputs.recv().await {
                let finish = input.message.text() == Some("finish");
                seen.send(Observed { execution_id: ctx.execution_id.clone(), text: input.message.text().unwrap_or("").to_string(),
                    user: input.user, params: input.service_params, metadata: input.metadata,
                    tenant: input.tenant, initial: false }).unwrap();
                yield Ok(status(&ctx, if finish { TaskState::Completed } else { TaskState::Working }));
                if finish { return; }
            }
        })
    }
    fn cancel(&self, ctx: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let seen = self.seen.clone();
        Box::pin(futures::stream::once(async move {
            seen.send(Observed {
                execution_id: ctx.execution_id.clone(),
                text: "cancel".into(),
                user: ctx.user.clone(),
                params: ctx.service_params.clone(),
                metadata: ctx.metadata.clone(),
                tenant: ctx.tenant.clone(),
                initial: false,
            })
            .unwrap();
            Ok(status(&ctx, TaskState::Canceled))
        }))
    }
}
fn handler() -> (
    DefaultRequestHandler,
    mpsc::UnboundedReceiver<Observed>,
    Arc<Notify>,
) {
    let (seen, receiver) = mpsc::unbounded_channel();
    let release = Arc::new(Notify::new());
    let handler = DefaultRequestHandler::new(
        Agent {
            seen,
            release: release.clone(),
        },
        InMemoryTaskStore::new(),
    )
    .with_authorizer(Tokens)
    .with_capabilities(AgentCapabilities {
        streaming: Some(true),
        ..Default::default()
    });
    (handler, receiver, release)
}
fn params(user: &str, exchange: Option<&str>) -> ServiceParams {
    let mut params = ServiceParams::from([
        (SVC_PARAM_VERSION.into(), vec!["1.1".into()]),
        (
            "authorization".into(),
            vec![format!("Bearer {user}-secret")],
        ),
        ("x-request".into(), vec![user.into()]),
    ]);
    if let Some(id) = exchange {
        params.insert(SDK_LIVE_EXCHANGE_HEADER.into(), vec![id.into()]);
    }
    params
}
fn send(text: &str, task: Option<&str>) -> StreamRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task.map(Into::into);
    // A claimed identity in client metadata must not become a trusted caller.
    message.metadata = Some([("user".into(), serde_json::json!("mallory"))].into());
    StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::SendMessage(SendMessageRequest {
            message: Some(message),
            configuration: None,
            metadata: Some([("request".into(), serde_json::json!(text))].into()),
            tenant: Some("workspace".into()),
            artifact_updates: vec![],
        }),
    }
}
fn subscribe(task: &str) -> StreamRequest {
    StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::Subscribe(SubscribeToTaskRequest {
            id: task.into(),
            tenant: None,
            last_seen_generation: None,
        }),
    }
}
async fn next(stream: &mut Downlink) -> StreamResponse {
    tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
}
async fn open(
    handler: &DefaultRequestHandler,
    params: &ServiceParams,
    first: StreamRequest,
) -> Downlink {
    match handler.send_live_request(params, first).await.unwrap() {
        LiveResponse::Stream(stream) => stream,
        _ => panic!("each identified opening gets its own stream"),
    }
}
async fn start(
    handler: &DefaultRequestHandler,
    params: &ServiceParams,
    text: &str,
) -> (String, Downlink) {
    let mut stream = open(handler, params, send(text, None)).await;
    let StreamResponse::Task(task) = next(&mut stream).await else {
        panic!("opening task")
    };
    assert!(matches!(
        next(&mut stream).await,
        StreamResponse::StatusUpdate(_)
    ));
    (task.id, stream)
}
async fn ack(handler: &DefaultRequestHandler, params: &ServiceParams, request: StreamRequest) {
    assert!(matches!(
        handler.send_live_request(params, request).await.unwrap(),
        LiveResponse::Ack(_)
    ));
}
async fn seen(receiver: &mut mpsc::UnboundedReceiver<Observed>) -> Observed {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap()
        .unwrap()
}
fn caller(observed: Observed, user: &str, text: &str, initial: bool) -> String {
    assert_eq!(observed.user.unwrap().name, user);
    assert_eq!(observed.params["x-request"], [user]);
    assert_eq!(observed.metadata.unwrap()["request"], text);
    assert_eq!(observed.tenant.as_deref(), Some("workspace"));
    assert_eq!(observed.text, text);
    assert_eq!(observed.initial, initial);
    assert!(!observed.execution_id.is_empty());
    observed.execution_id
}

#[tokio::test]
async fn unread_inputs_are_private_and_fresh_requests_keep_their_own_caller_context() {
    let (handler, mut receiver, release) = handler();
    let (id, _owner) = start(&handler, &params("alice", Some("a")), "hold").await;
    let first_execution = caller(seen(&mut receiver).await, "alice", "hold", true);
    let bob_request = send("resume", Some(&id));
    let alice_request = send("finish", Some(&id));
    let reference = |request: &StreamRequest| match &request.payload {
        StreamRequestPayload::SendMessage(request) => {
            request.message.as_ref().unwrap().message_id.clone()
        }
        _ => unreachable!(),
    };
    let bob_id = reference(&bob_request);
    let alice_id = reference(&alice_request);
    let mut bob = open(&handler, &params("bob", Some("b")), bob_request).await;
    next(&mut bob).await;
    let mut alice = open(&handler, &params("alice", Some("c")), alice_request).await;
    next(&mut alice).await;
    release.notify_one();
    for (downlink, id) in [(&mut bob, bob_id), (&mut alice, alice_id)] {
        loop {
            if let StreamResponse::Error(error) = next(downlink).await {
                assert_eq!(error.code, "UNAVAILABLE");
                assert_eq!(error.reference_id.as_deref(), Some(id.as_str()));
                break;
            }
        }
    }
    assert!(
        receiver.try_recv().is_err(),
        "unread inputs cannot start another execution"
    );
    ack(
        &handler,
        &params("bob", Some("b")),
        send("resume-fresh", Some(&id)),
    )
    .await;
    let next_execution = caller(seen(&mut receiver).await, "bob", "resume-fresh", true);
    assert_ne!(first_execution, next_execution);
    ack(
        &handler,
        &params("alice", Some("c")),
        send("finish", Some(&id)),
    )
    .await;
    let receiving_execution = caller(seen(&mut receiver).await, "alice", "finish", false);
    assert_eq!(next_execution, receiving_execution);
    handler.shutdown().await;
}

#[tokio::test]
async fn unary_streaming_and_live_inputs_carry_their_own_trusted_context() {
    for mode in ["unary", "streaming", "live"] {
        let (handler, mut receiver, _) = handler();
        let (id, _owner) = start(&handler, &params("alice", Some("a")), "start").await;
        let initial_execution = caller(seen(&mut receiver).await, "alice", "start", true);
        let request = send("finish", Some(&id));
        let StreamRequestPayload::SendMessage(mut message) = request.payload.clone() else {
            unreachable!()
        };
        let bob = params("bob", Some("b"));
        match mode {
            "unary" => {
                message.configuration = Some(SendMessageConfiguration {
                    return_immediately: Some(true),
                    if_generation_match: None,
                    timeline_length: None,
                    accepted_output_modes: None,
                    task_push_notification_config: None,
                    history_length: None,
                });
                handler.send_message(&bob, message).await.unwrap();
            }
            "streaming" => {
                let _stream = handler.send_streaming_message(&bob, message).await.unwrap();
            }
            _ => {
                let _stream = open(&handler, &bob, request).await;
            }
        }
        let input_execution = caller(seen(&mut receiver).await, "bob", "finish", false);
        assert_eq!(
            initial_execution, input_execution,
            "input does not start another execution while the task is running"
        );
    }
}

#[tokio::test]
async fn cancellation_identifies_the_active_execution_it_stops() {
    let (handler, mut receiver, _) = handler();
    let (task_id, _owner) = start(&handler, &params("alice", Some("a")), "start").await;
    let execution = seen(&mut receiver).await;
    let canceled = handler
        .cancel_task(
            &params("bob", None),
            CancelTaskRequest {
                id: task_id,
                metadata: None,
                tenant: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(canceled.status.state, TaskState::Canceled);
    let cancellation = seen(&mut receiver).await;
    assert_eq!(cancellation.text, "cancel");
    assert_eq!(cancellation.execution_id, execution.execution_id);
    assert_eq!(cancellation.user.unwrap().name, "bob");
}

#[tokio::test]
async fn shared_credentials_have_independent_rejection_streams_and_bound_task_routing() {
    let (handler, _receiver, _) = handler();
    let a = params("alice", Some("a"));
    let b = params("alice", Some("b"));
    let (id, mut owner) = start(&handler, &a, "start").await;
    let mut other = open(&handler, &b, subscribe(&id)).await;
    next(&mut other).await;
    let mut stale = send("stale", Some(&id));
    let reference = if let StreamRequestPayload::SendMessage(request) = &mut stale.payload {
        request.configuration = Some(SendMessageConfiguration {
            if_generation_match: Some(-1),
            return_immediately: None,
            timeline_length: None,
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: None,
        });
        request.message.as_ref().unwrap().message_id.clone()
    } else {
        unreachable!()
    };
    ack(&handler, &b, stale).await;
    let StreamResponse::Error(error) = next(&mut other).await else {
        panic!("private rejection")
    };
    assert_eq!(error.reference_id.as_deref(), Some(reference.as_str()));
    // The exchange ID supplies the bound task even when a later message omits it.
    ack(&handler, &b, send("hello", None)).await;
    for stream in [&mut owner, &mut other] {
        assert!(matches!(
            next(stream).await,
            StreamResponse::MessageUpdate(_)
        ));
        assert!(matches!(
            next(stream).await,
            StreamResponse::StatusUpdate(_)
        ));
    }
    ack(&handler, &a, send("finish", Some(&id))).await;
}

#[tokio::test]
async fn exchange_identifier_is_scoped_to_the_authenticated_principal() {
    let (handler, mut receiver, _) = handler();
    let alice = params("alice", Some("shared-id"));
    let bob = params("bob", Some("shared-id"));
    let (alice_task, _alice_output) = start(&handler, &alice, "start").await;
    caller(seen(&mut receiver).await, "alice", "start", true);
    // The same spelling in Bob's scope opens his own exchange and task;
    // it must not inherit Alice's implicit task binding.
    let (bob_task, _bob_output) = start(&handler, &bob, "start").await;
    caller(seen(&mut receiver).await, "bob", "start", true);
    assert_ne!(alice_task, bob_task);
    ack(&handler, &bob, send("finish", None)).await;
    caller(seen(&mut receiver).await, "bob", "finish", false);
    ack(&handler, &alice, send("finish", None)).await;
    caller(seen(&mut receiver).await, "alice", "finish", false);
}

#[tokio::test]
async fn ambiguous_legacy_input_is_refused_instead_of_routed_to_the_newest_stream() {
    let (handler, _receiver, _) = handler();
    let p = params("alice", None);
    let (id, _owner) = start(&handler, &p, "start").await;
    let mut other = open(&handler, &p, subscribe(&id)).await;
    next(&mut other).await;
    let error = handler
        .send_live_request(&p, send("ambiguous", Some(&id)))
        .await
        .err()
        .unwrap();
    assert!(error.message.contains("ambiguous"));
}

#[tokio::test]
async fn unread_input_rejection_is_private_to_the_exchange_that_accepted_it() {
    let (handler, _receiver, release) = handler();
    let a = params("alice", Some("a"));
    let b = params("alice", Some("b"));
    let (id, mut owner) = start(&handler, &a, "hold-finish").await;
    let mut other = open(&handler, &b, subscribe(&id)).await;
    next(&mut other).await;
    ack(&handler, &b, send("unread", Some(&id))).await;
    for stream in [&mut owner, &mut other] {
        assert!(matches!(
            next(stream).await,
            StreamResponse::MessageUpdate(_)
        ));
    }
    release.notify_one();
    assert!(matches!(next(&mut other).await, StreamResponse::Error(_)));
    for stream in [&mut owner, &mut other] {
        let StreamResponse::StatusUpdate(update) = next(stream).await else {
            panic!("completion, without another client's rejection")
        };
        assert_eq!(update.status.state, TaskState::Completed);
    }
}

#[tokio::test]
async fn native_live_rejections_stay_private_and_unary_rejections_are_not_broadcast() {
    for native_input in [true, false] {
        let (handler, _receiver, release) = handler();
        let a = params("alice", Some("a"));
        let (id, mut owner) = start(&handler, &a, "hold-finish").await;
        let (up, receiver) = mpsc::channel(4);
        up.send(subscribe(&id)).await.unwrap();
        let mut other = handler
            .send_live_message(
                &params("bob", None),
                Box::pin(tokio_stream::wrappers::ReceiverStream::new(receiver)),
            )
            .await
            .unwrap();
        next(&mut other).await;
        if native_input {
            up.send(send("unread", Some(&id))).await.unwrap();
        } else {
            let StreamRequestPayload::SendMessage(mut request) = send("unread", Some(&id)).payload
            else {
                unreachable!()
            };
            request.configuration = Some(SendMessageConfiguration {
                return_immediately: Some(true),
                if_generation_match: None,
                timeline_length: None,
                accepted_output_modes: None,
                task_push_notification_config: None,
                history_length: None,
            });
            handler
                .send_message(&params("bob", None), request)
                .await
                .unwrap();
        }
        for stream in [&mut owner, &mut other] {
            assert!(matches!(
                next(stream).await,
                StreamResponse::MessageUpdate(_)
            ));
        }
        release.notify_one();
        if native_input {
            assert!(matches!(next(&mut other).await, StreamResponse::Error(_)));
        }
        for stream in [&mut owner, &mut other] {
            let StreamResponse::StatusUpdate(update) = next(stream).await else {
                panic!("only the originating live stream receives an unread-input rejection")
            };
            assert_eq!(update.status.state, TaskState::Completed);
        }
    }
}
