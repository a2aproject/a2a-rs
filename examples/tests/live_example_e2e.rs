// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Exercise the runnable live server, the shared client conversation, and the
//! live-client binary. All servers use ephemeral ports and die with the test.

use std::{process::Stdio, sync::Arc, time::Duration};

use a2a::*;
use a2a_client::{A2AClientFactory, agent_card::AgentCardResolver};
use a2a_grpc::GrpcTransportFactory;
use examples_lib::live::converse;
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    time::timeout,
};

async fn server() -> (Child, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_live-server"))
        .args(["127.0.0.1:0", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn the live example server");
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let line = timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("server announces its HTTP address");
    let url = line
        .strip_prefix("HTTP=")
        .expect("HTTP address")
        .to_string();
    // Both listeners are already bound before the server announces them.
    timeout(Duration::from_secs(10), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .expect("server announces its gRPC address");
    (child, url)
}

async fn verify(protocol: &str) {
    let (_server, url) = server().await;
    let card = AgentCardResolver::new(None).resolve(&url).await.unwrap();
    let client = A2AClientFactory::builder()
        .register(Arc::new(GrpcTransportFactory::new()))
        .preferred_bindings(vec![protocol.into()])
        .build()
        .create_from_card(&card)
        .await
        .unwrap();
    let events = timeout(Duration::from_secs(15), converse(&client))
        .await
        .unwrap()
        .unwrap();
    let StreamResponse::Task(opening) = &events[0] else {
        panic!("opening task")
    };
    assert_eq!(opening.status.state, TaskState::Submitted);

    let mut states = Vec::new();
    let mut replies = Vec::new();
    let mut inputs = Vec::new();
    let mut generation = opening.generation;
    for event in &events[1..] {
        let next_generation = match event {
            StreamResponse::StatusUpdate(update) => {
                assert_eq!(update.task_id, opening.id);
                states.push(update.status.state.clone());
                replies.push(update.status.message.as_ref().unwrap().text().unwrap());
                update.generation
            }
            StreamResponse::MessageUpdate(update) => {
                assert_eq!(update.task_id, opening.id);
                inputs.push(update.message.text().unwrap());
                update.generation
            }
            other => panic!("unexpected event: {other:?}"),
        };
        assert!(
            next_generation > generation,
            "events preserve persisted order"
        );
        generation = next_generation;
    }
    assert_eq!(
        states,
        [
            TaskState::Working,
            TaskState::Working,
            TaskState::InputRequired,
            TaskState::Working,
            TaskState::Completed
        ]
    );
    assert_eq!(
        replies,
        [
            "Echo: start",
            "Echo: hello",
            "Send another message to resume",
            "Echo: resumed",
            "Conversation complete"
        ]
    );
    assert_eq!(inputs, ["hello", "pause", "resumed", "finish"]);

    let stored = client
        .get_task(&GetTaskRequest {
            id: opening.id.clone(),
            history_length: Some(100),
            tenant: None,
            current_generation: None,
            timeline_length: None,
        })
        .await
        .unwrap();
    assert_eq!(stored.status.state, TaskState::Completed);
    assert_eq!(stored.generation, generation);
    assert_eq!(
        stored
            .history
            .unwrap()
            .iter()
            .filter(|m| m.role == Role::User)
            .count(),
        5
    );
}

#[tokio::test]
async fn live_example_rest() {
    verify(TRANSPORT_PROTOCOL_HTTP_JSON).await;
}

#[tokio::test]
async fn live_example_jsonrpc() {
    verify(TRANSPORT_PROTOCOL_JSONRPC).await;
}

#[tokio::test]
async fn live_example_grpc() {
    verify(TRANSPORT_PROTOCOL_GRPC).await;
}

#[tokio::test]
async fn live_client_binary_completes_all_three_bindings() {
    let (_server, url) = server().await;
    let mut command = Command::new(env!("CARGO_BIN_EXE_live-client"));
    command.arg(url).kill_on_drop(true);
    let output = timeout(Duration::from_secs(45), command.output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    for protocol in [
        TRANSPORT_PROTOCOL_HTTP_JSON,
        TRANSPORT_PROTOCOL_JSONRPC,
        TRANSPORT_PROTOCOL_GRPC,
    ] {
        assert!(stdout.lines().any(|line| line == protocol), "{stdout}");
    }
    assert_eq!(
        stdout.matches("Completed: Conversation complete").count(),
        3,
        "{stdout}"
    );
    assert_eq!(stdout.matches("InputRequired:").count(), 3, "{stdout}");
    assert_eq!(
        stdout.matches("Working: Echo: resumed").count(),
        3,
        "{stdout}"
    );
}
