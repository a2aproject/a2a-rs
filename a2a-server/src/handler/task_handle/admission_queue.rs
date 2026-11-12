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
    offer: Option<InputOffer>,
    permit: Option<admission::InputPermit>,
    deadline: Option<Instant>,
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
        if let TaskCommand::BeginCancel { .. } = &work.command
            && let Some(previous) = self.waiting.first()
        {
            if self.waiting.len() >= super::super::TASK_INPUT_CAPACITY {
                Self::reject(work, Rejection::input_busy());
                return None;
            }
            let active = previous.active.clone();
            self.waiting.push(Pending {
                work,
                active,
                offer: None,
                permit: None,
                deadline: None,
            });
            return None;
        }
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
        if active
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
        let immediate = message
            .request
            .configuration
            .as_ref()
            .and_then(|c| c.return_immediately)
            .unwrap_or(false);
        if self.waiting.is_empty() && (message.request.message.is_none() || immediate) {
            return Some(work);
        }
        if self.waiting.len() >= super::super::TASK_INPUT_CAPACITY {
            Self::reject(work, Rejection::input_busy());
            return None;
        }
        let deadline = message
            .request
            .message
            .as_ref()
            .filter(|_| !immediate)
            .map(|_| Instant::now() + handler.input_admission_timeout());
        let reserved =
            admission::validate_and_reserve_input(&active, &message.request, active.task.id());
        let permit = match reserved {
            Ok((_, permit)) => permit,
            Err(error) => {
                Self::reject(work, error);
                return None;
            }
        };
        let offer = permit
            .as_ref()
            .filter(|_| !immediate)
            .map(|_| InputOffer::new(self.changed.clone()));
        self.waiting.push(Pending {
            work,
            active,
            offer,
            permit,
            deadline,
        });
        None
    }

    fn reject(work: QueuedCommand, error: Rejection) {
        match work.command {
            TaskCommand::Admit { reply, .. } => reply.complete(Err(error)),
            TaskCommand::BeginCancel { reply, .. } => reply.complete(Err(error.into_error())),
            _ => unreachable!("only requests can wait for admission"),
        }
    }

    fn caller_closed(work: &QueuedCommand) -> bool {
        matches!(&work.command, TaskCommand::Admit { reply, .. } if reply.sender.as_ref().is_none_or(|sender| sender.is_closed()))
    }

    fn ready(&mut self, state: &TaskController) -> Option<QueuedCommand> {
        loop {
            if let Some(index) = self.waiting.iter().position(|pending| {
                pending
                    .deadline
                    .is_some_and(|deadline| deadline <= Instant::now())
                    || Self::caller_closed(&pending.work)
            }) {
                let pending = self.waiting.remove(index);
                if let Some(offer) = &pending.offer {
                    pending.active.inputs.clear_offer(&offer.reader());
                }
                Self::reject(pending.work, Rejection::Declined("executor did not read the answer before the admission deadline; input was not committed".into()));
                continue;
            }
            let pending = self.waiting.first_mut()?;
            if pending.offer.as_ref().is_some_and(InputOffer::canceled) {
                if let Some(offer) = &pending.offer {
                    pending.active.inputs.clear_offer(&offer.reader());
                }
                pending.offer = Some(InputOffer::new(self.changed.clone()));
            }
            if let Some(offer) = &pending.offer {
                pending.active.inputs.offer(Some(offer.reader()));
            }
            let ready = pending.offer.as_ref().is_none_or(InputOffer::claimed)
                || pending
                    .active
                    .stream_ended
                    .load(std::sync::atomic::Ordering::Acquire)
                || !state.execution.is_current(&pending.active)
                || pending.active.snapshot_is_terminal()
                || pending
                    .deadline
                    .is_some_and(|deadline| deadline <= Instant::now())
                || Self::caller_closed(&pending.work);
            if !ready {
                return None;
            }
            let Pending {
                work,
                active,
                offer,
                permit,
                deadline,
            } = self.waiting.remove(0);
            if let Some(offer) = &offer {
                active.inputs.clear_offer(&offer.reader());
            }
            if deadline.is_some_and(|deadline| deadline <= Instant::now())
                || Self::caller_closed(&work)
            {
                Self::reject(work, Rejection::Declined("executor did not read the answer before the admission deadline; input was not committed".into()));
                continue;
            }
            if state.execution.is_current(&active)
                && !active
                    .stream_ended
                    .load(std::sync::atomic::Ordering::Acquire)
                && !active.inputs.is_revoked()
                && offer.as_ref().is_some_and(InputOffer::begin_commit)
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
                        offer: offer.unwrap(),
                        permit: permit.expect("a message offer holds its inbox reservation"),
                    },
                    _ => unreachable!("only input commands are deferred"),
                };
                return Some(QueuedCommand { command, ..work });
            }
            // No input was committed or delivered. Revoke the old offer and
            // admit this request afresh; accepted inbox messages are not replayed.
            drop(offer);
            drop(permit);
            if !state.execution.is_current(&active)
                && state.execution.invocation().is_some()
                && matches!(&work.command, TaskCommand::Admit { message, .. } if message.request.message.is_some())
            {
                // This request is still uncommitted. Preserve its FIFO position
                // and original budget when a successor also needs a read claim.
                let count = self.waiting.len();
                if let Some(work) = self.stage(work, state) {
                    return Some(work);
                }
                if self.waiting.len() > count {
                    let mut pending = self.waiting.pop().unwrap();
                    pending.deadline = deadline;
                    self.waiting.insert(0, pending);
                }
                continue;
            }
            return Some(work);
        }
    }

    pub(super) async fn next(
        &mut self,
        commands: &mut mpsc::Receiver<QueuedCommand>,
        state: &TaskController,
    ) -> Option<QueuedCommand> {
        let changed = self.changed.clone();
        loop {
            // A retiring controller can still be waiting on the task's Notify.
            // Register before checking readiness so broadcast wakeups cannot
            // be lost between that check and the select below.
            let notified = changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(work) = self.ready(state) {
                return Some(work);
            }
            let deadline = self
                .waiting
                .iter()
                .filter_map(|pending| pending.deadline)
                .min();
            tokio::select! {
                biased;
                _ = &mut notified => {},
                _ = async { match deadline { Some(deadline) => tokio::time::sleep_until(deadline).await, None => std::future::pending().await } } => {},
                _ = async {
                    let waiting: Vec<_> = self.waiting.iter_mut().filter_map(|pending| match &mut pending.work.command {
                        TaskCommand::Admit { reply, .. } => reply.sender.as_mut().map(|sender| Box::pin(sender.closed())),
                        _ => None,
                    }).collect();
                    if waiting.is_empty() { std::future::pending::<()>().await; }
                    else { let _ = futures::future::select_all(waiting).await; }
                } => {},
                work = commands.recv() => match work {
                    Some(work) => if let Some(work) = self.stage(work, state) { return Some(work); },
                    None => return None,
                },
            }
        }
    }
}
