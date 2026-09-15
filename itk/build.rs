// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0

/// Where `instruction.proto` may live, most authoritative first:
///
/// 1. the live a2a-itk checkout that `run_itk.sh` and the nightly workflow
///    create, so those always build against upstream;
/// 2. the path baked into the ITK Docker image;
/// 3. the copy vendored in this repository.
///
/// The vendored copy is last precisely so it never shadows upstream. It
/// exists so a plain checkout of this workspace builds — before it, this
/// script panicked whenever a2a-itk was absent, which made the crate
/// unbuildable by default and blocked adding it to CI (#207).
const PROTO_DIRS: [&str; 3] = ["a2a-itk/protos", "/tmp/protos", "protos"];

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Rerun if any candidate appears or changes, so cloning a2a-itk after a
    // build picks up the real proto rather than leaving the vendored one
    // compiled in.
    for dir in PROTO_DIRS {
        println!("cargo:rerun-if-changed={dir}/instruction.proto");
    }

    let proto_dir = PROTO_DIRS
        .iter()
        .find(|dir| std::path::Path::new(&format!("{dir}/instruction.proto")).exists())
        .ok_or_else(|| {
            format!(
                "instruction.proto not found in any of {PROTO_DIRS:?}. The vendored copy at \
                 itk/protos/instruction.proto should always be present — if it is missing, \
                 restore it or run run_itk.sh to clone a2a-itk."
            )
        })?;

    if *proto_dir == PROTO_DIRS[2] {
        println!(
            "cargo:warning=itk: building against the vendored instruction.proto. Run \
             run_itk.sh to check out a2a-itk if you need the upstream definition."
        );
    }

    prost_build::compile_protos(&[format!("{proto_dir}/instruction.proto")], &[proto_dir])?;
    Ok(())
}
