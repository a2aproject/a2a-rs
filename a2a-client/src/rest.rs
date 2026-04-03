// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;

use crate::transport::{ServiceParams, Transport, TransportFactory};

/// REST (HTTP+JSON) transport implementation.
///
/// Maps A2A operations to RESTful HTTP endpoints.
pub struct RestTransport {
    client: Client,
    base_url: String,
}

impl RestTransport {
    pub fn new(client: Client, base_url: String) -> Self {
        let base_url = base_url.trim_end_matches('/').to_string();
        RestTransport { client, base_url }
    }

    fn build_request(
        &self,
        method: reqwest::Method,
        path: &str,
        params: &ServiceParams,
    ) -> reqwest::RequestBuilder {
        let url = format!("{}{}", self.base_url, path);
        let mut builder = self.client.request(method, &url);
        for (key, values) in params {
            for v in values {
                builder = builder.header(key, v);
            }
        }
        builder
    }

    async fn post_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        params: &ServiceParams,
        body: &impl serde::Serialize,
    ) -> Result<T, A2AError> {
        let resp = self
            .build_request(reqwest::Method::POST, path, params)
            .json(body)
            .send()
            .await
            .map_err(|e| A2AError::internal(format!("HTTP request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(A2AError::internal(format!("HTTP {status}: {body}")));
        }

        resp.json()
            .await
            .map_err(|e| A2AError::internal(format!("failed to parse response: {e}")))
    }

    async fn get_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        params: &ServiceParams,
    ) -> Result<T, A2AError> {
        let resp = self
            .build_request(reqwest::Method::GET, path, params)
            .send()
            .await
            .map_err(|e| A2AError::internal(format!("HTTP request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(A2AError::internal(format!("HTTP {status}: {body}")));
        }

        resp.json()
            .await
            .map_err(|e| A2AError::internal(format!("failed to parse response: {e}")))
    }

    async fn post_streaming(
        &self,
        path: &str,
        params: &ServiceParams,
        body: &impl serde::Serialize,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let resp = self
            .build_request(reqwest::Method::POST, path, params)
            .header("Accept", "text/event-stream")
            .json(body)
            .send()
            .await
            .map_err(|e| A2AError::internal(format!("HTTP request failed: {e}")))?;

        let stream = resp.bytes_stream();
        Ok(crate::jsonrpc::parse_sse_stream_rest(stream))
    }

    async fn delete(&self, path: &str, params: &ServiceParams) -> Result<(), A2AError> {
        let resp = self
            .build_request(reqwest::Method::DELETE, path, params)
            .send()
            .await
            .map_err(|e| A2AError::internal(format!("HTTP request failed: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(A2AError::internal(format!("HTTP {status}: {body}")));
        }
        Ok(())
    }
}

#[async_trait]
impl Transport for RestTransport {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        self.post_json("/message/send", params, req).await
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.post_streaming("/message/stream", params, req).await
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: &GetTaskRequest,
    ) -> Result<Task, A2AError> {
        let mut path = format!("/tasks/{}", req.id);
        let mut query_parts = Vec::new();
        if let Some(hl) = req.history_length {
            query_parts.push(format!("historyLength={hl}"));
        }
        if !query_parts.is_empty() {
            path.push('?');
            path.push_str(&query_parts.join("&"));
        }
        self.get_json(&path, params).await
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        let mut query_parts = Vec::new();
        if let Some(ref cid) = req.context_id {
            query_parts.push(format!("contextId={cid}"));
        }
        if let Some(ref status) = req.status {
            let s = serde_json::to_value(status)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();
            query_parts.push(format!("status={s}"));
        }
        if let Some(ps) = req.page_size {
            query_parts.push(format!("pageSize={ps}"));
        }
        if let Some(ref pt) = req.page_token {
            query_parts.push(format!("pageToken={pt}"));
        }
        if let Some(hl) = req.history_length {
            query_parts.push(format!("historyLength={hl}"));
        }
        if let Some(ia) = req.include_artifacts {
            query_parts.push(format!("includeArtifacts={ia}"));
        }
        let mut path = "/tasks".to_string();
        if !query_parts.is_empty() {
            path.push('?');
            path.push_str(&query_parts.join("&"));
        }
        self.get_json(&path, params).await
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: &CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        self.post_json(&format!("/tasks/{}/cancel", req.id), params, req)
            .await
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: &SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.post_streaming(&format!("/tasks/{}/subscribe", req.id), params, req)
            .await
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: &CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.post_json(
            &format!("/tasks/{}/push-configs", req.task_id),
            params,
            &req.config,
        )
        .await
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        self.get_json(
            &format!("/tasks/{}/push-configs/{}", req.task_id, req.id),
            params,
        )
        .await
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        self.get_json(&format!("/tasks/{}/push-configs", req.task_id), params)
            .await
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        self.delete(
            &format!("/tasks/{}/push-configs/{}", req.task_id, req.id),
            params,
        )
        .await
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        _req: &GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        self.get_json("/agent-card/extended", params).await
    }

    async fn destroy(&self) -> Result<(), A2AError> {
        Ok(())
    }
}

/// Factory for creating [`RestTransport`] instances.
pub struct RestTransportFactory {
    client: Client,
}

impl RestTransportFactory {
    pub fn new(client: Option<Client>) -> Self {
        RestTransportFactory {
            client: client.unwrap_or_default(),
        }
    }
}

#[async_trait]
impl TransportFactory for RestTransportFactory {
    fn protocol(&self) -> &str {
        TRANSPORT_PROTOCOL_HTTP_JSON
    }

    async fn create(
        &self,
        _card: &AgentCard,
        iface: &AgentInterface,
    ) -> Result<Box<dyn Transport>, A2AError> {
        Ok(Box::new(RestTransport::new(
            self.client.clone(),
            iface.url.clone(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rest_transport_new_strips_trailing_slash() {
        let t = RestTransport::new(Client::new(), "http://localhost:8080/".into());
        assert_eq!(t.base_url, "http://localhost:8080");
    }

    #[test]
    fn test_rest_transport_new_no_trailing_slash() {
        let t = RestTransport::new(Client::new(), "http://localhost:8080".into());
        assert_eq!(t.base_url, "http://localhost:8080");
    }

    #[test]
    fn test_rest_transport_factory_protocol() {
        let f = RestTransportFactory::new(None);
        assert_eq!(f.protocol(), "HTTP+JSON");
    }

    #[tokio::test]
    async fn test_rest_transport_factory_create() {
        let f = RestTransportFactory::new(None);
        let card = AgentCard {
            name: "Test".into(),
            description: "Test".into(),
            version: "1.0".into(),
            supported_interfaces: vec![],
            capabilities: AgentCapabilities::default(),
            default_input_modes: vec!["text/plain".into()],
            default_output_modes: vec!["text/plain".into()],
            skills: vec![],
            provider: None,
            documentation_url: None,
            icon_url: None,
            security_schemes: None,
            security_requirements: None,
            signatures: None,
        };
        let iface = AgentInterface::new("http://localhost:8080/", "HTTP+JSON");
        let transport = f.create(&card, &iface).await.unwrap();
        transport.destroy().await.unwrap();
    }

    #[test]
    fn test_build_request_adds_params() {
        let t = RestTransport::new(Client::new(), "http://localhost:8080".into());
        let mut params = ServiceParams::new();
        params.insert("X-Custom".into(), vec!["val1".into(), "val2".into()]);
        let builder = t.build_request(reqwest::Method::GET, "/test", &params);
        let req = builder.build().unwrap();
        let vals: Vec<_> = req
            .headers()
            .get_all("X-Custom")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(vals, vec!["val1", "val2"]);
    }
}
