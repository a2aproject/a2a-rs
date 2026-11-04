// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Prepared input and executor context for one reserved invocation.

use super::super::DefaultRequestHandler;
use super::super::admission::{self, PreparedInput};
use super::super::delivery::{AuthorizedMessage, Delivery, StartMode, is_artifact_only};
use super::super::events::ActiveExecution;
use super::super::execution::InvocationState;
use crate::{ClientInputs, ExecutorContext};
use a2a::A2AError;
use std::sync::{Arc, atomic::Ordering};

pub(super) struct PreparedExecution {
    active: Arc<ActiveExecution>,
    inputs: ClientInputs,
    input: PreparedInput,
    mode: StartMode,
}

impl PreparedExecution {
    pub(super) async fn prepare(
        handler: &DefaultRequestHandler,
        message: &AuthorizedMessage,
        mode: StartMode,
        active: Arc<ActiveExecution>,
        inputs: ClientInputs,
        invocation: &mut InvocationState,
    ) -> Result<Self, A2AError> {
        // Only live exchanges create a new task up front; ordinary sends can
        // still answer with a bare Message without persisting a Task.
        let lazy = mode != StartMode::Bidi;
        let input = admission::prepare(handler, &message.request, active.task.id()).await?;
        handler
            .tasks
            .commit_input(&input, handler, message, lazy, invocation)
            .await?;
        active.set_snapshot(input.task.clone());
        active
            .uncreated
            .store(lazy && !input.existed, Ordering::SeqCst);
        Ok(Self {
            active,
            inputs,
            input,
            mode,
        })
    }

    pub(super) fn active(&self) -> &Arc<ActiveExecution> {
        &self.active
    }

    pub(super) fn attach_delivery(&mut self, handler: &DefaultRequestHandler) -> Delivery {
        handler.attach_pushes(&self.active.task);
        if self.input.existed || self.mode == StartMode::Bidi {
            // Existing subscribers observe persisted input. The new receiver
            // starts after it and gets the input through its opening response.
            self.active
                .publish_input(self.input.task.clone(), &self.input.events);
        } else {
            // Notify execution watchers without publishing an uncreated Task.
            self.active.task.notify();
        }
        Delivery::Started {
            task: self.input.task.clone(),
            receiver: self.active.subscribe(),
            recorded: std::mem::take(&mut self.input.events),
        }
    }

    pub(super) fn into_executor_context(self, message: AuthorizedMessage) -> ExecutorContext {
        let AuthorizedMessage {
            request,
            params,
            user,
            ..
        } = message;
        let task = self.input.task;
        let task_id = task.id.clone();
        let context_id = task.context_id.clone();
        // Ordinary streaming/live starts withhold a new task snapshot. Unary
        // starts include it; continuations always include the stored task.
        let stored_task = (self.input.existed || self.mode == StartMode::Blocking).then_some(task);
        ExecutorContext {
            execution_id: self.active.execution_id.clone(),
            message: (!is_artifact_only(&request)).then_some(request.message),
            task_id,
            stored_task,
            context_id,
            metadata: request.metadata,
            user,
            service_params: params,
            tenant: request.tenant,
            inputs: self.inputs,
            new_task: !self.input.existed,
        }
    }
}
