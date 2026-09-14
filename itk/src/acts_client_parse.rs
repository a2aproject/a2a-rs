// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! The `tck-client-parse` behaviour: ACTS §10 client tests.
//!
//! Every other ACTS step drives the SUT as a **server** — send bytes, assert
//! on what comes back. A client test inverts that: it supplies a canonical
//! wire payload and asks whether *this SDK's client* parses it correctly,
//! which no A2A operation can ask of a server. §10 defines the file format
//! and says nothing about the mechanism, so without a convention like this
//! one the runner cannot reach the client at all.
//!
//! The runner sends an ordinary `send_message` naming this behaviour with
//! `{operation, wire_payload}` in a data part; the agent drives a real client
//! against a fixture that returns that payload verbatim, and hands back
//! whatever its own client produced.
//!
//! **A fixture server rather than a mock transport.** The other SDKs inject a
//! fake HTTP layer — `httpx.MockTransport`, an `*http.Client`, a fake `fetch`.
//! `reqwest` has no request-level equivalent, and implementing
//! `a2a_client::Transport` would be worse than useless here: its methods
//! already return decoded `Task`/`SendMessageResponse`, so the envelope
//! handling, error mapping and ProtoJSON decode — the things a client test is
//! about — would never run. A loopback listener keeps all of them in play,
//! and is what `a2a-client`'s own tests use.

use std::net::SocketAddr;

use a2a::*;
use a2a_client::agent_card::AgentCardResolver;
use a2a_client::client::A2AClient;
use a2a_client::jsonrpc::JsonRpcTransport;
use axum::Router;
use axum::extract::State;
use axum::http::header;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tracing::info;

pub const BEHAVIOR: &str = "tck-client-parse";

/// What the fixture answers with, for every request it receives.
#[derive(Clone)]
struct Fixture {
    payload: Value,
    /// `application/json` for unary payloads. The JSON-RPC transport treats a
    /// response as SSE unless the content type says otherwise — a *missing*
    /// one is read as a stream — so this must always be set.
    content_type: &'static str,
}

/// Is this payload a JSON-RPC envelope rather than a bare object?
fn is_enveloped(payload: &Value) -> bool {
    ["jsonrpc", "result", "error"]
        .iter()
        .any(|key| payload.get(key).is_some())
}

async fn respond(State(fixture): State<Fixture>, body: String) -> impl IntoResponse {
    let mut payload = fixture.payload.clone();

    // Echo the request's JSON-RPC id, which is what a real server does. The
    // corpus's canned payloads carry a fixed id that cannot match one the
    // client invented at call time, so a client validating the correlation
    // would reject the payload before parsing any of it — leaving the test
    // measuring correlation rather than parsing.
    if is_enveloped(&payload)
        && let Ok(request) = serde_json::from_str::<Value>(&body)
        && let Some(id) = request.get("id")
        && let Some(object) = payload.as_object_mut()
    {
        object.insert("id".to_string(), id.clone());
    }

    (
        [(header::CONTENT_TYPE, fixture.content_type)],
        payload.to_string(),
    )
}

/// Serve `payload` on a loopback port until the returned guard is dropped.
async fn spawn_fixture(payload: Value) -> Result<(String, tokio::task::JoinHandle<()>), String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("fixture listener failed to bind: {e}"))?;
    let addr: SocketAddr = listener
        .local_addr()
        .map_err(|e| format!("fixture listener has no address: {e}"))?;

    let app = Router::new().fallback(respond).with_state(Fixture {
        payload,
        content_type: "application/json",
    });

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((format!("http://{addr}"), handle))
}

fn client_for(endpoint: String) -> Result<A2AClient<JsonRpcTransport>, String> {
    let http = a2a_client::default_reqwest_client(None)
        .map_err(|e| format!("could not build an HTTP client: {e}"))?;
    Ok(A2AClient::new(JsonRpcTransport::new(http, endpoint)))
}

