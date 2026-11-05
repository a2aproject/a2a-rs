// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Shared A2A rules, independent of storage, Tokio tasks and transport I/O.
//!
//! Read this module by operation, in the same order as the sections below:
//! - All operations: [`RequestRules`] checks versions, extensions and capabilities.
//! - SendMessage / SendStreamingMessage / SendLiveMessage: [`IncomingRequest`]
//!   classifies input, [`ExecutionProtocol`] validates executor output, and
//!   [`ResponsePolicy`] chooses opening events, completion and opening errors.
//!   [`Rejection`] defines refusals of individual live inputs.
//! - SubscribeToTask: [`subscription_plan`] chooses a snapshot or cursor resume;
//!   subscriptions share [`EventFilter`] with send responses and create no tasks.
//! - CancelTask: [`CancelEntry`] plans cancellation and its committed announcement.
//! - Task updates: [`record_client_input`] and [`apply_task_mutation`] enforce
//!   identity, generations, timeline order and artifact ownership.
//! - Reads: [`apply_lengths`] trims task responses; [`push_config_page_offset`]
//!   parses push configuration cursors (task listing cursors belong to the store).
//!
//! Rules return their own validation errors. [`ProtocolError`] defines failures
//! detected by runtime modules; application and store errors propagate unchanged.
//! The handler authorizes and calls these rules; runtime modules perform storage,
//! execution and delivery at their existing atomic boundaries. Transport bindings
//! encode the resulting responses and errors.

use super::delivery::MessageInput;
use crate::middleware::ServiceParams;
use crate::task_store::{apply_history_length, apply_timeline_length};
use a2a::*;

// Shared request validation: versions, extensions and advertised capabilities.

pub(crate) fn validate_protocol_version(params: &ServiceParams) -> Result<(), A2AError> {
    let mut values = params
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("a2a-version"))
        .flat_map(|(_, values)| values);
    let requested = values.next().map(String::as_str).unwrap_or("0.3");
    if values.next().is_some() {
        return Err(A2AError::version_not_supported(requested));
    }
    let parts: Vec<_> = requested.trim().split('.').collect();
    let numeric = (2..=3).contains(&parts.len())
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && part.parse::<u32>().is_ok()
        });
    if numeric && parts[0].parse::<u32>() == Ok(1) {
        Ok(())
    } else {
        Err(A2AError::version_not_supported(requested))
    }
}

/// The protocol version a request announced through `A2A-Version`, or "1.0"
/// when it sent none.
pub(super) fn requested_protocol_version(params: &ServiceParams) -> String {
    params
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(SVC_PARAM_VERSION))
        .and_then(|(_, values)| values.first())
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .unwrap_or("1.0")
        .to_string()
}

/// Whether `version` (an `A2A-Version` value) is 1.1 or later within 1.x.
pub(super) fn is_at_least_1_1(version: &str) -> bool {
    let mut parts = version.trim().split('.');
    let major = parts.next().and_then(|part| part.parse::<u32>().ok());
    let minor = parts
        .next()
        .map_or(Some(0), |part| part.parse::<u32>().ok());
    matches!((major, minor), (Some(1), Some(minor)) if minor >= 1)
}

pub(super) struct RequestRules<'a> {
    capabilities: &'a AgentCapabilities,
    input_modes: &'a [String],
}

impl<'a> RequestRules<'a> {
    pub(super) fn new(capabilities: &'a AgentCapabilities, input_modes: &'a [String]) -> Self {
        Self {
            capabilities,
            input_modes,
        }
    }

    /// Called after the application's authorization callback, for every operation.
    pub(super) fn validate_operation(&self, params: &ServiceParams) -> Result<(), A2AError> {
        if params
            .keys()
            .any(|name| name.eq_ignore_ascii_case(SVC_PARAM_VERSION))
        {
            validate_protocol_version(params)?;
        }
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

    pub(super) fn require_streaming(&self) -> Result<(), A2AError> {
        require_capability(self.capabilities.streaming, || {
            A2AError::unsupported_operation("the agent card does not declare streaming")
        })
    }
    pub(super) fn require_push_notifications(&self) -> Result<(), A2AError> {
        require_capability(self.capabilities.push_notifications, || {
            ProtocolError::PushUnsupported.into()
        })
    }
    pub(super) fn require_extended_agent_card(&self) -> Result<(), A2AError> {
        require_capability(self.capabilities.extended_agent_card, || {
            A2AError::unsupported_operation(
                "the agent card does not declare an extended agent card",
            )
        })
    }

    pub(super) fn validate_message(&self, request: &SendMessageRequest) -> Result<(), A2AError> {
        if !request.artifact_updates.is_empty() && self.capabilities.user_artifacts != Some(true) {
            return Err(A2AError::unsupported_operation(
                "the agent card does not declare userArtifacts",
            ));
        }
        if is_artifact_only(request) {
            return request_task_id(request).map(|_| ()).ok_or_else(|| {
                A2AError::invalid_params(
                    "artifactUpdates sent without a message must name the task",
                )
            });
        }
        let message = &request.message;
        if !self.input_modes.is_empty()
            && !message.parts.iter().all(|part| {
                part.media_type.as_deref().is_none_or(|media_type| {
                    media_type.is_empty() || self.input_modes.iter().any(|mode| mode == media_type)
                })
            })
        {
            return Err(A2AError::content_type_not_supported());
        }
        if message.message_id.is_empty() || message.parts.is_empty() || message.role != Role::User {
            return Err(A2AError::invalid_params(
                "client messages require a messageId, user role and at least one part",
            ));
        }
        Ok(())
    }
}

fn require_capability(
    declared: Option<bool>,
    unsupported: impl FnOnce() -> A2AError,
) -> Result<(), A2AError> {
    match declared {
        Some(true) => Ok(()),
        _ => Err(unsupported()),
    }
}

// Sending messages: task binding, stream openings, completion and refusals.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StartMode {
    /// `SendMessage`, with completion controlled by its request configuration.
    Blocking,
    /// `SendStreamingMessage`, with server output only.
    Streaming,
    /// `SendLiveMessage`, with an uplink and downlink bound to one task.
    Bidi,
}

/// A request that carries only `artifactUpdates` for an existing task. The
/// native `message` is not optional, so the wire's absent message arrives as
/// an empty one with no id (see the protobuf conversion); a message that has
/// an id, a body or a task of its own is never mistaken for it.
pub(super) fn is_artifact_only(req: &SendMessageRequest) -> bool {
    !req.artifact_updates.is_empty()
        && req.message.role == Role::Unspecified
        && req.message.message_id.is_empty()
        && req.message.parts.is_empty()
        && req.message.task_id.is_none()
        && req.message.context_id.is_none()
        && req.message.metadata.is_none()
        && req.message.extensions.is_none()
        && req.message.reference_task_ids.is_none()
}

/// The task a send request names: the message's, or for an artifact-only
/// request the one its updates target.
pub(super) fn request_task_id(req: &SendMessageRequest) -> Option<String> {
    if is_artifact_only(req) {
        req.artifact_updates
            .iter()
            .map(|update| update.task_id.as_str())
            .find(|task_id| !task_id.is_empty())
            .map(str::to_string)
    } else {
        req.message.task_id.clone()
    }
}

/// Resolve task identity before checking the application's authorization policy.
pub(super) fn bind_request_task(
    request: &mut SendMessageRequest,
    bound: &str,
) -> Result<(), A2AError> {
    match request_task_id(request) {
        Some(named) if named != bound => Err(A2AError::invalid_params(format!(
            "this stream is bound to task {bound}, not {named}"
        ))),
        Some(_) => Ok(()),
        None => {
            request.message.task_id = Some(bound.to_owned());
            Ok(())
        }
    }
}

/// Called against storage inside serialized admission, never against an earlier read.
pub(super) fn validate_task_reference(
    stored: Option<&Task>,
    request: &SendMessageRequest,
    task_id: &str,
) -> Result<(), A2AError> {
    match stored {
        None if request.message.task_id.is_some() || is_artifact_only(request) => {
            Err(A2AError::task_not_found(task_id))
        }
        Some(task) => validate_existing_task(task, request),
        None => Ok(()),
    }
}

