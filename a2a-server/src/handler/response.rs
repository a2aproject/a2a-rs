// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Follow task output with a shared runner and an explicit completion policy.

use super::delivery::StartMode;
use super::http_exchange::RegistrationLease;
use super::protocol::{
    Rejection, generation_of, is_terminal_event, should_interrupt_non_streaming,
};
use super::response_preparation::{DeliveryAttachment, SubscriptionPolicy};
use super::task_handle::TaskHandle;
use super::uplink::{Incoming, IncomingRequest, Receipt, Uplink};
use super::{DefaultRequestHandler, RESPONSE_BUFFER_CAPACITY};
use crate::middleware::ServiceParams;
use a2a::*;
use futures::{FutureExt, StreamExt, stream::BoxStream};
use std::sync::Arc;
use tokio::sync::mpsc;

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
    // Rust drops fields in declaration order. Unregister before dropping the
    // receiver, including when this run unwinds after a panic.
    registration: Option<RegistrationLease>,
    uplink: Option<Uplink>,
    pub(super) handler: DefaultRequestHandler,
    /// The caller's credentials and version. Uplink requests may carry their
    /// own parameters when another caller sends them.
    pub(super) params: ServiceParams,
    /// The task whose execution and updates this response follows.
    pub(super) task: Arc<TaskHandle>,
    pub(super) completion: RunCompletion,
    policy: SubscriptionPolicy,
}

/// When a response has observed enough of the task to finish.
pub(super) enum RunCompletion {
    /// Follow task updates across execution turns until the task is terminal.
    Task,
    /// Finish at the unary request's return condition, including interruptions.
    Unary { return_immediately: bool },
    /// Follow later turns while the uplink remains open.
    Exchange,
}

pub(super) type Events = BoxStream<'static, Result<StreamResponse, A2AError>>;
type ResponseSender = mpsc::Sender<Result<StreamResponse, A2AError>>;

impl Run {
    pub(super) fn new(
        handler: DefaultRequestHandler,
        params: ServiceParams,
        task: Arc<TaskHandle>,
        completion: RunCompletion,
        policy: SubscriptionPolicy,
    ) -> Self {
        Self {
            registration: None,
            uplink: None,
            handler,
            params,
            task,
            completion,
            policy,
        }
    }

    pub(super) fn with_uplink(
        mut self,
        uplink: Option<Uplink>,
        mut registration: Option<RegistrationLease>,
    ) -> Self {
        if let Some(registration) = &mut registration {
            registration.bind(&self.task.id);
        }
        self.registration = registration;
        self.uplink = uplink;
        self
    }

    /// Owns response delivery independently of the executor task. Dropping the
    /// receiver stops this runner, while the executor continues its task work.
    pub(super) fn start(
        self,
        opening: Option<Task>,
        inner: Option<Events>,
        seen_generation: i64,
    ) -> Events {
        let (out, downlink) = mpsc::channel(RESPONSE_BUFFER_CAPACITY);
        let source = self.task.clone();
        self.spawn(opening, inner, seen_generation, out);
        // Keep the authoritative snapshot until the response is consumed,
        // including when retention purges the task before unary delivery.
        futures::stream::unfold((downlink, source), |(mut downlink, source)| async move {
            let event = downlink.recv().await?;
            Some((event, (downlink, source)))
        })
        .boxed()
    }

    fn spawn(
        self,
        opening: Option<Task>,
        inner: Option<Events>,
        seen_generation: i64,
        out: ResponseSender,
    ) {
        let failure_sink = out.clone();
        tokio::spawn(async move {
            let outcome =
                std::panic::AssertUnwindSafe(self.run(opening, inner, seen_generation, out))
                    .catch_unwind()
                    .await;
            if outcome.is_err() {
                let _ = failure_sink
                    .send(Err(A2AError::internal("the response runner failed")))
                    .await;
            }
        });
    }

