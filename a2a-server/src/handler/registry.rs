// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Route active execution handles and task event sources; transitions use mailboxes.

use super::events::{ActiveExecution, ExecutionReceiver};
use super::push_delivery::PushSubscriptions;
use super::task_events::TaskEvents;
use a2a::*;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

#[derive(Default)]
pub(super) struct ExecutionManager {
    pub(super) coordinators: super::coordinator::TaskCoordinators,
    executions: RwLock<HashMap<TaskId, std::sync::Weak<ActiveExecution>>>,
    sources: std::sync::Mutex<HashMap<TaskId, std::sync::Weak<TaskEvents>>>,
    /// The webhook deliveries of each task, in the order its events were
    /// published.
    pub(super) pushes: PushSubscriptions,
}

impl ExecutionManager {
    /// Sources survive turns while any response or webhook subscriber owns them.
    pub(super) fn events_for(&self, task_id: &str) -> Arc<TaskEvents> {
        let mut sources = self
            .sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sources.retain(|_, source| source.strong_count() > 0);
        if let Some(source) = sources.get(task_id).and_then(std::sync::Weak::upgrade) {
            return source;
        }
        let source = TaskEvents::new(None);
        sources.insert(task_id.to_owned(), Arc::downgrade(&source));
        source
    }

    /// Publish a weak routing handle; the TaskRuntime owns the invocation.
    pub(super) async fn register(
        &self,
        task_id: &str,
        active: &Arc<ActiveExecution>,
    ) -> Result<(), A2AError> {
        let mut executions = self.executions.write().await;
        if executions
            .get(task_id)
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|active| active.is_prepared())
        {
            return Err(A2AError::invalid_request(format!(
                "task execution is already in progress: {task_id}"
            )));
        }
        executions.retain(|_, execution| execution.strong_count() > 0);
        executions.insert(task_id.to_owned(), Arc::downgrade(active));
        Ok(())
    }

    /// Build a routing fixture without starting an executor or coordinator.
    #[cfg(test)]
    pub(super) async fn reserve(
        &self,
        task_id: &str,
    ) -> Result<
        (
            Arc<ActiveExecution>,
            crate::ClientInputs,
            super::events::InvocationState,
        ),
        A2AError,
    > {
        let (active, inputs, state) = ActiveExecution::with_events(self.events_for(task_id));
        self.register(task_id, &active).await?;
        Ok((active, inputs, state))
    }

    pub(super) async fn get(&self, task_id: &str) -> Option<Arc<ActiveExecution>> {
        let executions = self.executions.read().await;
        executions.get(task_id).and_then(std::sync::Weak::upgrade)
    }

    pub(super) async fn resubscribe(
        &self,
        task_id: &str,
    ) -> Option<(ExecutionReceiver, Option<Task>, u64)> {
        let active = self.get(task_id).await?;
        let (receiver, snapshot_task, sequence) = active.resubscribe().await;
        Some((receiver, snapshot_task, sequence))
    }

    pub(super) async fn unregister(&self, task_id: &str, active: &Arc<ActiveExecution>) {
        let mut executions = self.executions.write().await;
        let should_remove = executions
            .get(task_id)
            .and_then(std::sync::Weak::upgrade)
            .is_some_and(|current| Arc::ptr_eq(&current, active));
        if should_remove {
            executions.remove(task_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    /// A reservation whose request was dropped (its release still on its way)
    /// is replaced by the next request instead of being mistaken for a running
    /// execution.
    #[tokio::test]
    async fn test_reserve_replaces_a_dropped_requests_leftover() {
        let manager = ExecutionManager::default();
        let (stale, _inputs, _invocation) = manager.reserve("t-stale").await.unwrap();

        let (fresh, _inputs, _invocation) = manager
            .reserve("t-stale")
            .await
            .expect("the leftover is not a running execution");

        assert!(!Arc::ptr_eq(&stale, &fresh));
        let registered = manager.get("t-stale").await.unwrap();
        assert!(Arc::ptr_eq(&registered, &fresh));
        // A stale routing handle must not remove its successor.
        manager.unregister("t-stale", &stale).await;
        assert!(manager.get("t-stale").await.is_some());
    }

    #[tokio::test]
    async fn test_stream_disconnect_preserves_execution_for_resubscription() {
        use futures::StreamExt;

        let (handler, release) = make_resumable_handler();
        let params = ServiceParams::new();
        let mut message = make_message();
        message.task_id = Some("t-disconnect".into());
        message.context_id = Some("c-disconnect".into());
        seed_task(&handler, "t-disconnect", "c-disconnect").await;

        let mut stream = handler
            .send_streaming_message(
                &params,
                SendMessageRequest {
                    artifact_updates: Vec::new(),
                    message,
                    configuration: None,
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let initial = stream.next().await.unwrap().unwrap();
        match initial {
            StreamResponse::Task(task) => assert_eq!(task.status.state, TaskState::Working),
            _ => panic!("expected initial working task"),
        }
        drop(stream);

        let mut resumed = handler
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    last_seen_generation: None,
                    id: "t-disconnect".into(),
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let snapshot = resumed.next().await.unwrap().unwrap();
        match snapshot {
            StreamResponse::Task(task) => assert_eq!(task.status.state, TaskState::Working),
            _ => panic!("expected working task snapshot"),
        }

        release.notify_waiters();

        let terminal = resumed.next().await.unwrap().unwrap();
        match terminal {
            StreamResponse::Task(task) => {
                assert_eq!(task.status.state, TaskState::Completed)
            }
            _ => panic!("expected terminal task"),
        }
        assert!(resumed.next().await.is_none());
    }

    #[tokio::test]
    async fn a_prepared_execution_cannot_be_reserved_twice() {
        let manager = ExecutionManager::default();
        let (active, _inputs, _invocation) = manager.reserve("task").await.unwrap();
        let task = task_with_history("task", "context", 0);
        active.set_snapshot(task).await;
        assert!(manager.reserve("task").await.is_err());
        assert!(Arc::ptr_eq(&manager.get("task").await.unwrap(), &active));
    }

    #[test]
    fn event_source_registry_recovers_from_poison_and_prunes_unused_sources() {
        let manager = ExecutionManager::default();
        let held = manager.events_for("held");
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _sources = manager.sources.lock().unwrap();
                panic!("interrupted registry owner");
            }))
            .is_err()
        );
        for id in 0..256 {
            drop(manager.events_for(&format!("abandoned-{id}")));
        }
        assert!(Arc::ptr_eq(&held, &manager.events_for("held")));
        let sources = manager
            .sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!sources.contains_key("abandoned-0"));
        assert!(sources.contains_key("held"));
    }

    #[tokio::test]
    async fn dropping_the_owner_expires_the_route_even_while_the_registry_is_locked() {
        let manager = ExecutionManager::default();
        let (active, _inputs, _invocation) = manager.reserve("dropped-owner").await.unwrap();
        let registry = manager.executions.write().await;
        drop(active);
        assert!(registry.get("dropped-owner").unwrap().upgrade().is_none());
        drop(registry);
        assert!(manager.get("dropped-owner").await.is_none());
    }
}
