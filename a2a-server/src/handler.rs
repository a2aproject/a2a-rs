// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Request handling with private modules for execution, storage updates and responses.
//! All DefaultRequestHandler methods stay in this file.

use self::artifacts::{apply_client_artifacts, record_client_input};
use self::events::{ActiveExecution, ExecutionEvent};
use self::execution::{ExecutionRuntime, drive_execution_guarded, publish_error};
use self::protocol::{
    apply_lengths, is_artifact_only, is_terminal_event, request_task_id,
    requested_protocol_version, should_interrupt_non_streaming,
};
use self::push_delivery::send_push_notifications;
use self::registry::{ExecutionManager, ReleaseOnDrop, ReservationGuard};
use self::subscription::{subscription_stream, with_opening_task};
use self::updates::{
    CancelEntry, apply_and_publish, apply_event_to_task, backfill_status_timestamp, save_task,
    validate_event_identity,
};
use crate::middleware::ServiceParams;
use crate::pagination::resolve_page_size;
use a2a::*;
use async_trait::async_trait;
use futures::{
    StreamExt,
    stream::{self, BoxStream},
};
use std::{sync::Arc, time::Duration};
use tokio::sync::{Notify, broadcast};

mod artifacts;
mod events;
mod execution;
mod protocol;
mod push_delivery;
mod registry;
mod subscription;
mod updates;

#[cfg(test)]
mod test_support;

const EXECUTION_BUFFER_CAPACITY: usize = 32;

/// What a subscription attaches to: the events to come, the task as of the
/// last event already published, and that event's sequence.
type Resubscription = (broadcast::Receiver<ExecutionEvent>, Option<Task>, u64);

/// How long a long-poll `GetTask` (`currentGeneration`) is held open when the
/// task does not advance. The spec lets a server return earlier with the
/// current state.
const DEFAULT_LONG_POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// Upper bound on the wait between two checks of the store during a
/// long-poll, so a task changed by another process is still noticed.
const LONG_POLL_RECHECK: Duration = Duration::from_secs(1);

/// Transport-agnostic request handler interface.
///
/// This is the core interface consumed by all protocol bindings
/// (JSON-RPC, REST, gRPC). The [`DefaultRequestHandler`] provides a
/// standard implementation that orchestrates task lifecycle, storage,
/// and executor dispatch.
#[async_trait]
pub trait RequestHandler: Send + Sync + 'static {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError>;

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError>;

    async fn get_task(&self, params: &ServiceParams, req: GetTaskRequest)
    -> Result<Task, A2AError>;

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError>;

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError>;

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError>;

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError>;

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError>;

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError>;

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError>;

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError>;
}

/// Decides whether a request may proceed.
///
/// The handler never treats a non-empty header as a principal. An
/// implementation must validate whatever credential it accepts (token, DID,
/// session) and return [`A2AError`] when that check fails. `task_id` is set
/// when the method addresses one task and is reserved for owner checks.
pub trait RequestAuthorizer: Send + Sync {
    fn authorize(&self, params: &ServiceParams, task_id: Option<&str>) -> Result<(), A2AError>;
}

impl<F> RequestAuthorizer for F
where
    F: Fn(&ServiceParams, Option<&str>) -> Result<(), A2AError> + Send + Sync,
{
    fn authorize(&self, params: &ServiceParams, task_id: Option<&str>) -> Result<(), A2AError> {
        self(params, task_id)
    }
}

/// Default implementation of [`RequestHandler`] that orchestrates
/// task lifecycle management, storage, and executor dispatch.
pub struct DefaultRequestHandler {
    executor: Arc<dyn crate::AgentExecutor>,
    task_store: Arc<dyn crate::TaskStore>,
    execution_manager: Arc<ExecutionManager>,
    push_config_store: Option<Arc<dyn crate::PushConfigStore>>,
    push_sender: Option<Arc<crate::HttpPushSender>>,
    capabilities: AgentCapabilities,
    default_input_modes: Vec<String>,
    authorizer: Option<Arc<dyn RequestAuthorizer>>,
    extended_agent_card: Option<Arc<dyn ExtendedAgentCardResolver>>,
    task_changed: Arc<Notify>,
    long_poll_timeout: Duration,
}

/// Supplies the authenticated extended Agent Card (§12.4, §10.1).
///
/// A resolver rather than a stored card because §12.4 has the extended card
/// fetched *under a security scheme*: which skills an agent reveals can
/// legitimately differ by caller, and `ServiceParams` is where the caller's
/// credentials arrive. An agent serving one card to everyone can ignore the
/// argument — [`DefaultRequestHandler::with_extended_agent_card`] wraps a
/// plain card for exactly that case.
pub trait ExtendedAgentCardResolver: Send + Sync {
    fn resolve(&self, params: &ServiceParams) -> Result<AgentCard, A2AError>;
}

impl<F> ExtendedAgentCardResolver for F
where
    F: Fn(&ServiceParams) -> Result<AgentCard, A2AError> + Send + Sync,
{
    fn resolve(&self, params: &ServiceParams) -> Result<AgentCard, A2AError> {
        self(params)
    }
}

impl DefaultRequestHandler {
    pub fn new(executor: impl crate::AgentExecutor, task_store: impl crate::TaskStore) -> Self {
        DefaultRequestHandler {
            executor: Arc::new(executor),
            task_store: Arc::new(task_store),
            execution_manager: Arc::new(ExecutionManager::default()),
            push_config_store: None,
            push_sender: None,
            capabilities: AgentCapabilities::default(),
            default_input_modes: Vec::new(),
            authorizer: None,
            extended_agent_card: None,
            task_changed: Arc::new(Notify::new()),
            long_poll_timeout: DEFAULT_LONG_POLL_TIMEOUT,
        }
    }

    /// Bounds how long a `GetTask` carrying `currentGeneration` is held open
    /// when the task does not advance. The request then returns the current
    /// state, which the spec allows.
    pub fn with_long_poll_timeout(mut self, timeout: Duration) -> Self {
        self.long_poll_timeout = timeout;
        self
    }

    /// Serve `card` as the extended Agent Card, identically for every caller.
    ///
    /// Also declares the `extendedAgentCard` capability, since serving one
    /// and advertising it should not be two separate things to remember —
    /// an explicit `with_capabilities(..)` still overrides it either way.
    pub fn with_extended_agent_card(mut self, card: AgentCard) -> Self {
        self.capabilities.extended_agent_card.get_or_insert(true);
        self.with_extended_agent_card_resolver(move |_: &ServiceParams| Ok(card.clone()))
    }

    /// Serve an extended Agent Card computed per caller, so an agent can
    /// reveal different skills to different principals (§12.4).
    pub fn with_extended_agent_card_resolver(
        mut self,
        resolver: impl ExtendedAgentCardResolver + 'static,
    ) -> Self {
        self.capabilities.extended_agent_card.get_or_insert(true);
        self.extended_agent_card = Some(Arc::new(resolver));
        self
    }

    /// Install an authorizer. The default handler does not enforce one.
    pub fn with_authorizer(mut self, authorizer: impl RequestAuthorizer + 'static) -> Self {
        self.authorizer = Some(Arc::new(authorizer));
        self
    }

    fn authorize(&self, params: &ServiceParams, task_id: Option<&str>) -> Result<(), A2AError> {
        if let Some(authorizer) = &self.authorizer {
            authorizer.authorize(params, task_id)?;
        }
        // In-process callers may omit transport metadata, but an announced
        // version must pass the same check as a transport-bound request.
        if params
            .keys()
            .any(|name| name.eq_ignore_ascii_case(SVC_PARAM_VERSION))
        {
            crate::middleware::check_a2a_version(params)?;
        }
        // Required extensions apply to every operation and every live input.
        for extension in self.capabilities.extensions.iter().flatten() {
            if extension.required == Some(true)
                && !params
                    .iter()
                    .filter(|(name, _)| name.eq_ignore_ascii_case(SVC_PARAM_EXTENSIONS))
                    .flat_map(|(_, values)| values)
                    .flat_map(|value| value.split(','))
                    .any(|uri| uri.trim() == extension.uri)
            {
                return Err(A2AError::new(
                    error_code::EXTENSION_SUPPORT_REQUIRED,
                    format!("required extension: {}", extension.uri),
                ));
            }
        }
        Ok(())
    }

    pub fn with_push_config_store(
        mut self,
        push_config_store: impl crate::PushConfigStore,
    ) -> Self {
        self.push_sender = Some(Arc::new(crate::HttpPushSender::new(None)));
        self.push_config_store = Some(Arc::new(push_config_store));
        // Only a default: an explicit `Some(false)` from `with_capabilities`
        // stands, so the result no longer depends on which builder method was
        // called last.
        self.capabilities.push_notifications.get_or_insert(true);
        self
    }

    pub fn with_push_notifications(
        mut self,
        push_config_store: impl crate::PushConfigStore,
        push_sender: crate::HttpPushSender,
    ) -> Self {
        self.push_config_store = Some(Arc::new(push_config_store));
        self.push_sender = Some(Arc::new(push_sender));
        self.capabilities.push_notifications.get_or_insert(true);
        self
    }

    pub fn with_capabilities(mut self, capabilities: AgentCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// Declares the media types incoming message parts may use (§5.1's
    /// `defaultInputModes`). Empty (the default) means unrestricted, so
    /// embedders that never call this see no change in behavior.
    pub fn with_default_input_modes(mut self, modes: impl Into<Vec<String>>) -> Self {
        self.default_input_modes = modes.into();
        self
    }

    /// A part with a declared, non-empty media type that isn't among the
    /// advertised `defaultInputModes` is rejected; a part that declares none
    /// is unrestricted, and so is every part when no modes were declared.
    fn validate_content_types(&self, message: &Message) -> Result<(), A2AError> {
        if self.default_input_modes.is_empty() {
            return Ok(());
        }
        let supported = message.parts.iter().all(|part| {
            part.media_type.as_deref().is_none_or(|media_type| {
                media_type.is_empty() || self.default_input_modes.iter().any(|m| m == media_type)
            })
        });
        if supported {
            Ok(())
        } else {
            Err(A2AError::content_type_not_supported())
        }
    }

    /// §13.3: refuse a capability-gated operation the card does not declare.
    ///
    /// `None` means "not restricted", not "refused". `AgentCapabilities::
    /// default()` leaves every field `None` and most embedders never call
    /// [`Self::with_capabilities`], so treating `None` as a refusal would
    /// break all of them at once. Only an explicit `Some(false)` refuses.
    fn require_capability(
        declared: Option<bool>,
        unsupported: impl FnOnce() -> A2AError,
    ) -> Result<(), A2AError> {
        match declared {
            Some(false) => Err(unsupported()),
            _ => Ok(()),
        }
    }

    /// `artifactUpdates` is opt-in: unlike the other capabilities, an
    /// undeclared `userArtifacts` means the agent does not accept them.
    fn validate_artifact_updates(&self, req: &SendMessageRequest) -> Result<(), A2AError> {
        if req.artifact_updates.is_empty() || self.capabilities.user_artifacts == Some(true) {
            return Ok(());
        }
        Err(A2AError::unsupported_operation(
            "the agent card does not declare userArtifacts",
        ))
    }

    /// Long-poll for `GetTaskRequest.currentGeneration` (A2A 1.1 §3.2.7):
    /// returns once the task's generation exceeds `current`, or with the
    /// current state when the timeout elapses or the task is terminal.
    async fn wait_for_generation(&self, task_id: &str, current: i64) -> Result<Task, A2AError> {
        let deadline = tokio::time::Instant::now() + self.long_poll_timeout;
        loop {
            // Registered before the store is read, so a change landing in
            // between still wakes this wait.
            let changed = self.task_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();

            let task = self.load_task(task_id).await?;
            if task.generation > current || task.status.state.is_terminal() {
                return Ok(task);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(task);
            }
            tokio::select! {
                _ = &mut changed => {}
                _ = tokio::time::sleep((deadline - now).min(LONG_POLL_RECHECK)) => {}
            }
        }
    }

    fn require_streaming(&self) -> Result<(), A2AError> {
        Self::require_capability(self.capabilities.streaming, || {
            A2AError::unsupported_operation("the agent card does not declare streaming")
        })
    }

    fn require_push_notifications(&self) -> Result<(), A2AError> {
        Self::require_capability(
            self.capabilities.push_notifications,
            A2AError::push_notification_not_supported,
        )
    }

    /// The push-config store, refusing first when the card declares that the
    /// agent does not support push notifications (§13.3). Distinct from
    /// [`Self::push_config_store`], which reports the *deployment* state — a
    /// store that was never supplied — with the same error code.
    fn push_config_store_checked(&self) -> Result<&dyn crate::PushConfigStore, A2AError> {
        self.require_push_notifications()?;
        self.push_config_store()
    }

    fn push_config_store(&self) -> Result<&dyn crate::PushConfigStore, A2AError> {
        self.push_config_store
            .as_deref()
            .ok_or_else(A2AError::push_notification_not_supported)
    }

    async fn load_task(&self, task_id: &str) -> Result<Task, A2AError> {
        self.task_store
            .get(task_id)
            .await?
            .ok_or_else(|| A2AError::task_not_found(task_id))
    }

    /// Applies the request to the task and persists the result, returning the
    /// task, whether it existed before, and its context id.
    ///
    /// The caller holds the task's execution reservation and its writer lock,
    /// so nothing else -- another request, a cancel -- can change it between
    /// the read, the `ifGenerationMatch` comparison and the save below.
    async fn prepare_task_for_execution(
        &self,
        req: &SendMessageRequest,
        task_id: &str,
    ) -> Result<(Task, bool, String), A2AError> {
        let artifact_only = is_artifact_only(req);
        // A2A §3.4.2: a client-provided taskId MUST reference an existing task,
        // and client-provided ids for *creating* tasks are not supported. Only
        // an absent taskId may mint a new one.
        let stored = self.task_store.get(task_id).await?;
        if stored.is_none() && (req.message.task_id.is_some() || artifact_only) {
            return Err(A2AError::task_not_found(task_id));
        }
        if stored
            .as_ref()
            .is_some_and(|task| task.status.state.is_terminal())
        {
            return Err(A2AError::unsupported_operation(format!(
                "task {task_id} has already reached a terminal state"
            )));
        }

        // A2A 1.1 §3.2.2: `ifGenerationMatch` is an optimistic concurrency
        // guard for a follow-up message. It is ignored when the message
        // creates the task, so there is nothing to compare then.
        if let (Some(task), Some(expected)) = (
            stored.as_ref(),
            req.configuration
                .as_ref()
                .and_then(|configuration| configuration.if_generation_match),
        ) {
            if task.generation != expected {
                return Err(A2AError::task_generation_mismatch(
                    &task.id,
                    expected,
                    task.generation,
                ));
            }
        }

        // A2A §3.4.3: reject a mismatching contextId/taskId pair, and infer
        // contextId from the task when only taskId is given.
        let context_id = match stored.as_ref() {
            Some(task) => {
                if let Some(requested) = req.message.context_id.as_deref() {
                    if requested != task.context_id {
                        return Err(A2AError::invalid_params(format!(
                            "contextId {requested} does not match task {task_id}, \
                             which belongs to context {}",
                            task.context_id
                        )));
                    }
                }
                task.context_id.clone()
            }
            None => req
                .message
                .context_id
                .clone()
                .unwrap_or_else(new_context_id),
        };

        let existed = stored.is_some();
        let task = if let Some(mut existing) = stored {
            // A continuation turn's own incoming message was never otherwise
            // recorded anywhere: the executor only sees it via `ctx.message`
            // for deciding this turn's behavior, and `apply_event_to_task`
            // only ever merges status/artifact deltas onto the snapshot
            // started here, never anything content-derived. Append it so
            // `history` actually accumulates across turns.
            if !artifact_only {
                let asked = existing.status.message.clone();
                let history = existing.history.get_or_insert_with(Vec::new);
                if let Some(asked) = asked.filter(|asked| {
                    history
                        .iter()
                        .all(|seen| seen.message_id != asked.message_id)
                }) {
                    history.push(asked);
                }
                history.push(req.message.clone());
            }
            record_client_input(&mut existing, req)?;
            // Persisted now rather than with the executor's first event, so
            // the message and its generation are visible at once and survive
            // an executor that answers with a direct message or nothing.
            self.task_store.update(existing.clone()).await?;
            existing
        } else {
            let mut task = Task {
                generation: 1,
                timeline: Some(vec![TimelineEntry {
                    content: TimelineContent::Message(req.message.clone()),
                    generation: 1,
                }]),
                id: task_id.to_string(),
                context_id: context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Submitted,
                    message: None,
                    timestamp: Some(chrono::Utc::now()),
                },
                artifacts: None,
                history: Some(vec![req.message.clone()]),
                metadata: None,
            };
            apply_client_artifacts(&mut task, req, true)?;
            self.task_store.create(task.clone()).await?;
            task
        };

        Ok((task, existed, context_id))
    }