/// Checks against the authoritative task while its writer is held, whether
/// the request joins a running turn or starts a continuation.
pub(super) fn validate_existing_task(
    task: &Task,
    req: &SendMessageRequest,
) -> Result<(), A2AError> {
    if task.status.state.is_terminal() {
        return Err(ProtocolError::TaskTerminated(&task.id).into());
    }
    if let Some(expected) = req
        .configuration
        .as_ref()
        .and_then(|c| c.if_generation_match)
    {
        if task.generation != expected {
            return Err(A2AError::task_generation_mismatch(
                &task.id,
                expected,
                task.generation,
            ));
        }
    }
    if let Some(requested) = req.message.context_id.as_deref() {
        if requested != task.context_id {
            return Err(A2AError::invalid_params(format!(
                "contextId {requested} does not match task {}, which belongs to context {}",
                task.id, task.context_id
            )));
        }
    }
    Ok(())
}

/// Protocol payload classified once at the native-stream or HTTP boundary.
pub(super) enum IncomingRequest {
    Message(Box<MessageInput>),
    Subscribe(SubscribeToTaskRequest),
    Invalid,
}

impl From<StreamRequest> for IncomingRequest {
    fn from(request: StreamRequest) -> Self {
        match request.payload {
            StreamRequestPayload::SendMessage(request) => {
                Self::Message(Box::new(MessageInput::Raw(request)))
            }
            StreamRequestPayload::Subscribe(request) => Self::Subscribe(request),
            _ => Self::Invalid,
        }
    }
}

impl IncomingRequest {
    /// Only the opening accepts subscriptions; follow-ups must be messages.
    pub(super) fn into_message(self) -> Result<MessageInput, A2AError> {
        match self {
            Self::Message(message) => Ok(*message),
            Self::Subscribe(_) => Err(ProtocolError::SubscriptionAfterOpening.into()),
            Self::Invalid => Err(ProtocolError::UnknownStreamRequest.into()),
        }
    }

    #[cfg(test)]
    pub(super) fn message(request: SendMessageRequest) -> Self {
        Self::Message(Box::new(MessageInput::Raw(request)))
    }

    pub(super) fn reference_id(&self) -> Option<String> {
        let message = match self {
            Self::Message(input) => input.request(),
            Self::Subscribe(subscribe) => return Some(subscribe.id.clone()),
            Self::Invalid => return None,
        };
        Some(message.message.message_id.clone())
            .filter(|id| !id.is_empty())
            .or_else(|| {
                message
                    .artifact_updates
                    .first()
                    .map(|update| update.artifact.artifact_id.clone())
            })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OutputKind {
    Reply,
    Update,
    Terminal,
}

pub(super) fn classify_output(event: &StreamResponse) -> OutputKind {
    match event {
        StreamResponse::Message(_) => OutputKind::Reply,
        StreamResponse::Task(task) if task.status.state.is_terminal() => OutputKind::Terminal,
        StreamResponse::StatusUpdate(update) if update.status.state.is_terminal() => {
            OutputKind::Terminal
        }
        _ => OutputKind::Update,
    }
}

#[derive(Clone, Copy, Default)]
enum ExecutionPhase {
    #[default]
    Initial,
    Task,
    Replied,
}

#[derive(Default)]
pub(super) struct ExecutionProtocol {
    phase: ExecutionPhase,
}

impl ExecutionProtocol {
    /// Validate one invocation's output before it reaches task storage or clients.
    pub(super) fn accept(&mut self, event: &StreamResponse) -> Result<OutputKind, A2AError> {
        let kind = classify_output(event);
        self.phase = match (self.phase, kind) {
            (ExecutionPhase::Replied, _) | (ExecutionPhase::Task, OutputKind::Reply) => {
                return Err(A2AError::invalid_agent_response());
            }
            (_, OutputKind::Reply) => ExecutionPhase::Replied,
            _ => ExecutionPhase::Task,
        };
        Ok(kind)
    }
}

/// How an event is meant to be delivered.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EventKind {
    Normal,
    /// A `Task` snapshot that changed nothing. It sits at a generation that was
    /// already announced, so streams meant for clients leave it out; the
    /// blocking send, which only needs to know the executor spoke, keeps it.
    Repeated,
    /// Something only a live exchange is told (a `StreamError` about a message
    /// of its own), not the other subscribers of the task.
    LiveOnly(Option<uuid::Uuid>),
    /// The executor's direct reply, with no task: the last thing any stream
    /// following the execution is sent.
    Reply,
}

/// Request options that affect the protocol response, resolved once at admission.
#[derive(Clone, Copy, Default)]
pub(super) struct ResponseOptions {
    pub(super) return_immediately: bool,
    history_length: Option<i32>,
    timeline_length: Option<i32>,
}

impl ResponseOptions {
    pub(super) fn from_request(request: &SendMessageRequest) -> Self {
        let config = request.configuration.as_ref();
        Self {
            return_immediately: config
                .and_then(|config| config.return_immediately)
                .unwrap_or(false),
            history_length: config.and_then(|config| config.history_length),
            timeline_length: config.and_then(|config| config.timeline_length),
        }
    }

    pub(super) fn trim(self, task: &mut Task) {
        apply_lengths(task, self.history_length, self.timeline_length);
    }
}

#[derive(Clone, Copy)]
pub(super) enum OpeningPolicy {
    None,
    OnFirstEvent,
    Snapshot,
}

pub(super) enum DeliveryAttachment {
    /// Start a response; streaming modes announce the input in their opening snapshot.
    Opening,
    /// An existing response must observe the follow-up's input events.
    FollowUp,
}

#[derive(Clone, Copy)]
pub(super) enum InputCursor {
    Before,
    After,
}

pub(super) struct DeliveryPlan {
    pub(super) attach_receiver: bool,
    pub(super) cursor: InputCursor,
    pub(super) wait_for_first_output: bool,
}

#[derive(Clone, Copy)]
pub(super) struct ResponsePolicy {
    mode: StartMode,
    wants_message_updates: bool,
    options: ResponseOptions,
    response_id: Option<uuid::Uuid>,
}

impl ResponsePolicy {
    pub(super) fn new(mode: StartMode, wants_message_updates: bool) -> Self {
        Self {
            mode,
            wants_message_updates,
            options: ResponseOptions::default(),
            response_id: None,
        }
    }

    /// Resolve response behavior once, before admitting the opening message.
    pub(super) fn message(
        mode: StartMode,
        params: &ServiceParams,
        request: &SendMessageRequest,
    ) -> Self {
        Self {
            options: ResponseOptions::from_request(request),
            ..Self::subscription(mode, params)
        }
    }

    pub(super) fn subscription(mode: StartMode, params: &ServiceParams) -> Self {
        Self {
            response_id: (mode == StartMode::Bidi).then(uuid::Uuid::now_v7),
            ..Self::new(mode, is_at_least_1_1(&requested_protocol_version(params)))
        }
    }

    pub(super) fn mode(self) -> StartMode {
        self.mode
    }

    pub(super) fn is_live(self) -> bool {
        self.mode == StartMode::Bidi
    }

    pub(super) fn return_immediately(self) -> bool {
        self.options.return_immediately
    }

    pub(super) fn trim(self, task: &mut Task) {
        self.options.trim(task);
    }

    pub(super) fn completes(self, event: &StreamResponse) -> bool {
        matches!(
            classify_output(event),
            OutputKind::Reply | OutputKind::Terminal
        ) || (self.mode == StartMode::Blocking
            && should_interrupt_non_streaming(self.return_immediately(), event).is_some())
    }

    pub(super) fn wants_message_updates(self) -> bool {
        self.wants_message_updates
    }

    pub(super) fn response_id(self) -> Option<uuid::Uuid> {
        self.response_id
    }

    pub(super) fn after_event(self, event: &Result<StreamResponse, A2AError>) -> StreamDecision {
        if event.as_ref().map_or(true, |event| self.completes(event)) {
            StreamDecision::Close
        } else {
            StreamDecision::KeepOpen
        }
    }

    /// Select the pre-stream error for this operation. Live follow-ups use
    /// Rejection::into_stream_error instead, keeping their exchange open.
    pub(super) fn opening_error(self, rejection: Rejection, task_id: Option<&str>) -> A2AError {
        match self.mode {
            StartMode::Bidi => rejection.into_error(),
            _ => unary_refusal(rejection, task_id),
        }
    }

