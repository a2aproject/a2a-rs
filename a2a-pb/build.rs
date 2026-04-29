// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::path::{Path, PathBuf};

const PROTO_FILES: &[&str] = &["proto/a2a.proto"];
const PROTO_INCLUDES: &[&str] = &["proto"];
const PROTOJSON_PACKAGES: &[&str] = &[".lf.a2a.v1"];
const NULL_REPEATED_FIELD_REPLACEMENTS: &[(&str, &str)] = &[
    (
        "extensions__ = Some(map_.next_value()?);",
        "extensions__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "supported_interfaces__ = Some(map_.next_value()?);",
        "supported_interfaces__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "security_requirements__ = Some(map_.next_value()?);",
        "security_requirements__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "default_input_modes__ = Some(map_.next_value()?);",
        "default_input_modes__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "default_output_modes__ = Some(map_.next_value()?);",
        "default_output_modes__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "skills__ = Some(map_.next_value()?);",
        "skills__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "signatures__ = Some(map_.next_value()?);",
        "signatures__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "tags__ = Some(map_.next_value()?);",
        "tags__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "examples__ = Some(map_.next_value()?);",
        "examples__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "input_modes__ = Some(map_.next_value()?);",
        "input_modes__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "output_modes__ = Some(map_.next_value()?);",
        "output_modes__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "parts__ = Some(map_.next_value()?);",
        "parts__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "reference_task_ids__ = Some(map_.next_value()?);",
        "reference_task_ids__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "accepted_output_modes__ = Some(map_.next_value()?);",
        "accepted_output_modes__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "list__ = Some(map_.next_value()?);",
        "list__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "configs__ = Some(map_.next_value()?);",
        "configs__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "tasks__ = Some(map_.next_value()?);",
        "tasks__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "artifacts__ = Some(map_.next_value()?);",
        "artifacts__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "history__ = Some(map_.next_value()?);",
        "history__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
];
const NULL_MAP_REPLACEMENT: (&str, &str) = (
    "map_.next_value::<std::collections::HashMap<_, _>>()?",
    "map_.next_value::<Option<std::collections::HashMap<_, _>>>()?.unwrap_or_default()",
);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var_os("PROTOC").is_none() {
        let protoc_path = protoc_bin_vendored::protoc_bin_path()?;

        unsafe {
            #[allow(clippy::disallowed_methods)]
            std::env::set_var("PROTOC", protoc_path);
        }
    }

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir("src/gen")
        .compile_protos(PROTO_FILES, PROTO_INCLUDES)?;

    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let descriptor_path = out_dir.join("a2a-descriptor.bin");

    tonic_prost_build::configure()
        .build_server(false)
        .build_client(false)
        .out_dir(&out_dir)
        .file_descriptor_set_path(&descriptor_path)
        .compile_well_known_types(true)
        .extern_path(".google.protobuf", "::pbjson_types")
        .compile_protos(PROTO_FILES, PROTO_INCLUDES)?;

    let descriptor_set = std::fs::read(&descriptor_path)?;
    let mut builder = pbjson_build::Builder::new();
    builder.out_dir(&out_dir);
    builder.register_descriptors(&descriptor_set)?;
    builder.build(PROTOJSON_PACKAGES)?;
    patch_generated_protojson_serde(&out_dir)?;

    Ok(())
}

fn patch_generated_protojson_serde(out_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let serde_path = out_dir.join("lf.a2a.v1.serde.rs");
    let mut generated = std::fs::read_to_string(&serde_path)?;

    generated = replace_if_present(generated, NULL_MAP_REPLACEMENT.0, NULL_MAP_REPLACEMENT.1);

    for &(from, to) in NULL_REPEATED_FIELD_REPLACEMENTS {
        generated = replace_if_present(generated, from, to);
    }

    std::fs::write(serde_path, generated)?;
    Ok(())
}

fn replace_if_present(source: String, from: &str, to: &str) -> String {
    if source.contains(from) {
        source.replace(from, to)
    } else {
        source
    }
}
