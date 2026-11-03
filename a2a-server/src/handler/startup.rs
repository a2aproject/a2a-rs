// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Prepare the initial events and completion policy for the shared runner.

use super::delivery::{Delivery, StartMode};
use super::response::{Events, RunCompletion};
use super::subscription::{DeliveryAttachment, SubscriptionAttachment, SubscriptionPolicy};
use a2a::*;
use futures::{StreamExt, stream::BoxStream};

/// Starting a send opens a response stream, unless an HTTP live request
/// joined an existing exchange and receives only its acknowledgement.
pub(super) enum StartedResponse {
    Stream { task_id: TaskId, events: Events },
    Ack(Box<Task>),
}

impl StartedResponse {
    pub(super) fn into_stream(self) -> Result<(TaskId, Events), A2AError> {
        match self {
            Self::Stream { task_id, events } => Ok((task_id, events)),
            Self::Ack(_) => Err(A2AError::internal(
                "only an HTTP live request can receive an acknowledgement",
            )),
        }
    }
}

pub(super) struct PreparedResponse {
    pub(super) task_id: TaskId,
    pub(super) events: Option<Events>,
    pub(super) opening: Option<Task>,
    pub(super) completion: RunCompletion,
    /// Reused for the opening, followups, and later executor invocations.
    pub(super) policy: SubscriptionPolicy,
    pub(super) seen_generation: i64,
}

impl PreparedResponse {
    /// Translate delivery into one initial subscription. A caller joining a
    /// running turn starts before or after its own input events according to
    /// the response mode; a new turn starts at its first event.
    pub(super) fn message(
        request: SendMessageRequest,
        delivery: Delivery,
        mode: StartMode,
        policy: SubscriptionPolicy,
    ) -> Self {
        let return_immediately = request
            .configuration
            .as_ref()
            .and_then(|config| config.return_immediately)
            .unwrap_or(false);
        let subscription =
            policy.delivery(delivery, DeliveryAttachment::Opening { return_immediately });
        let (task, events) = (subscription.task, subscription.events);
        let task_id = task.id.clone();
        let (opening, events, completion) = match mode {
            StartMode::Blocking => (None, events, RunCompletion::Unary(Box::new(request))),
            StartMode::Streaming => (None, with_opening_task(events, task), RunCompletion::Task),
            StartMode::Bidi => (Some(task), events, RunCompletion::Exchange),
        };
        Self {
            task_id,
            events: Some(events),
            opening,
            completion,
            policy,
            seen_generation: 0,
        }
    }

    pub(super) fn subscription(
        attachment: SubscriptionAttachment,
        completion: RunCompletion,
        policy: SubscriptionPolicy,
    ) -> Self {
        Self {
            task_id: attachment.task_id,
            events: attachment
                .receiver
                .map(|receiver| policy.stream(receiver, None, attachment.sequence)),
            opening: attachment.opening,
            completion,
            policy,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_http_acknowledgement_cannot_be_misclassified_as_a_response_stream() {
        let task = crate::handler::test_support::task_with_history("task", "context", 0);
        let error = StartedResponse::Ack(Box::new(task))
            .into_stream()
            .err()
            .expect("an ACK is not a downlink");
        assert_eq!(error.code, a2a::error_code::INTERNAL_ERROR);
    }
}
