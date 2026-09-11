// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

#[tokio::main]
async fn main() {
    if let Err(error) = a2acli::run_args(std::env::args_os()).await {
        error.report();
        std::process::exit(error.exit_code());
    }
}
