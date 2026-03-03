use rmcp::Json;
use schemars::{json_schema, JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Serialize};

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
