// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use a2a::*;
use a2a_pb::protojson_conv::{self, ProtoJsonPayload};
use async_trait::async_trait;
use futures::stream::BoxStream;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::push_config_compat::{
    deserialize_list_task_push_notification_configs_response,
    deserialize_task_push_notification_config,
};
use crate::transport::{ServiceParams, Transport, TransportFactory};

const REST_SEND_MESSAGE_PATH: &str = "/message:send";
const REST_STREAM_MESSAGE_PATH: &str = "/message:stream";
const REST_EXTENDED_AGENT_CARD_PATH: &str = "/extendedAgentCard";

#[derive(Debug, Deserialize)]
struct RestErrorEnvelope {
    error: RestErrorStatus,
}

#[derive(Debug, Deserialize)]
struct RestErrorStatus {
    message: String,

    #[serde(default)]
    details: Vec<TypedDetail>,
}

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

    fn build_request_with_query(
        &self,
        method: reqwest::Method,
        path: &str,
        params: &ServiceParams,
        query: &[(String, String)],
    ) -> reqwest::RequestBuilder {
        let builder = self.build_request(method, path, params);
        if query.is_empty() {
            builder
        } else {
            builder.query(query)
        }
    }

    async fn send(&self, builder: reqwest::RequestBuilder) -> Result<reqwest::Response, A2AError> {
        builder
            .send()
            .await
            .map_err(|e| A2AError::internal(format!("HTTP request failed: {e}")))
    }

    async fn into_rest_error(resp: reqwest::Response) -> A2AError {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        parse_rest_error(status, &body)
    }

    async fn post_value<Req>(
        &self,
        path: &str,
        params: &ServiceParams,
        body: &Req,
    ) -> Result<Value, A2AError>
    where
        Req: ProtoJsonPayload,
    {
        let payload = protojson_conv::to_value(body).map_err(|e| {
            A2AError::internal(format!("failed to serialize request as ProtoJSON: {e}"))
        })?;
        let resp = self
            .send(
                self.build_request(reqwest::Method::POST, path, params)
                    .json(&payload),
            )
            .await?;

        if !resp.status().is_success() {
            return Err(Self::into_rest_error(resp).await);
        }
        let payload = resp
            .json::<Value>()
            .await
            .map_err(|e| A2AError::internal(format!("failed to parse response: {e}")))?;

        Ok(payload)
    }

    async fn post_json<Req, Resp>(
        &self,
        path: &str,
        params: &ServiceParams,
        body: &Req,
    ) -> Result<Resp, A2AError>
    where
        Req: ProtoJsonPayload,
        Resp: ProtoJsonPayload,
    {
        let payload = self.post_value(path, params, body).await?;

        protojson_conv::from_value(payload).map_err(|e| {
            A2AError::internal(format!("failed to deserialize response as ProtoJSON: {e}"))
        })
    }

    async fn get_value(
        &self,
        path: &str,
        params: &ServiceParams,
        query: &[(String, String)],
    ) -> Result<Value, A2AError> {
        let resp = self
            .send(self.build_request_with_query(reqwest::Method::GET, path, params, query))
            .await?;

        if !resp.status().is_success() {
            return Err(Self::into_rest_error(resp).await);
        }
        let payload = resp
            .json::<Value>()
            .await
            .map_err(|e| A2AError::internal(format!("failed to parse response: {e}")))?;

        Ok(payload)
    }

    async fn get_json<Resp>(
        &self,
        path: &str,
        params: &ServiceParams,
        query: &[(String, String)],
    ) -> Result<Resp, A2AError>
    where
        Resp: ProtoJsonPayload,
    {
        let payload = self.get_value(path, params, query).await?;

        protojson_conv::from_value(payload).map_err(|e| {
            A2AError::internal(format!("failed to deserialize response as ProtoJSON: {e}"))
        })
    }

    async fn post_streaming<Req>(
        &self,
        path: &str,
        params: &ServiceParams,
        body: &Req,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError>
    where
        Req: ProtoJsonPayload,
    {
        let payload = protojson_conv::to_value(body).map_err(|e| {
            A2AError::internal(format!("failed to serialize request as ProtoJSON: {e}"))
        })?;
        let resp = self
            .send(
                self.build_request(reqwest::Method::POST, path, params)
                    .header("Accept", "text/event-stream")
                    .json(&payload),
            )
            .await?;

        if !resp.status().is_success() {
            return Err(Self::into_rest_error(resp).await);
        }

        let stream = resp.bytes_stream();
        Ok(crate::jsonrpc::parse_sse_stream_rest(stream))
    }

    async fn get_streaming(
        &self,
        path: &str,
        params: &ServiceParams,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        let resp = self
            .send(
                self.build_request(reqwest::Method::GET, path, params)
                    .header("Accept", "text/event-stream"),
            )
            .await?;

        if !resp.status().is_success() {
            return Err(Self::into_rest_error(resp).await);
        }

        let stream = resp.bytes_stream();
        Ok(crate::jsonrpc::parse_sse_stream_rest(stream))
    }

    async fn delete(&self, path: &str, params: &ServiceParams) -> Result<(), A2AError> {
        let resp = self
            .send(self.build_request(reqwest::Method::DELETE, path, params))
            .await?;

        if !resp.status().is_success() {
            return Err(Self::into_rest_error(resp).await);
        }
        Ok(())
    }
}

