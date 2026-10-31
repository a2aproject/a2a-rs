// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Publish execution events, filter subscriptions, and notify task watchers.

use super::protocol::{is_terminal_event, task_id_for_event};
use super::{EXECUTION_BUFFER_CAPACITY, TASK_INPUT_CAPACITY};
use crate::executor::{ClientInput, ClientInputSender, ClientInputs, QueuedInput, ReturnedInputs};
use a2a::*;
use futures::stream::BoxStream;
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{RwLock, broadcast};

#[derive(Clone)]
pub(super) struct ExecutionEvent {
    pub(super) sequence: u64,
    pub(super) result: Result<StreamResponse, A2AError>,
    pub(super) kind: EventKind,
}

/// How an event is meant to be delivered.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EventKind {
    Normal,
    /// A `Task` snapshot that changed nothing. It sits at a generation that was
    /// already announced, so streams meant for clients leave it out; the
    /// blocking send, which only needs to know the executor spoke, keeps it.
    Repeated,
    /// Something only a live exchange is told (a `StreamError` about a message
    /// of its own), not the other subscribers of the task.
    LiveOnly(Option<uuid::Uuid>),
    /// The executor's direct reply, with no task: the last thing any stream
    /// following the execution is sent.
    Reply,
}

/// A subscription to an execution's events. One that falls further behind than
/// the buffer holds can start over from the task as it stands, which is what a
/// client that sees a gap in the generations does anyway.
pub(super) struct ExecutionReceiver {
    receiver: broadcast::Receiver<ExecutionEvent>,
    /// Weak: a subscription must not keep a finished execution open.
    execution: std::sync::Weak<ActiveExecution>,
}

impl ExecutionReceiver {
    pub(super) async fn recv(&mut self) -> Result<ExecutionEvent, broadcast::error::RecvError> {
        self.receiver.recv().await
    }

    #[cfg(test)]
    pub(super) fn try_recv(&mut self) -> Result<ExecutionEvent, broadcast::error::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Starts over after the events were lost: the task as it stands, and the
    /// sequence after which events follow it. `None` when the execution is
    /// over or has no task yet.
    async fn resync(&mut self) -> Option<(Task, u64)> {
        let execution = self.execution.upgrade()?;
        let (receiver, snapshot, sequence) = execution.resubscribe().await;
        self.receiver = receiver.receiver;
        snapshot.map(|task| (task, sequence))
    }
}

#[derive(Default)]
struct ActiveExecutionState {
    sequence: u64,
    snapshot_task: Option<Task>,
    pending_inputs: Vec<QueuedInput>,
    returned_inputs: Option<ReturnedInputs>,
}

pub(super) struct ActiveExecution {
    pub(super) execution_id: String,
    sender: broadcast::Sender<ExecutionEvent>,
    state: RwLock<ActiveExecutionState>,
    /// Serialises every mutation of the task while it executes. The snapshot
    /// in `state` is the one authoritative copy: a writer takes this lock,
    /// reads the snapshot, applies its change, saves it and publishes the
    /// result, so executor events, a cancel and a follow-up cannot each work
    /// from a stale copy of their own.
    ///
    /// It is the task's own writer (see
    /// [`ExecutionManager::writer_for`](super::registry::ExecutionManager::writer_for)), not
    /// one per execution: a cancel or a follow-up that arrives while no
    /// execution is running serialises on the same lock.
    pub(super) writer: Arc<tokio::sync::Mutex<()>>,
    /// Where messages sent to this task go while the executor runs. The queue
    /// belongs to the execution turn and is handed off with unread inputs.
    pub(super) inputs: ClientInputSender,
    /// Completed after the runner has revoked the inbox and rejected unread input.
    pub(super) inputs_drained: tokio_util::sync::CancellationToken,
    pub(super) push_failure: super::output::FailureSignal,
    /// Set while the task this execution is for has not been created: a new
    /// task is created by the executor's first event, and does not exist if
    /// the executor answers with a message alone.
    pub(super) uncreated: std::sync::atomic::AtomicBool,
    /// The push configuration the request carried, kept until the task exists.
    pub(super) pending_push: std::sync::Mutex<Option<TaskPushNotificationConfig>>,
}

impl ActiveExecution {
    #[cfg(test)]
    pub(super) fn new(task: Option<Task>) -> Arc<Self> {
        Self::with_writer(task, Arc::new(tokio::sync::Mutex::new(()))).0
    }