    async fn save_request_push_config(
        &self,
        params: &ServiceParams,
        task_id: &str,
        req: &SendMessageRequest,
    ) -> Result<(), A2AError> {
        let Some(config) = req
            .configuration
            .as_ref()
            .and_then(|configuration| configuration.task_push_notification_config.clone())
        else {
            return Ok(());
        };

        let mut config = config;
        config.task_id = task_id.to_string();
        if config.tenant.is_none() {
            config.tenant = req.tenant.clone();
        }
        config
            .protocol_version
            .get_or_insert_with(|| requested_protocol_version(params));
        self.push_config_store_checked()?.save(config).await?;
        Ok(())
    }

    /// Applies the executor's cancel events, recording the cancellation itself
    /// when they do not. `begin_cancel` has already moved the task to
    /// `CANCELED` without an event of its own, so the transition counts as
    /// recorded only once a `CANCELED` status entry follows it -- including
    /// when the executor fails, so the state change is never left without a
    /// generation of its own.
    async fn run_cancel(
        &self,
        task_id: &str,
        canceled: &Task,
        recorded_by_store: bool,
        active: Option<&Arc<ActiveExecution>>,
        exec_ctx: crate::ExecutorContext,
    ) -> Result<(), A2AError> {
        let mut stream = self.executor.cancel(exec_ctx);
        let mut current_task = Some(canceled.clone());
        let mut failure = None;
        let mut entry = CancelEntry {
            recorded_by_store,
            generation: canceled.generation,
            announced: false,
        };

        // Whatever goes wrong below, the cancellation is still recorded
        // afterwards: `begin_cancel` has already changed the task.
        while let Some(event) = stream.next().await {
            let mut event = match event {
                Ok(event) => event,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            };
            backfill_status_timestamp(&mut event);
            let published = match self
                .apply_cancel_event(active, &mut current_task, &mut entry, &mut event)
                .await
            {
                Ok(published) => published,
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            };
            if !published {
                continue;
            }
            if let Err(error) = send_push_notifications(
                task_id,
                self.push_config_store.as_deref(),
                self.push_sender.as_deref(),
                &event,
            )
            .await
            {
                failure = Some(error);
                break;
            }
            if is_terminal_event(&event) {
                break;
            }
        }

        let recorded = if entry.recorded_by_store {
            self.announce_cancel_if_missing(task_id, active, &entry, &current_task)
                .await
        } else {
            self.record_cancel_if_missing(task_id, canceled, active, &mut current_task)
                .await
        };
        // The recorded transition is announced like any other, even when the
        // cancel itself is about to be reported as failed.
        let pushed = match &recorded {
            Ok(Some(event)) => {
                send_push_notifications(
                    task_id,
                    self.push_config_store.as_deref(),
                    self.push_sender.as_deref(),
                    event,
                )
                .await
            }
            _ => Ok(()),
        };
        if let Some(error) = failure {
            if let Some(active) = active {
                publish_error(active, &error).await;
            }
            return Err(error);
        }
        recorded?;
        pushed
    }

    /// Applies one event of a cancel under the task's writer lock. Returns
    /// whether it was published to the subscribers.
    async fn apply_cancel_event(
        &self,
        active: Option<&Arc<ActiveExecution>>,
        current_task: &mut Option<Task>,
        entry: &mut CancelEntry,
        event: &mut StreamResponse,
    ) -> Result<bool, A2AError> {
        if let Some(task) = current_task.as_ref() {
            validate_event_identity(task, event)?;
        }
        // The task is already canceled: a cancel executor may restate that,
        // with its own message, but not move the task anywhere else -- back to
        // work, or to a different end.
        let contradicts = match &*event {
            StreamResponse::Task(task) => task.status.state != TaskState::Canceled,
            StreamResponse::StatusUpdate(update) => update.status.state != TaskState::Canceled,
            _ => false,
        };
        if contradicts {
            return Err(A2AError::invalid_agent_response());
        }
        let published = match active {
            Some(active) => {
                let _writer = active.writer.lock().await;
                let base = active.snapshot().await;
                if let Some(base) = base.as_ref().filter(|base| entry.can_amend(base, event)) {
                    // The store already gave the cancellation its generation
                    // and timeline entry; the executor's status is that
                    // entry's content, announced as its one event.
                    let amended = self.amend_cancel_entry(base.clone(), event).await?;
                    active
                        .publish_marked(Ok(&*event), Some(amended), false)
                        .await;
                    entry.announced = true;
                    true
                } else {
                    let published =
                        apply_and_publish(self.task_store.as_ref(), active, base, event).await?;
                    entry.note_published(published, event);
                    published
                }
            }
            None if current_task
                .as_ref()
                .is_some_and(|base| entry.can_amend(base, event)) =>
            {
                let base = current_task.take().expect("checked above");
                *current_task = Some(self.amend_cancel_entry(base, event).await?);
                entry.announced = true;
                true
            }
            None => {
                let generation_before = current_task.as_ref().map(|task| task.generation);
                *current_task =
                    apply_event_to_task(self.task_store.as_ref(), current_task.take(), event)
                        .await?;
                // A snapshot that changed nothing is not announced a second
                // time at the generation it found.
                let published = !matches!(
                    &*event,
                    StreamResponse::Task(task) if Some(task.generation) == generation_before
                );
                entry.note_published(published, event);
                published
            }
        };
        self.task_changed.notify_waiters();
        Ok(published)
    }

    /// Gives the executor's `CANCELED` status to the entry the store already
    /// recorded for the cancellation, without advancing the generation, and
    /// stamps the event with it. Returns the saved task.
    async fn amend_cancel_entry(
        &self,
        mut task: Task,
        event: &mut StreamResponse,
    ) -> Result<Task, A2AError> {
        let StreamResponse::StatusUpdate(update) = event else {
            return Err(A2AError::internal(
                "only a status can amend the cancel entry",
            ));
        };
        task.status = update.status.clone();
        if let Some(TimelineEntry {
            content: TimelineContent::Status(status),
            ..
        }) = task.timeline.iter_mut().flatten().last()
        {
            *status = update.status.clone();
        }
        update.generation = task.generation;
        save_task(self.task_store.as_ref(), task).await
    }

    /// Announces the cancellation the store recorded when the executor sent no
    /// status of its own: a status event at the generation the store gave it,
    /// with no further advance. Returns the event, for the push.
    async fn announce_cancel_if_missing(
        &self,
        task_id: &str,
        active: Option<&Arc<ActiveExecution>>,
        entry: &CancelEntry,
        current_task: &Option<Task>,
    ) -> Result<Option<StreamResponse>, A2AError> {
        if entry.announced {
            return Ok(None);
        }
        let _writer = match active {
            Some(active) => Some(active.writer.lock().await),
            None => None,
        };
        let base = match active {
            Some(active) => active.snapshot().await,
            None => current_task.clone(),
        };
        let Some(base) = base else {
            return Ok(None);
        };
        if base.generation != entry.generation {
            // Something else changed the task since the store recorded the
            // cancellation (an artifact the cancel executor produced, say), so
            // announcing that entry now would repeat a generation already
            // used. The cancellation is recorded again, at the current one.
            let mut status = base.status.clone();
            status.state = TaskState::Canceled;
            let mut event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: task_id.to_string(),
                context_id: base.context_id.clone(),
                status,
                metadata: None,
                generation: 0,
            });
            match active {
                Some(active) => {
                    apply_and_publish(self.task_store.as_ref(), active, Some(base), &mut event)
                        .await?;
                }
                None => {
                    apply_event_to_task(self.task_store.as_ref(), Some(base), &mut event).await?;
                }
            }
            self.task_changed.notify_waiters();
            return Ok(Some(event));
        }
        let event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.to_string(),
            context_id: base.context_id.clone(),
            status: base.status.clone(),
            metadata: None,
            generation: base.generation,
        });
        if let Some(active) = active {
            active.publish(Ok(&event), None).await;
        }
        self.task_changed.notify_waiters();
        Ok(Some(event))
    }

    /// Records the cancellation as a status event unless one has followed the
    /// store transition already. Returns the event it recorded, if any.
    async fn record_cancel_if_missing(
        &self,
        task_id: &str,
        canceled: &Task,
        active: Option<&Arc<ActiveExecution>>,
        current_task: &mut Option<Task>,
    ) -> Result<Option<StreamResponse>, A2AError> {
        let _writer = match active {
            Some(active) => Some(active.writer.lock().await),
            None => None,
        };
        let base = match active {
            Some(active) => active.snapshot().await,
            None => current_task.clone(),
        };
        let recorded = base.as_ref().is_some_and(|task| {
            task.timeline.iter().flatten().any(|entry| {
                entry.generation > canceled.generation
                    && matches!(
                        &entry.content,
                        TimelineContent::Status(status) if status.state == TaskState::Canceled
                    )
            })
        });
        if recorded {
            return Ok(None);
        }
        // Whatever the executor did, this records a cancellation.
        let mut status = base
            .as_ref()
            .map_or_else(|| canceled.status.clone(), |task| task.status.clone());
        status.state = TaskState::Canceled;
        let mut event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task_id.to_string(),
            context_id: canceled.context_id.clone(),
            status,
            metadata: None,
            generation: 0,
        });
        match active {
            Some(active) => {
                apply_and_publish(self.task_store.as_ref(), active, base, &mut event).await?;
            }
            None => {
                *current_task =
                    apply_event_to_task(self.task_store.as_ref(), base, &mut event).await?;
            }
        }
        self.task_changed.notify_waiters();
        Ok(Some(event))
    }

    async fn start_execution(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
        include_task_snapshot_in_context: bool,
    ) -> Result<(Task, broadcast::Receiver<ExecutionEvent>), A2AError> {
        let artifact_only = is_artifact_only(&req);
        if !artifact_only {
            if req.message.message_id.is_empty()
                || req.message.parts.is_empty()
                || req.message.role != Role::User
            {
                return Err(A2AError::invalid_params(
                    "a message sent with artifactUpdates must have a messageId",
                ));
            }
            self.validate_content_types(&req.message)?;
        }
        self.validate_artifact_updates(&req)?;
        let task_id = match request_task_id(&req) {
            Some(task_id) => task_id,
            None if artifact_only => {
                return Err(A2AError::invalid_params(
                    "artifactUpdates sent without a message must name the task",
                ));
            }
            None => new_task_id(),
        };

        // Claim the task before reading it: from here until the execution
        // ends, this request and a cancel are the only writers. The guard gives the claim back
        // on every early return and if this future is dropped.
        let (active, writer_guard) = self.execution_manager.reserve(&task_id).await?;
        let reservation = ReservationGuard::new(
            Arc::clone(&self.execution_manager),
            task_id.clone(),
            Arc::clone(&active),
        );
        let (task, existed, context_id) = {
            // The writer, held since the reservation until the snapshot below
            // is set, makes a cancel or a subscriber that finds this execution
            // wait for it instead of racing it.
            let prepared = async {
                let prepared = self.prepare_task_for_execution(&req, &task_id).await?;
                self.save_request_push_config(params, &task_id, &req)
                    .await?;
                Ok::<_, A2AError>(prepared)
            }
            .await;
            match prepared {
                Ok(prepared) => {
                    active.set_snapshot(prepared.0.clone()).await;
                    let waiters = self
                        .execution_manager
                        .waiting()
                        .remove(&task_id)
                        .unwrap_or_default();
                    for waiter in waiters {
                        let _ = waiter.send(active.resubscribe().await);
                    }
                    prepared
                }
                Err(error) => {
                    // Released while the writer is still held, so whoever was
                    // waiting for it finds the task free, not half-reserved.
                    reservation.release().await;
                    return Err(error);
                }
            }
        };
        self.task_changed.notify_waiters();
        // Subscribed before the writer is let go: nothing published after that
        // can be missed, a cancel that lands straight away included.
        let receiver = active.subscribe();
        drop(writer_guard);

        let exec_ctx = crate::ExecutorContext {
            message: (!artifact_only).then_some(req.message),
            task_id: task_id.clone(),
            // The task as it stands once the request is applied. A task this
            // request created is withheld unless the caller asked for it
            // (`send_message` does, streaming does not): executors use its
            // absence to recognise a new task.
            stored_task: (existed || include_task_snapshot_in_context).then(|| task.clone()),
            context_id,
            metadata: req.metadata,
            user: None,
            service_params: params.clone(),
            tenant: req.tenant,
        };

        let runtime = ExecutionRuntime {
            executor: Arc::clone(&self.executor),
            task_store: Arc::clone(&self.task_store),
            push_config_store: self.push_config_store.clone(),
            push_sender: self.push_sender.clone(),
            execution_manager: Arc::clone(&self.execution_manager),
            task_changed: Arc::clone(&self.task_changed),
        };
        let active_execution = active.clone();
        let execution_task_id = task_id.clone();

        // From here the spawned execution owns the reservation. It keeps its
        // own guard so the task is released even if the executor panics.
        reservation.disarm();
        let release = ReservationGuard::new(
            Arc::clone(&self.execution_manager),
            task_id.clone(),
            Arc::clone(&active),
        );
        tokio::spawn(async move {
            drive_execution_guarded(runtime, execution_task_id, active_execution, exec_ctx).await;
            // Reached only if the executor did not panic: the execution has
            // already released the task itself.
            release.disarm();
        });

        Ok((task, receiver))
    }
}

