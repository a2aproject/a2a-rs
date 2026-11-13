// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

use super::{Resubscription, events::ActiveExecution, subscription::subscription_stream};
use a2a::*;
use futures::{
    StreamExt, future,
    stream::{self, BoxStream},
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard, PoisonError},
};
use tokio::sync::{RwLock, broadcast, oneshot};

#[derive(Default)]
pub(super) struct ExecutionManager {
    pub(super) executions: RwLock<HashMap<TaskId, Arc<ActiveExecution>>>,
    pub(super) waiting: Mutex<HashMap<TaskId, Vec<oneshot::Sender<Resubscription>>>>,
    /// One writer lock per task, shared by whoever is mutating it. Held weakly
    /// so a task nobody is touching costs nothing.
    pub(super) writers: std::sync::Mutex<TaskWriters>,
}

#[derive(Default)]
pub(super) struct TaskWriters {
    pub(super) locks: HashMap<TaskId, std::sync::Weak<tokio::sync::Mutex<()>>>,
    pub(super) calls: usize,
}

impl ExecutionManager {
    pub(super) fn waiting(
        &self,
    ) -> MutexGuard<'_, HashMap<TaskId, Vec<oneshot::Sender<Resubscription>>>> {
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The task's writer lock. Every mutation of a task takes it: executor
    /// events, a cancel, a follow-up being prepared. Two callers asking for the
    /// same task while either still holds it get the same lock.
    pub(super) fn writer_for(&self, task_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut writers = self
            .writers
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
}

impl ExecutionManager {
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
    ) -> Result<(Arc<ActiveExecution>, tokio::sync::OwnedMutexGuard<()>), A2AError> {
        let writer = self.writer_for(task_id);
        let guard = Arc::clone(&writer).lock_owned().await;
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

        let active = ActiveExecution::with_writer(None, writer);
        executions.insert(task_id.to_string(), active.clone());
        Ok((active, guard))
    }

    pub(super) async fn get(&self, task_id: &str) -> Option<Arc<ActiveExecution>> {
        let executions = self.executions.read().await;
        executions.get(task_id).cloned()
    }

    pub(super) async fn resubscribe(&self, task_id: &str) -> Option<Resubscription> {
        Some(self.get(task_id).await?.resubscribe().await)
    }

    /// Subscribes to the task's registered execution or, when there is none,
    /// to the next one `start` registers. Decided under the registry lock, so
    /// an execution registering meanwhile is either found here or finds the
    /// waiter.
    pub(super) async fn subscribe(self: &Arc<Self>, task_id: &str) -> Waiter {
        let (sender, receiver) = oneshot::channel();
        let executions = self.executions.read().await;
        match executions.get(task_id) {
            Some(active) => {
                let _ = sender.send(active.resubscribe().await);
            }
            None => {
                self.waiting()
                    .entry(task_id.to_owned())
                    .or_default()
                    .push(sender);
            }
        }
        drop(executions);
        Waiter {
            manager: Arc::clone(self),
            task_id: task_id.to_owned(),
            attached: Some(receiver),
        }
    }

    /// Ends the subscriptions still waiting for the task's next execution
    /// with its final snapshot: a canceled task has no execution for them to
    /// follow, and a terminal snapshot closes their streams.
    pub(super) fn release(&self, task: &Task) {
        let waiting = self.waiting().remove(&task.id).unwrap_or_default();
        for waiter in waiting {
            let (_, events) = broadcast::channel(1);
            let _ = waiter.send((events, Some(task.clone()), 0));
        }
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

/// Ends the subscriptions still waiting for a task when dropped, so a cancel
/// that fails part-way leaves none of them behind.
pub(super) struct ReleaseOnDrop<'a> {
    pub(super) manager: &'a ExecutionManager,
    pub(super) task: Task,
}

impl Drop for ReleaseOnDrop<'_> {
    fn drop(&mut self) {
        self.manager.release(&self.task);
    }
}

/// A subscription that `ExecutionManager::reserve` attaches to the task's next
/// execution. Dropped first, it leaves nothing behind in the registry.
pub(super) struct Waiter {
    pub(super) manager: Arc<ExecutionManager>,
    pub(super) task_id: TaskId,
    pub(super) attached: Option<oneshot::Receiver<Resubscription>>,
}

impl Waiter {
    /// The execution attached so far, if any.
    pub(super) fn attached(&mut self) -> Option<Resubscription> {
        self.attached.as_mut()?.try_recv().ok()
    }

    /// The events of the execution once attached; none if the handler goes
    /// away first.
    pub(super) fn into_stream(mut self) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        stream::once(async move { self.attached.as_mut()?.await.ok() })
            .filter_map(future::ready)
            .flat_map(|(receiver, snapshot_task, sequence)| {
                subscription_stream(receiver, snapshot_task, sequence, false)
            })
            .boxed()
    }
}

impl Drop for Waiter {
    fn drop(&mut self) {
        // Closing the receiver is what marks this waiter's own sender closed.
        self.attached = None;
        let mut waiting = self.manager.waiting();
        if let Some(waiters) = waiting.get_mut(&self.task_id) {
            waiters.retain(|waiter| !waiter.is_closed());
            if waiters.is_empty() {
                waiting.remove(&self.task_id);
            }
        }
    }
}

/// Releases a task's execution reservation when dropped, unless disarmed.
///
/// The reservation is taken before several awaits, so a request future that is
/// dropped part-way (a client that disconnects) or an executor that panics
/// would otherwise leave the task "already in progress" for good.
pub(super) struct ReservationGuard {
    pub(super) manager: Arc<ExecutionManager>,
    pub(super) task_id: TaskId,
    pub(super) active: Arc<ActiveExecution>,
    pub(super) armed: bool,
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