    pub(super) fn with_writer(
        task: Option<Task>,
        writer: Arc<tokio::sync::Mutex<()>>,
    ) -> (Arc<Self>, ClientInputs) {
        let (sender, _) = broadcast::channel(EXECUTION_BUFFER_CAPACITY);
        let (inputs, receiver, returned_inputs) = ClientInputs::for_execution(TASK_INPUT_CAPACITY);
        (
            Arc::new(Self {
                execution_id: uuid::Uuid::now_v7().to_string(),
                sender,
                state: RwLock::new(ActiveExecutionState {
                    sequence: 0,
                    snapshot_task: task,
                    pending_inputs: Vec::new(),
                    returned_inputs: Some(returned_inputs),
                }),
                writer,
                inputs,
                inputs_drained: tokio_util::sync::CancellationToken::new(),
                push_failure: tokio::sync::watch::channel(None).0,
                uncreated: std::sync::atomic::AtomicBool::new(false),
                pending_push: std::sync::Mutex::new(None),
            }),
            receiver,
        )
    }

    /// Keep receipts in the existing state lock; recv itself takes no lock.
    pub(super) async fn track_input(&self, input: ClientInput) -> QueuedInput {
        let input = QueuedInput::new(input);
        let mut state = self.state.write().await;
        state.pending_inputs.retain(QueuedInput::pending);
        state.pending_inputs.push(input.clone());
        input
    }

    /// The task writer excludes new input. Revoke the old reader before
    /// reclaiming unread receipts, even if a worker still owns that receiver.
    pub(super) async fn drain_inputs(&self) -> Vec<ClientInput> {
        self.inputs.revoke();
        let mut state = self.state.write().await;
        state.returned_inputs.take();
        state
            .pending_inputs
            .drain(..)
            .filter_map(QueuedInput::reclaim)
            .collect()
    }

    /// The most recently published execution event, read under the state lock.
    pub(super) async fn sequence(&self) -> u64 {
        self.state.read().await.sequence
    }

    pub(super) async fn snapshot(&self) -> Option<Task> {
        self.state.read().await.snapshot_task.clone()
    }

    pub(super) async fn set_snapshot(&self, task: Task) {
        self.state.write().await.snapshot_task = Some(task);
    }

    pub(super) fn subscribe(self: &Arc<Self>) -> ExecutionReceiver {
        ExecutionReceiver {
            receiver: self.sender.subscribe(),
            execution: Arc::downgrade(self),
        }
    }

    pub(super) async fn resubscribe(self: &Arc<Self>) -> (ExecutionReceiver, Option<Task>, u64) {
        let receiver = self.subscribe();
        let state = self.state.read().await;
        (receiver, state.snapshot_task.clone(), state.sequence)
    }

    pub(super) async fn publish(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot_task: Option<Task>,
    ) {
        self.publish_kind(result, snapshot_task, EventKind::Normal)
            .await;
    }

    pub(super) async fn publish_marked(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot_task: Option<Task>,
        repeated: bool,
    ) {
        let kind = if repeated {
            EventKind::Repeated
        } else {
            EventKind::Normal
        };
        self.publish_kind(result, snapshot_task, kind).await;
    }

