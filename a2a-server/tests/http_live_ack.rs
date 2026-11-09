// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! A2A 1.1 §3.1.12: HTTP ACKs carry Task state; request rejections use the downlink.

use a2a::*;
use a2a_pb::protojson_conv;
use a2a_server::{
    AgentExecutor, DefaultRequestHandler, ExecutorContext, InMemoryTaskStore, RequestHandler,
    ServiceParams, jsonrpc::jsonrpc_router, rest::rest_router,
};
use axum::{
    Router,
    body::Body,
    http::{Request, Response, StatusCode, header},
};
use futures::{StreamExt, stream, stream::BoxStream};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::time::timeout;
use tower::ServiceExt;

const WAIT: Duration = Duration::from_secs(2);
const TOKEN: &str = "Bearer test-token";

struct Executor;

fn status(task_id: &str, context_id: &str, state: TaskState) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: task_id.into(),
        context_id: context_id.into(),
        generation: 0,
        metadata: None,
        status: TaskStatus {
            state,
            message: None,
            timestamp: None,
        },
    })
}

impl AgentExecutor for Executor {
    fn execute(
        &self,
        mut ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(async_stream::stream! {
            yield Ok(status(&ctx.task_id, &ctx.context_id, TaskState::Working));
            while let Some(input) = ctx.inputs.recv().await {
                assert_eq!(input.message.text(), Some("finish"), "rejected inputs must not reach the executor");
                yield Ok(status(&ctx.task_id, &ctx.context_id, TaskState::Completed));
            }
        })
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}

#[derive(Clone, Copy)]
enum Binding {
    Rest,
    JsonRpc,
}

impl Binding {
    fn request(self, request: &StreamRequest, authorized: bool) -> Request<Body> {
        let mut body = protojson_conv::to_value(request).unwrap();
        let path = match self {
            Self::Rest => "/rest/message:live",
            Self::JsonRpc => {
                body = json!({"jsonrpc": "2.0", "id": 1, "method": methods::SEND_LIVE_MESSAGE, "params": body});
                "/rpc"
            }
        };
        Request::post(path)
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "text/event-stream, application/json")
            .header(
                header::AUTHORIZATION,
                if authorized { TOKEN } else { "Bearer invalid" },
            )
            .header(SVC_PARAM_VERSION, "1.1")
            .header(SDK_LIVE_EXCHANGE_HEADER, "exchange")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap()
    }

    fn payload(self, body: Value) -> Value {
        match self {
            Self::Rest => body,
            Self::JsonRpc => {
                assert_eq!(body["jsonrpc"], "2.0");
                assert!(body.get("error").is_none(), "{body}");
                body.get("result").unwrap().clone()
            }
        }
    }

    async fn ack(self, response: Response<Body>) -> Task {
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            match self {
                Self::Rest => "application/a2a+json",
                Self::JsonRpc => "application/json",
            }
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let payload = self.payload(serde_json::from_slice(&bytes).unwrap());
        assert!(
            payload.get("task").is_none(),
            "ACKs contain Task, not StreamResponse events"
        );
        protojson_conv::from_value(payload).unwrap()
    }
}

struct Downlink {
    body: Body,
    pending: String,
    binding: Binding,
}

impl Downlink {
    async fn next(&mut self) -> StreamResponse {
        timeout(WAIT, async {
            loop {
                if let Some(end) = self.pending.find("\n\n") {
                    let frame: String = self.pending.drain(..end + 2).collect();
                    if let Some(data) = frame.lines().find_map(|line| line.strip_prefix("data:")) {
                        let payload = self
                            .binding
                            .payload(serde_json::from_str(data.trim()).unwrap());
                        return protojson_conv::from_value(payload).unwrap();
                    }
                } else {
                    let frame = self
                        .body
                        .frame()
                        .await
                        .expect("the exchange stays open")
                        .unwrap();
                    if let Ok(data) = frame.into_data() {
                        self.pending.push_str(std::str::from_utf8(&data).unwrap());
                    }
                }
            }
        })
        .await
        .expect("a downlink event arrives")
    }
}

fn send(text: &str, task_id: Option<&str>) -> StreamRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task_id.map(str::to_owned);
    StreamRequest {
        tenant: None,
        payload: StreamRequestPayload::SendMessage(SendMessageRequest {
            message,
            configuration: None,
            metadata: None,
            tenant: None,
            artifact_updates: vec![],
        }),
    }
}

