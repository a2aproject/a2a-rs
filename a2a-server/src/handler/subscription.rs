// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Choose subscription cursors and visible events for each response mode.

use super::delivery::{Delivery, StartMode};
use super::events::{ExecutionReceiver, subscription_stream};
use super::response::Events;
use a2a::*;
use futures::StreamExt;

/// Validated task attachment before choosing how a response delivers it.
/// The receiver and snapshot share the cursor captured during lookup.
pub(super) struct SubscriptionAttachment {
    pub(super) task_id: TaskId,
    pub(super) receiver: Option<ExecutionReceiver>,
    pub(super) opening: Option<Task>,
    pub(super) sequence: u64,
    pub(super) seen_generation: i64,
}

impl SubscriptionAttachment {
    pub(super) async fn capture(
        handler: &super::DefaultRequestHandler,
        req: SubscribeToTaskRequest,
    ) -> Result<SubscriptionAttachment, A2AError> {
        // Earlier admission commands have completed before this lookup. An
        // unpublished lazy reservation is still private to its opening caller.
        if let Some(active) = handler.execution_manager.get(&req.id).await {
            if active.uncreated.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(A2AError::task_not_found(&req.id));
            }
        }
        let mut resubscribed = handler.execution_manager.resubscribe(&req.id).await;
        // An abandoned reservation with no snapshot has no subscribable Task.
        if matches!(resubscribed, Some((_, None, _))) {
            resubscribed = None;
        }
        if let Some((receiver, snapshot_task, sequence)) = resubscribed {
            if snapshot_task
                .as_ref()
                .is_some_and(|task| task.status.state.is_terminal())
            {
                return Err(A2AError::unsupported_operation(format!(
                    "task {} has already reached a terminal state",
                    req.id
                )));
            }
            // A client that already holds the task's current generation has
            // missed nothing, so it resumes without a snapshot. One that is
            // behind cannot be replayed to -- events are not retained -- and
            // falls back to a snapshot, which the spec allows.
            let snapshot_task = match (req.last_seen_generation, snapshot_task) {
                (Some(last_seen), Some(task))
                    if task.generation == last_seen && !task.status.state.is_terminal() =>
                {
                    None
                }
                (_, snapshot) => snapshot,
            };
            return Ok(SubscriptionAttachment {
                task_id: req.id,
                receiver: Some(receiver),
                opening: snapshot_task,
                sequence,
                seen_generation: req.last_seen_generation.unwrap_or(0),
            });
        }

        // A registry miss also happens once a task finishes and its
        // execution is cleaned up, so it doesn't by itself mean the task
        // never existed -- check the store to tell the two apart before
        // reporting TaskNotFoundError.
        if let Some(task) = handler.task_store.get(&req.id).await? {
            if task.status.state.is_terminal() {
                return Err(A2AError::unsupported_operation(format!(
                    "task {} has already reached a terminal state",
                    req.id
                )));
            }
            let current = req.last_seen_generation == Some(task.generation);
            return Ok(SubscriptionAttachment {
                task_id: req.id,
                receiver: None,
                opening: (!current).then_some(task),
                sequence: 0,
                seen_generation: req.last_seen_generation.unwrap_or(0),
            });
        }
        Err(A2AError::task_not_found(&req.id))
    }
}

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

    pub(super) fn wants_message_updates(self) -> bool {
        self.wants_message_updates
    }

    pub(super) fn response_id(self) -> Option<uuid::Uuid> {
        self.response_id
    }

    pub(super) fn stream(
        self,
        receiver: ExecutionReceiver,
        opening: Option<Task>,
        sequence: u64,
    ) -> Events {
        // A streaming response owns a Task subscription, not an invocation
        // subscription. Closing/reopening at a handoff can discard queued events.
        let receiver = if self.mode == StartMode::Blocking {
            receiver
        } else {
            receiver.follow_task()
        };
        self.event_stream(receiver, opening, sequence)
    }

    fn event_stream(
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
            Some(receiver)
                if self.mode == StartMode::Streaming
                    && matches!(attachment, DeliveryAttachment::Opening { .. }) =>
            {
                self.event_stream(receiver.opening_message(), None, sequence)
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn immediate_unary_joins_return_a_snapshot_without_waiting_for_executor_events() {
        use crate::handler::test_support::task_with_history;
        let task = task_with_history("task", "context", 0);
        let active = super::super::events::ActiveExecution::new(Some(task.clone()));
        let delivery = Delivery::Injected {
            task: task.clone(),
            receiver: active.subscribe().await,
            sequence_before: 0,
            sequence_after: 0,
        };
        let mut subscription = SubscriptionPolicy::new(StartMode::Blocking, false).delivery(
            delivery,
            DeliveryAttachment::Opening {
                return_immediately: true,
            },
        );
        assert!(
            matches!(subscription.events.next().await.unwrap().unwrap(), StreamResponse::Task(snapshot) if snapshot == task)
        );
        assert!(subscription.events.next().await.is_none());
    }
}