    pub(super) async fn publish_kind(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot_task: Option<Task>,
        kind: EventKind,
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
            kind,
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

struct SubscriptionState {
    receiver: ExecutionReceiver,
    snapshot_task: Option<Task>,
    min_sequence: u64,
    done: bool,
}

pub(super) fn subscription_stream(
    receiver: ExecutionReceiver,
    snapshot_task: Option<Task>,
    min_sequence: u64,
    include_repeats: bool,
    message_updates: bool,
    live: bool,
    response_id: Option<uuid::Uuid>,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    Box::pin(futures::stream::unfold(
        SubscriptionState {
            receiver,
            snapshot_task,
            min_sequence,
            done: false,
        },
        move |mut state| async move {
            if state.done {
                return None;
            }

            if let Some(task) = state.snapshot_task.take() {
                if task.status.state.is_terminal() {
                    state.done = true;
                }
                return Some((Ok(StreamResponse::Task(task)), state));
            }

            loop {
                match state.receiver.recv().await {
                    Ok(event) if event.sequence <= state.min_sequence => continue,
                    Ok(event) if event.kind == EventKind::Repeated && !include_repeats => {
                        state.min_sequence = event.sequence;
                        continue;
                    }
                    Ok(event)
                        if matches!(event.kind, EventKind::LiveOnly(recipient)
                        if !live || recipient != response_id) =>
                    {
                        state.min_sequence = event.sequence;
                        continue;
                    }
                    // A message appended to the task is a payload kind a 1.0
                    // client does not know; only one that announced 1.1 or later
                    // is sent it, and reconciles the generation gap otherwise.
                    Ok(event)
                        if !message_updates
                            && matches!(&event.result, Ok(StreamResponse::MessageUpdate(_))) =>
                    {
                        state.min_sequence = event.sequence;
                        continue;
                    }
                    Ok(event) => {
                        state.min_sequence = event.sequence;
                        match &event.result {
                            Ok(item) if is_terminal_event(item) => state.done = true,
                            Err(_) => state.done = true,
                            _ if event.kind == EventKind::Reply => state.done = true,
                            _ => {}
                        }
                        return Some((event.result.clone(), state));
                    }
                    // Events were lost: start over from the task as it stands.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        match state.receiver.resync().await {
                            Some((task, sequence)) => {
                                state.min_sequence = sequence;
                                if task.status.state.is_terminal() {
                                    state.done = true;
                                }
                                return Some((Ok(StreamResponse::Task(task)), state));
                            }
                            None => return None,
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    ))
}

/// Tells whoever is watching a task that it changed: a long-poll for its next
/// generation, an idle live exchange. Per task, so a change wakes only the
/// watchers of that task, and a watcher is woken by every change since it last
/// looked, however it timed its look.
#[derive(Default)]
pub(super) struct TaskChanges {
    watched: std::sync::Mutex<HashMap<TaskId, std::sync::Weak<tokio::sync::watch::Sender<u64>>>>,
}

/// A watcher of one task; keeps the task's channel open while it lives.
pub(super) struct TaskWatch {
    _channel: Arc<tokio::sync::watch::Sender<u64>>,
    changes: tokio::sync::watch::Receiver<u64>,
}

impl TaskWatch {
    /// Waits until the task has changed since this watcher last looked.
    pub(super) async fn changed(&mut self) {
        // The sender is held by this watcher, so the channel does not close.
        let _ = self.changes.changed().await;
    }
}

impl TaskChanges {
    pub(super) fn watch(&self, task_id: &str) -> TaskWatch {
        let mut watched = self
            .watched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        watched.retain(|_, channel| channel.strong_count() > 0);
        let channel = match watched.get(task_id).and_then(std::sync::Weak::upgrade) {
            Some(channel) => channel,
            None => {
                let channel = Arc::new(tokio::sync::watch::channel(0).0);
                watched.insert(task_id.to_string(), Arc::downgrade(&channel));
                channel
            }
        };
        TaskWatch {
            changes: channel.subscribe(),
            _channel: channel,
        }
    }

    /// `task_id` changed.
    pub(super) fn notify(&self, task_id: &str) {
        let channel = self
            .watched
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(task_id)
            .and_then(std::sync::Weak::upgrade);
        if let Some(channel) = channel {
            channel.send_modify(|version| *version += 1);
        }
    }

    /// The task `event` is about changed.
    pub(super) fn notify_event(&self, event: &StreamResponse) {
        if let Some(task_id) = task_id_for_event(event) {
            self.notify(task_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

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

    #[tokio::test]
    async fn test_subscribe_to_a_missing_task_is_not_found() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = SubscribeToTaskRequest {
            last_seen_generation: None,
            id: "t1".into(),
            tenant: None,
        };
        let result = handler.subscribe_to_task(&params, req).await;
        match result {
            Ok(_) => panic!("expected subscribe_to_task to fail"),
            Err(error) => assert_eq!(error.code, error_code::TASK_NOT_FOUND),
        }
    }

    #[tokio::test]
    async fn test_a_subscriber_that_falls_behind_starts_over_from_the_task() {
        // A subscriber further behind than the buffer holds is given the task as
        // it stands and carries on after it, instead of losing its stream.
        let task = Task {
            id: "t-lag".into(),
            context_id: "c".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            timeline: None,
            metadata: None,
            generation: 7,
        };
        let active = ActiveExecution::new(Some(task));
        let receiver = active.subscribe();
        let probe = |text: &str| {
            StreamResponse::Message(a2a::Message {
                message_id: text.to_string(),
                role: a2a::Role::Agent,
                parts: Vec::new(),
                context_id: None,
                task_id: None,
                metadata: None,
                extensions: None,
                reference_task_ids: None,
            })
        };
        for i in 0..(EXECUTION_BUFFER_CAPACITY * 3) {
            active.publish(Ok(&probe(&format!("lost-{i}"))), None).await;
        }

        let mut stream = subscription_stream(receiver, None, 0, true, false, false, None);
        let first = stream.next().await.expect("an item").expect("not an error");
        assert!(
            matches!(&first, StreamResponse::Task(task) if task.generation == 7),
            "{first:?}"
        );
        // And what is published afterwards follows.
        active.publish(Ok(&probe("after")), None).await;
        let next = stream.next().await.expect("an item").expect("not an error");
        assert!(
            matches!(&next, StreamResponse::Message(m) if m.message_id == "after"),
            "{next:?}"
        );
    }
}
