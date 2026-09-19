// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

mod error;
mod info;
mod serve;
mod tls;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(
    name = "a2a-transport-slimrpc",
    version,
    about = "SLIMRPC transport plugin for a2a-cli"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Start the loopback proxy server for a given SLIMRPC endpoint.
    Serve {
        /// SLIMRPC upstream endpoint (e.g. slim://org/namespace/agent).
        #[arg(long)]
        endpoint: String,
    },
    /// Print plugin metadata as JSON and exit.
    Info,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();

    let cli = Cli::parse();

    let result = match cli.command {
        Command::Serve { endpoint } => serve::run(&endpoint).await,
        Command::Info => info::run(),
    };

    if let Err(e) = result {
        eprintln!("a2a-transport-slimrpc: {e}");
        std::process::exit(1);
    }
}