#[async_trait]
impl RequestHandler for DefaultRequestHandler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        self.authorize(params, request_task_id(&req).as_deref())?;
        // ACTS CORE-HIST-003: configuration.historyLength governs this
        // response's own inline task the same way it governs a later
        // GetTask, so it has to be applied here too, not just there.
        let history_length = req
            .configuration
            .as_ref()
            .and_then(|configuration| configuration.history_length);
        let timeline_length = req
            .configuration
            .as_ref()
            .and_then(|configuration| configuration.timeline_length);
        let (task, receiver) = self.start_execution(params, req.clone(), true).await?;
        let task_id = task.id;
        // Unlike a client-facing stream this one keeps the executor's no-op
        // snapshots: the blocking send only needs to know the executor spoke.
        let mut stream = subscription_stream(receiver, None, 0, true);
        let mut last_event = None;

        while let Some(item) = stream.next().await {
            let event = item?;

            if let Some(interrupt_task_id) = should_interrupt_non_streaming(&req, &event) {
                let mut task = self.load_task(&interrupt_task_id).await?;
                apply_lengths(&mut task, history_length, timeline_length);
                return Ok(SendMessageResponse::Task(task));
            }

            match event {
                StreamResponse::Message(message) => {
                    return Ok(SendMessageResponse::Message(message));
                }
                other => last_event = Some(other),
            }
        }

        match last_event {
            Some(StreamResponse::Task(mut task)) => {
                apply_lengths(&mut task, history_length, timeline_length);
                Ok(SendMessageResponse::Task(task))
            }
            Some(StreamResponse::StatusUpdate(update)) => {
                let mut task = self.load_task(&update.task_id).await?;
                apply_lengths(&mut task, history_length, timeline_length);
                Ok(SendMessageResponse::Task(task))
            }
            Some(StreamResponse::ArtifactUpdate(update)) => {
                let mut task = self.load_task(&update.task_id).await?;
                apply_lengths(&mut task, history_length, timeline_length);
                Ok(SendMessageResponse::Task(task))
            }
            Some(StreamResponse::Message(message)) => Ok(SendMessageResponse::Message(message)),
            Some(_) | None => {
                let mut task = self.load_task(&task_id).await?;
                apply_lengths(&mut task, history_length, timeline_length);
                Ok(SendMessageResponse::Task(task))
            }
        }
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.authorize(params, request_task_id(&req).as_deref())?;
        self.require_streaming()?;
        let history_length = req.configuration.as_ref().and_then(|c| c.history_length);
        let timeline_length = req.configuration.as_ref().and_then(|c| c.timeline_length);
        let (task, receiver) = self.start_execution(params, req, false).await?;
        let stream = with_opening_task(subscription_stream(receiver, None, 0, false), task);
        Ok(Box::pin(stream.map(move |item| {
            item.map(|mut event| {
                if let StreamResponse::Task(task) = &mut event {
                    apply_lengths(task, history_length, timeline_length);
                }
                event
            })
        })))
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        self.authorize(params, Some(&req.id))?;
        let mut task = match req.current_generation {
            Some(current) => self.wait_for_generation(&req.id, current).await?,
            None => self.load_task(&req.id).await?,
        };
        apply_lengths(&mut task, req.history_length, req.timeline_length);
        Ok(task)
    }

    /// Bounds the page twice, and neither is redundant: `TaskStore` is a
    /// public trait.
    ///
    /// Clamping `page_size` before the call bounds a store that honours it
    /// -- the ordinary case, and the one that turns `pageSize=999999999`
    /// into a real query. Truncating the response bounds a store that
    /// ignores it, which cannot be detected from here.
    ///
    /// On truncation the store's `next_page_token` is left alone: paging is
    /// the store's own scheme (the in-tree stores use offsets, another may
    /// use an opaque cursor) so no correct token can be synthesised. See
    /// [`TaskStore::list`](crate::TaskStore::list).
    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        self.authorize(params, None)?;
        let page_size = resolve_page_size(req.page_size);
        let req = ListTasksRequest {
            page_size: Some(page_size as i32),
            ..req
        };
        let mut response = if let Some(authorizer) = &self.authorizer {
            // Filter before counting and paging; post-filtering leaks counts
            // and creates pages whose continuation reveals inaccessible tasks.
            self.task_store
                .list_scoped(&req, &|id| authorizer.authorize(params, Some(id)).is_ok())
                .await?
        } else {
            self.task_store.list(&req).await?
        };
        response.tasks.truncate(page_size);
        for task in &mut response.tasks {
            apply_lengths(task, req.history_length, req.timeline_length);
            if req.include_artifacts != Some(true) {
                task.artifacts = None;
            }
        }
        Ok(response)
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        self.authorize(params, Some(&req.id))?;

        // Whoever is mutating a task holds its writer lock, execution or not,
        // so a cancel takes its turn here: from the store transition until the
        // snapshot is updated, nothing else may save. The execution is looked
        // up only once the lock is held, so it cannot be one that has just
        // finished (or one a follow-up reserves a moment later).
        let writer = self.execution_manager.writer_for(&req.id);
        let transition = writer.lock_owned().await;
        let active_execution = self.execution_manager.get(&req.id).await;
        let task = self.load_task(&req.id).await?;
        let generation_before = task.generation;

        // Idempotent: re-cancelling an already-canceled task is a no-op that
        // returns the current task (matching a2a-go). Other terminal states
        // are still not cancelable, per A2A and a2a-cli SPEC.md §10.4.
        if task.status.state == TaskState::Canceled {
            return Ok(task);
        }
        if task.status.state.is_terminal() {
            return Err(A2AError::task_not_cancelable(&req.id));
        }

        // Atomically transition the task to CANCELED so two concurrent cancel
        // requests cannot both pass the check-then-act window.
        let task = match self.task_store.begin_cancel(&req.id).await {
            Ok(task) => task,
            Err(error) if error.code == error_code::TASK_NOT_CANCELABLE => {
                // Raced with another cancel or with task completion. Keep the
                // same semantics as the upfront check: a task another cancel
                // already moved to CANCELED is returned (idempotent); any
                // other terminal state is still not cancelable.
                let raced = self.load_task(&req.id).await?;
                if raced.status.state == TaskState::Canceled {
                    return Ok(raced);
                }
                return Err(error);
            }
            Err(error) => return Err(error),
        };

        // The task is canceled in the store from here on, however the rest of
        // this ends, so no subscription may stay waiting for its next turn.
        let _release = ReleaseOnDrop {
            manager: &self.execution_manager,
            task: task.clone(),
        };

        if let Some(active) = &active_execution {
            // The executor must see the cancellation before it writes again.
            active.set_snapshot(task.clone()).await;
        }
        drop(transition);
        // The task changed under every long-poll waiting on it.
        self.task_changed.notify_waiters();

        let exec_ctx = crate::ExecutorContext {
            message: None,
            task_id: req.id.clone(),
            stored_task: Some(task.clone()),
            context_id: task.context_id.clone(),
            metadata: req.metadata,
            user: None,
            service_params: params.clone(),
            tenant: req.tenant,
        };
        // A store that advanced the generation recorded the transition itself.
        let recorded_by_store = task.generation > generation_before;
        let outcome = self
            .run_cancel(
                &req.id,
                &task,
                recorded_by_store,
                active_execution.as_ref(),
                exec_ctx,
            )
            .await;

        if let Some(active) = active_execution {
            self.execution_manager.finish(&req.id, &active).await;
        }
        outcome?;

        // A subscription to a task between turns has no execution to follow
        // into the cancel; the final snapshot ends it. Subscriptions register
        // before reading the store, so one that read the task before
        // `begin_cancel` is waiting now, and one that reads it later finds it
        // terminal.
        let task = self.load_task(&req.id).await?;
        self.execution_manager.release(&task);
        Ok(task)
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.authorize(params, Some(&req.id))?;
        self.require_streaming()?;
        // An execution that is still being prepared has no snapshot to open
        // with yet. Wait for the writer -- the request preparing it holds it --
        // and look again, so the stream opens with the task, or, if the
        // request failed and released the task, falls through to the checks
        // below. The wait follows the lookup that found the execution, not one
        // before it: a registration made in between is waited for too.
        let mut waiter = self.execution_manager.subscribe(&req.id).await;
        let mut resubscribed = waiter.attached();
        if matches!(resubscribed, Some((_, None, _))) {
            resubscribed = match self.execution_manager.get(&req.id).await {
                Some(active) => {
                    let _writer = active.writer.lock().await;
                    // With the writer held a reservation that is still empty
                    // belongs to a dropped request, so the task is not
                    // executing; nothing is left to wait for.
                    match active.snapshot().await {
                        Some(_) => self.execution_manager.resubscribe(&req.id).await,
                        None => None,
                    }
                }
                None => None,
            };
        }
        if let Some((receiver, snapshot_task, sequence)) = resubscribed {
            // A client that already holds the task's current generation has
            // missed nothing, so it resumes without a snapshot. One that is
            // behind cannot be replayed to -- events are not retained -- and
            // falls back to a snapshot, which the spec allows.
            let snapshot_task = snapshot_task.filter(|task| {
                Some(task.generation) != req.last_seen_generation || task.status.state.is_terminal()
            });
            return Ok(subscription_stream(
                receiver,
                snapshot_task,
                sequence,
                false,
            ));
        }

        // A registry miss also happens once a task finishes and its
        // execution is cleaned up, so it doesn't by itself mean the task
        // never existed -- check the store to tell the two apart before
        // reporting TaskNotFoundError.
        let task = self.load_task(&req.id).await?;
        // An execution that started while the store was read may have moved
        // the task on already; the waiter has everything it published.
        if let Some((receiver, snapshot_task, sequence)) = waiter.attached() {
            let snapshot_task = snapshot_task.filter(|task| {
                Some(task.generation) != req.last_seen_generation || task.status.state.is_terminal()
            });
            return Ok(subscription_stream(
                receiver,
                snapshot_task,
                sequence,
                false,
            ));
        }
        if task.status.state.is_terminal() {
            return Err(A2AError::unsupported_operation(format!(
                "task {} has already reached a terminal state",
                req.id
            )));
        }

        // The task is between turns, in an interrupted state; §3.1.6 allows
        // subscribing to any non-terminal task. The stream opens with the
        // task as stored, then follows the execution the next message starts
        // to its end, as a resubscription would (TCK STREAM-SUB-002).
        let opening = (Some(task.generation) != req.last_seen_generation)
            .then_some(Ok(StreamResponse::Task(task)));
        Ok(stream::iter(opening).chain(waiter.into_stream()).boxed())
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.authorize(params, Some(&req.task_id))?;
        let mut req = req;
        req.protocol_version
            .get_or_insert_with(|| requested_protocol_version(params));
        let store = self.push_config_store_checked()?;
        self.load_task(&req.task_id).await?;
        if let Some(sender) = &self.push_sender {
            sender.validate_config(&req)?;
        }
        store.save(req).await
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.authorize(params, Some(&req.task_id))?;
        self.push_config_store_checked()?
            .get(&req.task_id, &req.id)
            .await
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        self.authorize(params, Some(&req.task_id))?;
        let mut configs = self.push_config_store_checked()?.list(&req.task_id).await?;
        configs.sort_by(|left, right| left.id.cmp(&right.id));

        // Paging for push configs is done here, not in the store: the store
        // is asked for every config for the task and the page is cut from
        // that. PushConfigStore is public, so this clamp is what bounds the
        // response for an implementation with no per-task limit of its own.
        let page_size = resolve_page_size(req.page_size);
        let start = if let Some(ref token) = req.page_token {
            token
                .parse::<usize>()
                .map_err(|_| A2AError::invalid_params("invalid page token"))?
        } else {
            0
        };
        let start = start.min(configs.len());
        let end = (start + page_size).min(configs.len());
        let next_page_token = (end < configs.len()).then(|| end.to_string());

        Ok(ListTaskPushNotificationConfigsResponse {
            configs: configs[start..end].to_vec(),
            next_page_token,
        })
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        self.authorize(params, Some(&req.task_id))?;
        self.push_config_store_checked()?
            .delete(&req.task_id, &req.id)
            .await
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        self.authorize(params, None)?;
        Self::require_capability(self.capabilities.extended_agent_card, || {
            A2AError::unsupported_operation(
                "the agent card does not declare an extended agent card",
            )
        })?;
        match &self.extended_agent_card {
            Some(resolver) => resolver.resolve(params),
            // A2A defines a code for exactly this condition.
            // UNSUPPORTED_OPERATION would say the agent does not do extended
            // cards at all, when the truth is that this deployment has not
            // configured one — the client can act on that difference.
            None => Err(A2AError::extended_card_not_configured()),
        }
    }
}

#[cfg(test)]
mod lag_coverage_tests {
    use super::events::ExecutionEvent;
    use super::*;
    use futures::StreamExt;
    use tokio::sync::broadcast;

