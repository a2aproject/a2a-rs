// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Event sources and destinations consumed by the shared delivery runner.

use super::protocol::is_at_least_1_1;
use super::push_delivery::QueuedNotifications;
use super::response::Events;
use a2a::*;
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

pub(super) type FailureSignal = watch::Sender<Option<A2AError>>;

pub(super) struct Notification {
    pub(super) store: Arc<dyn crate::PushConfigStore>,
    pub(super) sender: Arc<crate::HttpPushSender>,
    pub(super) failure: Option<FailureSignal>,
}

pub(super) struct DeliveryEvent {
    pub(super) result: Result<StreamResponse, A2AError>,
    pub(super) notification: Option<Notification>,
}

impl From<Result<StreamResponse, A2AError>> for DeliveryEvent {
    fn from(result: Result<StreamResponse, A2AError>) -> Self {
        Self {
            result,
            notification: None,
        }
    }
}

pub(super) enum EventSource {
    Subscription(Events),
    Notifications(QueuedNotifications),
}

impl From<Events> for EventSource {
    fn from(events: Events) -> Self {
        Self::Subscription(events)
    }
}

impl EventSource {
    pub(super) async fn next(&mut self) -> Option<DeliveryEvent> {
        match self {
            Self::Subscription(events) => events.next().await.map(Into::into),
            Self::Notifications(queue) => queue.next().await,
        }
    }
}

#[derive(Clone)]
pub(super) enum DeliverySink {
    Stream(mpsc::Sender<Result<StreamResponse, A2AError>>),
    Webhook(TaskId),
}

impl DeliverySink {
    pub(super) async fn closed(&self) {
        match self {
            Self::Stream(sender) => sender.closed().await,
            Self::Webhook(_) => std::future::pending().await,
        }
    }

    /// A delivery failure can notify its execution without stopping delivery
    /// to the task's other queued notifications or client response streams.
    pub(super) async fn send(&self, item: DeliveryEvent) -> bool {
        match self {
            Self::Stream(sender) => sender.send(item.result).await.is_ok(),
            Self::Webhook(task_id) => {
                if let (Ok(event), Some(notification)) = (item.result, item.notification) {
                    if let Err(error) = notification.deliver(task_id, event).await {
                        tracing::warn!(%task_id, %error, "could not push a notification");
                        notification.signal_failure(error);
                    }
                }
                true
            }
        }
    }

    pub(super) async fn report_panic(&self) {
        let error = A2AError::internal("the response runner failed");
        match self {
            Self::Stream(sender) => {
                let _ = sender.send(Err(error)).await;
            }
            Self::Webhook(task_id) => {
                tracing::error!(%task_id, %error, "webhook delivery runner panicked")
            }
        }
    }
}

impl Notification {
    fn signal_failure(&self, error: A2AError) {
        if self.sender.fail_on_error()
            && let Some(signal) = &self.failure
        {
            signal.send_replace(Some(error));
        }
    }

    async fn deliver(&self, task_id: &str, event: StreamResponse) -> Result<(), A2AError> {
        for config in self.store.list(task_id).await? {
            if matches!(event, StreamResponse::MessageUpdate(_))
                && !is_at_least_1_1(config.protocol_version.as_deref().unwrap_or("1.0"))
            {
                continue;
            }
            if let Err(error) = self.sender.send_push(&config, event.clone()).await {
                tracing::warn!(%task_id, config_id = ?config.id, %error, "webhook delivery failed");
                // Wake immediately when configured to fail; a later slow
                // endpoint must not defer the execution's failure signal.
                self.signal_failure(error);
            }
        }
        Ok(())
    }
}
