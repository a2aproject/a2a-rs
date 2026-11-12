// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Hold uncommitted answers until the current reader claims them or reaches EOF.

use super::{QueuedCommand, TaskCommand, TaskController};
use crate::{
    executor::InputOffer,
    handler::{admission, events::ActiveExecution, protocol::Rejection},
};
use a2a::TaskState;
use std::sync::Arc;
use tokio::{
    sync::{Notify, mpsc},
    time::Instant,
};

struct Pending {
    work: QueuedCommand,
    active: Arc<ActiveExecution>,
    offer: InputOffer,
    deadline: Instant,
}

pub(super) struct PendingAdmissions {
    waiting: Vec<Pending>,
    changed: Arc<Notify>,
}

impl PendingAdmissions {
    pub(super) fn new(changed: Arc<Notify>) -> Self {
        Self {
            waiting: Vec::new(),
            changed,
        }
    }

    /// Offers carry no message or generation. The owned request and its reserved
    /// publication permit stay here until admission, never in an executor inbox.
    pub(super) fn stage(
        &mut self,
        work: QueuedCommand,
        state: &TaskController,
    ) -> Option<QueuedCommand> {
        let TaskCommand::Admit {
            handler, message, ..
        } = &work.command
        else {
            return Some(work);
        };
        let Some(invocation) = state.execution.invocation() else {
            return Some(work);
        };
        let active = invocation.active.clone();
        if message.request.message.is_none()
            || active
                .stream_ended
                .load(std::sync::atomic::Ordering::Acquire)
            || active.inputs.is_revoked()
            || !(active.snapshot().is_some_and(|task| {
                matches!(
                    task.status.state,
                    TaskState::InputRequired | TaskState::AuthRequired
                )
            }) || self
                .waiting
                .iter()
                .any(|pending| Arc::ptr_eq(&pending.active, &active)))
        {
            return Some(work);
        }
        let deadline = Instant::now() + handler.input_admission_timeout();
        let reserved =
            admission::validate_and_reserve_input(&active, &message.request, active.task.id());
        let permit = match reserved {
            Ok((_, Some(permit))) => permit,
            Ok((_, None)) => return Some(work),
            Err(error) => {
                Self::reject(work, error);
                return None;
            }
        };
        let (queued, offer) = InputOffer::new(self.changed.clone());
        permit.send(queued);
        self.waiting.push(Pending {
            work,
            active,
            offer,
            deadline,
        });
        None
    }

    fn reject(work: QueuedCommand, error: Rejection) {
        if let TaskCommand::Admit { reply, .. } = work.command {
            reply.complete(Err(error));
        }
    }

    fn caller_closed(work: &QueuedCommand) -> bool {
        matches!(&work.command, TaskCommand::Admit { reply, .. } if reply.sender.as_ref().is_none_or(|sender| sender.is_closed()))
    }

    fn ready(&mut self, state: &TaskController) -> Option<QueuedCommand> {
        loop {
            let index = self.waiting.iter().position(|pending| {
                pending.offer.claimed()
                    || pending
                        .active
                        .stream_ended
                        .load(std::sync::atomic::Ordering::Acquire)
                    || !state.execution.is_current(&pending.active)
                    || pending.active.snapshot_is_terminal()
                    || pending.deadline <= Instant::now()
                    || Self::caller_closed(&pending.work)
            })?;
            let Pending {
                work,
                active,
                offer,
                deadline,
            } = self.waiting.remove(index);
            if deadline <= Instant::now() || Self::caller_closed(&work) {
                Self::reject(work, Rejection::Declined("executor did not read the answer before the admission deadline; input was not committed".into()));
                continue;
            }
            if state.execution.is_current(&active)
                && !active
                    .stream_ended
                    .load(std::sync::atomic::Ordering::Acquire)
                && !active.inputs.is_revoked()
                && offer.begin_commit()
            {
                let command = match work.command {
                    TaskCommand::Admit {
                        handler,
                        message,
                        mode,
                        id,
                        reply,
                    } => TaskCommand::AdmitClaimed {
                        handler,
                        message,
                        mode,
                        id,
                        reply,
                        active,
                        offer,
                    },
                    _ => unreachable!("only input commands are deferred"),
                };
                return Some(QueuedCommand { command, ..work });
            }
            // No input was committed or delivered. Revoke the old offer and
            // admit this request afresh; accepted inbox messages are not replayed.
            drop(offer);
            return Some(work);
        }
    }

    pub(super) async fn next(
        &mut self,
        commands: &mut mpsc::Receiver<QueuedCommand>,
        state: &TaskController,
    ) -> Option<QueuedCommand> {
        loop {
            if let Some(work) = self.ready(state) {
                return Some(work);
            }
            let deadline = self.waiting.iter().map(|pending| pending.deadline).min();
            tokio::select! {
                biased;
                _ = self.changed.notified() => {},
                _ = async { match deadline { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } } => {},
                work = commands.recv() => return work,
            }
        }
    }
}
