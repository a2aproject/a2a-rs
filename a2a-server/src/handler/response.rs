// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Follow task output with a shared runner and shared response settings.

use super::http_exchange::RegistrationLease;
#[cfg(test)]
use super::protocol::StartMode;
use super::protocol::{
    DeliveryAttachment, IncomingRequest, Rejection, ResponsePolicy, StreamDecision, generation_of,
};
use super::response_preparation::{attach_delivery, event_stream};
use super::task_handle::TaskHandle;
use super::uplink::{Incoming, Receipt, Uplink};
use super::{DefaultRequestHandler, RESPONSE_BUFFER_CAPACITY};
use crate::middleware::ServiceParams;
use a2a::*;
use futures::{FutureExt, StreamExt, stream::BoxStream};
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
    pub(super) task: TaskHandle,
    settings: ResponsePolicy,
}

pub(super) type Events = BoxStream<'static, Result<StreamResponse, A2AError>>;
type ResponseSender = mpsc::Sender<Result<StreamResponse, A2AError>>;

impl Run {
    pub(super) fn new(
        handler: DefaultRequestHandler,
        params: ServiceParams,
        task: TaskHandle,
        settings: ResponsePolicy,
    ) -> Self {
        Self {
            registration: None,
            uplink: None,
            handler,
            params,
            task,
            settings,
        }
    }

