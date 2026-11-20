// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Shipping agent server for the collaborative task e-commerce example.
//!
//! Run:
//!   cargo run --bin collaborate-shipping --package examples

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();
    examples_lib::collaborate::server::run_server(
        "shipping",
        "E-commerce shipping agent: knows tracking status and delivery estimates.",
        9003,
    )
    .await;
}
