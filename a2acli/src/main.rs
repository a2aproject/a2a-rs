// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use clap::Parser;

#[tokio::main]
async fn main() {
    let cli = a2acli::Cli::parse();
    match a2acli::run(cli).await {
        Ok(()) => {}
        Err(a2acli::CliError::A2A(error)) => {
            eprintln!("a2a error {}: {}", error.code, error.message);
            std::process::exit(1);
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
