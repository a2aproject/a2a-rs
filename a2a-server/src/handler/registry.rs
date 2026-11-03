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
    executions: RwLock<HashMap<TaskId, Arc<ActiveExecution>>>,
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

    /// The task coordinator calls this while processing its input or handoff.
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
        let mut executions = self.executions.write().await;
        if let Some(existing) = executions.get(task_id) {
            // Mailbox admission cannot overlap. An entry with no snapshot is
            // abandoned preparation whose panic cleanup may still be pending.
            if existing.snapshot().await.is_some() {
                return Err(A2AError::invalid_request(format!(
                    "task execution is already in progress: {task_id}"
                )));
            }
        }

        let (mut active, inputs, state) = ActiveExecution::with_events(self.events_for(task_id));
        Arc::get_mut(&mut active)
            .expect("new invocation has one owner")
            ._coordinator = Some(self.coordinators.for_task(task_id));
        executions.insert(task_id.to_string(), active.clone());
        Ok((active, inputs, state))
    }

    pub(super) async fn get(&self, task_id: &str) -> Option<Arc<ActiveExecution>> {
        let executions = self.executions.read().await;
        executions.get(task_id).cloned()
    }

    pub(super) async fn resubscribe(
        &self,
        task_id: &str,
    ) -> Option<(ExecutionReceiver, Option<Task>, u64)> {
        let active = self.get(task_id).await?;
        let (receiver, snapshot_task, sequence) = active.resubscribe().await;
        Some((receiver, snapshot_task, sequence))
    }

    /// [`Self::finish`] without waiting: `false` when the registry is busy.
    pub(super) fn try_finish(&self, task_id: &str, active: &Arc<ActiveExecution>) -> bool {
        let Ok(mut executions) = self.executions.try_write() else {
            return false;
        };
        if executions
            .get(task_id)
            .is_some_and(|current| Arc::ptr_eq(current, active))
        {
            executions.remove(task_id);
        }
        true
    }

    pub(super) async fn finish(&self, task_id: &str, active: &Arc<ActiveExecution>) {
        let mut executions = self.executions.write().await;
        let should_remove = executions
            .get(task_id)
            .is_some_and(|current| Arc::ptr_eq(current, active));
        if should_remove {
            executions.remove(task_id);
        }
    }
}

/// Releases a task's execution reservation when dropped, unless disarmed.
///
/// The reservation is taken before several awaits, so a request future that is
/// dropped part-way (a client that disconnects) or an executor that panics
/// would otherwise leave the task "already in progress" for good.
pub(super) struct ReservationGuard {
    manager: Arc<ExecutionManager>,
    task_id: TaskId,
    active: Arc<ActiveExecution>,
    armed: bool,
}

impl ReservationGuard {
    pub(super) fn new(
        manager: Arc<ExecutionManager>,
        task_id: TaskId,
        active: Arc<ActiveExecution>,
    ) -> Self {
        Self {
            manager,
            task_id,
            active,
            armed: true,
        }
    }

    /// Hands responsibility for the reservation to the spawned execution.
    pub(super) fn disarm(mut self) {
        self.armed = false;
    }

    /// Gives the task back now, so the next request already finds it free.
    /// Dropping the guard does the same, but only after the fact.
    pub(super) async fn release(mut self) {
        self.armed = false;
        self.manager.finish(&self.task_id, &self.active).await;
    }
}

impl Drop for ReservationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Cleanup never removes a successor invocation: finish checks pointer
        // identity. Avoid spawning unless the short routing lock is contended.
        if self.manager.try_finish(&self.task_id, &self.active) {
            return;
        }
        // `finish` is async; without a runtime to run it on (the process is
        // shutting down) there is nothing left to release.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let manager = Arc::clone(&self.manager);
            let task_id = self.task_id.clone();
            let active = Arc::clone(&self.active);
            runtime.spawn(async move { manager.finish(&task_id, &active).await });
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
        // The old guard's late release must not remove the new reservation.
        manager.finish("t-stale", &stale).await;
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
    async fn dropped_reservation_releases_after_a_contended_registry_is_unlocked() {
        let manager = Arc::new(ExecutionManager::default());
        let (active, _inputs, _invocation) = manager.reserve("contended-drop").await.unwrap();
        let reservation =
            ReservationGuard::new(manager.clone(), "contended-drop".into(), active.clone());
        let registry = manager.executions.write().await;
        drop(reservation);
        assert!(registry.contains_key("contended-drop"));
        drop(registry);
        timeout(Duration::from_secs(1), async {
            while manager.get("contended-drop").await.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}
