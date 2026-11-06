// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Publish execution events, filter subscriptions, and notify task watchers.

#[cfg(test)]
use super::EXECUTION_BUFFER_CAPACITY;
use super::TASK_INPUT_CAPACITY;
use super::execution::InvocationState;
use super::protocol::{EventDecision, EventFilter, EventKind};
use super::task_handle::TaskHandle;
use crate::executor::{ClientInputSender, ClientInputs};
use a2a::*;
use futures::stream::BoxStream;
use std::sync::Arc;
use tokio::sync::{broadcast, watch};

pub(super) type FailureSignal = watch::Sender<Option<A2AError>>;

#[derive(Clone)]
pub(super) struct ExecutionEvent {
    pub(super) sequence: u64,
    /// Highest input generation read when this output was emitted, not when it was delivered.
    pub(super) input_generation: i64,
    pub(super) result: Result<StreamResponse, A2AError>,
    pub(super) kind: EventKind,
    pub(super) failure: Option<FailureSignal>,
    /// Captured only for terminal/interrupted status events, shared by readers.
    pub(super) response_task: Option<Arc<Task>>,
}

pub(super) struct ExecutionReceiver {
    receiver: broadcast::Receiver<ExecutionEvent>,
    /// Keep the Task source, not the executor invocation, alive.
    task: TaskHandle,
    finished: Option<tokio_util::sync::CancellationToken>,
    opening_message: bool,
    /// Events at or before this subscription opened must not be replayed.
    opened_at: u64,
    reply: Option<Arc<super::task_handle::ReplyRetention>>,
    required_input: i64,
}

impl ExecutionReceiver {
    /// Subscribe to the task source independently of any executor invocation.
    pub(super) fn for_task(task: TaskHandle) -> (Self, Option<Task>, u64) {
        let (receiver, snapshot, sequence, reply) = task.response_subscription();
        (
            Self {
                receiver,
                task,
                finished: None,
                opening_message: false,
                opened_at: sequence,
                reply: Some(reply),
                required_input: 0,
            },
            snapshot,
            sequence,
        )
    }

    pub(super) fn task(&self) -> TaskHandle {
        self.task.clone()
    }

    pub(super) fn for_input(mut self, generation: i64) -> Self {
        self.required_input = generation;
        self
    }

    /// Streaming responses follow the Task source across invocation boundaries.
    pub(super) fn follow_task(mut self) -> Self {
        self.finished = None;
        self
    }

    /// A streaming send must still produce its opening snapshot if the agent
    /// ends without emitting a visible event. Then it follows the Task source.
    pub(super) fn opening_message(mut self) -> Self {
        self.opening_message = true;
        self
    }

    fn opened(&mut self) {
        if self.opening_message {
            self.opening_message = false;
            self.finished = None;
        }
    }

    pub(super) async fn recv(&mut self) -> Result<ExecutionEvent, broadcast::error::RecvError> {
        let received = match &self.finished {
            None => self.receiver.recv().await,
            Some(finished) => tokio::select! {
                biased;
                event = self.receiver.recv() => event,
                _ = finished.cancelled() => Err(broadcast::error::RecvError::Closed),
            },
        };
        if matches!(received, Err(broadcast::error::RecvError::Lagged(_)))
            && let Some(retained) = &self.reply
            && let Some(reply) = retained.reply.get()
        {
            return Ok(reply.clone());
        }
        received
    }

    #[cfg(test)]
    pub(super) fn try_recv(&mut self) -> Result<ExecutionEvent, broadcast::error::TryRecvError> {
        self.receiver.try_recv()
    }

    /// Starts over after the events were lost: the task as it stands, and the
    /// sequence after which events follow it. `None` when no Task exists yet.
    fn resync(&mut self, after: u64) -> Result<Option<(Task, u64, i64)>, A2AError> {
        let (receiver, snapshot, sequence, input_generation) = self.task.resynchronize(after)?;
        self.receiver = receiver;
        Ok(snapshot.map(|task| (task, sequence, input_generation)))
    }
}

