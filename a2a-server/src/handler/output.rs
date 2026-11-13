// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Client response sources and delivery, independent of webhook workers.

use super::response::Events;
use a2a::*;
use futures::StreamExt;
use tokio::sync::{mpsc, watch};

pub(super) type FailureSignal = watch::Sender<Option<A2AError>>;

pub(super) struct DeliveryEvent {
    pub(super) result: Result<StreamResponse, A2AError>,
}

impl From<Result<StreamResponse, A2AError>> for DeliveryEvent {
    fn from(result: Result<StreamResponse, A2AError>) -> Self {
        Self { result }
    }
}

pub(super) enum EventSource {
    Subscription(Events),
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
        }
    }
}

#[derive(Clone)]
pub(super) enum DeliverySink {
    Stream(mpsc::Sender<Result<StreamResponse, A2AError>>),
}
impl DeliverySink {
    pub(super) async fn closed(&self) {
        match self {
            Self::Stream(sender) => sender.closed().await,
        }
    }
    pub(super) async fn send(&self, item: DeliveryEvent) -> bool {
        match self {
            Self::Stream(sender) => sender.send(item.result).await.is_ok(),
        }
    }
    pub(super) async fn report_panic(&self) {
        let error = A2AError::internal("the response runner failed");
        match self {
            Self::Stream(sender) => {
                let _ = sender.send(Err(error)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn runner_panic_is_reported_and_a_disconnected_consumer_stops_delivery() {
        let (sender, mut receiver) = mpsc::channel(1);
        let sink = DeliverySink::Stream(sender);
        sink.report_panic().await;
        let error = receiver.recv().await.unwrap().unwrap_err();
        assert_eq!(error.code, a2a::error_code::INTERNAL_ERROR);
        assert_eq!(error.message, "the response runner failed");
        drop(receiver);
        sink.closed().await;
        assert!(!sink.send(DeliveryEvent::from(Err(error))).await);
    }
}