    /// The reservation hands back the task's writer already held: whoever
    /// finds the execution registered then waits on a lock that is taken, not
    /// one that is still free between the registration and the first write.
    #[tokio::test]
    async fn test_reserve_holds_the_writer_from_the_moment_it_registers() {
        let manager = ExecutionManager::default();

        let (active, guard) = manager.reserve("t-reserved").await.unwrap();
        assert!(manager.get("t-reserved").await.is_some());
        assert!(
            manager.writer_for("t-reserved").try_lock().is_err(),
            "the writer is held at registration"
        );
        // It is the same lock the execution writes under.
        assert!(Arc::ptr_eq(
            &active.writer,
            &manager.writer_for("t-reserved")
        ));

        drop(guard);
        assert!(manager.writer_for("t-reserved").try_lock().is_ok());
        // Once the execution has its snapshot a second reservation is refused,
        // and does not leave the writer taken.
        active
            .set_snapshot(Task {
                generation: 1,
                timeline: None,
                id: "t-reserved".into(),
                context_id: "c-reserved".into(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            })
            .await;
        assert!(manager.reserve("t-reserved").await.is_err());
        assert!(manager.writer_for("t-reserved").try_lock().is_ok());
    }

    #[tokio::test]
    async fn test_subscription_lag_is_reported_as_a_sanitized_internal_error() {
        // A subscriber that falls behind the active execution gets a generic
        // INTERNAL_ERROR rather than the broadcast internals, and the stream
        // terminates instead of spinning on a lost buffer.
        let (sender, receiver) = broadcast::channel(1);

        for sequence in 0..4 {
            let _ = sender.send(ExecutionEvent {
                sequence,
                result: Ok(StreamResponse::Message(a2a::Message {
                    message_id: "lag-probe".to_string(),
                    role: a2a::Role::Agent,
                    parts: Vec::new(),
                    context_id: None,
                    task_id: None,
                    metadata: None,
                    extensions: None,
                    reference_task_ids: None,
                })),
                repeated: false,
            });
        }

        let mut stream = subscription_stream(receiver, None, 0, true);
        let item = stream.next().await.expect("lagged item");

        let error = item.expect_err("lag must surface as an error");
        assert_eq!(error.code, a2a::error_code::INTERNAL_ERROR);
        assert_eq!(error.message, "Internal error");

        // `done` is set on lag, so the stream ends rather than repeating.
        assert!(stream.next().await.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::test_support::*;
    /// How the cancel of an `InputRequiredExecutor` ends.
    enum Cancel {
        Cancels,
        Fails,
        /// Appends to an artifact the task does not have, which the handler
        /// cannot apply to the task.
        SendsAnInapplicableEvent,
    }

    /// Asks for input on the task's first turn and completes on the next;
    /// its cancel reports the task canceled, or ends as asked to.
    struct InputRequiredExecutor {
        cancel: Cancel,
    }

    impl crate::AgentExecutor for InputRequiredExecutor {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let state = match ctx.stored_task.as_ref().map(|task| &task.status.state) {
                Some(TaskState::InputRequired) => TaskState::Completed,
                _ => TaskState::InputRequired,
            };
            let update = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                generation: 0,

                task_id: ctx.task_id,
                context_id: ctx.context_id,
                status: TaskStatus {
                    state,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            });
            Box::pin(stream::once(async move { Ok(update) }))
        }

        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let event = match self.cancel {
                Cancel::Fails => Err(A2AError::internal("cancel failed")),
                Cancel::SendsAnInapplicableEvent => {
                    Ok(StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                        generation: 0,

                        task_id: ctx.task_id,
                        context_id: ctx.context_id,
                        artifact: Artifact {
                            end_generation: None,
                            role: None,
                            start_generation: None,

                            artifact_id: "missing".into(),
                            name: None,
                            description: None,
                            parts: vec![Part::text("tail")],
                            metadata: None,
                            extensions: None,
                        },
                        append: Some(true),
                        last_chunk: None,
                        metadata: None,
                    }))
                }
                Cancel::Cancels => Ok(StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                    generation: 0,

                    task_id: ctx.task_id,
                    context_id: ctx.context_id,
                    status: TaskStatus {
                        state: TaskState::Canceled,
                        message: None,
                        timestamp: None,
                    },
                    metadata: None,
                })),
            };
            Box::pin(stream::once(async move { event }))
        }
    }

    fn make_input_required_handler() -> DefaultRequestHandler {
        DefaultRequestHandler::new(
            InputRequiredExecutor {
                cancel: Cancel::Cancels,
            },
            InMemoryTaskStore::new(),
        )
    }

    /// A task whose first turn ended asking for input: stored, but with no
    /// execution registered any more.
    async fn task_awaiting_input(handler: &DefaultRequestHandler) -> Task {
        let response = handler
            .send_message(
                &ServiceParams::new(),
                SendMessageRequest {
                    artifact_updates: Vec::new(),

                    message: make_message(),
                    configuration: None,
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        let mut task = None;
        if let SendMessageResponse::Task(sent) = response {
            task = Some(sent);
        }
        let task = task.expect("expected a task");
        assert_eq!(task.status.state, TaskState::InputRequired);
        assert!(handler.execution_manager.get(&task.id).await.is_none());
        task
    }

    /// A subscriber that finds an execution still being prepared waits for it
    /// and then opens with the task, not with whatever comes next.
    #[tokio::test]
    async fn test_subscribe_waits_for_an_execution_that_is_being_prepared() {
        use futures::StreamExt;

        let handler = Arc::new(make_handler());
        seed_task(&handler, "t-prep", "c-prep").await;
        let (active, guard) = handler.execution_manager.reserve("t-prep").await.unwrap();

        let subscribing = {
            let handler = handler.clone();
            tokio::spawn(async move {
                handler
                    .subscribe_to_task(
                        &ServiceParams::new(),
                        SubscribeToTaskRequest {
                            last_seen_generation: None,
                            id: "t-prep".into(),
                            tenant: None,
                        },
                    )
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!subscribing.is_finished(), "it waits for the preparation");

        let prepared = handler.task_store.get("t-prep").await.unwrap().unwrap();
        active.set_snapshot(prepared).await;
        drop(guard);

        let mut stream = subscribing.await.unwrap().unwrap();
        assert!(matches!(
            stream.next().await,
            Some(Ok(StreamResponse::Task(_)))
        ));
    }

    /// A cancel executor that restates the canceled task on an idle task is
    /// not announced a second time at the generation it found.
    #[tokio::test]
    async fn test_idle_cancel_does_not_announce_a_restated_snapshot() {
        let handler = make_handler();
        let canceled = Task {
            generation: 3,
            timeline: None,
            id: "t-idle-cancel".into(),
            context_id: "c-idle-cancel".into(),
            status: TaskStatus {
                state: TaskState::Canceled,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        let mut current = Some(canceled.clone());
        let mut event = StreamResponse::Task(canceled);

        let published = handler
            .apply_cancel_event(
                None,
                &mut current,
                &mut CancelEntry {
                    recorded_by_store: false,
                    generation: 3,
                    announced: false,
                },
                &mut event,
            )
            .await
            .unwrap();

        assert!(
            !published,
            "a snapshot that changes nothing is not published"
        );
        assert_eq!(current.unwrap().generation, 3);
    }

    /// A subscriber that finds such a leftover does not wait for it or spin on
    /// it: the task is simply not executing.
    #[tokio::test]
    async fn test_subscribe_ignores_a_dropped_requests_leftover() {
        let handler = make_handler();
        seed_task(&handler, "t-leftover", "c-leftover").await;
        let (_stale, guard) = handler
            .execution_manager
            .reserve("t-leftover")
            .await
            .unwrap();
        drop(guard);

        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            handler.subscribe_to_task(
                &ServiceParams::new(),
                SubscribeToTaskRequest {
                    last_seen_generation: None,
                    id: "t-leftover".into(),
                    tenant: None,
                },
            ),
        )
        .await
        .expect("the subscription neither hangs nor spins");

        // The stale reservation does not hide the stored, nonterminal task.
        let mut events = outcome.unwrap();
        assert!(
            matches!(events.next().await, Some(Ok(StreamResponse::Task(task)))
            if task.id == "t-leftover")
        );
    }

    #[tokio::test]
    async fn test_send_message_creates_task() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = SendMessageRequest {
            artifact_updates: Vec::new(),
            message: make_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let result = handler.send_message(&params, req).await.unwrap();
        match result {
            SendMessageResponse::Task(task) => {
                assert_eq!(task.status.state, TaskState::Completed);
            }
            _ => panic!("expected Task response"),
        }
    }

    #[tokio::test]
    async fn test_send_streaming_message() {
        use futures::StreamExt;
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = SendMessageRequest {
            artifact_updates: Vec::new(),
            message: make_message(),
            configuration: None,
            metadata: None,
            tenant: None,
        };
        let mut stream = handler.send_streaming_message(&params, req).await.unwrap();
        let event = stream.next().await.unwrap().unwrap();
        assert!(matches!(event, StreamResponse::Task(_)));
    }

    #[tokio::test]
    async fn test_get_task_not_found() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = GetTaskRequest {
            current_generation: None,
            timeline_length: None,
            id: "nonexistent".into(),
            history_length: None,
            tenant: None,
        };
        let result = handler.get_task(&params, req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_send_message_rejects_unsupported_content_type() {
        install_crypto_provider();
        let handler = make_handler().with_default_input_modes(vec!["text/plain".into()]);
        let mut req = send_req(None, None);
        req.message.parts = vec![
            Part::data(serde_json::json!({"value": "unsupported"}))
                .with_media_type("application/x-unsupported-type-12345"),
        ];
        let err = handler
            .send_message(&ServiceParams::new(), req)
            .await
            .expect_err("an undeclared content type must be rejected");
        assert_eq!(err.code, error_code::CONTENT_TYPE_NOT_SUPPORTED);
    }

    #[tokio::test]
    async fn test_send_message_accepts_a_declared_content_type() {
        install_crypto_provider();
        let handler = make_handler().with_default_input_modes(vec!["text/plain".into()]);
        let mut req = send_req(None, None);
        req.message.parts = vec![Part::text("hello").with_media_type("text/plain")];
        handler
            .send_message(&ServiceParams::new(), req)
            .await
            .expect("a declared content type must be accepted");
    }

    #[tokio::test]
    async fn test_send_message_allows_any_content_type_when_none_declared() {
        install_crypto_provider();
        let handler = make_handler();
        let mut req = send_req(None, None);
        req.message.parts = vec![
            Part::data(serde_json::json!({"value": "x"})).with_media_type("application/x-anything"),
        ];
        handler
            .send_message(&ServiceParams::new(), req)
            .await
            .expect("no declared modes means no restriction");
    }

    #[tokio::test]
    async fn test_send_message_unknown_task_id_is_not_found() {
        install_crypto_provider();
        let handler = make_handler();
        // §3.4.2: a client-provided taskId MUST reference an existing task, and
        // MUST NOT create one. Silently minting a task here is what let a
        // mistyped or replayed id start a fresh conversation.
        let err = handler
            .send_message(&ServiceParams::new(), send_req(Some("t-absent"), None))
            .await
            .expect_err("an unknown taskId must not create a task");
        assert_eq!(err.code, error_code::TASK_NOT_FOUND);
        assert!(
            handler.task_store.get("t-absent").await.unwrap().is_none(),
            "the rejected id must not have been created"
        );
    }

    #[tokio::test]
    async fn test_send_message_to_terminal_task_is_rejected() {
        install_crypto_provider();
        let handler = make_handler();
        handler
            .task_store
            .create(Task {
                generation: 0,
                timeline: None,
                id: "t-done".into(),
                context_id: "c-done".into(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            })
            .await
            .unwrap();
        let err = handler
            .send_message(&ServiceParams::new(), send_req(Some("t-done"), None))
            .await
            .expect_err("a follow-up to a terminal task must not be accepted");
        assert_eq!(err.code, error_code::UNSUPPORTED_OPERATION);
    }

    #[tokio::test]
    async fn test_send_message_mismatched_context_id_is_rejected() {
        install_crypto_provider();
        let handler = make_handler();
        seed_task(&handler, "t-ctx", "c-real").await;
        // §3.4.3: previously the stored contextId silently won, so a client
        // addressing the wrong conversation was never told.
        let err = handler
            .send_message(
                &ServiceParams::new(),
                send_req(Some("t-ctx"), Some("c-wrong")),
            )
            .await
            .expect_err("a mismatching contextId must be rejected");
        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(
            err.message.contains("c-wrong"),
            "error names the bad id: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn test_send_message_matching_context_id_is_accepted() {
        install_crypto_provider();
        let handler = make_handler();
        seed_task(&handler, "t-ok", "c-ok").await;
        handler
            .send_message(&ServiceParams::new(), send_req(Some("t-ok"), Some("c-ok")))
            .await
            .expect("a matching pair must be accepted");
    }

    #[tokio::test]
    async fn test_send_message_accumulates_history_across_turns() {
        // ACTS CORE-HIST-005/006: a multi-turn conversation's history must
        // include every turn's message, not just the one that created the
        // task. MultiTurnExecutor only ever emits a StatusUpdate, so this
        // exercises prepare_task_for_execution's continuation path exactly
        // as a real executor (which never touches history itself) does.
        install_crypto_provider();
        let handler = DefaultRequestHandler::new(MultiTurnExecutor, InMemoryTaskStore::new());

        let mut turn1 = make_message();
        turn1.parts = vec![Part::text("turn one")];
        let resp1 = handler
            .send_message(
                &ServiceParams::new(),
                SendMessageRequest {
                    artifact_updates: Vec::new(),
                    message: turn1,
                    configuration: None,
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        let task_id = match resp1 {
            SendMessageResponse::Task(t) => t.id,
            SendMessageResponse::Message(_) => panic!("expected a task"),
        };

        for text in ["turn two", "done"] {
            let mut turn = make_message();
            turn.task_id = Some(task_id.clone());
            turn.parts = vec![Part::text(text)];
            handler
                .send_message(
                    &ServiceParams::new(),
                    SendMessageRequest {
                        artifact_updates: Vec::new(),
                        message: turn,
                        configuration: None,
                        metadata: None,
                        tenant: None,
                    },
                )
                .await
                .unwrap();
        }

        let task = handler.task_store.get(&task_id).await.unwrap().unwrap();
        let history = task.history.expect("history must accumulate across turns");
        let texts: Vec<_> = history.iter().map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec![Some("turn one"), Some("turn two"), Some("done")]
        );
    }

    #[tokio::test]
    async fn test_send_message_applies_configuration_history_length_to_its_own_response() {
        // ACTS CORE-HIST-003: configuration.historyLength must govern
        // SendMessage's own inline task the same way history_length governs
        // a later GetTask, rather than always including the just-sent
        // message regardless of what was asked for.
        install_crypto_provider();
        let handler = DefaultRequestHandler::new(MultiTurnExecutor, InMemoryTaskStore::new());
        let mut message = make_message();
        message.parts = vec![Part::text("done")];
        let resp = handler
            .send_message(
                &ServiceParams::new(),
                SendMessageRequest {
                    artifact_updates: Vec::new(),
                    message,
                    configuration: Some(SendMessageConfiguration {
                        if_generation_match: None,
                        timeline_length: None,
                        accepted_output_modes: None,
                        task_push_notification_config: None,
                        history_length: Some(0),
                        return_immediately: None,
                    }),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        let task = match resp {
            SendMessageResponse::Task(t) => t,
            SendMessageResponse::Message(_) => panic!("expected a task"),
        };
        assert!(
            task.history.is_none_or(|h| h.is_empty()),
            "historyLength=0 must clear history on SendMessage's own response"
        );
    }

    #[tokio::test]
    async fn test_send_message_infers_context_id_from_the_task() {
        install_crypto_provider();
        let handler = make_handler();
        seed_task(&handler, "t-infer", "c-infer").await;
        // §3.4.3: contextId MUST be inferred when only taskId is given.
        handler
            .send_message(&ServiceParams::new(), send_req(Some("t-infer"), None))
            .await
            .unwrap();
        let task = handler.task_store.get("t-infer").await.unwrap().unwrap();
        assert_eq!(task.context_id, "c-infer");
    }

    #[tokio::test]
    async fn test_send_message_without_task_id_still_creates_one() {
        install_crypto_provider();
        let handler = make_handler();
        // The server generates the id. A contextId alone is allowed and starts
        // a new task within that conversation.
        let resp = handler
            .send_message(&ServiceParams::new(), send_req(None, Some("c-new")))
            .await
            .expect("a message with no taskId must still create a task");
        let _ = resp;
        let listed = handler
            .task_store
            .list(&ListTasksRequest {
                timeline_length: None,
                context_id: Some("c-new".into()),
                status: None,
                page_size: None,
                page_token: None,
                history_length: None,
                include_artifacts: None,
                status_timestamp_after: None,
                tenant: None,
            })
            .await
            .unwrap();
        assert_eq!(listed.tasks.len(), 1);
        assert_eq!(listed.tasks[0].context_id, "c-new");
    }

    #[tokio::test]
    async fn test_get_task_after_send() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let mut msg = make_message();
        msg.task_id = Some("t1".into());
        msg.context_id = Some("c1".into());
        seed_task(&handler, "t1", "c1").await;
        let req = SendMessageRequest {
            artifact_updates: Vec::new(),
            message: msg,
            configuration: None,
            metadata: None,
            tenant: None,
        };
        handler.send_message(&params, req).await.unwrap();
        let task = handler
            .get_task(
                &params,
                GetTaskRequest {
                    current_generation: None,
                    timeline_length: None,
                    id: "t1".into(),
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(task.id, "t1");
    }

    #[tokio::test]
    async fn test_list_tasks() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = ListTasksRequest {
            timeline_length: None,
            context_id: None,
            status: None,
            page_size: None,
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        };
        let resp = handler.list_tasks(&params, req).await.unwrap();
        assert!(resp.tasks.is_empty());
    }

    /// ListTasks omits artifacts unless `includeArtifacts` is true (§3.1.4).
    #[tokio::test]
    async fn test_list_tasks_includes_artifacts_only_on_request() {
        let store = InMemoryTaskStore::new();
        store
            .create(Task {
                generation: 0,
                timeline: None,

                id: "t-artifact".into(),
                context_id: "ctx".into(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: Some(chrono::Utc::now()),
                },
                artifacts: Some(vec![Artifact {
                    end_generation: None,
                    role: None,
                    start_generation: None,

                    artifact_id: "a1".into(),
                    name: None,
                    description: None,
                    parts: vec![Part::text("result")],
                    metadata: None,
                    extensions: None,
                }]),
                history: None,
                metadata: None,
            })
            .await
            .unwrap();
        let handler = DefaultRequestHandler::new(EchoExecutor, store);
        let params = ServiceParams::new();
        for (include_artifacts, expected) in
            [(None, false), (Some(false), false), (Some(true), true)]
        {
            let resp = handler
                .list_tasks(
                    &params,
                    ListTasksRequest {
                        timeline_length: None,

                        context_id: None,
                        status: None,
                        page_size: None,
                        page_token: None,
                        history_length: None,
                        status_timestamp_after: None,
                        include_artifacts,
                        tenant: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                resp.tasks[0].artifacts.is_some(),
                expected,
                "include_artifacts={include_artifacts:?}"
            );
        }
    }

    #[tokio::test]
    async fn test_cancel_task_not_found() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = CancelTaskRequest {
            id: "nonexistent".into(),
            metadata: None,
            tenant: None,
        };
        let result = handler.cancel_task(&params, req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_cancel_task_working() {
        let handler = make_handler();
        let params = ServiceParams::new();
        // First create a task via the store directly
        let task = Task {
            generation: 0,
            timeline: None,
            id: "t2".into(),
            context_id: "c2".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        handler.task_store.create(task).await.unwrap();
        let req = CancelTaskRequest {
            id: "t2".into(),
            metadata: None,
            tenant: None,
        };
        let result = handler.cancel_task(&params, req).await.unwrap();
        assert_eq!(result.status.state, TaskState::Canceled);
    }

    #[tokio::test]
    async fn test_cancel_task_concurrent_both_succeed_idempotently() {
        let handler = Arc::new(make_handler());
        let params = ServiceParams::new();
        let task = Task {
            generation: 0,
            timeline: None,
            id: "t-race".into(),
            context_id: "c-race".into(),
            status: TaskStatus {
                state: TaskState::Working,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        handler.task_store.create(task).await.unwrap();

        let handlers = vec![Arc::clone(&handler), Arc::clone(&handler)];
        let mut results = Vec::new();
        for handler in handlers {
            let params = params.clone();
            let req = CancelTaskRequest {
                id: "t-race".into(),
                metadata: None,
                tenant: None,
            };
            results.push(tokio::spawn(async move {
                handler.cancel_task(&params, req).await
            }));
        }
        for result in results {
            let task = result.await.unwrap().unwrap();
            assert_eq!(task.status.state, TaskState::Canceled);
        }
    }

    #[tokio::test]
    async fn test_cancel_task_already_completed() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let task = Task {
            generation: 0,
            timeline: None,
            id: "t3".into(),
            context_id: "c3".into(),
            status: TaskStatus {
                state: TaskState::Completed,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        handler.task_store.create(task).await.unwrap();
        let req = CancelTaskRequest {
            id: "t3".into(),
            metadata: None,
            tenant: None,
        };
        let result = handler.cancel_task(&params, req).await;
        match result {
            Ok(_) => panic!("expected cancel of COMPLETED to fail"),
            Err(error) => assert_eq!(error.code, error_code::TASK_NOT_CANCELABLE),
        }
    }

    #[tokio::test]
    async fn test_cancel_task_already_canceled_is_idempotent() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let task = Task {
            generation: 0,
            timeline: None,
            id: "t-canceled".into(),
            context_id: "c-canceled".into(),
            status: TaskStatus {
                state: TaskState::Canceled,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        handler.task_store.create(task).await.unwrap();
        let req = CancelTaskRequest {
            id: "t-canceled".into(),
            metadata: None,
            tenant: None,
        };
        let result = handler.cancel_task(&params, req).await.unwrap();
        assert_eq!(result.status.state, TaskState::Canceled);
    }

    #[tokio::test]
    async fn test_cancel_task_failed_and_rejected_are_not_cancelable() {
        let handler = make_handler();
        let params = ServiceParams::new();
        for (id, state) in [
            ("t-failed", TaskState::Failed),
            ("t-rejected", TaskState::Rejected),
        ] {
            handler
                .task_store
                .create(Task {
                    generation: 0,
                    timeline: None,
                    id: id.into(),
                    context_id: format!("c-{id}"),
                    status: TaskStatus {
                        state,
                        message: None,
                        timestamp: None,
                    },
                    artifacts: None,
                    history: None,
                    metadata: None,
                })
                .await
                .unwrap();
            let result = handler
                .cancel_task(
                    &params,
                    CancelTaskRequest {
                        id: id.into(),
                        metadata: None,
                        tenant: None,
                    },
                )
                .await;
            match result {
                Ok(_) => panic!("expected cancel of {id} to fail"),
                Err(error) => assert_eq!(error.code, error_code::TASK_NOT_CANCELABLE),
            }
        }
    }

    #[tokio::test]
    async fn test_get_task_applies_history_length() {
        let handler = make_handler();
        let params = ServiceParams::new();
        handler
            .task_store
            .create(task_with_history("hist", "c", 3))
            .await
            .unwrap();

        let full = handler
            .get_task(
                &params,
                GetTaskRequest {
                    current_generation: None,
                    timeline_length: None,
                    id: "hist".into(),
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(full.history.as_ref().unwrap().len(), 3);

        let last = handler
            .get_task(
                &params,
                GetTaskRequest {
                    current_generation: None,
                    timeline_length: None,
                    id: "hist".into(),
                    history_length: Some(1),
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(last.history.as_ref().unwrap().len(), 1);

        for hl in [0, -1] {
            let empty = handler
                .get_task(
                    &params,
                    GetTaskRequest {
                        current_generation: None,
                        timeline_length: None,
                        id: "hist".into(),
                        history_length: Some(hl),
                        tenant: None,
                    },
                )
                .await
                .unwrap();
            assert!(
                empty.history.as_ref().unwrap().is_empty(),
                "history_length={hl} must clear history"
            );
        }
    }

    #[tokio::test]
    async fn test_authorizer_rejects_handler_methods() {
        let handler = DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
            .with_authorizer(reject_unauthenticated)
            .with_push_config_store(InMemoryPushConfigStore::new());
        let params = ServiceParams::new();

        assert!(
            handler
                .send_message(
                    &params,
                    SendMessageRequest {
                        artifact_updates: Vec::new(),
                        message: make_message(),
                        configuration: None,
                        metadata: None,
                        tenant: None,
                    },
                )
                .await
                .is_err()
        );
        assert!(
            handler
                .get_task(
                    &params,
                    GetTaskRequest {
                        current_generation: None,
                        timeline_length: None,
                        id: "t".into(),
                        history_length: None,
                        tenant: None,
                    },
                )
                .await
                .is_err()
        );
        assert!(
            handler
                .list_tasks(
                    &params,
                    ListTasksRequest {
                        timeline_length: None,
                        context_id: None,
                        status: None,
                        page_size: None,
                        page_token: None,
                        history_length: None,
                        status_timestamp_after: None,
                        include_artifacts: None,
                        tenant: None,
                    },
                )
                .await
                .is_err()
        );
        assert!(
            handler
                .cancel_task(
                    &params,
                    CancelTaskRequest {
                        id: "t".into(),
                        metadata: None,
                        tenant: None,
                    },
                )
                .await
                .is_err()
        );
        assert!(
            handler
                .subscribe_to_task(
                    &params,
                    SubscribeToTaskRequest {
                        last_seen_generation: None,
                        id: "t".into(),
                        tenant: None,
                    },
                )
                .await
                .is_err()
        );
        assert!(
            handler
                .create_push_config(
                    &params,
                    TaskPushNotificationConfig {
                        protocol_version: None,
                        url: "https://example.com/push".into(),
                        id: None,
                        task_id: "t".into(),
                        token: None,
                        authentication: None,
                        tenant: None,
                    },
                )
                .await
                .is_err()
        );
        assert!(
            handler
                .get_push_config(
                    &params,
                    GetTaskPushNotificationConfigRequest {
                        task_id: "t".into(),
                        id: "cfg".into(),
                        tenant: None,
                    },
                )
                .await
                .is_err()
        );
        assert!(
            handler
                .list_push_configs(
                    &params,
                    ListTaskPushNotificationConfigsRequest {
                        task_id: "t".into(),
                        page_size: None,
                        page_token: None,
                        tenant: None,
                    },
                )
                .await
                .is_err()
        );
        assert!(
            handler
                .delete_push_config(
                    &params,
                    DeleteTaskPushNotificationConfigRequest {
                        task_id: "t".into(),
                        id: "cfg".into(),
                        tenant: None,
                    },
                )
                .await
                .is_err()
        );
        assert!(
            handler
                .get_extended_agent_card(&params, GetExtendedAgentCardRequest { tenant: None })
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn test_subscribe_to_an_unknown_task_is_not_found() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = SubscribeToTaskRequest {
            last_seen_generation: None,
            id: "t1".into(),
            tenant: None,
        };
        let result = handler.subscribe_to_task(&params, req).await;
        match result {
            Ok(_) => panic!("expected subscribe_to_task to fail"),
            Err(error) => assert_eq!(error.code, error_code::TASK_NOT_FOUND),
        }
        assert!(handler.execution_manager.waiting().is_empty());
    }

    #[tokio::test]
    async fn test_subscribe_to_terminal_task_is_unsupported_not_not_found() {
        let handler = make_handler();
        handler
            .task_store
            .create(Task {
                generation: 0,
                timeline: None,
                id: "t-done".into(),
                context_id: "c-done".into(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            })
            .await
            .unwrap();
        let result = handler
            .subscribe_to_task(
                &ServiceParams::new(),
                SubscribeToTaskRequest {
                    last_seen_generation: None,
                    id: "t-done".into(),
                    tenant: None,
                },
            )
            .await;
        match result {
            Ok(_) => panic!("expected subscribe_to_task to fail"),
            Err(error) => assert_eq!(error.code, error_code::UNSUPPORTED_OPERATION),
        }
        assert!(handler.execution_manager.waiting().is_empty());
    }

    #[tokio::test]
    async fn test_subscribe_to_a_task_awaiting_input_opens_with_the_stored_task() {
        let handler = make_input_required_handler();
        let params = ServiceParams::new();
        let task = task_awaiting_input(&handler).await;
        let stored = handler
            .get_task(
                &params,
                GetTaskRequest {
                    current_generation: None,
                    timeline_length: None,

                    id: task.id.clone(),
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(stored.status.state, TaskState::InputRequired);

        let mut subscription = handler
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    last_seen_generation: None,

                    id: task.id.clone(),
                    tenant: None,
                },
            )
            .await
            .unwrap();
        match subscription.next().await.unwrap().unwrap() {
            StreamResponse::Task(first) => assert_eq!(first, stored),
            other => panic!("expected the stored task first, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_cancel_ends_a_subscription_to_a_task_awaiting_input() {
        let handler = make_input_required_handler();
        let params = ServiceParams::new();
        let task = task_awaiting_input(&handler).await;
        let subscription = handler
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    last_seen_generation: None,

                    id: task.id.clone(),
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let canceled = handler
            .cancel_task(
                &params,
                CancelTaskRequest {
                    id: task.id.clone(),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);

        // The stored task, then the canceled one, and the stream ends.
        let events: Vec<_> = timeout(Duration::from_secs(5), subscription.collect())
            .await
            .expect("the subscription outlived the canceled task");
        assert!(matches!(
            events.as_slice(),
            [Ok(StreamResponse::Task(stored)), Ok(StreamResponse::Task(last))]
                if stored.status.state == TaskState::InputRequired
                    && last.status.state == TaskState::Canceled
        ));
        assert!(handler.execution_manager.waiting().is_empty());
    }

    #[tokio::test]
    async fn test_cancel_failing_part_way_still_ends_a_waiting_subscription() {
        for (cancel, code) in [
            (Cancel::Fails, error_code::INTERNAL_ERROR),
            (
                Cancel::SendsAnInapplicableEvent,
                error_code::INVALID_AGENT_RESPONSE,
            ),
        ] {
            let handler = DefaultRequestHandler::new(
                InputRequiredExecutor { cancel },
                InMemoryTaskStore::new(),
            );
            let params = ServiceParams::new();
            let task = task_awaiting_input(&handler).await;
            let subscription = handler
                .subscribe_to_task(
                    &params,
                    SubscribeToTaskRequest {
                        last_seen_generation: None,

                        id: task.id.clone(),
                        tenant: None,
                    },
                )
                .await
                .unwrap();

            let error = handler
                .cancel_task(
                    &params,
                    CancelTaskRequest {
                        id: task.id.clone(),
                        metadata: None,
                        tenant: None,
                    },
                )
                .await
                .unwrap_err();
            assert_eq!(error.code, code);

            // The store holds the task canceled before the executor ran.
            let events: Vec<_> = timeout(Duration::from_secs(5), subscription.collect())
                .await
                .expect("the subscription outlived the canceled task");
            assert!(matches!(
                events.as_slice(),
                [Ok(StreamResponse::Task(stored)), Ok(StreamResponse::Task(last))]
                    if stored.status.state == TaskState::InputRequired
                        && last.status.state == TaskState::Canceled
            ));
            assert!(handler.execution_manager.waiting().is_empty());
        }
    }

    #[tokio::test]
    async fn test_subscriptions_to_a_task_awaiting_input_follow_its_next_turn() {
        let handler = make_input_required_handler();
        let params = ServiceParams::new();
        let task = task_awaiting_input(&handler).await;
        let subscribe = || SubscribeToTaskRequest {
            last_seen_generation: None,

            id: task.id.clone(),
            tenant: None,
        };
        let first = handler
            .subscribe_to_task(&params, subscribe())
            .await
            .unwrap();
        let second = handler
            .subscribe_to_task(&params, subscribe())
            .await
            .unwrap();

        // The whole next turn runs before either stream is polled: what it
        // publishes must still reach both.
        let mut follow_up = make_message();
        follow_up.task_id = Some(task.id.clone());
        let response = handler
            .send_message(
                &params,
                SendMessageRequest {
                    artifact_updates: Vec::new(),

                    message: follow_up,
                    configuration: None,
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            response,
            SendMessageResponse::Task(task) if task.status.state == TaskState::Completed
        ));

        // The stored task, the task as the turn found it, then the turn.
        let follows_the_turn = |events: Vec<Result<StreamResponse, A2AError>>| {
            matches!(
                events.as_slice(),
                [
                    Ok(StreamResponse::Task(stored)),
                    Ok(StreamResponse::Task(found)),
                    Ok(StreamResponse::StatusUpdate(turn)),
                ] if stored.status.state == TaskState::InputRequired
                    && found.status.state == TaskState::InputRequired
                    && turn.status.state == TaskState::Completed
            )
        };
        assert!(follows_the_turn(first.collect().await));
        assert!(follows_the_turn(second.collect().await));

        // Over: a new subscription is refused as for any finished task.
        let result = handler.subscribe_to_task(&params, subscribe()).await;
        assert!(matches!(result, Err(error) if error.code == error_code::UNSUPPORTED_OPERATION));
    }

    #[tokio::test]
    async fn test_a_dropped_subscription_to_a_task_awaiting_input_unregisters() {
        let handler = make_input_required_handler();
        let params = ServiceParams::new();
        let task = task_awaiting_input(&handler).await;
        let subscription = handler
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    last_seen_generation: None,

                    id: task.id.clone(),
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(handler.execution_manager.waiting().len(), 1);
        drop(subscription);
        assert!(handler.execution_manager.waiting().is_empty());
    }

    #[tokio::test]
    async fn test_send_message_return_immediately_allows_resubscribe() {
        use futures::StreamExt;

        let (handler, release) = make_resumable_handler();
        let params = ServiceParams::new();
        let mut message = make_message();
        message.task_id = Some("t-resume".into());
        message.context_id = Some("c-resume".into());
        seed_task(&handler, "t-resume", "c-resume").await;

        let response = handler
            .send_message(
                &params,
                SendMessageRequest {
                    artifact_updates: Vec::new(),
                    message,
                    configuration: Some(SendMessageConfiguration {
                        if_generation_match: None,
                        timeline_length: None,
                        accepted_output_modes: None,
                        task_push_notification_config: None,
                        history_length: None,
                        return_immediately: Some(true),
                    }),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        match response {
            SendMessageResponse::Task(task) => {
                assert_eq!(task.status.state, TaskState::Working);
            }
            _ => panic!("expected Task response"),
        }

        let mut subscription = handler
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    last_seen_generation: None,
                    id: "t-resume".into(),
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let snapshot = subscription.next().await.unwrap().unwrap();
        match snapshot {
            StreamResponse::Task(task) => assert_eq!(task.status.state, TaskState::Working),
            _ => panic!("expected snapshot task"),
        }

        release.notify_waiters();

        let terminal = subscription.next().await.unwrap().unwrap();
        match terminal {
            StreamResponse::Task(task) => assert_eq!(task.status.state, TaskState::Completed),
            _ => panic!("expected terminal task"),
        }

        assert!(subscription.next().await.is_none());

        let result = handler
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    last_seen_generation: None,
                    id: "t-resume".into(),
                    tenant: None,
                },
            )
            .await;
        match result {
            Ok(_) => panic!("expected subscribe_to_task to fail after completion"),
            Err(error) => assert_eq!(error.code, error_code::UNSUPPORTED_OPERATION),
        }
    }

    #[tokio::test]
    async fn test_list_tasks_page_size_cap_bounds_a_store_that_ignores_it() {
        install_crypto_provider();
        // A store that pays no attention to page_size. Clamping the request
        // does nothing here; only truncating the response bounds it.
        let handler = DefaultRequestHandler::new(
            EchoExecutor,
            PagingTaskStore::new(MAX_PAGE_SIZE * 3, false),
        );

        let resp = handler
            .list_tasks(
                &ServiceParams::new(),
                ListTasksRequest {
                    timeline_length: None,
                    context_id: None,
                    status: None,
                    page_size: Some(1_000_000),
                    page_token: None,
                    history_length: None,
                    include_artifacts: None,
                    status_timestamp_after: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(resp.tasks.len(), MAX_PAGE_SIZE);
    }

    #[tokio::test]
    async fn test_paging_stores_delegate_everything_except_list() {
        install_crypto_provider();
        // The cap tests above are only measuring the handler if these two
        // doubles differ from the real stores in `list` and nowhere else.
        // Pin that, so a later edit cannot quietly make them lie.
        let tasks = PagingTaskStore::new(0, true);
        let task = Task {
            generation: 0,
            timeline: None,
            id: "t-delegated".into(),
            context_id: "c1".into(),
            status: TaskStatus {
                state: TaskState::Submitted,
                message: None,
                timestamp: None,
            },
            artifacts: None,
            history: None,
            metadata: None,
        };
        tasks.create(task.clone()).await.unwrap();
        assert_eq!(
            tasks
                .get("t-delegated")
                .await
                .unwrap()
                .map(|t| t.status.state),
            Some(TaskState::Submitted)
        );
        let mut done = task.clone();
        done.status.state = TaskState::Completed;
        tasks.update(done).await.unwrap();
        assert_eq!(
            tasks
                .get("t-delegated")
                .await
                .unwrap()
                .map(|t| t.status.state),
            Some(TaskState::Completed)
        );

        // begin_cancel has no default (#250), so this must reach
        // InMemoryTaskStore's atomic implementation -- and refuse a task that
        // is already terminal, as the trait documents.
        let mut cancelable = task.clone();
        cancelable.id = "t-cancelable".into();
        cancelable.status.state = TaskState::Working;
        tasks.create(cancelable).await.unwrap();
        assert_eq!(
            tasks
                .begin_cancel("t-cancelable")
                .await
                .unwrap()
                .status
                .state,
            TaskState::Canceled
        );
        assert_eq!(
            tasks.begin_cancel("t-delegated").await.unwrap_err().code,
            error_code::TASK_NOT_CANCELABLE
        );

        let configs = PagingPushConfigStore::new(0);
        configs
            .save(TaskPushNotificationConfig {
                protocol_version: None,
                task_id: "t1".into(),
                url: "https://example.com/callback".into(),
                id: Some("cfg-1".into()),
                token: None,
                authentication: None,
                tenant: None,
            })
            .await
            .unwrap();
        assert_eq!(
            configs.get("t1", "cfg-1").await.unwrap().id.as_deref(),
            Some("cfg-1")
        );
        configs.delete("t1", "cfg-1").await.unwrap();
        assert!(configs.get("t1", "cfg-1").await.is_err());
        configs.delete_all("t1").await.unwrap();
    }

    #[tokio::test]
    async fn test_list_tasks_page_size_cap_bounds_a_store_that_honours_it() {
        install_crypto_provider();
        // The ordinary case: the store does what it is asked, so what it is
        // asked for has to be bounded. Without the clamp this store would
        // build a million tasks.
        let store = PagingTaskStore::new(MAX_PAGE_SIZE * 3, true);
        let seen = Arc::clone(&store.seen_page_size);
        let handler = DefaultRequestHandler::new(EchoExecutor, store);

        let resp = handler
            .list_tasks(
                &ServiceParams::new(),
                ListTasksRequest {
                    timeline_length: None,
                    context_id: None,
                    status: None,
                    page_size: Some(1_000_000),
                    page_token: None,
                    history_length: None,
                    include_artifacts: None,
                    status_timestamp_after: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(resp.tasks.len(), MAX_PAGE_SIZE);
        // The point of the clamp: the store is never asked for the million.
        // Asserting only on the response would pass with no clamp at all,
        // because the response bound would hide it.
        assert_eq!(seen.load(Ordering::SeqCst), MAX_PAGE_SIZE as i32);
    }

    #[tokio::test]
    async fn test_list_tasks_truncation_keeps_the_store_continuation_token() {
        install_crypto_provider();
        // Truncating must not strand the remainder. The handler cannot
        // synthesise a token for a paging scheme it does not own, so the
        // store's own token has to survive untouched -- otherwise a caller
        // that lost 200 tasks has no way to ask for them.
        let handler = DefaultRequestHandler::new(
            EchoExecutor,
            PagingTaskStore::new(MAX_PAGE_SIZE * 3, false)
                .with_next_page_token("cursor-from-store"),
        );

        let resp = handler
            .list_tasks(
                &ServiceParams::new(),
                ListTasksRequest {
                    timeline_length: None,
                    context_id: None,
                    status: None,
                    page_size: None,
                    page_token: None,
                    history_length: None,
                    include_artifacts: None,
                    status_timestamp_after: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(resp.tasks.len(), DEFAULT_PAGE_SIZE);
        assert_eq!(resp.next_page_token, "cursor-from-store");
    }

    #[tokio::test]
    async fn test_list_tasks_honours_a_page_size_under_the_cap() {
        install_crypto_provider();
        // The clamp is an upper bound, not a floor: a modest request must
        // reach the store unchanged.
        let store = PagingTaskStore::new(MAX_PAGE_SIZE * 3, true);
        let seen = Arc::clone(&store.seen_page_size);
        let handler = DefaultRequestHandler::new(EchoExecutor, store);

        let resp = handler
            .list_tasks(
                &ServiceParams::new(),
                ListTasksRequest {
                    timeline_length: None,
                    context_id: None,
                    status: None,
                    page_size: Some(7),
                    page_token: None,
                    history_length: None,
                    include_artifacts: None,
                    status_timestamp_after: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        assert_eq!(seen.load(Ordering::SeqCst), 7);
        assert_eq!(resp.tasks.len(), 7);
    }

    fn handler_with_extensions(extensions: Vec<(&str, Option<bool>)>) -> DefaultRequestHandler {
        make_handler().with_capabilities(AgentCapabilities {
            user_artifacts: None,
            streaming: None,
            push_notifications: None,
            extensions: Some(
                extensions
                    .into_iter()
                    .map(|(uri, required)| AgentExtension {
                        uri: uri.into(),
                        description: None,
                        required,
                        params: None,
                    })
                    .collect(),
            ),
            extended_agent_card: None,
        })
    }

    /// §3.3.4: a request that does not activate an extension the card marks
    /// `required` is refused with ExtensionSupportRequiredError, on every
    /// operation.
    #[tokio::test]
    async fn test_required_extension_not_activated_is_refused() {
        let handler = handler_with_extensions(vec![
            ("urn:ext:optional", None),
            ("urn:ext:required", Some(true)),
        ]);
        let mut params = ServiceParams::new();
        params.insert("a2a-extensions".into(), vec!["urn:ext:optional".into()]);

        let error = handler
            .send_message(&params, send_request())
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::EXTENSION_SUPPORT_REQUIRED);
        assert!(
            error.message.contains("urn:ext:required"),
            "{}",
            error.message
        );

        let error = handler
            .get_task(
                &params,
                GetTaskRequest {
                    current_generation: None,
                    timeline_length: None,
                    id: "t1".into(),
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::EXTENSION_SUPPORT_REQUIRED);
    }

    /// The required extension may arrive comma-separated in one value or as
    /// a repeated service parameter; an optional one need not arrive at all.
    #[tokio::test]
    async fn test_required_extension_activated_is_accepted() {
        let handler = handler_with_extensions(vec![
            ("urn:ext:optional", Some(false)),
            ("urn:ext:required", Some(true)),
        ]);
        for activated in [
            vec!["urn:ext:other, urn:ext:required".to_string()],
            vec!["urn:ext:other".to_string(), "urn:ext:required".to_string()],
        ] {
            let mut params = ServiceParams::new();
            params.insert("a2a-extensions".into(), activated);
            handler.send_message(&params, send_request()).await.unwrap();
        }

        // Nothing required: no activation needed.
        let handler = handler_with_extensions(vec![("urn:ext:optional", None)]);
        handler
            .send_message(&ServiceParams::new(), send_request())
            .await
            .unwrap();
    }

    struct PushScriptedExecutor<F>(F);

    type PushStep = (Option<Arc<Notify>>, Result<StreamResponse, A2AError>);

    impl<F> crate::AgentExecutor for PushScriptedExecutor<F>
    where
        F: Fn(&ExecutorContext) -> Vec<PushStep> + Send + Sync + 'static,
    {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            push_play((self.0)(&ctx))
        }

        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            push_play((self.0)(&ctx))
        }
    }

    fn push_play(steps: Vec<PushStep>) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::iter(steps).then(|(gate, event)| async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            event
        }))
    }

    fn push_status_event(
        ctx: &ExecutorContext,
        state: TaskState,
        message: Option<Message>,
    ) -> StreamResponse {
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state,
                message,
                timestamp: None,
            },
            generation: 0,
            metadata: None,
        })
    }

    fn push_delivery_handler(executor: impl crate::AgentExecutor) -> DefaultRequestHandler {
        DefaultRequestHandler::new(executor, InMemoryTaskStore::new()).with_push_notifications(
            InMemoryPushConfigStore::new(),
            crate::HttpPushSender::new(Some(crate::HttpPushSenderConfig {
                validate_urls: false,
                ..Default::default()
            })),
        )
    }

    /// §4.3.3: the FAILED state an execution error leaves behind is a task
    /// update, so the webhook learns the task ended.
    #[tokio::test]
    async fn test_execution_error_pushes_the_failed_state() {
        install_crypto_provider();
        let (url, mut receiver, shutdown_tx, server) = start_push_webhook().await;
        let handler = push_delivery_handler(PushScriptedExecutor(|ctx: &ExecutorContext| {
            vec![
                (None, Ok(push_status_event(ctx, TaskState::Working, None))),
                (None, Err(A2AError::internal("boom"))),
            ]
        }));
        let mut request = send_request();
        request.configuration = Some(SendMessageConfiguration {
            if_generation_match: None,
            timeline_length: None,
            accepted_output_modes: None,
            task_push_notification_config: Some(TaskPushNotificationConfig {
                url: url.clone(),
                ..push_config("")
            }),
            history_length: None,
            return_immediately: None,
        });
        handler
            .send_message(&ServiceParams::new(), request)
            .await
            .unwrap_err();

        let working = next_push(&mut receiver).await.event;
        assert!(
            matches!(working, StreamResponse::StatusUpdate(ref update) if update.status.state == TaskState::Working),
            "{working:?}"
        );
        let failed = next_push(&mut receiver).await.event;
        assert!(
            matches!(
                failed,
                StreamResponse::StatusUpdate(ref update)
                    if update.status.state == TaskState::Failed && update.status.message.is_some()
            ),
            "{failed:?}"
        );

        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    /// The CANCELED state `begin_cancel` commits is pushed even when the
    /// executor's cancel emits nothing, and only once when it does.
    #[tokio::test]
    async fn test_cancel_pushes_the_canceled_state_once() {
        install_crypto_provider();
        for executor_emits_canceled in [false, true] {
            let (url, mut receiver, shutdown_tx, server) = start_push_webhook().await;
            let handler =
                push_delivery_handler(PushScriptedExecutor(move |ctx: &ExecutorContext| {
                    if executor_emits_canceled {
                        vec![(None, Ok(push_status_event(ctx, TaskState::Canceled, None)))]
                    } else {
                        Vec::<PushStep>::new()
                    }
                }));
            let params = ServiceParams::new();
            seed_task(&handler, "t-cancel-push", "c-cancel-push").await;
            handler
                .create_push_config(
                    &params,
                    TaskPushNotificationConfig {
                        url: url.clone(),
                        ..push_config("t-cancel-push")
                    },
                )
                .await
                .unwrap();

            let task = handler
                .cancel_task(
                    &params,
                    CancelTaskRequest {
                        id: "t-cancel-push".into(),
                        metadata: None,
                        tenant: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(task.status.state, TaskState::Canceled);

            let pushed = next_push(&mut receiver).await.event;
            assert!(
                matches!(
                    pushed,
                    StreamResponse::StatusUpdate(ref update)
                        if update.task_id == "t-cancel-push" && update.status.state == TaskState::Canceled
                ),
                "{pushed:?}"
            );
            assert!(
                timeout(Duration::from_millis(200), receiver.recv())
                    .await
                    .is_err(),
                "the CANCELED state was pushed twice"
            );

            shutdown_tx.send(()).unwrap();
            server.await.unwrap();
        }
    }

    /// A cancel stream that fails still leaves the task CANCELED, so the
    /// webhook gets that state and the caller still gets the error.
    #[tokio::test]
    async fn test_failed_cancel_still_pushes_the_canceled_state() {
        install_crypto_provider();
        let (url, mut receiver, shutdown_tx, server) = start_push_webhook().await;
        let handler = push_delivery_handler(PushScriptedExecutor(|_ctx: &ExecutorContext| {
            vec![(None, Err(A2AError::internal("boom")))]
        }));
        let params = ServiceParams::new();
        seed_task(&handler, "t-cancel-error", "c-cancel-error").await;
        handler
            .create_push_config(
                &params,
                TaskPushNotificationConfig {
                    url: url.clone(),
                    ..push_config("t-cancel-error")
                },
            )
            .await
            .unwrap();

        handler
            .cancel_task(
                &params,
                CancelTaskRequest {
                    id: "t-cancel-error".into(),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap_err();

        let pushed = next_push(&mut receiver).await.event;
        assert!(
            matches!(
                pushed,
                StreamResponse::StatusUpdate(ref update)
                    if update.task_id == "t-cancel-error" && update.status.state == TaskState::Canceled
            ),
            "{pushed:?}"
        );

        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_extended_agent_card_not_configured() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let result = handler
            .get_extended_agent_card(&params, GetExtendedAgentCardRequest { tenant: None })
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn test_with_capabilities() {
        let handler = make_handler().with_capabilities(AgentCapabilities {
            user_artifacts: None,
            streaming: Some(true),
            push_notifications: None,
            extensions: None,
            extended_agent_card: None,
        });
        assert_eq!(handler.capabilities.streaming, Some(true));
    }

    #[tokio::test]
    async fn test_streaming_refused_when_the_card_declares_it_absent() {
        let handler = make_handler().with_capabilities(caps(Some(false), None));
        let params = ServiceParams::new();

        // The Ok arm is a BoxStream, which is not Debug, so match rather
        // than unwrap_err().
        match handler
            .send_streaming_message(&params, send_request())
            .await
        {
            Err(error) => assert_eq!(error.code, error_code::UNSUPPORTED_OPERATION),
            Ok(_) => panic!("streaming must be refused when the card declares it absent"),
        }

        match handler
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    last_seen_generation: None,
                    id: "t1".into(),
                    tenant: None,
                },
            )
            .await
        {
            Err(error) => assert_eq!(error.code, error_code::UNSUPPORTED_OPERATION),
            Ok(_) => panic!("subscribe must be refused when the card declares it absent"),
        }
    }

    /// The regression guard. `AgentCapabilities::default()` leaves every field
    /// `None`, and most embedders never call `with_capabilities`; if `None`
    /// started meaning "refuse", all of them would break at once.
    #[tokio::test]
    async fn test_undeclared_capabilities_stay_permissive() {
        let handler = make_handler_with_push_configs();
        let params = ServiceParams::new();

        seed_task(&handler, "t1", "c1").await;
        assert!(
            handler
                .send_streaming_message(&params, send_request())
                .await
                .is_ok(),
            "an undeclared capability must not be treated as refused"
        );
        assert!(
            handler
                .create_push_config(&params, push_config("t1"))
                .await
                .is_ok()
        );
    }

    /// Supplying a store defaults the capability to `true` but must not
    /// override an explicit `false`, so the outcome does not depend on which
    /// builder method was called last.
    #[tokio::test]
    async fn test_builder_order_does_not_change_the_declared_capability() {
        let params = ServiceParams::new();

        for handler in [
            make_handler()
                .with_capabilities(caps(None, Some(false)))
                .with_push_config_store(InMemoryPushConfigStore::new()),
            make_handler()
                .with_push_config_store(InMemoryPushConfigStore::new())
                .with_capabilities(caps(None, Some(false))),
        ] {
            let error = handler
                .create_push_config(&params, push_config("t1"))
                .await
                .unwrap_err();
            assert_eq!(error.code, error_code::PUSH_NOTIFICATION_NOT_SUPPORTED);
        }
    }

    #[tokio::test]
    async fn test_extended_agent_card_is_served_when_configured() {
        let handler = make_handler().with_extended_agent_card(extended_card("secret skills"));
        let card = handler
            .get_extended_agent_card(&ServiceParams::new(), extended_request())
            .await
            .unwrap();
        assert_eq!(card.name, "secret skills");
    }

    /// Configuring a card also declares the capability, so serving one and
    /// advertising it are not two things to remember.
    #[tokio::test]
    async fn test_configuring_an_extended_card_declares_the_capability() {
        let handler = make_handler().with_extended_agent_card(extended_card("x"));
        assert_eq!(handler.capabilities.extended_agent_card, Some(true));
    }

    /// A2A gives this condition its own code. Before #204 it reported
    /// `UNSUPPORTED_OPERATION`, which says the agent does not do extended
    /// cards at all rather than that this deployment has not configured one.
    #[tokio::test]
    async fn test_unconfigured_extended_card_reports_its_own_code() {
        let handler = make_handler().with_capabilities(AgentCapabilities {
            user_artifacts: None,
            streaming: None,
            push_notifications: None,
            extensions: None,
            extended_agent_card: Some(true),
        });
        let error = handler
            .get_extended_agent_card(&ServiceParams::new(), extended_request())
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::EXTENDED_CARD_NOT_CONFIGURED);
        assert_ne!(error.code, error_code::UNSUPPORTED_OPERATION);
    }

    /// The capability gate from #203 runs first: a card declaring
    /// `extendedAgentCard: false` refuses before the configured/unconfigured
    /// distinction is even reached.
    #[tokio::test]
    async fn test_capability_gate_precedes_the_not_configured_check() {
        let handler = make_handler()
            .with_extended_agent_card(extended_card("x"))
            .with_capabilities(AgentCapabilities {
                user_artifacts: None,
                streaming: None,
                push_notifications: None,
                extensions: None,
                extended_agent_card: Some(false),
            });
        let error = handler
            .get_extended_agent_card(&ServiceParams::new(), extended_request())
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::UNSUPPORTED_OPERATION);
    }

    /// §12.4 fetches the extended card under a security scheme, so a
    /// resolver can vary what it reveals by caller.
    #[tokio::test]
    async fn test_resolver_sees_the_caller_params() {
        let handler = make_handler().with_extended_agent_card_resolver(|params: &ServiceParams| {
            let privileged = params.contains_key("x-privileged");
            Ok(extended_card(if privileged { "full" } else { "limited" }))
        });

        let card = handler
            .get_extended_agent_card(&ServiceParams::new(), extended_request())
            .await
            .unwrap();
        assert_eq!(card.name, "limited");

        let mut params = ServiceParams::new();
        params.insert("x-privileged".into(), vec!["yes".into()]);
        let card = handler
            .get_extended_agent_card(&params, extended_request())
            .await
            .unwrap();
        assert_eq!(card.name, "full");
    }
    #[tokio::test]
    async fn idle_cancellation_invokes_executor_hooks_and_persists_their_result() {
        for handler in [
            DefaultRequestHandler::new(PushEventExecutor, InMemoryTaskStore::new()),
            DefaultRequestHandler::new(MultiTurnExecutor, InMemoryTaskStore::new()),
        ] {
            seed_task(&handler, "idle-cancel", "context").await;
            let canceled = handler
                .cancel_task(
                    &ServiceParams::new(),
                    CancelTaskRequest {
                        id: "idle-cancel".into(),
                        metadata: None,
                        tenant: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(canceled.status.state, TaskState::Canceled);
            assert_eq!(
                handler
                    .task_store
                    .get("idle-cancel")
                    .await
                    .unwrap()
                    .unwrap(),
                canceled
            );
        }
    }

    #[tokio::test]
    async fn canceling_a_waiting_executor_wakes_subscribers_without_releasing_its_work_gate() {
        let (handler, _release) = make_resumable_handler();
        seed_task(&handler, "active-cancel", "context").await;
        let mut request = send_req(Some("active-cancel"), Some("context"));
        request.message.parts = vec![Part::text("wait")];
        let mut events = handler
            .send_streaming_message(&ServiceParams::new(), request)
            .await
            .unwrap();
        // An executor snapshot may itself be the opening response.
        loop {
            let event = timeout(Duration::from_secs(1), events.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if matches!(event, StreamResponse::Task(ref task) if task.status.state == TaskState::Working)
            {
                break;
            }
        }
        let canceled = timeout(
            Duration::from_secs(1),
            handler.cancel_task(
                &ServiceParams::new(),
                CancelTaskRequest {
                    id: "active-cancel".into(),
                    metadata: None,
                    tenant: None,
                },
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);
        let mut saw_cancel = false;
        while let Some(event) = timeout(Duration::from_secs(1), events.next())
            .await
            .unwrap()
        {
            let event = event.unwrap();
            saw_cancel |= matches!(event, StreamResponse::Task(ref task) if task.status.state == TaskState::Canceled)
                || matches!(event, StreamResponse::StatusUpdate(ref update) if update.status.state == TaskState::Canceled);
        }
        assert!(
            saw_cancel,
            "subscribers must observe the cancellation before closing"
        );
    }

    #[tokio::test]
    async fn idle_interrupted_tasks_can_be_canceled_with_each_executor_style() {
        let handlers = [
            make_push_delivery_handler(),
            make_resumable_handler().0,
            DefaultRequestHandler::new(MultiTurnExecutor, InMemoryTaskStore::new()),
        ];
        for handler in handlers {
            seed_task(&handler, "idle-cancel", "context").await;
            let mut task = handler.load_task("idle-cancel").await.unwrap();
            task.status.state = TaskState::InputRequired;
            handler.task_store.update(task).await.unwrap();
            let canceled = handler
                .cancel_task(
                    &ServiceParams::new(),
                    CancelTaskRequest {
                        id: "idle-cancel".into(),
                        metadata: None,
                        tenant: None,
                    },
                )
                .await
                .unwrap();
            assert_eq!(canceled.id, "idle-cancel");
            assert_eq!(canceled.context_id, "context");
            assert_eq!(canceled.status.state, TaskState::Canceled);
            assert_eq!(
                handler.load_task("idle-cancel").await.unwrap().status.state,
                TaskState::Canceled
            );
        }
    }

    #[test]
    fn required_extensions_are_checked_with_case_insensitive_multi_value_headers() {
        let handler = make_handler().with_capabilities(AgentCapabilities {
            extensions: Some(vec![AgentExtension {
                uri: "urn:required".into(),
                description: None,
                required: Some(true),
                params: None,
            }]),
            ..Default::default()
        });
        assert_eq!(
            handler
                .authorize(&ServiceParams::new(), None)
                .unwrap_err()
                .code,
            error_code::EXTENSION_SUPPORT_REQUIRED
        );
        let mut params = ServiceParams::from([(
            "A2A-Extensions".into(),
            vec!["urn:other".into(), " urn:required, urn:extra ".into()],
        )]);
        handler.authorize(&params, Some("task")).unwrap();
        params.insert("A2A-Extensions".into(), vec!["urn:required-suffix".into()]);
        assert_eq!(
            handler.authorize(&params, None).unwrap_err().code,
            error_code::EXTENSION_SUPPORT_REQUIRED
        );
    }

    #[tokio::test]
    async fn legacy_cancel_without_a_store_generation_is_recorded_exactly_once() {
        for running in [false, true] {
            let handler = make_handler();
            seed_task(&handler, "legacy-cancel", "context").await;
            let mut canceled = handler.load_task("legacy-cancel").await.unwrap();
            canceled.status.state = TaskState::Canceled;
            handler.task_store.update(canceled.clone()).await.unwrap();
            let active = running.then(|| ActiveExecution::new(Some(canceled.clone())));
            let mut current = Some(canceled.clone());
            let event = handler
                .record_cancel_if_missing("legacy-cancel", &canceled, active.as_ref(), &mut current)
                .await
                .unwrap()
                .unwrap();
            let StreamResponse::StatusUpdate(status) = event else {
                panic!("expected cancellation status");
            };
            assert_eq!(status.generation, canceled.generation + 1);
            assert_eq!(status.status.state, TaskState::Canceled);
            let task = handler.load_task("legacy-cancel").await.unwrap();
            assert_eq!(task.generation, status.generation);
            assert_eq!(task.timeline.as_ref().unwrap().len(), 1);
            assert!(
                handler
                    .record_cancel_if_missing(
                        "legacy-cancel",
                        &canceled,
                        active.as_ref(),
                        &mut current
                    )
                    .await
                    .unwrap()
                    .is_none()
            );
            assert_eq!(handler.load_task("legacy-cancel").await.unwrap(), task);
        }
    }

    #[tokio::test]
    async fn authorized_listing_fails_closed_for_stores_without_scoped_listing() {
        let handler = DefaultRequestHandler::new(EchoExecutor, PagingTaskStore::new(1, true))
            .with_authorizer(|_: &ServiceParams, _: Option<&str>| Ok(()));
        let request: ListTasksRequest = serde_json::from_value(serde_json::json!({})).unwrap();
        let error = handler
            .list_tasks(&ServiceParams::new(), request)
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::UNSUPPORTED_OPERATION);
        assert!(error.message.contains("authorized task listing"));
    }
}

#[cfg(test)]
mod upstream_regressions {
    use super::*;
    use crate::handler::protocol::task_id_for_event;
    use crate::handler::test_support::*;
    fn upstream_handler(
        executor: impl crate::AgentExecutor,
        store: impl crate::TaskStore,
    ) -> DefaultRequestHandler {
        DefaultRequestHandler::new(executor, store).with_capabilities(AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        })
    }
    /// Plays the events a test scripts for each call: `execute` gets the
    /// context with the incoming message, `cancel` the one without. A step
    /// can wait on a `Notify` first, so a test orders the executor against
    /// its own calls without sleeping.
    struct ScriptedExecutor<F>(F);

    type Step = (Option<Arc<Notify>>, Result<StreamResponse, A2AError>);

    impl<F> crate::AgentExecutor for ScriptedExecutor<F>
    where
        F: Fn(&ExecutorContext) -> Vec<Step> + Send + Sync + 'static,
    {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            play((self.0)(&ctx))
        }

        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            play((self.0)(&ctx))
        }
    }

    fn play(steps: Vec<Step>) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
        Box::pin(stream::iter(steps).then(|(gate, event)| async move {
            if let Some(gate) = gate {
                gate.notified().await;
            }
            event
        }))
    }

    fn status_event(
        ctx: &ExecutorContext,
        state: TaskState,
        message: Option<Message>,
    ) -> StreamResponse {
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            generation: 0,
            task_id: ctx.task_id.clone(),
            context_id: ctx.context_id.clone(),
            status: TaskStatus {
                state,
                message,
                timestamp: None,
            },
            metadata: None,
        })
    }

    /// An execution that fails midway leaves its task FAILED with a
    /// sanitized status message; the error itself still reaches the caller.
    #[tokio::test]
    async fn test_execution_error_leaves_the_task_failed() {
        let handler = upstream_handler(
            ScriptedExecutor(|ctx: &ExecutorContext| {
                vec![
                    (None, Ok(status_event(ctx, TaskState::Working, None))),
                    (None, Err(A2AError::internal("executor exploded"))),
                ]
            }),
            InMemoryTaskStore::new(),
        );
        let events: Vec<_> = handler
            .send_streaming_message(&ServiceParams::new(), send_request())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(matches!(events.last(), Some(Err(error)) if error.message == "executor exploded"));
        let task_id = task_id_for_event(events[0].as_ref().unwrap())
            .unwrap()
            .to_owned();

        let task = handler
            .get_task(
                &ServiceParams::new(),
                GetTaskRequest {
                    current_generation: None,
                    timeline_length: None,
                    id: task_id,
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(task.status.state, TaskState::Failed);
        assert_eq!(
            task.status.message.as_ref().and_then(Message::text),
            Some("Internal error")
        );
    }

    /// An event the server cannot apply fails the task the same way.
    #[tokio::test]
    async fn test_unappliable_event_leaves_the_task_failed() {
        let handler = upstream_handler(
            ScriptedExecutor(|ctx: &ExecutorContext| {
                let append = StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                    generation: 0,
                    task_id: ctx.task_id.clone(),
                    context_id: ctx.context_id.clone(),
                    artifact: Artifact {
                        role: None,
                        start_generation: Some(0),
                        end_generation: None,
                        artifact_id: "unknown".into(),
                        name: None,
                        description: None,
                        parts: vec![Part::text("more")],
                        metadata: None,
                        extensions: None,
                    },
                    append: Some(true),
                    last_chunk: None,
                    metadata: None,
                });
                vec![(None, Ok(append))]
            }),
            InMemoryTaskStore::new(),
        );
        let error = handler
            .send_message(&ServiceParams::new(), send_request())
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::INVALID_AGENT_RESPONSE);

        let listed = handler
            .list_tasks(
                &ServiceParams::new(),
                ListTasksRequest {
                    timeline_length: None,
                    context_id: None,
                    status: None,
                    page_size: None,
                    page_token: None,
                    history_length: None,
                    status_timestamp_after: None,
                    include_artifacts: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(listed.tasks[0].status.state, TaskState::Failed);
    }

    /// A push the sender cannot deliver, with `fail_on_error`, fails the task
    /// as well.
    #[tokio::test]
    async fn test_failed_push_leaves_the_task_failed() {
        let handler = upstream_handler(
            ScriptedExecutor(|ctx: &ExecutorContext| {
                vec![(None, Ok(status_event(ctx, TaskState::Working, None)))]
            }),
            InMemoryTaskStore::new(),
        )
        .with_push_notifications(
            InMemoryPushConfigStore::new(),
            crate::HttpPushSender::new(Some(crate::HttpPushSenderConfig {
                fail_on_error: true,
                ..Default::default()
            })),
        );
        let mut request = send_request();
        request.configuration = Some(SendMessageConfiguration {
            if_generation_match: None,
            timeline_length: None,
            accepted_output_modes: None,
            // A loopback webhook is refused by the default URL validation.
            task_push_notification_config: Some(TaskPushNotificationConfig {
                url: "http://127.0.0.1:9/callback".into(),
                ..push_config("")
            }),
            history_length: None,
            return_immediately: None,
        });
        handler
            .send_message(&ServiceParams::new(), request)
            .await
            .unwrap_err();

        let listed = handler
            .list_tasks(
                &ServiceParams::new(),
                ListTasksRequest {
                    timeline_length: None,
                    context_id: None,
                    status: None,
                    page_size: None,
                    page_token: None,
                    history_length: None,
                    status_timestamp_after: None,
                    include_artifacts: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(listed.tasks[0].status.state, TaskState::Failed);
    }

    /// Cancelling a running task leaves it CANCELED: the execution the cancel
    /// stops is not a failure.
    #[tokio::test]
    async fn test_canceled_task_is_not_marked_failed() {
        let gate = Arc::new(Notify::new());
        let handler = upstream_handler(
            ScriptedExecutor(move |ctx: &ExecutorContext| {
                if ctx.message.is_some() {
                    vec![
                        (None, Ok(status_event(ctx, TaskState::Working, None))),
                        (
                            Some(gate.clone()),
                            Ok(status_event(ctx, TaskState::Working, None)),
                        ),
                    ]
                } else {
                    vec![(None, Ok(status_event(ctx, TaskState::Canceled, None)))]
                }
            }),
            InMemoryTaskStore::new(),
        );
        let mut request = send_request();
        request.configuration = Some(SendMessageConfiguration {
            if_generation_match: None,
            timeline_length: None,
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: None,
            return_immediately: Some(true),
        });
        handler
            .send_message(&ServiceParams::new(), request)
            .await
            .unwrap();
        let task_id = handler
            .list_tasks(
                &ServiceParams::new(),
                ListTasksRequest {
                    timeline_length: None,
                    context_id: None,
                    status: None,
                    page_size: None,
                    page_token: None,
                    history_length: None,
                    status_timestamp_after: None,
                    include_artifacts: None,
                    tenant: None,
                },
            )
            .await
            .unwrap()
            .tasks[0]
            .id
            .clone();
        let canceled = handler
            .cancel_task(
                &ServiceParams::new(),
                CancelTaskRequest {
                    id: task_id.clone(),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(canceled.status.state, TaskState::Canceled);

        let stored = handler
            .get_task(
                &ServiceParams::new(),
                GetTaskRequest {
                    current_generation: None,
                    timeline_length: None,
                    id: task_id,
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(stored.status.state, TaskState::Canceled);
    }

    /// The agent's INPUT_REQUIRED question stays in history between the
    /// turn that asked it and the answer.
    #[tokio::test]
    async fn test_continuation_keeps_the_question_in_history() {
        let handler = upstream_handler(
            ScriptedExecutor(|ctx: &ExecutorContext| {
                let asked = ctx
                    .message
                    .as_ref()
                    .and_then(Message::text)
                    .is_some_and(|text| text == "ask");
                let event = if asked {
                    let question = Message::new(Role::Agent, vec![Part::text("Which file?")]);
                    status_event(ctx, TaskState::InputRequired, Some(question))
                } else {
                    status_event(ctx, TaskState::Completed, None)
                };
                vec![(None, Ok(event))]
            }),
            InMemoryTaskStore::new(),
        );
        let params = ServiceParams::new();
        let ask = SendMessageRequest {
            message: Message::new(Role::User, vec![Part::text("ask")]),
            ..send_request()
        };
        handler.send_message(&params, ask).await.unwrap();
        let task_id = handler
            .list_tasks(
                &params,
                ListTasksRequest {
                    timeline_length: None,
                    context_id: None,
                    status: None,
                    page_size: None,
                    page_token: None,
                    history_length: None,
                    status_timestamp_after: None,
                    include_artifacts: None,
                    tenant: None,
                },
            )
            .await
            .unwrap()
            .tasks[0]
            .id
            .clone();
        let answer = SendMessageRequest {
            message: Message {
                task_id: Some(task_id.clone()),
                ..Message::new(Role::User, vec![Part::text("main.rs")])
            },
            ..send_request()
        };
        handler.send_message(&params, answer).await.unwrap();

        let task = handler
            .get_task(
                &params,
                GetTaskRequest {
                    current_generation: None,
                    timeline_length: None,
                    id: task_id,
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        let said: Vec<_> = task
            .history
            .unwrap()
            .iter()
            .map(|message| message.text().unwrap().to_owned())
            .collect();
        assert_eq!(said, ["ask", "Which file?", "main.rs"]);
    }

    /// An agent that asks "Which file?" on "ask" and completes on any other
    /// message. With `in_history`, the question is already in the `history`
    /// of the task snapshot it emits, as an agent that records it itself does.
    fn asking_handler(in_history: bool) -> DefaultRequestHandler {
        upstream_handler(
            ScriptedExecutor(move |ctx: &ExecutorContext| {
                let asked = ctx
                    .message
                    .as_ref()
                    .and_then(Message::text)
                    .is_some_and(|text| text == "ask");
                let event = match (asked, &ctx.message) {
                    (true, Some(ask)) if in_history => {
                        let question = Message::new(Role::Agent, vec![Part::text("Which file?")]);
                        StreamResponse::Task(Task {
                            generation: 0,
                            timeline: None,
                            id: ctx.task_id.clone(),
                            context_id: ctx.context_id.clone(),
                            status: TaskStatus {
                                state: TaskState::InputRequired,
                                message: Some(question.clone()),
                                timestamp: None,
                            },
                            artifacts: None,
                            history: Some(vec![ask.clone(), question]),
                            metadata: None,
                        })
                    }
                    (true, _) => {
                        let question = Message::new(Role::Agent, vec![Part::text("Which file?")]);
                        status_event(ctx, TaskState::InputRequired, Some(question))
                    }
                    (false, _) => status_event(ctx, TaskState::Completed, None),
                };
                vec![(None, Ok(event))]
            }),
            InMemoryTaskStore::new(),
        )
    }

    /// Sends "ask", then "main.rs" to the task it created, and returns the
    /// task's history as text.
    async fn answer_the_question(handler: &DefaultRequestHandler, streaming: bool) -> Vec<String> {
        async fn turn(handler: &DefaultRequestHandler, streaming: bool, message: Message) {
            let params = ServiceParams::new();
            let request = SendMessageRequest {
                message,
                ..send_request()
            };
            if streaming {
                let _: Vec<_> = handler
                    .send_streaming_message(&params, request)
                    .await
                    .unwrap()
                    .collect()
                    .await;
            } else {
                handler.send_message(&params, request).await.unwrap();
            }
        }

        let params = ServiceParams::new();
        turn(
            handler,
            streaming,
            Message::new(Role::User, vec![Part::text("ask")]),
        )
        .await;
        let task_id = handler
            .list_tasks(
                &params,
                ListTasksRequest {
                    timeline_length: None,
                    context_id: None,
                    status: None,
                    page_size: None,
                    page_token: None,
                    history_length: None,
                    status_timestamp_after: None,
                    include_artifacts: None,
                    tenant: None,
                },
            )
            .await
            .unwrap()
            .tasks[0]
            .id
            .clone();
        let answer = Message {
            task_id: Some(task_id.clone()),
            ..Message::new(Role::User, vec![Part::text("main.rs")])
        };
        turn(handler, streaming, answer).await;
        let task = handler
            .get_task(
                &params,
                GetTaskRequest {
                    current_generation: None,
                    timeline_length: None,
                    id: task_id,
                    history_length: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();
        task.history
            .unwrap()
            .iter()
            .map(|message| message.text().unwrap().to_owned())
            .collect()
    }

    /// `send_streaming_message` continues a task through the same path.
    #[tokio::test]
    async fn test_streamed_continuation_keeps_the_question_in_history() {
        let said = answer_the_question(&asking_handler(false), true).await;
        assert_eq!(said, ["ask", "Which file?", "main.rs"]);
    }

    /// A question the agent already put in `history` is not added twice.
    #[tokio::test]
    async fn test_continuation_does_not_repeat_a_question_already_in_history() {
        let said = answer_the_question(&asking_handler(true), false).await;
        assert_eq!(said, ["ask", "Which file?", "main.rs"]);
    }

    /// `configuration.historyLength` caps the Task snapshots of a stream as
    /// it caps the task a blocking send returns.
    #[tokio::test]
    async fn test_streamed_task_snapshots_respect_history_length() {
        let handler = upstream_handler(
            ScriptedExecutor(|ctx: &ExecutorContext| {
                let history = ["one", "two", "three"]
                    .map(|text| Message::new(Role::User, vec![Part::text(text)]))
                    .to_vec();
                let task = Task {
                    generation: 0,
                    timeline: None,
                    id: ctx.task_id.clone(),
                    context_id: ctx.context_id.clone(),
                    status: TaskStatus {
                        state: TaskState::Completed,
                        message: None,
                        timestamp: None,
                    },
                    artifacts: None,
                    history: Some(history),
                    metadata: None,
                };
                vec![(None, Ok(StreamResponse::Task(task)))]
            }),
            InMemoryTaskStore::new(),
        );
        let request = SendMessageRequest {
            configuration: Some(SendMessageConfiguration {
                if_generation_match: None,
                timeline_length: None,
                accepted_output_modes: None,
                task_push_notification_config: None,
                history_length: Some(1),
                return_immediately: None,
            }),
            ..send_request()
        };
        let events: Vec<_> = handler
            .send_streaming_message(&ServiceParams::new(), request)
            .await
            .unwrap()
            .collect()
            .await;
        assert!(matches!(
            &events[..],
            [Ok(StreamResponse::Task(task))] if task.history.as_ref().map(Vec::len) == Some(1)
        ));
    }

    /// A Message after the task's own events is an invalid agent response,
    /// not the answer (§3.1.2), and the task fails.
    #[tokio::test]
    async fn test_message_after_task_events_is_an_invalid_agent_response() {
        let handler = upstream_handler(
            ScriptedExecutor(|ctx: &ExecutorContext| {
                vec![
                    (None, Ok(status_event(ctx, TaskState::Working, None))),
                    (
                        None,
                        Ok(StreamResponse::Message(Message::new(
                            Role::Agent,
                            vec![Part::text("late")],
                        ))),
                    ),
                ]
            }),
            InMemoryTaskStore::new(),
        );
        let error = handler
            .send_message(&ServiceParams::new(), send_request())
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::INVALID_AGENT_RESPONSE);
    }

    /// Nothing may follow a Message that answered the request.
    #[tokio::test]
    async fn test_event_after_an_answering_message_is_an_invalid_agent_response() {
        let handler = upstream_handler(
            ScriptedExecutor(|ctx: &ExecutorContext| {
                vec![
                    (
                        None,
                        Ok(StreamResponse::Message(Message::new(
                            Role::Agent,
                            vec![Part::text("answer")],
                        ))),
                    ),
                    (None, Ok(status_event(ctx, TaskState::Completed, None))),
                ]
            }),
            InMemoryTaskStore::new(),
        );
        let events: Vec<_> = handler
            .send_streaming_message(&ServiceParams::new(), send_request())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(matches!(events[0], Ok(StreamResponse::Message(_))));
        assert!(
            matches!(&events[1], Err(error) if error.code == error_code::INVALID_AGENT_RESPONSE)
        );
    }
}