async fn snapshot(handler: &DefaultRequestHandler, task_id: &str) -> Task {
    handler
        .get_task(
            &ServiceParams::from([
                (SVC_PARAM_VERSION.into(), vec!["1.1".into()]),
                ("authorization".into(), vec![TOKEN.into()]),
            ]),
            GetTaskRequest {
                id: task_id.into(),
                history_length: None,
                timeline_length: None,
                current_generation: None,
                tenant: None,
            },
        )
        .await
        .unwrap()
}

async fn rejected_follow_up(binding: Binding) {
    let handler = Arc::new(
        DefaultRequestHandler::new(Executor, InMemoryTaskStore::new())
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            })
            .with_authorizer(|params: &ServiceParams, _: Option<&str>| {
                if params
                    .get("authorization")
                    .and_then(|values| values.first())
                    .map(String::as_str)
                    == Some(TOKEN)
                {
                    Ok(())
                } else {
                    Err(A2AError::invalid_request("caller denied"))
                }
            }),
    );
    let app = Router::new()
        .nest("/rest", rest_router(handler.clone()))
        .nest("/rpc", jsonrpc_router(handler.clone()));
    let opening = app
        .clone()
        .oneshot(binding.request(&send("start", None), true))
        .await
        .unwrap();
    assert_eq!(opening.status(), StatusCode::OK);
    assert!(
        opening.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    let mut downlink = Downlink {
        body: opening.into_body(),
        pending: String::new(),
        binding,
    };
    let StreamResponse::Task(task) = downlink.next().await else {
        panic!("opening Task")
    };
    assert!(matches!(
        downlink.next().await,
        StreamResponse::StatusUpdate(_)
    ));
    let before = snapshot(&handler, &task.id).await;

    // Authentication failure is a binding error before routing, not a Task ACK.
    let denied = app
        .clone()
        .oneshot(binding.request(&send("denied", Some(&task.id)), false))
        .await
        .unwrap();
    assert_eq!(
        denied.status(),
        match binding {
            Binding::Rest => StatusCode::BAD_REQUEST,
            Binding::JsonRpc => StatusCode::OK,
        }
    );
    let body: Value =
        serde_json::from_slice(&denied.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["error"]["message"], "caller denied");
    match binding {
        Binding::Rest => assert_eq!(body["error"]["status"], "INVALID_ARGUMENT"),
        Binding::JsonRpc => {
            assert_eq!(body["error"]["code"], error_code::INVALID_REQUEST);
            assert!(body.get("result").is_none());
        }
    }

    let mut stale = send("stale", Some(&task.id));
    let StreamRequestPayload::SendMessage(request) = &mut stale.payload else {
        unreachable!()
    };
    let reference = request.message.message_id.clone();
    request.configuration = Some(SendMessageConfiguration {
        if_generation_match: Some(-1),
        return_immediately: None,
        accepted_output_modes: None,
        task_push_notification_config: None,
        history_length: None,
        timeline_length: None,
    });
    let response = timeout(WAIT, app.clone().oneshot(binding.request(&stale, true)))
        .await
        .unwrap()
        .unwrap();
    let ack = binding.ack(response).await;
    assert_eq!(
        ack, before,
        "a rejected input must not advance or mutate the task"
    );
    let StreamResponse::Error(error) = downlink.next().await else {
        panic!("rejection belongs on the original downlink")
    };
    assert_eq!(error.task_id, task.id);
    assert_eq!(error.code, "TASK_GENERATION_MISMATCH");
    assert_eq!(error.reference_id.as_deref(), Some(reference.as_str()));
    assert_eq!(snapshot(&handler, &task.id).await, before);

    // The same exchange remains usable and only the accepted input is announced.
    let response = app
        .oneshot(binding.request(&send("finish", Some(&task.id)), true))
        .await
        .unwrap();
    assert_eq!(binding.ack(response).await.id, task.id);
    let StreamResponse::MessageUpdate(update) = downlink.next().await else {
        panic!("accepted message update")
    };
    assert_eq!(update.message.text(), Some("finish"));
    let StreamResponse::StatusUpdate(update) = downlink.next().await else {
        panic!("completion")
    };
    assert_eq!(update.status.state, TaskState::Completed);
    assert!(
        timeout(WAIT, downlink.body.frame())
            .await
            .unwrap()
            .is_none()
    );
    handler.shutdown().await;
}

#[tokio::test]
async fn rest_acks_a_rejected_live_follow_up_and_reports_its_error_on_the_downlink() {
    rejected_follow_up(Binding::Rest).await;
}

#[tokio::test]
async fn jsonrpc_acks_a_rejected_live_follow_up_and_reports_its_error_on_the_downlink() {
    rejected_follow_up(Binding::JsonRpc).await;
}
