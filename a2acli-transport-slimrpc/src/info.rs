// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

use serde::Serialize;

#[derive(Serialize)]
pub struct Info {
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
    pub protocol: &'static str,
    pub binding: &'static str,
}

pub fn run() -> Result<(), crate::error::PluginError> {
    let info = Info {
        name: "slimrpc",
        version: env!("CARGO_PKG_VERSION"),
        description: "SLIMRPC transport plugin for a2a-cli",
        protocol: a2a::VERSION,
        binding: a2a::TRANSPORT_PROTOCOL_GRPC,
    };
    let json = serde_json::to_string(&info)
        .map_err(|e| crate::error::PluginError::Handshake(e.to_string()))?;
    println!("{json}");
    Ok(())
}
