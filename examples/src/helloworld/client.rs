// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

//! Hello World A2A client.
//!
//! Demonstrates the full client lifecycle:
//!   1. Resolve the agent card from each server URL.
//!   2. For each supported protocol (gRPC, JSON-RPC, REST), build a factory
//!      whose preferred_bindings pin it to that protocol, then create one
//!      client from the card.
//!   3. Exercise every server method on each client.
//!
//! Run the server first:
//!   cargo run --bin helloworld-server --package examples
//! Then run this client:
//!   cargo run --bin helloworld-client --package examples

use a2a::event::StreamResponse;
use a2a::*;
use a2a_client::agent_card::AgentCardResolver;
use a2a_client::{A2AClient, A2AClientFactory, Transport};
use a2a_grpc::GrpcTransportFactory;
use futures::StreamExt;
use std::sync::Arc;

/// Base URLs of A2A servers to connect to.
const SERVER_URLS: &[&str] = &["http://localhost:3000"];

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let resolver = AgentCardResolver::new(None);

    // Protocols to try, in preference order.  Each factory is built with a
    // single preferred binding so create_from_card selects exactly that
    // transport when the agent card advertises it.
    let protocols: &[&str] = &[
        TRANSPORT_PROTOCOL_GRPC,
        TRANSPORT_PROTOCOL_JSONRPC,
        TRANSPORT_PROTOCOL_HTTP_JSON,
    ];

    for &url in SERVER_URLS {
        // ------------------------------------------------------------------
        // Step 1: pull the agent card
        // ------------------------------------------------------------------
        tracing::info!(url, "fetching agent card");
        let card = match resolver.resolve(url).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(url, error = %e, "failed to fetch agent card");
                continue;
            }
        };
        tracing::info!(
            name = %card.name,
            version = %card.version,
            interfaces = card.supported_interfaces.len(),
            "agent card fetched",
        );
        for iface in &card.supported_interfaces {
            tracing::info!(
                protocol = %iface.protocol_binding,
                url = %iface.url,
                "  interface",
            );
        }

        // ------------------------------------------------------------------
        // Step 2: for each protocol, build a factory pinned to that binding
        //         and create one client from the card
        // ------------------------------------------------------------------
        for &protocol in protocols {
            let factory = A2AClientFactory::builder()
                .register(Arc::new(GrpcTransportFactory))
                .preferred_bindings(vec![protocol.to_string()])
                .build();

            let client = match factory.create_from_card(&card).await {
                Ok(c) => c,
                Err(e) => {
                    eprintln!("[{protocol}] skipped: {e}");
                    continue;
                }
            };

            // ----------------------------------------------------------------
            // Step 3: call the server methods
            // ----------------------------------------------------------------
            exercise_client(protocol, &client).await;
        }
    }
}

/// A request whose task completes immediately (Working → Completed).
fn req(text: &str) -> SendMessageRequest {
    SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text(text)]),
        configuration: None,
        metadata: None,
        tenant: None,
    }
}

/// A request whose task stays `Working` until explicitly cancelled.
/// `return_immediately` is set so that `send_message` returns as soon as the
/// first `Working` event arrives instead of blocking forever.
fn wait_req(text: &str) -> SendMessageRequest {
    SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text(format!("wait:{text}"))]),
        configuration: Some(SendMessageConfiguration {
            return_immediately: Some(true),
            accepted_output_modes: None,
            push_notification_config: None,
            history_length: None,
        }),
        metadata: None,
        tenant: None,
    }
}

/// Return the last 8 hex characters of a UUID string for compact display.
fn short_id(id: &str) -> &str {
    if id.len() > 8 { &id[id.len() - 8..] } else { id }
}

/// Render a TaskState as a compact symbol + label.
fn fmt_state(state: &TaskState) -> &'static str {
    match state {
        TaskState::Working   => "⟳ working",
        TaskState::Completed => "✓ completed",
        TaskState::Canceled  => "✗ canceled",
        TaskState::Failed        => "! failed",
        TaskState::Submitted     => "· submitted",
        TaskState::Rejected      => "✗ rejected",
        TaskState::AuthRequired  => "? auth-required",
        TaskState::InputRequired => "? input-required",
        TaskState::Unspecified   => "? unspecified",
    }
}

/// Format a top-level method row: `<method:26>  …<id:8>  <state>`
macro_rules! row {
    ($method:expr, $id:expr, $state:expr) => {
        println!("{:<26}  …{}  {}", $method, short_id($id), fmt_state($state))
    };
}

/// Format a streaming sub-event row (indented): `  [n] <kind:13>  …<id:8>  <state>`
macro_rules! sub_row {
    ($n:expr, $kind:expr, $id:expr, $state:expr) => {
        println!("  [{n:>2}] {:<13}  …{}  {}", $kind, short_id($id), fmt_state($state), n = $n)
    };
    ($n:expr, $kind:expr, $id:expr) => {
        println!("  [{n:>2}] {:<13}  …{}", $kind, short_id($id), n = $n)
    };
}

