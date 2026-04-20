// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! STDIO server — reads JSON-RPC requests from stdin, dispatches to a
//! `RequestHandler`, and writes responses to stdout.

use std::sync::Arc;

use a2a::*;
use a2a_server::RequestHandler;
use a2a_server::middleware::ServiceParams;
use futures::StreamExt;
use tokio::io::{AsyncWrite, BufReader};

use crate::errors::StdioError;
use crate::framing;
use crate::handshake::{self, Handshake, HandshakeFeatures};

/// Dispatch a unary (request → response) JSON-RPC method.
///
/// Deserialises `$params` into `$Req`, calls `$handler.$method(…)`,
/// serialises the response or error, and writes the frame.
/// Uses `continue` to skip to the next request on parse errors,
/// so this macro must be invoked inside the request loop.
macro_rules! dispatch_unary {
    ($writer:expr, $id:expr, $handler:expr, $sp:expr, $params:expr,
     $Req:ty, $method:ident) => {{
        let req: $Req = match serde_json::from_value($params) {
            Ok(r) => r,
            Err(e) => {
                write_error(
                    &mut $writer,
                    $id,
                    error_code::INVALID_PARAMS,
                    &format!("invalid params: {e}"),
                )
                .await?;
                continue;
            }
        };
        match $handler.$method(&$sp, req).await {
            Ok(value) => {
                let result_value = serde_json::to_value(&value).map_err(StdioError::Json)?;
                let resp = JsonRpcResponse::success($id, result_value);
                let body = serde_json::to_vec(&resp)?;
                framing::write_frame(&mut $writer, &body).await?;
            }
            Err(e) => {
                write_error(&mut $writer, $id, e.code, &e.message).await?;
            }
        }
    }};
}

/// Dispatch a streaming JSON-RPC method.
///
/// Like `dispatch_unary!` but the handler returns a `BoxStream` of events.
/// Each event is sent as a JSON-RPC notification, followed by a final
/// success response carrying the original request id.
macro_rules! dispatch_streaming {
    ($writer:expr, $id:expr, $handler:expr, $sp:expr, $params:expr,
     $Req:ty, $method:ident) => {{
        let req: $Req = match serde_json::from_value($params) {
            Ok(r) => r,
            Err(e) => {
                write_error(&mut $writer, $id, error_code::INVALID_PARAMS, &format!("invalid params: {e}")).await?;
                continue;
            }
        };
        match $handler.$method(&$sp, req).await {
            Ok(mut stream) => {
                while let Some(event) = stream.next().await {
                    let notification = match &event {
                        Ok(sr) => {
                            let params = serde_json::to_value(sr).map_err(StdioError::Json)?;
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "event/streamResponse",
                                "params": params
                            })
                        }
                        Err(e) => {
                            serde_json::json!({
                                "jsonrpc": "2.0",
                                "method": "event/streamError",
                                "params": {
                                    "code": e.code,
                                    "message": e.message
                                }
                            })
                        }
                    };
                    let body = serde_json::to_vec(&notification)?;
                    framing::write_frame(&mut $writer, &body).await?;
                }
                // Final response indicating stream complete.
                let resp = JsonRpcResponse::success($id, serde_json::Value::Null);
                let body = serde_json::to_vec(&resp)?;
                framing::write_frame(&mut $writer, &body).await?;
            }
            Err(e) => {
                write_error(&mut $writer, $id, e.code, &e.message).await?;
            }
        }
    }};
}

/// STDIO server that reads from stdin and writes to stdout.
///
/// Wraps a `RequestHandler` and dispatches incoming JSON-RPC requests
/// to the appropriate handler method based on the method name.
pub struct StdioServer<H: RequestHandler> {
    handler: Arc<H>,
}

impl<H: RequestHandler> StdioServer<H> {
    pub fn new(handler: Arc<H>) -> Self {
        StdioServer { handler }
    }