    pub(super) fn opening(self) -> OpeningPolicy {
        match self.mode {
            StartMode::Blocking => OpeningPolicy::None,
            StartMode::Streaming => OpeningPolicy::OnFirstEvent,
            StartMode::Bidi => OpeningPolicy::Snapshot,
        }
    }
    pub(super) fn follows_task(self) -> bool {
        self.mode != StartMode::Blocking
    }
    pub(super) fn delivery_plan(self, attachment: DeliveryAttachment) -> DeliveryPlan {
        DeliveryPlan {
            attach_receiver: !(self.mode == StartMode::Blocking
                && self.return_immediately()
                && matches!(attachment, DeliveryAttachment::Opening)),
            cursor: if self.mode == StartMode::Blocking
                || matches!(attachment, DeliveryAttachment::FollowUp)
            {
                InputCursor::Before
            } else {
                InputCursor::After
            },
            wait_for_first_output: self.mode == StartMode::Streaming
                && matches!(attachment, DeliveryAttachment::Opening),
        }
    }
    pub(super) fn event_filter(self) -> EventFilter {
        EventFilter {
            include_repeats: self.mode == StartMode::Blocking,
            message_updates: self.mode != StartMode::Blocking && self.wants_message_updates,
            live: self.mode == StartMode::Bidi,
            response_id: self.response_id,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StreamDecision {
    KeepOpen,
    Close,
}

#[derive(Clone, Copy)]
pub(super) struct EventFilter {
    pub(super) include_repeats: bool,
    pub(super) message_updates: bool,
    pub(super) live: bool,
    pub(super) response_id: Option<uuid::Uuid>,
}

pub(super) enum EventDecision {
    Skip,
    Deliver,
    DeliverAndClose,
}

impl EventFilter {
    pub(super) fn decide(
        self,
        kind: EventKind,
        result: &Result<StreamResponse, A2AError>,
    ) -> EventDecision {
        if (kind == EventKind::Repeated && !self.include_repeats)
            || matches!(kind, EventKind::LiveOnly(recipient) if !self.live || recipient != self.response_id)
            || (!self.message_updates && matches!(result, Ok(StreamResponse::MessageUpdate(_))))
        {
            return EventDecision::Skip;
        }
        if result.as_ref().map_or(true, is_terminal_event) || kind == EventKind::Reply {
            EventDecision::DeliverAndClose
        } else {
            EventDecision::Deliver
        }
    }
}

pub(super) fn needs_opening_task(first: &Result<StreamResponse, A2AError>) -> bool {
    !matches!(
        first,
        Ok(StreamResponse::Message(_) | StreamResponse::Task(_)) | Err(_)
    )
}

pub(super) fn is_terminal_event(event: &StreamResponse) -> bool {
    classify_output(event) == OutputKind::Terminal
}

pub(super) fn should_interrupt_non_streaming(
    return_immediately: bool,
    event: &StreamResponse,
) -> Option<TaskId> {
    if return_immediately {
        return match event {
            StreamResponse::Message(_) => None,
            _ => task_id_for_event(event).map(ToOwned::to_owned),
        };
    }

    // An interrupted state ends a blocking call (A2A §3.1.1): the turn may
    // stay open to take more input, so the stream ending is not what to wait for.
    let interrupted =
        |state: &TaskState| matches!(state, TaskState::AuthRequired | TaskState::InputRequired);
    match event {
        StreamResponse::Task(task) if interrupted(&task.status.state) => Some(task.id.clone()),
        StreamResponse::StatusUpdate(update) if interrupted(&update.status.state) => {
            Some(update.task_id.clone())
        }
        _ => None,
    }
}

/// Why a live request was not carried out.
pub(super) enum Rejection {
    /// The bounded queue is full, so the request cannot be accepted now. The
    /// client may retry; this is not an error in the request.
    Declined(String),
    Failed(A2AError),
    /// A transition panicked or lost its reply; storage must establish whether
    /// it committed before an HTTP followup can receive an acknowledgement.
    Interrupted(A2AError),
}

impl From<ProtocolError<'_>> for Rejection {
    fn from(error: ProtocolError<'_>) -> Self {
        Self::Failed(error.into())
    }
}

impl From<A2AError> for Rejection {
    fn from(error: A2AError) -> Self {
        Rejection::Failed(error)
    }
}

impl Rejection {
    pub(super) fn input_busy() -> Self {
        Self::Declined("the agent is busy with earlier input".into())
    }

    pub(super) fn unread_input() -> Self {
        Self::Declined("the agent finished its turn before reading this message".into())
    }

    /// The error to give a caller that is not on a stream yet.
    pub(super) fn into_error(self) -> A2AError {
        match self {
            Rejection::Declined(reason) => A2AError::unsupported_operation(reason),
            Rejection::Failed(error) | Rejection::Interrupted(error) => error,
        }
    }

    /// The non-fatal rejection of one request on a live stream: the code is the
    /// one the equivalent unary operation would report.
    pub(super) fn into_stream_error(
        self,
        task_id: &str,
        reference_id: Option<String>,
    ) -> StreamResponse {
        let (code, message) = match self {
            Rejection::Declined(reason) => ("UNAVAILABLE".to_string(), reason),
            Rejection::Failed(error) | Rejection::Interrupted(error) => {
                (stream_error_code(&error), error.message)
            }
        };
        StreamResponse::Error(StreamError {
            task_id: task_id.to_string(),
            code,
            message,
            reference_id: reference_id.filter(|id| !id.is_empty()),
            metadata: None,
        })
    }
}

/// The error for a request on a unary or streaming call that could not be
/// carried out. A message the running agent will not take is reported as the
/// task being busy, which is what these calls said before they could join a
/// running turn.
fn unary_refusal(rejection: Rejection, task_id: Option<&str>) -> A2AError {
    match rejection {
        Rejection::Declined(reason) => A2AError::invalid_request(format!(
            "task execution is already in progress: {} ({reason})",
            task_id.unwrap_or_default()
        )),
        Rejection::Failed(error) | Rejection::Interrupted(error) => error,
    }
}

fn stream_error_code(error: &A2AError) -> String {
    match error.code {
        error_code::INVALID_PARAMS
        | error_code::INVALID_REQUEST
        | error_code::PARSE_ERROR
        | error_code::CONTENT_TYPE_NOT_SUPPORTED => "INVALID_ARGUMENT".to_string(),
        error_code::TASK_NOT_FOUND => "NOT_FOUND".to_string(),
        code => error_reason(code).to_string(),
    }
}

// Subscribing to an existing task: cursor recovery and opening snapshots.

pub(super) struct SubscriptionPlan {
    pub(super) opening: Option<Task>,
    pub(super) seen_generation: i64,
}

/// A current cursor resumes directly; a stale cursor recovers from a snapshot.
pub(super) fn subscription_plan(
    snapshot: Task,
    request: &SubscribeToTaskRequest,
) -> Result<SubscriptionPlan, A2AError> {
    if snapshot.status.state.is_terminal() {
        return Err(ProtocolError::TaskTerminated(&request.id).into());
    }
    Ok(SubscriptionPlan {
        opening: (request.last_seen_generation != Some(snapshot.generation)).then_some(snapshot),
        seen_generation: request.last_seen_generation.unwrap_or(0),
    })
}

// Canceling tasks: idempotency, committed state and one announcement.

/// False is an idempotent re-cancel; true requires an atomic store transition.
pub(super) fn cancel_requires_transition(task: &Task) -> Result<bool, A2AError> {
    match task.status.state {
        TaskState::Canceled => Ok(false),
        _ if task.status.state.is_terminal() => Err(A2AError::task_not_cancelable(&task.id)),
        _ => Ok(true),
    }
}

/// Where a cancellation stands while its executor's events are applied.
pub(super) struct CancelEntry {
    /// The store gave the transition its generation and timeline entry.
    pub(super) recorded_by_store: bool,
    /// The generation it gave.
    pub(super) generation: i64,
    /// An event announcing the cancellation has been published.
    pub(super) announced: bool,
}

impl CancelEntry {
    /// Whether `event` is the executor's own status for the cancellation, which
    /// belongs in the store's entry rather than in one of its own: the task has
    /// not moved on since (nothing else advanced the generation).
    pub(super) fn can_amend(&self, base: &Task, event: &StreamResponse) -> bool {
        self.recorded_by_store
            && !self.announced
            && base.generation == self.generation
            && matches!(
                event,
                StreamResponse::StatusUpdate(update) if update.status.state == TaskState::Canceled
            )
    }

    /// Validate hook output and preserve the store's cancellation generation.
    pub(super) fn plan_update(
        &self,
        base: Task,
        event: &mut StreamResponse,
    ) -> Result<CancellationMutation, A2AError> {
        validate_event_identity(&base, event)?;
        let contradicts = match event {
            StreamResponse::Task(task) => task.status.state != TaskState::Canceled,
            StreamResponse::StatusUpdate(update) => update.status.state != TaskState::Canceled,
            _ => false,
        };
        if contradicts {
            return Err(A2AError::invalid_agent_response());
        }
        if !self.can_amend(&base, event) {
            return Ok(CancellationMutation::Apply(base));
        }
        let StreamResponse::StatusUpdate(update) = event else {
            unreachable!("can_amend matches a status")
        };
        let mut amended = base;
        amended.status = update.status.clone();
        if let Some(TimelineEntry {
            content: TimelineContent::Status(status),
            ..
        }) = amended.timeline.iter_mut().flatten().last()
        {
            *status = update.status.clone();
        }
        update.generation = amended.generation;
        Ok(CancellationMutation::Amend(amended))
    }

    /// Decide whether finalization still owes subscribers a CANCELED update.
    pub(super) fn notification(&self, base: Option<Task>) -> CancellationNotification {
        if self.recorded_by_store && self.announced {
            return CancellationNotification::None;
        }
        let Some(base) = base else {
            return CancellationNotification::None;
        };
        if !self.recorded_by_store && base.timeline.iter().flatten().any(|entry| {
            entry.generation > self.generation && matches!(&entry.content, TimelineContent::Status(status) if status.state == TaskState::Canceled)
        }) { return CancellationNotification::None; }
        let mut status = base.status.clone();
        status.state = TaskState::Canceled;
        let mut event = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: base.id.clone(),
            context_id: base.context_id.clone(),
            status,
            metadata: None,
            generation: 0,
        });
        if self.recorded_by_store && base.generation == self.generation {
            let StreamResponse::StatusUpdate(update) = &mut event else {
                unreachable!()
            };
            update.generation = base.generation;
            CancellationNotification::Publish(event)
        } else {
            CancellationNotification::Persist {
                task: Box::new(base),
                event,
            }
        }
    }

