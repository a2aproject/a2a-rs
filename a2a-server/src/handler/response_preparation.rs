// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Prepare response subscriptions, initial events, and completion settings.

use super::delivery::{Delivery, StartMode};
use super::events::{ExecutionReceiver, subscription_stream};
use super::protocol::{
    is_at_least_1_1, is_terminal_event, requested_protocol_version, should_interrupt_non_streaming,
};
use super::response::Events;
use super::{task_handle::TaskHandle, task_service::TaskService};
use crate::middleware::ServiceParams;
use a2a::*;
use futures::{StreamExt, stream::BoxStream};
use std::sync::Arc;

#[derive(Clone, Copy, Default)]
pub(super) struct ResponseOptions {
    pub(super) return_immediately: bool,
    history_length: Option<i32>,
    timeline_length: Option<i32>,
}

impl ResponseOptions {
    pub(super) fn from_request(request: &SendMessageRequest) -> Self {
        let config = request.configuration.as_ref();
        Self {
            return_immediately: config
                .and_then(|config| config.return_immediately)
                .unwrap_or(false),
            history_length: config.and_then(|config| config.history_length),
            timeline_length: config.and_then(|config| config.timeline_length),
        }
    }

    pub(super) fn trim(self, task: &mut Task) {
        super::protocol::apply_lengths(task, self.history_length, self.timeline_length);
    }
}

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
    pub(super) settings: ResponseSettings,
    pub(super) seen_generation: i64,
}

impl PreparedResponse {
    /// Translate delivery into one initial subscription. A caller joining a
    /// running turn starts before or after its own input events according to
    /// the response mode; a new turn starts at its first event.
    pub(super) fn message(delivery: Delivery, settings: ResponseSettings) -> Self {
        let handle = delivery.handle();
        let subscription = settings.delivery(delivery, DeliveryAttachment::Opening);
        let (task, events) = (subscription.task, subscription.events);
        let (opening, events) = match settings.mode() {
            StartMode::Blocking => (None, events),
            StartMode::Streaming => (None, with_opening_task(events, task)),
            StartMode::Bidi => (Some(task), events),
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
        settings: ResponseSettings,
    ) -> Self {
        Self {
            task: attachment.task,
            events: attachment
                .receiver
                .map(|receiver| settings.stream(receiver, None, attachment.sequence)),
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
            let head: Vec<_> = match &first {
                Ok(StreamResponse::Message(_) | StreamResponse::Task(_)) | Err(_) => vec![first],
                _ => vec![Ok(StreamResponse::Task(opening)), first],
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

#[derive(Clone, Copy)]
pub(super) struct ResponseSettings {
    mode: StartMode,
    wants_message_updates: bool,
    options: ResponseOptions,
    response_id: Option<uuid::Uuid>,
}

pub(super) enum DeliveryAttachment {
    /// Start a response; streaming modes announce the input in their opening snapshot.
    Opening,
    /// An existing response must observe the follow-up's input events.
    FollowUp,
}

pub(super) struct DeliverySubscription {
    pub(super) task: Task,
    pub(super) events: Events,
    /// Inputs recorded before the new turn had any subscribers.
    pub(super) recorded: Vec<StreamResponse>,
}

impl ResponseSettings {
    pub(super) fn new(mode: StartMode, wants_message_updates: bool) -> Self {
        Self {
            mode,
            wants_message_updates,
            options: ResponseOptions::default(),
            response_id: None,
        }
    }

    /// Resolve response behavior once, before admitting the opening message.
    pub(super) fn message(
        mode: StartMode,
        params: &ServiceParams,
        request: &SendMessageRequest,
    ) -> Self {
        Self {
            options: ResponseOptions::from_request(request),
            ..Self::subscription(mode, params)
        }
    }

    pub(super) fn subscription(mode: StartMode, params: &ServiceParams) -> Self {
        Self {
            response_id: (mode == StartMode::Bidi).then(uuid::Uuid::now_v7),
            ..Self::new(mode, is_at_least_1_1(&requested_protocol_version(params)))
        }
    }

    pub(super) fn mode(self) -> StartMode {
        self.mode
    }

    pub(super) fn is_live(self) -> bool {
        self.mode == StartMode::Bidi
    }

    pub(super) fn return_immediately(self) -> bool {
        self.options.return_immediately
    }

    pub(super) fn trim(self, task: &mut Task) {
        self.options.trim(task);
    }

    pub(super) fn completes(self, event: &StreamResponse) -> bool {
        is_terminal_event(event)
            || matches!(event, StreamResponse::Message(_))
            || (self.mode == StartMode::Blocking
                && should_interrupt_non_streaming(self.return_immediately(), event).is_some())
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
                    && self.return_immediately()
                    && matches!(attachment, DeliveryAttachment::Opening);
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
                    && matches!(attachment, DeliveryAttachment::Opening) =>
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
        let settings = ResponseSettings {
            options: ResponseOptions {
                return_immediately: true,
                ..ResponseOptions::default()
            },
            ..ResponseSettings::new(StartMode::Blocking, false)
        };
        let mut subscription = settings.delivery(delivery, DeliveryAttachment::Opening);
        assert!(
            matches!(subscription.events.next().await.unwrap().unwrap(), StreamResponse::Task(snapshot) if snapshot == task)
        );
        assert!(subscription.events.next().await.is_none());
    }
}
