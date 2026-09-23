// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
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
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serve_parses_the_endpoint_flag() {
        let cli = Cli::try_parse_from([
            "a2a-transport-slimrpc",
            "serve",
            "--endpoint",
            "slim://acme/billing/invoicer",
        ])
        .unwrap();
        match cli.command {
            Command::Serve { endpoint } => assert_eq!(endpoint, "slim://acme/billing/invoicer"),
            Command::Info => panic!("expected Serve"),
        }
    }

    #[test]
    fn test_info_takes_no_arguments() {
        let cli = Cli::try_parse_from(["a2a-transport-slimrpc", "info"]).unwrap();
        assert!(matches!(cli.command, Command::Info));
    }

    #[test]
    fn test_serve_requires_the_endpoint_flag() {
        assert!(Cli::try_parse_from(["a2a-transport-slimrpc", "serve"]).is_err());
    }

    #[test]
    fn test_an_unknown_subcommand_is_rejected() {
        assert!(Cli::try_parse_from(["a2a-transport-slimrpc", "bogus"]).is_err());
    }
}
