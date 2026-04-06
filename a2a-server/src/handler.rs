// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use async_trait::async_trait;
use futures::{StreamExt, stream::BoxStream};
use std::{collections::HashMap, sync::Arc};
use tokio::sync::{RwLock, broadcast};

use crate::middleware::ServiceParams;

const EXECUTION_BUFFER_CAPACITY: usize = 32;

#[derive(Clone)]
struct ExecutionEvent {
    sequence: u64,
    result: Result<StreamResponse, A2AError>,
}

#[derive(Default)]
struct ActiveExecutionState {
    sequence: u64,
    snapshot_task: Option<Task>,
}

struct ActiveExecution {
    sender: broadcast::Sender<ExecutionEvent>,
    state: RwLock<ActiveExecutionState>,
}

impl ActiveExecution {
    fn new(task: Task) -> Arc<Self> {
        let (sender, _) = broadcast::channel(EXECUTION_BUFFER_CAPACITY);
        Arc::new(Self {
            sender,
            state: RwLock::new(ActiveExecutionState {
                sequence: 0,
                snapshot_task: Some(task),
            }),
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<ExecutionEvent> {
        self.sender.subscribe()
    }

    async fn resubscribe(&self) -> (broadcast::Receiver<ExecutionEvent>, Option<Task>, u64) {
        let receiver = self.sender.subscribe();
        let state = self.state.read().await;
        (receiver, state.snapshot_task.clone(), state.sequence)
    }

    async fn publish(&self, result: Result<StreamResponse, A2AError>, snapshot_task: Option<Task>) {
        let event = {
            let mut state = self.state.write().await;
            if let Some(task) = snapshot_task {
                state.snapshot_task = Some(task);
            }
            state.sequence += 1;
            ExecutionEvent {
                sequence: state.sequence,
                result,
            }
        };
        let _ = self.sender.send(event);
    }

    async fn snapshot_is_terminal(&self) -> bool {
        let state = self.state.read().await;
        state
            .snapshot_task
            .as_ref()
            .is_some_and(|task| task.status.state.is_terminal())
    }
}

#[derive(Default)]
struct ExecutionManager {
    executions: RwLock<HashMap<TaskId, Arc<ActiveExecution>>>,
}

impl ExecutionManager {
    async fn start(&self, task: Task) -> Result<Arc<ActiveExecution>, A2AError> {
        let mut executions = self.executions.write().await;
        if executions.contains_key(&task.id) {
            return Err(A2AError::invalid_request(format!(
                "task execution is already in progress: {}",
                task.id
            )));
        }

        let active = ActiveExecution::new(task.clone());
        executions.insert(task.id.clone(), active.clone());
        Ok(active)
    }

    async fn get(&self, task_id: &str) -> Option<Arc<ActiveExecution>> {
        let executions = self.executions.read().await;
        executions.get(task_id).cloned()
    }

    async fn resubscribe(
        &self,
        task_id: &str,
    ) -> Option<(broadcast::Receiver<ExecutionEvent>, Option<Task>, u64)> {
        let active = self.get(task_id).await?;
        let (receiver, snapshot_task, sequence) = active.resubscribe().await;
        Some((receiver, snapshot_task, sequence))
    }

    async fn finish(&self, task_id: &str, active: &Arc<ActiveExecution>) {
        let mut executions = self.executions.write().await;
        let should_remove = executions
            .get(task_id)
            .is_some_and(|current| Arc::ptr_eq(current, active));
        if should_remove {
            executions.remove(task_id);
        }
    }
}

struct ExecutionRuntime {
    executor: Arc<dyn crate::AgentExecutor>,
    task_store: Arc<dyn crate::TaskStore>,
    push_config_store: Option<Arc<dyn crate::PushConfigStore>>,
    push_sender: Option<Arc<crate::HttpPushSender>>,
    execution_manager: Arc<ExecutionManager>,
}

struct SubscriptionState {
    receiver: broadcast::Receiver<ExecutionEvent>,
    snapshot_task: Option<Task>,
    min_sequence: u64,
    done: bool,
}

fn is_terminal_event(event: &StreamResponse) -> bool {
    match event {
        StreamResponse::Task(task) => task.status.state.is_terminal(),
        StreamResponse::StatusUpdate(update) => update.status.state.is_terminal(),
        _ => false,
    }
}

fn task_id_for_event(event: &StreamResponse) -> Option<&str> {
    match event {
        StreamResponse::Task(task) => Some(&task.id),
        StreamResponse::StatusUpdate(update) => Some(&update.task_id),
        StreamResponse::ArtifactUpdate(update) => Some(&update.task_id),
        StreamResponse::Message(_) => None,
    }
}

fn should_interrupt_non_streaming(
    req: &SendMessageRequest,
    event: &StreamResponse,
) -> Option<TaskId> {
    if req
        .configuration
        .as_ref()
        .and_then(|config| config.return_immediately)
        .unwrap_or(false)
    {
        return match event {
            StreamResponse::Message(_) => None,
            _ => task_id_for_event(event).map(ToOwned::to_owned),
        };
    }

    match event {
        StreamResponse::Task(task) if task.status.state == TaskState::AuthRequired => {
            Some(task.id.clone())
        }
        StreamResponse::StatusUpdate(update) if update.status.state == TaskState::AuthRequired => {
            Some(update.task_id.clone())
        }
        _ => None,
    }
}

async fn send_push_notifications(
    task_id: &str,
    push_config_store: Option<&dyn crate::PushConfigStore>,
    push_sender: Option<&crate::HttpPushSender>,
    event: &StreamResponse,
) -> Result<(), A2AError> {
    let Some(push_config_store) = push_config_store else {
        return Ok(());
    };
    let Some(push_sender) = push_sender else {
        return Ok(());
    };

    let configs = push_config_store.list(task_id).await?;
    for config in configs {
        push_sender.send_push(&config, event.clone()).await?;
    }

    Ok(())
}

async fn save_task(task_store: &dyn crate::TaskStore, task: Task) -> Result<Task, A2AError> {
    match task_store.update(task.clone()).await {
        Ok(_) => Ok(task),
        Err(error) if error.code == error_code::TASK_NOT_FOUND => {
            task_store.create(task.clone()).await?;
            Ok(task)
        }
        Err(error) => Err(error),
    }
}

async fn apply_event_to_task(
    task_store: &dyn crate::TaskStore,
    current_task: Option<Task>,
    event: &StreamResponse,
) -> Result<Option<Task>, A2AError> {
    match event {
        StreamResponse::Task(task) => save_task(task_store, task.clone()).await.map(Some),
        StreamResponse::StatusUpdate(update) => {
            let mut task = current_task
                .or(task_store.get(&update.task_id).await?)
                .ok_or_else(|| A2AError::task_not_found(&update.task_id))?;
            task.status = update.status.clone();
            save_task(task_store, task).await.map(Some)
        }
        StreamResponse::ArtifactUpdate(update) => {
            let mut task = current_task
                .or(task_store.get(&update.task_id).await?)
                .ok_or_else(|| A2AError::task_not_found(&update.task_id))?;
            let artifacts = task.artifacts.get_or_insert_with(Vec::new);
            artifacts.push(update.artifact.clone());
            save_task(task_store, task).await.map(Some)
        }
        StreamResponse::Message(_) => Ok(current_task),
    }
}

fn subscription_stream(
    receiver: broadcast::Receiver<ExecutionEvent>,
    snapshot_task: Option<Task>,
    min_sequence: u64,
) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
    Box::pin(futures::stream::unfold(
        SubscriptionState {
            receiver,
            snapshot_task,
            min_sequence,
            done: false,
        },
        |mut state| async move {
            if state.done {
                return None;
            }

            if let Some(task) = state.snapshot_task.take() {
                if task.status.state.is_terminal() {
                    state.done = true;
                }
                return Some((Ok(StreamResponse::Task(task)), state));
            }

            loop {
                match state.receiver.recv().await {
                    Ok(event) if event.sequence <= state.min_sequence => continue,
                    Ok(event) => {
                        state.min_sequence = event.sequence;
                        match &event.result {
                            Ok(item) if is_terminal_event(item) => state.done = true,
                            Err(_) => state.done = true,
                            _ => {}
                        }
                        return Some((event.result.clone(), state));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        state.done = true;
                        return Some((
                            Err(A2AError::internal(
                                "subscription fell behind active execution",
                            )),
                            state,
                        ));
                    }
                    Err(broadcast::error::RecvError::Closed) => return None,
                }
            }
        },
    ))
}

async fn drive_execution(
    runtime: ExecutionRuntime,
    task_id: TaskId,
    active: Arc<ActiveExecution>,
    exec_ctx: crate::ExecutorContext,
) {
    let mut current_task = {
        let state = active.state.read().await;
        state.snapshot_task.clone()
    };
    let mut stream = runtime.executor.execute(exec_ctx);

    while let Some(result) = stream.next().await {
        if active.snapshot_is_terminal().await {
            break;
        }

        match result {
            Ok(event) => {
                match apply_event_to_task(runtime.task_store.as_ref(), current_task.clone(), &event)
                    .await
                {
                    Ok(updated_task) => {
                        current_task = updated_task.clone();
                        if let Err(error) = send_push_notifications(
                            &task_id,
                            runtime.push_config_store.as_deref(),
                            runtime.push_sender.as_deref(),
                            &event,
                        )
                        .await
                        {
                            active.publish(Err(error), None).await;
                            break;
                        }
                        active.publish(Ok(event.clone()), updated_task).await;
                        if is_terminal_event(&event) {
                            break;
                        }
                    }
                    Err(error) => {
                        active.publish(Err(error), None).await;
                        break;
                    }
                }
            }
            Err(error) => {
                active.publish(Err(error), None).await;
                break;
            }
        }
    }

    runtime.execution_manager.finish(&task_id, &active).await;
}

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
        req: CreateTaskPushNotificationConfigRequest,
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

/// Default implementation of [`RequestHandler`] that orchestrates
/// task lifecycle management, storage, and executor dispatch.
pub struct DefaultRequestHandler {
    executor: Arc<dyn crate::AgentExecutor>,
    task_store: Arc<dyn crate::TaskStore>,
    execution_manager: Arc<ExecutionManager>,
    push_config_store: Option<Arc<dyn crate::PushConfigStore>>,
    push_sender: Option<Arc<crate::HttpPushSender>>,
    capabilities: AgentCapabilities,
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
        }
    }