    /// Notes that a published event announced the canceled state.
    pub(super) fn note_published(&mut self, published: bool, event: &StreamResponse) {
        let canceled = match event {
            StreamResponse::StatusUpdate(update) => update.status.state == TaskState::Canceled,
            StreamResponse::Task(task) => task.status.state == TaskState::Canceled,
            _ => false,
        };
        self.announced |= published && canceled;
    }
}

pub(super) enum CancellationMutation {
    Amend(Task),
    Apply(Task),
}

pub(super) enum CancellationNotification {
    None,
    Publish(StreamResponse),
    Persist {
        task: Box<Task>,
        event: StreamResponse,
    },
}

// Task updates: client inputs, executor output, generations and artifacts.

pub(super) struct RecordedInput {
    pub(super) events: Vec<StreamResponse>,
    /// The message's generation before any artifact updates advanced it.
    pub(super) message_generation: Option<i64>,
}

/// Records a client message in history and the timeline, then applies
/// the artifacts sent with it (A2A 1.1 §3.4: the message first).
///
/// Returns the events that describe what was recorded, in order, for a live
/// stream to announce: the message as it was appended, then each artifact.
pub(super) fn record_client_input(
    task: &mut Task,
    req: &SendMessageRequest,
    creating: bool,
) -> Result<RecordedInput, A2AError> {
    let mut events = Vec::new();
    let mut message_generation = None;
    if !is_artifact_only(req) {
        let asked = task.status.message.clone();
        let history = task.history.get_or_insert_with(Vec::new);
        if !creating
            && let Some(asked) = asked.filter(|asked| {
                history
                    .iter()
                    .all(|seen| seen.message_id != asked.message_id)
            })
        {
            history.push(asked);
        }
        history.push(req.message.clone());
        task.generation += 1;
        message_generation = Some(task.generation);
        task.timeline
            .get_or_insert_with(Vec::new)
            .push(TimelineEntry {
                content: TimelineContent::Message(req.message.clone()),
                generation: task.generation,
            });
        events.push(client_message_event(task, &req.message));
    }
    events.extend(
        apply_client_artifacts(task, req, creating)?
            .into_iter()
            .map(StreamResponse::ArtifactUpdate),
    );
    Ok(RecordedInput {
        events,
        message_generation,
    })
}

/// The event that announces a client message appended to `task` at its current
/// generation.
pub(super) fn client_message_event(task: &Task, message: &Message) -> StreamResponse {
    StreamResponse::MessageUpdate(TaskMessageUpdateEvent {
        task_id: task.id.clone(),
        context_id: task.context_id.clone(),
        message: message.clone(),
        metadata: None,
        generation: task.generation,
    })
}

pub(super) enum TaskMutation {
    Save(Task),
    Unchanged(Option<Task>),
}

/// Storage reads remain outside this module, and only mutating event kinds request one.
pub(super) fn mutation_task_id(event: &StreamResponse) -> Option<&str> {
    match event {
        StreamResponse::Task(task) => Some(&task.id),
        StreamResponse::StatusUpdate(update) => Some(&update.task_id),
        StreamResponse::ArtifactUpdate(update) => Some(&update.task_id),
        _ => None,
    }
}

/// Caller already checked event identity against its original authoritative snapshot.
pub(super) fn apply_task_mutation(
    current: Option<Task>,
    event: &mut StreamResponse,
) -> Result<TaskMutation, A2AError> {
    match event {
        StreamResponse::Task(task) => {
            match current {
                Some(previous) => rebase_snapshot(task, previous),
                None => {
                    task.generation = 0;
                    task.timeline = None;
                    let generation = record_status(task, task.status.clone());
                    for artifact in task.artifacts.iter_mut().flatten() {
                        artifact.start_generation.get_or_insert(generation);
                    }
                    if task.status.state.is_terminal() {
                        close_open_artifacts(task, generation);
                    }
                }
            }
            Ok(TaskMutation::Save(task.clone()))
        }
        StreamResponse::StatusUpdate(update) => {
            let mut task = current.ok_or_else(|| A2AError::task_not_found(&update.task_id))?;
            task.status = update.status.clone();
            update.generation = record_status(&mut task, update.status.clone());
            if task.status.state.is_terminal() {
                close_open_artifacts(&mut task, update.generation);
            }
            Ok(TaskMutation::Save(task))
        }
        StreamResponse::ArtifactUpdate(update) => {
            let mut task = current.ok_or_else(|| A2AError::task_not_found(&update.task_id))?;
            update.generation = merge_artifact(&mut task, update)?;
            Ok(TaskMutation::Save(task))
        }
        StreamResponse::MessageUpdate(_) | StreamResponse::Error(_) => {
            Err(A2AError::invalid_agent_response())
        }
        _ => Ok(TaskMutation::Unchanged(current)),
    }
}

pub(super) fn repeats_generation(event: &StreamResponse, before: Option<i64>) -> bool {
    matches!(event, StreamResponse::Task(task) if Some(task.generation) == before)
}

pub(super) fn prepare_task_for_storage(task: &mut Task) {
    if task.status.state.is_terminal() {
        close_open_artifacts(task, task.generation);
    }
    if task.status.timestamp.is_none() {
        task.status.timestamp = Some(chrono::Utc::now());
    }
}

/// An execution may only publish or mutate its bound task and context.
/// A message's optional task/context IDs may be absent for direct replies, but never name
/// another task. Validate before looking up or saving the event's IDs.
pub(super) fn validate_event_identity(task: &Task, event: &StreamResponse) -> Result<(), A2AError> {
    let (id, context) = match event {
        StreamResponse::Task(value) => (Some(value.id.as_str()), Some(value.context_id.as_str())),
        StreamResponse::StatusUpdate(value) => (
            Some(value.task_id.as_str()),
            Some(value.context_id.as_str()),
        ),
        StreamResponse::ArtifactUpdate(value) => (
            Some(value.task_id.as_str()),
            Some(value.context_id.as_str()),
        ),
        StreamResponse::MessageUpdate(value) => (
            Some(value.task_id.as_str()),
            Some(value.context_id.as_str()),
        ),
        StreamResponse::Error(value) => (Some(value.task_id.as_str()), None),
        StreamResponse::Message(value) => (value.task_id.as_deref(), value.context_id.as_deref()),
        _ => return Ok(()),
    };
    if id.is_some_and(|id| id != task.id)
        || context.is_some_and(|context| context != task.context_id)
    {
        return Err(A2AError::invalid_agent_response());
    }
    Ok(())
}

/// DM-SERIAL-001: backfills a missing status timestamp on the event itself,
/// before it is both persisted (via `apply_event_to_task`/`save_task_with`) and
/// broadcast to any live subscriber -- `save_task_with`'s own backfill only
/// covers what gets persisted; a subscriber watching `send_streaming_message`
/// or `subscribe_to_task` receives the broadcast event directly, never a
/// reload from the store, so backfilling only at persistence time still
/// leaves a live subscriber seeing a timestamp-less status.
pub(super) fn backfill_status_timestamp(event: &mut StreamResponse) {
    let status = match event {
        StreamResponse::Task(task) => &mut task.status,
        StreamResponse::StatusUpdate(update) => &mut update.status,
        _ => return,
    };
    if status.timestamp.is_none() {
        status.timestamp = Some(chrono::Utc::now());
    }
}

/// Records an agent status as the task's next timeline entry, advancing the
/// generation. Returns the generation after the mutation.
pub(super) fn record_status(task: &mut Task, status: TaskStatus) -> i64 {
    task.generation += 1;
    task.timeline
        .get_or_insert_with(Vec::new)
        .push(TimelineEntry {
            content: TimelineContent::Status(status),
            generation: task.generation,
        });
    task.generation
}

/// Rebases a snapshot an executor built onto the stored task. The generation,
/// timeline, history and artifact stamps belong to the server: an executor
/// that does not set `artifacts` or `history` leaves what is stored in place,
/// and a snapshot that changes nothing is not a mutation.
pub(super) fn rebase_snapshot(task: &mut Task, previous: Task) {
    let status_changed = task.status.state != previous.status.state
        || task.status.message != previous.status.message;
    // The artifacts the client wrote are the server's to keep: an executor
    // lists what it produced, not what the client sent it.
    let supplied = task.artifacts.take().map(|mut new| {
        for kept in previous.artifacts.iter().flatten() {
            if kept.role == Some(Role::User) {
                match new
                    .iter_mut()
                    .find(|artifact| artifact.artifact_id == kept.artifact_id)
                {
                    Some(stale) => *stale = kept.clone(),
                    None => new.push(kept.clone()),
                }
            }
        }
        new
    });
    let artifacts_changed = supplied
        .as_ref()
        .is_some_and(|new| artifacts_differ(new, previous.artifacts.as_deref().unwrap_or(&[])));

    task.timeline = previous.timeline;
    task.generation = previous.generation;
    // Recorded messages are append-only. A snapshot built before a later
    // client input must not erase that input from the compatibility history.
    task.history = match (previous.history, task.history.take()) {
        (Some(mut current), Some(supplied)) => {
            for message in supplied {
                if !current
                    .iter()
                    .any(|kept| kept.message_id == message.message_id)
                {
                    current.push(message);
                }
            }
            Some(current)
        }
        (current, supplied) => supplied.or(current),
    };
    if task.metadata.is_none() {
        task.metadata = previous.metadata;
    }
    let stored_artifacts = previous.artifacts;
    task.artifacts = match supplied {
        Some(new) if artifacts_changed => Some(new),
        _ => stored_artifacts.clone(),
    };

    if status_changed || artifacts_changed {
        task.generation += 1;
        let generation = task.generation;
        if status_changed {
            task.timeline
                .get_or_insert_with(Vec::new)
                .push(TimelineEntry {
                    content: TimelineContent::Status(task.status.clone()),
                    generation,
                });
        }
        let stored = stored_artifacts.unwrap_or_default();
        for artifact in task.artifacts.iter_mut().flatten() {
            let before = stored
                .iter()
                .find(|stored| stored.artifact_id == artifact.artifact_id);
            artifact.start_generation = artifact
                .start_generation
                .or(before.and_then(|stored| stored.start_generation))
                .or(Some(generation));
            artifact.end_generation = artifact
                .end_generation
                .or(before.and_then(|stored| stored.end_generation));
            artifact.role = artifact
                .role
                .clone()
                .or_else(|| before.and_then(|stored| stored.role.clone()));
        }
    }
    if task.status.state.is_terminal() {
        close_open_artifacts(task, task.generation);
    }
}

/// Merges an artifact update into the task, advancing the generation. The
/// stored artifact is stamped with the generations at which it started and
/// completed; the update itself is left as sent apart from the generation it
/// is assigned. Returns the generation after the mutation.
pub(super) fn merge_artifact(
    task: &mut Task,
    update: &mut TaskArtifactUpdateEvent,
) -> Result<i64, A2AError> {
    task.generation += 1;
    let generation = task.generation;
    let append = update.append.unwrap_or(false);
    // The wire cannot tell an unset `lastChunk` from `false`, so only an
    // explicit `true` completes an artifact here; one left open is closed when
    // the task reaches a terminal state.
    let complete = update.last_chunk == Some(true);

    let artifacts = task.artifacts.get_or_insert_with(Vec::new);
    let existing = artifacts
        .iter_mut()
        .find(|artifact| artifact.artifact_id == update.artifact.artifact_id);
    if append {
        let artifact = existing.ok_or_else(A2AError::invalid_agent_response)?;
        artifact.parts.extend(update.artifact.parts.iter().cloned());
        if let Some(metadata) = &update.artifact.metadata {
            artifact
                .metadata
                .get_or_insert_with(Default::default)
                .extend(metadata.clone());
        }
        if complete {
            artifact.end_generation = Some(generation);
        }
    } else {
        let mut replacement = update.artifact.clone();
        // Replacing an artifact keeps who wrote it.
        replacement.role = replacement
            .role
            .clone()
            .or_else(|| existing.as_ref().and_then(|artifact| artifact.role.clone()));
        replacement.start_generation = Some(
            existing
                .as_ref()
                .and_then(|artifact| artifact.start_generation)
                .unwrap_or(generation),
        );
        replacement.end_generation = complete.then_some(generation);
        match existing {
            Some(artifact) => *artifact = replacement,
            None => artifacts.push(replacement),
        }
    }
    Ok(generation)
}

/// Applies the artifacts a client wrote in `artifactUpdates`, in list order,
/// advancing the generation for each. The client does not own `generation`:
/// whatever it sent is ignored and the artifact is attributed to the user.
///
/// When the message creates the task the client cannot know its ids, so they
/// are assigned; on an existing task they must match. A client may only touch
/// artifacts it wrote itself.
pub(super) fn apply_client_artifacts(
    task: &mut Task,
    req: &SendMessageRequest,
    new_task: bool,
) -> Result<Vec<TaskArtifactUpdateEvent>, A2AError> {
    let mut applied = Vec::with_capacity(req.artifact_updates.len());
    for update in &req.artifact_updates {
        if update.artifact.artifact_id.is_empty() {
            return Err(A2AError::invalid_params(
                "an artifact update needs an artifactId",
            ));
        }
        let mut update = update.clone();
        if !new_task {
            if !update.task_id.is_empty() && update.task_id != task.id {
                return Err(A2AError::invalid_params(format!(
                    "artifact update targets task {}, not {}",
                    update.task_id, task.id
                )));
            }
            if !update.context_id.is_empty() && update.context_id != task.context_id {
                return Err(A2AError::invalid_params(format!(
                    "artifact update names context {}, but task {} belongs to {}",
                    update.context_id, task.id, task.context_id
                )));
            }
        }
        let existing_role = task
            .artifacts
            .iter()
            .flatten()
            .find(|artifact| artifact.artifact_id == update.artifact.artifact_id)
            .map(|artifact| artifact.role.clone());
        if existing_role.is_some_and(|role| role != Some(Role::User)) {
            return Err(A2AError::invalid_params(format!(
                "artifact {} was not written by the client and cannot be changed by it",
                update.artifact.artifact_id
            )));
        }
        update.artifact.role = Some(Role::User);
        update.task_id = task.id.clone();
        update.context_id = task.context_id.clone();
        update.generation = merge_artifact(task, &mut update).map_err(|_| {
            A2AError::invalid_params(format!(
                "cannot append to unknown artifact {}",
                update.artifact.artifact_id
            ))
        })?;
        applied.push(update);
    }
    Ok(applied)
}

/// Whether the artifacts an executor supplied differ in content from the
/// stored ones. The generations the server stamps are not part of the content.
pub(super) fn artifacts_differ(supplied: &[Artifact], stored: &[Artifact]) -> bool {
    supplied.len() != stored.len()
        || supplied.iter().any(|a| {
            stored
                .iter()
                .find(|b| b.artifact_id == a.artifact_id)
                .is_none_or(|b| {
                    a.name != b.name
                        || a.description != b.description
                        || a.parts != b.parts
                        || a.metadata != b.metadata
                        || a.extensions != b.extensions
                })
        })
}

/// A terminal task has no more output coming, so any artifact still open is
/// completed at the generation that ended it.
pub(super) fn close_open_artifacts(task: &mut Task, generation: i64) {
    for artifact in task.artifacts.iter_mut().flatten() {
        artifact.end_generation.get_or_insert(generation);
    }
}

pub(super) enum ExecutionFailure<'a> {
    Error(&'a A2AError),
    Panic,
}

pub(super) fn failure_event(task: &Task, failure: ExecutionFailure<'_>) -> StreamResponse {
    let message = match failure {
        ExecutionFailure::Error(error) => Message {
            task_id: Some(task.id.clone()),
            context_id: Some(task.context_id.clone()),
            ..Message::new(
                Role::Agent,
                vec![Part::text(
                    crate::sanitized_internal_error(&error.message).message,
                )],
            )
        },
        ExecutionFailure::Panic => Message::new(
            Role::Agent,
            vec![Part::text("The agent failed unexpectedly.")],
        ),
    };
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        task_id: task.id.clone(),
        context_id: task.context_id.clone(),
        generation: 0,
        metadata: None,
        status: TaskStatus {
            state: TaskState::Failed,
            message: Some(message),
            timestamp: Some(chrono::Utc::now()),
        },
    })
}

