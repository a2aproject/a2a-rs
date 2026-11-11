// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Owned executor output with producer-side input correlation.

use super::ClientInput;
use a2a::{A2AError, StreamResponse};
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
/// buffered events have been read. No separate worker registration is needed.
pub struct ExecutorOutputReceiver {
    receiver: mpsc::Receiver<ExecutorOutput>,
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
            ExecutorOutputReceiver { receiver },
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
    /// Read a tagged event in a standalone runner. Returning this receiver as
    /// the executor stream also carries its tag through the default handler.
    pub async fn recv(&mut self) -> Option<ExecutorOutput> {
        futures::future::poll_fn(|cx| self.poll_recv(cx)).await
    }

    fn poll_recv(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<ExecutorOutput>> {
        let _ = POLLING_EXECUTOR.try_with(|scope| scope.produced.set(None));
        let output = std::task::ready!(self.receiver.poll_recv(cx));
        if let Some(output) = &output {
            let _ = POLLING_EXECUTOR
                .try_with(|scope| scope.produced.set(Some(output.input_generation)));
        }
        std::task::Poll::Ready(output)
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
        let _ = POLLING_EXECUTOR.try_with(|scope| scope.produced.set(None));
        let Some(result) = std::task::ready!(stream.as_mut().poll_next(cx)) else {
            return std::task::Poll::Ready(None);
        };
        let generation = POLLING_EXECUTOR
            .try_with(|scope| scope.produced.take())
            .ok()
            .flatten()
            .unwrap_or_else(|| inline.load(Ordering::Acquire));
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
