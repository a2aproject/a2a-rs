// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Prepare response subscriptions, initial events, and completion settings.

use super::delivery::Delivery;
use super::events::{ExecutionReceiver, subscription_stream};
#[cfg(test)]
use super::protocol::StartMode;
use super::protocol::{self, DeliveryAttachment, InputCursor, OpeningPolicy, ResponsePolicy};
use super::response::Events;
use super::{task_handle::TaskHandle, task_service::TaskService};
use a2a::*;
use futures::{StreamExt, stream::BoxStream};
use std::sync::Arc;

/// A direct reply has no task to wait for. Task replies retain their completion stream.
pub(super) enum MessageResponse {
    Message(Message),
    Task(TaskResponse),
}

impl MessageResponse {
    pub(super) async fn receive(
        tasks: Arc<TaskService>,
        task: TaskHandle,
        mut events: Events,
    ) -> Result<Self, A2AError> {
        // Retention may remove a terminal task before the caller reads its response.
        if let Some(StreamResponse::Message(message)) = events.next().await.transpose()? {
            return Ok(Self::Message(message));
        }
        Ok(Self::Task(TaskResponse {
            tasks,
            task,
            events,
        }))
    }
}

pub(super) struct TaskResponse {
    tasks: Arc<TaskService>,
    task: TaskHandle,
    events: Events,
}

impl TaskResponse {
    /// The shared runner finishes at completion or an interruption such as input-required.
    pub(super) async fn wait(mut self) -> Result<SendMessageResponse, A2AError> {
        while let Some(event) = self.events.next().await {
            if let StreamResponse::Message(message) = event? {
                return Ok(SendMessageResponse::Message(message));
            }
        }
        Ok(SendMessageResponse::Task(self.snapshot().await?))
    }

    pub(super) async fn snapshot(self) -> Result<Task, A2AError> {
        match self.tasks.load(self.task.id()).await {
            Ok(task) => Ok(task),
            Err(error) if error.code == error_code::TASK_NOT_FOUND => match self.task.snapshot() {
                Some(task) if task.status.state.is_terminal() => Ok(task),
                _ => Err(error),
            },
            Err(error) => Err(error),
        }
    }
}

pub(super) struct PreparedResponse {
    pub(super) task: TaskHandle,
    pub(super) events: Option<Events>,
    pub(super) opening: Option<Task>,
    /// Reused for the opening, followups, and later executor invocations.
    pub(super) settings: ResponsePolicy,
    pub(super) seen_generation: i64,
}

impl PreparedResponse {
    /// Translate delivery into one initial subscription. A caller joining a
    /// running turn starts before or after its own input events according to
    /// the response mode; a new turn starts at its first event.
    pub(super) fn message(delivery: Delivery, settings: ResponsePolicy) -> Self {
        let handle = delivery.handle();
        let subscription = attach_delivery(settings, delivery, DeliveryAttachment::Opening);
        let (task, events) = (subscription.task, subscription.events);
        let (opening, events) = match settings.opening() {
            OpeningPolicy::None => (None, events),
            OpeningPolicy::OnFirstEvent => (None, with_opening_task(events, task)),
            OpeningPolicy::Snapshot => (Some(task), events),
        };
        Self {
            task: handle,
            events: Some(events),
            opening,
            settings,
            seen_generation: 0,
        }
    }

    pub(super) fn subscription(
        attachment: SubscriptionAttachment,
        settings: ResponsePolicy,
    ) -> Self {
        Self {
            task: attachment.task,
            events: attachment
                .receiver
                .map(|receiver| event_stream(settings, receiver, None, attachment.sequence)),
            opening: attachment.opening,
            settings,
            seen_generation: attachment.seen_generation,
        }
    }
}

/// Opens a stream with the task, as A2A requires of a streaming send
/// (§3.1.2). An agent that answers with a message alone gets a stream that is
/// that message and nothing else, and one whose first event is already a task
/// snapshot needs no other opening. Otherwise (a status or artifact update
/// first) the server's own snapshot goes first, so a client never has to guess
/// the task's state from a partial event.
pub(super) fn with_opening_task(
    stream: BoxStream<'static, Result<StreamResponse, A2AError>>,
    opening: Task,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    Box::pin(
        futures::stream::once(async move {
            let mut stream = stream;
            let Some(first) = stream.next().await else {
                // The executor said nothing a client should see (it only
                // restated the task, or ended): the stream is the task.
                return futures::stream::iter([Ok(StreamResponse::Task(opening))]).boxed();
            };
            let head = if protocol::needs_opening_task(&first) {
                vec![Ok(StreamResponse::Task(opening)), first]
            } else {
                vec![first]
            };
            futures::stream::iter(head).chain(stream).boxed()
        })
        .flatten(),
    )
}

