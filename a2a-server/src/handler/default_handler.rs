// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Default request-handler state, configuration, operations, and tests.

use super::delivery::{AuthorizedMessage, Delivery, MessageInput};
use super::http_exchange::{self, HttpOpening, RegistrationLease};
use super::protocol::{
    self, IncomingRequest, ProtocolError, Rejection, RequestRules, ResponsePolicy, StartMode,
    apply_lengths, request_task_id, requested_protocol_version,
};
use super::push_delivery::PushService;
use super::response::{
    ResponseStream, Run, TaskEvents, message_response, task_response, unary_response,
};
use super::task_handle::TaskHandle;
use super::task_service::TaskService;
use super::uplink::{HttpLiveResponses, Uplink};

use crate::middleware::{ServiceParams, User};
use crate::pagination::resolve_page_size;
use a2a::*;
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use std::{sync::Arc, time::Duration};

use super::{ExtendedAgentCardResolver, LiveResponse, RequestAuthorizer, RequestHandler};

/// How long a long-poll `GetTask` (`currentGeneration`) is held open when the
/// task does not advance. The spec lets a server return earlier with the
/// current state.
const DEFAULT_LONG_POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// Deadline for waiting on the agent cancellation hook; finalization is separate.
const DEFAULT_CANCEL_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

/// Default implementation of [`RequestHandler`] that orchestrates
/// task lifecycle management, storage, and executor dispatch.
///
/// Cheap to clone: everything it holds is shared.
#[derive(Clone)]
pub struct DefaultRequestHandler {
    pub(super) tasks: Arc<TaskService>,
    pub(super) pushes: Option<Arc<PushService>>,
    pub(super) capabilities: AgentCapabilities,
    default_input_modes: Vec<String>,
    authorizer: Option<Arc<dyn RequestAuthorizer>>,
    extended_agent_card: Option<Arc<dyn ExtendedAgentCardResolver>>,
    long_poll_timeout: Duration,
    blocking_response_timeout: Option<Duration>,
    pub(super) cancel_hook_timeout: Duration,
    pub(super) http_live_responses: Arc<HttpLiveResponses>,
}

impl DefaultRequestHandler {
    // Configuration.
    /// Build a handler with optional capabilities initially undeclared.
    ///
    /// Use [`Self::with_capabilities`] to match the served Agent Card. Streaming,
    /// user artifacts, push notifications, and extended cards require an explicit
    /// capability declaration. Configuring push notifications or an extended card
    /// also declares that capability unless it was explicitly disabled.
    pub fn new(executor: impl crate::AgentExecutor, task_store: impl crate::TaskStore) -> Self {
        DefaultRequestHandler {
            tasks: Arc::new(TaskService::new(executor, task_store)),
            pushes: None,
            capabilities: AgentCapabilities::default(),
            default_input_modes: Vec::new(),
            authorizer: None,
            extended_agent_card: None,
            long_poll_timeout: DEFAULT_LONG_POLL_TIMEOUT,
            blocking_response_timeout: None,
            cancel_hook_timeout: DEFAULT_CANCEL_HOOK_TIMEOUT,
            http_live_responses: Arc::new(HttpLiveResponses::default()),
        }
    }

    /// A handler for an [`Agent`](crate::Agent), which decides for each message
    /// whether a task exists.
    pub fn from_agent(agent: impl crate::Agent, task_store: impl crate::TaskStore) -> Self {
        Self::new(
            crate::executor::AgentExecutorAdapter(Arc::new(agent)),
            task_store,
        )
    }

    /// Resume persisted nonterminal tasks at application startup.
    ///
    /// Explicitly call this before accepting requests, after configuring the handler.
    /// Executors opt in through [`crate::AgentExecutor::restart`] (or [`crate::Agent::restart`]).
    /// Returns the number of tasks whose restart hook accepted execution, not completed work.
    /// Missing, terminal, already-running, and unsupported tasks are skipped. The store
    /// must persist across process restarts. This does not provide distributed ownership
    /// across server processes, restore caller credentials, or replay previous messages.
    /// Per-task failures are logged and recovery continues through all pages before
    /// returning the first task error. Listing errors and handler shutdown stop the scan.
    /// Successfully restarted tasks remain running even if this returns an error;
    /// retrying skips those executions.
    ///
    /// ```no_run
    /// # async fn startup(handler: a2a_server::DefaultRequestHandler) -> Result<(), a2a::A2AError> {
    /// let restarted = handler.restart_tasks().await?;
    /// // Pass the configured handler to the server after recovery is scheduled.
    /// # let _ = restarted;
    /// # Ok(()) }
    /// ```
    pub async fn restart_tasks(&self) -> Result<usize, A2AError> {
        self.tasks.restart_tasks(self.clone()).await
    }

    /// Stop SDK-owned workers and wait until their futures have been dropped.
    ///
    /// Shutdown applies to every clone and is permanent. Stop accepting transport
    /// requests first. Active responses close and persisted tasks keep their last
    /// committed state for optional recovery. Application-spawned executor tasks
    /// remain the application's responsibility. This is cooperative: user futures
    /// must yield rather than block Tokio's worker threads.
    pub async fn shutdown(&self) {
        self.tasks.background_tasks.shutdown().await;
    }

    /// Bounds how long a `GetTask` carrying `currentGeneration` is held open
    /// when the task does not advance. The request then returns the current
    /// state, which the spec allows.
    pub fn with_long_poll_timeout(mut self, timeout: Duration) -> Self {
        self.long_poll_timeout = timeout;
        self
    }

    /// Bounds a blocking `SendMessage` response after input admission.
    ///
    /// The default has no deadline: A2A requires waiting for completion or
    /// interruption. Expiry returns the current committed Task, so callers do
    /// not mistake an admitted input for a failed send. Before lazy creation
    /// has committed any Task, expiry remains an error. Execution continues.
    /// Streaming, live, and `returnImmediately`
    /// responses use their own completion policies and have no such deadline.
    pub fn with_blocking_response_timeout(mut self, timeout: Duration) -> Self {
        self.blocking_response_timeout = Some(timeout);
        self
    }