    pub(super) fn with_uplink(
        mut self,
        uplink: Option<Uplink>,
        mut registration: Option<RegistrationLease>,
    ) -> Self {
        if let Some(registration) = &mut registration {
            registration.bind(self.task.id());
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
        let background_tasks = self.handler.tasks.background_tasks.clone();
        background_tasks.spawn(async move {
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
        self.settings.completes(event)
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
        let keep_open = self.settings.after_event(&item) == StreamDecision::KeepOpen;
        if let Some(generation) = item.as_ref().ok().and_then(generation_of) {
            *last_generation = (*last_generation).max(generation);
        }
        out.send(item).await.is_ok() && keep_open
    }

    fn run(
        mut self,
        opening: Option<Task>,
        mut inner: Option<Events>,
        seen_generation: i64,
        out: ResponseSender,
    ) -> futures::future::BoxFuture<'static, ()> {
        Box::pin(async move {
            // Register before the first state read so intervening changes are observed.
            let mut watch = self.task.watch();
            let mut last_generation = seen_generation;
            let mut uplink_open = self.uplink.is_some();
            if !self
                .initialize_response(opening, &mut inner, &mut last_generation, &out)
                .await
            {
                return self.finish().await;
            }

            // Keep all concurrent waits here; handlers run only after a branch wins.
            loop {
                tokio::select! {
                    () = out.closed() => break,
                    event = async {
                        match inner.as_mut() {
                            Some(events) => events.next().await,
                            None => std::future::pending().await,
                        }
                    } => {
                        if !self.handle_output(event, &mut inner, &mut last_generation, &out, uplink_open).await {
                            break;
                        }
                    },
                    () = watch.changed(), if inner.is_none() ||
                        (self.settings.is_live() && !uplink_open) => {
                        if !self.handle_task_change(&mut inner, &mut last_generation, &out, uplink_open).await {
                            break;
                        }
                    },
                    item = async {
                        match self.uplink.as_mut() {
                            Some(uplink) => uplink.next().await,
                            None => std::future::pending().await,
                        }
                    }, if uplink_open => {
                        if !self.handle_uplink(item, &mut uplink_open, &mut inner, &mut last_generation, &out).await {
                            break;
                        }
                    },
                }
            }
            self.finish().await;
        })
    }

    async fn initialize_response(
        &mut self,
        opening: Option<Task>,
        inner: &mut Option<Events>,
        last_generation: &mut i64,
        out: &ResponseSender,
    ) -> bool {
        if let Some(task) = opening
            && !self
                .forward(Ok(StreamResponse::Task(task)), last_generation, out)
                .await
        {
            return false;
        }
        inner.is_some()
            || !matches!(
                self.reattach(inner, last_generation, out).await,
                Reattach::End
            )
    }

    async fn handle_output(
        &mut self,
        event: Option<Result<StreamResponse, A2AError>>,
        inner: &mut Option<Events>,
        last_generation: &mut i64,
        out: &ResponseSender,
        uplink_open: bool,
    ) -> bool {
        if let Some(event) = event {
            return self.forward(event, last_generation, out).await;
        }
        // An invocation ending need not end the task or its client response.
        *inner = None;
        if self.settings.is_live() && !uplink_open {
            return false;
        }
        !matches!(
            self.reattach(inner, last_generation, out).await,
            Reattach::End
        )
    }

    async fn handle_task_change(
        &mut self,
        inner: &mut Option<Events>,
        last_generation: &mut i64,
        out: &ResponseSender,
        uplink_open: bool,
    ) -> bool {
        if inner.is_some() && !uplink_open && self.task.active().is_none() {
            // A closed uplink finishes after the invocation and buffered output.
            self.flush(inner, last_generation, out).await;
            return false;
        }
        inner.is_some()
            || !matches!(
                self.reattach(inner, last_generation, out).await,
                Reattach::End
            )
    }

    async fn handle_uplink(
        &mut self,
        item: Option<Incoming>,
        uplink_open: &mut bool,
        inner: &mut Option<Events>,
        last_generation: &mut i64,
        out: &ResponseSender,
    ) -> bool {
        if let Some(item) = item {
            return self.handle(item, inner, last_generation, out).await;
        }
        *uplink_open = false;
        if inner.is_none() {
            return false;
        }
        if self.task.active().is_none() {
            self.flush(inner, last_generation, out).await;
            return false;
        }
        true
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
            *inner = Some(event_stream(self.settings, receiver, opening, sequence));
            return Reattach::Changed;
        }
        let task = match self.handler.tasks.load(self.task.id()).await {
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
                let event = rejection.into_stream_error(self.task.id(), reference_id);
                downlink_open = self.forward(Ok(event), last_generation, out).await;
            }
        }
        if let Some(receipt) = receipt.filter(|_| !interrupted) {
            receipt.complete(self.handler.tasks.load(self.task.id()).await);
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
                request.into_message()?,
                self.settings.mode(),
                Some(self.task.id()),
                self.settings.response_id(),
            )
            .await?;
        // The source belongs to the Task, so this subscription also receives
        // a newly started turn's input and output. Keep its buffered events:
        // replacing it or replaying recorded inputs would duplicate updates.
        if inner.is_some() {
            return Ok(true);
        }
        let subscription = attach_delivery(self.settings, delivery, DeliveryAttachment::FollowUp);
        *inner = Some(subscription.events);
        // This response was detached when input was published. Announce the
        // recorded input once; the newly attached receiver starts after it.
        for event in subscription.recorded {
            if matches!(event, StreamResponse::MessageUpdate(_))
                && !self.settings.wants_message_updates()
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
            ResponsePolicy::new(StartMode::Blocking, false),
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
                ResponsePolicy::new(mode, false),
            )
            .start(None, Some(events()), 0);
            let normal = tokio::time::timeout(Duration::from_secs(2), normal.collect::<Vec<_>>())
                .await
                .unwrap();
            let mut run = Run::new(
                handler.clone(),
                ServiceParams::new(),
                handler.tasks.task("task"),
                ResponsePolicy::new(mode, false),
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
            ResponsePolicy::new(StartMode::Bidi, false),
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
                ResponsePolicy::new(StartMode::Streaming, false),
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
            ResponsePolicy::new(StartMode::Streaming, false),
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
    async fn detached_followups_announce_persisted_inputs_once_and_respect_disconnects() {
        for wants_messages in [false, true] {
            for disconnected in [false, true] {
                let handler = make_handler().with_capabilities(AgentCapabilities {
                    streaming: Some(true),
                    user_artifacts: Some(true),
                    ..Default::default()
                });
                seed_task(&handler, "task", "context").await;
                let mut run = Run::new(
                    handler.clone(),
                    ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])]),
                    handler.tasks.task("task"),
                    ResponsePolicy::new(StartMode::Bidi, wants_messages),
                );
                let mut request = send_request();
                let message_id = request.message.message_id.clone();
                request.message.task_id = Some("task".into());
                request.artifact_updates.push(TaskArtifactUpdateEvent {
                    task_id: "task".into(),
                    context_id: "context".into(),
                    artifact: Artifact {
                        artifact_id: "upload".into(),
                        parts: vec![Part::text("client input")],
                        role: Some(Role::User),
                        name: None,
                        description: None,
                        metadata: None,
                        extensions: None,
                        start_generation: None,
                        end_generation: None,
                    },
                    generation: 0,
                    append: None,
                    last_chunk: None,
                    metadata: None,
                });
                let (out, received) = mpsc::channel(8);
                let mut received = (!disconnected).then_some(received);
                let mut inner = None;
                let mut generation = 0;
                let keep_open = run
                    .deliver(
                        IncomingRequest::message(request),
                        &mut inner,
                        &mut generation,
                        &out,
                    )
                    .await
                    .unwrap_or_else(|error| {
                        panic!(
                            "a valid followup must be admitted: {:?}",
                            error.into_error()
                        )
                    });
                assert_eq!(keep_open, !disconnected);
                assert!(inner.is_some(), "the followup must attach task output");
                if let Some(received) = &mut received {
                    let mut updates = Vec::new();
                    while let Ok(event) = received.try_recv() {
                        updates.push(event.unwrap());
                    }
                    assert_eq!(updates.len(), if wants_messages { 2 } else { 1 });
                    assert!(
                        matches!(updates.last(), Some(StreamResponse::ArtifactUpdate(update)) if update.artifact.artifact_id == "upload")
                    );
                    if wants_messages {
                        assert!(
                            matches!(&updates[0], StreamResponse::MessageUpdate(update) if update.message.message_id == message_id)
                        );
                    }
                    assert_eq!(generation, 2);
                }
                let task = handler.tasks.load("task").await.unwrap();
                assert_eq!(
                    task.history
                        .unwrap()
                        .iter()
                        .filter(|m| m.message_id == message_id)
                        .count(),
                    1
                );
                assert_eq!(task.artifacts.unwrap()[0].artifact_id, "upload");
                handler.shutdown().await;
            }
        }
    }

    #[tokio::test]
    async fn runner_panic_is_reported_and_a_disconnected_consumer_stops_delivery() {
        let run = || {
            let handler = make_handler();
            Run::new(
                handler.clone(),
                ServiceParams::new(),
                handler.tasks.task("task"),
                ResponsePolicy::new(StartMode::Streaming, false),
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
    use crate::handler::response_preparation::MessageResponse;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn unary_response_does_not_return_missing_or_nonterminal_cached_tasks() {
        for cached in [false, true] {
            let handler = make_handler();
            let source = handler.tasks.task("missing");
            if cached {
                let mut task = task_with_history("missing", "context", 0);
                task.status.state = TaskState::Working;
                source.set_snapshot(task);
            }
            let response =
                MessageResponse::receive(handler.tasks.clone(), source, stream::empty().boxed())
                    .await
                    .unwrap();
            let MessageResponse::Task(response) = response else {
                panic!("an empty stream is not a direct reply")
            };
            let error = response.wait().await.unwrap_err();
            assert_eq!(error.code, error_code::TASK_NOT_FOUND);
            handler.shutdown().await;
        }
    }

    #[tokio::test]
    async fn detached_response_stops_when_its_consumer_is_gone() {
        let handler = make_handler();
        let mut task = task_with_history("task", "context", 0);
        task.generation = 2;
        handler.tasks.store().create(task).await.unwrap();
        let mut run = Run::new(
            handler.clone(),
            ServiceParams::new(),
            handler.tasks.task("task"),
            ResponsePolicy::new(StartMode::Streaming, false),
        );
        let (sender, receiver) = mpsc::channel(1);
        drop(receiver);
        let mut inner = None;
        let mut generation = 0;
        assert!(matches!(
            run.reattach(&mut inner, &mut generation, &sender).await,
            Reattach::End
        ));
        assert_eq!(generation, 2);
        assert!(inner.is_none());
        handler.shutdown().await;
    }

    #[tokio::test]
    async fn flushing_an_exhausted_subscription_detaches_it_without_closing_downlink() {
        let handler = make_handler();
        let mut run = Run::new(
            handler.clone(),
            ServiceParams::new(),
            handler.tasks.task("task"),
            ResponsePolicy::new(StartMode::Streaming, false),
        );
        let mut inner = Some(stream::empty().boxed());
        let (sender, mut receiver) = mpsc::channel(1);
        let mut generation = 7;
        assert!(run.flush(&mut inner, &mut generation, &sender).await);
        assert!(inner.is_none());
        assert_eq!(generation, 7);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        handler.shutdown().await;
    }
}
