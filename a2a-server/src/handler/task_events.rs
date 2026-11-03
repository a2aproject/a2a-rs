// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Task-scoped publication shared by executor invocations and subscribers.

use super::events::{EventKind, ExecutionEvent};
use super::output::FailureSignal;
use a2a::*;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

#[derive(Default)]
struct State {
    sequence: u64,
    snapshot: Option<Task>,
    last_error: Option<(u64, A2AError)>,
}

pub(super) struct TaskEvents {
    pub(super) sender: broadcast::Sender<ExecutionEvent>,
    state: RwLock<State>,
    pub(super) notifications: std::sync::OnceLock<super::push_delivery::NotificationSettings>,
    pub(super) webhook_subscribed: std::sync::Mutex<super::push_delivery::SubscriptionState>,
}

impl TaskEvents {
    pub(super) fn new(snapshot: Option<Task>) -> Arc<Self> {
        Arc::new(Self {
            sender: broadcast::channel(super::EXECUTION_BUFFER_CAPACITY).0,
            state: RwLock::new(State {
                snapshot,
                ..State::default()
            }),
            notifications: std::sync::OnceLock::new(),
            webhook_subscribed: std::sync::Mutex::default(),
        })
    }

    pub(super) async fn snapshot(&self) -> Option<Task> {
        self.state.read().await.snapshot.clone()
    }

    pub(super) async fn set_snapshot(&self, task: Task) {
        self.state.write().await.snapshot = Some(task);
    }

    pub(super) async fn sequence(&self) -> u64 {
        self.state.read().await.sequence
    }

    pub(super) async fn subscribe(&self) -> (broadcast::Receiver<ExecutionEvent>, u64) {
        let receiver = self.sender.subscribe();
        let sequence = self.state.read().await.sequence;
        (receiver, sequence)
    }

    pub(super) async fn resubscribe(
        &self,
    ) -> (broadcast::Receiver<ExecutionEvent>, Option<Task>, u64) {
        let receiver = self.sender.subscribe();
        let state = self.state.read().await;
        (receiver, state.snapshot.clone(), state.sequence)
    }

    /// A lagging response must still observe an execution error published since
    /// its cursor, even after a later turn has overwritten the broadcast buffer.
    pub(super) async fn resynchronize(
        &self,
        after: u64,
    ) -> Result<(broadcast::Receiver<ExecutionEvent>, Option<Task>, u64), A2AError> {
        let receiver = self.sender.subscribe();
        let state = self.state.read().await;
        if let Some((sequence, error)) = &state.last_error
            && *sequence > after
        {
            return Err(error.clone());
        }
        Ok((receiver, state.snapshot.clone(), state.sequence))
    }

    /// The Task coordinator orders this publication after persistence. Every
    /// consumer sees the same publication order, regardless of execution turn.
    pub(super) async fn publish(
        self: &Arc<Self>,
        result: Result<&StreamResponse, &A2AError>,
        snapshot: Option<Task>,
        kind: EventKind,
        failure: Option<FailureSignal>,
    ) {
        let sequence = {
            let mut state = self.state.write().await;
            if let Some(task) = snapshot {
                state.snapshot = Some(task);
            }
            state.sequence += 1;
            if let Err(error) = result {
                state.last_error = Some((state.sequence, error.clone()));
            }
            state.sequence
        };
        // Subscription startup/retirement and send share this short lock; an
        // idle subscriber cannot disappear between admission and publication.
        let notifies = kind == EventKind::Normal
            && result.is_ok_and(|event| super::protocol::task_id_for_event(event).is_some());
        let _subscription = if notifies {
            self.notifications
                .get()
                .map(|_| super::push_delivery::subscribe(self))
        } else {
            None
        };
        if self.sender.receiver_count() > 0 {
            let _ = self.sender.send(ExecutionEvent {
                sequence,
                result: result.cloned().map_err(Clone::clone),
                kind,
                failure,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::registry::ExecutionManager;
    use crate::handler::test_support::task_with_history;

    #[tokio::test]
    async fn task_subscription_and_sequence_survive_executor_handoff() {
        let manager = ExecutionManager::default();
        let (first, _inputs, _invocation) = manager.reserve("task").await.unwrap();
        let source = first.events.clone();
        let mut subscriber = source.sender.subscribe();
        let task = task_with_history("task", "context", 0);
        first.set_snapshot(task.clone()).await;
        first
            .publish(Ok(&StreamResponse::Task(task.clone())), Some(task.clone()))
            .await;
        manager.unregister("task", &first).await;
        drop(first);

        let (second, _inputs, _invocation) = manager.reserve("task").await.unwrap();
        assert!(Arc::ptr_eq(&source, &second.events));
        assert!(
            second.snapshot().await.is_none(),
            "a new reservation must wait for preparation"
        );
        second.set_snapshot(task.clone()).await;
        second.publish(Ok(&StreamResponse::Task(task)), None).await;
        assert_eq!(subscriber.recv().await.unwrap().sequence, 1);
        assert_eq!(subscriber.recv().await.unwrap().sequence, 2);
    }

    #[tokio::test]
    async fn registry_does_not_retain_unused_task_sources() {
        let manager = ExecutionManager::default();
        let source = manager.events_for("unused");
        let weak = Arc::downgrade(&source);
        drop(source);
        assert!(weak.upgrade().is_none());
    }
}
