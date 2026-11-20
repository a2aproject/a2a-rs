// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Inventory agent server for the collaborative task e-commerce example.
//!
//! Run:
//!   cargo run --bin collaborate-inventory --package examples

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();
    examples_lib::collaborate::server::run_server(
        "inventory",
        "E-commerce inventory agent: knows stock levels and restock timelines.",
        9002,
    )
    .await;
}
