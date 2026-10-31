// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Regression coverage for authorization boundaries and required protocol fields.
use a2a::*;
use a2a_server::task_store::TaskVersion;
use a2a_server::*;
use futures::{StreamExt, stream, stream::BoxStream};
struct Agent(bool);
impl AgentExecutor for Agent {
    fn execute(
        &self,
        ctx: ExecutorContext,
    ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        let terminal = self.0;
        let event = stream::once(async move {
            Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: ctx.task_id,
                context_id: ctx.context_id,
                status: TaskStatus {
                    state: if terminal {
                        TaskState::Completed
                    } else {
                        TaskState::Working
                    },
                    message: None,
                    timestamp: None,
                },
                metadata: None,
                generation: 0,
            }))
        });
        if terminal {
            event.boxed()
        } else {
            event.chain(stream::pending()).boxed()
        }
    }
    fn cancel(&self, _: ExecutorContext) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::empty().boxed()
    }
}
struct Policy;
impl RequestAuthorizer for Policy {
    fn authorize(&self, params: &ServiceParams, task: Option<&str>) -> Result<(), A2AError> {
        let scope = params
            .get("authorization")
            .and_then(|v| v.first())
            .map(String::as_str);
        match scope {
            Some("admin") => Ok(()),
            Some("limited") if task.is_none_or(|id| id == "public") => Ok(()),
            _ => Err(A2AError::invalid_request("denied")),
        }
    }
    fn authenticated_user(&self, _: &ServiceParams) -> Result<Option<User>, A2AError> {
        Ok(Some(User::authenticated("alice")))
    }
}
fn params(scope: &str) -> ServiceParams {
    [
        (SVC_PARAM_VERSION.into(), vec!["1.1".into()]),
        ("authorization".into(), vec![scope.into()]),
        (
            SDK_LIVE_EXCHANGE_HEADER.into(),
            vec!["admin-exchange".into()],
        ),
    ]
    .into()
}
fn send(text: &str, task: Option<&str>) -> SendMessageRequest {
    let mut message = Message::new(Role::User, vec![Part::text(text)]);
    message.task_id = task.map(Into::into);
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

fn list_request(size: i32) -> ListTasksRequest {
    ListTasksRequest {
        context_id: None,
        status: None,
        page_size: Some(size),
        page_token: None,
        history_length: None,
        timeline_length: None,
        status_timestamp_after: None,
        include_artifacts: None,
        tenant: None,
    }
}
fn task(id: &str, timestamp: Option<&str>) -> Task {
    Task {
        id: id.into(),
        context_id: "context".into(),
        generation: 1,
        status: TaskStatus {
            state: TaskState::Working,
            message: None,
            timestamp: timestamp.map(|s| s.parse().unwrap()),
        },
        history: None,
        artifacts: None,
        timeline: None,
        metadata: None,
    }
}
#[tokio::test]
async fn narrowed_credentials_cannot_read_the_bound_exchange_through_an_ack() {
    let handler = DefaultRequestHandler::new(Agent(false), InMemoryTaskStore::new())
        .with_authorizer(Policy)
        .with_capabilities(AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        });
    let LiveResponse::Stream(mut owner) = handler
        .send_live_request(&params("admin"), live(send("secret", None)))
        .await
        .unwrap()
    else {
        panic!()
    };
    let StreamResponse::Task(secret) = owner.next().await.unwrap().unwrap() else {
        panic!()
    };
    owner.next().await.unwrap().unwrap();
    assert!(
        handler
            .send_live_request(&params("limited"), live(send("attack", Some("public"))))
            .await
            .is_err()
    );
    // Rejection neither consumes the exchange nor records the attack.
    let LiveResponse::Ack(snapshot) = handler
        .send_live_request(&params("admin"), live(send("allowed", Some(&secret.id))))
        .await
        .unwrap()
    else {
        panic!()
    };
    let history = snapshot.history.unwrap();
    assert!(history.iter().any(|m| m.text() == Some("allowed")));
    assert!(!history.iter().any(|m| m.text() == Some("attack")));
}
#[tokio::test]
async fn list_filters_before_counting_and_cursor_creation() {
    let store = InMemoryTaskStore::new();
    store
        .create(task("public", Some("2026-01-01T00:00:00Z")))
        .await
        .unwrap();
    store
        .create(task("secret", Some("2026-10-01T00:00:00Z")))
        .await
        .unwrap();
    let handler = DefaultRequestHandler::new(Agent(true), store).with_authorizer(Policy);
    let page = handler
        .list_tasks(&params("limited"), list_request(1))
        .await
        .unwrap();
    assert_eq!(page.total_size, 1);
    assert_eq!(page.tasks.len(), 1);
    assert_eq!(page.tasks[0].id, "public");
    assert!(page.next_page_token.is_empty());
}
#[tokio::test]
async fn required_extensions_are_checked_on_every_operation() {
    let handler = DefaultRequestHandler::new(Agent(true), InMemoryTaskStore::new())
        .with_capabilities(AgentCapabilities {
            extensions: Some(vec![AgentExtension {
                uri: "urn:required".into(),
                required: Some(true),
                description: None,
                params: None,
            }]),
            ..Default::default()
        });
    let mut headers = params("admin");
    assert_eq!(
        handler
            .send_message(&headers, send("hello", None))
            .await
            .unwrap_err()
            .code,
        error_code::EXTENSION_SUPPORT_REQUIRED
    );
    assert_eq!(
        handler
            .list_tasks(&headers, list_request(10))
            .await
            .unwrap_err()
            .code,
        error_code::EXTENSION_SUPPORT_REQUIRED
    );
    headers.insert(
        "a2a-extensions".into(),
        vec!["urn:optional, urn:required".into(), "urn:another".into()],
    );
    assert!(
        handler
            .send_message(&headers, send("hello", None))
            .await
            .is_ok()
    );
    assert!(handler.list_tasks(&headers, list_request(10)).await.is_ok());
}
#[tokio::test]
async fn invalid_messages_are_rejected_across_send_modes() {
    let handler = DefaultRequestHandler::new(Agent(true), InMemoryTaskStore::new())
        .with_capabilities(AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        });
    for value in [
        serde_json::json!({}),
        serde_json::json!({"message":{"messageId":"","role":"ROLE_USER","parts":[{"text":"hi"}]}}),
        serde_json::json!({"message":{"messageId":"id","role":"ROLE_USER","parts":[]}}),
        serde_json::json!({"message":{"messageId":"id","role":"ROLE_UNSPECIFIED","parts":[{"text":"hi"}]}}),
    ] {
        let request = a2a_pb::protojson_conv::from_value::<SendMessageRequest>(value).unwrap();
        assert_eq!(
            handler
                .send_message(&params("admin"), request.clone())
                .await
                .unwrap_err()
                .code,
            error_code::INVALID_PARAMS
        );
        assert!(
            handler
                .send_streaming_message(&params("admin"), request.clone())
                .await
                .is_err()
        );
        assert!(
            handler
                .send_live_message(
                    &params("admin"),
                    stream::iter([live(request.clone())]).boxed()
                )
                .await
                .is_err()
        );
        assert!(
            handler
                .send_live_request(&params("admin"), live(request))
                .await
                .is_err()
        );
    }
}
#[tokio::test]
async fn push_configuration_requires_an_existing_authorized_task() {
    let store = InMemoryTaskStore::new();
    store.create(task("public", None)).await.unwrap();
    let handler = DefaultRequestHandler::new(Agent(true), store)
        .with_authorizer(Policy)
        .with_push_config_store(InMemoryPushConfigStore::new());
    let config = TaskPushNotificationConfig {
        task_id: "absent".into(),
        tenant: None,
        protocol_version: None,
        id: None,
        url: "https://example.com/hook".into(),
        token: None,
        authentication: None,
    };
    assert_eq!(
        handler
            .create_push_config(&params("admin"), config.clone())
            .await
            .unwrap_err()
            .code,
        error_code::TASK_NOT_FOUND
    );
    assert!(
        handler
            .create_push_config(&params("limited"), config.clone())
            .await
            .unwrap_err()
            .message
            .contains("denied")
    );
    assert!(
        handler
            .create_push_config(
                &params("limited"),
                TaskPushNotificationConfig {
                    task_id: "public".into(),
                    ..config
                }
            )
            .await
            .is_ok()
    );
}
#[test]
fn version_requires_numeric_major_minor_and_optional_patch() {
    for version in [
        "1.not-a-version",
        "1",
        "1.",
        "1.2.3.4",
        "1.-2",
        "1.+2",
        "2.0",
        "",
    ] {
        let headers = ServiceParams::from([(SVC_PARAM_VERSION.into(), vec![version.into()])]);
        assert!(
            a2a_server::middleware::check_a2a_version(&headers).is_err(),
            "{version}"
        );
    }
    let duplicates =
        ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into(), "1.bad".into()])]);
    assert!(a2a_server::middleware::check_a2a_version(&duplicates).is_err());
    for version in ["1.0", "1.1", "1.42", "1.1.3"] {
        let headers = ServiceParams::from([(SVC_PARAM_VERSION.into(), vec![version.into()])]);
        assert!(
            a2a_server::middleware::check_a2a_version(&headers).is_ok(),
            "{version}"
        );
    }
}
#[tokio::test]
async fn timestamp_cursors_are_stable_when_newer_tasks_are_inserted() {
    let store = InMemoryTaskStore::new();
    for (id, timestamp) in [
        ("a-old", Some("2026-01-01T00:00:00Z")),
        ("z-new", Some("2026-10-01T00:00:00Z")),
        ("b-tie", Some("2026-01-01T00:00:00Z")),
        ("undated", None),
    ] {
        store.create(task(id, timestamp)).await.unwrap();
    }
    let mut request = list_request(1);
    let first = store.list(&request).await.unwrap();
    assert_eq!(first.tasks[0].id, "z-new");
    request.page_token = Some(first.next_page_token);
    store
        .create(task("newest", Some("2026-10-30T00:00:00Z")))
        .await
        .unwrap();
    let mut remainder = Vec::new();
    loop {
        let page = store.list(&request).await.unwrap();
        remainder.extend(page.tasks.into_iter().map(|t| t.id));
        if page.next_page_token.is_empty() {
            break;
        }
        request.page_token = Some(page.next_page_token);
    }
    assert_eq!(remainder, ["a-old", "b-tie", "undated"]);
}

#[tokio::test]
async fn custom_stores_fail_closed_until_scoped_listing_is_implemented() {
    struct UnscopedStore;
    #[async_trait::async_trait]
    impl TaskStore for UnscopedStore {
        async fn create(&self, _: Task) -> Result<TaskVersion, A2AError> {
            unreachable!()
        }
        async fn update(&self, _: Task) -> Result<TaskVersion, A2AError> {
            unreachable!()
        }
        async fn get(&self, _: &str) -> Result<Option<Task>, A2AError> {
            unreachable!()
        }
        async fn begin_cancel(&self, _: &str) -> Result<Task, A2AError> {
            unreachable!()
        }
        async fn list(&self, _: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
            panic!("an authorized list must not call the unscoped store method")
        }
    }
    let handler = DefaultRequestHandler::new(Agent(true), UnscopedStore).with_authorizer(Policy);
    assert_eq!(
        handler
            .list_tasks(&params("limited"), list_request(1))
            .await
            .unwrap_err()
            .code,
        error_code::UNSUPPORTED_OPERATION
    );
}
