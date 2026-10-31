// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

use super::{events::ExecutionEvent, protocol::is_terminal_event};
use a2a::*;
use futures::{StreamExt, stream::BoxStream};
use tokio::sync::broadcast;

pub(super) struct SubscriptionState {
    pub(super) receiver: broadcast::Receiver<ExecutionEvent>,
    pub(super) snapshot_task: Option<Task>,
    pub(super) min_sequence: u64,
    pub(super) done: bool,
}

pub(super) fn subscription_stream(
    receiver: broadcast::Receiver<ExecutionEvent>,
    snapshot_task: Option<Task>,
    min_sequence: u64,
    include_repeats: bool,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    Box::pin(futures::stream::unfold(
        SubscriptionState {
            receiver,
            snapshot_task,
            min_sequence,
            done: false,
        },
        move |mut state| async move {
            if state.done {
                return None;
            }

            if let Some(task) = state.snapshot_task.take() {
                if task.status.state.is_terminal() {
                    state.done = true;
                }
                return Some((Ok(StreamResponse::Task(task)), state));
            }

            loop {
                match state.receiver.recv().await {
                    Ok(event) if event.sequence <= state.min_sequence => continue,
                    Ok(event) if event.repeated && !include_repeats => {
                        state.min_sequence = event.sequence;
                        continue;
                    }
                    Ok(event) => {
                        state.min_sequence = event.sequence;
                        match &event.result {
                            Ok(item) if is_terminal_event(item) => state.done = true,
                            Err(_) => state.done = true,
                            _ => {}
                        }
                        return Some((event.result.clone(), state));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        state.done = true;
                        return Some((
                            Err(crate::sanitized_internal_error(
                                "subscription fell behind active execution",
                            )),
                            state,
                        ));
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    ))
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
