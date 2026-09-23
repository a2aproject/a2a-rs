// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

//! Captures the git commit and its date at build time, for the `info`
//! subcommand. Falls back to "unknown" when `.git` isn't available (e.g.
//! building from a published source tarball), rather than failing the build.

use std::process::Command;

fn main() {
    let commit =
        git_output(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    let commit_date =
        git_output(&["log", "-1", "--format=%cI"]).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=A2ACLI_SLIMRPC_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=A2ACLI_SLIMRPC_BUILD_DATE={commit_date}");
}

fn git_output(args: &[&str]) -> Option<String> {
    let output = Command::new("git").args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}
