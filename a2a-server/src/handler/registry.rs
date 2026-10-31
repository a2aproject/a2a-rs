// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Manage active executions, task writer locks, and reservation cleanup.

use super::events::{ActiveExecution, ExecutionReceiver};
use super::push_delivery::PushQueues;
use a2a::*;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::RwLock;

#[derive(Default)]
pub(super) struct ExecutionManager {
    executions: RwLock<HashMap<TaskId, Arc<ActiveExecution>>>,
    /// One writer lock per task, shared by whoever is mutating it. Held weakly
    /// so a task nobody is touching costs nothing.
    writers: std::sync::Mutex<TaskWriters>,
    /// The webhook deliveries of each task, in the order its events were
    /// published.
    pub(super) pushes: PushQueues,
}

#[derive(Default)]
struct TaskWriters {
    locks: HashMap<TaskId, std::sync::Weak<tokio::sync::Mutex<()>>>,
    calls: usize,
}

impl ExecutionManager {
    /// The task's writer lock. Every mutation of a task takes it: executor
    /// events, a cancel, a follow-up being prepared. Two callers asking for the
    /// same task while either still holds it get the same lock.
    pub(super) fn writer_for(&self, task_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        Self::lock_for(&self.writers, task_id)
    }

    fn lock_for(
        locks: &std::sync::Mutex<TaskWriters>,
        task_id: &str,
    ) -> Arc<tokio::sync::Mutex<()>> {
        let mut writers = locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        writers.calls += 1;
        if writers.calls % 256 == 0 {
            writers.locks.retain(|_, lock| lock.strong_count() > 0);
        }
        if let Some(lock) = writers
            .locks
            .get(task_id)
            .and_then(std::sync::Weak::upgrade)
        {
            return lock;
        }
        let lock = Arc::new(tokio::sync::Mutex::new(()));
        writers
            .locks
            .insert(task_id.to_string(), Arc::downgrade(&lock));
        lock
    }

    /// Claims the task for one execution before anything reads it, so two
    /// requests for the same task cannot both prepare from the same stored
    /// state. The snapshot is filled in once the request has been applied.
    ///
    /// The task's writer is taken first and handed back with the reservation:
    /// anyone who finds the execution registered (a subscriber, a cancel) then
    /// waits on a lock that is already held, instead of slipping in between
    /// the registration and the first write.
    pub(super) async fn reserve(
        &self,
        task_id: &str,
    ) -> Result<
        (
            Arc<ActiveExecution>,
            crate::ClientInputs,
            tokio::sync::OwnedMutexGuard<()>,
        ),
        A2AError,
    > {
        let writer = self.writer_for(task_id);
        let guard = Arc::clone(&writer).lock_owned().await;
        self.reserve_held(task_id, guard).await
    }

    /// [`Self::reserve`] for a caller that already holds the task's writer.
    pub(super) async fn reserve_held(
        &self,
        task_id: &str,
        guard: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<
        (
            Arc<ActiveExecution>,
            crate::ClientInputs,
            tokio::sync::OwnedMutexGuard<()>,
        ),
        A2AError,
    > {
        let writer = self.writer_for(task_id);
        let mut executions = self.executions.write().await;
        if let Some(existing) = executions.get(task_id) {
            // A live reservation holds the writer until its snapshot is set,
            // and this caller holds it now: an entry that still has no
            // snapshot belongs to a request that was dropped, whose release is
            // still on its way. It is replaced, not mistaken for a running
            // execution.
            if existing.snapshot().await.is_some() {
                return Err(A2AError::invalid_request(format!(
                    "task execution is already in progress: {task_id}"
                )));
            }
        }

        let (active, inputs) = ActiveExecution::with_writer(None, writer);
        executions.insert(task_id.to_string(), active.clone());
        Ok((active, inputs, guard))
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
        // Released now if the registry is free, so a request already waiting
        // for the writer this future still holds finds the task released, not
        // a stale registration. Only a registry that is busy at this instant
        // falls back to releasing from a spawned task.
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

/// Execution and delivery share the same immutable handler services.
pub(super) type ExecutionRuntime = super::DefaultRequestHandler;

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
        let (stale, _inputs, guard) = manager.reserve("t-stale").await.unwrap();
        drop(guard); // the request goes away; nothing released its entry

        let (fresh, _inputs, _guard) = manager
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

    /// The reservation hands back the task's writer already held: whoever
    /// finds the execution registered then waits on a lock that is taken, not
    /// one that is still free between the registration and the first write.
    #[tokio::test]
    async fn test_reserve_holds_the_writer_from_the_moment_it_registers() {
        let manager = ExecutionManager::default();

        let (active, _inputs, guard) = manager.reserve("t-reserved").await.unwrap();
        assert!(manager.get("t-reserved").await.is_some());
        assert!(
            manager.writer_for("t-reserved").try_lock().is_err(),
            "the writer is held at registration"
        );
        // It is the same lock the execution writes under.
        assert!(Arc::ptr_eq(
            &active.writer,
            &manager.writer_for("t-reserved")
        ));

        drop(guard);
        assert!(manager.writer_for("t-reserved").try_lock().is_ok());
        // Once the execution has its snapshot a second reservation is refused,
        // and does not leave the writer taken.
        active
            .set_snapshot(Task {
                generation: 1,
                timeline: None,
                id: "t-reserved".into(),
                context_id: "c-reserved".into(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            })
            .await;
        assert!(manager.reserve("t-reserved").await.is_err());
        assert!(manager.writer_for("t-reserved").try_lock().is_ok());
    }
    #[test]
    fn writer_map_recovers_from_poison_and_prunes_unused_locks() {
        let manager = ExecutionManager::default();
        let held = manager.writer_for("held");
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _writers = manager.writers.lock().unwrap();
            panic!("interrupted registry owner");
        }));
        assert!(poisoned.is_err());
        for id in 0..256 {
            drop(manager.writer_for(&format!("abandoned-{id}")));
        }
        assert!(Arc::ptr_eq(&held, &manager.writer_for("held")));
        let writers = manager
            .writers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(!writers.locks.contains_key("abandoned-0"));
        assert!(writers.locks.contains_key("held"));
    }

    #[tokio::test]
    async fn dropped_reservation_releases_after_a_contended_registry_is_unlocked() {
        let manager = Arc::new(ExecutionManager::default());
        let (active, _inputs, writer) = manager.reserve("contended-drop").await.unwrap();
        drop(writer);
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
