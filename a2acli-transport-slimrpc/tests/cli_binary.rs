// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Runs the built `a2a-transport-slimrpc` binary as a subprocess.
//!
//! `main()`'s process-wide setup (tracing subscriber init, crypto provider
//! install) can each only run once per process, so it can't be exercised
//! from a unit test in the same test binary. An integration test gets a
//! real, separate process for free, and with it `CARGO_BIN_EXE_*`, which
//! only Cargo sets for integration tests, not for a bin crate's own unit
//! tests referencing itself.

use std::process::Command;

fn binary() -> Command {
    Command::new(env!("CARGO_BIN_EXE_a2a-transport-slimrpc"))
}

#[test]
fn test_main_exits_nonzero_and_reports_a_missing_config_env_var() {
    let output = binary()
        .args(["serve", "--endpoint", "org/namespace/agent"])
        .env_remove("A2A_SLIMRPC_PLUGIN_CONFIG")
        .output()
        .expect("failed to run the built binary");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("a2a-transport-slimrpc:"),
        "unexpected stderr: {stderr}"
    );

    // The a2a-cli plugin contract reads a single JSON line from stdout to
    // learn startup succeeded or failed; without it, the host sees a bare
    // EOF instead of our actual error (see a2a-cli's
    // internal/transportplugin/launch.go: readHandshake).
    let stdout = String::from_utf8_lossy(&output.stdout);
    let handshake: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap_or(""))
        .unwrap_or_else(|e| {
            panic!("stdout was not a single JSON handshake line: {e}\nstdout: {stdout}")
        });
    assert_eq!(handshake["success"], false);
    assert!(
        handshake["error"]
            .as_str()
            .is_some_and(|e| e.contains("A2A_SLIMRPC_PLUGIN_CONFIG")),
        "unexpected handshake: {handshake}"
    );
}

#[test]
fn test_main_prints_info_json_and_exits_zero() {
    let output = binary()
        .arg("info")
        .output()
        .expect("failed to run the built binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(json["name"], "slimrpc");
}
