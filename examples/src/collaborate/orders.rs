// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Orders agent server for the collaborative task e-commerce example.
//!
//! Run:
//!   cargo run --bin collaborate-orders --package examples

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();
    examples_lib::collaborate::server::run_server(
        "orders",
        "E-commerce orders agent: knows order status and fulfillment details.",
        9001,
    )
    .await;
}
