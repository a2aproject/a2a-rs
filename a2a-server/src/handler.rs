// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Public request API and the complete default-handler implementation.
//!
//! Private modules own execution state, task mutations, response running,
//! and transport input. All default-handler methods are defined here.

use self::delivery::{
    AuthorizedMessage, Delivery, StartMode, is_artifact_only, record_client_input, request_task_id,
    validate_existing_task,
};
use self::events::{ActiveExecution, ExecutionReceiver, TaskChanges};
use self::exchange::{ExchangeAdapter, OpenExchange};
use self::execution::{drive_execution_guarded, publish_error};
use self::protocol::{
    Rejection, apply_lengths, is_at_least_1_1, is_terminal_event, requested_protocol_version,
    unary_refusal,
};
use self::registry::{ExecutionManager, ReservationGuard};
use self::response::{Run, RunCompletion};
use self::startup::{PreparedResponse, StartedResponse};
use self::subscription::SubscriptionPolicy;
use self::updates::{
    CancelEntry, apply_and_publish, apply_event_to_task, backfill_status_timestamp, save_task,
    validate_event_identity,
};
use self::uplink::{IncomingRequest, LiveSessions, ResponseInput};
use crate::executor::{ClientInput, ClientInputs};
use crate::middleware::{ServiceParams, User};
use crate::pagination::resolve_page_size;
use a2a::*;
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use std::{sync::Arc, time::Duration};
use tokio::sync::mpsc;

mod artifacts;
mod delivery;
mod events;
mod exchange;
mod execution;
mod output;
mod protocol;
mod push_delivery;
mod registry;
mod response;
mod startup;
mod subscription;
mod turns;
mod updates;
mod uplink;

pub use uplink::LiveResponse;

const EXECUTION_BUFFER_CAPACITY: usize = 32;

/// How many requests may queue on a live exchange's uplink.
const LIVE_UPLINK_CAPACITY: usize = 32;

/// How many events may wait before a response runner waits for its consumer.
const RESPONSE_BUFFER_CAPACITY: usize = 32;

/// How many client messages may wait for the executor before it reports busy.
const TASK_INPUT_CAPACITY: usize = 16;

/// How long a long-poll `GetTask` (`currentGeneration`) is held open when the
/// task does not advance. The spec lets a server return earlier with the
/// current state.
const DEFAULT_LONG_POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// Deadline for waiting on the agent cancellation hook; finalization is separate.
const DEFAULT_CANCEL_HOOK_TIMEOUT: Duration = Duration::from_secs(30);

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

    /// `SendLiveMessage` over a transport with native duplex (gRPC): `uplink`
    /// carries the client's requests and the returned stream the task's events,
    /// for as long as the exchange lasts. A request that cannot even open the
    /// exchange is an `Err`; one rejected later is a `StreamError` on the
    /// stream, which stays open.
    ///
    /// Not supported unless the handler overrides it.
    async fn send_live_message(
        &self,
        params: &ServiceParams,
        uplink: BoxStream<'static, StreamRequest>,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let _ = (params, uplink);
        Err(A2AError::unsupported_operation(
            "SendLiveMessage is not supported",
        ))
    }

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

    /// `SendLiveMessage` over a transport that has none (JSON-RPC, REST): each
    /// call carries one request. The call that opens an exchange gets its events
    /// back; a later call for the same task is input to that exchange and gets
    /// only the task as it stands (see [`LiveResponse`]).
    ///
    /// Not supported unless the handler overrides it.
    async fn send_live_request(
        &self,
        params: &ServiceParams,
        request: StreamRequest,
    ) -> Result<LiveResponse, A2AError> {
        let _ = (params, request);
        Err(A2AError::unsupported_operation(
            "SendLiveMessage is not supported",
        ))
    }
}

/// Decides whether a request may proceed.
///
/// The handler never treats a non-empty header as a principal. An
/// implementation must validate whatever credential it accepts (token, DID,
/// session) and return [`A2AError`] when that check fails. `task_id` is set
/// when the method addresses one task and is reserved for owner checks.
pub trait RequestAuthorizer: Send + Sync {
    fn authorize(&self, params: &ServiceParams, task_id: Option<&str>) -> Result<(), A2AError>;

