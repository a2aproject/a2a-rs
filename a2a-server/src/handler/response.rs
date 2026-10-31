// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Follow task output with a shared runner and an explicit completion policy.

use super::delivery::{Delivery, StartMode};
use super::exchange::Exchange;
use super::output::{DeliveryEvent, DeliverySink, EventSource};
use super::protocol::{
    Rejection, apply_lengths, generation_of, is_at_least_1_1, is_terminal_event,
    requested_protocol_version, should_interrupt_non_streaming,
};
use super::subscription::{DeliveryAttachment, SubscriptionPolicy};
use super::uplink::{Incoming, IncomingRequest, Receipt};
use super::{DefaultRequestHandler, RESPONSE_BUFFER_CAPACITY};
use crate::middleware::ServiceParams;
use a2a::*;
use futures::{FutureExt, StreamExt, stream::BoxStream};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// What looking at an idle task found.
enum Reattach {
    /// The exchange is over (the task ended, or is gone).
    End,
    /// The exchange follows a running execution now, or was told of a change.
    Changed,
    Unchanged,
}

/// Follows task events and optionally accepts requests through an uplink.
pub(super) struct Run {
    pub(super) handler: DefaultRequestHandler,
    /// The caller's credentials and version. Uplink requests may carry their
    /// own parameters when another caller sends them.
    pub(super) params: ServiceParams,
    /// The task whose execution and updates this response follows.
    pub(super) task_id: TaskId,
    pub(super) wants_message_updates: bool,
    pub(super) completion: RunCompletion,
    response_id: Option<uuid::Uuid>,
}

/// When a response has observed enough of the task to finish.
pub(super) enum RunCompletion {
    /// Follow task updates across execution turns until the task is terminal.
    Task,
    /// Finish at the unary request's return condition, including interruptions.
    Unary(Box<SendMessageRequest>),
    /// Follow later turns while the uplink remains open.
    Exchange,
    /// Drain ordered webhook input across turns, retiring after its idle timeout.
    Notifications,
}

pub(super) type Events = BoxStream<'static, Result<StreamResponse, A2AError>>;

impl Run {
    pub(super) fn new(
        handler: DefaultRequestHandler,
        params: ServiceParams,
        task_id: TaskId,
        completion: RunCompletion,
    ) -> Self {
        let wants_message_updates = matches!(completion, RunCompletion::Notifications)
            || is_at_least_1_1(&requested_protocol_version(&params));
        Self {
            handler,
            params,
            task_id,
            wants_message_updates,
            completion,
            response_id: None,
        }
    }

    pub(super) fn with_response_id(mut self, response_id: Option<uuid::Uuid>) -> Self {
        self.response_id = response_id;
        self
    }

    /// Owns response delivery independently of the executor task. Dropping the
    /// receiver stops this runner, while the executor continues its task work.
    pub(super) fn start(
        self,
        opening: Option<Task>,
        inner: Option<Events>,
        seen_generation: i64,
        uplink: Option<Exchange>,
    ) -> Events {
        let (out, downlink) = mpsc::channel(RESPONSE_BUFFER_CAPACITY);
        self.spawn(
            opening,
            inner.map(EventSource::Subscription),
            seen_generation,
            uplink,
            DeliverySink::Stream(out),
        );
        ReceiverStream::new(downlink).boxed()
    }

    pub(super) fn start_notifications(self, source: EventSource, sink: DeliverySink) {
        self.spawn(None, Some(source), 0, None, sink);
    }

    fn spawn(
        self,
        opening: Option<Task>,
        inner: Option<EventSource>,
        seen_generation: i64,
        uplink: Option<Exchange>,
        out: DeliverySink,
    ) {
        let failure_sink = out.clone();
        tokio::spawn(async move {
            let outcome = std::panic::AssertUnwindSafe(self.run(
                opening,
                inner,
                seen_generation,
                uplink,
                out,
            ))
            .catch_unwind()
            .await;
            if outcome.is_err() {
                failure_sink.report_panic().await;
            }
        });
    }

    /// Converts the shared runner's output into the single unary response.
    pub(super) async fn unary_response(
        handler: DefaultRequestHandler,
        task_id: TaskId,
        mut events: Events,
        history_length: Option<i32>,
        timeline_length: Option<i32>,
    ) -> Result<SendMessageResponse, A2AError> {
        while let Some(item) = events.next().await {
            if let StreamResponse::Message(message) = item? {
                return Ok(SendMessageResponse::Message(message));
            }
        }
        let mut task = handler.load_task(&task_id).await?;
        apply_lengths(&mut task, history_length, timeline_length);
        Ok(SendMessageResponse::Task(task))
    }