/// Feed `payload` to this SDK's client and return what it produced.
///
/// The result is the §4.2 assertion root for `operation`: a send keeps its
/// `task`/`message` discriminator, `get_task` returns the Task's own fields,
/// and a card operation returns the card.
pub async fn parse(operation: &str, payload: Value) -> Value {
    let had_error = payload.get("error").cloned();
    let (endpoint, server) = match spawn_fixture(payload).await {
        Ok(pair) => pair,
        Err(message) => return json!({ "error": { "message": message } }),
    };

    let parsed = run(operation, &endpoint).await;
    server.abort();

    match parsed {
        Ok(value) => value,
        // The envelope's own error is preferred when the payload carried one:
        // the assertion is about the client having surfaced *that* error, and
        // inventing a code here would pass the test without the client having
        // done anything.
        Err(error) => match had_error {
            Some(wire) => json!({ "error": wire, "raised": error.message }),
            None => json!({ "error": { "code": error.code, "message": error.message } }),
        },
    }
}

async fn run(operation: &str, endpoint: &str) -> Result<Value, A2AError> {
    match operation {
        // Both card operations land here when the payload is a bare card,
        // which is how the corpus writes them — correctly, since a card is
        // fetched over plain HTTP on every binding, so there is no envelope
        // to unwrap. The resolver's well-known path is hard-coded, and the
        // fixture answers every path, so pointing it at the root is enough.
        "get_agent_card" | "get_extended_agent_card" => {
            let http = a2a_client::default_reqwest_client(None)
                .map_err(|e| A2AError::internal(format!("could not build an HTTP client: {e}")))?;
            let card = AgentCardResolver::new(Some(http)).resolve(endpoint).await?;
            rendered(serde_json::to_value(&card))
        }

        "send_message" => {
            let client = client_for(endpoint.to_string()).map_err(A2AError::internal)?;
            let response = client
                .send_message(&SendMessageRequest {
                    message: Message::new(Role::User, vec![Part::text("acts")]),
                    configuration: None,
                    metadata: None,
                    tenant: None,
                })
                .await?;
            // `SendMessageResponse` serializes with its own discriminator,
            // which is exactly what `expect_parsed: {task: ...}` addresses.
            rendered(serde_json::to_value(&response))
        }

        "get_task" => {
            let client = client_for(endpoint.to_string()).map_err(A2AError::internal)?;
            let task = client
                .get_task(&GetTaskRequest {
                    id: "acts".to_string(),
                    history_length: None,
                    tenant: None,
                })
                .await?;
            rendered(serde_json::to_value(&task))
        }

        other => Err(A2AError::unsupported_operation(format!(
            "unsupported client operation {other:?}"
        ))),
    }
}

fn rendered(result: Result<Value, serde_json::Error>) -> Result<Value, A2AError> {
    result.map_err(|e| A2AError::internal(format!("could not render the parse result: {e}")))
}

/// Read `{operation, wire_payload}` out of the step's data part.
pub fn request_from(message: &Message) -> Option<(String, Value)> {
    for part in &message.parts {
        let PartContent::Data(data) = &part.content else {
            continue;
        };
        let Some(operation) = data.get("operation").and_then(Value::as_str) else {
            continue;
        };
        let payload = data.get("wire_payload").cloned().unwrap_or(Value::Null);
        return Some((operation.to_string(), payload));
    }
    None
}

