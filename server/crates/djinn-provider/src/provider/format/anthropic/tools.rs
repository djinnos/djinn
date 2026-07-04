use serde_json::{Value, json};

use crate::provider::format::tool_projection;
use crate::provider::{FormatFamily, ToolSchemaCompat};

/// Convert an RMCP-shaped tool value into the Anthropic tool shape and run the
/// converted `input_schema` through the shared `tool_projection` core.
///
/// # RMCP → Anthropic field conversion
///
/// RMCP: `{"name", "description", "inputSchema"}`
/// Anthropic: `{"name", "description", "input_schema"}`
///
/// Rebuilt clean either way: a stray camelCase `inputSchema` means no
/// `input_schema` reaches the API, which strict Anthropic-compatible vendors
/// reject (MiniMax error 2013 "function name or parameters is empty").
///
/// # Schema projection
///
/// After the field-shape conversion, the `input_schema` is passed through
/// [`tool_projection::project`] using `compat` and [`FormatFamily::Anthropic`].
/// When `compat` is `None` the projection is identity — native Anthropic
/// providers receive the schema verbatim. When `compat` is
/// `Some(ToolSchemaCompat::Moonshot)` (used for Kimi/Moonshot/MiniMax
/// Anthropic-compatible endpoints) the shared Moonshot rewrites strip
/// `$ref` siblings, collapse `prefixItems` into `items`, and remove
/// `unevaluatedItems`. The Anthropic formatter itself never infers vendor
/// behavior — it just forwards `config.tool_schema_compat` to the projection
/// core delivered by epic ms1t.
pub(crate) fn convert_tool(tool: &Value, compat: Option<ToolSchemaCompat>) -> Value {
    let input_schema = tool
        .get("input_schema")
        .or_else(|| tool.get("inputSchema"))
        .cloned()
        .unwrap_or(json!({"type": "object"}));

    let projected_schema = tool_projection::project(input_schema, compat, FormatFamily::Anthropic);

    json!({
        "name": tool.get("name").cloned().unwrap_or(json!("")),
        "description": tool.get("description").cloned().unwrap_or(json!("")),
        "input_schema": projected_schema,
    })
}
