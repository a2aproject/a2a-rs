// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Cooperative shutdown and completion tracking for SDK-owned workers.

use a2a::A2AError;
use std::future::Future;
use tokio::task::JoinHandle;
use tokio_util::{sync::CancellationToken, task::TaskTracker};

#[derive(Default)]
pub(super) struct BackgroundTasks {
    stopping: CancellationToken,
    tasks: TaskTracker,
}

impl BackgroundTasks {
    pub(super) fn check_running(&self) -> Result<(), A2AError> {
        if self.stopping.is_cancelled() {
            Err(A2AError::internal("handler is shutting down"))
        } else {
            Ok(())
        }
    }

    pub(super) async fn cancelled(&self) {
        self.stopping.cancelled().await;
    }

    pub(super) fn spawn<F>(&self, future: F) -> Option<JoinHandle<Option<F::Output>>>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        // Register before checking shutdown. A racing spawn is either counted
        // by shutdown's wait or rejected without spawning any work.
        let tracked = self.tasks.token();
        if self.stopping.is_cancelled() {
            drop(future);
            return None;
        }
        let stopping = self.stopping.clone();
        Some(tokio::spawn(async move {
            let _tracked = tracked;
            tokio::select! {
                biased;
                _ = stopping.cancelled() => None,
                result = future => Some(result),
            }
        }))
    }

    pub(super) async fn shutdown(&self) {
        self.stopping.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct Lease(Arc<AtomicUsize>);
    impl Lease {
        fn new(count: &Arc<AtomicUsize>) -> Self {
            count.fetch_add(1, Ordering::SeqCst);
            Self(count.clone())
        }
    }
    impl Drop for Lease {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn shutdown_racing_spawns_releases_every_future_and_rejects_later_work() {
        let background_tasks = Arc::new(BackgroundTasks::default());
        let live = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(65));
        let mut callers = tokio::task::JoinSet::new();
        for _ in 0..64 {
            let (background_tasks, live, barrier) =
                (background_tasks.clone(), live.clone(), barrier.clone());
            callers.spawn(async move {
                let lease = Lease::new(&live);
                barrier.wait().await;
                background_tasks.spawn(async move {
                    let _lease = lease;
                    std::future::pending::<()>().await;
                })
            });
        }
        barrier.wait().await;
        background_tasks.shutdown().await;
        while let Some(caller) = callers.join_next().await {
            caller.unwrap();
        }
        background_tasks.shutdown().await;
        assert_eq!(live.load(Ordering::SeqCst), 0);
        let lease = Lease::new(&live);
        assert!(
            background_tasks
                .spawn(async move {
                    drop(lease);
                    panic!("must not be polled")
                })
                .is_none()
        );
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert!(background_tasks.tasks.is_empty());
    }
}