    /// Resolve the trusted caller after authorization. Implementations must
    /// validate credentials or use trusted server state; a header or message
    /// claiming a user name is not an authenticated identity.
    /// The default preserves compatibility with authorization-only hooks.
    fn authenticated_user(&self, _params: &ServiceParams) -> Result<Option<User>, A2AError> {
        Ok(None)
    }
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
///
/// Cheap to clone: everything it holds is shared.
#[derive(Clone)]
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
    changes: Arc<TaskChanges>,
    long_poll_timeout: Duration,
    cancel_hook_timeout: Duration,
    live: Arc<LiveSessions>,
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
    // Configuration.
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
            changes: Arc::new(TaskChanges::default()),
            long_poll_timeout: DEFAULT_LONG_POLL_TIMEOUT,
            cancel_hook_timeout: DEFAULT_CANCEL_HOOK_TIMEOUT,
            live: Arc::new(LiveSessions::default()),
        }
    }

    /// Bounds how long a `GetTask` carrying `currentGeneration` is held open
    /// when the task does not advance. The request then returns the current
    /// state, which the spec allows.
    pub fn with_long_poll_timeout(mut self, timeout: Duration) -> Self {
        self.long_poll_timeout = timeout;
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

    fn authenticated_user(&self, params: &ServiceParams) -> Result<Option<User>, A2AError> {
        self.authorizer
            .as_ref()
            .map_or(Ok(None), |authorizer| authorizer.authenticated_user(params))
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
    /// Optional capabilities are available only when explicitly declared true.
    fn require_capability(
        declared: Option<bool>,
        unsupported: impl FnOnce() -> A2AError,
    ) -> Result<(), A2AError> {
        match declared {
            Some(true) => Ok(()),
            _ => Err(unsupported()),
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

    /// Common entry point for every send: route an existing HTTP exchange,
    /// adapt its input, prepare the first response, then start the shared runner.
    async fn start_response(
        &self,
        params: &ServiceParams,
        mode: StartMode,
        input: ResponseInput,
    ) -> Result<StartedResponse, A2AError> {
        // Check that the agent supports the requested response mode.
        if mode != StartMode::Blocking {
            self.require_streaming()?;
        }
        // Adapt routing and transport input into one exchange or an acknowledgement.
        let mut exchange = match ExchangeAdapter::new(self, params).open(input, mode).await? {
            OpenExchange::Acknowledged(task) => return Ok(StartedResponse::Ack(task)),
            OpenExchange::Attached(exchange) => exchange,
        };
        // Every attached exchange supplies the same normalized requests.
        let first = exchange
            .next()
            .await
            .ok_or_else(|| A2AError::invalid_request("a response needs a first request"))?;
        // Prepare the initial events and the condition that ends this response.
        let wants_message_updates = is_at_least_1_1(&requested_protocol_version(params));
        let response_id = (mode == StartMode::Bidi).then(uuid::Uuid::now_v7);
        // Once preparation begins, its mutations and executor delivery finish
        // even if this caller disconnects while awaiting persistence.
        let handler = self.clone();
        let service_params = params.clone();
        let prepared = tokio::spawn(async move {
            handler
                .prepare_response(
                    &service_params,
                    mode,
                    first.request,
                    wants_message_updates,
                    response_id,
                )
                .await
        })
        .await
        .map_err(|error| {
            tracing::error!(%error, "message preparation failed unexpectedly");
            A2AError::internal("message preparation failed unexpectedly")
        })??;
        // Bind its lifetime to the task, then let the shared runner deliver events.
        exchange.bind(&prepared.task_id);
        let run = Run::new(
            self.clone(),
            params.clone(),
            prepared.task_id.clone(),
            prepared.completion,
        )
        .with_response_id(response_id);
        Ok(StartedResponse::Stream {
            task_id: prepared.task_id,
            events: run.start(
                prepared.opening,
                Some(prepared.events),
                prepared.seen_generation,
                (mode == StartMode::Bidi).then_some(exchange),
            ),
        })
    }

    /// Admit the first message or subscription, then prepare its events and
    /// completion policy without starting a separate execution path per mode.
    async fn prepare_response(
        &self,
        params: &ServiceParams,
        mode: StartMode,
        request: IncomingRequest,
        wants_message_updates: bool,
        response_id: Option<uuid::Uuid>,
    ) -> Result<PreparedResponse, A2AError> {
        match request {
            input @ (IncomingRequest::Authorized(_)
            | IncomingRequest::Raw(StreamRequest {
                payload: StreamRequestPayload::SendMessage(_),
                ..
            })) => {
                let request = input.send_request().expect("matched a message").clone();
                let delivery = self
                    .receive_message(params, input, mode, None, response_id)
                    .await
                    .map_err(|rejection| match mode {
                        StartMode::Bidi => rejection.into_error(),
                        _ => unary_refusal(rejection, &request),
                    })?;
                Ok(PreparedResponse::message(
                    request,
                    delivery,
                    mode,
                    wants_message_updates,
                    response_id,
                ))
            }
            IncomingRequest::Raw(StreamRequest {
                payload: StreamRequestPayload::Subscribe(subscribe),
                ..
            }) if mode == StartMode::Bidi => {
                let events = self
                    .subscribe_stream(params, subscribe.clone(), true, response_id)
                    .await?;
                Ok(PreparedResponse::subscription(subscribe, events))
            }
            _ => Err(A2AError::invalid_request(
                "unrecognised StreamRequest payload",
            )),
        }
    }

    /// Bind and authorize each message once, using its caller rather than
    /// the caller that opened a live exchange. Routed HTTP calls enter here
    /// before enqueueing; other inputs enter from receive_message.
    fn authorize_message(
        &self,
        params: &ServiceParams,
        input: IncomingRequest,
        bound_task: Option<&str>,
    ) -> Result<AuthorizedMessage, Rejection> {
        let (mut request, authorized) = match input {
            IncomingRequest::Authorized(message) => (message.request.clone(), Some(message)),
            IncomingRequest::Raw(StreamRequest {
                payload: StreamRequestPayload::SendMessage(request),
                ..
            }) => (request, None),
            IncomingRequest::Raw(StreamRequest {
                payload: StreamRequestPayload::Subscribe(_),
                ..
            }) => {
                return Err(A2AError::invalid_request(
                    "subscribe is only valid as the first request of a stream",
                )
                .into());
            }
            _ => {
                return Err(A2AError::invalid_request("unrecognised StreamRequest payload").into());
            }
        };
        if let Some(bound) = bound_task {
            match request_task_id(&request) {
                Some(named) if named != bound => {
                    return Err(A2AError::invalid_params(format!(
                        "this stream is bound to task {bound}, not {named}"
                    ))
                    .into());
                }
                Some(_) => {}
                None => request.message.task_id = Some(bound.to_string()),
            }
        }
        if let Some(mut authorized) = authorized {
            authorized.request = request;
            return Ok(authorized);
        }
        self.authorize(params, request_task_id(&request).as_deref())?;
        Ok(AuthorizedMessage {
            request,
            params: params.clone(),
            user: self.authenticated_user(params)?,
            response_id: None,
        })
    }

    /// The message gateway for unary, server-streaming, and every live input.
    async fn receive_message(
        &self,
        params: &ServiceParams,
        input: IncomingRequest,
        mode: StartMode,
        bound_task: Option<&str>,
        response_id: Option<uuid::Uuid>,
    ) -> Result<Delivery, Rejection> {
        let mut message = self.authorize_message(params, input, bound_task)?;
        message.response_id = response_id;
        let request = &message.request;
        self.validate_artifact_updates(request)?;
        if !is_artifact_only(request) {
            self.validate_content_types(&request.message)?;
            if request.message.message_id.is_empty()
                || request.message.parts.is_empty()
                || request.message.role != Role::User
            {
                return Err(A2AError::invalid_params(
                    "client messages require a messageId, user role and at least one part",
                )
                .into());
            }
        } else if request_task_id(request).is_none() {
            return Err(A2AError::invalid_params(
                "artifactUpdates sent without a message must name the task",
            )
            .into());
        }
        self.deliver_message(message, mode).await
    }

    // Delivery.
    /// Hands a message to a task: into the execution that is running, or as the
    /// start of a new turn.
    ///
    /// The choice and what follows from it happen under the task's writer, which
    /// the end of a turn also takes to close its input queue and deregister: the
    /// message finds either a running execution that is taking input, or none.
    async fn deliver_message(
        &self,
        message: AuthorizedMessage,
        mode: StartMode,
    ) -> Result<Delivery, Rejection> {
        // No task named: the message creates one, and there is nothing to join.
        let Some(task_id) = request_task_id(&message.request) else {
            return self.start_delivery(message, mode, None).await;
        };
        let writer = self
            .execution_manager
            .writer_for(&task_id)
            .lock_owned()
            .await;
        // An entry with no snapshot is a request that was dropped before it
        // began; it is no execution.
        let running = match self.execution_manager.get(&task_id).await {
            Some(active) if active.snapshot().await.is_some() => Some(active),
            _ => None,
        };
        match running {
            Some(active) => self.inject(&message, &task_id, &active, writer).await,
            None => self.start_delivery(message, mode, Some(writer)).await,
        }
    }

    async fn start_delivery(
        &self,
        message: AuthorizedMessage,
        mode: StartMode,
        writer: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<Delivery, Rejection> {
        let (task, receiver, recorded) = self
            .start_execution(message, mode, writer)
            .await
            .map_err(Rejection::Failed)?;
        Ok(Delivery::Started {
            task,
            receiver,
            recorded,
        })
    }

    /// Appends the message to the task and passes it to the running executor,
    /// under `writer`, which the caller holds for the task.
    async fn inject(
        &self,
        message: &AuthorizedMessage,
        task_id: &str,
        active: &Arc<ActiveExecution>,
        writer: tokio::sync::OwnedMutexGuard<()>,
    ) -> Result<Delivery, Rejection> {
        let (params, request) = (&message.params, &message.request);
        // A task the executor has not created yet is not one a client can know.
        if active.uncreated.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Rejection::Failed(A2AError::task_not_found(task_id)));
        }
        let Some(base) = active.snapshot().await else {
            return Err(Rejection::Failed(A2AError::internal(
                "a running execution has no task",
            )));
        };
        validate_existing_task(&base, request)?;
        let artifact_only = is_artifact_only(request);
        // Room for the message is secured before anything is changed, so a
        // message the executor cannot take is declined with nothing recorded.
        let permit = if artifact_only {
            None
        } else {
            Some(active.inputs.sender.clone().try_reserve_owned().map_err(
                |refusal| match refusal {
                    mpsc::error::TrySendError::Full(_) => {
                        Rejection::Declined("the agent is busy with earlier input".to_string())
                    }
                    // The queue is closed only together with the execution being
                    // deregistered, under the writer held here.
                    mpsc::error::TrySendError::Closed(_) => Rejection::Failed(A2AError::internal(
                        "the input queue of a registered execution is closed",
                    )),
                },
            )?)
        };

        let mut task = base;
        let recorded = record_client_input(&mut task, request, false)?;
        let events = recorded.events;
        // Admit configuration before committing input: a rejected configuration
        // must not leave a recorded message that no executor will receive.
        self.save_request_push_config(params, task_id, request)
            .await?;
        let task = save_task(self.task_store.as_ref(), task).await?;

        let sequence_before = active.sequence().await;
        let receiver = active.subscribe();
        let last = events.len().saturating_sub(1);
        for (index, event) in events.iter().enumerate() {
            let snapshot = (index == last).then(|| task.clone());
            active.publish_marked(Ok(event), snapshot, false).await;
        }
        if events.is_empty() {
            active.set_snapshot(task.clone()).await;
        }
        let sequence_after = active.sequence().await;
        if let Some(permit) = permit {
            let input = ClientInput::received(
                request,
                recorded
                    .message_generation
                    .expect("a reserved input has a message"),
                message.user.clone(),
                params,
                message.response_id,
            );
            permit.send(active.track_input(input).await);
        }
        // The webhooks are queued under the writer, in publish order, and never
        // waited for.
        for event in &events {
            self.execution_manager
                .pushes
                .push(self, task_id, None, event);
        }
        drop(writer);
        self.changes.notify(task_id);

        Ok(Delivery::Injected {
            task,
            receiver,
            sequence_before,
            sequence_after,
        })
    }

    // Subscriptions.
    /// `SubscribeToTask`, for an ordinary stream or (`live`) for the downlink of
    /// a live exchange, which is also told what only a live exchange is.
    async fn subscribe_stream(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
        live: bool,
        response_id: Option<uuid::Uuid>,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.authorize(params, Some(&req.id))?;
        self.require_streaming()?;
        // An execution that is still being prepared has no snapshot to open
        // with yet. Wait for the writer -- the request preparing it holds it --
        // and look again, so the stream opens with the task, or, if the
        // request failed and released the task, falls through to the checks
        // below. The wait follows the lookup that found the execution, not one
        // before it: a registration made in between is waited for too.
        // A task the executor has not created yet is not one a client can know.
        if let Some(active) = self.execution_manager.get(&req.id).await {
            if active.uncreated.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(A2AError::task_not_found(&req.id));
            }
        }
        let mut resubscribed = self.execution_manager.resubscribe(&req.id).await;
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
            if snapshot_task
                .as_ref()
                .is_some_and(|task| task.status.state.is_terminal())
            {
                return Err(A2AError::unsupported_operation(format!(
                    "task {} has already reached a terminal state",
                    req.id
                )));
            }
            // A client that already holds the task's current generation has
            // missed nothing, so it resumes without a snapshot. One that is
            // behind cannot be replayed to -- events are not retained -- and
            // falls back to a snapshot, which the spec allows.
            let snapshot_task = match (req.last_seen_generation, snapshot_task) {
                (Some(last_seen), Some(task))
                    if task.generation == last_seen && !task.status.state.is_terminal() =>
                {
                    None
                }
                (_, snapshot) => snapshot,
            };
            let mode = if live {
                StartMode::Bidi
            } else {
                StartMode::Streaming
            };
            return Ok(SubscriptionPolicy::new(
                mode,
                is_at_least_1_1(&requested_protocol_version(params)),
            )
            .for_response(response_id)
            .stream(receiver, snapshot_task, sequence));
        }

        // A registry miss also happens once a task finishes and its
        // execution is cleaned up, so it doesn't by itself mean the task
        // never existed -- check the store to tell the two apart before
        // reporting TaskNotFoundError.
        if let Some(task) = self.task_store.get(&req.id).await? {
            if task.status.state.is_terminal() {
                return Err(A2AError::unsupported_operation(format!(
                    "task {} has already reached a terminal state",
                    req.id
                )));
            }
            let current = req.last_seen_generation == Some(task.generation);
            return Ok(if current {
                futures::stream::empty().boxed()
            } else {
                futures::stream::once(async move { Ok(StreamResponse::Task(task)) }).boxed()
            });
        }
        Err(A2AError::task_not_found(&req.id))
    }

    /// Long-poll for `GetTaskRequest.currentGeneration` (A2A 1.1 §3.2.7):
    /// returns once the task's generation exceeds `current`, or with the
    /// current state when the timeout elapses or the task is terminal.
    async fn wait_for_generation(&self, task_id: &str, current: i64) -> Result<Task, A2AError> {
        let deadline = tokio::time::Instant::now() + self.long_poll_timeout;
        // Watching before the store is read: a change landing in between is seen.
        let mut watch = self.changes.watch(task_id);
        loop {
            let task = self.load_task(task_id).await?;
            if task.generation > current || task.status.state.is_terminal() {
                return Ok(task);
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Ok(task);
            }
            tokio::select! {
                () = watch.changed() => {}
                _ = tokio::time::sleep((deadline - now).min(LONG_POLL_RECHECK)) => {}
            }
        }
    }

    async fn load_task(&self, task_id: &str) -> Result<Task, A2AError> {
        self.task_store
            .get(task_id)
            .await?
            .ok_or_else(|| A2AError::task_not_found(task_id))
    }

    // Preparation.
    /// Applies the request to a prepared snapshot, returning the
    /// task, whether it existed before, and its context id.
    ///
    /// The caller holds the task's execution reservation and its writer lock,
    /// so nothing else -- another request, a cancel -- can change it between
    /// the read, the `ifGenerationMatch` comparison and the save below.
    async fn prepare_task_for_execution(
        &self,
        req: &SendMessageRequest,
        task_id: &str,
    ) -> Result<(Task, bool, String, Vec<StreamResponse>), A2AError> {
        let artifact_only = is_artifact_only(req);
        // A2A §3.4.2: a client-provided taskId MUST reference an existing task,
        // and client-provided ids for *creating* tasks are not supported. Only
        // an absent taskId may mint a new one.
        let stored = self.task_store.get(task_id).await?;
        if stored.is_none() && (req.message.task_id.is_some() || artifact_only) {
            return Err(A2AError::task_not_found(task_id));
        }
        if let Some(task) = &stored {
            validate_existing_task(task, req)?;
        }
        let context_id = stored
            .as_ref()
            .map(|task| task.context_id.clone())
            .unwrap_or_else(|| {
                req.message
                    .context_id
                    .clone()
                    .unwrap_or_else(new_context_id)
            });

        let existed = stored.is_some();
        let events;
        let task = if let Some(mut existing) = stored {
            events = record_client_input(&mut existing, req, false)?.events;
            existing
        } else {
            let mut task = Task {
                generation: 0,
                timeline: None,
                id: task_id.to_string(),
                context_id: context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Submitted,
                    message: None,
                    timestamp: Some(chrono::Utc::now()),
                },
                artifacts: None,
                history: None,
                metadata: None,
            };
            events = record_client_input(&mut task, req, true)?.events;
            task
        };

        Ok((task, existed, context_id, events))
    }

    async fn start_execution(
        &self,
        message: AuthorizedMessage,
        mode: StartMode,
        writer: Option<tokio::sync::OwnedMutexGuard<()>>,
    ) -> Result<(Task, ExecutionReceiver, Vec<StreamResponse>), A2AError> {
        let AuthorizedMessage {
            request: req,
            params,
            user,
            ..
        } = message;
        let params = &params;
        let include_task_snapshot_in_context = mode == StartMode::Blocking;
        // A new task is created by the executor: only a live exchange, which is
        // bound to its task from the first request, creates it up front.
        let lazy = false;
        let artifact_only = is_artifact_only(&req);
        let task_id = request_task_id(&req).unwrap_or_else(new_task_id);

        // Claim the task before reading it: from here until the execution
        // ends, this request and a cancel are the only writers. The guard gives the claim back
        // on every early return and if this future is dropped.
        let (active, inputs, writer_guard) = match writer {
            Some(held) => self.execution_manager.reserve_held(&task_id, held).await?,
            None => self.execution_manager.reserve(&task_id).await?,
        };
        let reservation = ReservationGuard::new(
            Arc::clone(&self.execution_manager),
            task_id.clone(),
            Arc::clone(&active),
        );
        let (task, existed, context_id, input_events) = {
            // The writer, held since the reservation until the snapshot below
            // is set, makes a cancel or a subscriber that finds this execution
            // wait for it instead of racing it.
            let prepared = async {
                let prepared = self.prepare_task_for_execution(&req, &task_id).await?;
                if lazy && !prepared.1 {
                    // For a task that may never exist, the configuration waits
                    // for the executor to create it.
                    let config = self.request_push_config(params, &task_id, &req)?;
                    *active
                        .pending_push
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
                } else {
                    self.save_request_push_config(params, &task_id, &req)
                        .await?;
                }
                // All admission checks and configuration saves precede the
                // input commit. The owned preparation then starts its executor.
                if prepared.1 {
                    self.task_store.update(prepared.0.clone()).await?;
                } else if !lazy {
                    self.task_store.create(prepared.0.clone()).await?;
                }
                Ok::<_, A2AError>(prepared)
            }
            .await;
            match prepared {
                Ok(prepared) => {
                    active.set_snapshot(prepared.0.clone()).await;
                    if lazy && !prepared.1 {
                        active
                            .uncreated
                            .store(true, std::sync::atomic::Ordering::SeqCst);
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
        self.changes.notify(&task_id);
        // The task input queue is owned by the server and already exists
        // before the writer is let go, so every channel can feed this turn.
        // Subscribed before the writer is let go: nothing published after that
        // can be missed, a cancel that lands straight away included.
        let receiver = active.subscribe();
        drop(writer_guard);

        let runtime = self.clone();
        let stored_task = (existed || include_task_snapshot_in_context).then(|| task.clone());
        let service_params = params.clone();
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
            let exec_ctx = crate::ExecutorContext {
                execution_id: active_execution.execution_id.clone(),
                message: (!artifact_only).then_some(req.message),
                task_id: execution_task_id.clone(),
                // The task as it stands once the request is applied. A task this
                // request created is withheld unless the caller asked for it
                // (`send_message` does, streaming does not): executors use its
                // absence to recognise a new task.
                stored_task,
                context_id,
                metadata: req.metadata,
                user,
                service_params,
                tenant: req.tenant,
                inputs,
                new_task: !existed,
            };
            drive_execution_guarded(runtime, execution_task_id, active_execution, exec_ctx).await;
            // Reached only if the executor did not panic: the execution has
            // already released the task itself.
            release.disarm();
        });

        Ok((task, receiver, input_events))
    }

    // Push delivery.
    /// The push configuration the request carries, for `task_id`.
    fn request_push_config(
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
        self.push_config_store_checked()?;
        config.task_id = task_id.to_string();
        if config.tenant.is_none() {
            config.tenant = req.tenant.clone();
        }
        config
            .protocol_version
            .get_or_insert_with(|| requested_protocol_version(params));
        if let Some(sender) = &self.push_sender {
            sender.validate_config(&config)?;
        }
        Ok(Some(config))
    }

    async fn save_request_push_config(
        &self,
        params: &ServiceParams,
        task_id: &str,
        req: &SendMessageRequest,
    ) -> Result<(), A2AError> {
        if let Some(config) = self.request_push_config(params, task_id, req)? {
            self.push_config_store_checked()?.save(config).await?;
        }
        Ok(())
    }

    // Cancellation.
    /// Complete an authorized cancellation independently of the request
    /// future, including executor shutdown and cancellation event finalization.
    async fn cancel_task_owned(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
        user: Option<User>,
    ) -> Result<Task, A2AError> {
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
        if let Some(active) = &active_execution {
            // Stop polling the executor and let its runner recover unread
            // inputs. Wait only after releasing the writer: the runner needs
            // it to publish input rejections before the cancellation event.
            active.set_snapshot(task.clone()).await;
            active.inputs.close();
        }
        drop(transition);
        if let Some(active) = &active_execution {
            active.inputs_drained.cancelled().await;
        }
        // The task changed under every long-poll waiting on it.
        self.changes.notify(&req.id);

        let cancel_inputs = ClientInputs::closed();
        let exec_ctx = crate::ExecutorContext {
            execution_id: active_execution.as_ref().map_or_else(
                || uuid::Uuid::now_v7().to_string(),
                |active| active.execution_id.clone(),
            ),
            message: None,
            task_id: req.id.clone(),
            stored_task: Some(task.clone()),
            context_id: task.context_id.clone(),
            metadata: req.metadata,
            user,
            service_params: params.clone(),
            tenant: req.tenant,
            inputs: cancel_inputs,
            new_task: false,
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

        self.load_task(&req.id).await
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
        let deadline = tokio::time::Instant::now() + self.cancel_hook_timeout;
        loop {
            let event = match tokio::time::timeout_at(deadline, stream.next()).await {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(_) => {
                    failure = Some(A2AError::internal("agent cancellation hook timed out"));
                    break;
                }
            };
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
            self.execution_manager
                .pushes
                .push(self, task_id, None, &event);
            if is_terminal_event(&event) {
                break;
            }
        }

        // Release the hook before finalization; the deadline does not wrap
        // persistence or publication of the cancellation transition.
        drop(stream);
        let recorded = if entry.recorded_by_store {
            self.announce_cancel_if_missing(task_id, active, &entry, &current_task)
                .await
        } else {
            self.record_cancel_if_missing(task_id, canceled, active, &mut current_task)
                .await
        };
        // The recorded transition is announced like any other, even when the
        // cancel itself is about to be reported as failed.
        if let Ok(Some(event)) = &recorded {
            self.execution_manager
                .pushes
                .push(self, task_id, None, event);
        }
        if let Some(error) = failure {
            if let Some(active) = active {
                publish_error(active, &error).await;
            }
            return Err(error);
        }
        recorded?;
        Ok(())
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
        self.changes.notify_event(event);
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
            self.changes.notify_event(&event);
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
        self.changes.notify_event(&event);
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
        self.changes.notify_event(&event);
        Ok(Some(event))
    }
}

#[async_trait]
impl RequestHandler for DefaultRequestHandler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
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
        let (task_id, events) = self
            .start_response(params, StartMode::Blocking, ResponseInput::message(req))
            .await?
            .into_stream()?;
        Run::unary_response(
            self.clone(),
            task_id,
            events,
            history_length,
            timeline_length,
        )
        .await
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let (_, events) = self
            .start_response(params, StartMode::Streaming, ResponseInput::message(req))
            .await?
            .into_stream()?;
        Ok(events)
    }

    async fn send_live_message(
        &self,
        params: &ServiceParams,
        uplink: BoxStream<'static, StreamRequest>,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let (_, events) = self
            .start_response(params, StartMode::Bidi, ResponseInput::Stream(uplink))
            .await?
            .into_stream()?;
        Ok(events)
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
        }
        Ok(response)
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        self.authorize(params, Some(&req.id))?;

        // The runtime owns cancellation after authorization. Dropping this
        // caller's JoinHandle only stops waiting; the mutation still finishes.
        let handler = self.clone();
        let params = params.clone();
        let task_id = req.id.clone();
        let user = self.authenticated_user(&params)?;
        tokio::spawn(async move { handler.cancel_task_owned(&params, req, user).await })
            .await
            .map_err(|error| {
                tracing::error!(%task_id, %error, "cancellation operation failed");
                A2AError::internal("cancellation operation failed unexpectedly")
            })?
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let task_id = req.id.clone();
        let seen_generation = req.last_seen_generation.unwrap_or(0);
        let events = self.subscribe_stream(params, req, false, None).await?;
        Ok(
            Run::new(self.clone(), params.clone(), task_id, RunCompletion::Task).start(
                None,
                Some(events),
                seen_generation,
                None,
            ),
        )
    }

    async fn send_live_request(
        &self,
        params: &ServiceParams,
        request: StreamRequest,
    ) -> Result<LiveResponse, A2AError> {
        match self
            .start_response(
                params,
                StartMode::Bidi,
                ResponseInput::Request(Box::new(request)),
            )
            .await?
        {
            StartedResponse::Stream { events, .. } => Ok(LiveResponse::Stream(events)),
            StartedResponse::Ack(task) => Ok(LiveResponse::Ack(task)),
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
mod test_support;

#[cfg(test)]
mod tests {
    use super::test_support::*;
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
    async fn test_undeclared_capabilities_are_refused() {
        let handler =
            make_handler_with_push_configs().with_capabilities(AgentCapabilities::default());
        let params = ServiceParams::new();
        assert!(
            matches!(handler.send_streaming_message(&params, send_request()).await,
            Err(error) if error.code == error_code::UNSUPPORTED_OPERATION)
        );
        assert!(
            matches!(handler.subscribe_to_task(&params, SubscribeToTaskRequest {
            id: "t1".into(), tenant: None, last_seen_generation: None,
        }).await, Err(error) if error.code == error_code::UNSUPPORTED_OPERATION)
        );
        assert!(
            matches!(handler.send_live_message(&params, Box::pin(futures::stream::empty())).await,
            Err(error) if error.code == error_code::UNSUPPORTED_OPERATION)
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
        let (active, _inputs, guard) = handler.execution_manager.reserve("t-prep").await.unwrap();

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

    /// A subscriber that finds such a leftover does not wait for it or spin on
    /// it: the task is simply not executing.
    #[tokio::test]
    async fn test_subscribe_ignores_a_dropped_requests_leftover() {
        let handler = make_handler();
        seed_task(&handler, "t-leftover", "c-leftover").await;
        let (_stale, _inputs, guard) = handler
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
        let active = handler
            .execution_manager
            .get("detached-cancel")
            .await
            .unwrap();
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
        assert!(
            handler
                .execution_manager
                .get("detached-cancel")
                .await
                .is_none()
        );
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
        let mut canceled = handler.load_task("timeout-cancel").await.unwrap();
        canceled.status.state = TaskState::Canceled;
        handler.task_store.update(canceled.clone()).await.unwrap();
        let inputs = ClientInputs::closed();
        let ctx = ExecutorContext::new("timeout-cancel", "context", inputs)
            .with_stored_task(canceled.clone());
        let result = timeout(
            Duration::from_secs(2),
            handler.run_cancel("timeout-cancel", &canceled, false, None, ctx),
        )
        .await
        .expect("the hook deadline must bound the wait");
        assert!(
            matches!(result, Err(error) if error.message == "agent cancellation hook timed out")
        );
        let recorded = handler.load_task("timeout-cancel").await.unwrap();
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
    async fn test_idle_subscription_follows_a_later_turn_to_completion() {
        let handler = make_handler();
        seed_task(&handler, "idle", "context").await;
        let mut task = handler.load_task("idle").await.unwrap();
        task.status.state = TaskState::InputRequired;
        handler.task_store.update(task).await.unwrap();
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
}