/// Validated task attachment before choosing how a response delivers it.
/// The receiver and snapshot share the cursor captured during lookup.
pub(super) struct SubscriptionAttachment {
    pub(super) task: TaskHandle,
    pub(super) receiver: Option<ExecutionReceiver>,
    pub(super) opening: Option<Task>,
    pub(super) sequence: u64,
    pub(super) seen_generation: i64,
}

pub(super) struct DeliverySubscription {
    pub(super) task: Task,
    pub(super) events: Events,
    /// Inputs recorded before the new turn had any subscribers.
    pub(super) recorded: Vec<StreamResponse>,
}

/// Attach the protocol-selected cursor to the actual task event receiver.
pub(super) fn event_stream(
    policy: ResponsePolicy,
    receiver: ExecutionReceiver,
    opening: Option<Task>,
    sequence: u64,
) -> Events {
    let receiver = if policy.follows_task() {
        receiver.follow_task()
    } else {
        receiver
    };
    subscription_stream_with_policy(policy, receiver, opening, sequence)
}

fn subscription_stream_with_policy(
    policy: ResponsePolicy,
    receiver: ExecutionReceiver,
    opening: Option<Task>,
    sequence: u64,
) -> Events {
    let filter = policy.event_filter();
    subscription_stream(receiver, opening, sequence, filter)
}

pub(super) fn attach_delivery(
    policy: ResponsePolicy,
    delivery: Delivery,
    attachment: DeliveryAttachment,
) -> DeliverySubscription {
    let plan = policy.delivery_plan(attachment);
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
            let sequence = match plan.cursor {
                InputCursor::Before => sequence_before,
                InputCursor::After => sequence_after,
            };
            (
                task,
                plan.attach_receiver.then_some(receiver),
                sequence,
                Vec::new(),
            )
        }
    };
    let events = match receiver {
        Some(receiver) if plan.wait_for_first_output => {
            subscription_stream_with_policy(policy, receiver.opening_message(), None, sequence)
        }
        Some(receiver) => event_stream(policy, receiver, None, sequence),
        None => futures::stream::once({
            let task = task.clone();
            async move { Ok(StreamResponse::Task(task)) }
        })
        .boxed(),
    };
    DeliverySubscription {
        task,
        events,
        recorded,
    }
}

#[cfg(test)]
mod task_response_tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn task_completion_propagates_errors_after_the_initial_task() {
        let handler = make_handler();
        seed_task(&handler, "task", "context").await;
        let task = handler.tasks.load("task").await.unwrap();
        let events = stream::iter([
            Ok(StreamResponse::Task(task)),
            Err(A2AError::internal("executor failed after starting")),
        ])
        .boxed();
        let MessageResponse::Task(task) =
            MessageResponse::receive(handler.tasks.clone(), handler.tasks.task("task"), events)
                .await
                .unwrap()
        else {
            panic!("expected a task response");
        };
        assert_eq!(
            task.wait().await.unwrap_err().message,
            "executor failed after starting"
        );
    }
}

#[cfg(test)]
mod subscription_tests {
    use super::*;

    #[tokio::test]
    async fn immediate_unary_joins_return_a_snapshot_without_waiting_for_executor_events() {
        use crate::handler::test_support::task_with_history;
        let task = task_with_history("task", "context", 0);
        let active = super::super::events::ActiveExecution::new(Some(task.clone()));
        let delivery = Delivery::Injected {
            task: task.clone(),
            receiver: active.subscribe(),
            sequence_before: 0,
            sequence_after: 0,
        };
        let mut request = crate::handler::test_support::send_request();
        request.configuration = Some(SendMessageConfiguration {
            return_immediately: Some(true),
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: None,
            timeline_length: None,
            if_generation_match: None,
        });
        let settings =
            ResponsePolicy::message(StartMode::Blocking, &crate::ServiceParams::new(), &request);
        let mut subscription = attach_delivery(settings, delivery, DeliveryAttachment::Opening);
        assert!(
            matches!(subscription.events.next().await.unwrap().unwrap(), StreamResponse::Task(snapshot) if snapshot == task)
        );
        assert!(subscription.events.next().await.is_none());
    }
}
