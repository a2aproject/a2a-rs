// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

#[tokio::main]
async fn main() {
    match a2acli::run_args(std::env::args_os()).await {
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
