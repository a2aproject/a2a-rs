// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Execution state owned exclusively by the Task coordinator.

use super::cancellation::Cancellation;
use super::events::{ActiveExecution, InvocationState};
use std::sync::Arc;

pub(super) struct Invocation {
    pub(super) active: Arc<ActiveExecution>,
    pub(super) bookkeeping: InvocationState,
}

#[derive(Default)]
pub(super) enum ExecutionState {
    #[default]
    Idle,
    Running(Invocation),
    Canceling {
        invocation: Option<Invocation>,
        progress: Box<Cancellation>,
    },
}

impl ExecutionState {
    pub(super) fn invocation(&self) -> Option<&Invocation> {
        match self {
            Self::Running(invocation) => Some(invocation),
            Self::Canceling { invocation, .. } => invocation.as_ref(),
            Self::Idle => None,
        }
    }

    pub(super) fn bookkeeping(&mut self, active: &ActiveExecution) -> Option<&mut InvocationState> {
        let invocation = match self {
            Self::Running(invocation) => invocation,
            Self::Canceling { invocation, .. } => invocation.as_mut()?,
            Self::Idle => return None,
        };
        (invocation.active.execution_id == active.execution_id)
            .then_some(&mut invocation.bookkeeping)
    }

    pub(super) fn is_current(&self, active: &Arc<ActiveExecution>) -> bool {
        self.invocation()
            .is_some_and(|invocation| Arc::ptr_eq(&invocation.active, active))
    }

    pub(super) fn release(&mut self, active: &Arc<ActiveExecution>) {
        if !self.is_current(active) {
            return;
        }
        match self {
            Self::Running(_) => *self = Self::Idle,
            Self::Canceling { invocation, .. } => *invocation = None,
            Self::Idle => {}
        }
    }
}