    /// Deadline for polling agent cancellation output (30 seconds by default).
    /// Expiry still runs handler finalization; it does not cancel storage writes.
    pub fn with_cancel_hook_timeout(mut self, timeout: Duration) -> Self {
        self.cancel_hook_timeout = timeout;
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

    pub(super) fn authenticated_user(
        &self,
        params: &ServiceParams,
    ) -> Result<Option<User>, A2AError> {
        self.authorizer
            .as_ref()
            .map_or(Ok(None), |authorizer| authorizer.authenticated_user(params))
    }

    pub(super) fn authorize(
        &self,
        params: &ServiceParams,
        task_id: Option<&str>,
    ) -> Result<(), A2AError> {
        self.tasks.background_tasks.check_running()?;
        if let Some(authorizer) = &self.authorizer {
            authorizer.authorize(params, task_id)?;
        }
        self.protocol().validate_operation(params)
    }

    pub fn with_push_config_store(self, push_config_store: impl crate::PushConfigStore) -> Self {
        self.with_push_notifications(push_config_store, crate::HttpPushSender::new(None))
    }

    pub fn with_push_notifications(
        mut self,
        push_config_store: impl crate::PushConfigStore,
        push_sender: crate::HttpPushSender,
    ) -> Self {
        self.pushes = Some(PushService::new(push_config_store, push_sender));
        self.capabilities.push_notifications.get_or_insert(true);
        self
    }

    /// Replace the capabilities used to validate requests with those of the Agent Card.
    ///
    /// Optional features require `Some(true)`. Both `None` and `Some(false)` mean
    /// unsupported, as required by A2A 1.1 §3.3.4. Requests using them receive
    /// `UNSUPPORTED_OPERATION`, or `PUSH_NOTIFICATION_NOT_SUPPORTED` for push
    /// configuration operations. Push notifications and extended cards also need
    /// their corresponding service or card/resolver configuration.
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

    /// Shared request rules; application callbacks and worker state stay here.
    fn protocol(&self) -> RequestRules<'_> {
        RequestRules::new(&self.capabilities, &self.default_input_modes)
    }

    fn require_streaming(&self) -> Result<(), A2AError> {
        self.tasks.background_tasks.check_running()?;
        self.protocol().require_streaming()
    }

    fn require_push_notifications(&self) -> Result<(), A2AError> {
        self.protocol().require_push_notifications()
    }

    /// Require the advertised capability and a fully configured webhook service.
    fn push_service_checked(&self) -> Result<&PushService, A2AError> {
        self.require_push_notifications()?;
        self.pushes
            .as_deref()
            .ok_or_else(|| ProtocolError::PushUnsupported.into())
    }

    pub(super) fn attach_pushes(&self, task: &TaskHandle) {
        if let Some(pushes) = &self.pushes {
            pushes.attach(task);
        }
    }

    /// Live transports supply an opening request and retain their input stream.
    async fn start_live_response(
        &self,
        params: &ServiceParams,
        mut uplink: Uplink,
        registration: Option<RegistrationLease>,
    ) -> Result<ResponseStream, A2AError> {
        let first = tokio::select! {
            biased;
            () = self.tasks.background_tasks.cancelled() => {
                return Err(A2AError::internal("handler is shutting down"));
            }
            first = uplink.next() => first
                .ok_or(ProtocolError::MissingOpeningRequest)?,
        };
        let policy = match &first.request {
            IncomingRequest::Message(input) => {
                ResponsePolicy::message(StartMode::Bidi, params, input.request())
            }
            _ => ResponsePolicy::subscription(StartMode::Bidi, params),
        };
        let response = match first.request {
            IncomingRequest::Message(input) => self.admit_message(params, *input, policy).await?,
            IncomingRequest::Subscribe(request) => {
                self.attach_subscription(params, request, policy).await?
            }
            IncomingRequest::Invalid => {
                return Err(ProtocolError::UnknownStreamRequest.into());
            }
        };
        Ok(self.response_stream(params, response, policy, Some(uplink), registration))
    }

    /// Ordinary responses are polled directly; live responses own an uplink runner.
    fn response_stream(
        &self,
        params: &ServiceParams,
        (task, events): TaskEvents,
        policy: ResponsePolicy,
        uplink: Option<Uplink>,
        registration: Option<RegistrationLease>,
    ) -> ResponseStream {
        if policy.is_live() {
            Run::new(self.clone(), params.clone(), task, policy)
                .with_uplink(uplink, registration)
                .start(None, Some(events), 0)
        } else {
            task_response(self.clone(), task, policy, None, Some(events), 0)
        }
    }

    /// All message modes use the same authorization, validation, and task admission.
    async fn admit_message(
        &self,
        params: &ServiceParams,
        input: MessageInput,
        policy: ResponsePolicy,
    ) -> Result<TaskEvents, A2AError> {
        let requested_task = request_task_id(input.request());
        let delivery = self
            .receive_message(params, input, policy.mode(), None, policy.response_id())
            .await
            .map_err(|rejection| policy.opening_error(rejection, requested_task.as_deref()))?;
        Ok(message_response(delivery, policy))
    }

    /// Bind and authorize each message once, using its caller rather than
    /// the caller that opened a live exchange. Routed HTTP calls enter here
    /// before enqueueing; other inputs enter from receive_message.
    pub(super) fn authorize_message(
        &self,
        params: &ServiceParams,
        mut input: MessageInput,
        bound_task: Option<&str>,
    ) -> Result<AuthorizedMessage, Rejection> {
        // Bind before authorization so native follow-ups check the inferred task.
        if let Some(bound) = bound_task {
            protocol::bind_request_task(input.request_mut(), bound)?;
        }
        match input {
            // HTTP routing already checked this caller. Keep its credentials.
            MessageInput::Authorized(message) => Ok(message),
            MessageInput::Raw(request) => {
                self.authorize(params, request_task_id(&request).as_deref())?;
                Ok(AuthorizedMessage {
                    request,
                    params: params.clone(),
                    user: self.authenticated_user(params)?,
                    response_id: None,
                })
            }
        }
    }

    /// The message gateway for unary, server-streaming, and every live input.
    pub(super) async fn receive_message(
        &self,
        params: &ServiceParams,
        input: MessageInput,
        mode: StartMode,
        bound_task: Option<&str>,
        response_id: Option<uuid::Uuid>,
    ) -> Result<Delivery, Rejection> {
        let mut message = self.authorize_message(params, input, bound_task)?;
        message.response_id = response_id;
        // Validate before admission can persist input or deliver it to an executor.
        self.protocol().validate_message(&message.request)?;
        self.tasks.append_input(self.clone(), message, mode).await
    }

    // Subscriptions.
    /// Validate and capture a task subscription without starting response delivery.
    async fn attach_subscription(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
        policy: ResponsePolicy,
    ) -> Result<TaskEvents, A2AError> {
        self.authorize(params, Some(&req.id))?;
        self.require_streaming()?;
        self.tasks.subscribe(self.clone(), req, policy).await
    }

    // Push delivery.
    /// The push configuration the request carries, for `task_id`.
    pub(super) fn request_push_config(
        &self,
        params: &ServiceParams,
        task_id: &str,
        req: &SendMessageRequest,
    ) -> Result<Option<TaskPushNotificationConfig>, A2AError> {
        let Some(mut config) = req
            .configuration
            .as_ref()
            .and_then(|configuration| configuration.task_push_notification_config.clone())
        else {
            return Ok(None);
        };
        // Refused up front when the handler has nowhere to keep it.
        let pushes = self.push_service_checked()?;
        config.task_id = task_id.to_string();
        if config.tenant.is_none() {
            config.tenant = req.tenant.clone();
        }
        config
            .protocol_version
            .get_or_insert_with(|| requested_protocol_version(params));
        pushes.validate_config(&config)?;
        Ok(Some(config))
    }

