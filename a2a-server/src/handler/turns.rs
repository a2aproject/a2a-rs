// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Close execution turns and transfer unread inputs to the next turn.

use super::DefaultRequestHandler;
use super::coordinator::TaskRuntime;
use super::events::{ActiveExecution, EventKind};
use super::registry::ExecutionManager;
use a2a::*;
use std::sync::Arc;

/// Starts the invocation that takes over `unread` inside the completion
/// command. Gives the messages back if reservation fails.
pub(super) async fn start_next_turn(
    handler: &DefaultRequestHandler,
    task_id: &TaskId,
    snapshot: Task,
    mut unread: Vec<crate::executor::ClientInput>,
    state: &mut TaskRuntime,
) -> Result<(), Vec<crate::executor::ClientInput>> {
    let Ok((next, inputs, invocation)) = handler.execution_manager.reserve(task_id).await else {
        return Err(unread);
    };
    state
        .invocations
        .insert(next.execution_id.clone(), invocation);
    next.set_snapshot(snapshot.clone()).await;
    let first = unread.remove(0);
    for input in unread {
        if let Ok(permit) = next.inputs.sender.clone().try_reserve_owned() {
            permit.send(
                state
                    .invocations
                    .get_mut(&next.execution_id)
                    .unwrap()
                    .track_input(input),
            );
        }
    }

    let exec_ctx = crate::ExecutorContext {
        execution_id: next.execution_id.clone(),
        message: Some(first.message),
        task_id: task_id.clone(),
        stored_task: Some(snapshot.clone()),
        context_id: snapshot.context_id.clone(),
        metadata: first.metadata,
        user: first.user,
        service_params: first.service_params,
        tenant: first.tenant,
        inputs,
        new_task: false,
    };
    super::execution::spawn(handler.clone(), task_id.clone(), next, exec_ctx);
    Ok(())
}

/// Close input delivery and unregister in one coordinator transition. Later
/// input commands either join a successor or start a new invocation.
pub(super) async fn release_execution(
    manager: &ExecutionManager,
    active: &Arc<ActiveExecution>,
    task_id: &str,
    state: &mut TaskRuntime,
) {
    reject_unread_inputs(active, task_id, state).await;
    state.invocations.remove(&active.execution_id);
    manager.finish(task_id, active).await;
}

/// Saves the push configuration the request that started the execution
/// carried, now that the task it is for exists.
pub(super) async fn save_pending_push_config(
    active: &ActiveExecution,
    handler: &DefaultRequestHandler,
    state: &mut TaskRuntime,
) {
    let pending = state
        .invocations
        .get_mut(&active.execution_id)
        .and_then(|invocation| invocation.pending_push.take());
    if let (Some(config), Some(store)) = (pending, handler.push_config_store.as_ref()) {
        if let Err(error) = store.save(config).await {
            tracing::warn!(%error, "could not save the push configuration of a new task");
        }
    }
}

/// Closes the task-input queue and tells the exchanges about each
/// message the executor never read. Those messages were accepted, recorded and
/// announced; the executor's turn ended without handling them, which the
/// client that sent one is told by a `StreamError` naming it, rather than by
/// silence.
pub(super) async fn reject_unread_inputs(
    active: &ActiveExecution,
    task_id: &str,
    state: &mut TaskRuntime,
) {
    reject_inputs(
        active,
        task_id,
        state
            .invocations
            .entry(active.execution_id.clone())
            .or_default()
            .drain_inputs(&active.inputs),
    )
    .await;
}