pub(super) fn current_status_event(task: Task) -> StreamResponse {
    StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
        generation: task.generation,
        task_id: task.id,
        context_id: task.context_id,
        status: task.status,
        metadata: None,
    })
}

// Reading tasks and push configurations: response trimming and pagination.

/// Applies the response-trimming parameters shared by every operation that
/// returns a task: the deprecated `historyLength` and its replacement
/// `timelineLength`.
pub(super) fn apply_lengths(
    task: &mut Task,
    history_length: Option<i32>,
    timeline_length: Option<i32>,
) {
    apply_history_length(task, history_length);
    apply_timeline_length(task, timeline_length);
}

/// Parse a paging cursor using the same error contract as the operation.
pub(super) fn push_config_page_offset(token: Option<&str>) -> Result<usize, A2AError> {
    token.map_or(Ok(0), |token| {
        token
            .parse()
            .map_err(|_| ProtocolError::InvalidPageToken.into())
    })
}

// Shared event identity and processing receipts.

pub(super) fn task_id_for_event(event: &StreamResponse) -> Option<&str> {
    match event {
        StreamResponse::Task(task) => Some(&task.id),
        StreamResponse::StatusUpdate(update) => Some(&update.task_id),
        StreamResponse::ArtifactUpdate(update) => Some(&update.task_id),
        StreamResponse::MessageUpdate(update) => Some(&update.task_id),
        StreamResponse::Message(_) | StreamResponse::Error(_) => None,
        _ => None,
    }
}

