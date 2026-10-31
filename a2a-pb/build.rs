// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
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
    (
        "timeline__ = Some(map_.next_value()?);",
        "timeline__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
    ),
    (
        "artifact_updates__ = Some(map_.next_value()?);",
        "artifact_updates__ = Some(map_.next_value::<Option<_>>()?.unwrap_or_default());",
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

    // Only OUT_DIR is ours to write: the crate's own sources can be read-only
    // (`cargo vendor`, Nix, a sandboxed build).
    let out_dir = PathBuf::from(std::env::var("OUT_DIR")?);
    let grpc_out_dir = out_dir.join("grpc");
    std::fs::create_dir_all(&grpc_out_dir)?;

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .out_dir(&grpc_out_dir)
        .compile_protos(PROTO_FILES, PROTO_INCLUDES)?;

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
    // Spec §5.7: implementations SHOULD ignore unrecognized fields in
    // messages, allowing for forward compatibility. Unknown enum string
    // values are still rejected.
    builder.ignore_unknown_fields();
    builder.build(PROTOJSON_PACKAGES)?;
    patch_generated_protojson_serde(&out_dir, &descriptor_set)?;

    Ok(())
}

fn patch_generated_protojson_serde(
    out_dir: &Path,
    descriptor_set: &[u8],
) -> Result<(), Box<dyn std::error::Error>> {
    let serde_path = out_dir.join("lf.a2a.v1.serde.rs");
    let mut generated = std::fs::read_to_string(&serde_path)?;

    generated = replace_if_present(generated, NULL_MAP_REPLACEMENT.0, NULL_MAP_REPLACEMENT.1);

    for &(from, to) in NULL_REPEATED_FIELD_REPLACEMENTS {
        generated = replace_if_present(generated, from, to);
    }

    for (message, json_name) in timestamp_fields(descriptor_set)? {
        generated = write_timestamp_as_wire_form(generated, &message, &json_name)?;
    }

    std::fs::write(serde_path, generated)?;
    Ok(())
}

/// The `(message, JSON name)` of every `google.protobuf.Timestamp` field.
///
/// pbjson writes a Timestamp with chrono's `to_rfc3339`, which ends in
/// `+00:00`; the wire form is `Z` (spec §5.6.1). Only a plain field of a
/// top-level message is handled, which is every Timestamp in the proto today;
/// any other shape fails the build rather than reaching the wire unpatched.
fn timestamp_fields(
    descriptor_set: &[u8],
) -> Result<Vec<(String, String)>, Box<dyn std::error::Error>> {
    use prost::Message;
    use prost_types::{
        DescriptorProto, FileDescriptorSet, field_descriptor_proto::Label as FieldLabel,
    };

    const TIMESTAMP: &str = ".google.protobuf.Timestamp";

    fn nested_timestamp(message: &DescriptorProto) -> Option<&str> {
        message
            .nested_type
            .iter()
            .find(|nested| {
                nested.field.iter().any(|f| f.type_name() == TIMESTAMP)
                    || nested_timestamp(nested).is_some()
            })
            .map(|nested| nested.name())
    }

    let mut fields = Vec::new();
    for file in FileDescriptorSet::decode(descriptor_set)?.file {
        if !PROTOJSON_PACKAGES.contains(&format!(".{}", file.package()).as_str()) {
            continue;
        }
        for message in &file.message_type {
            if let Some(nested) = nested_timestamp(message) {
                return Err(format!(
                    "{}.{nested} has a google.protobuf.Timestamp field; \
                     build.rs only writes those of top-level messages in wire form",
                    message.name()
                )
                .into());
            }
            for field in &message.field {
                if field.type_name() != TIMESTAMP {
                    continue;
                }
                if field.label() != FieldLabel::Optional || field.oneof_index.is_some() {
                    return Err(format!(
                        "{}.{} is a repeated or oneof google.protobuf.Timestamp field; \
                         build.rs only writes a plain one in wire form",
                        message.name(),
                        field.name()
                    )
                    .into());
                }
                fields.push((message.name().to_string(), field.json_name().to_string()));
            }
        }
    }
    Ok(fields)
}

/// Route one Timestamp field through `crate::timestamp::WireTimestamp`.
///
/// The edit has to land exactly once inside the message's own `Serialize`
/// impl, and a miss fails the build: a pbjson upgrade that changes its output
/// must not silently bring back the `+00:00` form.
fn write_timestamp_as_wire_form(
    generated: String,
    message: &str,
    json_name: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let impl_start = format!("impl serde::Serialize for {message} {{");
    let impl_end = format!("impl<'de> serde::Deserialize<'de> for {message} {{");
    let call = format!("struct_ser.serialize_field(\"{json_name}\", v)?;");
    let wired = format!(
        "struct_ser.serialize_field(\"{json_name}\", &crate::timestamp::WireTimestamp(v))?;"
    );

    let not_found = || format!("pbjson output changed: no `{call}` to patch in {message}");
    let start = generated.find(&impl_start).ok_or_else(not_found)?;
    let end = start + generated[start..].find(&impl_end).ok_or_else(not_found)?;
    let serialize_impl = &generated[start..end];
    if serialize_impl.matches(&call).count() != 1 {
        return Err(not_found().into());
    }
    Ok(format!(
        "{}{}{}",
        &generated[..start],
        serialize_impl.replace(&call, &wired),
        &generated[end..]
    ))
}

fn replace_if_present(source: String, from: &str, to: &str) -> String {
    if source.contains(from) {
        source.replace(from, to)
    } else {
        source
    }
}
