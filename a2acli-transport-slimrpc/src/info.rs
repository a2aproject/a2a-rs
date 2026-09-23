// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
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

fn build_info() -> Info {
    Info {
        name: "slimrpc",
        version: env!("CARGO_PKG_VERSION"),
        description: "SLIMRPC transport plugin for a2a-cli",
        protocol: a2a::VERSION,
        binding: a2a::TRANSPORT_PROTOCOL_GRPC,
    }
}

pub fn run() -> Result<(), crate::error::PluginError> {
    let json = serde_json::to_string(&build_info())
        .map_err(|e| crate::error::PluginError::Handshake(e.to_string()))?;
    println!("{json}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_info_reports_the_grpc_binding() {
        let info = build_info();
        assert_eq!(info.name, "slimrpc");
        assert_eq!(info.binding, a2a::TRANSPORT_PROTOCOL_GRPC);
    }

    #[test]
    fn test_info_serializes_with_the_expected_keys() {
        let json = serde_json::to_value(build_info()).unwrap();
        for key in ["name", "version", "description", "protocol", "binding"] {
            assert!(json.get(key).is_some(), "missing key: {key}");
        }
    }

    #[test]
    fn test_run_prints_the_info_json_and_returns_ok() {
        assert!(run().is_ok());
    }
}