    fn completes(&self, event: &StreamResponse) -> bool {
        if matches!(self.completion, RunCompletion::Notifications) {
            return false;
        }
        is_terminal_event(event)
            || matches!(event, StreamResponse::Message(_))
            || match &self.completion {
                RunCompletion::Unary(request) => {
                    should_interrupt_non_streaming(request, event).is_some()
                }
                _ => false,
            }
    }

    fn subscription_policy(&self) -> SubscriptionPolicy {
        let mode = match self.completion {
            RunCompletion::Unary(_) => StartMode::Blocking,
            RunCompletion::Task | RunCompletion::Notifications => StartMode::Streaming,
            RunCompletion::Exchange => StartMode::Bidi,
        };
        SubscriptionPolicy::new(mode, self.wants_message_updates).for_response(self.response_id)
    }

    /// Forward one event, keeping generation tracking and response completion
    /// identical whether it arrives normally or while flushing queued output.
    /// Returns false after completion or when the downlink is closed.
    async fn forward(
        &self,
        item: DeliveryEvent,
        last_generation: &mut i64,
        out: &DeliverySink,
    ) -> bool {
        let ends = item
            .result
            .as_ref()
            .map_or(true, |event| self.completes(event));
        if let Some(generation) = item.result.as_ref().ok().and_then(generation_of) {
            *last_generation = (*last_generation).max(generation);
        }
        out.send(item).await && !ends
    }

