// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Owned executor output with producer-side input correlation.

use super::ClientInput;
use a2a::{A2AError, StreamResponse, error_code};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use tokio::sync::mpsc;

/// One locally produced event. Correlation is SDK context, never a wire field.
#[derive(Debug)]
#[non_exhaustive]
pub struct ExecutorOutput {
    pub result: Result<StreamResponse, A2AError>,
    pub input_generation: i64,
}

/// The sending end of the stream returned by an executor.
/// Use `send_for` to associate an interruption with the input it answers.
#[derive(Clone)]
pub struct ExecutorOutputSender {
    sender: mpsc::Sender<ExecutorOutput>,
    opening_generation: i64,
}

/// An executor's bounded output stream, including producer-side correlation.
/// Return this receiver directly from `execute`, boxed as its output stream.
/// Dropping it closes the channel; dropping all senders ends the stream after
/// buffered events have been read. Attach an owned worker with `with_worker`
/// to report its errors and abort it when the stream is dropped.
pub struct ExecutorOutputReceiver {
    receiver: mpsc::Receiver<ExecutorOutput>,
    opening_generation: i64,
    worker: Option<tokio::task::JoinHandle<Result<(), A2AError>>>,
    failure: Option<A2AError>,
    ended: bool,
}

impl ExecutorOutputSender {
    /// Connect a standalone executor to a bounded output stream. The opening
    /// generation is zero. In an executor, use `ExecutorContext::output_channel`
    /// to bind untagged output to the request that started execution.
    pub fn channel(capacity: usize) -> (Self, ExecutorOutputReceiver) {
        Self::for_execution(capacity, 0)
    }

    pub(crate) fn for_execution(
        capacity: usize,
        opening_generation: i64,
    ) -> (Self, ExecutorOutputReceiver) {
        let (sender, receiver) = mpsc::channel(capacity);
        (
            Self {
                sender,
                opening_generation,
            },
            ExecutorOutputReceiver {
                receiver,
                opening_generation,
                worker: None,
                failure: None,
                ended: false,
            },
        )
    }

    /// Emit output for the opening input, including a task or standalone reply.
    pub async fn send(&self, event: StreamResponse) -> Result<(), A2AError> {
        self.send_result(Ok(event), self.opening_generation).await
    }

    /// Emit output for this exact consumed input, even if it was buffered while
    /// another worker consumed a newer input. The receiver preserves this tag.
    pub async fn send_for(
        &self,
        input: &ClientInput,
        event: StreamResponse,
    ) -> Result<(), A2AError> {
        self.send_result(Ok(event), input.generation).await
    }

    pub async fn fail(&self, error: A2AError) -> Result<(), A2AError> {
        self.send_result(Err(error), self.opening_generation).await
    }

    async fn send_result(
        &self,
        result: Result<StreamResponse, A2AError>,
        input_generation: i64,
    ) -> Result<(), A2AError> {
        self.sender
            .send(ExecutorOutput {
                result,
                input_generation,
            })
            .await
            .map_err(|_| A2AError::internal("executor output is closed"))
    }
}

impl ExecutorOutputReceiver {
    /// Monitor the worker that owns this output stream. A panic, cancellation,
    /// or returned error becomes a stream error after buffered output. Dropping
    /// the stream aborts the worker, so shutdown cannot leave it detached.
    pub fn with_worker(mut self, worker: tokio::task::JoinHandle<Result<(), A2AError>>) -> Self {
        if let Some(previous) = self.worker.replace(worker) {
            previous.abort();
        }
        self
    }

    fn poll_worker(&mut self, cx: &mut std::task::Context<'_>) {
        use std::future::Future;
        let Some(worker) = self.worker.as_mut() else {
            return;
        };
        let std::task::Poll::Ready(result) = std::pin::Pin::new(worker).poll(cx) else {
            return;
        };
        self.worker = None;
        let failure = match result {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(error) => {
                tracing::error!(%error, "agent output worker failed");
                Some(A2AError::internal(if error.is_panic() {
                    "agent output worker panicked"
                } else {
                    "agent output worker was canceled"
                }))
            }
        };
        if let Some(error) = failure {
            self.receiver.close();
            self.failure = Some(error);
        }
    }