/// Shareable invocation controls and published-state projection. Mutable input
/// bookkeeping is exclusively owned by the task runtime.
pub(super) struct ActiveExecution {
    pub(super) execution_id: String,
    pub(super) task: TaskHandle,
    pub(super) finished: tokio_util::sync::CancellationToken,
    prepared: std::sync::atomic::AtomicBool,
    /// Where messages sent to this task go while the executor runs. The queue
    /// belongs to the execution turn and is handed off with unread inputs.
    pub(super) inputs: ClientInputSender,
    /// Completed after the runner has revoked the inbox and rejected unread input.
    pub(super) inputs_drained: tokio_util::sync::CancellationToken,
    pub(super) push_failure: FailureSignal,
    /// Set while the task this execution is for has not been created: a new
    /// task is created by the executor's first event, and does not exist if
    /// the executor answers with a message alone.
    pub(super) uncreated: std::sync::atomic::AtomicBool,
    output_input_generation: std::sync::atomic::AtomicI64,
}

impl ActiveExecution {
    #[cfg(test)]
    pub(super) fn new(task: Option<Task>) -> Arc<Self> {
        let prepared = task.is_some();
        let (active, _inputs, _state) = Self::with_task(TaskHandle::new(
            task.as_ref()
                .map(|task| task.id.clone())
                .unwrap_or_default(),
            task,
        ));
        active
            .prepared
            .store(prepared, std::sync::atomic::Ordering::SeqCst);
        active
    }

    pub(super) fn with_task(task: TaskHandle) -> (Arc<Self>, ClientInputs, InvocationState) {
        let (inputs, receiver, returned_inputs) = ClientInputs::for_execution(TASK_INPUT_CAPACITY);
        (
            Arc::new(Self {
                execution_id: uuid::Uuid::now_v7().to_string(),
                task,
                finished: tokio_util::sync::CancellationToken::new(),
                prepared: std::sync::atomic::AtomicBool::new(false),
                inputs,
                inputs_drained: tokio_util::sync::CancellationToken::new(),
                push_failure: tokio::sync::watch::channel(None).0,
                uncreated: std::sync::atomic::AtomicBool::new(false),
                output_input_generation: std::sync::atomic::AtomicI64::new(0),
            }),
            receiver,
            InvocationState::new(returned_inputs),
        )
    }

    /// The most recently published execution event, read under the state lock.
    pub(super) fn sequence(&self) -> u64 {
        self.task.sequence()
    }

