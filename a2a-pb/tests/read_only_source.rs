// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0

/// The build script writes only to `OUT_DIR`, so the crate builds from a
/// read-only source such as `cargo vendor` output or the Nix store.
#[test]
fn build_writes_nothing_into_the_source_tree() {
    let generated = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/gen");
    assert!(!generated.exists(), "{} exists", generated.display());
}