    /// Read a tagged event in a standalone runner. Returning this receiver as
    /// the executor stream also carries its tag through the default handler.
    pub async fn recv(&mut self) -> Option<ExecutorOutput> {
        futures::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<ExecutorOutput>> {
        use std::task::Poll;
        if self.ended {
            return Poll::Ready(None);
        }
        self.poll_worker(cx);
        let _ = POLLING_EXECUTOR.try_with(|scope| scope.produced.set(None));
        let output = match self.receiver.poll_recv(cx) {
            Poll::Ready(Some(output)) => Some(output),
            Poll::Ready(None) | Poll::Pending if self.failure.is_some() => {
                self.ended = true;
                self.failure.take().map(|error| ExecutorOutput {
                    result: Err(error),
                    input_generation: self.opening_generation,
                })
            }
            Poll::Ready(None) if self.worker.is_none() => {
                self.ended = true;
                None
            }
            Poll::Ready(None) | Poll::Pending => return Poll::Pending,
        };
        if let Some(output) = &output {
            let _ = POLLING_EXECUTOR.try_with(|scope| {
                scope.reads.set(scope.reads.get() + 1);
                scope.produced.set(Some(output.input_generation));
            });
        }
        Poll::Ready(output)
    }
}

impl Drop for ExecutorOutputReceiver {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

impl futures::Stream for ExecutorOutputReceiver {
    type Item = Result<StreamResponse, A2AError>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.get_mut()
            .poll_recv(cx)
            .map(|output| output.map(|output| output.result))
    }
}

struct PollScope {
    inputs: Arc<AtomicI64>,
    inline: Arc<AtomicI64>,
    produced: std::cell::Cell<Option<i64>>,
    reads: std::cell::Cell<usize>,
    warned: std::cell::Cell<bool>,
}

tokio::task_local! { static POLLING_EXECUTOR: PollScope; }

/// Spawned workers do not inherit this scope. Reading an input elsewhere cannot
/// incorrectly stamp an older event pulled from an executor-owned buffer.
pub(crate) async fn scope<F: std::future::Future>(
    inputs: Arc<AtomicI64>,
    inline: Arc<AtomicI64>,
    future: F,
) -> F::Output {
    POLLING_EXECUTOR
        .scope(
            PollScope {
                inputs,
                inline,
                produced: std::cell::Cell::new(None),
                reads: std::cell::Cell::new(0),
                warned: std::cell::Cell::new(false),
            },
            future,
        )
        .await
}

/// Capture correlation in the same poll that yields the item. A tagged
/// channel item overrides inline read progress only for this output; it cannot
/// contaminate a later legacy item or an unrelated pending stream poll.
pub(crate) async fn next(
    stream: &mut futures::stream::BoxStream<'static, Result<StreamResponse, A2AError>>,
    inline: &AtomicI64,
) -> Option<ExecutorOutput> {
    futures::future::poll_fn(|cx| {
        let _ = POLLING_EXECUTOR.try_with(|scope| { scope.produced.set(None); scope.reads.set(0); });
        let polled = stream.as_mut().poll_next(cx);
        let (tag, unusable) = POLLING_EXECUTOR.try_with(|scope| {
            let tag = scope.produced.take();
            let reads = scope.reads.get();
            let ambiguous = reads > 1 || (reads > 0 && (polled.is_pending() || tag.is_none()));
            if ambiguous && !scope.warned.replace(true) {
                tracing::warn!(reads, "executor output adapter discarded input correlation; return the supplied receiver directly and transform before send_for");
            }
            (tag, reads > 1 || (reads > 0 && polled.is_pending()))
        }).unwrap_or((None, false));
        let generation = tag.unwrap_or_else(|| inline.load(Ordering::Acquire));
        if unusable {
            return std::task::Poll::Ready(Some(ExecutorOutput {
                result: Err(A2AError::new(error_code::INVALID_AGENT_RESPONSE,
                    "executor output lost input correlation through a buffering adapter; return the supplied receiver directly")),
                input_generation: generation,
            }));
        }
        let Some(result) = std::task::ready!(polled) else {
            return std::task::Poll::Ready(None);
        };
        std::task::Poll::Ready(Some(ExecutorOutput {
            result,
            input_generation: generation,
        }))
    })
    .await
}

pub(crate) fn input_read(inputs: &Arc<AtomicI64>, generation: i64) {
    let _ = POLLING_EXECUTOR.try_with(|scope| {
        if Arc::ptr_eq(inputs, &scope.inputs) {
            scope.inline.fetch_max(generation, Ordering::AcqRel);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::{Message, Part, Role};

    #[tokio::test]
    async fn producer_tags_survive_buffering_and_channel_closure() {
        let (outputs, mut receiver) = ExecutorOutputSender::channel(2);
        let opening =
            StreamResponse::Message(Message::new(Role::Agent, vec![Part::text("opening")]));
        outputs.send(opening.clone()).await.unwrap();
        let event = receiver.recv().await.unwrap();
        assert_eq!(event.input_generation, 0);
        assert_eq!(event.result.unwrap(), opening);
        let input = ClientInput::new(Message::new(Role::User, vec![]), 17);
        outputs
            .send_for(
                &input,
                StreamResponse::Message(Message::new(Role::Agent, vec![Part::text("answer")])),
            )
            .await
            .unwrap();
        outputs.fail(A2AError::internal("failed")).await.unwrap();
        assert_eq!(receiver.recv().await.unwrap().input_generation, 17);
        assert!(receiver.recv().await.unwrap().result.is_err());
        drop(receiver);
        assert!(outputs.fail(A2AError::internal("closed")).await.is_err());
    }

    #[tokio::test]
    async fn tagged_channel_output_does_not_overwrite_later_inline_correlation() {
        use futures::StreamExt;
        let inputs = Arc::new(AtomicI64::new(7));
        let inline = Arc::new(AtomicI64::new(7));
        let (outputs, receiver) = ExecutorOutputSender::channel(2);
        let input = ClientInput::new(Message::new(Role::User, vec![]), 3);
        outputs
            .send_for(
                &input,
                StreamResponse::Message(Message::new(Role::Agent, vec![])),
            )
            .await
            .unwrap();
        outputs
            .send_for(
                &ClientInput::new(Message::new(Role::User, vec![]), 4),
                StreamResponse::Message(Message::new(Role::Agent, vec![Part::text("discard")])),
            )
            .await
            .unwrap();
        drop(outputs);
        let receiver = receiver.filter(|event| futures::future::ready(
            !matches!(event, Ok(StreamResponse::Message(message)) if message.text() == Some("discard"))));
        let legacy = futures::stream::once(async {
            Ok(StreamResponse::Message(Message::new(Role::Agent, vec![])))
        });
        let mut stream = receiver.chain(legacy).boxed();
        scope(inputs, inline.clone(), async {
            assert_eq!(
                next(&mut stream, &inline).await.unwrap().input_generation,
                3
            );
            assert_eq!(inline.load(Ordering::Acquire), 7);
            assert_eq!(
                next(&mut stream, &inline).await.unwrap().input_generation,
                7
            );
            assert!(next(&mut stream, &inline).await.is_none());
        })
        .await;
    }

    #[tokio::test]
    async fn worker_errors_follow_buffered_output_even_with_another_sender_alive() {
        use futures::StreamExt;
        let (outputs, receiver) = ExecutorOutputSender::channel(2);
        let outside_sender = outputs.clone();
        let worker = tokio::spawn(async move {
            outputs
                .send(StreamResponse::Message(Message::new(Role::Agent, vec![])))
                .await?;
            Err(A2AError::internal("worker returned an error"))
        });
        let mut events = receiver.with_worker(worker);
        assert!(events.next().await.unwrap().is_ok());
        assert_eq!(
            events.next().await.unwrap().unwrap_err().message,
            "worker returned an error"
        );
        assert!(events.next().await.is_none());
        assert!(
            outside_sender
                .fail(A2AError::internal("late"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn dropping_a_monitored_stream_aborts_its_worker() {
        let (outputs, receiver) = ExecutorOutputSender::channel(1);
        let started = Arc::new(tokio::sync::Notify::new());
        let stopped = Arc::new(tokio::sync::Notify::new());
        struct Stopped(Arc<tokio::sync::Notify>);
        impl Drop for Stopped {
            fn drop(&mut self) {
                self.0.notify_one();
            }
        }
        let ready = started.clone();
        let done = stopped.clone();
        let worker = tokio::spawn(async move {
            let _stopped = Stopped(done);
            let _outputs = outputs;
            ready.notify_one();
            std::future::pending::<Result<(), A2AError>>().await
        });
        let stream = receiver.with_worker(worker);
        started.notified().await;
        // Replacing the monitored worker must abort the former owner too.
        let replacement = tokio::spawn(std::future::pending::<Result<(), A2AError>>());
        let replacement_abort = replacement.abort_handle();
        let stream = stream.with_worker(replacement);
        tokio::time::timeout(std::time::Duration::from_secs(1), stopped.notified())
            .await
            .unwrap();
        drop(stream);
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !replacement_abort.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn an_aborted_worker_is_an_error_instead_of_silent_eof() {
        use futures::StreamExt;
        let (outputs, receiver) = ExecutorOutputSender::channel(1);
        let worker = tokio::spawn(async move {
            let _outputs = outputs;
            std::future::pending::<Result<(), A2AError>>().await
        });
        worker.abort();
        let mut events = receiver.with_worker(worker);
        assert!(
            events
                .next()
                .await
                .unwrap()
                .unwrap_err()
                .message
                .contains("canceled")
        );
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn asynchronous_or_prefetching_adapters_cannot_silently_relabel_tagged_output() {
        use futures::StreamExt;
        for adapter in 0..3 {
            let (outputs, mut receiver) = ExecutorOutputSender::channel(2);
            for generation in [7, 9] {
                outputs
                    .send_for(
                        &ClientInput::new(Message::new(Role::User, vec![]), generation),
                        StreamResponse::Message(Message::new(Role::Agent, vec![])),
                    )
                    .await
                    .unwrap();
            }
            drop(outputs);
            let mut stream: futures::stream::BoxStream<'static, Result<StreamResponse, A2AError>> =
                match adapter {
                    0 => receiver
                        .then(|event| async move {
                            std::future::pending::<()>().await;
                            event
                        })
                        .boxed(),
                    1 => receiver.map(futures::future::ready).buffered(2).boxed(),
                    _ => Box::pin(async_stream::stream! {
                        let event = receiver.next().await.unwrap();
                        std::future::pending::<()>().await;
                        yield event;
                    }),
                };
            let inputs = Arc::new(AtomicI64::new(9));
            let inline = Arc::new(AtomicI64::new(1));
            scope(inputs, inline.clone(), async {
                let output = tokio::time::timeout(
                    std::time::Duration::from_secs(1),
                    next(&mut stream, &inline),
                )
                .await
                .unwrap()
                .unwrap();
                assert_eq!(
                    output.result.unwrap_err().code,
                    error_code::INVALID_AGENT_RESPONSE
                );
                assert!(POLLING_EXECUTOR.with(|scope| scope.warned.get()));
            })
            .await;
        }
    }

    #[tokio::test]
    async fn spawned_read_cannot_advance_legacy_output_correlation() {
        let inputs = Arc::new(AtomicI64::new(0));
        let inline = Arc::new(AtomicI64::new(0));
        scope(inputs.clone(), inline.clone(), async {
            input_read(&inputs, 2);
            let worker = inputs.clone();
            tokio::spawn(async move {
                input_read(&worker, 9);
            })
            .await
            .unwrap();
            assert_eq!(inline.load(Ordering::Acquire), 2);
            input_read(&Arc::new(AtomicI64::new(0)), 7);
            assert_eq!(inline.load(Ordering::Acquire), 2);
        })
        .await;
    }
}