    fn completes(&self, event: &StreamResponse) -> bool {
        is_terminal_event(event)
            || matches!(event, StreamResponse::Message(_))
            || match &self.completion {
                RunCompletion::Unary { return_immediately } => {
                    should_interrupt_non_streaming(*return_immediately, event).is_some()
                }
                _ => false,
            }
    }

    /// Forward one event, keeping generation tracking and response completion
    /// identical whether it arrives normally or while flushing queued output.
    /// Returns false after completion or when the downlink is closed.
    async fn forward(
        &mut self,
        item: Result<StreamResponse, A2AError>,
        last_generation: &mut i64,
        out: &ResponseSender,
    ) -> bool {
        let ends = item.as_ref().map_or(true, |event| self.completes(event));
        if let Some(generation) = item.as_ref().ok().and_then(generation_of) {
            *last_generation = (*last_generation).max(generation);
        }
        out.send(item).await.is_ok() && !ends
    }

    fn run(
        mut self,
        opening: Option<Task>,
        mut inner: Option<Events>,
        seen_generation: i64,
        out: ResponseSender,
    ) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(async move {
            // Watched from here, so a change from now on is seen however it is
            // timed against the looks below.
            let mut watch = self.task.watch();
            // The generation the client has been told of, so a change it was not
            // there for (an idle task, then another client's turn) is noticed.
            let mut last_generation = seen_generation;
            if let Some(task) = opening {
                if !self
                    .forward(Ok(StreamResponse::Task(task)), &mut last_generation, &out)
                    .await
                {
                    return self.finish().await;
                }
            }
            let mut uplink_open = self.uplink.is_some();
            // What happened between the opening and the watch is looked at once.
            if inner.is_none()
                && matches!(
                    self.reattach(&mut inner, &mut last_generation, &out).await,
                    Reattach::End
                )
            {
                return self.finish().await;
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
                    () = watch.changed(), if inner.is_none() ||
                        (matches!(self.completion, RunCompletion::Exchange) && !uplink_open) => {
                        if inner.is_some() && !uplink_open
                            && self.task.active().is_none()
                        {
                            // The input half is closed and the invocation ended.
                            // Preserve all buffered Task events before closing.
                            self.flush(&mut inner, &mut last_generation, &out).await;
                            break;
                        }
                        if inner.is_none() && matches!(
                            self.reattach(&mut inner, &mut last_generation, &out).await,
                            Reattach::End
                        ) {
                            break;
                        }
                    },
                    item = async {
                        match self.uplink.as_mut() {
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
                            if self.task.active().is_none() {
                                self.flush(&mut inner, &mut last_generation, &out).await;
                                break;
                            }
                        }
                    },
                }
            }
            self.finish().await;
        })
    }
    /// Ends the exchange: no more calls are routed to it, and a call that was
    /// queued but never reached is told so (it is delivered nowhere, so the
    /// caller may open a new exchange with it).
    async fn finish(mut self) {
        drop(self.registration.take());
        if let Some(mut uplink) = self.uplink.take() {
            uplink.close().await;
        }
    }

    /// Looks at the task while the exchange follows no execution. Follows a new
    /// one if it has started, tells the client of changes it was not there for
    /// (and that the task has ended). `false` when the exchange is over.
    async fn reattach(
        &mut self,
        inner: &mut Option<Events>,
        last_generation: &mut i64,
        out: &ResponseSender,
    ) -> Reattach {
        if let Some((receiver, Some(snapshot), sequence)) = self.task.resubscribe_active() {
            let event = StreamResponse::Task(snapshot.clone());
            let ends = self.completes(&event);
            let opening =
                (snapshot.generation > *last_generation || ends).then_some(snapshot.clone());
            *last_generation = (*last_generation).max(snapshot.generation);
            *inner = Some(self.policy.stream(receiver, opening, sequence));
            return Reattach::Changed;
        }
        let task = match self.handler.tasks.load(&self.task.id).await {
            Ok(task) => task,
            Err(error) => {
                self.forward(Err(error), last_generation, out).await;
                return Reattach::End;
            }
        };
        let ended = self.completes(&StreamResponse::Task(task.clone()));
        let changed = task.generation > *last_generation;
        if (ended || changed)
            && !self
                .forward(Ok(StreamResponse::Task(task)), last_generation, out)
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
        &mut self,
        item: Incoming,
        inner: &mut Option<Events>,
        last_generation: &mut i64,
        out: &ResponseSender,
    ) -> bool {
        let Incoming { request, receipt } = item;
        // Mark processing before delivery: a lost task now leaves an unknown
        // outcome instead of a receipt saying that nothing was handled.
        let receipt = receipt.map(Receipt::begin);
        let reference_id = request.reference_id();
        let result = self.deliver(request, inner, last_generation, out).await;
        let interrupted = matches!(&result, Err(Rejection::Interrupted(_)));
        let mut downlink_open = *result.as_ref().unwrap_or(&true);
        if let Err(rejection) = result {
            // Ordering is by arrival: what the exchange has already published
            // for earlier requests goes out before this one's rejection.
            downlink_open = self.flush(inner, last_generation, out).await;
            if downlink_open {
                let event = rejection.into_stream_error(&self.task.id, reference_id);
                downlink_open = self.forward(Ok(event), last_generation, out).await;
            }
        }
        if let Some(receipt) = receipt.filter(|_| !interrupted) {
            receipt.complete(self.handler.tasks.load(&self.task.id).await);
        }
        // Dropping a begun, interrupted receipt makes the HTTP adapter verify
        // persistence. A readable Task alone does not prove this input committed.
        downlink_open
    }

    /// Forwards what the execution being followed has published already,
    /// without waiting for more. `false` when the downlink is gone or the
    /// exchange ended with a terminal event.
    async fn flush(
        &mut self,
        inner: &mut Option<Events>,
        last_generation: &mut i64,
        out: &ResponseSender,
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
        &mut self,
        request: IncomingRequest,
        inner: &mut Option<Events>,
        last_generation: &mut i64,
        out: &ResponseSender,
    ) -> Result<bool, Rejection> {
        let delivery = self
            .handler
            .receive_message(
                &self.params,
                request,
                StartMode::Bidi,
                Some(&self.task.id),
                self.policy.response_id(),
            )
            .await?;
        // The source belongs to the Task, so this subscription also receives
        // a newly started turn's input and output. Keep its buffered events:
        // replacing it or replaying recorded inputs would duplicate updates.
        if inner.is_some() {
            return Ok(true);
        }
        let subscription = self.policy.delivery(delivery, DeliveryAttachment::FollowUp);
        *inner = Some(subscription.events);
        // This response was detached when input was published. Announce the
        // recorded input once; the newly attached receiver starts after it.
        for event in subscription.recorded {
            if matches!(event, StreamResponse::MessageUpdate(_))
                && !self.policy.wants_message_updates()
            {
                continue;
            }
            if !self.forward(Ok(event), last_generation, out).await {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::response_preparation::MessageResponse;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn blocking_response_waits_after_a_working_turn_ends() {
        let handler = make_handler();
        seed_task(&handler, "working", "context").await;
        let mut task = handler.tasks.load("working").await.unwrap();
        task.status.state = TaskState::Working;
        handler.tasks.store().update(task.clone()).await.unwrap();
        let drained = Arc::new(Notify::new());
        let signal = drained.clone();
        let events = Box::pin(async_stream::stream! {
            yield Ok(StreamResponse::Task(task));
            signal.notify_one();
        });
        let run = Run::new(
            handler.clone(),
            ServiceParams::new(),
            handler.tasks.task("working"),
            RunCompletion::Unary {
                return_immediately: false,
            },
            SubscriptionPolicy::new(StartMode::Blocking, false),
        );
        let events = run.start(None, Some(events), 0);
        let tasks = handler.tasks.clone();
        let mut response = tokio::spawn(async move {
            let MessageResponse::Task(task) =
                MessageResponse::receive(tasks.clone(), tasks.task("working"), events).await?
            else {
                panic!("expected a task response");
            };
            task.wait().await
        });
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
        let task = handler.tasks.load("task").await.unwrap();
        for mode in [StartMode::Blocking, StartMode::Streaming, StartMode::Bidi] {
            let completion = || match mode {
                StartMode::Blocking => RunCompletion::Unary {
                    return_immediately: false,
                },
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
                handler.tasks.task("task"),
                completion(),
                SubscriptionPolicy::new(mode, false),
            )
            .start(None, Some(events()), 0);
            let normal = tokio::time::timeout(Duration::from_secs(2), normal.collect::<Vec<_>>())
                .await
                .unwrap();
            let mut run = Run::new(
                handler.clone(),
                ServiceParams::new(),
                handler.tasks.task("task"),
                completion(),
                SubscriptionPolicy::new(mode, false),
            );
            let (out, mut received) = mpsc::channel(8);
            let mut generation = 0;
            assert!(!run.flush(&mut Some(events()), &mut generation, &out).await);
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
        let handler = make_handler();
        let mut run = Run::new(
            handler.clone(),
            ServiceParams::new(),
            handler.tasks.task("task"),
            RunCompletion::Exchange,
            SubscriptionPolicy::new(StartMode::Bidi, false),
        );
        let mut events = Some(
            futures::stream::iter([
                Ok(StreamResponse::Message(Message::new(
                    Role::Agent,
                    vec![Part::text("reply")],
                ))),
                Err(A2AError::internal("after the reply")),
            ])
            .boxed(),
        );
        let (out, mut received) = mpsc::channel(4);
        assert!(!run.flush(&mut events, &mut 0, &out).await);
        assert!(matches!(
            received.try_recv(),
            Ok(Ok(StreamResponse::Message(_)))
        ));
        assert!(received.try_recv().is_err());
    }

    #[tokio::test]
    async fn detached_responses_finish_on_missing_or_terminal_tasks() {
        for exists in [false, true] {
            let handler = make_handler();
            if exists {
                let task = task_with_history("task", "context", 0);
                handler.tasks.store().create(task).await.unwrap();
            }
            let mut events = Run::new(
                handler.clone(),
                ServiceParams::new(),
                handler.tasks.task("task"),
                RunCompletion::Task,
                SubscriptionPolicy::new(StartMode::Streaming, false),
            )
            .start(None, None, 0);
            let event = timeout(Duration::from_secs(1), events.next())
                .await
                .unwrap()
                .unwrap();
            if exists {
                assert!(
                    matches!(event.unwrap(), StreamResponse::Task(task) if task.status.state == TaskState::Completed)
                );
            } else {
                assert_eq!(event.unwrap_err().code, error_code::TASK_NOT_FOUND);
            }
            assert!(events.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn unary_direct_messages_and_errors_do_not_require_a_stored_task() {
        let handler = make_handler();
        let message = make_message();
        let events = stream::once({
            let message = message.clone();
            async move { Ok(StreamResponse::Message(message)) }
        })
        .boxed();
        let response =
            MessageResponse::receive(handler.tasks.clone(), handler.tasks.task("missing"), events)
                .await
                .unwrap();
        assert!(matches!(response, MessageResponse::Message(reply) if reply == message));
        let events = stream::once(async { Err(A2AError::internal("executor error")) }).boxed();
        let error =
            MessageResponse::receive(handler.tasks.clone(), handler.tasks.task("missing"), events)
                .await
                .err()
                .expect("executor error");
        assert_eq!(error.message, "executor error");
    }

    #[tokio::test]
    async fn detached_responses_catch_up_with_nonterminal_stored_generations() {
        let handler = make_handler();
        let mut task = task_with_history("working", "context", 0);
        task.status.state = TaskState::Working;
        task.generation = 3;
        handler.tasks.store().create(task.clone()).await.unwrap();
        let mut events = Run::new(
            handler.clone(),
            ServiceParams::new(),
            handler.tasks.task(&task.id),
            RunCompletion::Task,
            SubscriptionPolicy::new(StartMode::Streaming, false),
        )
        .start(None, None, 0);
        assert!(
            matches!(timeout(Duration::from_secs(1), events.next()).await.unwrap().unwrap().unwrap(), StreamResponse::Task(snapshot) if snapshot == task)
        );
        assert!(
            timeout(Duration::from_millis(10), events.next())
                .await
                .is_err(),
            "unchanged state must not be replayed"
        );
    }
    #[tokio::test]
    async fn unary_completion_survives_immediate_terminal_task_retention() {
        use crate::handler::test_support::*;
        let handler = DefaultRequestHandler::new(
            EchoExecutor,
            InMemoryTaskStore::new().with_terminal_task_limit(0),
        );
        let response = handler
            .send_message(&ServiceParams::new(), send_req(None, None))
            .await
            .unwrap();
        let SendMessageResponse::Task(task) = response else {
            panic!("expected the completed task");
        };
        assert_eq!(task.status.state, TaskState::Completed);
        assert!(handler.tasks.store().get(&task.id).await.unwrap().is_none());
    }
}

#[cfg(test)]
mod delivery_tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn runner_panic_is_reported_and_a_disconnected_consumer_stops_delivery() {
        let run = || {
            let handler = make_handler();
            Run::new(
                handler.clone(),
                ServiceParams::new(),
                handler.tasks.task("task"),
                RunCompletion::Task,
                SubscriptionPolicy::new(StartMode::Streaming, false),
            )
        };
        let panic_stream = stream::once(async { panic!("private runner detail") }).boxed();
        let mut events = run().start(None, Some(panic_stream), 0);
        let error = timeout(Duration::from_secs(2), events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert_eq!(error.code, error_code::INTERNAL_ERROR);
        assert_eq!(error.message, "the response runner failed");
        assert!(events.next().await.is_none());

        let (stopped, wait) = oneshot::channel::<()>();
        let pending = stream::once(async move {
            let _stopped = stopped;
            futures::future::pending::<Result<StreamResponse, A2AError>>().await
        })
        .boxed();
        let events = run().start(None, Some(pending), 0);
        drop(events);
        assert!(
            timeout(Duration::from_secs(2), wait)
                .await
                .unwrap()
                .is_err()
        );
    }
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn unary_response_does_not_return_missing_or_nonterminal_cached_tasks() {
        for cached in [false, true] {
            let handler = make_handler();
            let source = handler.execution_manager.events_for("missing");
            if cached {
                let mut task = task_with_history("missing", "context", 0);
                task.status.state = TaskState::Working;
                source.set_snapshot(task).await;
            }
            let error = Run::unary_response(
                handler,
                "missing".into(),
                stream::empty().boxed(),
                None,
                None,
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, error_code::TASK_NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn detached_response_stops_when_its_consumer_is_gone() {
        let handler = make_handler();
        let mut task = task_with_history("task", "context", 0);
        task.generation = 2;
        handler.task_store.create(task).await.unwrap();
        let run = Run::new(
            handler,
            ServiceParams::new(),
            "task".into(),
            RunCompletion::Task,
        );
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let mut inner = None;
        let mut generation = 0;
        assert!(matches!(
            run.reattach(&mut inner, &mut generation, &DeliverySink::Stream(sender))
                .await,
            Reattach::End
        ));
        assert_eq!(generation, 2);
        assert!(inner.is_none());
    }

    #[tokio::test]
    async fn flushing_an_exhausted_subscription_detaches_it_without_closing_downlink() {
        let run = Run::new(
            make_handler(),
            ServiceParams::new(),
            "task".into(),
            RunCompletion::Task,
        );
        let mut inner = Some(EventSource::Subscription(stream::empty().boxed()));
        let (sender, mut receiver) = mpsc::channel(1);
        let mut generation = 7;
        let sink = DeliverySink::Stream(sender);
        assert!(run.flush(&mut inner, &mut generation, &sink).await);
        assert!(inner.is_none());
        assert_eq!(generation, 7);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
}
