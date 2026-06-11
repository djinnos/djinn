use serde_json::{Value, json};

pub(crate) fn convert_tool(tool: &Value) -> Value {
    // Convert RMCP tool format to the Anthropic tool shape.
    // RMCP: {"name", "description", "inputSchema"}
    // Anthropic: {"name", "description", "input_schema"}
    // Rebuilt clean either way: a stray camelCase `inputSchema`
    // means no `input_schema` reaches the API, which strict
    // Anthropic-compatible vendors reject (MiniMax error 2013
    // "function name or parameters is empty").
    let input_schema = tool
        .get("input_schema")
        .or_else(|| tool.get("inputSchema"))
        .cloned()
        .unwrap_or(json!({"type": "object"}));
    json!({
        "name": tool.get("name").cloned().unwrap_or(json!("")),
        "description": tool.get("description").cloned().unwrap_or(json!("")),
        "input_schema": input_schema,
    })
}
