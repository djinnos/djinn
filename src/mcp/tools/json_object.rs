use rmcp::Json;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

/// A JSON object (`Map<String, Value>`) with a valid JSON Schema.
///
/// serde_json::Value generates `true` as its schema, which strict validators
/// (like Claude Code's MCP client) reject. This wrapper emits
/// `{"type": "object", "additionalProperties": true}` instead.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObjectJson(pub serde_json::Map<String, serde_json::Value>);

impl JsonSchema for ObjectJson {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "ObjectJson".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "type": "object",
            "additionalProperties": true
        })
    }
}

impl From<serde_json::Value> for ObjectJson {
    fn from(value: serde_json::Value) -> Self {
        match value {
            serde_json::Value::Object(map) => Self(map),
            other => {
                let mut map = serde_json::Map::new();
                map.insert("value".to_string(), other);
                Self(map)
            }
        }
    }
}

pub fn json_object(value: serde_json::Value) -> Json<ObjectJson> {
    Json(ObjectJson::from(value))
}

/// Any JSON value with a valid JSON Schema.
///
/// Bare `serde_json::Value` generates `true` as its schema — a catch-all that
/// strict MCP clients reject. This wrapper emits `{}` (the empty schema, which
/// also accepts any value) so the tool list passes validation.
///
/// Use this in typed response/param structs wherever the field holds arbitrary
/// JSON (e.g. `Vec<AnyJson>`, `Option<AnyJson>`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AnyJson(pub serde_json::Value);

impl JsonSchema for AnyJson {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "AnyJson".into()
    }

    fn json_schema(_generator: &mut SchemaGenerator) -> Schema {
        // Empty schema = accepts any JSON value, but is a valid schema object
        // (unlike bare `true` which schemars emits for serde_json::Value).
        json_schema!({})
    }
}

impl From<serde_json::Value> for AnyJson {
    fn from(value: serde_json::Value) -> Self {
        Self(value)
    }
}
