// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Choose subscription cursors and visible events for each response mode.

use super::delivery::{Delivery, StartMode};
use super::events::{ExecutionReceiver, subscription_stream};
use super::response::Events;
use a2a::*;
use futures::StreamExt;

#[derive(Clone, Copy)]
pub(super) struct SubscriptionPolicy {
    mode: StartMode,
    wants_message_updates: bool,
    response_id: Option<uuid::Uuid>,
}

pub(super) enum DeliveryAttachment {
    /// Start a response; streaming modes announce the input in their opening snapshot.
    Opening { return_immediately: bool },
    /// An existing response must observe the follow-up's input events.
    FollowUp,
}

pub(super) struct DeliverySubscription {
    pub(super) task: Task,
    pub(super) events: Events,
    /// Inputs recorded before the new turn had any subscribers.
    pub(super) recorded: Vec<StreamResponse>,
}

impl SubscriptionPolicy {
    pub(super) fn new(mode: StartMode, wants_message_updates: bool) -> Self {
        Self {
            mode,
            wants_message_updates,
            response_id: None,
        }
    }

    pub(super) fn for_response(mut self, response_id: Option<uuid::Uuid>) -> Self {
        self.response_id = response_id;
        self
    }

    pub(super) fn stream(
        self,
        receiver: ExecutionReceiver,
        opening: Option<Task>,
        sequence: u64,
    ) -> Events {
        subscription_stream(
            receiver,
            opening,
            sequence,
            self.mode == StartMode::Blocking,
            self.mode != StartMode::Blocking && self.wants_message_updates,
            self.mode == StartMode::Bidi,
            self.response_id,
        )
    }

    pub(super) fn delivery(
        self,
        delivery: Delivery,
        attachment: DeliveryAttachment,
    ) -> DeliverySubscription {
        let (task, receiver, sequence, recorded) = match delivery {
            Delivery::Started {
                task,
                receiver,
                recorded,
            } => (task, Some(receiver), 0, recorded),
            Delivery::Injected {
                task,
                receiver,
                sequence_before,
                sequence_after,
            } => {
                let immediate = self.mode == StartMode::Blocking
                    && matches!(
                        attachment,
                        DeliveryAttachment::Opening {
                            return_immediately: true
                        }
                    );
                let sequence = if self.mode == StartMode::Blocking
                    || matches!(attachment, DeliveryAttachment::FollowUp)
                {
                    sequence_before
                } else {
                    sequence_after
                };
                (task, (!immediate).then_some(receiver), sequence, Vec::new())
            }
        };
        let events = match receiver {
            Some(receiver) => self.stream(receiver, None, sequence),
            None => {
                let event = StreamResponse::Task(task.clone());
                futures::stream::once(async move { Ok(event) }).boxed()
            }
        };
        DeliverySubscription {
            task,
            events,
            recorded,
        }
    }
}