fn parse_rest_error(status: reqwest::StatusCode, body: &str) -> A2AError {
    let Ok(envelope) = serde_json::from_str::<RestErrorEnvelope>(body) else {
        return A2AError::internal(format!("HTTP {status}: {body}"));
    };

    crate::a2a_error_from_details(
        error_code::INTERNAL_ERROR,
        envelope.error.message,
        envelope.error.details,
    )
}

#[async_trait]
impl Transport for RestTransport {
    async fn send_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<SendMessageResponse, A2AError> {
        self.post_json(REST_SEND_MESSAGE_PATH, params, req).await
    }

    async fn send_streaming_message(
        &self,
        params: &ServiceParams,
        req: &SendMessageRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.post_streaming(REST_STREAM_MESSAGE_PATH, params, req)
            .await
    }

    async fn get_task(
        &self,
        params: &ServiceParams,
        req: &GetTaskRequest,
    ) -> Result<Task, A2AError> {
        let path = format!("/tasks/{}", req.id);
        let mut query_parts = Vec::new();
        if let Some(hl) = req.history_length {
            query_parts.push(("historyLength".to_string(), hl.to_string()));
        }
        self.get_json(&path, params, &query_parts).await
    }

    async fn list_tasks(
        &self,
        params: &ServiceParams,
        req: &ListTasksRequest,
    ) -> Result<ListTasksResponse, A2AError> {
        let mut query_parts = Vec::new();
        if let Some(ref cid) = req.context_id {
            query_parts.push(("contextId".to_string(), cid.clone()));
        }
        if let Some(ref status) = req.status {
            let s = serde_json::to_value(status)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .unwrap_or_default();
            query_parts.push(("status".to_string(), s));
        }
        if let Some(ps) = req.page_size {
            query_parts.push(("pageSize".to_string(), ps.to_string()));
        }
        if let Some(ref pt) = req.page_token {
            query_parts.push(("pageToken".to_string(), pt.clone()));
        }
        if let Some(hl) = req.history_length {
            query_parts.push(("historyLength".to_string(), hl.to_string()));
        }
        if let Some(ref ts) = req.status_timestamp_after {
            query_parts.push(("statusTimestampAfter".to_string(), ts.to_rfc3339()));
        }
        if let Some(ia) = req.include_artifacts {
            query_parts.push(("includeArtifacts".to_string(), ia.to_string()));
        }
        self.get_json("/tasks", params, &query_parts).await
    }

    async fn cancel_task(
        &self,
        params: &ServiceParams,
        req: &CancelTaskRequest,
    ) -> Result<Task, A2AError> {
        self.post_json(&format!("/tasks/{}:cancel", req.id), params, req)
            .await
    }

