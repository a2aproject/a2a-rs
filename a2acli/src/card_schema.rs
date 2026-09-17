// Copyright AGNTCY Contributors (https://github.com/agntcy)
// Copyright A2A Contributors (https://github.com/a2aproject)
// SPDX-License-Identifier: Apache-2.0
//! Agent Card validation against the vendored A2A JSON Schema (§10.1).

use std::collections::HashMap;

use serde::Serialize;
use serde_json::{Map, Value};

/// The canonical A2A schema bundle, vendored byte-identical to upstream so a
/// reviewer can diff it. Its `$ref`s are retargeted at load time rather than
/// in the file, keeping the copy honest.
const BUNDLE: &str = include_str!("../schema/a2a.json");

/// Which A2A the vendored bundle came from: "valid" says nothing without
/// naming what it was valid against.
pub const SCHEMA_A2A_VERSION: &str = "v1.0.1-48-g2b93e318";

/// The bundle's name for an Agent Card. Its definitions are spaced words
/// rather than type names.
const AGENT_CARD: &str = "Agent Card";

/// One schema violation, located by JSON pointer into the card.
///
/// `Serialize` derives directly, rather than through a second CLI-side type:
/// this is exactly the shape `-o json` reports each violation as under
/// `card get --validate`'s error envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Violation {
    /// JSON pointer to the offending value; empty for the card itself.
    pub path: String,
    pub message: String,
}

/// Normalised key for matching a `$ref` filename to a definition name:
/// upstream spells the same type `APIKeySecurityScheme` in a ref and
/// `API Key Security Scheme` in a definition, and its own splitting is not
/// consistent (`Authorization CodeO Auth Flow`), so compare on letters alone.
fn normalise(name: &str) -> String {
    name.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The normalised key a `$ref` names, if the bundle has a definition for it.
///
/// A ref is one of two shapes: an internal `#/definitions/Agent Card`
/// (the bundle's own definitions cross-reference each other this way), or an
/// external sibling file the bundle does not ship
/// (`a2a.v1.AgentSkill.jsonschema.json`, `google.protobuf.Struct.jsonschema.json`).
/// Both name a definition by a spelling that differs from its key, so both
/// are redirected to the same normalised key the definitions are rehoused
/// under -- which also sidesteps the definitions' names containing spaces,
/// not legal in a URI fragment (`#/definitions/Agent Card` does not parse).
fn definition_key(reference: &str, index: &HashMap<String, String>) -> Option<String> {
    let name = reference
        .strip_prefix("#/definitions/")
        .unwrap_or(reference);
    let base = name.strip_suffix(".jsonschema.json").unwrap_or(name);
    let type_name = base.rsplit('.').next()?;
    let key = normalise(type_name);
    index.contains_key(&key).then_some(key)
}

/// Rewrites every `$ref` -- internal or external -- to the matching
/// normalised key. Returns the refs it could not place, so a bundle update
/// cannot silently leave part of the card unvalidated.
fn retarget_refs(node: &mut Value, index: &HashMap<String, String>, unresolved: &mut Vec<String>) {
    match node {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get("$ref") {
                match definition_key(reference, index) {
                    Some(key) => {
                        let pointer = format!("#/definitions/{key}");
                        map.insert("$ref".to_string(), Value::String(pointer));
                    }
                    None => unresolved.push(reference.clone()),
                }
            }
            for value in map.values_mut() {
                retarget_refs(value, index, unresolved);
            }
        }
        Value::Array(items) => {
            for item in items {
                retarget_refs(item, index, unresolved);
            }
        }
        _ => {}
    }
}

/// The Agent Card schema, as a self-contained document.
///
/// The bundle's definitions are rehoused under normalised, space-free keys
/// before anything references them, since `Agent Card` and its siblings
/// cannot appear in a `$ref` as spelled.
fn agent_card_schema() -> Result<Value, String> {
    let bundle: Value =
        serde_json::from_str(BUNDLE).map_err(|e| format!("vendored schema is not JSON: {e}"))?;
    build_schema(bundle)
}

/// The guts of [`agent_card_schema`], taking the bundle as a `Value` rather
/// than reading the vendored file, so its defensive paths -- a bundle update
/// that drops the `Agent Card` definition, or adds a `$ref` this index can't
/// place -- are exercised against a deliberately broken fixture instead of
/// staying dead code no real (i.e. currently valid) bundle would reach.
fn build_schema(mut bundle: Value) -> Result<Value, String> {
    let original = bundle
        .get("definitions")
        .and_then(Value::as_object)
        .ok_or("vendored schema has no definitions")?
        .clone();
    let index: HashMap<String, String> = original
        .keys()
        .map(|name| (normalise(name), name.clone()))
        .collect();

    let card_key = normalise(AGENT_CARD);
    if !index.contains_key(&card_key) {
        return Err(format!("vendored schema has no {AGENT_CARD} definition"));
    }

    let mut unresolved = Vec::new();
    retarget_refs(&mut bundle, &index, &mut unresolved);
    if !unresolved.is_empty() {
        unresolved.sort();
        unresolved.dedup();
        return Err(format!(
            "vendored schema has unplaceable references: {}",
            unresolved.join(", ")
        ));
    }

    let rehoused: Map<String, Value> = bundle
        .get("definitions")
        .and_then(Value::as_object)
        .ok_or("vendored schema has no definitions")?
        .iter()
        .map(|(name, schema)| (normalise(name), schema.clone()))
        .collect();

    let mut document = Map::new();
    document.insert(
        "$schema".to_string(),
        Value::String("https://json-schema.org/draft/2020-12/schema".to_string()),
    );
    document.insert(
        "$ref".to_string(),
        Value::String(format!("#/definitions/{card_key}")),
    );
    document.insert("definitions".to_string(), Value::Object(rehoused));
    Ok(Value::Object(document))
}

