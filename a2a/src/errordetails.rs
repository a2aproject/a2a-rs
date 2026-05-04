// Copyright AGNTCY Contributors (https://github.com/agntcy)
// SPDX-License-Identifier: Apache-2.0
use std::collections::HashMap;

use serde::{Deserialize, Serialize, de::Deserializer, ser::Serializer};
use serde_json::Value;

pub const ERROR_INFO_TYPE: &str = "type.googleapis.com/google.rpc.ErrorInfo";
pub const BAD_REQUEST_TYPE: &str = "type.googleapis.com/google.rpc.BadRequest";
pub const STRUCT_TYPE: &str = "type.googleapis.com/google.protobuf.Struct";
pub const PROTOCOL_DOMAIN: &str = "a2a-protocol.org";

/// A field-level validation error, matching `google.rpc.BadRequest.FieldViolation`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldViolation {
    pub field: String,
    pub description: String,
}

/// A typed error detail object using ProtoJSON `Any` representation.
///
/// Each detail object carries a `@type` URL identifying its schema and
/// a map of additional fields. When serialized to JSON, `@type` is
/// flattened into the object alongside the value fields.
#[derive(Debug, Clone, PartialEq)]
pub struct TypedDetail {
    pub type_url: String,
    pub value: HashMap<String, Value>,
}

impl TypedDetail {
    pub fn new(type_url: impl Into<String>, value: HashMap<String, Value>) -> Self {
        Self {
            type_url: type_url.into(),
            value,
        }
    }

    /// Create a `google.rpc.ErrorInfo` detail.
    pub fn error_info(
        reason: impl Into<String>,
        domain: impl Into<String>,
        metadata: Option<HashMap<String, String>>,
    ) -> Self {
        let mut value = HashMap::new();
        value.insert("reason".to_string(), Value::String(reason.into()));
        value.insert("domain".to_string(), Value::String(domain.into()));
        if let Some(meta) = metadata {
            let meta_obj: serde_json::Map<String, Value> = meta
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect();
            value.insert("metadata".to_string(), Value::Object(meta_obj));
        }
        Self {
            type_url: ERROR_INFO_TYPE.to_string(),
            value,
        }
    }

    /// Create a `google.rpc.BadRequest` detail with field violations.
    pub fn bad_request(field_violations: Vec<FieldViolation>) -> Self {
        let violations: Vec<Value> = field_violations
            .into_iter()
            .map(|fv| serde_json::to_value(fv).unwrap_or_default())
            .collect();
        let mut value = HashMap::new();
        value.insert("fieldViolations".to_string(), Value::Array(violations));
        Self {
            type_url: BAD_REQUEST_TYPE.to_string(),
            value,
        }
    }

    /// Create a typed detail from a struct (arbitrary map).
    pub fn from_struct(fields: HashMap<String, Value>) -> Self {
        Self {
            type_url: STRUCT_TYPE.to_string(),
            value: fields,
        }
    }
}

impl Serialize for TypedDetail {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.value.len() + 1))?;
        map.serialize_entry("@type", &self.type_url)?;
        for (k, v) in &self.value {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for TypedDetail {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut map: HashMap<String, Value> = HashMap::deserialize(deserializer)?;
        let type_url = map
            .remove("@type")
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| STRUCT_TYPE.to_string());
        Ok(Self {
            type_url,
            value: map,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_serialize_typed_detail() {
        let detail = TypedDetail::error_info("TASK_NOT_FOUND", PROTOCOL_DOMAIN, None);
        let json = serde_json::to_value(&detail).unwrap();
        assert_eq!(json["@type"], ERROR_INFO_TYPE);
        assert_eq!(json["reason"], "TASK_NOT_FOUND");
        assert_eq!(json["domain"], PROTOCOL_DOMAIN);
    }

    #[test]
    fn test_deserialize_typed_detail() {
        let json = serde_json::json!({
            "@type": ERROR_INFO_TYPE,
            "reason": "TASK_NOT_FOUND",
            "domain": PROTOCOL_DOMAIN,
            "metadata": {"taskId": "t1"}
        });
        let detail: TypedDetail = serde_json::from_value(json).unwrap();
        assert_eq!(detail.type_url, ERROR_INFO_TYPE);
        assert_eq!(detail.value["reason"], "TASK_NOT_FOUND");
        assert_eq!(detail.value["domain"], PROTOCOL_DOMAIN);
        assert_eq!(detail.value["metadata"]["taskId"], "t1");
    }

    #[test]
    fn test_deserialize_without_type_defaults_to_struct() {
        let json = serde_json::json!({"resource": "task"});
        let detail: TypedDetail = serde_json::from_value(json).unwrap();
        assert_eq!(detail.type_url, STRUCT_TYPE);
        assert_eq!(detail.value["resource"], "task");
    }

    #[test]
    fn test_round_trip() {
        let meta = HashMap::from([("taskId".to_string(), "t1".to_string())]);
        let detail = TypedDetail::error_info("TASK_NOT_FOUND", PROTOCOL_DOMAIN, Some(meta));
        let serialized = serde_json::to_value(&detail).unwrap();
        let deserialized: TypedDetail = serde_json::from_value(serialized).unwrap();
        assert_eq!(detail, deserialized);
    }

    #[test]
    fn test_bad_request() {
        let violations = vec![
            FieldViolation {
                field: "message.parts".into(),
                description: "At least one part is required".into(),
            },
            FieldViolation {
                field: "message.role".into(),
                description: "Role must be 'user' or 'agent'".into(),
            },
        ];
        let detail = TypedDetail::bad_request(violations);
        assert_eq!(detail.type_url, BAD_REQUEST_TYPE);

        let json = serde_json::to_value(&detail).unwrap();
        assert_eq!(json["@type"], BAD_REQUEST_TYPE);
        let fv = json["fieldViolations"].as_array().unwrap();
        assert_eq!(fv.len(), 2);
        assert_eq!(fv[0]["field"], "message.parts");
        assert_eq!(fv[0]["description"], "At least one part is required");
        assert_eq!(fv[1]["field"], "message.role");
    }

    #[test]
    fn test_bad_request_round_trip() {
        let violations = vec![FieldViolation {
            field: "task.id".into(),
            description: "Must not be empty".into(),
        }];
        let detail = TypedDetail::bad_request(violations);
        let serialized = serde_json::to_value(&detail).unwrap();
        let deserialized: TypedDetail = serde_json::from_value(serialized).unwrap();
        assert_eq!(detail, deserialized);
    }

    #[test]
    fn test_from_struct() {
        let mut fields = HashMap::new();
        fields.insert("key".to_string(), Value::String("val".to_string()));
        let detail = TypedDetail::from_struct(fields.clone());
        assert_eq!(detail.type_url, STRUCT_TYPE);
        assert_eq!(detail.value, fields);
    }
}