    async fn subscribe_to_task(
        &self,
        params: &ServiceParams,
        req: &SubscribeToTaskRequest,
    ) -> Result<BoxStream<'static, Result<StreamResponse, A2AError>>, A2AError> {
        self.get_streaming(&format!("/tasks/{}:subscribe", req.id), params)
            .await
    }

    async fn create_push_config(
        &self,
        params: &ServiceParams,
        req: &CreateTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let payload = self
            .post_value(
                &format!("/tasks/{}/pushNotificationConfigs", req.task_id),
                params,
                &req.config,
            )
            .await?;
        deserialize_task_push_notification_config(payload)
    }

    async fn get_push_config(
        &self,
        params: &ServiceParams,
        req: &GetTaskPushNotificationConfigRequest,
    ) -> Result<TaskPushNotificationConfig, A2AError> {
        let payload = self
            .get_value(
                &format!("/tasks/{}/pushNotificationConfigs/{}", req.task_id, req.id),
                params,
                &[],
            )
            .await?;
        deserialize_task_push_notification_config(payload)
    }

    async fn list_push_configs(
        &self,
        params: &ServiceParams,
        req: &ListTaskPushNotificationConfigsRequest,
    ) -> Result<ListTaskPushNotificationConfigsResponse, A2AError> {
        let mut query_parts = Vec::new();
        if let Some(page_size) = req.page_size {
            query_parts.push(("pageSize".to_string(), page_size.to_string()));
        }
        if let Some(ref page_token) = req.page_token {
            query_parts.push(("pageToken".to_string(), page_token.clone()));
        }

        let payload = self
            .get_value(
                &format!("/tasks/{}/pushNotificationConfigs", req.task_id),
                params,
                &query_parts,
            )
            .await?;
        deserialize_list_task_push_notification_configs_response(payload)
    }

    async fn delete_push_config(
        &self,
        params: &ServiceParams,
        req: &DeleteTaskPushNotificationConfigRequest,
    ) -> Result<(), A2AError> {
        self.delete(
            &format!("/tasks/{}/pushNotificationConfigs/{}", req.task_id, req.id),
            params,
        )
        .await
    }

    async fn get_extended_agent_card(
        &self,
        params: &ServiceParams,
        _req: &GetExtendedAgentCardRequest,
    ) -> Result<AgentCard, A2AError> {
        self.get_json(REST_EXTENDED_AGENT_CARD_PATH, params, &[])
            .await
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

    #[cfg(any(feature = "rustls-tls", feature = "native-tls"))]
    pub fn with_root_certificates_pem(pem: &[u8]) -> Result<Self, A2AError> {
        Ok(Self {
            client: crate::build_reqwest_client_with_root_pem(pem)?,
        })
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
    use crate::test_utils::rcgen_self_signed_ca_pem;
    use serde_json::json;

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

    #[test]
    fn test_parse_rest_error_preserves_a2a_error_code() {
        let body = json!({
            "error": {
                "code": 404,
                "status": "NOT_FOUND",
                "message": "task not found: t1",
                "details": [
                    {
                        "@type": errordetails::ERROR_INFO_TYPE,
                        "reason": "TASK_NOT_FOUND",
                        "domain": errordetails::PROTOCOL_DOMAIN,
                        "metadata": {
                            "taskId": "t1"
                        }
                    },
                    {
                        "resource": "task"
                    }
                ]
            }
        })
        .to_string();

        let err = parse_rest_error(reqwest::StatusCode::NOT_FOUND, &body);

        assert_eq!(err.code, error_code::TASK_NOT_FOUND);
        assert_eq!(err.message, "task not found: t1");
        let details = err.details.expect("expected structured details");
        assert_eq!(details.len(), 2);
        assert_eq!(details[0].type_url, errordetails::ERROR_INFO_TYPE);
        assert_eq!(
            details[1].value.get("resource"),
            Some(&Value::String("task".into()))
        );
    }

    #[test]
    fn test_parse_rest_error_accepts_go_reason_aliases() {
        let body = json!({
            "error": {
                "code": 400,
                "status": "INVALID_ARGUMENT",
                "message": "incompatible content types",
                "details": [
                    {
                        "@type": errordetails::ERROR_INFO_TYPE,
                        "reason": "UNSUPPORTED_CONTENT_TYPE",
                        "domain": errordetails::PROTOCOL_DOMAIN,
                        "metadata": {}
                    }
                ]
            }
        })
        .to_string();

        let err = parse_rest_error(reqwest::StatusCode::BAD_REQUEST, &body);
        assert_eq!(err.code, error_code::CONTENT_TYPE_NOT_SUPPORTED);
    }

    #[test]
    fn test_parse_rest_error_bad_request_fallback() {
        let body = json!({
            "error": {
                "code": 400,
                "status": "INVALID_ARGUMENT",
                "message": "invalid request parameters",
                "details": [
                    {
                        "@type": errordetails::BAD_REQUEST_TYPE,
                        "fieldViolations": [
                            {
                                "field": "message.parts",
                                "description": "At least one part is required"
                            }
                        ]
                    }
                ]
            }
        })
        .to_string();

        let err = parse_rest_error(reqwest::StatusCode::BAD_REQUEST, &body);
        assert_eq!(err.code, error_code::INVALID_PARAMS);
        assert!(
            err.message
                .contains("message.parts: At least one part is required")
        );
        let details = err.details.expect("expected details");
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].type_url, errordetails::BAD_REQUEST_TYPE);
        let violations = details[0].value.get("fieldViolations").unwrap();
        assert_eq!(violations[0]["field"], "message.parts");
    }

    #[test]
    fn test_parse_rest_error_bad_request_with_error_info_uses_reason() {
        let body = json!({
            "error": {
                "code": 400,
                "status": "INVALID_ARGUMENT",
                "message": "bad params",
                "details": [
                    {
                        "@type": errordetails::BAD_REQUEST_TYPE,
                        "fieldViolations": [
                            {"field": "task.id", "description": "required"}
                        ]
                    },
                    {
                        "@type": errordetails::ERROR_INFO_TYPE,
                        "reason": "INVALID_PARAMS",
                        "domain": errordetails::PROTOCOL_DOMAIN,
                        "metadata": {}
                    }
                ]
            }
        })
        .to_string();

        let err = parse_rest_error(reqwest::StatusCode::BAD_REQUEST, &body);
        assert_eq!(err.code, error_code::INVALID_PARAMS);
    }

    #[test]
    fn test_with_root_certificates_pem_valid() {
        let pem = rcgen_self_signed_ca_pem();
        let f = RestTransportFactory::with_root_certificates_pem(&pem).unwrap();
        assert_eq!(f.protocol(), TRANSPORT_PROTOCOL_HTTP_JSON);
    }

    #[tokio::test]
    async fn test_with_root_certificates_pem_factory_create() {
        let pem = rcgen_self_signed_ca_pem();
        let f = RestTransportFactory::with_root_certificates_pem(&pem).unwrap();
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
        let iface = AgentInterface::new("https://localhost:3443/rest", "HTTP+JSON");
        let transport = f.create(&card, &iface).await.unwrap();
        transport.destroy().await.unwrap();
    }
}