    pub fn with_push_config_store(
        mut self,
        push_config_store: impl crate::PushConfigStore,
    ) -> Self {
        self.push_sender = Some(Arc::new(crate::HttpPushSender::new(None)));
        self.push_config_store = Some(Arc::new(push_config_store));
        self.capabilities.push_notifications = Some(true);
        self
    }

    pub fn with_push_notifications(
        mut self,
        push_config_store: impl crate::PushConfigStore,
        push_sender: crate::HttpPushSender,
    ) -> Self {
        self.push_config_store = Some(Arc::new(push_config_store));
        self.push_sender = Some(Arc::new(push_sender));
        self.capabilities.push_notifications = Some(true);
        self
    }

    pub fn with_capabilities(mut self, capabilities: AgentCapabilities) -> Self {
        self.capabilities = capabilities;
        self
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

    async fn prepare_task_for_execution(
        &self,
        req: &SendMessageRequest,
    ) -> Result<(Task, Option<Task>, String), A2AError> {
        let task_id = req.message.task_id.clone().unwrap_or_else(new_task_id);
        let stored = self.task_store.get(&task_id).await?;
        let context_id = stored
            .as_ref()
            .map(|task| task.context_id.clone())
            .or_else(|| req.message.context_id.clone())
            .unwrap_or_else(new_context_id);

        let task = if let Some(existing) = stored.clone() {
            existing
        } else {
            let task = Task {
                id: task_id,
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
            self.task_store.create(task.clone()).await?;
            task
        };

        Ok((task, stored, context_id))
    }

    async fn save_request_push_config(
        &self,
        task_id: &str,
        req: &SendMessageRequest,
    ) -> Result<(), A2AError> {
        let Some(config) = req
            .configuration
            .as_ref()
            .and_then(|configuration| configuration.push_notification_config.clone())
        else {
            return Ok(());
        };

        self.push_config_store()?.save(task_id, config).await?;
        Ok(())
    }

    async fn start_execution(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
        include_task_snapshot_in_context: bool,
    ) -> Result<(TaskId, BoxStream<'static, Result<StreamResponse, A2AError>>), A2AError> {
        let (task, stored_task, context_id) = self.prepare_task_for_execution(&req).await?;
        let task_id = task.id.clone();
        self.save_request_push_config(&task_id, &req).await?;
        let active = self.execution_manager.start(task.clone()).await?;
        let receiver = active.subscribe();

        let exec_ctx = crate::ExecutorContext {
            message: Some(req.message),
            task_id: task_id.clone(),
            stored_task: if include_task_snapshot_in_context {
                Some(task.clone())
            } else {
                stored_task
            },
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
        };
        let active_execution = active.clone();
        let execution_task_id = task_id.clone();

        tokio::spawn(async move {
            drive_execution(runtime, execution_task_id, active_execution, exec_ctx).await;
        });

        Ok((task_id, subscription_stream(receiver, None, 0)))
    }
}

#[async_trait]
impl RequestHandler for DefaultRequestHandler {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        let (task_id, mut stream) = self.start_execution(params, req.clone(), true).await?;
        let mut last_event = None;

        while let Some(item) = stream.next().await {
            let event = item?;

            if let Some(interrupt_task_id) = should_interrupt_non_streaming(&req, &event) {
                return Ok(SendMessageResponse::Task(
                    self.load_task(&interrupt_task_id).await?,
                ));
            }

            match event {
                StreamResponse::Message(message) => {
                    return Ok(SendMessageResponse::Message(message));
                }
                other => last_event = Some(other),
            }
        }

        match last_event {
            Some(StreamResponse::Task(task)) => Ok(SendMessageResponse::Task(task)),
            Some(StreamResponse::StatusUpdate(update)) => Ok(SendMessageResponse::Task(
                self.load_task(&update.task_id).await?,
            )),
            Some(StreamResponse::ArtifactUpdate(update)) => Ok(SendMessageResponse::Task(
                self.load_task(&update.task_id).await?,
            )),
            Some(StreamResponse::Message(message)) => Ok(SendMessageResponse::Message(message)),
            None => Ok(SendMessageResponse::Task(self.load_task(&task_id).await?)),
        }
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let (_, stream) = self.start_execution(params, req, false).await?;
        Ok(stream)
    }

    async fn get_task(
        &self,
        _params: &ServiceParams,
        req: GetTaskRequest,
    ) -> Result<Task, A2AError> {
        self.task_store
            .get(&req.id)
            .await?
            .ok_or_else(|| A2AError::task_not_found(&req.id))
    }

    async fn list_tasks(
        &self,
        _params: &ServiceParams,
        req: ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        self.task_store.list(&req).await
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        let task = self.load_task(&req.id).await?;

        if task.status.state.is_terminal() {
            return Err(A2AError::task_not_cancelable(&req.id));
        }

        let active_execution = self.execution_manager.get(&req.id).await;

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

        let mut stream = self.executor.cancel(exec_ctx);
        let mut current_task = Some(task);

        while let Some(event) = stream.next().await {
            match event {
                Ok(event) => {
                    let updated_task =
                        apply_event_to_task(self.task_store.as_ref(), current_task.clone(), &event)
                            .await?;
                    current_task = updated_task.clone();

                    send_push_notifications(
                        &req.id,
                        self.push_config_store.as_deref(),
                        self.push_sender.as_deref(),
                        &event,
                    )
                    .await?;

                    if let Some(active) = &active_execution {
                        active.publish(Ok(event.clone()), updated_task).await;
                    }

                    if is_terminal_event(&event) {
                        break;
                    }
                }
                Err(error) => {
                    if let Some(active) = &active_execution {
                        active.publish(Err(error.clone()), None).await;
                    }
                    return Err(error);
                }
            }
        }

        if let Some(active) = active_execution {
            self.execution_manager.finish(&req.id, &active).await;
        }

        self.load_task(&req.id).await
    }

    async fn subscribe_to_task(
        &self,
        _params: &ServiceParams,
        req: SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let (receiver, snapshot_task, sequence) = self
            .execution_manager
            .resubscribe(&req.id)
            .await
            .ok_or_else(|| A2AError::task_not_found(&req.id))?;
        Ok(subscription_stream(receiver, snapshot_task, sequence))
    }

    async fn create_push_config(
        &self,
        _params: &ServiceParams,
        req: CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let saved = self
            .push_config_store()?
            .save(&req.task_id, req.config)
            .await?;
        Ok(TaskPushNotificationConfig {
            task_id: req.task_id,
            config: saved,
            tenant: req.tenant,
        })
    }

    async fn get_push_config(
        &self,
        _params: &ServiceParams,
        req: GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let config = self.push_config_store()?.get(&req.task_id, &req.id).await?;
        Ok(TaskPushNotificationConfig {
            task_id: req.task_id,
            config,
            tenant: req.tenant,
        })
    }

    async fn list_push_configs(
        &self,
        _params: &ServiceParams,
        req: ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let mut configs = self.push_config_store()?.list(&req.task_id).await?;
        configs.sort_by(|left, right| left.id.cmp(&right.id));

        let page_size = match req.page_size {
            Some(size) if size > 0 => size as usize,
            _ => 50,
        };
        let start = req
            .page_token
            .as_deref()
            .and_then(|token| token.parse::<usize>().ok())
            .unwrap_or(0)
            .min(configs.len());
        let end = (start + page_size).min(configs.len());
        let next_page_token = (end < configs.len()).then(|| end.to_string());

        Ok(ListTaskPushNotificationConfigsResponse {
            configs: configs[start..end]
                .iter()
                .cloned()
                .map(|config| TaskPushNotificationConfig {
                    task_id: req.task_id.clone(),
                    config,
                    tenant: req.tenant.clone(),
                })
                .collect(),
            next_page_token,
        })
    }

    async fn delete_push_config(
        &self,
        _params: &ServiceParams,
        req: DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        self.push_config_store()?
            .delete(&req.task_id, &req.id)
            .await
    }

    async fn get_extended_agent_card(
        &self,
        _params: &ServiceParams,
        _req: GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        Err(A2AError::unsupported_operation(
            "extended agent card not configured",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Bytes,
        extract::State,
        http::{HeaderMap, StatusCode, header},
        routing::post,
    };
    use crate::executor::ExecutorContext;
    use crate::push::InMemoryPushConfigStore;
    use crate::task_store::InMemoryTaskStore;
    use futures::stream;
    use std::sync::Arc;
    use tokio::{
        net::TcpListener,
        sync::{Notify, mpsc, oneshot},
        time::{Duration, timeout},
    };

    struct EchoExecutor;

    struct PushEventExecutor;

    struct ResumableExecutor {
        release: Arc<Notify>,
    }

    #[derive(Debug)]
    struct CapturedPush {
        authorization: Option<String>,
        notification_token: Option<String>,
        event: StreamResponse,
    }

    impl crate::AgentExecutor for EchoExecutor {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let task = Task {
                id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: ctx.message,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            };
            Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
        }

        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let task = Task {
                id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            };
            Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
        }
    }

    impl crate::AgentExecutor for PushEventExecutor {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let working = StreamResponse::StatusUpdate(TaskStatusUpdateEvent {
                task_id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                metadata: None,
            });
            let completed = StreamResponse::Task(Task {
                id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: ctx.message,
                    timestamp: None,
                },
                artifacts: None,
                history: ctx.stored_task.and_then(|task| task.history),
                metadata: None,
            });

            Box::pin(stream::iter(vec![Ok(working), Ok(completed)]))
        }

        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let task = Task {
                id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            };
            Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
        }
    }

    impl crate::AgentExecutor for ResumableExecutor {
        fn execute(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let release = self.release.clone();
            let working_task = Task {
                id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Working,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: ctx.stored_task.and_then(|task| task.history),
                metadata: None,
            };
            let completed_task = Task {
                id: working_task.id.clone(),
                context_id: working_task.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Completed,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: working_task.history.clone(),
                metadata: None,
            };

            Box::pin(stream::unfold(
                (Some(working_task), Some((release, completed_task))),
                |(working_task, completion)| async move {
                    if let Some(task) = working_task {
                        return Some((Ok(StreamResponse::Task(task)), (None, completion)));
                    }

                    if let Some((release, task)) = completion {
                        release.notified().await;
                        return Some((Ok(StreamResponse::Task(task)), (None, None)));
                    }

                    None
                },
            ))
        }

        fn cancel(
            &self,
            ctx: ExecutorContext,
        ) -> BoxStream<'static, Result<StreamResponse, A2AError>> {
            let task = Task {
                id: ctx.task_id.clone(),
                context_id: ctx.context_id.clone(),
                status: TaskStatus {
                    state: TaskState::Canceled,
                    message: None,
                    timestamp: None,
                },
                artifacts: None,
                history: None,
                metadata: None,
            };
            Box::pin(stream::once(async move { Ok(StreamResponse::Task(task)) }))
        }
    }

    fn make_handler() -> DefaultRequestHandler {
        DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
    }

    fn make_handler_with_push_configs() -> DefaultRequestHandler {
        DefaultRequestHandler::new(EchoExecutor, InMemoryTaskStore::new())
            .with_push_config_store(InMemoryPushConfigStore::new())
    }

    fn make_push_delivery_handler() -> DefaultRequestHandler {
        DefaultRequestHandler::new(PushEventExecutor, InMemoryTaskStore::new())
            .with_push_config_store(InMemoryPushConfigStore::new())
    }

    fn make_resumable_handler() -> (DefaultRequestHandler, Arc<Notify>) {
        let release = Arc::new(Notify::new());
        (
            DefaultRequestHandler::new(
                ResumableExecutor {
                    release: release.clone(),
                },
                InMemoryTaskStore::new(),
            ),
            release,
        )
    }

    fn make_message() -> Message {
        Message::new(Role::User, vec![Part::text("hello")])
    }

    async fn capture_push(
        State(sender): State<mpsc::UnboundedSender<CapturedPush>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> StatusCode {
        let captured = CapturedPush {
            authorization: headers
                .get(header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned),
            notification_token: headers
                .get("A2A-Notification-Token")
                .and_then(|value| value.to_str().ok())
                .map(ToOwned::to_owned),
            event: serde_json::from_slice(&body).unwrap(),
        };
        sender.send(captured).unwrap();
        StatusCode::ACCEPTED
    }

    async fn start_push_webhook(
    ) -> (
        String,
        mpsc::UnboundedReceiver<CapturedPush>,
        oneshot::Sender<()>,
        tokio::task::JoinHandle<()>,
    ) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let app = Router::new().route("/", post(capture_push)).with_state(sender);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        (format!("http://{address}/"), receiver, shutdown_tx, server)
    }

    async fn next_push(receiver: &mut mpsc::UnboundedReceiver<CapturedPush>) -> CapturedPush {
        timeout(Duration::from_secs(5), receiver.recv())
            .await
            .unwrap()
            .unwrap()
    }

    #[tokio::test]
    async fn test_send_message_creates_task() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = SendMessageRequest {
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
            id: "nonexistent".into(),
            history_length: None,
            tenant: None,
        };
        let result = handler.get_task(&params, req).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_task_after_send() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let mut msg = make_message();
        msg.task_id = Some("t1".into());
        msg.context_id = Some("c1".into());
        let req = SendMessageRequest {
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
    async fn test_cancel_task_already_completed() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let task = Task {
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
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_subscribe_requires_active_execution() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let req = SubscribeToTaskRequest {
            id: "t1".into(),
            tenant: None,
        };
        let result = handler.subscribe_to_task(&params, req).await;
        match result {
            Ok(_) => panic!("expected subscribe_to_task to fail"),
            Err(error) => assert_eq!(error.code, error_code::TASK_NOT_FOUND),
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

        let response = handler
            .send_message(
                &params,
                SendMessageRequest {
                    message,
                    configuration: Some(SendMessageConfiguration {
                        accepted_output_modes: None,
                        push_notification_config: None,
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
                    id: "t-resume".into(),
                    tenant: None,
                },
            )
            .await;
        match result {
            Ok(_) => panic!("expected subscribe_to_task to fail after completion"),
            Err(error) => assert_eq!(error.code, error_code::TASK_NOT_FOUND),
        }
    }

    #[tokio::test]
    async fn test_push_config_not_supported_without_store() {
        let handler = make_handler();
        let params = ServiceParams::new();
        let result = handler
            .create_push_config(
                &params,
                CreateTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    config: PushNotificationConfig {
                        url: "http://example.com".into(),
                        id: None,
                        token: None,
                        authentication: None,
                    },
                    tenant: None,
                },
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_push_config_crud_with_store() {
        let handler = make_handler_with_push_configs();
        let params = ServiceParams::new();

        let created = handler
            .create_push_config(
                &params,
                CreateTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    config: PushNotificationConfig {
                        url: "https://example.com/first".into(),
                        id: Some("cfg-1".into()),
                        token: None,
                        authentication: None,
                    },
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(created.config.id.as_deref(), Some("cfg-1"));

        handler
            .create_push_config(
                &params,
                CreateTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    config: PushNotificationConfig {
                        url: "https://example.com/second".into(),
                        id: Some("cfg-2".into()),
                        token: None,
                        authentication: None,
                    },
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();

        let fetched = handler
            .get_push_config(
                &params,
                GetTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    id: "cfg-1".into(),
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(fetched.config.url, "https://example.com/first");

        let first_page = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: Some(1),
                    page_token: Some("0".into()),
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(first_page.configs.len(), 1);
        assert_eq!(first_page.configs[0].config.id.as_deref(), Some("cfg-1"));
        assert_eq!(first_page.next_page_token.as_deref(), Some("1"));

        let default_page = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: Some(0),
                    page_token: None,
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(default_page.configs.len(), 2);
        assert!(default_page.next_page_token.is_none());

        handler
            .delete_push_config(
                &params,
                DeleteTaskPushNotificationConfigRequest {
                    task_id: "t1".into(),
                    id: "cfg-1".into(),
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();

        let remaining = handler
            .list_push_configs(
                &params,
                ListTaskPushNotificationConfigsRequest {
                    task_id: "t1".into(),
                    page_size: None,
                    page_token: None,
                    tenant: Some("tenant-a".into()),
                },
            )
            .await
            .unwrap();
        assert_eq!(remaining.configs.len(), 1);
        assert_eq!(remaining.configs[0].config.id.as_deref(), Some("cfg-2"));
    }

    #[tokio::test]
    async fn test_send_message_request_push_config_delivers_webhooks() {
        let (url, mut receiver, shutdown_tx, server) = start_push_webhook().await;
        let handler = make_push_delivery_handler();
        let params = ServiceParams::new();
        let mut message = make_message();
        message.task_id = Some("t-push-request".into());
        message.context_id = Some("c-push-request".into());

        let response = handler
            .send_message(
                &params,
                SendMessageRequest {
                    message,
                    configuration: Some(SendMessageConfiguration {
                        accepted_output_modes: None,
                        push_notification_config: Some(PushNotificationConfig {
                            url: url.clone(),
                            id: Some("cfg-request".into()),
                            token: Some("notify-token".into()),
                            authentication: Some(AuthenticationInfo {
                                scheme: "bearer".into(),
                                credentials: Some("secret-token".into()),
                            }),
                        }),
                        history_length: None,
                        return_immediately: None,
                    }),
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        match response {
            SendMessageResponse::Task(task) => assert_eq!(task.status.state, TaskState::Completed),
            _ => panic!("expected Task response"),
        }

        let saved = handler
            .get_push_config(
                &params,
                GetTaskPushNotificationConfigRequest {
                    task_id: "t-push-request".into(),
                    id: "cfg-request".into(),
                    tenant: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(saved.config.url, url);

        let first = next_push(&mut receiver).await;
        assert_eq!(first.authorization.as_deref(), Some("Bearer secret-token"));
        assert_eq!(first.notification_token.as_deref(), Some("notify-token"));
        match first.event {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.task_id, "t-push-request");
                assert_eq!(update.status.state, TaskState::Working);
            }
            _ => panic!("expected status update push"),
        }

        let second = next_push(&mut receiver).await;
        assert_eq!(second.authorization.as_deref(), Some("Bearer secret-token"));
        assert_eq!(second.notification_token.as_deref(), Some("notify-token"));
        match second.event {
            StreamResponse::Task(task) => {
                assert_eq!(task.id, "t-push-request");
                assert_eq!(task.status.state, TaskState::Completed);
            }
            _ => panic!("expected final task push"),
        }

        shutdown_tx.send(()).unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn test_send_message_stored_push_config_delivers_webhooks() {
        let (url, mut receiver, shutdown_tx, server) = start_push_webhook().await;
        let handler = make_push_delivery_handler();
        let params = ServiceParams::new();

        handler
            .create_push_config(
                &params,
                CreateTaskPushNotificationConfigRequest {
                    task_id: "t-push-stored".into(),
                    config: PushNotificationConfig {
                        url: url.clone(),
                        id: Some("cfg-stored".into()),
                        token: Some("stored-token".into()),
                        authentication: None,
                    },
                    tenant: None,
                },
            )
            .await
            .unwrap();

        let mut message = make_message();
        message.task_id = Some("t-push-stored".into());
        message.context_id = Some("c-push-stored".into());
        let response = handler
            .send_message(
                &params,
                SendMessageRequest {
                    message,
                    configuration: None,
                    metadata: None,
                    tenant: None,
                },
            )
            .await
            .unwrap();

        match response {
            SendMessageResponse::Task(task) => assert_eq!(task.status.state, TaskState::Completed),
            _ => panic!("expected Task response"),
        }

        let first = next_push(&mut receiver).await;
        assert_eq!(first.notification_token.as_deref(), Some("stored-token"));
        match first.event {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.task_id, "t-push-stored");
                assert_eq!(update.status.state, TaskState::Working);
            }
            _ => panic!("expected status update push"),
        }

        let second = next_push(&mut receiver).await;
        assert_eq!(second.notification_token.as_deref(), Some("stored-token"));
        match second.event {
            StreamResponse::Task(task) => {
                assert_eq!(task.id, "t-push-stored");
                assert_eq!(task.status.state, TaskState::Completed);
            }
            _ => panic!("expected final task push"),
        }

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
            streaming: Some(true),
            push_notifications: None,
            extensions: None,
            extended_agent_card: None,
        });
        assert_eq!(handler.capabilities.streaming, Some(true));
    }

    #[test]
    fn test_with_push_config_store_enables_push_capability() {
        let handler = make_handler_with_push_configs();
        assert_eq!(handler.capabilities.push_notifications, Some(true));
    }
}