/// The generation an event reports, if it reports one.
pub(super) fn generation_of(event: &StreamResponse) -> Option<i64> {
    match event {
        StreamResponse::Task(task) => Some(task.generation),
        StreamResponse::StatusUpdate(update) => Some(update.generation),
        StreamResponse::ArtifactUpdate(update) => Some(update.generation),
        StreamResponse::MessageUpdate(update) => Some(update.generation),
        _ => None,
    }
}

/// Whether `task` holds the message `request` carries (always true for a
/// request that has no message id to look for).
pub(super) fn records_message(task: &Task, request: &StreamRequest) -> bool {
    let StreamRequestPayload::SendMessage(send) = &request.payload else {
        return true;
    };
    let Some(message) = &send.message else {
        return true;
    };
    let id = &message.message_id;
    id.is_empty()
        || task.history.iter().flatten().any(|m| &m.message_id == id)
        || task.timeline.iter().flatten().any(
            |entry| matches!(&entry.content, TimelineContent::Message(m) if &m.message_id == id),
        )
}

// Failures detected outside this module: canonical codes and messages.

/// Protocol failures discovered by the runtime. Their codes and wire messages
/// are selected here; application/store errors continue to propagate unchanged.
/// The exchange variants preserve this SDK's existing HTTP routing convention.
#[derive(Clone, Copy, Debug)]
pub(super) enum ProtocolError<'a> {
    // Task lookup, subscription and execution admission.
    TaskNotFound(&'a str),
    TaskTerminated(&'a str),
    ExecutionInProgress,
    // Stream openings and follow-up inputs.
    MissingOpeningRequest,
    UnknownStreamRequest,
    SubscriptionAfterOpening,
    // Push configuration and extended agent card operations.
    InvalidPageToken,
    PushUnsupported,
    ExtendedCardNotConfigured,
    // Existing SDK-specific HTTP live routing.
    InvalidExchangeRoutingHeader,
    AmbiguousExchangeIdentifier,
    AmbiguousExchange,
    ForeignExchangeCaller,
    ExchangeIdentifierInUse,
}

impl From<ProtocolError<'_>> for A2AError {
    fn from(error: ProtocolError<'_>) -> Self {
        match error {
            ProtocolError::TaskNotFound(id) => Self::task_not_found(id),
            ProtocolError::TaskTerminated(id) => Self::unsupported_operation(format!(
                "task {id} has already reached a terminal state"
            )),
            ProtocolError::ExecutionInProgress => {
                Self::invalid_request("task execution is already in progress")
            }
            ProtocolError::MissingOpeningRequest => {
                Self::invalid_request("a response needs a first request")
            }
            ProtocolError::UnknownStreamRequest => {
                Self::invalid_request("unrecognised StreamRequest payload")
            }
            ProtocolError::SubscriptionAfterOpening => {
                Self::invalid_request("subscribe is only valid as the first request of a stream")
            }
            ProtocolError::InvalidPageToken => Self::invalid_params("invalid page token"),
            ProtocolError::PushUnsupported => Self::push_notification_not_supported(),
            ProtocolError::ExtendedCardNotConfigured => Self::extended_card_not_configured(),
            ProtocolError::InvalidExchangeRoutingHeader => {
                Self::invalid_params("invalid live exchange routing header")
            }
            ProtocolError::AmbiguousExchangeIdentifier => {
                Self::invalid_request("ambiguous live exchange identifier")
            }
            ProtocolError::AmbiguousExchange => Self::invalid_request(
                "ambiguous live exchange; use a distinct exchange routing identifier",
            ),
            ProtocolError::ForeignExchangeCaller => {
                Self::invalid_request("live exchange belongs to a different caller")
            }
            ProtocolError::ExchangeIdentifierInUse => {
                Self::invalid_request("live exchange identifier is already in use")
            }
        }
    }
}

#[cfg(test)]
mod coverage_tests {
    use super::*;
    use crate::handler::test_support::{make_message, send_req};

    #[test]
    fn busy_live_rejections_keep_the_request_reference_and_allow_retry() {
        for reference in [None, Some("".into()), Some("input-id".into())] {
            let event = Rejection::Declined("input queue full".into())
                .into_stream_error("task", reference.clone());
            let StreamResponse::Error(error) = &event else {
                panic!("expected an in-band rejection")
            };
            assert_eq!(error.code, "UNAVAILABLE");
            assert_eq!(error.task_id, "task");
            assert_eq!(error.message, "input queue full");
            assert_eq!(error.reference_id, reference.filter(|id| !id.is_empty()));
            assert!(!is_terminal_event(&event));
            assert_eq!(task_id_for_event(&event), None);
        }
    }

    #[test]
    fn subscription_receipts_do_not_require_a_recorded_message() {
        let request = StreamRequest {
            tenant: None,
            payload: StreamRequestPayload::Subscribe(SubscribeToTaskRequest {
                id: "task".into(),
                tenant: None,
                last_seen_generation: None,
            }),
        };
        assert!(records_message(
            &crate::handler::test_support::task_with_history("task", "context", 0),
            &request
        ));
    }

    #[test]
    fn immediate_unary_responses_do_not_discard_a_direct_message() {
        let mut request = send_req(None, None);
        request.configuration = Some(SendMessageConfiguration {
            return_immediately: Some(true),
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: None,
            timeline_length: None,
            if_generation_match: None,
        });
        let reply = StreamResponse::Message(make_message());
        assert_eq!(
            should_interrupt_non_streaming(
                ResponseOptions::from_request(&request).return_immediately,
                &reply
            ),
            None
        );
        assert_eq!(task_id_for_event(&reply), None);
    }