/// Every violation in `card`, ordered by position so the report is stable.
///
/// All of them, not just the first: a card with three problems should take
/// one run to diagnose.
pub fn validate_agent_card(card: &Value) -> Result<Vec<Violation>, String> {
    let schema = agent_card_schema()?;
    let validator = jsonschema::validator_for(&schema)
        .map_err(|e| format!("vendored schema does not compile: {e}"))?;

    let mut violations: Vec<Violation> = validator
        .iter_errors(card)
        .map(|error| Violation {
            path: error.instance_path().to_string(),
            message: error.to_string(),
        })
        .collect();
    violations.sort_by(|a, b| (&a.path, &a.message).cmp(&(&b.path, &b.message)));
    violations.dedup();
    Ok(violations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn valid_card() -> Value {
        json!({
            "name": "Fixture Agent",
            "description": "a card that satisfies the schema",
            "version": "1.0.0",
            "supportedInterfaces": [
                {"url": "http://localhost:3000/jsonrpc", "protocolBinding": "JSONRPC"}
            ],
            "capabilities": {"streaming": true},
            "defaultInputModes": ["text/plain"],
            "defaultOutputModes": ["text/plain"],
            "skills": []
        })
    }

    /// The bundle ships refs to sibling files it does not contain, so this is
    /// the load-bearing step: if a future update adds a type the index cannot
    /// place, it must fail loudly rather than skip validating that subtree.
    #[test]
    fn every_reference_in_the_bundle_resolves() {
        let schema = agent_card_schema().expect("bundle should load");
        let rendered = serde_json::to_string(&schema).unwrap();
        assert!(
            !rendered.contains(".jsonschema.json"),
            "an external reference survived retargeting"
        );
        assert!(jsonschema::validator_for(&schema).is_ok());
    }

    /// The real vendored bundle always has an `Agent Card` definition, so
    /// this defensive check needs a fixture that deliberately lacks one.
    #[test]
    fn a_bundle_missing_the_agent_card_definition_is_rejected() {
        let bundle = json!({
            "definitions": {
                "Something Else": {"type": "object"}
            }
        });
        let error = build_schema(bundle).unwrap_err();
        assert!(error.contains("Agent Card"), "{error}");
    }

    /// Likewise, the real bundle's refs always resolve; this needs one that
    /// deliberately doesn't.
    #[test]
    fn a_bundle_with_an_unplaceable_reference_is_rejected() {
        let bundle = json!({
            "definitions": {
                "Agent Card": {"$ref": "#/definitions/NoSuchType"}
            }
        });
        let error = build_schema(bundle).unwrap_err();
        assert!(error.contains("NoSuchType"), "{error}");
    }

    #[test]
    fn a_valid_card_has_no_violations() {
        assert_eq!(validate_agent_card(&valid_card()).unwrap(), vec![]);
    }

    #[test]
    fn an_unknown_property_is_reported_with_its_path() {
        let mut card = valid_card();
        card["speling"] = json!("mistake");

        let violations = validate_agent_card(&card).unwrap();
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert!(
            violations[0].message.contains("speling"),
            "{:?}",
            violations[0]
        );
    }

    #[test]
    fn a_wrong_type_is_reported_with_its_path() {
        let mut card = valid_card();
        card["name"] = json!(42);

        let violations = validate_agent_card(&card).unwrap();
        assert_eq!(violations.len(), 1, "{violations:?}");
        assert_eq!(violations[0].path, "/name");
    }

    /// The whole point of collecting rather than short-circuiting.
    #[test]
    fn several_violations_are_all_reported_each_with_its_own_path() {
        let mut card = valid_card();
        card["name"] = json!(42);
        card["defaultInputModes"] = json!("text/plain");
        card["nonsense"] = json!(true);

        let violations = validate_agent_card(&card).unwrap();
        let paths: Vec<&str> = violations.iter().map(|v| v.path.as_str()).collect();
        assert!(paths.contains(&"/name"), "{violations:?}");
        assert!(paths.contains(&"/defaultInputModes"), "{violations:?}");
        assert!(violations.len() >= 3, "{violations:?}");
    }

    /// Enums reach the bundle as an `anyOf` of a pattern, an enum and an
    /// integer range, so an unrecognised member still fails every branch.
    #[test]
    fn an_unrecognised_enum_member_is_reported() {
        let mut card = valid_card();
        card["skills"] = json!([{
            "id": "s1",
            "name": "skill",
            "description": "d",
            "tags": [],
            "security": [{"schemes": {"oauth": {"oauth2": {"flows": {"unknownFlow": {}}}}}}]
        }]);

        let violations = validate_agent_card(&card).unwrap();
        assert!(!violations.is_empty(), "an invalid nested shape passed");
        assert!(
            violations.iter().all(|v| v.path.starts_with("/skills")),
            "{violations:?}"
        );
    }

    #[test]
    fn the_schema_records_which_a2a_it_came_from() {
        assert!(SCHEMA_A2A_VERSION.starts_with("v1."));
    }
}
