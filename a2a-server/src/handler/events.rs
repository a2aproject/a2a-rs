// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

use super::EXECUTION_BUFFER_CAPACITY;
use a2a::*;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

#[derive(Clone)]
pub(super) struct ExecutionEvent {
    pub(super) sequence: u64,
    pub(super) result: Result<StreamResponse, A2AError>,
    /// A `Task` snapshot that changed nothing. It sits at a generation that
    /// was already announced, so streams meant for clients leave it out; the
    /// blocking send, which only needs to know the executor spoke, keeps it.
    pub(super) repeated: bool,
}

#[derive(Default)]
pub(super) struct ActiveExecutionState {
    pub(super) sequence: u64,
    pub(super) snapshot_task: Option<Task>,
}

pub(super) struct ActiveExecution {
    pub(super) sender: broadcast::Sender<ExecutionEvent>,
    pub(super) state: RwLock<ActiveExecutionState>,
    /// Serialises every mutation of the task while it executes. The snapshot
    /// in `state` is the one authoritative copy: a writer takes this lock,
    /// reads the snapshot, applies its change, saves it and publishes the
    /// result, so executor events, a cancel and a follow-up cannot each work
    /// from a stale copy of their own.
    ///
    /// It is the task's own writer (see [`ExecutionManager::writer_for`]), not
    /// one per execution: a cancel or a follow-up that arrives while no
    /// execution is running serialises on the same lock.
    pub(super) writer: Arc<tokio::sync::Mutex<()>>,
}

impl ActiveExecution {
    #[cfg(test)]
    pub(super) fn new(task: Option<Task>) -> Arc<Self> {
        Self::with_writer(task, Arc::new(tokio::sync::Mutex::new(())))
    }

    pub(super) fn with_writer(
        task: Option<Task>,
        writer: Arc<tokio::sync::Mutex<()>>,
    ) -> Arc<Self> {
        let (sender, _) = broadcast::channel(EXECUTION_BUFFER_CAPACITY);
        Arc::new(Self {
            sender,
            state: RwLock::new(ActiveExecutionState {
                sequence: 0,
                snapshot_task: task,
            }),
            writer,
        })
    }

    pub(super) async fn snapshot(&self) -> Option<Task> {
        self.state.read().await.snapshot_task.clone()
    }

    pub(super) async fn set_snapshot(&self, task: Task) {
        self.state.write().await.snapshot_task = Some(task);
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<ExecutionEvent> {
        self.sender.subscribe()
    }

    pub(super) async fn resubscribe(
        &self,
    ) -> (broadcast::Receiver<ExecutionEvent>, Option<Task>, u64) {
        let receiver = self.sender.subscribe();
        let state = self.state.read().await;
        (receiver, state.snapshot_task.clone(), state.sequence)
    }

    pub(super) async fn publish(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot_task: Option<Task>,
    ) {
        self.publish_marked(result, snapshot_task, false).await;
    }

    pub(super) async fn publish_marked(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot_task: Option<Task>,
        repeated: bool,
    ) {
        let sequence = {
            let mut state = self.state.write().await;
            if let Some(task) = snapshot_task {
                state.snapshot_task = Some(task);
            }
            state.sequence += 1;
            state.sequence
        };

        // Task state remains available for a later subscription, but transient
        // events do not need to be allocated when no subscriber is listening.
        if self.sender.receiver_count() == 0 {
            return;
        }

        let event = ExecutionEvent {
            sequence,
            result: result.cloned().map_err(Clone::clone),
            repeated,
        };
        let _ = self.sender.send(event);
    }

    pub(super) async fn snapshot_is_terminal(&self) -> bool {
        let state = self.state.read().await;
        state
            .snapshot_task
            .as_ref()
            .is_some_and(|task| task.status.state.is_terminal())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_active_execution_keeps_snapshot_without_subscribers() {
        let task = Task {
            generation: 0,
            timeline: None,
            id: "t-buffering".into(),
            context_id: "c-buffering".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        let active = ActiveExecution::new(Some(task.clone()));

        let event = StreamResponse::Task(task.clone());
        active.publish(Ok(&event), Some(task)).await;

        let (mut receiver, snapshot_task, sequence) = active.resubscribe().await;
        assert_eq!(snapshot_task.unwrap().id, "t-buffering");
        assert_eq!(sequence, 1);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_active_execution_publishes_errors_to_subscribers() {
        let task = Task {
            generation: 0,
            timeline: None,
            id: "t-buffering-error".into(),
            context_id: "c-buffering-error".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        let active = ActiveExecution::new(Some(task));
        let mut receiver = active.subscribe();
        let error = A2AError::internal("execution failed");

        active.publish(Err(&error), None).await;

        let event = receiver.recv().await.unwrap();
        assert_eq!(event.sequence, 1);
        assert_eq!(event.result.unwrap_err().code, error.code);
    }

    #[tokio::test]
    async fn test_active_execution_drops_errors_without_subscribers() {
        let task = Task {
            generation: 0,
            timeline: None,
            id: "t-error-no-sub".into(),
            context_id: "c-error-no-sub".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        let active = ActiveExecution::new(Some(task.clone()));
        let error = A2AError::internal("execution failed");

        // Publish error without any subscribers
        active.publish(Err(&error), Some(task.clone())).await;

        // Resubscribe to verify snapshot was updated but no event was buffered
        let (mut receiver, snapshot_task, sequence) = active.resubscribe().await;
        assert_eq!(snapshot_task.unwrap().id, "t-error-no-sub");
        assert_eq!(sequence, 1);
        assert!(receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn test_active_execution_publishes_events_with_snapshot_updates() {
        let initial_task = Task {
            generation: 0,
            timeline: None,
            id: "t-snapshot-update".into(),
            context_id: "c-snapshot-update".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        let active = ActiveExecution::new(Some(initial_task.clone()));
        let mut receiver = active.subscribe();

        let updated_task = Task {
            generation: 0,
            timeline: None,
            id: "t-snapshot-update".into(),
            context_id: "c-snapshot-update".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };

        let event = StreamResponse::Task(updated_task.clone());
        active.publish(Ok(&event), Some(updated_task.clone())).await;

        // Verify event was published to subscriber
        let published = receiver.recv().await.unwrap();
        assert_eq!(published.sequence, 1);
        match published.result {
            Ok(StreamResponse::Task(task)) => {
                assert_eq!(task.id, "t-snapshot-update");
            }
            _ => panic!("expected Ok(Task) event"),
        }

        // Verify snapshot was updated
        let (_, snapshot_task, sequence) = active.resubscribe().await;
        assert_eq!(snapshot_task.unwrap().id, "t-snapshot-update");
        assert_eq!(sequence, 1);
    }

    #[tokio::test]
    async fn test_active_execution_broadcasts_to_multiple_subscribers() {
        let task = Task {
            generation: 0,
            timeline: None,
            id: "t-multi-sub".into(),
            context_id: "c-multi-sub".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        let active = ActiveExecution::new(Some(task.clone()));

        // Create two subscribers
        let mut sub1 = active.subscribe();
        let mut sub2 = active.subscribe();

        let event = StreamResponse::Task(task);
        active.publish(Ok(&event), None).await;

        // Both subscribers should receive the same event
        let evt1 = sub1.recv().await.unwrap();
        let evt2 = sub2.recv().await.unwrap();

        assert_eq!(evt1.sequence, 1);
        assert_eq!(evt2.sequence, 1);
        assert!(matches!(evt1.result, Ok(StreamResponse::Task(_))));
        assert!(matches!(evt2.result, Ok(StreamResponse::Task(_))));
    }
}