/// Publishes unread-input errors inside the coordinator's completion command.
pub(super) async fn reject_inputs(
    active: &ActiveExecution,
    task_id: &str,
    inputs: Vec<crate::executor::ClientInput>,
) {
    for input in inputs {
        let event = StreamResponse::Error(StreamError {
            task_id: task_id.to_string(),
            code: "UNAVAILABLE".to_string(),
            message: "the agent finished its turn before reading this message".to_string(),
            reference_id: Some(input.message.message_id.clone()).filter(|id| !id.is_empty()),
            metadata: None,
        });
        active
            .publish_kind(Ok(&event), None, EventKind::LiveOnly(input.response_id))
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn terminal_turn_cleanup_rejects_unread_input_even_after_creation_fails() {
        for deferred in [false, true] {
            let runtime = make_handler();
            let mut task = task_with_history("terminal", "context", 0);
            task.status.state = TaskState::Canceled;
            runtime.task_store.create(task.clone()).await.unwrap();
            let (active, _inbox, writer) =
                runtime.execution_manager.reserve(&task.id).await.unwrap();
            active.set_snapshot(task.clone()).await;
            active
                .uncreated
                .store(deferred, std::sync::atomic::Ordering::SeqCst);
            let input = crate::ClientInput::new(make_message(), 1);
            let reference = input.message.message_id.clone();
            let queued = active.track_input(input).await;
            active.inputs.sender.try_send(queued).unwrap();
            let mut events = active.subscribe().await;
            drop(writer);
            // In the deferred case create collides with an already persisted
            // terminal task; cleanup must still release and reject the inbox.
            end_turn(&runtime, &active, &task.id).await;
            let event = events.recv().await.unwrap().result.unwrap();
            assert!(matches!(event, StreamResponse::Error(ref error)
                if error.code == "UNAVAILABLE" && error.reference_id.as_deref() == Some(reference.as_str())));
            assert!(active.inputs.closed.is_cancelled());
            assert!(runtime.execution_manager.get(&task.id).await.is_none());
            assert_eq!(
                runtime.task_store.get(&task.id).await.unwrap().unwrap(),
                task
            );
        }
    }

    #[tokio::test]
    async fn a_busy_handoff_returns_input_with_its_caller_identity_intact() {
        let runtime = make_handler();
        let task = task_with_history("busy-handoff", "context", 0);
        let (active, _inputs, _invocation) =
            runtime.execution_manager.reserve(&task.id).await.unwrap();
        active.set_snapshot(task.clone()).await;
        let mut input = crate::ClientInput::new(make_message(), 7);
        input.user = Some(crate::User::authenticated("alice"));
        input.tenant = Some("tenant-a".into());
        input.metadata = Some(std::collections::HashMap::from([(
            "trace".into(),
            serde_json::json!(42),
        )]));
        let returned = start_next_turn(
            &runtime,
            &task.id,
            task.clone(),
            vec![input.clone()],
            &mut TaskRuntime::default(),
        )
        .await
        .unwrap_err();
        assert_eq!(returned.len(), 1);
        assert_eq!(returned[0].message, input.message);
        assert_eq!(returned[0].user, input.user);
        assert_eq!(returned[0].metadata, input.metadata);
        assert_eq!(returned[0].tenant, input.tenant);
        assert!(runtime.execution_manager.get(&task.id).await.is_some());
    }

    #[tokio::test]
    async fn rejected_pending_push_configuration_does_not_replace_existing_endpoints() {
        let runtime = make_handler_with_push_configs();
        let config = |id: usize| TaskPushNotificationConfig {
            task_id: "configured".into(),
            id: Some(id.to_string()),
            url: "https://example.com/push".into(),
            token: None,
            authentication: None,
            tenant: None,
            protocol_version: Some("1.1".into()),
        };
        let store = runtime.push_config_store.as_ref().unwrap();
        for id in 0..50 {
            store.save(config(id)).await.unwrap();
        }
        let active = ActiveExecution::new(None);
        let mut state = TaskRuntime::default();
        let mut invocation = super::super::events::InvocationState::default();
        invocation.pending_push = Some(config(50));
        state
            .invocations
            .insert(active.execution_id.clone(), invocation);
        save_pending_push_config(&active, &runtime, &mut state).await;
        assert!(
            state
                .invocations
                .get(&active.execution_id)
                .unwrap()
                .pending_push
                .is_none()
        );
        assert_eq!(store.list("configured").await.unwrap().len(), 50);
        assert!(store.get("configured", "50").await.is_err());
        save_pending_push_config(&active, &runtime, &mut state).await;
        assert_eq!(store.list("configured").await.unwrap().len(), 50);
    }

    #[tokio::test]
    async fn artifact_only_unread_input_has_no_empty_message_reference() {
        let active = ActiveExecution::new(None);
        let mut events = active.subscribe().await;
        let mut message = make_message();
        message.message_id.clear();
        reject_inputs(&active, "task", vec![crate::ClientInput::new(message, 3)]).await;
        let event = events.recv().await.unwrap();
        assert!(matches!(event.kind, EventKind::LiveOnly(None)));
        let StreamResponse::Error(error) = event.result.unwrap() else {
            panic!("expected a rejection")
        };
        assert_eq!(error.task_id, "task");
        assert_eq!(error.code, "UNAVAILABLE");
        assert_eq!(error.reference_id, None);
    }
    struct CaptureTurn(
        tokio::sync::mpsc::UnboundedSender<(crate::ExecutorContext, Vec<crate::ClientInput>)>,
    );

    impl crate::AgentExecutor for CaptureTurn {
        fn execute(
            &self,
            mut context: crate::ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let queued = std::iter::from_fn(|| context.inputs.try_recv()).collect();
            let mut task = context.stored_task.clone().unwrap();
            task.status.state = TaskState::Completed;
            self.0
                .send((context, queued))
                .unwrap_or_else(|_| panic!("observer dropped"));
            Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
        }
        fn cancel(
            &self,
            context: crate::ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            crate::AgentExecutor::cancel(&EchoExecutor, context)
        }
    }

    #[tokio::test]
    async fn a_handoff_preserves_the_trigger_and_queues_remaining_inputs_in_order() {
        let (sent, mut received) = tokio::sync::mpsc::unbounded_channel();
        let runtime = DefaultRequestHandler::new(CaptureTurn(sent), InMemoryTaskStore::new());
        let mut task = task_with_history("next-turn", "context", 0);
        task.status.state = TaskState::Working;
        runtime.task_store.create(task.clone()).await.unwrap();
        let inputs: Vec<_> = ["first", "second", "third"]
            .into_iter()
            .enumerate()
            .map(|(index, id)| {
                let mut message = make_message();
                message.message_id = id.into();
                let mut input = crate::ClientInput::new(message, index as i64 + 1);
                input.user = Some(crate::User::authenticated("alice"));
                input.tenant = Some("tenant-a".into());
                input.metadata = Some(std::collections::HashMap::from([(
                    "trace".into(),
                    serde_json::json!(id),
                )]));
                input
            })
            .collect();
        assert!(
            start_next_turn(
                &runtime,
                &task.id,
                task.clone(),
                inputs.clone(),
                &mut TaskRuntime::default()
            )
            .await
            .is_ok()
        );
        let (context, queued) = timeout(Duration::from_secs(1), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(context.message, Some(inputs[0].message.clone()));
        assert_eq!(context.metadata, inputs[0].metadata);
        assert_eq!(context.user, inputs[0].user);
        assert_eq!(context.tenant, inputs[0].tenant);
        assert_eq!(
            queued
                .iter()
                .map(|input| input.message.message_id.as_str())
                .collect::<Vec<_>>(),
            ["second", "third"]
        );
        assert_eq!(
            queued
                .iter()
                .map(|input| input.generation)
                .collect::<Vec<_>>(),
            [2, 3]
        );
        timeout(Duration::from_secs(1), async {
            while runtime.execution_manager.get(&task.id).await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            runtime
                .task_store
                .get(&task.id)
                .await
                .unwrap()
                .unwrap()
                .status
                .state,
            TaskState::Completed
        );
    }
}
