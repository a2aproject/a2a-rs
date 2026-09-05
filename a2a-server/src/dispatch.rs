// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

use crate::handler::RequestHandler;
use crate::middleware::ServiceParams;
use a2a::*;
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use futures::{StreamExt, stream::BoxStream};
use serde_json::Value;

fn decode<T: ProtoJsonPayload>(value: Value) -> Result<T, A2AError> {
    protojson_conv::from_value(value).map_err(parse_error)
}

fn encode<T: ProtoJsonPayload>(value: &T) -> Result<Value, A2AError> {
    protojson_conv::to_value(value)
        .map_err(|e| A2AError::internal(format!("failed to serialize ProtoJSON payload: {e}")))
}

fn encode_stream(
    stream: BoxStream<'static, Result<StreamResponse, A2AError>>,
) -> BoxStream<'static, Result<Value, A2AError>> {
    Box::pin(stream.map(|item| {
        item.and_then(|value| {
            protojson_conv::to_value(&value).map_err(|e| {
                A2AError::internal(format!("failed to serialize ProtoJSON stream payload: {e}"))
            })
        })
    }))
}

fn parse_error(e: impl std::fmt::Display) -> A2AError {
    A2AError {
        code: error_code::PARSE_ERROR,
        message: format!("invalid params: {e}"),
        details: None,
    }
}

pub async fn dispatch_unary<H>(
    handler: &H,
    params: &ServiceParams,
    method: &str,
    raw_params: Value,
) -> Result<Value, A2AError>
where
    H: RequestHandler + ?Sized,
{
    match method {
        methods::SEND_MESSAGE => encode(&handler.send_message(params, decode(raw_params)?).await?),
        methods::GET_TASK => encode(&handler.get_task(params, decode(raw_params)?).await?),
        methods::LIST_TASKS => encode(&handler.list_tasks(params, decode(raw_params)?).await?),
        methods::CANCEL_TASK => encode(&handler.cancel_task(params, decode(raw_params)?).await?),

        methods::CREATE_PUSH_CONFIG => {
            let req: TaskPushNotificationConfig = decode(raw_params)?;
            encode(&handler.create_push_config(params, req).await?)
        }
        methods::GET_PUSH_CONFIG => {
            encode(&handler.get_push_config(params, decode(raw_params)?).await?)
        }
        methods::LIST_PUSH_CONFIGS => encode(
            &handler
                .list_push_configs(params, decode(raw_params)?)
                .await?,
        ),
        methods::DELETE_PUSH_CONFIG => {
            handler
                .delete_push_config(params, decode(raw_params)?)
                .await?;
            Ok(Value::Null)
        }
        methods::GET_EXTENDED_AGENT_CARD => encode(
            &handler
                .get_extended_agent_card(params, decode(raw_params)?)
                .await?,
        ),

        "" => Err(A2AError::invalid_request("method is required")),
        _ => Err(A2AError::method_not_found(method)),
    }
}

pub async fn dispatch_streaming<H>(
    handler: &H,
    params: &ServiceParams,
    method: &str,
    raw_params: Value,
) -> Result<BoxStream<'static, Result<Value, A2AError>>, A2AError>
where
    H: RequestHandler + ?Sized,
{
    match method {
        methods::SEND_STREAMING_MESSAGE => {
            let s = handler
                .send_streaming_message(params, decode(raw_params)?)
                .await?;
            Ok(encode_stream(s))
        }
        methods::SUBSCRIBE_TO_TASK => {
            let s = handler
                .subscribe_to_task(params, decode(raw_params)?)
                .await?;
            Ok(encode_stream(s))
        }
        _ => Err(A2AError::method_not_found(method)),
    }
}