    /// Run the server loop on the given stdin/stdout streams.
    ///
    /// 1. Sends the handshake to the client.
    /// 2. Reads the handshake ack.
    /// 3. Enters the main request loop: read request → dispatch → write response.
    /// 4. Exits when stdin is closed (EOF).
    pub async fn run<R, W>(&self, reader: R, writer: W) -> Result<(), StdioError>
    where
        R: tokio::io::AsyncRead + Unpin + Send,
        W: AsyncWrite + Unpin + Send,
    {
        let mut reader = BufReader::new(reader);
        let mut writer = writer;

        // Step 1: Send handshake.
        let session_id =
            std::env::var("A2A_SESSION_ID").unwrap_or_else(|_| uuid::Uuid::now_v7().to_string());

        let hs = Handshake {
            msg_type: "handshake".to_string(),
            session_id,
            supported_variants: vec!["a2a/v1".to_string()],
            pid: Some(std::process::id()),
            features: Some(HandshakeFeatures::default()),
        };
        handshake::write_handshake(&mut writer, &hs).await?;

        // Step 2: Read handshake ack.
        let ack = handshake::read_handshake_ack(&mut reader).await?;
        if !ack.accept {
            return Err(StdioError::HandshakeFailed(
                "client rejected handshake".to_string(),
            ));
        }

        // Step 3: Main request loop.
        let handler = &*self.handler;
        loop {
            let frame = match framing::read_frame(&mut reader).await? {
                Some(f) => f,
                None => break, // EOF — client closed stdin.
            };

            let value: serde_json::Value = match serde_json::from_slice(&frame) {
                Ok(v) => v,
                Err(e) => {
                    write_error(
                        &mut writer,
                        JsonRpcId::Null,
                        error_code::PARSE_ERROR,
                        &format!("invalid json: {e}"),
                    )
                    .await?;
                    continue;
                }
            };

            // Parse as a JSON-RPC request.
            let rpc_request: JsonRpcRequest = match serde_json::from_value(value) {
                Ok(r) => r,
                Err(e) => {
                    write_error(
                        &mut writer,
                        JsonRpcId::Null,
                        error_code::PARSE_ERROR,
                        &format!("invalid request: {e}"),
                    )
                    .await?;
                    continue;
                }
            };

            let id = rpc_request.id.clone();
            let method = rpc_request.method.clone();
            let params_value = rpc_request.params.unwrap_or(serde_json::Value::Null);
            let sp = ServiceParams::new();

            match method.as_str() {
                "message/send" => {
                    dispatch_unary!(
                        writer,
                        id,
                        handler,
                        sp,
                        params_value,
                        SendMessageRequest,
                        send_message
                    );
                }
                "message/stream" => {
                    dispatch_streaming!(
                        writer,
                        id,
                        handler,
                        sp,
                        params_value,
                        SendMessageRequest,
                        send_streaming_message
                    );
                }
                "tasks/get" => {
                    dispatch_unary!(
                        writer,
                        id,
                        handler,
                        sp,
                        params_value,
                        GetTaskRequest,
                        get_task
                    );
                }
                "tasks/list" => {
                    dispatch_unary!(
                        writer,
                        id,
                        handler,
                        sp,
                        params_value,
                        ListTasksRequest,
                        list_tasks
                    );
                }
                "tasks/cancel" => {
                    dispatch_unary!(
                        writer,
                        id,
                        handler,
                        sp,
                        params_value,
                        CancelTaskRequest,
                        cancel_task
                    );
                }
                "tasks/subscribe" => {
                    dispatch_streaming!(
                        writer,
                        id,
                        handler,
                        sp,
                        params_value,
                        SubscribeToTaskRequest,
                        subscribe_to_task
                    );
                }
                "tasks/pushNotificationConfig/create" => {
                    dispatch_unary!(
                        writer,
                        id,
                        handler,
                        sp,
                        params_value,
                        CreateTaskPushNotificationConfigRequest,
                        create_push_config
                    );
                }
                "tasks/pushNotificationConfig/get" => {
                    dispatch_unary!(
                        writer,
                        id,
                        handler,
                        sp,
                        params_value,
                        GetTaskPushNotificationConfigRequest,
                        get_push_config
                    );
                }
                "tasks/pushNotificationConfig/list" => {
                    dispatch_unary!(
                        writer,
                        id,
                        handler,
                        sp,
                        params_value,
                        ListTaskPushNotificationConfigsRequest,
                        list_push_configs
                    );
                }
                "tasks/pushNotificationConfig/delete" => {
                    dispatch_unary!(
                        writer,
                        id,
                        handler,
                        sp,
                        params_value,
                        DeleteTaskPushNotificationConfigRequest,
                        delete_push_config
                    );
                }
                "agent/extendedCard" => {
                    dispatch_unary!(
                        writer,
                        id,
                        handler,
                        sp,
                        params_value,
                        GetExtendedAgentCardRequest,
                        get_extended_agent_card
                    );
                }
                _ => {
                    write_error(
                        &mut writer,
                        id,
                        error_code::METHOD_NOT_FOUND,
                        &format!("unknown method: {method}"),
                    )
                    .await?;
                }
            }
        }

        Ok(())
    }
}

/// Write a JSON-RPC error response.
async fn write_error(
    writer: &mut (impl AsyncWrite + Unpin + Send),
    id: JsonRpcId,
    code: i32,
    message: &str,
) -> Result<(), StdioError> {
    let resp = JsonRpcResponse::error(
        id,
        JsonRpcError {
            code,
            message: message.to_string(),
            data: None,
        },
    );
    let body = serde_json::to_vec(&resp)?;
    framing::write_frame(writer, &body).await
}

/// Convenience function to run a `StdioServer` on the actual process stdin/stdout.
pub async fn serve<H: RequestHandler>(handler: Arc<H>) -> Result<(), StdioError> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let server = StdioServer::new(handler);
    server.run(stdin, stdout).await
}