    #[test]
    fn live_rejections_preserve_codes_and_omit_empty_reference_ids() {
        for (error, expected) in [
            (A2AError::invalid_params("bad input"), "INVALID_ARGUMENT"),
            (A2AError::task_not_found("gone"), "NOT_FOUND"),
            (A2AError::internal("store unavailable"), "INTERNAL_ERROR"),
        ] {
            let StreamResponse::Error(rejection) =
                Rejection::from(error.clone()).into_stream_error("task", Some(String::new()))
            else {
                panic!("expected stream error")
            };
            assert_eq!(rejection.code, expected);
            assert_eq!(rejection.message, error.message);
            assert_eq!(rejection.reference_id, None);
            let returned = Rejection::from(error.clone()).into_error();
            assert_eq!(returned.code, error.code);
            assert_eq!(returned.message, error.message);
        }
        assert_eq!(
            Rejection::Declined("busy".into()).into_error().code,
            error_code::UNSUPPORTED_OPERATION
        );
        let refusal = unary_refusal(Rejection::Declined("full".into()), None);
        assert_eq!(refusal.code, error_code::INVALID_REQUEST);
        assert!(refusal.message.contains("full"));
    }
    #[test]
    fn update_events_route_to_their_task_and_auth_required_interrupts_unary_calls() {
        let artifact = StreamResponse::ArtifactUpdate(
            serde_json::from_value(serde_json::json!({
                "taskId":"task", "contextId":"context",
                "artifact":{"artifactId":"artifact", "parts":[{"text":"artifact"}]}
            }))
            .unwrap(),
        );
        let message = StreamResponse::MessageUpdate(
            serde_json::from_value(serde_json::json!({
                "taskId":"task", "contextId":"context",
                "message":{"messageId":"message", "role":"ROLE_USER", "parts":[{"text":"message"}]}
            }))
            .unwrap(),
        );
        assert_eq!(task_id_for_event(&artifact), Some("task"));
        assert_eq!(task_id_for_event(&message), Some("task"));
        let task: Task = serde_json::from_value(serde_json::json!({
            "id":"task", "contextId":"context", "status":{"state":"TASK_STATE_AUTH_REQUIRED"}
        }))
        .unwrap();
        let status = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: task.id.clone(),
            context_id: task.context_id.clone(),
            status: task.status.clone(),
            metadata: None,
            generation: 0,
        });
        let request = crate::handler::test_support::send_req(None, None);
        assert_eq!(
            should_interrupt_non_streaming(
                ResponseOptions::from_request(&request).return_immediately,
                &StreamResponse::Task(task)
            ),
            Some("task".into())
        );
        assert_eq!(
            should_interrupt_non_streaming(
                ResponseOptions::from_request(&request).return_immediately,
                &status
            ),
            Some("task".into())
        );
    }
}

#[cfg(test)]
mod artifact_tests {
    use super::*;
    use crate::handler::test_support::*;

    /// Reported in #161 against a different crate (`a2a-protocol-server`),
    /// but worth pinning here: two tasks sharing a `contextId` must each
    /// carry only their own artifacts. The task store is keyed by task id,
    /// so there is no path by which a context could aggregate them — this
    /// test exists so that stays true.
    #[tokio::test]
    async fn test_artifacts_do_not_aggregate_across_tasks_in_one_context() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let context_id = "ctx-shared";

        for (task_id, artifact_id) in [("t-first", "a-1"), ("t-second", "a-2")] {
            handler
                .tasks
                .store()
                .create(Task {
                    generation: 0,
                    timeline: None,
                    id: task_id.into(),
                    context_id: context_id.into(),
                    status: TaskStatus {
                        state: TaskState::Working,
                        message: None,
                        timestamp: None,
                    },
                    artifacts: None,
                    history: None,
                    metadata: None,
                })
                .await
                .unwrap();

            apply_event_to_task(
                handler.tasks.store(),
                None,
                &mut StreamResponse::ArtifactUpdate(TaskArtifactUpdateEvent {
                    generation: 0,
                    task_id: task_id.into(),
                    context_id: context_id.into(),
                    artifact: Artifact {
                        end_generation: None,
                        role: None,
                        start_generation: None,
                        artifact_id: artifact_id.into(),
                        name: None,
                        description: None,
                        parts: vec![Part::text("payload")],
                        metadata: None,
                        extensions: None,
                    },
                    append: None,
                    last_chunk: None,
                    metadata: None,
                }),
            )
            .await
            .unwrap();
        }

        for (task_id, expected) in [("t-first", "a-1"), ("t-second", "a-2")] {
            let task = handler
                .get_task(
                    &params,
                    GetTaskRequest {
                        current_generation: None,
                        timeline_length: None,
                        id: task_id.into(),
                        history_length: None,
                        tenant: None,
                    },
                )
                .await
                .unwrap();
            let ids: Vec<&str> = task
                .artifacts
                .as_deref()
                .unwrap_or_default()
                .iter()
                .map(|a| a.artifact_id.as_str())
                .collect();
            assert_eq!(
                ids,
                vec![expected],
                "{task_id} must carry only its own artifact"
            );
        }
    }
    #[tokio::test]
    async fn rejected_artifact_contexts_and_unknown_appends_do_not_change_storage() {
        let handler = make_handler().with_capabilities(AgentCapabilities {
            user_artifacts: Some(true),
            ..AgentCapabilities::default()
        });
        seed_task(&handler, "artifact-validation", "context").await;
        let before = handler
            .tasks
            .store()
            .get("artifact-validation")
            .await
            .unwrap()
            .unwrap();
        for (context, append) in [("other-context", false), ("context", true)] {
            let mut request = send_req(Some("artifact-validation"), Some("context"));
            request.artifact_updates = vec![TaskArtifactUpdateEvent {
                task_id: "artifact-validation".into(),
                context_id: context.into(),
                artifact: serde_json::from_value(serde_json::json!({
                    "artifactId": "unknown", "parts": [{"text": "client payload"}]
                }))
                .unwrap(),
                append: Some(append),
                last_chunk: Some(true),
                metadata: None,
                generation: 0,
            }];
            let error = handler
                .send_message(&ServiceParams::new(), request)
                .await
                .unwrap_err();
            assert_eq!(error.code, error_code::INVALID_PARAMS);
            assert_eq!(
                handler
                    .tasks
                    .store()
                    .get("artifact-validation")
                    .await
                    .unwrap()
                    .unwrap(),
                before
            );
        }
    }
}

#[cfg(test)]
mod policy_tests {
    use super::*;
    use crate::handler::test_support::{send_request, task_with_history};

    fn snapshot(state: TaskState) -> StreamResponse {
        let mut task = task_with_history("task", "context", 0);
        task.status.state = state;
        StreamResponse::Task(task)
    }

