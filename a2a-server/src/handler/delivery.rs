// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Identify task requests and represent delivery outcomes.

use super::artifacts::apply_client_artifacts;
use super::events::ExecutionReceiver;
use crate::middleware::ServiceParams;
use a2a::*;

/// A request that carries only `artifactUpdates` for an existing task. The
/// native `message` is not optional, so the wire's absent message arrives as
/// an empty one with no id (see the protobuf conversion); a message that has
/// an id, a body or a task of its own is never mistaken for it.
pub(super) fn is_artifact_only(req: &SendMessageRequest) -> bool {
    !req.artifact_updates.is_empty()
        && req.message.role == Role::Unspecified
        && req.message.message_id.is_empty()
        && req.message.parts.is_empty()
        && req.message.task_id.is_none()
        && req.message.context_id.is_none()
        && req.message.metadata.is_none()
        && req.message.extensions.is_none()
        && req.message.reference_task_ids.is_none()
}

/// The task a send request names: the message's, or for an artifact-only
/// request the one its updates target.
pub(super) fn request_task_id(req: &SendMessageRequest) -> Option<String> {
    if is_artifact_only(req) {
        req.artifact_updates
            .iter()
            .map(|update| update.task_id.as_str())
            .find(|task_id| !task_id.is_empty())
            .map(str::to_string)
    } else {
        req.message.task_id.clone()
    }
}

/// A message bound to its task and authorized for its own caller. HTTP
/// routing carries this value through the uplink rather than authorizing twice.
pub(super) struct AuthorizedMessage {
    pub(super) request: SendMessageRequest,
    pub(super) params: ServiceParams,
    pub(super) user: Option<crate::middleware::User>,
    pub(super) response_id: Option<uuid::Uuid>,
}

/// Checks against the authoritative task while its writer is held, whether
/// the request joins a running turn or starts a continuation.
pub(super) fn validate_existing_task(
    task: &Task,
    req: &SendMessageRequest,
) -> Result<(), A2AError> {
    if task.status.state.is_terminal() {
        return Err(A2AError::unsupported_operation(format!(
            "task {} has already reached a terminal state",
            task.id
        )));
    }
    if let Some(expected) = req
        .configuration
        .as_ref()
        .and_then(|c| c.if_generation_match)
    {
        if task.generation != expected {
            return Err(A2AError::task_generation_mismatch(
                &task.id,
                expected,
                task.generation,
            ));
        }
    }
    if let Some(requested) = req.message.context_id.as_deref() {
        if requested != task.context_id {
            return Err(A2AError::invalid_params(format!(
                "contextId {requested} does not match task {}, which belongs to context {}",
                task.id, task.context_id
            )));
        }
    }
    Ok(())
}

pub(super) struct RecordedInput {
    pub(super) events: Vec<StreamResponse>,
    /// The message's generation before any artifact updates advanced it.
    pub(super) message_generation: Option<i64>,
}

/// Records a client message in history and the timeline, then applies
/// the artifacts sent with it (A2A 1.1 §3.4: the message first).
///
/// Returns the events that describe what was recorded, in order, for a live
/// stream to announce: the message as it was appended, then each artifact.
pub(super) fn record_client_input(
    task: &mut Task,
    req: &SendMessageRequest,
    creating: bool,
) -> Result<RecordedInput, A2AError> {
    let mut events = Vec::new();
    let mut message_generation = None;
    if !is_artifact_only(req) {
        task.history
            .get_or_insert_with(Vec::new)
            .push(req.message.clone());
        task.generation += 1;
        message_generation = Some(task.generation);
        task.timeline
            .get_or_insert_with(Vec::new)
            .push(TimelineEntry {
                content: TimelineContent::Message(req.message.clone()),
                generation: task.generation,
            });
        events.push(client_message_event(task, &req.message));
    }
    events.extend(
        apply_client_artifacts(task, req, creating)?
            .into_iter()
            .map(StreamResponse::ArtifactUpdate),
    );
    Ok(RecordedInput {
        events,
        message_generation,
    })
}

/// The event that announces a client message appended to `task` at its current
/// generation.
pub(super) fn client_message_event(task: &Task, message: &Message) -> StreamResponse {
    StreamResponse::MessageUpdate(TaskMessageUpdateEvent {
        task_id: task.id.clone(),
        context_id: task.context_id.clone(),
        message: message.clone(),
        metadata: None,
        generation: task.generation,
    })
}

/// A message that reached a task, and what the session needs to carry on.
pub(super) enum Delivery {
    /// The message started a new turn.
    Started {
        task: Task,
        receiver: ExecutionReceiver,
        /// What the request recorded before the turn began, as events: the
        /// message as it was appended, then each artifact. A stream that
        /// already holds the task is told them; the first request's opening
        /// snapshot already reflects them.
        recorded: Vec<StreamResponse>,
    },
    /// The message went into the execution that was running.
    Injected {
        task: Task,
        receiver: ExecutionReceiver,
        /// The execution's sequence before the message's events were published,
        /// and after: a stream that already holds the task resumes after the
        /// latter, one that joins now takes them from the former.
        sequence_before: u64,
        sequence_after: u64,
    },
}

/// How a request reaches `start_execution`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartMode {
    /// `SendMessage`, with completion controlled by its request configuration.
    Blocking,
    /// `SendStreamingMessage`, with server output only.
    Streaming,
    /// `SendLiveMessage`, with an uplink and downlink bound to one task.
    Bidi,
}
