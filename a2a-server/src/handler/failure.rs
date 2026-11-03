// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Persist a sanitized terminal status for failures of an existing Task.

use super::{DefaultRequestHandler, events::ActiveExecution, updates::persist_event};
use a2a::*;

/// Called only inside the Task coordinator. A failed lazy start leaves no Task.
pub(super) async fn persist_failure(
    handler: &DefaultRequestHandler,
    active: &ActiveExecution,
    task_id: &str,
    error: &A2AError,
) -> Option<Task> {
    if active.snapshot_is_terminal().await {
        return None;
    }
    let task = match handler.task_store.get(task_id).await {
        Ok(Some(task)) if !task.status.state.is_terminal() => task,
        Ok(_) => return None,
        Err(cause) => {
            tracing::warn!(%task_id, %cause, "could not load the failed Task");
            return None;
        }
    };
    let text = crate::sanitized_internal_error(&error.message).message;
    let mut event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: task.id.clone(),
        context_id: task.context_id.clone(),
        generation: 0,
        metadata: None,
        status: TaskStatus {
            state: TaskState::Failed,
            message: Some(Message {
                task_id: Some(task.id.clone()),
                context_id: Some(task.context_id.clone()),
                ..Message::new(Role::Agent, vec![Part::text(text)])
            }),
            timestamp: Some(chrono::Utc::now()),
        },
    });
    match persist_event(handler.task_store.as_ref(), Some(task), &mut event, false).await {
        Ok((snapshot, _)) => snapshot,
        Err(cause) => {
            tracing::warn!(%task_id, %cause, "could not persist the failed Task");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Clone, Default)]
    struct UnavailableStore {
        inner: Arc<InMemoryTaskStore>,
        fail_read: Arc<AtomicBool>,
        fail_write: Arc<AtomicBool>,
    }

    #[async_trait]
    impl TaskStore for UnavailableStore {
        async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.inner.create(task).await
        }
        async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
            if self.fail_write.load(Ordering::SeqCst) {
                return Err(A2AError::internal("storage write unavailable"));
            }
            self.inner.update(task).await
        }
        async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
            if self.fail_read.load(Ordering::SeqCst) {
                return Err(A2AError::internal("storage read unavailable"));
            }
            self.inner.get(id).await
        }
        async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
            self.inner.list(req).await
        }
        async fn begin_cancel(&self, id: &str) -> Result<Task, A2AError> {
            self.inner.begin_cancel(id).await
        }
    }

    #[tokio::test]
    async fn storage_outages_do_not_report_an_uncommitted_failure_or_corrupt_the_task() {
        let store = UnavailableStore::default();
        let handler = DefaultRequestHandler::new(EchoExecutor, store.clone());
        seed_task(&handler, "task", "context").await;
        let before = handler.load_task("task").await.unwrap();
        let active = ActiveExecution::new(None);
        active.set_snapshot(before.clone()).await;
        let error = A2AError::internal("private executor detail");

        store.fail_read.store(true, Ordering::SeqCst);
        assert!(
            persist_failure(&handler, &active, "task", &error)
                .await
                .is_none()
        );
        store.fail_read.store(false, Ordering::SeqCst);
        store.fail_write.store(true, Ordering::SeqCst);
        assert!(
            persist_failure(&handler, &active, "task", &error)
                .await
                .is_none()
        );
        assert_eq!(handler.load_task("task").await.unwrap(), before);
        assert_eq!(active.snapshot().await.unwrap(), before);

        store.fail_write.store(false, Ordering::SeqCst);
        let failed = persist_failure(&handler, &active, "task", &error)
            .await
            .unwrap();
        assert_eq!(failed.status.state, TaskState::Failed);
        assert_eq!(
            failed.status.message.unwrap().parts,
            vec![Part::text("Internal error")]
        );
        let listed = store
            .list(&serde_json::from_value(serde_json::json!({})).unwrap())
            .await
            .unwrap();
        assert_eq!(listed.tasks.len(), 1);
        assert_eq!(listed.tasks[0].status.state, TaskState::Failed);
        assert_eq!(
            store.begin_cancel("task").await.unwrap_err().code,
            error_code::TASK_NOT_CANCELABLE
        );

        active
            .set_snapshot(handler.load_task("task").await.unwrap())
            .await;
        assert!(
            persist_failure(&handler, &active, "task", &error)
                .await
                .is_none()
        );
        assert_eq!(handler.load_task("task").await.unwrap(), listed.tasks[0]);
    }
    #[tokio::test]
    async fn an_executor_failure_releases_its_owner_even_when_failure_persistence_is_down() {
        use crate::handler::{execution::TurnEnd, runtime::TaskRuntime};
        for read_outage in [true, false] {
            let store = UnavailableStore::default();
            let handler = DefaultRequestHandler::new(EchoExecutor, store.clone());
            seed_task(&handler, "task", "context").await;
            let before = handler.load_task("task").await.unwrap();
            let mut runtime = TaskRuntime::default();
            let (active, _inputs) = runtime.reserve(&handler, "task").await.unwrap();
            active.set_snapshot(before.clone()).await;
            let mut events = active.subscribe().await;
            store.fail_read.store(read_outage, Ordering::SeqCst);
            store.fail_write.store(!read_outage, Ordering::SeqCst);
            runtime
                .finished(
                    &handler,
                    "task".into(),
                    active.clone(),
                    TurnEnd::Failed(A2AError::internal("agent failed")),
                )
                .await;
            assert!(events.recv().await.unwrap().result.is_err());
            assert!(handler.execution_manager.get("task").await.is_none());
            assert!(runtime.execution.invocation().is_none());
            assert_eq!(active.snapshot().await.unwrap(), before);
            store.fail_read.store(false, Ordering::SeqCst);
            assert_eq!(handler.load_task("task").await.unwrap(), before);
            let (successor, _inputs) = runtime.reserve(&handler, "task").await.unwrap();
            assert_ne!(successor.execution_id, active.execution_id);
            runtime.release(&handler, "task", &successor).await;
        }
    }
}
