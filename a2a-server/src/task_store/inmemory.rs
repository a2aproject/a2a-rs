// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use async_trait::async_trait;
#[cfg(test)]
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::Instant;

use super::apply_history_length;
use super::cursor::{KeysetCursors, Position, listing_query};
use super::store::{TaskListScope, TaskStore, TaskVersion};
use crate::pagination::resolve_page_size;

struct StoredEntry {
    task: Arc<Task>,
    version: TaskVersion,
}

const MAX_LISTINGS: usize = 1024;
const MAX_LISTING_IDS: usize = 1_000_000;
const MAX_PRINCIPAL_LISTINGS: usize = 32;
const MAX_PRINCIPAL_IDS: usize = 250_000;
const MAX_TENANT_LISTINGS: usize = 256;
const MAX_TENANT_IDS: usize = 500_000;
const LISTING_TTL: Duration = Duration::from_secs(300);

fn matches_task(
    task: &Task,
    req: &ListTasksRequest,
    visible: Option<&(dyn for<'id> Fn(&'id str) -> bool + Send + Sync)>,
) -> bool {
    visible.is_none_or(|visible| visible(&task.id))
        && req
            .context_id
            .as_ref()
            .is_none_or(|id| *id == task.context_id)
        && req
            .status
            .as_ref()
            .is_none_or(|state| *state == task.status.state)
        && req
            .status_timestamp_after
            .is_none_or(|after| task.status.timestamp.is_some_and(|time| time >= after))
}

fn principal(scope: &TaskListScope) -> Option<&str> {
    scope
        .principal
        .as_ref()
        .and_then(|(authenticated, name)| authenticated.then_some(name.as_str()))
}

struct Listing {
    id: uuid::Uuid,
    expires: Instant,
    query: ListTasksRequest,
    tasks: Arc<Vec<TaskId>>,
    scope: TaskListScope,
}

#[derive(Default)]
struct Tasks {
    entries: HashMap<TaskId, StoredEntry>,
    /// Terminal task ids, oldest first; tracked only under a limit.
    terminal: VecDeque<TaskId>,
    /// Bounded, short-lived membership snapshots. Only IDs are retained;
    /// continuation pages always read current task contents.
    listings: VecDeque<Listing>,
}

impl Tasks {
    fn expire_listings(&mut self) {
        let now = Instant::now();
        self.listings.retain(|listing| listing.expires > now);
    }

    /// Records that `id` reached a terminal state, then purges the tasks that
    /// reached one earliest until at most `limit` remain.
    fn retire(&mut self, id: &str, limit: Option<usize>) {
        let Some(limit) = limit else {
            return;
        };
        self.terminal.push_back(id.to_string());
        while self.terminal.len() > limit {
            if let Some(oldest) = self.terminal.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }
}

/// In-memory task store. Contents do not survive restarts.
///
/// Keeps every task for the life of the process unless
/// [`Self::with_terminal_task_limit`] bounds how many finished ones it holds.
/// Authenticated lists can cache ordered IDs for five minutes of inactivity.
/// Anonymous lists and cache overflow use stateless, scope-bound live cursors.
/// Every page rechecks current contents, filters and visibility. Snapshot-cache
/// limits never prevent another caller from listing tasks.
pub struct InMemoryTaskStore {
    tasks: RwLock<Tasks>,
    terminal_task_limit: Option<usize>,
    listing_ttl: Duration,
    cursors: KeysetCursors,
}

impl InMemoryTaskStore {
    pub fn new() -> Self {
        InMemoryTaskStore {
            tasks: RwLock::new(Tasks::default()),
            terminal_task_limit: None,
            listing_ttl: LISTING_TTL,
            cursors: KeysetCursors::default(),
        }
    }

    // Live cursor positions retain no per-caller state. This is the default
    // for anonymous access and the fallback when snapshot caching is full.
    async fn list_stateless(
        &self,
        req: &ListTasksRequest,
        scope: &TaskListScope,
        visible: Option<&(dyn for<'id> Fn(&'id str) -> bool + Send + Sync)>,
    ) -> Result<ListTasksResponse, A2AError> {
        let position = self
            .cursors
            .position(req.page_token.as_deref(), req, scope)?;
        let mut tasks: Vec<_> = self
            .tasks
            .read()
            .await
            .entries
            .values()
            .map(|entry| entry.task.clone())
            .collect();
        tasks.retain(|task| matches_task(task, req, visible));
        tasks.sort_by_key(|task| (std::cmp::Reverse(task.status.timestamp), task.id.clone()));
        let total_size = tasks.len().min(i32::MAX as usize) as i32;
        let start = match position {
            Position::Offset(position) => position,
            Position::After((timestamp, id)) => tasks.partition_point(|task| {
                (std::cmp::Reverse(task.status.timestamp), &task.id)
                    <= (std::cmp::Reverse(timestamp), &id)
            }),
        };
        let page_size = resolve_page_size(req.page_size);
        let has_more = start.saturating_add(page_size) < tasks.len();
        let page: Vec<_> = tasks.into_iter().skip(start).take(page_size).collect();
        let next_page_token = if has_more {
            page.last()
                .map(|task| self.cursors.encode(task, req, scope))
                .transpose()?
                .unwrap_or_default()
        } else {
            String::new()
        };
        let tasks = page
            .into_iter()
            .map(|task| {
                let mut task = (*task).clone();
                apply_history_length(&mut task, req.history_length);
                task
            })
            .collect();
        Ok(ListTasksResponse {
            tasks,
            next_page_token,
            page_size: page_size as i32,
            total_size,
        })
    }