/// Call every A2A method against the given client and print the results.
async fn exercise_client<T: Transport>(protocol: &str, client: &A2AClient<T>) {
    let sep = "─".repeat(50);

    println!();
    println!("{protocol}");
    println!("{sep}");

    // ---- send_message ----------------------------------------------------
    let task_id = match client.send_message(&req("Hello, world!")).await {
        Ok(SendMessageResponse::Task(t)) => {
            row!("send_message", &t.id, &t.status.state);
            t.id
        }
        Ok(SendMessageResponse::Message(m)) => {
            row!("send_message", &m.message_id, &TaskState::Unspecified);
            return;
        }
        Err(e) => {
            eprintln!("send_message               ✗  {e}");
            return;
        }
    };

    // ---- get_task --------------------------------------------------------
    match client
        .get_task(&GetTaskRequest { id: task_id.clone(), history_length: Some(10), tenant: None })
        .await
    {
        Ok(t)  => row!("get_task", &t.id, &t.status.state),
        Err(e) => eprintln!("get_task                   ✗  {e}"),
    }

    // ---- list_tasks ------------------------------------------------------
    match client
        .list_tasks(&ListTasksRequest {
            context_id: None,
            status: None,
            page_size: Some(10),
            page_token: None,
            history_length: None,
            status_timestamp_after: None,
            include_artifacts: None,
            tenant: None,
        })
        .await
    {
        Ok(l)  => println!("{:<26}  {} task(s)", "list_tasks", l.tasks.len()),
        Err(e) => eprintln!("list_tasks                 ✗  {e}"),
    }

    // ---- cancel_task -----------------------------------------------------
    match client.send_message(&wait_req("cancel demo")).await {
        Ok(SendMessageResponse::Task(t)) => {
            match client
                .cancel_task(&CancelTaskRequest { id: t.id, metadata: None, tenant: None })
                .await
            {
                Ok(t)  => row!("cancel_task", &t.id, &t.status.state),
                Err(e) => eprintln!("cancel_task                ✗  {e}"),
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("cancel_task                ✗  {e}"),
    }

    // ---- send_streaming_message ------------------------------------------
    let stream_req = SendMessageRequest {
        message: Message::new(Role::User, vec![Part::text("Stream me!")]),
        configuration: None,
        metadata: None,
        tenant: None,
    };
    match client.send_streaming_message(&stream_req).await {
        Ok(mut stream) => {
            println!("send_streaming_message");
            let mut n = 0usize;
            while let Some(event) = stream.next().await {
                n += 1;
                match event {
                    Ok(StreamResponse::Task(t)) =>
                        sub_row!(n, "task", &t.id, &t.status.state),
                    Ok(StreamResponse::StatusUpdate(u)) =>
                        sub_row!(n, "status_update", &u.task_id, &u.status.state),
                    Ok(StreamResponse::Message(m)) =>
                        sub_row!(n, "message", &m.message_id),
                    Ok(StreamResponse::ArtifactUpdate(a)) =>
                        sub_row!(n, "artifact", &a.artifact.artifact_id),
                    Err(e) =>
                        eprintln!("  [{n:>2}] ✗  {e}"),
                }
            }
        }
        Err(e) => eprintln!("send_streaming_message     ✗  {e}"),
    }

    // ---- subscribe_to_task -----------------------------------------------
    match client.send_message(&wait_req("subscribe demo")).await {
        Ok(SendMessageResponse::Task(t)) => {
            let sub_id = t.id;
            match client
                .subscribe_to_task(&SubscribeToTaskRequest { id: sub_id.clone(), tenant: None })
                .await
            {
                Ok(mut sub) => {
                    match client
                        .cancel_task(&CancelTaskRequest { id: sub_id.clone(), metadata: None, tenant: None })
                        .await
                    {
                        Ok(t)  => row!("subscribe_to_task (cancel)", &t.id, &t.status.state),
                        Err(e) => eprintln!("subscribe_to_task (cancel) ✗  {e}"),
                    }
                    let mut n = 0usize;
                    while let Some(event) = sub.next().await {
                        n += 1;
                        match event {
                            Ok(StreamResponse::Task(t)) =>
                                sub_row!(n, "task", &t.id, &t.status.state),
                            Ok(StreamResponse::StatusUpdate(u)) =>
                                sub_row!(n, "status_update", &u.task_id, &u.status.state),
                            Ok(_) => {}
                            Err(e) => eprintln!("  [{n:>2}] ✗  {e}"),
                        }
                    }
                }
                Err(e) => eprintln!("subscribe_to_task          ✗  {e}"),
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("subscribe_to_task          ✗  {e}"),
    }

    println!("{sep}");
}
