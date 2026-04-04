// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use a2a::{A2AError, StreamResponse};
use a2a_pb::pbconv;
use a2a_pb::proto;
use a2a_server::RequestHandler;
use futures::StreamExt;

use crate::common::{
    A2A_SLIMRPC_SERVICE, METHOD_CANCEL_TASK, METHOD_CREATE_PUSH_CONFIG, METHOD_DELETE_PUSH_CONFIG,
    METHOD_GET_EXTENDED_AGENT_CARD, METHOD_GET_PUSH_CONFIG, METHOD_GET_TASK,
    METHOD_LIST_PUSH_CONFIGS, METHOD_LIST_TASKS, METHOD_SEND_MESSAGE,
    METHOD_SEND_STREAMING_MESSAGE, METHOD_SUBSCRIBE_TO_TASK, ServiceParamsMap,
    context_to_service_params, decode_proto_request, encode_proto_message,
};
use crate::errors::a2a_error_to_rpc_error;

type HandlerFuture<T> = Pin<Box<dyn Future<Output = Result<T, A2AError>> + Send>>;

/// Registers A2A request handling methods on a `slim_bindings::Server`.
pub struct SlimRpcHandler<H: RequestHandler> {
    handler: Arc<H>,
}

impl<H: RequestHandler> SlimRpcHandler<H> {
    pub fn new(handler: Arc<H>) -> Self {
        Self { handler }
    }

    pub fn register(&self, server: &slim_bindings::Server) {
        register_unary_unary::<H, proto::SendMessageRequest, _, _, _, _>(
            server,
            self.handler.clone(),
            METHOD_SEND_MESSAGE,
            "SendMessageRequest",
            pbconv::from_proto_send_message_request,
            |handler, params, request| {
                Box::pin(async move { handler.send_message(&params, request).await })
            },
            |response| encode_proto_message(&pbconv::to_proto_send_message_response(response)),
        );

        register_unary_stream_send_message(server, self.handler.clone());

        register_unary_unary::<H, proto::GetTaskRequest, _, _, _, _>(
            server,
            self.handler.clone(),
            METHOD_GET_TASK,
            "GetTaskRequest",
            pbconv::from_proto_get_task_request,
            |handler, params, request| {
                Box::pin(async move { handler.get_task(&params, request).await })
            },
            |response| encode_proto_message(&pbconv::to_proto_task(response)),
        );

        register_unary_unary::<H, proto::ListTasksRequest, _, _, _, _>(
            server,
            self.handler.clone(),
            METHOD_LIST_TASKS,
            "ListTasksRequest",
            pbconv::from_proto_list_tasks_request,
            |handler, params, request| {
                Box::pin(async move { handler.list_tasks(&params, request).await })
            },
            |response| encode_proto_message(&pbconv::to_proto_list_tasks_response(response)),
        );

        register_unary_unary::<H, proto::CancelTaskRequest, _, _, _, _>(
            server,
            self.handler.clone(),
            METHOD_CANCEL_TASK,
            "CancelTaskRequest",
            pbconv::from_proto_cancel_task_request,
            |handler, params, request| {
                Box::pin(async move { handler.cancel_task(&params, request).await })
            },
            |response| encode_proto_message(&pbconv::to_proto_task(response)),
        );

        register_unary_stream_subscribe_to_task(server, self.handler.clone());

        register_unary_unary::<H, proto::TaskPushNotificationConfig, _, _, _, _>(
            server,
            self.handler.clone(),
            METHOD_CREATE_PUSH_CONFIG,
            "TaskPushNotificationConfig",
            pbconv::from_proto_create_task_push_notification_config_request,
            |handler, params, request| {
                Box::pin(async move { handler.create_push_config(&params, request).await })
            },
            |response| {
                encode_proto_message(&pbconv::to_proto_task_push_notification_config(response))
            },
        );

        register_unary_unary::<H, proto::GetTaskPushNotificationConfigRequest, _, _, _, _>(
            server,
            self.handler.clone(),
            METHOD_GET_PUSH_CONFIG,
            "GetTaskPushNotificationConfigRequest",
            pbconv::from_proto_get_task_push_notification_config_request,
            |handler, params, request| {
                Box::pin(async move { handler.get_push_config(&params, request).await })
            },
            |response| {
                encode_proto_message(&pbconv::to_proto_task_push_notification_config(response))
            },
        );

        register_unary_unary::<H, proto::ListTaskPushNotificationConfigsRequest, _, _, _, _>(
            server,
            self.handler.clone(),
            METHOD_LIST_PUSH_CONFIGS,
            "ListTaskPushNotificationConfigsRequest",
            pbconv::from_proto_list_task_push_notification_configs_request,
            |handler, params, request| {
                Box::pin(async move { handler.list_push_configs(&params, request).await })
            },
            |response| {
                encode_proto_message(
                    &pbconv::to_proto_list_task_push_notification_configs_response(response),
                )
            },
        );

        register_unary_unary::<H, proto::GetExtendedAgentCardRequest, _, _, _, _>(
            server,
            self.handler.clone(),
            METHOD_GET_EXTENDED_AGENT_CARD,
            "GetExtendedAgentCardRequest",
            pbconv::from_proto_get_extended_agent_card_request,
            |handler, params, request| {
                Box::pin(async move { handler.get_extended_agent_card(&params, request).await })
            },
            |response| encode_proto_message(&pbconv::to_proto_agent_card(response)),
        );

        register_unary_unary::<H, proto::DeleteTaskPushNotificationConfigRequest, _, _, _, _>(
            server,
            self.handler.clone(),
            METHOD_DELETE_PUSH_CONFIG,
            "DeleteTaskPushNotificationConfigRequest",
            pbconv::from_proto_delete_task_push_notification_config_request,
            |handler, params, request| {
                Box::pin(async move {
                    handler.delete_push_config(&params, request).await?;
                    Ok(())
                })
            },
            |_| vec![0],
        );
    }
}

