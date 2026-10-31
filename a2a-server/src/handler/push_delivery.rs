// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

use a2a::*;

pub(super) async fn send_push_notifications(
    task_id: &str,
    push_config_store: Option<&dyn crate::PushConfigStore>,
    push_sender: Option<&crate::HttpPushSender>,
    event: &StreamResponse,
) -> Result<(), A2AError> {
    let Some(push_config_store) = push_config_store else {
        return Ok(());
    };
    let Some(push_sender) = push_sender else {
        return Ok(());
    };

    let configs = push_config_store.list(task_id).await?;
    for config in configs {
        push_sender.send_push(&config, event.clone()).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn test_push_config_not_supported_without_store() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let result = handler
            .create_push_config(
                &params,
                TaskPushNotificationConfig {
                    protocol_version: None,
                    task_id: "t1".into(),
                    url: "http://example.com".into(),
                    id: None,
                    token: None,
                    authentication: None,
                    tenant: None,
                },
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_list_push_configs_invalid_page_token_errors() {
        install_crypto_provider();
        let handler = make_handler_with_push_configs();
        seed_task(&handler, "t1", "c1").await;
        let params = ServiceParams::new();
        handler
            .create_push_config(
                &params,
                TaskPushNotificationConfig {
                    protocol_version: None,
                    task_id: "t1".into(),
                    url: "https://example.com/callback".into(),
                    id: Some("cfg-1".into()),
                    token: None,
                    authentication: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let result = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: None,
                    page_token: Some("not-a-number".into()),
                    tenant: None,
                },
            )
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn test_list_push_configs_page_size_cap_bounds_a_custom_store() {
        install_crypto_provider();
        // InMemoryPushConfigStore caps a task at 50 configs, below the page
        // cap of 100, so it can never reach the clamp. A store without that
        // limit can, and is what the clamp exists for.
        let handler = DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
            .with_push_config_store(PagingPushConfigStore::new(MAX_PAGE_SIZE * 3));

        let resp = handler
            .list_push_configs(
                &ServiceParams::new(),
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: Some(1_000_000),
                    page_token: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(resp.configs.len(), MAX_PAGE_SIZE);
        // The remainder must stay reachable, or the cap silently truncates.
        assert_eq!(resp.next_page_token.as_deref(), Some("100"));
    }

    #[tokio::test]
    async fn test_list_push_configs_page_size_is_capped() {
        install_crypto_provider();
        let handler = make_handler_with_push_configs();
        seed_task(&handler, "t1", "c1").await;
        let params = ServiceParams::new();
        // Keep the count below InMemoryPushConfigStore's per-task cap
        // (MAX_PUSH_CONFIGS_PER_TASK), which rejects the 51st config.
        for i in 0..40 {
            handler
                .create_push_config(
                    &params,
                    TaskPushNotificationConfig {
                        protocol_version: None,
                        task_id: "t1".into(),
                        url: format!("https://example.com/callback/{i}"),
                        id: Some(format!("cfg-{i:03}")),
                        token: None,
                        authentication: None,
                        tenant: None,
                    },
                )
                .await
                .unwrap();
        }

        // Requesting a huge page size must not error and returns everything
        // available. This does NOT exercise the cap -- 40 < MAX_PAGE_SIZE, so
        // a capped and an uncapped implementation agree here. The cap itself
        // is pinned by test_list_push_configs_page_size_cap_bounds_a_custom_store,
        // which is the only route that can reach it.
        let resp = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: Some(1000),
                    page_token: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(resp.configs.len(), 40);
    }

    #[tokio::test]
    async fn test_push_config_crud_with_store() {
        install_crypto_provider();
        let handler = make_handler_with_push_configs();
        seed_task(&handler, "t1", "c1").await;
        let params = ServiceParams::new();

        let created = handler
            .create_push_config(
                &params,
                TaskPushNotificationConfig {
                    protocol_version: None,
                    task_id: "t1".into(),
                    url: "https://example.com/first".into(),
                    id: Some("cfg-1".into()),
                    token: None,
                    authentication: None,
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(created.id.as_deref(), Some("cfg-1"));

        handler
            .create_push_config(
                &params,
                TaskPushNotificationConfig {
                    protocol_version: None,
                    task_id: "t1".into(),
                    url: "https://example.com/second".into(),
                    id: Some("cfg-2".into()),
                    token: None,
                    authentication: None,
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();

        let fetched = handler
            .get_push_config(
                &params,
                GetTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    id: "cfg-1".into(),
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(fetched.url, "https://example.com/first");

        let first_page = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: Some(1),
                    page_token: Some("0".into()),
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(first_page.configs.len(), 1);
        assert_eq!(first_page.configs[0].id.as_deref(), Some("cfg-1"));
        assert_eq!(first_page.next_page_token.as_deref(), Some("1"));

        let default_page = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: Some(0),
                    page_token: None,
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(default_page.configs.len(), 2);
        assert!(default_page.next_page_token.is_none());

        handler
            .delete_push_config(
                &params,
                DeleteTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    id: "cfg-1".into(),
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();

        let remaining = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: None,
                    page_token: None,
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(remaining.configs.len(), 1);
        assert_eq!(remaining.configs[0].id.as_deref(), Some("cfg-2"));
    }

    #[tokio::test]
    async fn test_send_message_request_push_config_delivers_webhooks() {
        install_crypto_provider();
        let (url, mut receiver, shutdown_tx, server) = start_push_webhook().await;
        let handler = make_push_delivery_handler();
        let params = ServiceParams::new();
        let mut message = make_message();
        message.task_id = Some("t-push-request".into());
        message.context_id = Some("c-push-request".into());
        seed_task(&handler, "t-push-request", "c-push-request").await;

        let response = handler
            .send_message(
                &params,
                SendMessageRequest {
                    artifact_updates: Vec::new(),
                    message,
                    configuration: Some(SendMessageConfiguration {
                        if_generation_match: None,
                        timeline_length: None,
                        accepted_output_modes: None,
                        task_push_notification_config: Some(TaskPushNotificationConfig {
                            protocol_version: None,
                            task_id: String::new(),
                            url: url.clone(),
                            id: Some("cfg-request".into()),
                            token: Some("notify-token".into()),
                            authentication: Some(AuthenticationInfo {
                                scheme: "bearer".into(),
                                credentials: Some("secret-token".into()),
                            }),
                            tenant: None,
                        }),
                        history_length: None,
                        return_immediately: None,
                    }),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        match response {
            SendMessageResponse::Task(task) => assert_eq!(task.status.state, TaskState::Completed),
            _ => panic!("expected Task response"),
        }

        let saved = handler
            .get_push_config(
                &params,
                GetTaskPushNotificationConfigRequest {
                    task_id: "t-push-request".into(),
                    id: "cfg-request".into(),
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(saved.url, url);

        let first = next_push(&mut receiver).await;
        assert_eq!(first.authorization.as_deref(), Some("Bearer secret-token"));
        assert_eq!(first.notification_token.as_deref(), Some("notify-token"));
        match first.event {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.task_id, "t-push-request");
                assert_eq!(update.status.state, TaskState::Working);
            }
            _ => panic!("expected status update push"),
        }

        let second = next_push(&mut receiver).await;
        assert_eq!(second.authorization.as_deref(), Some("Bearer secret-token"));
        assert_eq!(second.notification_token.as_deref(), Some("notify-token"));
        match second.event {
            StreamResponse::Task(task) => {
                assert_eq!(task.id, "t-push-request");
                assert_eq!(task.status.state, TaskState::Completed);
            }
            _ => panic!("expected final task push"),
        }

        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_send_message_stored_push_config_delivers_webhooks() {
        install_crypto_provider();
        let (url, mut receiver, shutdown_tx, server) = start_push_webhook().await;
        let handler = make_push_delivery_handler();
        let params = ServiceParams::new();

        seed_task(&handler, "t-push-stored", "c-push-stored").await;
        handler
            .create_push_config(
                &params,
                TaskPushNotificationConfig {
                    protocol_version: None,
                    task_id: "t-push-stored".into(),
                    url: url.clone(),
                    id: Some("cfg-stored".into()),
                    token: Some("stored-token".into()),
                    authentication: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let mut message = make_message();
        message.task_id = Some("t-push-stored".into());
        message.context_id = Some("c-push-stored".into());
        let response = handler
            .send_message(
                &params,
                SendMessageRequest {
                    artifact_updates: Vec::new(),
                    message,
                    configuration: None,
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        match response {
            SendMessageResponse::Task(task) => assert_eq!(task.status.state, TaskState::Completed),
            _ => panic!("expected Task response"),
        }

        let first = next_push(&mut receiver).await;
        assert_eq!(first.notification_token.as_deref(), Some("stored-token"));
        match first.event {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.task_id, "t-push-stored");
                assert_eq!(update.status.state, TaskState::Working);
            }
            _ => panic!("expected status update push"),
        }

        let second = next_push(&mut receiver).await;
        assert_eq!(second.notification_token.as_deref(), Some("stored-token"));
        match second.event {
            StreamResponse::Task(task) => {
                assert_eq!(task.id, "t-push-stored");
                assert_eq!(task.status.state, TaskState::Completed);
            }
            _ => panic!("expected final task push"),
        }

        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[test]
    fn test_with_push_config_store_enables_push_capability() {
        install_crypto_provider();
        let handler = make_handler_with_push_configs();
        assert_eq!(handler.capabilities.push_notifications, Some(true));
    }

    #[tokio::test]
    async fn test_push_refused_when_the_card_declares_it_absent() {
        // A store *is* configured; the card declaring `false` is what refuses.
        // Before #203 this combination served push regardless.
        let handler = make_handler_with_push_configs().with_capabilities(caps(None, Some(false)));
        let params = ServiceParams::new();

        let error = handler
            .create_push_config(&params, push_config("t1"))
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);

        let error = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: None,
                    page_token: None,
                    tenant: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);
    }

    /// `with_push_notifications` defaults the capability the same way as
    /// `with_push_config_store`, and likewise must not override an explicit
    /// `false`. It had no unit test before #203 — only an example — so the
    /// two builders could have drifted apart unnoticed.
    #[tokio::test]
    async fn test_with_push_notifications_defaults_the_capability() {
        let params = ServiceParams::new();

        let handler = make_handler().with_push_notifications(
            InMemoryPushConfigStore::new(),
            crate::HttpPushSender::new(None),
        );
        seed_task(&handler, "t1", "c1").await;
        assert_eq!(handler.capabilities.push_notifications, Some(true));
        assert!(
            handler
                .create_push_config(&params, push_config("t1"))
                .await
                .is_ok()
        );

        let handler = make_handler()
            .with_capabilities(caps(None, Some(false)))
            .with_push_notifications(
                InMemoryPushConfigStore::new(),
                crate::HttpPushSender::new(None),
            );
        assert_eq!(handler.capabilities.push_notifications, Some(false));
        let error = handler
            .create_push_config(&params, push_config("t1"))
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);
    }

    /// A store that was never supplied still reports the same code — that is
    /// the deployment state, distinct from the card's declaration.
    #[tokio::test]
    async fn test_missing_store_still_reports_push_unsupported() {
        let handler = make_handler();
        let error = handler
            .create_push_config(&ServiceParams::new(), push_config("t1"))
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);
    }
}
