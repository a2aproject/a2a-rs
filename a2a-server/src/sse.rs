// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::StreamExt;
use futures::stream::BoxStream;
use std::convert::Infallible;
use std::time::Duration;

/// How often an idle SSE response emits a comment frame.
///
/// Proxies and load balancers commonly drop idle connections after 30-60s,
/// which is well inside the lifetime of the long-running tasks `subscribe`
/// exists for, so the interval has to sit comfortably below that.
const SSE_KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

/// Event ids are assigned per response, counting from 1.
///
/// A monotonic counter rather than a2a-go's per-event UUID: it lets a
/// client notice that event 4 never arrived between 3 and 5, which an
/// opaque identifier cannot. Both satisfy SSE's `Last-Event-ID`; only this
/// one makes a gap detectable.
fn next_event_id(counter: &mut u64) -> String {
    *counter += 1;
    counter.to_string()
}

/// Convert a boxed stream of serializable items into an SSE response.
pub fn sse_from_stream<T: serde::Serialize + Send + 'static>(
    stream: BoxStream<'static, Result<T, a2a::A2AError>>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let mut event_id = 0u64;
    let sse_stream = stream.map(move |item| {
        let data = match item {
            Ok(val) => serde_json::to_string(&val).unwrap_or_default(),
            Err(err) => serde_json::to_string(&err.to_jsonrpc_error()).unwrap_or_default(),
        };
        Ok::<_, Infallible>(Event::default().id(next_event_id(&mut event_id)).data(data))
    });
    Sse::new(sse_stream).keep_alive(KeepAlive::new().interval(SSE_KEEP_ALIVE_INTERVAL))
}

/// Convert a boxed stream of JSON-RPC responses into an SSE response.
pub fn sse_jsonrpc_stream<T: serde::Serialize + Send + 'static>(
    request_id: a2a::JsonRpcId,
    stream: BoxStream<'static, Result<T, a2a::A2AError>>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let mut event_id = 0u64;
    let sse_stream = stream.map(move |item| {
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
        Ok::<_, Infallible>(Event::default().id(next_event_id(&mut event_id)).data(data))
    });
    Sse::new(sse_stream).keep_alive(KeepAlive::new().interval(SSE_KEEP_ALIVE_INTERVAL))
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
    }

    /// Parse the `id:` values out of an SSE body, in order.
    fn event_ids(body: &str) -> Vec<String> {
        body.lines()
            .filter_map(|line| line.strip_prefix("id:"))
            .map(|id| id.trim().to_string())
            .collect()
    }

    /// Every event carries an id, counting from 1, so a client can tell a
    /// gap from a clean sequence and has something to put in `Last-Event-ID`.
    #[tokio::test]
    async fn test_sse_from_stream_assigns_monotonic_event_ids() {
        let s: BoxStream<'static, Result<serde_json::Value, A2AError>> =
            Box::pin(stream::iter(vec![
                Ok(serde_json::json!({"n": 1})),
                Ok(serde_json::json!({"n": 2})),
                Err(A2AError::internal("boom")),
            ]));

        let body = sse_from_stream(s)
            .into_response()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();

        // The error event is numbered too: a client counting events must not
        // see a gap just because one of them reported a failure.
        assert_eq!(event_ids(&body), vec!["1", "2", "3"], "{body}");
    }

    #[tokio::test]
    async fn test_sse_jsonrpc_stream_assigns_monotonic_event_ids() {
        let s: BoxStream<'static, Result<serde_json::Value, A2AError>> =
            Box::pin(stream::iter(vec![
                Ok(serde_json::json!({"n": 1})),
                Ok(serde_json::json!({"n": 2})),
                Ok(serde_json::json!({"n": 3})),
            ]));

        let body = sse_jsonrpc_stream(JsonRpcId::Number(1), s)
            .into_response()
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let body = String::from_utf8(body.to_vec()).unwrap();

        assert_eq!(event_ids(&body), vec!["1", "2", "3"], "{body}");
    }

    /// Ids restart per response rather than continuing across streams, so
    /// two concurrent subscriptions do not share a numbering space.
    #[tokio::test]
    async fn test_event_ids_are_scoped_to_one_response() {
        let make = || -> BoxStream<'static, Result<serde_json::Value, A2AError>> {
            Box::pin(stream::once(async { Ok(serde_json::json!({"n": 1})) }))
        };

        for _ in 0..2 {
            let body = sse_from_stream(make())
                .into_response()
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes();
            let body = String::from_utf8(body.to_vec()).unwrap();
            assert_eq!(event_ids(&body), vec!["1"], "{body}");
        }
    }
}