/// Run the behaviour and report what the client parsed, as a data part on an
/// artifact — the shape the runner digs `expect_parsed` out of.
pub async fn artifact_parts(message: Option<&Message>) -> Result<Vec<Part>, String> {
    let request = message
        .and_then(request_from)
        .ok_or_else(|| format!("{BEHAVIOR} needs {{operation, wire_payload}}"))?;
    let (operation, payload) = request;
    info!(operation = %operation, "Running an ACTS client-parse fixture");
    Ok(vec![Part::data(parse(&operation, payload).await)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_message(value: Value) -> Message {
        Message::new(Role::User, vec![Part::text(BEHAVIOR), Part::data(value)])
    }

    #[test]
    fn the_request_is_read_out_of_the_data_part() {
        let message = data_message(json!({
            "operation": "get_task",
            "wire_payload": {"jsonrpc": "2.0", "result": {"id": "t1"}},
        }));
        let (operation, payload) = request_from(&message).expect("a request");
        assert_eq!(operation, "get_task");
        assert_eq!(payload["result"]["id"], json!("t1"));
    }

    #[test]
    fn a_message_without_a_data_part_is_not_a_request() {
        assert!(request_from(&Message::new(Role::User, vec![Part::text(BEHAVIOR)])).is_none());
    }

    #[test]
    fn a_data_part_without_an_operation_is_not_a_request() {
        assert!(request_from(&data_message(json!({"wire_payload": {}}))).is_none());
    }

    #[test]
    fn envelopes_are_told_apart_from_bare_objects() {
        assert!(is_enveloped(&json!({"jsonrpc": "2.0", "result": {}})));
        assert!(is_enveloped(&json!({"error": {"code": -32001}})));
        // A bare agent card, which is how the corpus writes the card payloads.
        assert!(!is_enveloped(
            &json!({"name": "Example", "version": "1.0.0"})
        ));
    }

    #[tokio::test]
    async fn a_success_envelope_parses_into_a_task() {
        let parsed = parse(
            "get_task",
            json!({
                "jsonrpc": "2.0",
                "id": "req-003",
                "result": {"id": "task-xyz", "contextId": "ctx-002",
                           "status": {"state": "TASK_STATE_COMPLETED"}},
            }),
        )
        .await;
        assert_eq!(parsed["id"], json!("task-xyz"));
        assert_eq!(parsed["status"]["state"], json!("TASK_STATE_COMPLETED"));
    }

    #[tokio::test]
    async fn a_send_keeps_its_discriminator() {
        let parsed = parse(
            "send_message",
            json!({
                "jsonrpc": "2.0",
                "id": "1",
                "result": {"task": {"id": "t1", "contextId": "c1",
                                    "status": {"state": "TASK_STATE_SUBMITTED"}}},
            }),
        )
        .await;
        assert_eq!(parsed["task"]["id"], json!("t1"));
    }

    #[tokio::test]
    async fn an_error_envelope_comes_back_as_the_parse_result() {
        // CLIENT-PARSE-004: the error IS what the client was meant to parse,
        // so it is reported rather than raised.
        let parsed = parse(
            "get_task",
            json!({
                "jsonrpc": "2.0",
                "id": "1",
                "error": {"code": -32001, "message": "Task not found"},
            }),
        )
        .await;
        assert_eq!(parsed["error"]["code"], json!(-32001));
        assert_eq!(parsed["error"]["message"], json!("Task not found"));
    }

    #[tokio::test]
    async fn a_complete_bare_card_parses_through_the_resolver() {
        let parsed = parse(
            "get_agent_card",
            json!({
                "name": "Capability Gated Agent",
                "description": "d",
                "version": "1.0.0",
                "capabilities": {"streaming": false, "pushNotifications": false},
                "defaultInputModes": ["text/plain"],
                "defaultOutputModes": ["text/plain"],
                "supportedInterfaces": [{"url": "https://example.com/",
                                         "protocolBinding": "REST",
                                         "protocolVersion": "1.0"}],
            }),
        )
        .await;
        assert_eq!(
            parsed["name"],
            json!("Capability Gated Agent"),
            "got {parsed}"
        );
        // CLIENT-CAP-001 asserts on a capability the client parsed as false,
        // so it has to survive rendering rather than being skipped.
        assert_eq!(parsed["capabilities"]["streaming"], json!(false));
    }

    #[tokio::test]
    async fn a_card_omitting_description_or_the_default_modes_is_rejected() {
        // Not a harness limitation — an SDK finding, and the reason
        // CLIENT-CAP-001 and CLIENT-AUTH-001 fail here while passing on the
        // other SDKs. `AgentCard::description`, `default_input_modes` and
        // `default_output_modes` carry no `#[serde(default)]`, so this SDK
        // requires fields the corpus's canonical cards omit. Pinned rather
        // than worked around: papering over it in the fixture would report a
        // conformance the client has not got.
        let parsed = parse(
            "get_agent_card",
            json!({
                "name": "Capability Gated Agent",
                "version": "1.0.0",
                "capabilities": {"streaming": false},
                "supportedInterfaces": [{"url": "https://example.com/",
                                         "protocolBinding": "REST",
                                         "protocolVersion": "1.0"}],
            }),
        )
        .await;
        assert!(
            parsed["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("failed to parse agent card"),
            "got {parsed}"
        );
    }

    #[tokio::test]
    async fn an_unsupported_operation_says_so() {
        let parsed = parse("teleport", json!({})).await;
        assert!(
            parsed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("teleport")
        );
    }
}
