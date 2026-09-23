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

/// Serializes tests that touch the `A2A_SLIMRPC_PLUGIN_CONFIG` env var, which
/// is process-global and would otherwise race across `cargo test`'s parallel
/// threads. Held for the duration of any such test, across `.await` points,
/// hence `tokio::sync::Mutex` rather than `std::sync::Mutex`.
#[cfg(test)]
pub(crate) static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Runs the parsed subcommand. Split out from `main` so it's testable
/// without the process-wide setup (tracing/crypto-provider init, which can't
/// safely run twice in the same process) or `std::process::exit`.
async fn dispatch(command: Command) -> Result<(), error::PluginError> {
    match command {
        Command::Serve { endpoint } => serve::run(&endpoint).await,
        Command::Info => info::run(),
    }
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

    if let Err(e) = dispatch(cli.command).await {
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

    #[tokio::test]
    async fn test_dispatch_info_succeeds() {
        assert!(dispatch(Command::Info).await.is_ok());
    }

    #[tokio::test]
    async fn test_dispatch_serve_reports_a_missing_config_env_var() {
        let _guard = ENV_LOCK.lock().await;
        // SAFETY: ENV_LOCK serializes every test in this crate that touches
        // this env var.
        unsafe {
            std::env::remove_var("A2A_SLIMRPC_PLUGIN_CONFIG");
        }
        let err = dispatch(Command::Serve {
            endpoint: "org/namespace/agent".to_string(),
        })
        .await
        .unwrap_err();
        assert!(matches!(err, error::PluginError::ConfigEnvMissing));
    }
}