    /// Expire a list after this much inactivity. Successful continuations renew it.
    pub fn with_listing_ttl(mut self, ttl: Duration) -> Self {
        self.listing_ttl = ttl;
        self
    }

    /// Keep at most `limit` tasks in a terminal state, purging the one that
    /// reached its terminal state earliest first. A purged task is reported
    /// as `TaskNotFoundError`, which §3.3.2 allows for a completed and purged
    /// task. Tasks that are not terminal are never purged.
    pub fn with_terminal_task_limit(mut self, limit: usize) -> Self {
        self.terminal_task_limit = Some(limit);
        self
    }
}

impl Default for InMemoryTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl TaskStore for InMemoryTaskStore {
    async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
        let mut store = self.tasks.write().await;
        store.expire_listings();
        if store.entries.contains_key(&task.id) {
            return Err(crate::sanitized_internal_error("task already exists"));
        }
        let id = task.id.clone();
        let terminal = task.status.state.is_terminal();
        store.entries.insert(
            id.clone(),
            StoredEntry {
                task: Arc::new(task),
                version: 1,
            },
        );
        if terminal {
            store.retire(&id, self.terminal_task_limit);
        }
        Ok(1)
    }

    async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
        let mut store = self.tasks.write().await;
        store.expire_listings();
        let entry = store
            .entries
            .get_mut(&task.id)
            .ok_or_else(|| A2AError::task_not_found(&task.id))?;
        // A terminal task's state is immutable. Re-applying the same
        // terminal state is allowed so idempotent re-emission (e.g. cancel
        // flows) keeps working, but a terminal state can never be overwritten
        // by a different state.
        if entry.task.status.state.is_terminal() && entry.task.status.state != task.status.state {
            return Err(A2AError::invalid_request(format!(
                "task {} is in terminal state {:?} and cannot transition to {:?}",
                task.id, entry.task.status.state, task.status.state
            )));
        }
        let retired = !entry.task.status.state.is_terminal() && task.status.state.is_terminal();
        entry.version += 1;
        entry.task = Arc::new(task);
        let version = entry.version;
        if retired {
            let id = entry.task.id.clone();
            store.retire(&id, self.terminal_task_limit);
        }
        Ok(version)
    }

    async fn begin_cancel(&self, task_id: &str) -> Result<Task, A2AError> {
        // Hold the write lock across the check and the state transition so two
        // concurrent cancel requests cannot both pass the check-then-act window
        // window.
        let mut store = self.tasks.write().await;
        store.expire_listings();
        let entry = store
            .entries
            .get_mut(task_id)
            .ok_or_else(|| A2AError::task_not_found(task_id))?;
        if entry.task.status.state.is_terminal() {
            return Err(A2AError::task_not_cancelable(task_id));
        }
        entry.version += 1;
        let task = Arc::make_mut(&mut entry.task);
        task.status.state = TaskState::Canceled;
        // What the agent said while working does not describe the cancellation.
        task.status.message = None;
        task.status.timestamp = Some(chrono::Utc::now());
        // The state change and its generation land in one write: no reader
        // sees the task canceled at the generation it had while working. The
        // handler announces the transition as the one status event of this
        // generation.
        task.generation += 1;
        // Cancellation completes every artifact at this same generation.
        for artifact in task.artifacts.iter_mut().flatten() {
            artifact.end_generation.get_or_insert(task.generation);
        }
        let status = task.status.clone();
        let generation = task.generation;
        task.timeline
            .get_or_insert_with(Vec::new)
            .push(TimelineEntry {
                content: TimelineContent::Status(status),
                generation,
            });
        let task = task.clone();
        store.retire(task_id, self.terminal_task_limit);
        Ok(task)
    }

    async fn get(&self, task_id: &str) -> Result<Option<Task>, A2AError> {
        let store = self.tasks.read().await;
        Ok(store.entries.get(task_id).map(|e| (*e.task).clone()))
    }

    async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
        self.list_scoped(req, &|_| true).await
    }

    async fn list_scoped(
        &self,
        req: &ListTasksRequest,
        visible: &(dyn for<'id> Fn(&'id str) -> bool + Send + Sync),
    ) -> Result<ListTasksResponse, A2AError> {
        self.list_for_scope(
            req,
            &TaskListScope {
                tenant: req.tenant.clone(),
                principal: None,
            },
            Some(visible),
        )
        .await
    }

    async fn list_for_scope(
        &self,
        req: &ListTasksRequest,
        scope: &TaskListScope,
        visible: Option<&(dyn for<'id> Fn(&'id str) -> bool + Send + Sync)>,
    ) -> Result<ListTasksResponse, A2AError> {
        if principal(scope).is_none()
            || req
                .page_token
                .as_deref()
                .is_some_and(|token| token.starts_with("keyset:"))
        {
            return self.list_stateless(req, scope, visible).await;
        }
        self.tasks.write().await.expire_listings();
        let page_size = resolve_page_size(req.page_size);
        let token = req.page_token.as_deref().filter(|token| !token.is_empty());
        let continuation = token.and_then(|token| token.strip_prefix("snapshot:"));
        let matches = |task: &Task| matches_task(task, req, visible);
        let (ids, start, listing_id) = if let Some(cursor) = continuation {
            let (id, position) = cursor
                .split_once(':')
                .ok_or_else(|| A2AError::invalid_params("invalid page token"))?;
            let id = uuid::Uuid::parse_str(id)
                .map_err(|_| A2AError::invalid_params("invalid page token"))?;
            let position = position
                .parse::<usize>()
                .map_err(|_| A2AError::invalid_params("invalid page token"))?;
            let mut store = self.tasks.write().await;
            store.expire_listings();
            let listing = store
                .listings
                .iter_mut()
                .find(|listing| listing.id == id)
                .ok_or_else(|| A2AError::invalid_params("page token expired; start a new list"))?;
            if listing.scope != *scope
                || listing.query != listing_query(req)
                || position > listing.tasks.len()
            {
                return Err(A2AError::invalid_params("invalid page token"));
            }
            listing.expires = Instant::now() + self.listing_ttl;
            (listing.tasks.clone(), position, id)
        } else {
            // Copy only Arcs under the read lock. Authorization and sorting run
            // without a store lock, so a slow listing never holds up writes.
            let mut tasks: Vec<_> = {
                let store = self.tasks.read().await;
                store
                    .entries
                    .values()
                    .map(|entry| entry.task.clone())
                    .collect()
            };
            tasks.retain(|task| matches(task));
            tasks.sort_by_key(|task| (std::cmp::Reverse(task.status.timestamp), task.id.clone()));
            let start = match self.cursors.position(token, req, scope)? {
                Position::Offset(position) => position,
                Position::After((timestamp, id)) => tasks.partition_point(|task| {
                    (std::cmp::Reverse(task.status.timestamp), &task.id)
                        <= (std::cmp::Reverse(timestamp), &id)
                }),
            };
            (
                Arc::new(
                    tasks
                        .into_iter()
                        .map(|task| task.id.clone())
                        .collect::<Vec<_>>(),
                ),
                start,
                uuid::Uuid::now_v7(),
            )
        };
        // Refresh contents without cloning whole Tasks while holding a lock.
        let current: Vec<_> = {
            let store = self.tasks.read().await;
            ids.iter()
                .enumerate()
                .filter_map(|(position, id)| {
                    store
                        .entries
                        .get(id)
                        .map(|entry| (position, entry.task.clone()))
                })
                .collect()
        };
        let mut total_size = 0;
        let mut page = Vec::with_capacity(page_size + 1);
        for (position, task) in current {
            if matches(&task) {
                total_size += 1;
                if position >= start && page.len() <= page_size {
                    page.push((position, task));
                }
            }
        }
        let has_more = page.len() > page_size;
        page.truncate(page_size);
        let next_page_token = if has_more {
            if continuation.is_none() {
                let mut store = self.tasks.write().await;
                store.expire_listings();
                let usage = |predicate: &dyn Fn(&Listing) -> bool| {
                    store
                        .listings
                        .iter()
                        .filter(|entry| predicate(entry))
                        .fold((0usize, 0usize), |(count, size), entry| {
                            (count + 1, size + entry.tasks.len())
                        })
                };
                // A request-supplied tenant cannot mint cache budgets. Cache
                // exhaustion changes cursor mode, never listing availability.
                let principal_usage = usage(&|entry| principal(&entry.scope) == principal(scope));
                let tenant = usage(&|entry| entry.scope.tenant == scope.tenant);
                let global = usage(&|_| true);
                if principal_usage.0 >= MAX_PRINCIPAL_LISTINGS
                    || ids.len() > MAX_PRINCIPAL_IDS.saturating_sub(principal_usage.1)
                    || (scope.tenant.is_some()
                        && (tenant.0 >= MAX_TENANT_LISTINGS
                            || ids.len() > MAX_TENANT_IDS.saturating_sub(tenant.1)))
                    || global.0 >= MAX_LISTINGS
                    || ids.len() > MAX_LISTING_IDS.saturating_sub(global.1)
                {
                    drop(store);
                    return self.list_stateless(req, scope, visible).await;
                }
                store.listings.push_back(Listing {
                    id: listing_id,
                    expires: Instant::now() + self.listing_ttl,
                    query: listing_query(req),
                    tasks: ids,
                    scope: scope.clone(),
                });
            }
            page.last()
                .map(|(position, _)| format!("snapshot:{listing_id}:{}", position + 1))
        } else {
            None
        };
        let page = page
            .into_iter()
            .map(|(_, task)| {
                let mut task = (*task).clone();
                apply_history_length(&mut task, req.history_length);
                task
            })
            .collect();
        Ok(ListTasksResponse {
            tasks: page,
            next_page_token: next_page_token.unwrap_or_default(),
            page_size: page_size as i32,
            total_size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_task(id: &str, ctx: &str, state: TaskState) -> Task {
        Task {
            generation: 0,
            timeline: None,
            id: id.to_string(),
            context_id: ctx.to_string(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        }
    }

    fn list_request(page_size: i32) -> ListTasksRequest {
        ListTasksRequest {
            context_id: None,
            status: None,
            page_size: Some(page_size),
            page_token: None,
            history_length: None,
            timeline_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        }
    }

    async fn dated_store() -> InMemoryTaskStore {
        let store = InMemoryTaskStore::new();
        for (id, seconds) in [("a", 30), ("b", 20), ("c", 10)] {
            let mut task = make_task(id, "ctx", TaskState::Submitted);
            task.status.timestamp = chrono::DateTime::from_timestamp(seconds, 0);
            store.create(task).await.unwrap();
        }
        store
    }

    fn cached_scope() -> TaskListScope {
        TaskListScope {
            tenant: None,
            principal: Some((true, "snapshot-reader".into())),
        }
    }

    async fn cached_list(
        store: &InMemoryTaskStore,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        store.list_for_scope(req, &cached_scope(), None).await
    }

    #[tokio::test]
    async fn list_snapshot_does_not_skip_updated_tasks_or_repeat_moved_tasks() {
        let store = dated_store().await;
        let mut req = list_request(1);
        let first = cached_list(&store, &req).await.unwrap();
        assert_eq!(first.tasks[0].id, "a");

        for (id, seconds) in [("a", 0), ("b", 40), ("d", 50)] {
            let mut task = make_task(id, "ctx", TaskState::Working);
            task.status.timestamp = chrono::DateTime::from_timestamp(seconds, 0);
            if id == "d" {
                store.create(task).await.unwrap();
            } else {
                store.update(task).await.unwrap();
            }
        }

        req.page_token = Some(first.next_page_token);
        let second = cached_list(&store, &req).await.unwrap();
        assert_eq!(second.tasks[0].id, "b");
        assert_eq!(second.tasks[0].status.state, TaskState::Working);
        assert_eq!(second.total_size, 3);
        // Replaying a continuation is stable too.
        assert_eq!(cached_list(&store, &req).await.unwrap(), second);
        req.page_token = Some(second.next_page_token);
        let third = cached_list(&store, &req).await.unwrap();
        assert_eq!(third.tasks[0].id, "c");
        assert!(third.next_page_token.is_empty());
        let fresh = store.list(&list_request(10)).await.unwrap();
        assert_eq!(
            fresh
                .tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            ["d", "b", "c", "a"]
        );
        assert_eq!(fresh.tasks[1].status.state, TaskState::Working);
    }

    #[tokio::test]
    async fn list_accepts_legacy_numeric_and_sort_key_tokens() {
        let store = dated_store().await;
        let timestamp = chrono::DateTime::from_timestamp(30, 0);
        for token in [
            "1".into(),
            URL_SAFE_NO_PAD.encode(serde_json::to_vec(&(timestamp, "a")).unwrap()),
        ] {
            let mut req = list_request(1);
            req.page_token = Some(token);
            let page = store.list(&req).await.unwrap();
            assert_eq!(page.tasks[0].id, "b");
            assert!(page.next_page_token.starts_with("keyset:"));
            req.page_token = Some(page.next_page_token);
            assert_eq!(store.list(&req).await.unwrap().tasks[0].id, "c");
        }
        let mut req = list_request(1);
        req.page_token = Some(usize::MAX.to_string());
        assert!(store.list(&req).await.unwrap().tasks.is_empty());
    }

    #[tokio::test]
    async fn list_snapshot_rechecks_visibility_deletion_and_query_filters() {
        let store = dated_store().await;
        let mut req = list_request(1);
        req.page_token = Some(cached_list(&store, &req).await.unwrap().next_page_token);
        let scoped = store
            .list_for_scope(&req, &cached_scope(), Some(&|id| id == "c"))
            .await
            .unwrap();
        assert_eq!(scoped.tasks[0].id, "c");
        assert_eq!(scoped.total_size, 1);
        assert!(scoped.next_page_token.is_empty());

        store.tasks.write().await.entries.remove("b");
        assert_eq!(cached_list(&store, &req).await.unwrap().tasks[0].id, "c");
        req.context_id = Some("different-context".into());
        assert_eq!(
            cached_list(&store, &req).await.unwrap_err().code,
            error_code::INVALID_PARAMS
        );
    }

    #[tokio::test]
    async fn continuation_rechecks_current_filter_values() {
        let store = dated_store().await;
        let mut req = list_request(1);
        req.status = Some(TaskState::Submitted);
        req.page_token = Some(store.list(&req).await.unwrap().next_page_token);
        store
            .update(make_task("b", "ctx", TaskState::Working))
            .await
            .unwrap();
        let page = store.list(&req).await.unwrap();
        assert_eq!(page.tasks[0].id, "c");
        assert_eq!(page.total_size, 2);
    }

    #[tokio::test]
    async fn cache_capacity_preserves_existing_cursors_and_falls_back_without_retaining_tasks() {
        let store = dated_store().await;
        let mut req = list_request(1);
        req.page_token = Some(cached_list(&store, &req).await.unwrap().next_page_token);
        for _ in 1..MAX_PRINCIPAL_LISTINGS {
            cached_list(&store, &list_request(1)).await.unwrap();
        }
        assert_eq!(
            store.tasks.read().await.listings.len(),
            MAX_PRINCIPAL_LISTINGS
        );
        assert!(
            cached_list(&store, &list_request(1))
                .await
                .unwrap()
                .next_page_token
                .starts_with("keyset:")
        );
        assert_eq!(cached_list(&store, &req).await.unwrap().tasks[0].id, "b");
        let old_task = Arc::downgrade(&store.tasks.read().await.entries["a"].task);
        store
            .update(make_task("a", "ctx", TaskState::Working))
            .await
            .unwrap();
        assert!(old_task.upgrade().is_none());
        for listing in &mut store.tasks.write().await.listings {
            listing.expires = Instant::now() - Duration::from_secs(1);
        }
        cached_list(&store, &list_request(10)).await.unwrap();
        assert!(store.tasks.read().await.listings.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn caller_quota_and_cursor_ownership_are_isolated_and_activity_renews_ttl() {
        let store = dated_store()
            .await
            .with_listing_ttl(Duration::from_secs(120));
        let alice = TaskListScope {
            tenant: Some("tenant".into()),
            principal: Some((true, "alice".into())),
        };
        let bob = TaskListScope {
            principal: Some((true, "bob".into())),
            ..alice.clone()
        };
        let request = list_request(1);
        let mut continuation = request.clone();
        continuation.page_token = Some(
            store
                .list_for_scope(&request, &alice, None)
                .await
                .unwrap()
                .next_page_token,
        );
        for _ in 1..MAX_PRINCIPAL_LISTINGS {
            store.list_for_scope(&request, &alice, None).await.unwrap();
        }
        assert!(
            store
                .list_for_scope(&request, &alice, None)
                .await
                .unwrap()
                .next_page_token
                .starts_with("keyset:")
        );
        assert!(store.list_for_scope(&request, &bob, None).await.is_ok());
        assert!(
            store
                .list_for_scope(&continuation, &bob, None)
                .await
                .is_err()
        );
        tokio::time::advance(Duration::from_secs(90)).await;
        assert!(
            store
                .list_for_scope(&continuation, &alice, None)
                .await
                .is_ok()
        );
        tokio::time::advance(Duration::from_secs(90)).await;
        assert!(
            store
                .list_for_scope(&continuation, &alice, None)
                .await
                .is_ok()
        );
        tokio::time::advance(Duration::from_secs(121)).await;
        assert!(
            store
                .list_for_scope(&continuation, &alice, None)
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn anonymous_lists_and_cross_tenant_cache_overflow_never_exhaust_other_callers() {
        let store = dated_store().await;
        for _ in 0..64 {
            assert!(
                store
                    .list(&list_request(1))
                    .await
                    .unwrap()
                    .next_page_token
                    .starts_with("keyset:")
            );
        }
        for index in 0..64 {
            let scope = TaskListScope {
                tenant: Some(format!("public-{index}")),
                principal: Some((false, format!("untrusted-{index}"))),
            };
            let page = store
                .list_for_scope(&list_request(1), &scope, None)
                .await
                .unwrap();
            assert!(page.next_page_token.starts_with("keyset:"));
        }
        assert!(store.tasks.read().await.listings.is_empty());
        for index in 0..MAX_PRINCIPAL_LISTINGS {
            let scope = TaskListScope {
                tenant: Some(format!("tenant-{index}")),
                principal: Some((true, "alice".into())),
            };
            store
                .list_for_scope(&list_request(1), &scope, None)
                .await
                .unwrap();
        }
        let scope = TaskListScope {
            tenant: Some("another-tenant".into()),
            principal: Some((true, "alice".into())),
        };
        assert!(
            store
                .list_for_scope(&list_request(1), &scope, None)
                .await
                .unwrap()
                .next_page_token
                .starts_with("keyset:")
        );
        assert!(
            store
                .list(&list_request(1))
                .await
                .unwrap()
                .next_page_token
                .starts_with("keyset:")
        );
        assert_eq!(
            store.tasks.read().await.listings.len(),
            MAX_PRINCIPAL_LISTINGS
        );
    }

    #[tokio::test]
    async fn global_snapshot_capacity_never_denies_new_principals_or_anonymous_lists() {
        let store = dated_store().await;
        for principal in 0..MAX_LISTINGS / MAX_PRINCIPAL_LISTINGS {
            let scope = TaskListScope {
                tenant: None,
                principal: Some((true, format!("reader-{principal}"))),
            };
            for _ in 0..MAX_PRINCIPAL_LISTINGS {
                store
                    .list_for_scope(&list_request(1), &scope, None)
                    .await
                    .unwrap();
            }
        }
        assert_eq!(store.tasks.read().await.listings.len(), MAX_LISTINGS);
        let scope = TaskListScope {
            tenant: None,
            principal: Some((true, "new-reader".into())),
        };
        let first = store
            .list_for_scope(&list_request(1), &scope, None)
            .await
            .unwrap();
        assert!(first.next_page_token.starts_with("keyset:"));
        let mut next = list_request(1);
        next.page_token = Some(first.next_page_token);
        assert_eq!(
            store
                .list_for_scope(&next, &scope, None)
                .await
                .unwrap()
                .tasks[0]
                .id,
            "b"
        );
        assert!(store.list(&list_request(1)).await.is_ok());
        assert_eq!(store.tasks.read().await.listings.len(), MAX_LISTINGS);
    }

    #[tokio::test]
    async fn anonymous_live_cursors_recheck_authorization_and_bind_query_and_scope() {
        let store = dated_store().await;
        let scope = TaskListScope::default();
        let mut req = list_request(1);
        req.page_token = Some(store.list(&req).await.unwrap().next_page_token);
        let page = store
            .list_for_scope(&req, &scope, Some(&|id| id == "c"))
            .await
            .unwrap();
        assert_eq!(page.tasks[0].id, "c");
        assert_eq!(page.total_size, 1);
        let mut other = req.clone();
        other.context_id = Some("other".into());
        assert!(store.list(&other).await.is_err());
        let other_scope = TaskListScope {
            tenant: Some("other".into()),
            ..scope
        };
        assert!(
            store
                .list_for_scope(&req, &other_scope, None)
                .await
                .is_err()
        );
        assert!(store.tasks.read().await.listings.is_empty());
    }

    #[tokio::test]
    async fn authorization_predicates_run_without_a_store_lock() {
        let store = dated_store().await;
        store
            .list_scoped(&list_request(1), &|_| {
                assert!(
                    store.tasks.try_write().is_ok(),
                    "listing held a store lock while authorizing"
                );
                true
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn malformed_cursors_cannot_invalidate_an_existing_listing() {
        let store = dated_store().await;
        let mut request = list_request(1);
        let cursor = cached_list(&store, &request).await.unwrap().next_page_token;
        let id = cursor.split(':').nth(1).unwrap();
        for invalid in [
            "snapshot:missing-position".to_string(),
            "snapshot:not-a-uuid:1".to_string(),
            format!("snapshot:{id}:not-a-position"),
            format!("snapshot:{id}:{}", usize::MAX),
            URL_SAFE_NO_PAD.encode(b"not a JSON sort key"),
            "999999999999999999999999999999999999999999".to_string(),
        ] {
            request.page_token = Some(invalid);
            assert_eq!(
                cached_list(&store, &request).await.unwrap_err().code,
                error_code::INVALID_PARAMS
            );
        }
        request.page_token = Some(cursor);
        assert_eq!(
            cached_list(&store, &request).await.unwrap().tasks[0].id,
            "b"
        );
    }

    #[tokio::test]
    async fn test_create_and_get() {
        let store = InMemoryTaskStore::new();
        let task = make_task("t1", "c1", TaskState::Submitted);
        let ver = store.create(task.clone()).await.unwrap();
        assert_eq!(ver, 1);

        let got = store.get("t1").await.unwrap().unwrap();
        assert_eq!(got.id, "t1");
        assert_eq!(got.context_id, "c1");
    }

    #[tokio::test]
    async fn test_create_duplicate() {
        let store = InMemoryTaskStore::new();
        let task = make_task("t1", "c1", TaskState::Submitted);
        store.create(task.clone()).await.unwrap();

        let result = store.create(task).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_update() {
        let store = InMemoryTaskStore::new();
        let task = make_task("t1", "c1", TaskState::Submitted);
        store.create(task).await.unwrap();

        let updated = make_task("t1", "c1", TaskState::Working);
        let ver = store.update(updated).await.unwrap();
        assert_eq!(ver, 2);

        let got = store.get("t1").await.unwrap().unwrap();
        assert_eq!(got.status.state, TaskState::Working);
    }

    #[tokio::test]
    async fn test_update_not_found() {
        let store = InMemoryTaskStore::new();
        let task = make_task("t1", "c1", TaskState::Working);
        let result = store.update(task).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_update_terminal_state_rejected() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task("t1", "c1", TaskState::Completed))
            .await
            .unwrap();

        // A terminal task cannot transition to a different state.
        for state in [
            TaskState::Submitted,
            TaskState::Working,
            TaskState::Canceled,
            TaskState::Failed,
        ] {
            let result = store.update(make_task("t1", "c1", state.clone())).await;
            assert!(result.is_err(), "expected {state:?} update to be rejected");
            assert_eq!(
                result.unwrap_err().code,
                error_code::INVALID_REQUEST,
                "unexpected code for {state:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_update_terminal_same_state_allowed() {
        let store = InMemoryTaskStore::new();
        let mut task = make_task("t1", "c1", TaskState::Canceled);
        store.create(task.clone()).await.unwrap();

        // Re-applying the same terminal state (idempotent re-emission) is fine.
        task.status.message = Some(Message::new(Role::Agent, vec![Part::text("after cancel")]));
        let ver = store.update(task).await.unwrap();
        assert_eq!(ver, 2);
        let got = store.get("t1").await.unwrap().unwrap();
        assert_eq!(got.status.state, TaskState::Canceled);
        assert!(got.status.message.is_some());
    }

    #[tokio::test]
    async fn test_begin_cancel_transitions_non_terminal() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task("t1", "c1", TaskState::Working))
            .await
            .unwrap();

        let task = store.begin_cancel("t1").await.unwrap();
        assert_eq!(task.status.state, TaskState::Canceled);

        // A second cancel on the now-terminal task fails.
        let result = store.begin_cancel("t1").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::TASK_NOT_CANCELABLE);
    }

    #[tokio::test]
    async fn test_begin_cancel_terminal_rejected() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task("t1", "c1", TaskState::Completed))
            .await
            .unwrap();
        let result = store.begin_cancel("t1").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::TASK_NOT_CANCELABLE);
    }

    #[tokio::test]
    async fn test_begin_cancel_not_found() {
        let store = InMemoryTaskStore::new();
        let result = store.begin_cancel("nonexistent").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::TASK_NOT_FOUND);
    }

    #[tokio::test]
    async fn test_terminal_task_limit_purges_the_oldest_finished_task() {
        let store = InMemoryTaskStore::new().with_terminal_task_limit(2);
        store
            .create(make_task("working", "c1", TaskState::Working))
            .await
            .unwrap();
        for id in ["t1", "t2", "t3"] {
            store
                .create(make_task(id, "c1", TaskState::Working))
                .await
                .unwrap();
            store
                .update(make_task(id, "c1", TaskState::Completed))
                .await
                .unwrap();
        }
        // Re-applying a terminal state does not count the task twice.
        store
            .update(make_task("t3", "c1", TaskState::Completed))
            .await
            .unwrap();

        assert!(store.get("t1").await.unwrap().is_none());
        assert!(store.get("t2").await.unwrap().is_some());
        assert!(store.get("t3").await.unwrap().is_some());
        assert!(store.get("working").await.unwrap().is_some());

        store.begin_cancel("working").await.unwrap();
        assert!(store.get("t2").await.unwrap().is_none());
        assert!(store.get("working").await.unwrap().is_some());

        store
            .create(make_task("t4", "c1", TaskState::Failed))
            .await
            .unwrap();
        assert!(store.get("t3").await.unwrap().is_none());
        assert_eq!(store.tasks.read().await.entries.len(), 2);
    }

    #[tokio::test]
    async fn test_unlimited_store_keeps_every_finished_task() {
        let store = InMemoryTaskStore::new();
        for i in 0..100 {
            let id = format!("t{i}");
            store
                .create(make_task(&id, "c1", TaskState::Completed))
                .await
                .unwrap();
        }
        assert_eq!(store.tasks.read().await.entries.len(), 100);
        assert!(store.tasks.read().await.terminal.is_empty());
    }

    #[tokio::test]
    async fn test_get_not_found() {
        let store = InMemoryTaskStore::new();
        let got = store.get("nonexistent").await.unwrap();
        assert!(got.is_none());
    }

    #[tokio::test]
    async fn test_list_filter_by_context() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task("t1", "c1", TaskState::Submitted))
            .await
            .unwrap();
        store
            .create(make_task("t2", "c2", TaskState::Working))
            .await
            .unwrap();
        store
            .create(make_task("t3", "c1", TaskState::Completed))
            .await
            .unwrap();

        let req = ListTasksRequest {
            timeline_length: None,
            context_id: Some("c1".to_string()),
            status: None,
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks.len(), 2);
        assert_eq!(resp.total_size, 2);
    }

    #[tokio::test]
    async fn test_list_filter_by_status() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task("t1", "c1", TaskState::Submitted))
            .await
            .unwrap();
        store
            .create(make_task("t2", "c1", TaskState::Working))
            .await
            .unwrap();

        let req = ListTasksRequest {
            timeline_length: None,
            context_id: None,
            status: Some(TaskState::Working),
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks.len(), 1);
        assert_eq!(resp.tasks[0].id, "t2");
    }

    #[tokio::test]
    async fn test_list_filter_by_status_timestamp_after() {
        let store = InMemoryTaskStore::new();
        let cutoff = chrono::DateTime::parse_from_rfc3339("2025-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&chrono::Utc);

        let mut old = make_task("old", "c1", TaskState::Completed);
        old.status.timestamp = Some(cutoff - chrono::Duration::seconds(1));
        store.create(old).await.unwrap();

        let mut recent = make_task("recent", "c1", TaskState::Completed);
        recent.status.timestamp = Some(cutoff + chrono::Duration::seconds(1));
        store.create(recent).await.unwrap();

        // The bound itself is included: "greater than or equal".
        let mut at_cutoff = make_task("at-cutoff", "c1", TaskState::Completed);
        at_cutoff.status.timestamp = Some(cutoff);
        store.create(at_cutoff).await.unwrap();

        // Missing timestamps do not satisfy an "after" filter.
        store
            .create(make_task("missing", "c1", TaskState::Completed))
            .await
            .unwrap();

        let resp = store
            .list(&ListTasksRequest {
                timeline_length: None,
                context_id: None,
                status: None,
                page_size: None,
                page_token: None,
                history_length: None,
                status_timestamp_after: Some(cutoff),
                include_artifacts: None,
                tenant: None,
            })
            .await
            .unwrap();

        assert_eq!(resp.total_size, 2);
        let mut ids = resp
            .tasks
            .iter()
            .map(|task| task.id.as_str())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, ["at-cutoff", "recent"]);
    }

    #[tokio::test]
    async fn test_list_pagination() {
        let store = InMemoryTaskStore::new();
        for i in 0..5 {
            store
                .create(make_task(&format!("t{i}"), "c1", TaskState::Submitted))
                .await
                .unwrap();
        }

        let req = ListTasksRequest {
            timeline_length: None,
            context_id: None,
            status: None,
            page_size: Some(2),
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks.len(), 2);
        assert_eq!(resp.total_size, 5);
        assert!(!resp.next_page_token.is_empty());

        // Get second page
        let req2 = ListTasksRequest {
            page_token: Some(resp.next_page_token),
            page_size: Some(2),
            ..req
        };
        let resp2 = store.list(&req2).await.unwrap();
        assert_eq!(resp2.tasks.len(), 2);
    }

    #[tokio::test]
    async fn test_list_zero_page_size_uses_default_window() {
        let store = InMemoryTaskStore::new();
        for i in 0..3 {
            store
                .create(make_task(&format!("t{i}"), "c1", TaskState::Submitted))
                .await
                .unwrap();
        }

        let req = ListTasksRequest {
            timeline_length: None,
            context_id: None,
            status: None,
            page_size: Some(0),
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks.len(), 3);
        assert_eq!(resp.page_size, 50);
    }

    #[tokio::test]
    async fn test_list_history_truncation() {
        let store = InMemoryTaskStore::new();
        let mut task = make_task("t1", "c1", TaskState::Working);
        task.history = Some(vec![
            Message::new(Role::User, vec![Part::text("1")]),
            Message::new(Role::Agent, vec![Part::text("2")]),
            Message::new(Role::User, vec![Part::text("3")]),
        ]);
        store.create(task).await.unwrap();

        let req = ListTasksRequest {
            timeline_length: None,
            context_id: None,
            status: None,
            page_size: None,
            page_token: None,
            history_length: Some(1),
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks[0].history.as_ref().unwrap().len(), 1);

        let empty = store
            .list(&ListTasksRequest {
                history_length: Some(-1),
                ..req
            })
            .await
            .unwrap();
        assert!(empty.tasks[0].history.as_ref().unwrap().is_empty());
    }

    fn make_task_with_history(id: &str, messages: Vec<&str>) -> Task {
        let mut task = make_task(id, "c1", TaskState::Working);
        task.history = Some(
            messages
                .into_iter()
                .map(|m| Message::new(Role::User, vec![Part::text(m)]))
                .collect(),
        );
        task
    }

    #[tokio::test]
    async fn test_list_negative_history_length_returns_empty_history() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task_with_history("t1", vec!["1", "2", "3"]))
            .await
            .unwrap();

        let req = ListTasksRequest {
            timeline_length: None,
            context_id: None,
            status: None,
            page_size: None,
            page_token: None,
            history_length: Some(-1),
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks.len(), 1);
        assert_eq!(resp.tasks[0].history.as_ref().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn test_list_page_size_is_capped() {
        let store = InMemoryTaskStore::new();
        for i in 0..150 {
            store
                .create(make_task(&format!("t{i:03}"), "c1", TaskState::Submitted))
                .await
                .unwrap();
        }

        let req = ListTasksRequest {
            timeline_length: None,
            context_id: None,
            status: None,
            page_size: Some(1000),
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();
        assert_eq!(resp.tasks.len(), 100);
        assert_eq!(resp.page_size, 100);
        assert_eq!(resp.total_size, 150);
    }

    #[tokio::test]
    async fn test_list_invalid_page_token_errors() {
        let store = InMemoryTaskStore::new();
        store
            .create(make_task("t1", "c1", TaskState::Submitted))
            .await
            .unwrap();

        let req = ListTasksRequest {
            timeline_length: None,
            context_id: None,
            status: None,
            page_size: None,
            page_token: Some("not-a-number".into()),
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let result = store.list(&req).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, error_code::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn test_list_out_of_range_page_token_returns_empty_page() {
        let store = InMemoryTaskStore::new();
        for i in 0..3 {
            store
                .create(make_task(&format!("t{i}"), "c1", TaskState::Submitted))
                .await
                .unwrap();
        }

        let req = ListTasksRequest {
            timeline_length: None,
            context_id: None,
            status: None,
            page_size: None,
            page_token: Some(URL_SAFE_NO_PAD.encode(
                serde_json::to_vec(&(None::<chrono::DateTime<chrono::Utc>>, "zzzz")).unwrap(),
            )),
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = store.list(&req).await.unwrap();

        assert!(resp.tasks.is_empty());
        assert!(
            resp.next_page_token.is_empty(),
            "must not hand back another token"
        );
        assert_eq!(resp.total_size, 3);
    }
}
