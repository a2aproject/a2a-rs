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

/// An owned sender for workers that receive input outside the executor stream.
/// Use `send_for` to associate an interruption with the input it answers.
#[derive(Clone, Default)]
pub struct ExecutorOutputSender {
    sender: Option<mpsc::Sender<ExecutorOutput>>,
    opening_generation: i64,
    workers: Option<mpsc::WeakUnboundedSender<()>>,
    worker_lease: Option<mpsc::UnboundedSender<()>>,
}

impl ExecutorOutputSender {
    /// Connect a standalone executor to a bounded output receiver in a test or
    /// custom runner. The default generation is zero; `send_for` uses the input.
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<ExecutorOutput>) {
        let (mut sender, receiver, _workers) = Self::for_execution(capacity, 0);
        // A standalone runner owns channel lifetime itself.
        sender.workers = None;
        (sender, receiver)
    }

    pub(crate) fn for_execution(
        capacity: usize,
        opening_generation: i64,
    ) -> (Self, mpsc::Receiver<ExecutorOutput>, OutputWorkers) {
        let (sender, receiver) = mpsc::channel(capacity);
        let (opening, done) = mpsc::unbounded_channel();
        (
            Self {
                sender: Some(sender),
                opening_generation,
                workers: Some(opening.downgrade()),
                worker_lease: None,
            },
            receiver,
            OutputWorkers {
                opening: Some(opening),
                done,
            },
        )
    }

    /// Explicitly keep this invocation alive after its inline stream ends.
    /// Call before returning/detaching the stream, then move this sender into
    /// the worker. Merely capturing ExecutorContext does not extend a turn.
    /// Dropping all opted-in sender clones lets the turn finish.
    pub fn into_worker(mut self) -> Result<Self, A2AError> {
        if self.worker_lease.is_none()
            && let Some(workers) = &self.workers
        {
            self.worker_lease = Some(
                workers
                    .upgrade()
                    .ok_or_else(|| A2AError::internal("the inline execution has already ended"))?,
            );
        }
        Ok(self)
    }

    /// Emit output for the opening input, including a task or standalone reply.
    pub async fn send(&self, event: StreamResponse) -> Result<(), A2AError> {
        self.send_result(Ok(event), self.opening_generation).await
    }

    /// Emit output for this exact consumed input, even if it was buffered while
    /// another worker consumed a newer input. The server never infers this tag.
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
        let sender = self
            .sender
            .as_ref()
            .ok_or_else(|| A2AError::internal("executor output is not connected"))?;
        sender
            .send(ExecutorOutput {
                result,
                input_generation,
            })
            .await
            .map_err(|_| A2AError::internal("executor output is closed"))
    }
}

/// No messages are sent here. Only opted-in workers retain strong senders;
/// passive Context/sender clones hold weak references and cannot stall EOF.
pub(crate) struct OutputWorkers {
    opening: Option<mpsc::UnboundedSender<()>>,
    done: mpsc::UnboundedReceiver<()>,
}

impl OutputWorkers {
    pub(crate) fn inline_ended(&mut self) -> bool {
        self.opening.take();
        self.done.is_closed()
    }

    pub(crate) async fn ended(&mut self) {
        let _ = self.done.recv().await;
    }
}

struct PollScope {
    inputs: Arc<AtomicI64>,
    inline: Arc<AtomicI64>,
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
        .scope(PollScope { inputs, inline }, future)
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
        assert!(
            ExecutorOutputSender::default()
                .fail(A2AError::internal("disconnected"))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn only_explicit_workers_extend_inline_output_lifetime() {
        let (sender, _receiver, mut workers) = ExecutorOutputSender::for_execution(1, 1);
        let passive = sender.clone();
        assert!(workers.inline_ended());
        assert!(passive.into_worker().is_err());
        let (sender, _receiver, mut workers) = ExecutorOutputSender::for_execution(1, 1);
        let active = sender.clone().into_worker().unwrap();
        assert!(!workers.inline_ended());
        drop(active);
        tokio::time::timeout(std::time::Duration::from_secs(1), workers.ended())
            .await
            .unwrap();
        // The passive sender still exists and does not retain the turn.
        drop(sender);
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