fn register_unary_unary<H, ReqProto, ReqNative, ResNative, DecodeReq, EncodeRes>(
    server: &slim_bindings::Server,
    handler: Arc<H>,
    method_name: &'static str,
    request_name: &'static str,
    decode_req: DecodeReq,
    call: impl Fn(Arc<H>, ServiceParamsMap, ReqNative) -> HandlerFuture<ResNative>
    + Send
    + Sync
    + 'static,
    encode_res: EncodeRes,
) where
    H: RequestHandler,
    ReqProto: prost::Message + Default + Send + 'static,
    ReqNative: Send + 'static,
    ResNative: Send + 'static,
    DecodeReq: Fn(&ReqProto) -> ReqNative + Send + Sync + 'static,
    EncodeRes: Fn(&ResNative) -> Vec<u8> + Send + Sync + 'static,
{
    let decode_req = Arc::new(decode_req);
    let call = Arc::new(call);
    let encode_res = Arc::new(encode_res);

    server.register_unary_unary_internal(
        A2A_SLIMRPC_SERVICE,
        method_name,
        move |request: Vec<u8>, context: slim_bindings::Context| {
            let handler = handler.clone();
            let decode_req = decode_req.clone();
            let call = call.clone();
            let encode_res = encode_res.clone();

            async move {
                let proto_request = decode_proto_request::<ReqProto>(request, request_name)?;
                let params = context_to_service_params(&context);
                let request = decode_req(&proto_request);
                let response = call(handler, params, request)
                    .await
                    .map_err(|error| a2a_error_to_rpc_error(&error))?;
                Ok(encode_res(&response))
            }
        },
    );
}

fn register_unary_stream_send_message<H>(server: &slim_bindings::Server, handler: Arc<H>)
where
    H: RequestHandler,
{
    server.register_unary_stream_internal(
        A2A_SLIMRPC_SERVICE,
        METHOD_SEND_STREAMING_MESSAGE,
        move |request: Vec<u8>, context: slim_bindings::Context| {
            let handler = handler.clone();

            async move {
                let proto_request = decode_proto_request::<proto::SendMessageRequest>(
                    request,
                    "SendMessageRequest",
                )?;
                let params = context_to_service_params(&context);
                let request = pbconv::from_proto_send_message_request(&proto_request);
                let stream = handler
                    .send_streaming_message(&params, request)
                    .await
                    .map_err(|error| a2a_error_to_rpc_error(&error))?;

                Ok(stream.map(map_stream_response))
            }
        },
    );
}

fn register_unary_stream_subscribe_to_task<H>(server: &slim_bindings::Server, handler: Arc<H>)
where
    H: RequestHandler,
{
    server.register_unary_stream_internal(
        A2A_SLIMRPC_SERVICE,
        METHOD_SUBSCRIBE_TO_TASK,
        move |request: Vec<u8>, context: slim_bindings::Context| {
            let handler = handler.clone();

            async move {
                let proto_request = decode_proto_request::<proto::SubscribeToTaskRequest>(
                    request,
                    "SubscribeToTaskRequest",
                )?;
                let params = context_to_service_params(&context);
                let request = pbconv::from_proto_subscribe_to_task_request(&proto_request);
                let stream = handler
                    .subscribe_to_task(&params, request)
                    .await
                    .map_err(|error| a2a_error_to_rpc_error(&error))?;

                Ok(stream.map(map_stream_response))
            }
        },
    );
}

fn map_stream_response(
    response: Result<StreamResponse, A2AError>,
) -> Result<Vec<u8>, slim_bindings::RpcError> {
    response
        .map(|response| encode_proto_message(&pbconv::to_proto_stream_response(&response)))
        .map_err(|error| a2a_error_to_rpc_error(&error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_slimrpc_handler_new() {
        use a2a::*;
        use a2a_server::handler::DefaultRequestHandler;
        use a2a_server::task_store::InMemoryTaskStore;

        struct NoopExecutor;

        impl a2a_server::AgentExecutor for NoopExecutor {
            fn execute(
                &self,
                _ctx: a2a_server::executor::ExecutorContext,
            ) -> futures::stream::BoxStream<'static, Result<a2a::event::StreamResponse, A2AError>>
            {
                Box::pin(futures::stream::empty())
            }

            fn cancel(
                &self,
                _ctx: a2a_server::executor::ExecutorContext,
            ) -> futures::stream::BoxStream<'static, Result<a2a::event::StreamResponse, A2AError>>
            {
                Box::pin(futures::stream::empty())
            }
        }

        let handler = Arc::new(DefaultRequestHandler::new(
            NoopExecutor,
            InMemoryTaskStore::new(),
        ));
        let slimrpc_handler = SlimRpcHandler::new(handler);
        let _ = slimrpc_handler;
    }
}
