// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! One weak routing entry per task; channels live in the task handle.

use super::task_handle::TaskHandle;
use a2a::TaskId;
use parking_lot::RwLock;
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

#[derive(Default)]
pub(super) struct TaskRegistry {
    tasks: RwLock<HashMap<TaskId, Weak<TaskHandle>>>,
}

impl TaskRegistry {
    pub(super) fn task(&self, task_id: &str) -> Arc<TaskHandle> {
        if let Some(task) = self.tasks.read().get(task_id).and_then(Weak::upgrade) {
            return task;
        }
        let mut tasks = self.tasks.write();
        // Another request may have installed a handle while we acquired the writer.
        if let Some(task) = tasks.get(task_id).and_then(Weak::upgrade) {
            return task;
        }
        tasks.retain(|_, task| task.strong_count() > 0);
        let task = TaskHandle::new(task_id.to_owned(), None);
        tasks.insert(task_id.to_owned(), Arc::downgrade(&task));
        task
    }

    #[cfg(test)]
    pub(super) fn watcher_count(&self) -> usize {
        self.tasks
            .read()
            .values()
            .filter_map(Weak::upgrade)
            .filter(|task| task.is_watched())
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_first_requests_share_the_task_and_operation_queue() {
        let registry = Arc::new(TaskRegistry::default());
        let barrier = Arc::new(tokio::sync::Barrier::new(32));
        let mut requests = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let registry = registry.clone();
            let barrier = barrier.clone();
            requests.spawn(async move {
                barrier.wait().await;
                let task = registry.task("shared");
                let operations = task.operations();
                (task, operations)
            });
        }
        let (task, operations) = requests.join_next().await.unwrap().unwrap();
        while let Some(result) = requests.join_next().await {
            let (other_task, other_operations) = result.unwrap();
            assert!(Arc::ptr_eq(&task, &other_task));
            assert!(Arc::ptr_eq(&operations, &other_operations));
        }
        let (active, _inputs, _state) = task.reserve().unwrap();
        assert!(Arc::ptr_eq(&active.task, &task));
        assert!(Arc::ptr_eq(
            &registry.task("shared").active().unwrap(),
            &active
        ));
        assert!(!Arc::ptr_eq(&task, &registry.task("different")));
    }

    #[tokio::test]
    async fn releasing_all_task_users_retires_its_worker_and_handle() {
        let registry = TaskRegistry::default();
        let task = registry.task("retired");
        let weak = Arc::downgrade(&task);
        let watch = task.watch();
        let operations = task.operations();
        super::super::runtime::submit(
            operations.clone(),
            super::super::runtime::TaskOperation::Subscribe,
            |_| Box::pin(async {}),
        )
        .await
        .unwrap();
        drop(operations);
        drop(task);
        assert!(weak.upgrade().is_some(), "the watcher retains the task");
        drop(watch);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the handle and worker must not form a reference cycle");
        let _next = registry.task("next");
        assert!(!registry.tasks.read().contains_key("retired"));
    }

    /// A reservation whose request was dropped (its release still on its way)
    /// is replaced by the next request instead of being mistaken for a running
    /// execution.
    #[test]
    fn test_reserve_replaces_a_dropped_requests_leftover() {
        let manager = TaskRegistry::default();
        let (stale, _inputs, _invocation) = manager.task("t-stale").reserve().unwrap();

        let (fresh, _inputs, _invocation) = manager
            .task("t-stale")
            .reserve()
            .expect("the leftover is not a running execution");

        assert!(!Arc::ptr_eq(&stale, &fresh));
        let registered = manager.task("t-stale").active().unwrap();
        assert!(Arc::ptr_eq(&registered, &fresh));
        // A stale routing handle must not remove its successor.
        manager.task("t-stale").unregister(&stale);
        assert!(manager.task("t-stale").active().is_some());
    }

    #[tokio::test]
    async fn test_stream_disconnect_preserves_execution_for_resubscription() {
        use futures::StreamExt;

        let (handler, release) = make_resumable_handler();
        let params = ServiceParams::new();
        let mut message = make_message();
        message.task_id = Some("t-disconnect".into());
        message.context_id = Some("c-disconnect".into());
        seed_task(&handler, "t-disconnect", "c-disconnect").await;

        let mut stream = handler
            .send_streaming_message(
                &params,
                SendMessageRequest {
                    artifact_updates: Vec::new(),
                    message,
                    configuration: None,
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let initial = stream.next().await.unwrap().unwrap();
        match initial {
            StreamResponse::Task(task) => assert_eq!(task.status.state, TaskState::Working),
            _ => panic!("expected initial working task"),
        }
        drop(stream);

        let mut resumed = handler
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    last_seen_generation: None,
                    id: "t-disconnect".into(),
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let snapshot = resumed.next().await.unwrap().unwrap();
        match snapshot {
            StreamResponse::Task(task) => assert_eq!(task.status.state, TaskState::Working),
            _ => panic!("expected working task snapshot"),
        }

        release.notify_waiters();

        let terminal = resumed.next().await.unwrap().unwrap();
        match terminal {
            StreamResponse::Task(task) => {
                assert_eq!(task.status.state, TaskState::Completed)
            }
            _ => panic!("expected terminal task"),
        }
        assert!(resumed.next().await.is_none());
    }

    #[tokio::test]
    async fn a_prepared_execution_cannot_be_reserved_twice() {
        let manager = TaskRegistry::default();
        let (active, _inputs, _invocation) = manager.task("task").reserve().unwrap();
        let task = task_with_history("task", "context", 0);
        active.set_snapshot(task);
        assert!(manager.task("task").reserve().is_err());
        assert!(Arc::ptr_eq(
            &manager.task("task").active().unwrap(),
            &active
        ));
    }

    #[test]
    fn task_registry_remains_usable_after_panic_and_prunes_unused_handles() {
        let manager = TaskRegistry::default();
        let held = manager.task("held");
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _sources = manager.tasks.write();
                panic!("interrupted registry owner");
            }))
            .is_err()
        );
        for id in 0..256 {
            drop(manager.task(&format!("abandoned-{id}")));
        }
        assert!(Arc::ptr_eq(&held, &manager.task("held")));
        let sources = manager.tasks.write();
        assert!(!sources.contains_key("abandoned-0"));
        assert!(sources.contains_key("held"));
    }

    #[test]
    fn dropping_the_owner_expires_the_route_even_while_the_registry_is_locked() {
        let manager = TaskRegistry::default();
        let (active, _inputs, _invocation) = manager.task("dropped-owner").reserve().unwrap();
        let task = active.task.clone();
        let registry = manager.tasks.read();
        drop(active);
        assert!(
            registry
                .get("dropped-owner")
                .unwrap()
                .upgrade()
                .unwrap()
                .active()
                .is_none()
        );
        drop(registry);
        assert!(task.active().is_none());
    }
}