    fn run(
        self,
        opening: Option<Task>,
        mut inner: Option<EventSource>,
        seen_generation: i64,
        mut uplink: Option<Exchange>,
        out: DeliverySink,
    ) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(async move {
            // Watched from here, so a change from now on is seen however it is
            // timed against the looks below.
            let mut watch = self.handler.changes.watch(&self.task_id);
            // The generation the client has been told of, so a change it was not
            // there for (an idle task, then another client's turn) is noticed.
            let mut last_generation = seen_generation;
            if let Some(task) = opening {
                if !self
                    .forward(
                        Ok(StreamResponse::Task(task)).into(),
                        &mut last_generation,
                        &out,
                    )
                    .await
                {
                    return self.finish(uplink).await;
                }
            }
            let mut uplink_open = uplink.is_some();
            // What happened between the opening and the watch is looked at once.
            if inner.is_none()
                && matches!(
                    self.reattach(&mut inner, &mut last_generation, &out).await,
                    Reattach::End
                )
            {
                return self.finish(uplink).await;
            }
            loop {
                tokio::select! {
                    // The client went away: nothing is left to deliver to.
                    () = out.closed() => break,
                    event = async {
                        match inner.as_mut() {
                            Some(events) => events.next().await,
                            None => std::future::pending().await,
                        }
                    } => match event {
                        Some(item) => {
                            if !self.forward(item, &mut last_generation, &out).await {
                                break;
                            }
                        }
                        None => {
                            // The execution is over. The task may not be: the
                            // exchange stays open for the next message.
                            inner = None;
                            // The webhook source already spans turns and drains
                            // before retirement; it must never replay snapshots.
                            if matches!(self.completion, RunCompletion::Notifications) { break; }
                            if matches!(self.completion, RunCompletion::Exchange) && !uplink_open {
                                break;
                            }
                            // Watch registration predates this read, so a turn or
                            // cancellation that raced the old stream's closure is
                            // observed even if no further notification arrives.
                            if matches!(self.reattach(&mut inner, &mut last_generation, &out).await, Reattach::End) {
                                break;
                            }
                        }
                    },
                    // Nothing is running that this exchange follows: it watches the
                    // task, so another client's turn, or a cancel, reaches it too.
                    () = watch.changed(), if inner.is_none() => {
                        if matches!(
                            self.reattach(&mut inner, &mut last_generation, &out).await,
                            Reattach::End
                        ) {
                            break;
                        }
                    },
                    item = async {
                        match uplink.as_mut() {
                            Some(uplink) => uplink.next().await,
                            None => std::future::pending().await,
                        }
                    }, if uplink_open => match item {
                        Some(item) => {
                            if !self.handle(item, &mut inner, &mut last_generation, &out).await {
                                break;
                            }
                        }
                        None => {
                            uplink_open = false;
                            if inner.is_none() {
                                break;
                            }
                        }
                    },
                }
            }
            self.finish(uplink).await;
        })
    }
    /// Ends the exchange: no more calls are routed to it, and a call that was
    /// queued but never reached is told so (it is delivered nowhere, so the
    /// caller may open a new exchange with it).
    async fn finish(&self, uplink: Option<Exchange>) {
        if let Some(exchange) = uplink {
            exchange.close().await;
        }
    }

    /// Looks at the task while the exchange follows no execution. Follows a new
    /// one if it has started, tells the client of changes it was not there for
    /// (and that the task has ended). `false` when the exchange is over.
    async fn reattach(
        &self,
        inner: &mut Option<EventSource>,
        last_generation: &mut i64,
        out: &DeliverySink,
    ) -> Reattach {
        if let Some((receiver, Some(snapshot), sequence)) = self
            .handler
            .execution_manager
            .resubscribe(&self.task_id)
            .await
        {
            let event = StreamResponse::Task(snapshot.clone());
            let ends = self.completes(&event);
            let opening =
                (snapshot.generation > *last_generation || ends).then_some(snapshot.clone());
            *last_generation = (*last_generation).max(snapshot.generation);
            *inner = Some(
                self.subscription_policy()
                    .stream(receiver, opening, sequence)
                    .into(),
            );
            return Reattach::Changed;
        }
        let task = match self.handler.load_task(&self.task_id).await {
            Ok(task) => task,
            Err(error) => {
                self.forward(Err(error).into(), last_generation, out).await;
                return Reattach::End;
            }
        };
        let ended = self.completes(&StreamResponse::Task(task.clone()));
        let changed = task.generation > *last_generation;
        if (ended || changed)
            && !self
                .forward(Ok(StreamResponse::Task(task)).into(), last_generation, out)
                .await
        {
            return Reattach::End;
        }
        if ended {
            Reattach::End
        } else if changed {
            Reattach::Changed
        } else {
            Reattach::Unchanged
        }
    }

    /// Carries out one request that arrived after the exchange opened. Returns
    /// `false` when the downlink is gone.
    async fn handle(
        &self,
        item: Incoming,
        inner: &mut Option<EventSource>,
        last_generation: &mut i64,
        out: &DeliverySink,
    ) -> bool {
        let Incoming { request, receipt } = item;
        // Mark processing before delivery: a lost task now leaves an unknown
        // outcome instead of a receipt saying that nothing was handled.
        let receipt = receipt.map(Receipt::begin);
        let reference_id = request.reference_id();
        let result = self
            .deliver(&self.params, request, inner, last_generation, out)
            .await;
        let mut downlink_open = *result.as_ref().unwrap_or(&true);
        if let Err(rejection) = result {
            // Ordering is by arrival: what the exchange has already published
            // for earlier requests goes out before this one's rejection.
            downlink_open = self.flush(inner, last_generation, out).await;
            if downlink_open {
                let event = rejection.into_stream_error(&self.task_id, reference_id);
                downlink_open = self.forward(Ok(event).into(), last_generation, out).await;
            }
        }
        if let Some(receipt) = receipt {
            receipt.complete(self.handler.load_task(&self.task_id).await);
        }
        downlink_open
    }

    /// Forwards what the execution being followed has published already,
    /// without waiting for more. `false` when the downlink is gone or the
    /// exchange ended with a terminal event.
    async fn flush(
        &self,
        inner: &mut Option<EventSource>,
        last_generation: &mut i64,
        out: &DeliverySink,
    ) -> bool {
        while let Some(events) = inner.as_mut() {
            match events.next().now_or_never() {
                Some(Some(item)) => {
                    if !self.forward(item, last_generation, out).await {
                        return false;
                    }
                }
                Some(None) => *inner = None,
                None => break,
            }
        }
        true
    }

    async fn deliver(
        &self,
        params: &ServiceParams,
        request: IncomingRequest,
        inner: &mut Option<EventSource>,
        last_generation: &mut i64,
        out: &DeliverySink,
    ) -> Result<bool, Rejection> {
        let delivery = self
            .handler
            .receive_message(
                params,
                request,
                StartMode::Bidi,
                Some(&self.task_id),
                self.response_id,
            )
            .await?;
        let starts_turn = matches!(delivery, Delivery::Started { .. });
        // An attached stream already receives injected inputs from its current
        // subscription. Replacing it could lose events that are still queued.
        if !starts_turn && inner.is_some() {
            return Ok(true);
        }
        if starts_turn && !self.flush(inner, last_generation, out).await {
            return Ok(false);
        }
        let subscription = self
            .subscription_policy()
            .delivery(delivery, DeliveryAttachment::FollowUp);
        *inner = Some(EventSource::Subscription(subscription.events));
        // A new turn recorded input before it had subscribers. Announce it
        // once here; injected inputs arrive through their subscription instead.
        for event in subscription.recorded {
            if matches!(event, StreamResponse::MessageUpdate(_)) && !self.wants_message_updates {
                continue;
            }
            if !self.forward(Ok(event).into(), last_generation, out).await {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn blocking_response_waits_after_a_working_turn_ends() {
        let handler = make_handler();
        seed_task(&handler, "working", "context").await;
        let mut task = handler.load_task("working").await.unwrap();
        task.status.state = TaskState::Working;
        handler.task_store.update(task.clone()).await.unwrap();
        let drained = Arc::new(Notify::new());
        let signal = drained.clone();
        let events = Box::pin(async_stream::stream! {
            yield Ok(StreamResponse::Task(task));
            signal.notify_one();
        });
        let run = Run::new(
            handler.clone(),
            ServiceParams::new(),
            "working".into(),
            RunCompletion::Unary(Box::new(send_req(Some("working"), None))),
        );
        let events = run.start(None, Some(events), 0, None);
        let mut response = tokio::spawn(Run::unary_response(
            handler.clone(),
            "working".into(),
            events,
            None,
            None,
        ));
        drained.notified().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut response)
                .await
                .is_err(),
            "ending the executor turn must not return a working task"
        );
        handler
            .cancel_task(
                &ServiceParams::new(),
                CancelTaskRequest {
                    id: "working".into(),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), response)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let SendMessageResponse::Task(task) = result else {
            panic!("expected a task")
        };
        assert_eq!(task.status.state, TaskState::Canceled);
    }
    #[tokio::test]
    async fn queued_and_normal_events_obey_the_same_completion_policy() {
        let handler = make_handler();
        seed_task(&handler, "task", "context").await;
        let task = handler.load_task("task").await.unwrap();
        for mode in [StartMode::Blocking, StartMode::Streaming, StartMode::Bidi] {
            let completion = || match mode {
                StartMode::Blocking => RunCompletion::Unary(Box::new(send_req(Some("task"), None))),
                StartMode::Streaming => RunCompletion::Task,
                StartMode::Bidi => RunCompletion::Exchange,
            };
            let events = || -> Events {
                futures::stream::iter(
                    [
                        TaskState::Working,
                        TaskState::InputRequired,
                        TaskState::Completed,
                    ]
                    .into_iter()
                    .enumerate()
                    .map(|(generation, state)| {
                        let mut snapshot = task.clone();
                        snapshot.status.state = state;
                        snapshot.generation = generation as i64 + 1;
                        Ok(StreamResponse::Task(snapshot))
                    })
                    .collect::<Vec<_>>(),
                )
                .boxed()
            };
            let normal = Run::new(
                handler.clone(),
                ServiceParams::new(),
                "task".into(),
                completion(),
            )
            .start(None, Some(events()), 0, None);
            let normal = tokio::time::timeout(Duration::from_secs(2), normal.collect::<Vec<_>>())
                .await
                .unwrap();
            let run = Run::new(
                handler.clone(),
                ServiceParams::new(),
                "task".into(),
                completion(),
            );
            let (out, mut received) = mpsc::channel(8);
            let out = DeliverySink::Stream(out);
            let mut generation = 0;
            assert!(
                !run.flush(
                    &mut Some(EventSource::Subscription(events())),
                    &mut generation,
                    &out
                )
                .await
            );
            let mut queued = Vec::new();
            while let Ok(item) = received.try_recv() {
                queued.push(item);
            }
            let expected_count = if mode == StartMode::Blocking { 2 } else { 3 };
            assert_eq!(normal.len(), expected_count);
            assert_eq!(
                queued.into_iter().map(Result::unwrap).collect::<Vec<_>>(),
                normal.into_iter().map(Result::unwrap).collect::<Vec<_>>()
            );
            assert_eq!(generation, expected_count as i64);
        }
    }

    #[tokio::test]
    async fn flushing_a_direct_reply_does_not_forward_later_events() {
        let run = Run::new(
            make_handler(),
            ServiceParams::new(),
            "task".into(),
            RunCompletion::Exchange,
        );
        let mut events = Some(EventSource::Subscription(
            futures::stream::iter([
                Ok(StreamResponse::Message(Message::new(
                    Role::Agent,
                    vec![Part::text("reply")],
                ))),
                Err(A2AError::internal("after the reply")),
            ])
            .boxed(),
        ));
        let (out, mut received) = mpsc::channel(4);
        let out = DeliverySink::Stream(out);
        assert!(!run.flush(&mut events, &mut 0, &out).await);
        assert!(matches!(
            received.try_recv(),
            Ok(Ok(StreamResponse::Message(_)))
        ));
        assert!(received.try_recv().is_err());
    }
}