    pub(super) async fn save_request_push_config(
        &self,
        params: &ServiceParams,
        task_id: &str,
        req: &SendMessageRequest,
    ) -> Result<(), A2AError> {
        if let Some(config) = self.request_push_config(params, task_id, req)? {
            self.push_service_checked()?.store().save(config).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl RequestHandler for DefaultRequestHandler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let policy = ResponsePolicy::message(StartMode::Blocking, params, &req);
        let (task, events) = self
            .admit_message(params, MessageInput::Raw(req), policy)
            .await?;
        let events = self.response_stream(params, (task.clone(), events), policy, None, None);
        let response = unary_response(self.tasks.clone(), task.clone(), events);
        let mut response =
            match self
                .blocking_response_timeout
                .filter(|_| !policy.return_immediately())
            {
                Some(timeout) => match tokio::time::timeout(timeout, response).await {
                    Ok(response) => response?,
                    Err(_) => {
                        let uncreated = task.active().is_some_and(|active| {
                            active.uncreated.load(std::sync::atomic::Ordering::Acquire)
                        });
                        if uncreated {
                            return Err(ProtocolError::BlockingResponseTimedOut.into());
                        }
                        SendMessageResponse::Task(task.snapshot().ok_or_else(|| {
                            A2AError::from(ProtocolError::BlockingResponseTimedOut)
                        })?)
                    }
                },
                None => response.await?,
            };
        if let SendMessageResponse::Task(task) = &mut response {
            policy.trim(task);
        }
        Ok(response)
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let policy = ResponsePolicy::message(StartMode::Streaming, params, &req);
        self.require_streaming()?;
        let response = self
            .admit_message(params, MessageInput::Raw(req), policy)
            .await?;
        let events = self.response_stream(params, response, policy, None, None);
        Ok(Box::pin(events.map(move |item| {
            item.map(|mut event| {
                if let StreamResponse::Task(task) = &mut event {
                    policy.trim(task);
                }
                event
            })
        })))
    }

    async fn send_live_message(
        &self,
        params: &ServiceParams,
        uplink: BoxStream<'static, StreamRequest>,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.require_streaming()?;
        self.start_live_response(params, Uplink::duplex(uplink), None)
            .await
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        self.authorize(params, Some(&req.id))?;
        let mut task = match req.current_generation {
            Some(current) => {
                self.tasks
                    .wait_for_generation(&req.id, current, self.long_poll_timeout)
                    .await?
            }
            None => self.tasks.load(&req.id).await?,
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
            self.tasks
                .list(
                    &req,
                    Some(&|id| authorizer.authorize(params, Some(id)).is_ok()),
                )
                .await?
        } else {
            self.tasks.list(&req, None).await?
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

        self.tasks
            .cancel(
                self.clone(),
                params.clone(),
                req,
                self.authenticated_user(params)?,
            )
            .await
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let policy = ResponsePolicy::subscription(StartMode::Streaming, params);
        let response = self.attach_subscription(params, req, policy).await?;
        Ok(self.response_stream(params, response, policy, None, None))
    }

    async fn send_live_request(
        &self,
        params: &ServiceParams,
        request: StreamRequest,
    ) -> Result<LiveResponse, A2AError> {
        self.require_streaming()?;
        let policy = match &request.payload {
            StreamRequestPayload::SendMessage(request) => {
                Some(ResponsePolicy::message(StartMode::Bidi, params, request))
            }
            _ => None,
        };
        match http_exchange::open(self, params, request).await? {
            HttpOpening::Acknowledged(mut task) => {
                if let Some(policy) = policy {
                    policy.trim(&mut task);
                }
                Ok(LiveResponse::Ack(task))
            }
            HttpOpening::Attached {
                registration,
                uplink,
            } => {
                let events = self
                    .start_live_response(params, uplink, Some(registration))
                    .await?;
                Ok(LiveResponse::Stream(events))
            }
        }
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
        let pushes = self.push_service_checked()?;
        self.tasks.load(&req.task_id).await?;
        pushes.validate_config(&req)?;
        pushes.store().save(req).await
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.authorize(params, Some(&req.task_id))?;
        self.push_service_checked()?
            .store()
            .get(&req.task_id, &req.id)
            .await
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        self.authorize(params, Some(&req.task_id))?;
        let pushes = self.push_service_checked()?;
        self.tasks.load(&req.task_id).await?;
        let mut configs = pushes.store().list(&req.task_id).await?;
        configs.sort_by(|left, right| left.id.cmp(&right.id));

        // Paging for push configs is done here, not in the store: the store
        // is asked for every config for the task and the page is cut from
        // that. PushConfigStore is public, so this clamp is what bounds the
        // response for an implementation with no per-task limit of its own.
        let page_size = resolve_page_size(req.page_size);
        let start = protocol::push_config_page_offset(req.page_token.as_deref())?;
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
        let pushes = self.push_service_checked()?;
        self.tasks.load(&req.task_id).await?;
        pushes.delete(&req.task_id, &req.id).await
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        self.authorize(params, None)?;
        self.protocol().require_extended_agent_card()?;
        match &self.extended_agent_card {
            Some(resolver) => resolver.resolve(params),
            // A2A defines a code for exactly this condition.
            // UNSUPPORTED_OPERATION would say the agent does not do extended
            // cards at all, when the truth is that this deployment has not
            // configured one — the client can act on that difference.
            None => Err(ProtocolError::ExtendedCardNotConfigured.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::ClientInputs;
    use crate::handler::TASK_INPUT_CAPACITY;
    use crate::handler::cancellation;
    use crate::handler::events::ActiveExecution;
    use crate::handler::protocol::is_terminal_event;
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
        .with_capabilities(AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        })
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
        timeout(Duration::from_secs(2), async {
            while handler.tasks.task(&task.id).active().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task
    }

    use super::*;

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
    async fn test_authorizer_rejects_handler_methods() {
        let handler = DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
            .with_capabilities(a2a::AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            })
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

    /// An omitted optional capability is unavailable, just like explicit false.
    #[tokio::test]
    async fn test_unset_and_disabled_capabilities_are_refused() {
        for declared in [None, Some(false)] {
            let handler = make_handler_with_push_configs().with_capabilities(AgentCapabilities {
                streaming: declared,
                push_notifications: declared,
                extended_agent_card: declared,
                user_artifacts: declared,
                ..Default::default()
            });
            let params = ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])]);
            let streaming = handler
                .send_streaming_message(&params, send_request())
                .await;
            assert!(
                matches!(streaming, Err(error) if error.code == error_code::UNSUPPORTED_OPERATION)
            );
            let subscription = handler
                .subscribe_to_task(
                    &params,
                    SubscribeToTaskRequest {
                        id: "t1".into(),
                        tenant: None,
                        last_seen_generation: None,
                    },
                )
                .await;
            assert!(
                matches!(subscription, Err(error) if error.code == error_code::UNSUPPORTED_OPERATION)
            );
            let native_live = handler
                .send_live_message(&params, Box::pin(futures::stream::empty()))
                .await;
            assert!(
                matches!(native_live, Err(error) if error.code == error_code::UNSUPPORTED_OPERATION)
            );
            let http_live = handler
                .send_live_request(
                    &params,
                    StreamRequest {
                        tenant: None,
                        payload: StreamRequestPayload::SendMessage(send_request()),
                    },
                )
                .await;
            assert!(
                matches!(http_live, Err(error) if error.code == error_code::UNSUPPORTED_OPERATION)
            );
            let mut request = send_request();
            request.artifact_updates.push(TaskArtifactUpdateEvent {
                task_id: String::new(),
                context_id: String::new(),
                generation: 0,
                metadata: None,
                append: None,
                last_chunk: Some(true),
                artifact: serde_json::from_value(
                    serde_json::json!({"artifactId":"client","parts":[{"text":"input"}]}),
                )
                .unwrap(),
            });
            assert_eq!(
                handler
                    .send_message(&params, request)
                    .await
                    .unwrap_err()
                    .code,
                error_code::UNSUPPORTED_OPERATION
            );
            assert_eq!(
                handler
                    .create_push_config(&params, push_config("t1"))
                    .await
                    .unwrap_err()
                    .code,
                error_code::PUSH_NOTIFICATION_NOT_SUPPORTED
            );
            assert_eq!(
                handler
                    .get_extended_agent_card(&params, extended_request())
                    .await
                    .unwrap_err()
                    .code,
                error_code::UNSUPPORTED_OPERATION
            );
            assert_eq!(
                handler.tasks.background_tasks.count(),
                0,
                "capability rejection precedes execution"
            );
            handler.shutdown().await;
        }
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

    /// A subscriber that finds an execution still being prepared waits for it
    /// and then opens with the task, not with whatever comes next.
    #[tokio::test]
    async fn test_subscribe_waits_for_an_execution_that_is_being_prepared() {
        use futures::StreamExt;

        let handler = Arc::new(make_handler());
        seed_task(&handler, "t-prep", "c-prep").await;
        let (active, _inputs, _invocation) = handler.tasks.task("t-prep").reserve().unwrap();
        let _lease = handler.tasks.test_lease("t-prep");
        let (entered, began) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let preparing = {
            let handler = handler.clone();
            let tasks = handler.tasks.clone();
            tokio::spawn(async move {
                tasks
                    .test_update("t-prep", move |_| {
                        Box::pin(async move {
                            entered.send(()).unwrap();
                            released.await.unwrap();
                            active.set_snapshot(handler.tasks.load("t-prep").await.unwrap());
                        })
                    })
                    .await
                    .unwrap();
            })
        };
        began.await.unwrap();
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

        release.send(()).unwrap();
        preparing.await.unwrap();
        let mut stream = subscribing.await.unwrap().unwrap();
        assert!(matches!(
            stream.next().await,
            Some(Ok(StreamResponse::Task(_)))
        ));
    }

    /// A subscriber that finds such a leftover does not wait for it or spin on
    /// it: the task is simply not executing.
    #[tokio::test]
    async fn test_subscribe_ignores_a_dropped_requests_leftover() {
        let handler = make_handler();
        seed_task(&handler, "t-leftover", "c-leftover").await;
        let (_stale, _inputs, _invocation) = handler.tasks.task("t-leftover").reserve().unwrap();

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

        // A stale reservation must not hide a stored, nonterminal task.
        let mut stream = outcome.expect("the idle task is subscribable");
        let Some(Ok(StreamResponse::Task(task))) = stream.next().await else {
            panic!("the subscription opens with its task");
        };
        assert_eq!(task.id, "t-leftover");
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
    async fn test_subscribe_to_terminal_task_is_unsupported_not_not_found() {
        let handler = make_handler();
        handler
            .tasks
            .store()
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
                assert_eq!(task.status.state, TaskState::Submitted);
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

        // Admission can return before the executor's first WORKING event.
        loop {
            match subscription.next().await.unwrap().unwrap() {
                StreamResponse::Task(task) if task.status.state == TaskState::Working => break,
                StreamResponse::Task(task) => assert_eq!(task.status.state, TaskState::Submitted),
                other => panic!("expected task snapshot: {other:?}"),
            }
        }

        release.notify_one();

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
        let mut canceled = canceled;
        canceled.status.timestamp = Some(chrono::Utc::now());
        handler
            .tasks
            .store()
            .create(canceled.clone())
            .await
            .unwrap();
        let active = ActiveExecution::new(Some(canceled.clone()));
        let progress =
            cancellation::Cancellation::new(active.clone(), canceled.clone(), false, false);
        let sequence = active.sequence();
        progress
            .acknowledge(StreamResponse::Task(canceled))
            .unwrap();
        assert_eq!(
            active.sequence(),
            sequence,
            "cleanup acknowledgements are not published"
        );
        assert_eq!(active.snapshot().unwrap().generation, 3);
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
    async fn cancellation_finishes_after_the_caller_is_dropped_following_commit() {
        struct ObservedCancel {
            inner: ResumableExecutor,
            called: Arc<Notify>,
        }
        impl crate::AgentExecutor for ObservedCancel {
            fn execute(
                &self,
                ctx: ExecutorContext,
            ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
                crate::AgentExecutor::execute(&self.inner, ctx)
            }
            fn cancel(
                &self,
                ctx: ExecutorContext,
            ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
                let called = self.called.clone();
                crate::AgentExecutor::cancel(&self.inner, ctx)
                    .inspect(move |_| called.notify_one())
                    .boxed()
            }
        }
        let store = Arc::new(InMemoryTaskStore::new());
        let committed = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let called = Arc::new(Notify::new());
        let handler = DefaultRequestHandler::new(
            ObservedCancel {
                inner: ResumableExecutor {
                    release: Arc::new(Notify::new()),
                },
                called: called.clone(),
            },
            PausingCancelStore {
                inner: store.clone(),
                committed: committed.clone(),
                release: release.clone(),
            },
        )
        .with_capabilities(AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        });
        seed_task(&handler, "detached-cancel", "context").await;
        let mut events = handler
            .send_streaming_message(
                &ServiceParams::new(),
                send_req(Some("detached-cancel"), None),
            )
            .await
            .unwrap();
        assert!(
            matches!(timeout(Duration::from_secs(2), events.next()).await.unwrap(),
            Some(Ok(StreamResponse::Task(task))) if task.status.state == TaskState::Working)
        );
        let active = handler.tasks.task("detached-cancel").active().unwrap();
        let caller = {
            let handler = handler.clone();
            tokio::spawn(async move {
                handler
                    .cancel_task(
                        &ServiceParams::new(),
                        CancelTaskRequest {
                            id: "detached-cancel".into(),
                            metadata: None,
                            tenant: None,
                        },
                    )
                    .await
            })
        };
        timeout(Duration::from_secs(2), committed.notified())
            .await
            .unwrap();
        assert_eq!(
            store
                .get("detached-cancel")
                .await
                .unwrap()
                .unwrap()
                .status
                .state,
            TaskState::Canceled
        );
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        release.notify_one();
        timeout(Duration::from_secs(2), called.notified())
            .await
            .expect("the cancellation hook must still run");
        assert!(active.inputs_drained.is_cancelled());
        let remaining = timeout(Duration::from_secs(2), events.collect::<Vec<_>>())
            .await
            .unwrap();
        assert!(remaining.into_iter().any(|event| match event {
            Ok(StreamResponse::Task(task)) => task.status.state == TaskState::Canceled,
            Ok(StreamResponse::StatusUpdate(update)) => update.status.state == TaskState::Canceled,
            _ => false,
        }));
        assert!(handler.tasks.task("detached-cancel").active().is_none());
    }

    #[tokio::test]
    async fn cancellation_hook_timeout_still_records_the_transition() {
        struct StallingCancel;
        impl crate::AgentExecutor for StallingCancel {
            fn execute(
                &self,
                _: ExecutorContext,
            ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
                stream::empty().boxed()
            }
            fn cancel(
                &self,
                _: ExecutorContext,
            ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
                stream::pending().boxed()
            }
        }
        let handler = DefaultRequestHandler::new(StallingCancel, InMemoryTaskStore::new())
            .with_cancel_hook_timeout(Duration::from_millis(10));
        seed_task(&handler, "timeout-cancel", "context").await;
        // Model a store that changes state without recording the generation.
        let mut canceled = handler.tasks.load("timeout-cancel").await.unwrap();
        canceled.status.state = TaskState::Canceled;
        handler
            .tasks
            .store()
            .update(canceled.clone())
            .await
            .unwrap();
        let inputs = ClientInputs::closed();
        let ctx = ExecutorContext::new("timeout-cancel", "context", inputs)
            .with_stored_task(canceled.clone());
        let _lease = handler.tasks.test_lease("timeout-cancel");
        let progress = cancellation::Cancellation::new(
            ActiveExecution::new(Some(canceled.clone())),
            canceled.clone(),
            false,
            false,
        );
        handler
            .tasks
            .test_update("timeout-cancel", move |state| {
                Box::pin(async move {
                    state.execution = crate::handler::execution::ExecutionState::Canceling {
                        invocation: None,
                        progress: Box::new(progress),
                    };
                })
            })
            .await
            .unwrap();
        let result = timeout(
            Duration::from_secs(2),
            cancellation::run(handler.clone(), ctx),
        )
        .await
        .expect("the hook deadline must bound the wait");
        assert!(
            matches!(result, Err(error) if error.message == "agent cancellation hook timed out")
        );
        let recorded = handler.tasks.load("timeout-cancel").await.unwrap();
        assert_eq!(recorded.status.state, TaskState::Canceled);
        assert_eq!(recorded.generation, canceled.generation + 1);
        assert!(recorded.timeline.unwrap().iter().any(|entry| matches!(
            &entry.content, TimelineContent::Status(status) if status.state == TaskState::Canceled)));
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
        handler.tasks.store().create(task).await.unwrap();
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
        handler.tasks.store().create(task).await.unwrap();

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
        handler.tasks.store().create(task).await.unwrap();
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
        handler.tasks.store().create(task).await.unwrap();
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
                .tasks
                .store()
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
    async fn test_idle_subscription_follows_a_later_turn_to_completion() {
        let handler = make_handler();
        seed_task(&handler, "idle", "context").await;
        let mut task = handler.tasks.load("idle").await.unwrap();
        task.status.state = TaskState::InputRequired;
        handler.tasks.store().update(task).await.unwrap();
        let params = ServiceParams::new();
        let mut events = handler
            .subscribe_to_task(
                &params,
                SubscribeToTaskRequest {
                    id: "idle".into(),
                    tenant: None,
                    last_seen_generation: None,
                },
            )
            .await
            .unwrap();
        let Some(Ok(StreamResponse::Task(opening))) = events.next().await else {
            panic!("an idle subscription opens with its task");
        };
        assert_eq!(opening.id, "idle");
        assert_eq!(opening.status.state, TaskState::InputRequired);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), events.next())
                .await
                .is_err()
        );
        handler
            .send_message(&params, send_req(Some("idle"), None))
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(2), events.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if !is_terminal_event(&event) {
            loop {
                let event = tokio::time::timeout(Duration::from_secs(2), events.next())
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                if is_terminal_event(&event) {
                    break;
                }
            }
        }
        assert!(events.next().await.is_none());
    }

    #[tokio::test]
    async fn test_streaming_send_stays_open_across_input_required_turns() {
        let handler = DefaultRequestHandler::new(MultiTurnExecutor, InMemoryTaskStore::new())
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            });
        let params = ServiceParams::new();
        let mut events = handler
            .send_streaming_message(&params, send_req(None, None))
            .await
            .unwrap();
        let Some(Ok(StreamResponse::Task(opening))) = events.next().await else {
            panic!("the stream opens with a task");
        };
        loop {
            if matches!(events.next().await.unwrap().unwrap(), StreamResponse::StatusUpdate(update)
                if update.status.state == TaskState::InputRequired)
            {
                break;
            }
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(30), events.next())
                .await
                .is_err()
        );
        let mut follow_up = send_req(Some(&opening.id), None);
        follow_up.message.parts = vec![Part::text("done")];
        handler.send_message(&params, follow_up).await.unwrap();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), events.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            if is_terminal_event(&event) {
                break;
            }
        }
        assert!(events.next().await.is_none());
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
            handler
                .tasks
                .store()
                .get("t-absent")
                .await
                .unwrap()
                .is_none(),
            "the rejected id must not have been created"
        );
    }

    #[tokio::test]
    async fn test_send_message_to_terminal_task_is_rejected() {
        install_crypto_provider();
        let handler = make_handler();
        handler
            .tasks
            .store()
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
        let handler = DefaultRequestHandler::new(MultiTurnExecutor, InMemoryTaskStore::new())
            .with_capabilities(a2a::AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            });

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

        let task = handler.tasks.store().get(&task_id).await.unwrap().unwrap();
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
        let handler = DefaultRequestHandler::new(MultiTurnExecutor, InMemoryTaskStore::new())
            .with_capabilities(a2a::AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            });
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
        let task = handler.tasks.store().get("t-infer").await.unwrap().unwrap();
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
            .tasks
            .store()
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
    async fn test_get_task_applies_history_length() {
        let handler = make_handler();
        let params = ServiceParams::new();
        handler
            .tasks
            .store()
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

    #[tokio::test]
    async fn test_list_tasks_page_size_cap_bounds_a_store_that_ignores_it() {
        install_crypto_provider();
        // A store that pays no attention to page_size. Clamping the request
        // does nothing here; only truncating the response bounds it.
        let handler = DefaultRequestHandler::new(
            EchoExecutor,
            PagingTaskStore::new(MAX_PAGE_SIZE * 3, false),
        )
        .with_capabilities(a2a::AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        });

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
        let handler = DefaultRequestHandler::new(EchoExecutor, store).with_capabilities(
            a2a::AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            },
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
        )
        .with_capabilities(a2a::AgentCapabilities {
            streaming: Some(true),
            ..Default::default()
        });

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
        let handler = DefaultRequestHandler::new(EchoExecutor, store).with_capabilities(
            a2a::AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            },
        );

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
                    .tasks
                    .store()
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
            let mut task = handler.tasks.load("idle-cancel").await.unwrap();
            task.status.state = TaskState::InputRequired;
            handler.tasks.store().update(task).await.unwrap();
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
                handler
                    .tasks
                    .load("idle-cancel")
                    .await
                    .unwrap()
                    .status
                    .state,
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
            let mut canceled = handler.tasks.load("legacy-cancel").await.unwrap();
            canceled.status.state = TaskState::Canceled;
            handler
                .tasks
                .store()
                .update(canceled.clone())
                .await
                .unwrap();
            let active = ActiveExecution::new(Some(canceled.clone()));
            let mut progress =
                cancellation::Cancellation::new(active.clone(), canceled.clone(), false, running);
            progress.record_if_missing(&handler).await.unwrap();
            let task = handler.tasks.load("legacy-cancel").await.unwrap();
            assert_eq!(task.generation, canceled.generation + 1);
            assert_eq!(task.status.state, TaskState::Canceled);
            assert_eq!(task.timeline.as_ref().unwrap().len(), 1);
            progress.record_if_missing(&handler).await.unwrap();
            assert_eq!(handler.tasks.load("legacy-cancel").await.unwrap(), task);
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

    #[tokio::test]
    async fn a_full_executor_inbox_declines_input_without_persisting_it() {
        let (handler, release) = make_resumable_handler();
        let params = ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])]);
        let mut events = handler
            .send_streaming_message(&params, send_req(None, None))
            .await
            .unwrap();
        let StreamResponse::Task(task) = events.next().await.unwrap().unwrap() else {
            panic!("expected Task");
        };
        for _ in 0..TASK_INPUT_CAPACITY {
            let request = StreamRequest {
                tenant: None,
                payload: StreamRequestPayload::SendMessage(send_req(
                    Some(&task.id),
                    Some(&task.context_id),
                )),
            };
            assert!(
                handler
                    .receive_message(
                        &params,
                        IncomingRequest::from(request).into_message().unwrap(),
                        StartMode::Bidi,
                        Some(&task.id),
                        None
                    )
                    .await
                    .is_ok()
            );
        }
        let before = handler.tasks.load(&task.id).await.unwrap();
        let request = StreamRequest {
            tenant: None,
            payload: StreamRequestPayload::SendMessage(send_req(
                Some(&task.id),
                Some(&task.context_id),
            )),
        };
        let refusal = handler
            .receive_message(
                &params,
                IncomingRequest::from(request).into_message().unwrap(),
                StartMode::Bidi,
                Some(&task.id),
                None,
            )
            .await
            .err()
            .expect("full inbox must decline input");
        assert!(matches!(refusal, Rejection::Declined(_)));
        assert_eq!(handler.tasks.load(&task.id).await.unwrap(), before);
        release.notify_one();
        timeout(Duration::from_secs(2), events.collect::<Vec<_>>())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn prepared_but_uncreated_tasks_are_not_visible_to_other_requests() {
        let handler = make_handler();
        let _lease = handler.tasks.test_lease("uncreated");
        let owned = handler.clone();
        let (active, _inputs) = handler
            .tasks
            .test_update("uncreated", move |state| {
                Box::pin(async move { state.reserve(&owned, "uncreated").await })
            })
            .await
            .unwrap()
            .unwrap();
        let mut task = task_with_history("uncreated", "context", 0);
        task.status.state = TaskState::Submitted;
        active.set_snapshot(task);
        active
            .uncreated
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let params = ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])]);
        for payload in [
            StreamRequestPayload::SendMessage(send_req(Some("uncreated"), Some("context"))),
            StreamRequestPayload::Subscribe(SubscribeToTaskRequest {
                id: "uncreated".into(),
                tenant: None,
                last_seen_generation: None,
            }),
        ] {
            let request = StreamRequest {
                tenant: None,
                payload,
            };
            let error = handler
                .send_live_message(&params, stream::once(async move { request }).boxed())
                .await
                .err()
                .expect("uncreated tasks must stay private");
            assert_eq!(error.code, error_code::TASK_NOT_FOUND);
        }
        assert!(
            handler
                .tasks
                .store()
                .get("uncreated")
                .await
                .unwrap()
                .is_none()
        );
        handler.tasks.task("uncreated").unregister(&active);
    }

    #[test]
    fn subscriptions_and_unknown_payloads_are_rejected_as_live_follow_ups() {
        for payload in [
            StreamRequestPayload::Subscribe(SubscribeToTaskRequest {
                id: "task".into(),
                tenant: None,
                last_seen_generation: None,
            }),
            StreamRequestPayload::Unrecognised,
        ] {
            let error = IncomingRequest::from(StreamRequest {
                tenant: None,
                payload,
            })
            .into_message()
            .err()
            .expect("not a message");
            assert_eq!(error.code, error_code::INVALID_REQUEST);
        }
    }

    struct RacingStore {
        inner: Arc<InMemoryTaskStore>,
        other_state: TaskState,
        panic_on_get: bool,
    }
    #[async_trait]
    impl TaskStore for RacingStore {
        async fn create(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.inner.create(task).await
        }
        async fn update(&self, task: Task) -> Result<TaskVersion, A2AError> {
            self.inner.update(task).await
        }
        async fn get(&self, id: &str) -> Result<Option<Task>, A2AError> {
            assert!(!self.panic_on_get, "private storage failure");
            self.inner.get(id).await
        }
        async fn list(&self, req: &ListTasksRequest) -> Result<ListTasksResponse, A2AError> {
            self.inner.list(req).await
        }
        async fn begin_cancel(&self, id: &str) -> Result<Task, A2AError> {
            // Model an external writer winning before this process's CAS.
            let mut task = self.inner.get(id).await?.unwrap();
            task.status.state = self.other_state.clone();
            task.status.timestamp = Some(chrono::Utc::now());
            task.generation += 1;
            task.timeline
                .get_or_insert_with(Vec::new)
                .push(TimelineEntry {
                    generation: task.generation,
                    content: TimelineContent::Status(task.status.clone()),
                });
            self.inner.update(task).await?;
            Err(A2AError::task_not_cancelable(id))
        }
    }

    #[tokio::test]
    async fn external_cancel_races_preserve_the_winning_terminal_state() {
        for state in [TaskState::Canceled, TaskState::Completed] {
            let store = Arc::new(InMemoryTaskStore::new());
            let handler = DefaultRequestHandler::new(
                EchoExecutor,
                RacingStore {
                    inner: store.clone(),
                    other_state: state.clone(),
                    panic_on_get: false,
                },
            );
            seed_task(&handler, "race", "context").await;
            let result = handler
                .cancel_task(
                    &ServiceParams::new(),
                    CancelTaskRequest {
                        id: "race".into(),
                        metadata: None,
                        tenant: None,
                    },
                )
                .await;
            if state == TaskState::Canceled {
                assert_eq!(result.unwrap().status.state, state);
            } else {
                assert_eq!(result.unwrap_err().code, error_code::TASK_NOT_CANCELABLE);
            }
            let saved = store.get("race").await.unwrap().unwrap();
            assert_eq!(saved.status.state, state);
            assert_eq!(saved.generation, 1);
        }
    }

    #[tokio::test]
    async fn preparation_panics_release_reservations_and_hide_internal_details() {
        let store = Arc::new(InMemoryTaskStore::new());
        let handler = DefaultRequestHandler::new(
            EchoExecutor,
            RacingStore {
                inner: store.clone(),
                other_state: TaskState::Canceled,
                panic_on_get: true,
            },
        );
        seed_task(&handler, "panic", "context").await;
        let _lease = handler.tasks.test_lease("panic");
        let error = handler
            .send_message(
                &ServiceParams::new(),
                send_req(Some("panic"), Some("context")),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, error_code::INTERNAL_ERROR);
        assert_eq!(error.message, "message preparation failed unexpectedly");
        let idle = handler
            .tasks
            .test_update("panic", |state| {
                Box::pin(async move {
                    matches!(
                        state.execution,
                        crate::handler::execution::ExecutionState::Idle
                    )
                })
            })
            .await
            .unwrap();
        assert!(
            idle,
            "panic recovery resets the resident task state before its next command"
        );
        assert!(handler.tasks.task("panic").active().is_none());
        assert_eq!(
            store.get("panic").await.unwrap().unwrap().status.state,
            TaskState::Submitted
        );
    }

    #[tokio::test]
    async fn a_live_opening_accepts_subscriptions_but_rejects_unknown_request_kinds() {
        let handler = make_handler();
        seed_task(&handler, "task", "context").await;
        let params = ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])]);
        let request = StreamRequest {
            tenant: None,
            payload: StreamRequestPayload::Subscribe(SubscribeToTaskRequest {
                id: "task".into(),
                tenant: None,
                last_seen_generation: None,
            }),
        };
        let before = handler.tasks.load("task").await.unwrap();
        let mut events = handler
            .send_live_message(&params, stream::once(async move { request }).boxed())
            .await
            .unwrap();
        assert!(
            matches!(events.next().await.unwrap().unwrap(), StreamResponse::Task(task) if task == before)
        );
        drop(events);
        assert_eq!(handler.tasks.load("task").await.unwrap(), before);
        let request = StreamRequest {
            tenant: None,
            payload: StreamRequestPayload::Unrecognised,
        };
        let error = handler
            .send_live_message(&params, stream::once(async move { request }).boxed())
            .await
            .err()
            .expect("unknown request kind");
        assert_eq!(error.code, error_code::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn terminal_active_snapshots_cannot_be_resubscribed() {
        let handler = make_handler();
        let task = task_with_history("finished", "context", 0);
        handler.tasks.store().create(task.clone()).await.unwrap();
        let (active, _inputs, _invocation) = handler.tasks.task(&task.id).reserve().unwrap();
        active.set_snapshot(task.clone());
        let error = handler
            .subscribe_to_task(
                &ServiceParams::new(),
                SubscribeToTaskRequest {
                    id: task.id.clone(),
                    tenant: None,
                    last_seen_generation: None,
                },
            )
            .await
            .err()
            .expect("task is terminal");
        assert_eq!(error.code, error_code::UNSUPPORTED_OPERATION);
        handler.tasks.task(&task.id).unregister(&active);
    }

    #[tokio::test]
    async fn artifact_only_inputs_update_storage_without_consuming_executor_inbox_capacity() {
        let (handler, release) = make_resumable_handler();
        let handler = handler.with_capabilities(AgentCapabilities {
            streaming: Some(true),
            user_artifacts: Some(true),
            ..Default::default()
        });
        let params = ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])]);
        let mut events = handler
            .send_streaming_message(&params, send_req(None, None))
            .await
            .unwrap();
        let StreamResponse::Task(task) = events.next().await.unwrap().unwrap() else {
            panic!("expected Task");
        };
        let active = handler.tasks.task(&task.id).active().unwrap();
        let capacity = active.inputs.sender.capacity();
        let mut request = send_req(None, None);
        request.message = Message::new(Role::Unspecified, Vec::new());
        request.message.message_id.clear();
        request.artifact_updates = vec![TaskArtifactUpdateEvent {
            task_id: task.id.clone(),
            context_id: task.context_id.clone(),
            artifact: serde_json::from_value(
                serde_json::json!({"artifactId":"client","parts":[{"text":"input artifact"}]}),
            )
            .unwrap(),
            append: Some(false),
            last_chunk: Some(true),
            metadata: None,
            generation: 0,
        }];
        let before = handler.tasks.load(&task.id).await.unwrap();
        assert!(
            handler
                .receive_message(
                    &params,
                    MessageInput::Raw(request),
                    StartMode::Bidi,
                    Some(&task.id),
                    None
                )
                .await
                .is_ok()
        );
        assert_eq!(active.inputs.sender.capacity(), capacity);
        let stored = handler.tasks.load(&task.id).await.unwrap();
        assert_eq!(stored.history, before.history);
        assert!(
            stored
                .artifacts
                .as_ref()
                .unwrap()
                .iter()
                .any(|artifact| artifact.artifact_id == "client"
                    && artifact.role == Some(Role::User))
        );
        release.notify_one();
        timeout(Duration::from_secs(2), events.collect::<Vec<_>>())
            .await
            .unwrap();
    }

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
        assert!(handler.tasks.watcher_count() == 0);
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
            [Ok(StreamResponse::Task(stored)), Ok(StreamResponse::StatusUpdate(last))]
                if stored.status.state == TaskState::InputRequired
                    && last.status.state == TaskState::Canceled
        ));
        assert!(handler.tasks.watcher_count() == 0);
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
            )
            .with_capabilities(AgentCapabilities {
                streaming: Some(true),
                ..Default::default()
            });
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
                [Ok(StreamResponse::Task(stored)), Ok(StreamResponse::StatusUpdate(last))]
                    if stored.status.state == TaskState::InputRequired
                        && last.status.state == TaskState::Canceled
            ));
            assert!(handler.tasks.watcher_count() == 0);
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

        // Resynchronization may carry the completed state as a snapshot.
        let follows_the_turn = |events: Vec<Result<StreamResponse, A2AError>>| {
            matches!(events.first(), Some(Ok(StreamResponse::Task(stored)))
                if stored.status.state == TaskState::InputRequired)
                && events.iter().all(Result::is_ok)
                && match events.last() {
                    Some(Ok(StreamResponse::Task(task))) => {
                        task.status.state == TaskState::Completed
                    }
                    Some(Ok(StreamResponse::StatusUpdate(update))) => {
                        update.status.state == TaskState::Completed
                    }
                    _ => false,
                }
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
        assert!(matches!(
            subscription.next().await,
            Some(Ok(StreamResponse::Task(_)))
        ));
        assert_eq!(handler.tasks.watcher_count(), 1);
        drop(subscription);
        timeout(Duration::from_secs(2), async {
            while handler.tasks.watcher_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
}

#[cfg(test)]
mod subscription_attachment_tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn standalone_and_live_subscriptions_preserve_resume_cursors_for_active_and_idle_tasks() {
        for live in [false, true] {
            for running in [false, true] {
                for cursor in [None, Some(4), Some(5)] {
                    let handler = make_handler();
                    seed_task(&handler, "task", "context").await;
                    let mut task = handler.tasks.load("task").await.unwrap();
                    task.generation = 5;
                    handler.tasks.store().update(task.clone()).await.unwrap();
                    let execution = if running {
                        let (active, inputs, _invocation) =
                            handler.tasks.task("task").reserve().unwrap();
                        active.set_snapshot(task.clone());
                        Some((active, inputs))
                    } else {
                        None
                    };
                    let request = SubscribeToTaskRequest {
                        id: task.id.clone(),
                        tenant: None,
                        last_seen_generation: cursor,
                    };
                    let params =
                        ServiceParams::from([(SVC_PARAM_VERSION.into(), vec!["1.1".into()])]);
                    // Keep the live uplink open while observing the same Task.
                    let mut events = if live {
                        let opening = StreamRequest {
                            tenant: None,
                            payload: StreamRequestPayload::Subscribe(request),
                        };
                        handler
                            .send_live_message(
                                &params,
                                stream::once(async move { opening })
                                    .chain(stream::pending())
                                    .boxed(),
                            )
                            .await
                            .unwrap()
                    } else {
                        handler.subscribe_to_task(&params, request).await.unwrap()
                    };
                    assert_eq!(
                        handler.tasks.task("task").active().is_some(),
                        running,
                        "subscribing must not start an executor"
                    );
                    if cursor == Some(5) {
                        assert!(
                            timeout(Duration::from_millis(10), events.next())
                                .await
                                .is_err(),
                            "a current cursor must not replay the opening snapshot"
                        );
                    } else {
                        let opening = timeout(Duration::from_secs(2), events.next())
                            .await
                            .unwrap()
                            .unwrap()
                            .unwrap();
                        assert!(
                            matches!(opening, StreamResponse::Task(snapshot) if snapshot == task)
                        );
                    }
                    task.generation = 6;
                    task.status.state = TaskState::Completed;
                    handler.tasks.store().update(task.clone()).await.unwrap();
                    if let Some((active, _inputs)) = execution {
                        active
                            .publish(Ok(&StreamResponse::Task(task.clone())), Some(task.clone()))
                            .await;
                        handler.tasks.task("task").unregister(&active);
                    }
                    if !running {
                        handler
                            .tasks
                            .task("task")
                            .publish(
                                Ok(&StreamResponse::Task(task.clone())),
                                Some(task.clone()),
                                protocol::EventKind::Normal,
                                None,
                                0,
                            )
                            .await;
                    }
                    let remaining = timeout(Duration::from_secs(2), events.collect::<Vec<_>>())
                        .await
                        .unwrap();
                    assert!(
                        matches!(remaining.as_slice(), [Ok(StreamResponse::Task(snapshot))] if snapshot == &task),
                        "the shared runner must deliver completion once: {remaining:?}"
                    );
                }
            }
        }
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
                vec![
                    (None, Ok(status_event(ctx, TaskState::Working, None))),
                    (None, Ok(append)),
                ]
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
                vec![
                    (None, Ok(status_event(ctx, TaskState::Working, None))),
                    (
                        Some(Arc::new(Notify::new())),
                        Ok(status_event(ctx, TaskState::Working, None)),
                    ),
                ]
            }),
            InMemoryTaskStore::new(),
        )
        .with_push_notifications(
            InMemoryPushConfigStore::new(),
            crate::HttpPushSender::new(Some(crate::HttpPushSenderConfig {
                fail_on_error: true,
                validate_urls: false,
                ..Default::default()
            })),
        );
        let mut request = send_request();
        request.configuration = Some(SendMessageConfiguration {
            if_generation_match: None,
            timeline_length: None,
            accepted_output_modes: None,
            // A valid local endpoint whose connection fails during delivery.
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
                let mut events = handler
                    .send_streaming_message(&params, request)
                    .await
                    .unwrap();
                timeout(Duration::from_secs(2), async {
                    while let Some(event) = events.next().await {
                        let state = match event.unwrap() {
                            StreamResponse::Task(task) => Some(task.status.state),
                            StreamResponse::StatusUpdate(update) => Some(update.status.state),
                            _ => None,
                        };
                        if state.is_some_and(|state| {
                            state == TaskState::InputRequired || state.is_terminal()
                        }) {
                            return;
                        }
                    }
                    panic!("the turn must announce its result");
                })
                .await
                .expect("streaming remains open at InputRequired; read the turn result instead");
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
    async fn test_message_after_task_handle_is_an_invalid_agent_response() {
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
    async fn test_an_answering_message_closes_before_later_executor_output() {
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
        assert_eq!(events.len(), 1, "a Message closes its response");
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
}

#[cfg(test)]
mod shutdown_tests {
    use super::*;
    use crate::handler::test_support::*;

    #[tokio::test]
    async fn shutdown_releases_execution_queues_responses_and_http_registrations() {
        let (handler, _release) = make_resumable_handler();
        let request = StreamRequest {
            tenant: None,
            payload: StreamRequestPayload::SendMessage(send_req(None, None)),
        };
        let LiveResponse::Stream(mut events) = handler
            .send_live_request(&ServiceParams::new(), request)
            .await
            .unwrap()
        else {
            panic!("expected live stream");
        };
        let StreamResponse::Task(task) = events.next().await.unwrap().unwrap() else {
            panic!("task");
        };
        let handle = handler.tasks.task(&task.id);
        let active = Arc::downgrade(&handle.active().unwrap());
        let queue = Arc::downgrade(&handle.operations());
        assert!(!handler.http_live_responses.open.read().is_empty());
        timeout(Duration::from_secs(2), handler.shutdown())
            .await
            .unwrap();
        assert!(active.upgrade().is_none());
        assert!(queue.upgrade().is_none());
        assert!(handler.http_live_responses.open.read().is_empty());
        timeout(Duration::from_secs(1), events.collect::<Vec<_>>())
            .await
            .unwrap();
        assert!(
            !handler
                .tasks
                .load(&task.id)
                .await
                .unwrap()
                .status
                .state
                .is_terminal()
        );
        assert!(
            handler
                .send_message(&ServiceParams::new(), send_req(None, None))
                .await
                .is_err()
        );
        handler.clone().shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_interrupts_a_live_opening_waiting_for_its_first_input() {
        let handler = make_handler();
        let entered = Arc::new(Notify::new());
        let signal = entered.clone();
        let input = stream::once(async move {
            signal.notify_one();
            std::future::pending::<StreamRequest>().await
        })
        .boxed();
        let opening = tokio::spawn({
            let handler = handler.clone();
            async move {
                handler
                    .send_live_message(&ServiceParams::new(), input)
                    .await
            }
        });
        entered.notified().await;
        handler.shutdown().await;
        assert!(
            timeout(Duration::from_secs(1), opening)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
    }
}