    pub(super) fn is_prepared(&self) -> bool {
        self.prepared.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub(super) fn snapshot(&self) -> Option<Task> {
        if self.prepared.load(std::sync::atomic::Ordering::SeqCst) {
            self.task.snapshot()
        } else {
            None
        }
    }

    pub(super) fn set_snapshot(&self, task: Task) {
        self.task.set_snapshot(task);
        self.prepared
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub(super) fn subscribe(self: &Arc<Self>) -> ExecutionReceiver {
        let (receiver, _, sequence, reply) = self.task.response_subscription();
        ExecutionReceiver {
            receiver,
            task: self.task.clone(),
            finished: Some(self.finished.clone()),
            opening_message: false,
            opened_at: sequence,
            reply: Some(reply),
            required_input: 0,
        }
    }

    pub(super) async fn publish(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot_task: Option<Task>,
    ) {
        self.publish_kind(result, snapshot_task, EventKind::Normal)
            .await;
    }

    /// Capture causality before persistence or publication can await other work.
    pub(super) fn emitted_output(&self) {
        self.output_input_generation.store(
            self.inputs.received_generation(),
            std::sync::atomic::Ordering::Release,
        );
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

    /// Publish an already-persisted input batch with its final snapshot.
    pub(super) async fn publish_input(&self, task: Task, events: &[StreamResponse]) {
        self.prepared
            .store(true, std::sync::atomic::Ordering::SeqCst);
        self.task
            .publish_input(task, events, self.push_failure.clone())
            .await;
    }

    pub(super) async fn publish_kind(
        &self,
        result: Result<&StreamResponse, &A2AError>,
        snapshot_task: Option<Task>,
        kind: EventKind,
    ) {
        if snapshot_task.is_some() {
            self.prepared
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.task
            .publish(
                result,
                snapshot_task,
                kind,
                Some(self.push_failure.clone()),
                self.output_input_generation
                    .load(std::sync::atomic::Ordering::Acquire),
            )
            .await;
    }

    pub(super) fn snapshot_is_terminal(&self) -> bool {
        self.task
            .snapshot()
            .as_ref()
            .is_some_and(|task| task.status.state.is_terminal())
    }
}

impl Drop for ActiveExecution {
    fn drop(&mut self) {
        // A Task broadcast outlives its invocation. Response subscriptions still
        // observe invocation closure after the driver's final error/publication.
        self.finished.cancel();
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
    filter: EventFilter,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    let min_sequence = min_sequence.max(receiver.opened_at);
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
                state.receiver.opened();
                if task.status.state.is_terminal() {
                    state.done = true;
                }
                return Some((Ok(StreamResponse::Task(task)), state));
            }

            loop {
                match state.receiver.recv().await {
                    Ok(event) if event.sequence <= state.min_sequence => continue,
                    Ok(event) => {
                        state.min_sequence = event.sequence;
                        let decision = filter.decide(event.kind, &event.result);
                        if matches!(decision, EventDecision::Skip) {
                            continue;
                        }
                        if event.result.as_ref().is_ok_and(|output| {
                            filter.waits_for_input(
                                state.receiver.required_input,
                                event.input_generation,
                                output,
                            )
                        }) {
                            continue;
                        }
                        state.receiver.opened();
                        state.done = matches!(decision, EventDecision::DeliverAndClose);
                        let result = if filter.task_snapshots {
                            event.response_task.as_ref().map_or_else(
                                || event.result.clone(),
                                |task| Ok(StreamResponse::Task((**task).clone())),
                            )
                        } else {
                            event.result.clone()
                        };
                        return Some((result, state));
                    }
                    // Events were lost: start over from the task as it stands.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        match state.receiver.resync(state.min_sequence) {
                            Ok(Some((task, sequence, input_generation))) => {
                                if filter.waits_for_input(
                                    state.receiver.required_input,
                                    input_generation,
                                    &StreamResponse::Task(task.clone()),
                                ) {
                                    state.min_sequence = sequence;
                                    continue;
                                }
                                state.receiver.opened();
                                state.min_sequence = sequence;
                                if task.status.state.is_terminal() {
                                    state.done = true;
                                }
                                return Some((Ok(StreamResponse::Task(task)), state));
                            }
                            Ok(None) => return None,
                            Err(error) => {
                                state.done = true;
                                return Some((Err(error), state));
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) if state.receiver.opening_message => {
                        match state.receiver.resync(state.min_sequence) {
                            Ok(Some((task, sequence, input_generation))) => {
                                if filter.waits_for_input(
                                    state.receiver.required_input,
                                    input_generation,
                                    &StreamResponse::Task(task.clone()),
                                ) {
                                    // This invocation ended without answering this input. A successor
                                    // can still consume it, so follow the task instead of the old turn.
                                    state.receiver.finished = None;
                                    state.min_sequence = sequence;
                                    continue;
                                }
                                state.receiver.opened();
                                state.min_sequence = sequence;
                                state.done = task.status.state.is_terminal();
                                return Some((Ok(StreamResponse::Task(task)), state));
                            }
                            Ok(None) => return None,
                            Err(error) => {
                                state.done = true;
                                return Some((Err(error), state));
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    ))
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

        let (mut receiver, snapshot_task, sequence) =
            ExecutionReceiver::for_task(active.task.clone());
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
        let (mut receiver, snapshot_task, sequence) =
            ExecutionReceiver::for_task(active.task.clone());
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
        let (_, snapshot_task, sequence) = ExecutionReceiver::for_task(active.task.clone());
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
        for _ in 0..(EXECUTION_BUFFER_CAPACITY * 3) {
            active
                .publish(Ok(&StreamResponse::Task(active.snapshot().unwrap())), None)
                .await;
        }

        let mut stream = subscription_stream(
            receiver,
            None,
            0,
            EventFilter {
                include_repeats: true,
                task_snapshots: false,
                message_updates: false,
                live: false,
                response_id: None,
            },
        );
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

    #[tokio::test]
    async fn terminal_snapshots_close_subscriptions_including_after_broadcast_lag() {
        for lagged in [false, true] {
            let mut task = task_with_history("terminal-snapshot", "context", 0);
            task.status.state = TaskState::Working;
            let active = ActiveExecution::new(Some(task.clone()));
            let receiver = active.subscribe();
            if lagged {
                for _ in 0..EXECUTION_BUFFER_CAPACITY + 2 {
                    active
                        .publish(Ok(&StreamResponse::Task(task.clone())), None)
                        .await;
                }
            }
            task.status.state = TaskState::Completed;
            task.generation = 99;
            active
                .publish(Ok(&StreamResponse::Task(task.clone())), Some(task.clone()))
                .await;
            let opening = (!lagged).then_some(task);
            let mut events = subscription_stream(
                receiver,
                opening,
                0,
                EventFilter {
                    include_repeats: false,
                    task_snapshots: false,
                    message_updates: true,
                    live: false,
                    response_id: None,
                },
            );
            assert!(
                matches!(events.next().await.unwrap().unwrap(), StreamResponse::Task(task) if task.status.state == TaskState::Completed && task.generation == 99)
            );
            assert!(
                events.next().await.is_none(),
                "a terminal resync must finish without waiting for another event"
            );
        }
    }
    #[tokio::test]
    async fn lagged_subscribers_recover_errors_across_executor_handoffs() {
        use crate::handler::test_support::make_handler;
        use futures::StreamExt;
        let manager = make_handler().tasks;
        let mut task = task_with_history("handoff-error", "context", 0);
        task.status.state = TaskState::Working;
        let (first, _inputs, _invocation) = manager.task(&task.id).reserve().unwrap();
        first.set_snapshot(task.clone());
        let (receiver, _, sequence) = ExecutionReceiver::for_task(first.task.clone());
        let mut old_response = subscription_stream(
            receiver,
            None,
            sequence,
            EventFilter {
                include_repeats: false,
                task_snapshots: false,
                message_updates: true,
                live: false,
                response_id: None,
            },
        );
        let error = A2AError::internal("executor failed");
        first.publish(Err(&error), None).await;
        manager.task(&task.id).unregister(&first);
        drop(first);

        let (second, _inputs, _invocation) = manager.task(&task.id).reserve().unwrap();
        second.set_snapshot(task.clone());
        for _ in 0..EXECUTION_BUFFER_CAPACITY + 1 {
            task.generation += 1;
            second
                .publish(Ok(&StreamResponse::Task(task.clone())), Some(task.clone()))
                .await;
        }
        let recovered = old_response.next().await.unwrap().unwrap_err();
        assert_eq!(recovered.code, error.code);
        assert_eq!(recovered.message, error.message);
        assert!(old_response.next().await.is_none());

        // A response opened after the error follows the new turn normally.
        let receiver = second.subscribe();
        let mut new_response = subscription_stream(
            receiver,
            Some(task.clone()),
            0,
            EventFilter {
                include_repeats: false,
                task_snapshots: false,
                message_updates: true,
                live: false,
                response_id: None,
            },
        );
        assert!(matches!(
            new_response.next().await,
            Some(Ok(StreamResponse::Task(_)))
        ));
        for _ in 0..EXECUTION_BUFFER_CAPACITY + 1 {
            task.generation += 1;
            second
                .publish(Ok(&StreamResponse::Task(task.clone())), Some(task.clone()))
                .await;
        }
        assert!(matches!(
            new_response.next().await,
            Some(Ok(StreamResponse::Task(snapshot))) if snapshot.generation == task.generation
        ));
        manager.task("handoff-error").unregister(&second);
    }
}
