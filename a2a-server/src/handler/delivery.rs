// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Identify task requests and represent delivery outcomes.

use super::events::ExecutionReceiver;
use super::task_handle::TaskHandle;
use crate::middleware::ServiceParams;
use a2a::*;

/// A message bound to its task and authorized for its own caller. HTTP
/// routing carries this value through the uplink rather than authorizing twice.
pub(super) struct AuthorizedMessage {
    pub(super) request: SendMessageRequest,
    pub(super) params: ServiceParams,
    pub(super) user: Option<crate::middleware::User>,
    pub(super) response_id: Option<uuid::Uuid>,
}

/// Message-only input. HTTP routing can retain the original caller's authorization.
pub(super) enum MessageInput {
    Raw(SendMessageRequest),
    Authorized(AuthorizedMessage),
}

impl MessageInput {
    pub(super) fn request(&self) -> &SendMessageRequest {
        match self {
            Self::Raw(request) => request,
            Self::Authorized(message) => &message.request,
        }
    }

    pub(super) fn request_mut(&mut self) -> &mut SendMessageRequest {
        match self {
            Self::Raw(request) => request,
            Self::Authorized(message) => &mut message.request,
        }
    }
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
impl Delivery {
    pub(super) fn handle(&self) -> TaskHandle {
        match self {
            Self::Started { receiver, .. } | Self::Injected { receiver, .. } => receiver.task(),
        }
    }
}