    fn status(state: TaskState) -> StreamResponse {
        StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
            task_id: "task".into(),
            context_id: "context".into(),
            status: TaskStatus {
                state,
                message: None,
                timestamp: None,
            },
            generation: 1,
            metadata: None,
        })
    }

    fn reply() -> StreamResponse {
        StreamResponse::Message(Message::new(Role::Agent, vec![Part::text("reply")]))
    }

    fn message_update() -> StreamResponse {
        StreamResponse::MessageUpdate(TaskMessageUpdateEvent {
            task_id: "task".into(),
            context_id: "context".into(),
            message: Message::new(Role::User, vec![Part::text("input")]),
            generation: 1,
            metadata: None,
        })
    }

    #[test]
    fn executor_output_sequences_allow_one_reply_or_a_task_lifecycle() {
        for (events, valid) in [
            (vec![reply()], true),
            (
                vec![snapshot(TaskState::Working), status(TaskState::Completed)],
                true,
            ),
            (vec![status(TaskState::Working), reply()], false),
            (vec![snapshot(TaskState::Working), reply()], false),
            (vec![reply(), reply()], false),
            (vec![reply(), status(TaskState::Working)], false),
        ] {
            let mut execution = ExecutionProtocol::default();
            let result = events
                .iter()
                .try_for_each(|event| execution.accept(event).map(|_| ()));
            assert_eq!(result.is_ok(), valid);
            if let Err(error) = result {
                assert_eq!(error.code, error_code::INVALID_AGENT_RESPONSE);
            }
        }
    }

    #[test]
    fn response_completion_matrix_distinguishes_messages_updates_and_interruptions() {
        for mode in [StartMode::Blocking, StartMode::Streaming, StartMode::Bidi] {
            for immediate in [false, true] {
                let mut request = send_request();
                request.configuration = Some(SendMessageConfiguration {
                    return_immediately: Some(immediate),
                    accepted_output_modes: None,
                    task_push_notification_config: None,
                    history_length: None,
                    timeline_length: None,
                    if_generation_match: None,
                });
                let policy = ResponsePolicy::message(mode, &ServiceParams::new(), &request);
                assert_eq!(policy.after_event(&Ok(reply())), StreamDecision::Close);
                assert_eq!(
                    policy.after_event(&Err(A2AError::internal("stream failed"))),
                    StreamDecision::Close
                );
                let rejection = Rejection::Declined("busy".into())
                    .into_stream_error("task", Some("input".into()));
                assert_eq!(policy.after_event(&Ok(rejection)), StreamDecision::KeepOpen);
                assert_eq!(
                    policy.completes(&message_update()),
                    mode == StartMode::Blocking && immediate
                );
                for state in [
                    TaskState::Submitted,
                    TaskState::Working,
                    TaskState::InputRequired,
                    TaskState::AuthRequired,
                    TaskState::Completed,
                    TaskState::Failed,
                    TaskState::Canceled,
                    TaskState::Rejected,
                ] {
                    let close = state.is_terminal()
                        || (mode == StartMode::Blocking
                            && (immediate
                                || matches!(
                                    state,
                                    TaskState::InputRequired | TaskState::AuthRequired
                                )));
                    for event in [snapshot(state.clone()), status(state)] {
                        assert_eq!(
                            policy.completes(&event),
                            close,
                            "mode: {mode:?}, event: {event:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn opening_and_subscription_rules_preserve_direct_replies_and_peer_visibility() {
        assert!(!needs_opening_task(&Ok(reply())));
        assert!(!needs_opening_task(&Ok(snapshot(TaskState::Working))));
        assert!(!needs_opening_task(&Err(A2AError::internal("no output"))));
        assert!(needs_opening_task(&Ok(status(TaskState::Working))));
        for mode in [StartMode::Blocking, StartMode::Streaming, StartMode::Bidi] {
            for version in ["1.0", "1.1"] {
                let params =
                    ServiceParams::from([(SVC_PARAM_VERSION.into(), vec![version.into()])]);
                let policy = ResponsePolicy::subscription(mode, &params);
                let filter = policy.event_filter();
                let update = filter.decide(EventKind::Normal, &Ok(message_update()));
                assert_eq!(
                    matches!(update, EventDecision::Deliver),
                    version == "1.1" && mode != StartMode::Blocking
                );
                let repeated =
                    filter.decide(EventKind::Repeated, &Ok(snapshot(TaskState::Working)));
                assert_eq!(
                    matches!(repeated, EventDecision::Deliver),
                    mode == StartMode::Blocking
                );
                assert!(matches!(
                    filter.decide(EventKind::Reply, &Ok(reply())),
                    EventDecision::DeliverAndClose
                ));
                let rejection = Rejection::Declined("busy".into())
                    .into_stream_error("task", Some("input".into()));
                let mine = filter.decide(
                    EventKind::LiveOnly(policy.response_id()),
                    &Ok(rejection.clone()),
                );
                assert_eq!(
                    matches!(mine, EventDecision::Deliver),
                    mode == StartMode::Bidi
                );
                assert!(matches!(
                    filter.decide(
                        EventKind::LiveOnly(Some(uuid::Uuid::now_v7())),
                        &Ok(rejection)
                    ),
                    EventDecision::Skip
                ));
            }
        }
    }

    #[test]
    fn request_binding_and_generation_checks_use_the_authoritative_task() {
        let mut request = send_request();
        bind_request_task(&mut request, "task").unwrap();
        assert_eq!(request.message.task_id.as_deref(), Some("task"));
        let before = request.clone();
        assert_eq!(
            bind_request_task(&mut request, "other").unwrap_err().code,
            error_code::INVALID_PARAMS
        );
        assert_eq!(request, before);
        let mut stored = task_with_history("task", "context", 0);
        stored.status.state = TaskState::Working;
        assert!(validate_task_reference(None, &request, "task").is_err());
        validate_task_reference(Some(&stored), &request, "task").unwrap();
        request.configuration = Some(SendMessageConfiguration {
            if_generation_match: Some(stored.generation + 1),
            return_immediately: None,
            accepted_output_modes: None,
            task_push_notification_config: None,
            history_length: None,
            timeline_length: None,
        });
        assert_eq!(
            validate_task_reference(Some(&stored), &request, "task")
                .unwrap_err()
                .code,
            error_code::TASK_GENERATION_MISMATCH
        );
    }

    #[test]
    fn cancellation_amends_and_announces_the_committed_generation_once() {
        let mut task = task_with_history("task", "context", 0);
        task.status.state = TaskState::Canceled;
        task.generation = 8;
        let mut entry = CancelEntry {
            recorded_by_store: true,
            generation: 8,
            announced: false,
        };
        assert!(!cancel_requires_transition(&task).unwrap());
        let mut event = status(TaskState::Canceled);
        let CancellationMutation::Amend(amended) =
            entry.plan_update(task.clone(), &mut event).unwrap()
        else {
            panic!("the hook must amend the store's cancellation");
        };
        assert_eq!(amended.generation, 8);
        assert_eq!(generation_of(&event), Some(8));
        assert!(matches!(
            entry.notification(Some(task.clone())),
            CancellationNotification::Publish(_)
        ));
        entry.note_published(true, &event);
        assert!(matches!(
            entry.notification(Some(task)),
            CancellationNotification::None
        ));
        assert!(
            entry
                .plan_update(amended, &mut status(TaskState::Working))
                .is_err()
        );
    }
}

#[cfg(test)]
mod error_policy_tests {
    use super::*;

    #[test]
    fn protocol_error_catalog_preserves_codes_and_messages() {
        for (violation, code, message) in [
            (
                ProtocolError::MissingOpeningRequest,
                error_code::INVALID_REQUEST,
                "a response needs a first request",
            ),
            (
                ProtocolError::UnknownStreamRequest,
                error_code::INVALID_REQUEST,
                "unrecognised StreamRequest payload",
            ),
            (
                ProtocolError::SubscriptionAfterOpening,
                error_code::INVALID_REQUEST,
                "subscribe is only valid as the first request of a stream",
            ),
            (
                ProtocolError::TaskTerminated("task"),
                error_code::UNSUPPORTED_OPERATION,
                "task task has already reached a terminal state",
            ),
            (
                ProtocolError::ExecutionInProgress,
                error_code::INVALID_REQUEST,
                "task execution is already in progress",
            ),
            (
                ProtocolError::InvalidPageToken,
                error_code::INVALID_PARAMS,
                "invalid page token",
            ),
            (
                ProtocolError::InvalidExchangeRoutingHeader,
                error_code::INVALID_PARAMS,
                "invalid live exchange routing header",
            ),
            (
                ProtocolError::ForeignExchangeCaller,
                error_code::INVALID_REQUEST,
                "live exchange belongs to a different caller",
            ),
        ] {
            let error = A2AError::from(violation);
            assert_eq!(error.code, code);
            assert_eq!(error.message, message);
        }
    }

    #[test]
    fn input_refusals_use_the_operation_and_phase_error_contract() {
        for mode in [StartMode::Blocking, StartMode::Streaming, StartMode::Bidi] {
            let policy = ResponsePolicy::new(mode, true);
            let error = policy.opening_error(Rejection::input_busy(), Some("task"));
            assert_eq!(
                error.code,
                if mode == StartMode::Bidi {
                    error_code::UNSUPPORTED_OPERATION
                } else {
                    error_code::INVALID_REQUEST
                }
            );
            assert!(
                error
                    .message
                    .contains("the agent is busy with earlier input")
            );
            let event = Rejection::input_busy().into_stream_error("task", Some("input".into()));
            let StreamResponse::Error(rejection) = &event else {
                panic!("expected an in-band live rejection")
            };
            assert_eq!(rejection.code, "UNAVAILABLE");
            assert_eq!(rejection.task_id, "task");
            assert_eq!(rejection.reference_id.as_deref(), Some("input"));
            assert_eq!(policy.after_event(&Ok(event)), StreamDecision::KeepOpen);
            for original in [
                A2AError::task_not_found("task"),
                A2AError::internal("backend failure"),
            ] {
                let returned =
                    policy.opening_error(Rejection::Failed(original.clone()), Some("task"));
                assert_eq!(returned.code, original.code);
                assert_eq!(returned.message, original.message);
                assert_eq!(returned.details, original.details);
            }
        }
    }

    #[test]
    fn push_config_cursor_errors_have_one_protocol_definition() {
        assert_eq!(push_config_page_offset(None).unwrap(), 0);
        assert_eq!(push_config_page_offset(Some("0")).unwrap(), 0);
        assert_eq!(push_config_page_offset(Some("0004")).unwrap(), 4);
        let overflowing = format!("{}0", usize::MAX);
        for token in ["", "-1", "opaque-token", overflowing.as_str()] {
            let error = push_config_page_offset(Some(token)).unwrap_err();
            assert_eq!(error.code, error_code::INVALID_PARAMS);
            assert_eq!(error.message, "invalid page token");
        }
    }
}
