// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::StreamExt;
use futures::stream::BoxStream;
use std::convert::Infallible;
use std::time::Duration;

/// Convert a boxed stream of serializable items into an SSE response.
///
/// Each event carries a monotonic, counter-based `id` so clients can resume
/// interrupted subscriptions, and the response sends a keep-alive comment
/// frame every 15 seconds so intermediaries do not terminate idle
/// connections (BUG-57).
pub fn sse_from_stream<T: serde::Serialize + Send + 'static>(
    stream: BoxStream<'static, Result<T, a2a::A2AError>>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let mut counter = 0u64;
    let sse_stream = stream.map(move |item| {
        counter += 1;
        let event = match item {
            Ok(val) => {
                let data = serde_json::to_string(&val).unwrap_or_default();
                Event::default().id(counter.to_string()).data(data)
            }
            Err(err) => {
                let error_json = serde_json::to_string(&err.to_jsonrpc_error()).unwrap_or_default();
                Event::default().id(counter.to_string()).data(error_json)
            }
        };
        Ok::<_, Infallible>(event)
    });
    Sse::new(sse_stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

/// Convert a boxed stream of JSON-RPC responses into an SSE response.
///
/// Each event carries a monotonic, counter-based `id` and the response sends
/// a keep-alive comment frame every 15 seconds (BUG-57).
pub fn sse_jsonrpc_stream<T: serde::Serialize + Send + 'static>(
    request_id: a2a::JsonRpcId,
    stream: BoxStream<'static, Result<T, a2a::A2AError>>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let mut counter = 0u64;
    let sse_stream = stream.map(move |item| {
        counter += 1;
        let data = match item {
            Ok(val) => {
                let result_val = serde_json::to_value(&val).unwrap_or_default();
                let resp = a2a::JsonRpcResponse::success(request_id.clone(), result_val);
                serde_json::to_string(&resp).unwrap_or_default()
            }
            Err(err) => {
                let resp = a2a::JsonRpcResponse::error(request_id.clone(), err.to_jsonrpc_error());
                serde_json::to_string(&resp).unwrap_or_default()
            }
        };
        Ok::<_, Infallible>(Event::default().id(counter.to_string()).data(data))
    });
    Sse::new(sse_stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use a2a::*;
    use axum::response::IntoResponse;
    use futures::stream;
    use http_body_util::BodyExt;

    #[tokio::test]
    async fn test_sse_from_stream_ok() {
        let s: BoxStream<'static, Result<serde_json::Value, A2AError>> =
            Box::pin(stream::once(async {
                Ok(serde_json::json!({"key": "value"}))
            }));
        let sse = sse_from_stream(s);
        let resp = sse.into_response();
        assert_eq!(resp.status(), 200);
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("data:"));
        assert!(body_str.contains("value"));
        assert!(body_str.contains("id: 1"));
    }

    #[tokio::test]
    async fn test_sse_from_stream_err() {
        let s: BoxStream<'static, Result<serde_json::Value, A2AError>> =
            Box::pin(stream::once(async { Err(A2AError::internal("fail")) }));
        let sse = sse_from_stream(s);
        let resp = sse.into_response();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("data:"));
        assert!(body_str.contains("fail"));
        assert!(body_str.contains("id: 1"));
    }

    #[tokio::test]
    async fn test_sse_jsonrpc_stream_ok() {
        let s: BoxStream<'static, Result<serde_json::Value, A2AError>> =
            Box::pin(stream::once(async {
                Ok(serde_json::json!({"status": "ok"}))
            }));
        let sse = sse_jsonrpc_stream(JsonRpcId::Number(1), s);
        let resp = sse.into_response();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("data:"));
        assert!(body_str.contains("jsonrpc"));
        assert!(body_str.contains("2.0"));
        assert!(body_str.contains("id: 1"));
    }

    #[tokio::test]
    async fn test_sse_jsonrpc_stream_err() {
        let s: BoxStream<'static, Result<serde_json::Value, A2AError>> =
            Box::pin(stream::once(async { Err(A2AError::internal("fail")) }));
        let sse = sse_jsonrpc_stream(JsonRpcId::Number(1), s);
        let resp = sse.into_response();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        assert!(body_str.contains("fail"));
        assert!(body_str.contains("error"));
        assert!(body_str.contains("id: 1"));
    }

    #[tokio::test]
    async fn test_sse_jsonrpc_stream_multiple_events() {
        let s: BoxStream<'static, Result<serde_json::Value, A2AError>> =
            Box::pin(stream::iter(vec![
                Ok(serde_json::json!({"n": 1})),
                Ok(serde_json::json!({"n": 2})),
                Err(A2AError::internal("done")),
            ]));
        let sse = sse_jsonrpc_stream(JsonRpcId::String("req-1".into()), s);
        let resp = sse.into_response();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let body_str = String::from_utf8(body.to_vec()).unwrap();
        // Should contain multiple data lines
        let data_count = body_str.matches("data:").count();
        assert!(
            data_count >= 3,
            "expected >= 3 data lines, got {data_count}"
        );
        // Event IDs must be present and monotonic.
        assert!(body_str.contains("id: 1"));
        assert!(body_str.contains("id: 2"));
        assert!(body_str.contains("id: 3"));
    }
}
