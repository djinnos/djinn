//! Production-backed MCP tool-surface inventory and deterministic canonicalizer.
//!
//! This module derives the unique advertised tool union from the active
//! role/session aggregation functions in [`crate::tool_defs`], not from a
//! handwritten registry.  The canonical output is the review contract for
//! Djinn-owned agent-facing MCP tools.
//!
//! # Included surface
//!
//! The union is produced by calling every active schema aggregator:
//!
//! - [`tool_schemas_worker`](crate::tool_defs::tool_schemas_worker)
//! - [`tool_schemas_reviewer`](crate::tool_defs::tool_schemas_reviewer)
//! - [`tool_schemas_lead`](crate::tool_defs::tool_schemas_lead)
//! - [`tool_schemas_planner`](crate::tool_defs::tool_schemas_planner)
//! - [`tool_schemas_architect`](crate::tool_defs::tool_schemas_architect)
//! - [`tool_schemas_advocate`](crate::tool_defs::tool_schemas_advocate)
//! - [`tool_schemas_adversary`](crate::tool_defs::tool_schemas_adversary)
//! - [`tool_schemas_judge`](crate::tool_defs::tool_schemas_judge)
//! - [`tool_schemas_evidence_spike`](crate::tool_defs::tool_schemas_evidence_spike)
//!
//! Only the fields actually emitted by production are retained: `name`,
//! `description`, `inputSchema`, and the safety annotations injected by
//! [`crate::shared_schemas::annotate_tool_safety`].
//!
//! # Excluded callable compatibility definition
//!
//! [`tool_request_lead`](crate::tool_defs::tool_request_lead) is a
//! [HISTORICAL-COMPAT] callable definition retained for the drain window after
//! epic 10qg cut-over.  No active role/session aggregator advertises it, so it
//! is intentionally omitted from the canonical inventory.  Stale sessions
//! dispatched before the cut-over may still call it through the compatibility
//! handler, but it is not part of the advertised review contract.
//!
//! # Deterministic normalization rules
//!
//! 1. JSON object keys are sorted recursively (lexicographically).
//! 2. The top-level array is sorted by tool `name`.
//! 3. Optional fields present as `null` are dropped so absent and null are
//!    equivalent.
//! 4. Output is pretty-printed with `serde_json::to_string_pretty` and exactly
//!    one trailing newline.

use serde_json::Value;
use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet};

use crate::tool_defs::{
    tool_schemas_adversary, tool_schemas_advocate, tool_schemas_architect,
    tool_schemas_evidence_spike, tool_schemas_judge, tool_schemas_lead, tool_schemas_planner,
    tool_schemas_reviewer, tool_schemas_worker,
};

/// Error returned when the canonical tool surface contains a conflict.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("tool surface conflict for `{name}`: {message}")]
pub struct ToolSurfaceConflict {
    pub name: String,
    pub message: String,
}

/// A reviewed exception for a description mention that is not an operation.
/// Every exception needs an explicit approval rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OperationDescriptionAllowlistEntry {
    pub value: &'static str,
    pub approved_rationale: &'static str,
}

/// Reviewed operation-description exceptions. This intentionally starts empty:
/// adding an exception is a contract decision, not a parser escape hatch.
pub const OPERATION_DESCRIPTION_ALLOWLIST: &[OperationDescriptionAllowlistEntry] = &[];

/// A description-promised operation absent from a matching schema enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationEnumDiagnostic {
    pub tool_name: String,
    /// Dot-separated JSON-schema path rooted at `inputSchema`.
    pub field_path: String,
    pub missing_mentioned_value: String,
    /// At most three values ordered by edit distance, then bytewise name.
    pub nearest_enum_values: Vec<String>,
}

impl std::fmt::Display for OperationEnumDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "tool `{}` field `{}` mentions operation `{}` absent from enum; nearest enum values: [{}]",
            self.tool_name,
            self.field_path,
            self.missing_mentioned_value,
            self.nearest_enum_values.join(", ")
        )
    }
}

/// Check all recursive string `operation` properties with enum arrays against
/// narrowly extracted promises in the owning tool's description.
///
/// Extraction accepts Markdown code spans in an operation context and clear
/// slash/comma lists near operation language (or substantially overlapping the
/// enum). It deliberately does not tokenize ordinary natural-language prose.
pub fn validate_operation_description_enums(tools: &[Value]) -> Vec<OperationEnumDiagnostic> {
    let mut diagnostics = Vec::new();
    for tool in tools {
        let Some(object) = tool.as_object() else {
            continue;
        };
        let (Some(name), Some(description), Some(schema)) = (
            object.get("name").and_then(Value::as_str),
            object.get("description").and_then(Value::as_str),
            object.get("inputSchema"),
        ) else {
            continue;
        };
        collect_operation_diagnostics(schema, "inputSchema", name, description, &mut diagnostics);
    }
    diagnostics.sort_by(|left, right| {
        (
            &left.tool_name,
            &left.field_path,
            &left.missing_mentioned_value,
        )
            .cmp(&(
                &right.tool_name,
                &right.field_path,
                &right.missing_mentioned_value,
            ))
    });
    diagnostics
}

fn collect_operation_diagnostics(
    schema: &Value,
    path: &str,
    tool_name: &str,
    description: &str,
    diagnostics: &mut Vec<OperationEnumDiagnostic>,
) {
    let Some(object) = schema.as_object() else {
        return;
    };
    if let Some(properties) = object.get("properties").and_then(Value::as_object) {
        if let Some(operation_schema) = properties.get("operation") {
            if let Some(enum_values) = string_enum(operation_schema) {
                let enum_set: BTreeSet<&str> = enum_values.iter().map(String::as_str).collect();
                for candidate in extract_operation_candidates(description, &enum_set) {
                    if !enum_set.contains(candidate.as_str()) && !is_allowlisted(&candidate) {
                        diagnostics.push(OperationEnumDiagnostic {
                            tool_name: tool_name.to_string(),
                            field_path: format!("{path}.properties.operation"),
                            nearest_enum_values: nearest_enum_values(&candidate, &enum_values),
                            missing_mentioned_value: candidate,
                        });
                    }
                }
            }
        }
        for (property_name, property_schema) in properties {
            collect_operation_diagnostics(
                property_schema,
                &format!("{path}.properties.{property_name}"),
                tool_name,
                description,
                diagnostics,
            );
        }
    }
    for (key, value) in object {
        if key == "properties" {
            continue;
        }
        match value {
            Value::Object(_) => collect_operation_diagnostics(
                value,
                &format!("{path}.{key}"),
                tool_name,
                description,
                diagnostics,
            ),
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    if value.is_object() {
                        collect_operation_diagnostics(
                            value,
                            &format!("{path}.{key}[{index}]"),
                            tool_name,
                            description,
                            diagnostics,
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

fn string_enum(schema: &Value) -> Option<Vec<String>> {
    let object = schema.as_object()?;
    if object.get("type").and_then(Value::as_str) != Some("string") {
        return None;
    }
    object
        .get("enum")?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn is_allowlisted(candidate: &str) -> bool {
    OPERATION_DESCRIPTION_ALLOWLIST
        .iter()
        .any(|entry| entry.value == candidate)
}

fn extract_operation_candidates(
    description: &str,
    enum_values: &BTreeSet<&str>,
) -> BTreeSet<String> {
    let mut candidates = BTreeSet::new();
    let mut remainder = description;
    let mut consumed = 0;
    while let Some(start) = remainder.find('`') {
        let after_start = &remainder[start + 1..];
        let Some(end) = after_start.find('`') else {
            break;
        };
        let candidate = &after_start[..end];
        let operation_context = description[..consumed + start]
            .chars()
            .rev()
            .take(48)
            .collect::<String>()
            .to_ascii_lowercase()
            .contains("operation");
        if is_operation_token(candidate) && (operation_context || enum_values.contains(candidate)) {
            candidates.insert(candidate.to_string());
        }
        consumed += start + end + 2;
        remainder = &after_start[end + 1..];
    }

    for list in description.split(|character: char| matches!(character, ';' | '.' | '\n')) {
        let mut values = list_values(list);
        if values.len() < 2 {
            continue;
        }
        values.retain(|value| value != "operation" && value != "operations");
        let lower_list = list.to_ascii_lowercase();
        let operation_context = lower_list.contains("operation:")
            || lower_list.contains("operations:")
            || lower_list.contains("operation=");
        let enum_overlap = values
            .iter()
            .filter(|value| enum_values.contains(value.as_str()))
            .count();
        if operation_context || enum_overlap >= 2 {
            candidates.extend(values)
        }
    }
    candidates
}

fn list_values(text: &str) -> Vec<String> {
    let mut values = Vec::new();
    // Slash catalogues are intentionally restricted to one whitespace-delimited
    // run, so prose after `ranked/cycles/... = ...` is never treated as values.
    for run in text.split_whitespace().filter(|run| run.contains('/')) {
        values.extend(run.split('/').filter_map(clean_operation_token));
    }

    // Comma catalogues require an explicit operation label. This accepts the
    // conventional `operations: foo, bar` form without promoting a prose comma.
    let lower = text.to_ascii_lowercase();
    let marker = ["operations:", "operation:", "operation="]
        .into_iter()
        .find_map(|marker| lower.find(marker).map(|index| index + marker.len()));
    if let Some(marker) = marker {
        values.extend(text[marker..].split(',').filter_map(clean_operation_token));
    }
    values
}

fn clean_operation_token(value: &str) -> Option<String> {
    let token = value.trim_matches(|character: char| {
        !character.is_ascii_alphanumeric() && character != '_' && character != '-'
    });
    is_operation_token(token).then(|| token.to_string())
}

fn is_operation_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn nearest_enum_values(candidate: &str, enum_values: &[String]) -> Vec<String> {
    let mut values = enum_values.to_vec();
    values.sort_by(|left, right| {
        levenshtein(candidate, left)
            .cmp(&levenshtein(candidate, right))
            .then_with(|| left.cmp(right))
    });
    values.into_iter().take(3).collect()
}

fn levenshtein(left: &str, right: &str) -> usize {
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    for (left_index, left_byte) in left.bytes().enumerate() {
        let mut current = vec![left_index + 1];
        for (right_index, right_byte) in right.bytes().enumerate() {
            current.push(
                (previous[right_index + 1] + 1)
                    .min(current[right_index] + 1)
                    .min(previous[right_index] + usize::from(left_byte != right_byte)),
            );
        }
        previous = current;
    }
    previous[right.len()]
}

/// Collect the unique advertised tool union from all active role/session
/// aggregators.
///
/// Returns `Err` if the same tool name is advertised by two aggregators with
/// incompatible schemas (after canonical projection).  This is a programming
/// error: every active role should agree on the public schema for a shared tool.
///
/// The returned array is sorted by tool name, but the function does not perform
/// the recursive JSON key sort.  Use [`canonicalize_tool_surface_json`] for
/// the final review contract.
pub fn collect_tool_surface_union() -> Result<Vec<Value>, ToolSurfaceConflict> {
    let mut by_name: HashMap<String, Value> = HashMap::new();
    let report_conflict = |name: String, first: Value, second: Value| {
        Err(ToolSurfaceConflict {
            name: name.clone(),
            message: format!(
                "same name advertised with different schemas; first={first}, second={second}"
            ),
        })
    };

    for schema in all_active_schemas() {
        let name = schema
            .get("name")
            .and_then(|n| n.as_str())
            .map(String::from)
            .unwrap_or_else(|| "<unnamed>".to_string());

        let projected = project_tool_schema(schema);

        if let Some(existing) = by_name.get(&name) {
            if *existing != projected {
                return report_conflict(name, existing.clone(), projected);
            }
        } else {
            by_name.insert(name, projected);
        }
    }

    let mut tools: Vec<Value> = by_name.into_values().collect();
    tools.sort_by(|a, b| {
        let a_name = a.get("name").and_then(|n| n.as_str()).unwrap_or("");
        let b_name = b.get("name").and_then(|n| n.as_str()).unwrap_or("");
        a_name.cmp(b_name)
    });
    Ok(tools)
}

/// Return the canonical pretty-printed JSON for the advertised tool surface.
///
/// The output is deterministic: recursive key sorting, tools sorted by name,
/// null optional fields normalized away, and exactly one trailing newline.
///
/// This function is the shared generation path used by the fixture regression
/// test and the regeneration binary.
pub fn canonicalize_tool_surface_json() -> Result<String, ToolSurfaceConflict> {
    let tools = collect_tool_surface_union()?;
    let mut sorted: Vec<Value> = Vec::with_capacity(tools.len());
    for tool in tools {
        sorted.push(recursively_sort_and_normalize(tool));
    }
    let value = Value::Array(sorted);
    let mut output = serde_json::to_string_pretty(&value).expect("serialize tool surface");
    if !output.ends_with('\n') {
        output.push('\n');
    }
    Ok(output)
}

/// Return the tool surface as a single JSON array string.
///
/// This is the primary public entry point for tests and consumers that need the
/// canonical review contract.
pub fn tool_surface_baseline_json() -> String {
    canonicalize_tool_surface_json()
        .expect("tool surface should have no duplicate-name conflicts in production")
}

/// Project a serialized tool schema to the canonical review contract subset.
///
/// Retained fields: `name`, `description`, `inputSchema`, and the injected
/// safety annotations.  Everything else is dropped.
fn project_tool_schema(value: Value) -> Value {
    let mut projected = serde_json::Map::new();
    if let Some(obj) = value.as_object() {
        for key in [
            "name",
            "description",
            "inputSchema",
            "readOnly",
            "destructive",
            "idempotent",
            "openWorld",
            "concurrent_safe",
        ] {
            if let Some(v) = obj.get(key) {
                projected.insert(key.to_string(), v.clone());
            }
        }
    }
    Value::Object(projected)
}

/// Recursively sort JSON object keys and drop `null` optional values.
fn recursively_sort_and_normalize(value: Value) -> Value {
    match value {
        Value::Object(obj) => {
            let mut sorted = BTreeMap::new();
            for (k, v) in obj.into_iter() {
                if !v.is_null() {
                    sorted.insert(k, recursively_sort_and_normalize(v));
                }
            }
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(arr) => arr
            .into_iter()
            .map(recursively_sort_and_normalize)
            .collect(),
        other => other,
    }
}

fn all_active_schemas() -> Vec<Value> {
    let mut all = Vec::new();
    all.extend(tool_schemas_worker());
    all.extend(tool_schemas_reviewer());
    all.extend(tool_schemas_lead());
    all.extend(tool_schemas_planner());
    all.extend(tool_schemas_architect());
    all.extend(tool_schemas_advocate());
    all.extend(tool_schemas_adversary());
    all.extend(tool_schemas_judge());
    all.extend(tool_schemas_evidence_spike());
    all
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonicalization_recursively_sorts_object_keys() {
        let input = json!({
            "z": 1,
            "a": {
                "c": 2,
                "b": 3
            }
        });
        let output = recursively_sort_and_normalize(input);
        let text = serde_json::to_string(&output).unwrap();
        assert_eq!(text, r#"{"a":{"b":3,"c":2},"z":1}"#);
    }

    #[test]
    fn canonicalization_sorts_tools_by_name() {
        let tools = collect_tool_surface_union().expect("collect union");
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").and_then(|n| n.as_str()).unwrap_or(""))
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "tools must be sorted by name");
    }

    #[test]
    fn canonicalization_normalizes_null_optional_fields() {
        let input = json!({
            "name": "x",
            "description": "desc",
            "inputSchema": {"type": "object"},
            "unused": null,
            "nested": {"keep": 1, "drop": null}
        });
        let output = recursively_sort_and_normalize(input);
        assert!(output.get("unused").is_none());
        let nested = output.get("nested").unwrap().as_object().unwrap();
        assert!(nested.contains_key("keep"));
        assert!(!nested.contains_key("drop"));
    }

    #[test]
    fn canonicalization_does_not_drop_null_inside_arrays() {
        let input = json!({"arr": [null, 1]});
        let output = recursively_sort_and_normalize(input);
        let arr = output.get("arr").unwrap().as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert!(arr[0].is_null());
    }

    #[test]
    fn conflict_detection_fails_on_duplicate_name_different_schema() {
        let first = json!({
            "name": "dup",
            "description": "first",
            "inputSchema": {"type": "object"}
        });
        let second = json!({
            "name": "dup",
            "description": "second",
            "inputSchema": {"type": "object"}
        });
        let err = simulate_conflict(first, second);
        assert!(err.is_err());
        let err = err.unwrap_err();
        assert_eq!(err.name, "dup");
        assert!(err.message.contains("different schemas"));
    }

    fn simulate_conflict(first: Value, second: Value) -> Result<(), ToolSurfaceConflict> {
        let mut by_name: HashMap<String, Value> = HashMap::new();
        let name = "dup".to_string();
        let p1 = project_tool_schema(first);
        let p2 = project_tool_schema(second);
        by_name.insert(name.clone(), p1);
        // force the second to conflict by checking against the inserted content
        if let Some(existing) = by_name.get(&name) {
            if *existing != p2 {
                return Err(ToolSurfaceConflict {
                    name: name.clone(),
                    message: format!(
                        "same name advertised with different schemas; first={existing}, second={p2}"
                    ),
                });
            }
        }
        Ok(())
    }

    #[test]
    fn repeated_generation_is_byte_identical() {
        let first = tool_surface_baseline_json();
        let second = tool_surface_baseline_json();
        assert_eq!(first, second, "canonical output must be byte-identical");
        assert!(first.ends_with('\n'));
        assert_eq!(first.len(), second.len());
    }

    #[test]
    fn output_contains_only_canonical_fields() {
        let baseline = tool_surface_baseline_json();
        let tools: Vec<Value> = serde_json::from_str(&baseline).unwrap();
        assert!(!tools.is_empty(), "tool surface must not be empty");
        for tool in tools {
            let obj = tool.as_object().expect("tool is an object");
            let allowed: std::collections::BTreeSet<String> = [
                "name",
                "description",
                "inputSchema",
                "readOnly",
                "destructive",
                "idempotent",
                "openWorld",
                "concurrent_safe",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            for key in obj.keys() {
                assert!(
                    allowed.contains(key),
                    "unexpected field {key} in canonical tool surface"
                );
            }
            assert!(obj.contains_key("name"));
            assert!(obj.contains_key("description"));
            assert!(obj.contains_key("inputSchema"));
            assert!(obj.contains_key("readOnly"));
            assert!(obj.contains_key("destructive"));
            assert!(obj.contains_key("idempotent"));
            assert!(obj.contains_key("openWorld"));
            assert!(obj.contains_key("concurrent_safe"));
        }
    }

    #[test]
    fn request_lead_is_not_in_canonical_surface() {
        let tools = collect_tool_surface_union().expect("collect union");
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").and_then(|n| n.as_str()).unwrap_or(""))
            .collect();
        assert!(
            !names.contains(&"request_lead"),
            "request_lead is a historical-compat definition and must not be advertised"
        );
    }

    #[test]
    fn evidence_spike_tools_are_present_in_union() {
        let tools = collect_tool_surface_union().expect("collect union");
        let names: std::collections::BTreeSet<String> = tools
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        assert!(names.contains("code_graph"));
        assert!(names.contains("submit_work"));
        assert!(names.contains("task_show"));
    }

    fn operation_tool(description: &str, input_schema: Value) -> Value {
        json!({
            "name": "example_tool",
            "description": description,
            "inputSchema": input_schema,
        })
    }

    #[test]
    fn operation_guard_accepts_complete_slash_catalogue() {
        let tool = operation_tool(
            "ranked/cycles/orphans/path/edges",
            json!({"type": "object", "properties": {
                "operation": {"type": "string", "enum": ["ranked", "cycles", "orphans", "path", "edges"]}
            }}),
        );
        assert!(validate_operation_description_enums(&[tool]).is_empty());
    }

    #[test]
    fn operation_guard_reports_a7qo_style_catalogue_mismatch() {
        let tool = operation_tool(
            "api_surface/dead_symbols/deprecated_callers/route_map/shape_check/api_impact/flow",
            json!({"type": "object", "properties": {
                "operation": {"type": "string", "enum": ["api_surface", "dead_symbols", "deprecated_callers"]}
            }}),
        );
        let diagnostics = validate_operation_description_enums(&[tool]);
        let missing: Vec<&str> = diagnostics
            .iter()
            .map(|diagnostic| diagnostic.missing_mentioned_value.as_str())
            .collect();
        assert_eq!(missing, ["api_impact", "flow", "route_map", "shape_check"]);
        assert!(diagnostics.iter().all(|diagnostic| {
            diagnostic.tool_name == "example_tool"
                && diagnostic.field_path == "inputSchema.properties.operation"
                && diagnostic.nearest_enum_values.len() == 3
        }));
        assert!(diagnostics[2].to_string().contains("route_map"));
    }

    #[test]
    fn operation_guard_recurses_and_preserves_candidate_case() {
        let tool = operation_tool(
            "Select operation `Run`.",
            json!({"type": "object", "properties": {
                "config": {"type": "array", "items": {"type": "object", "properties": {
                    "operation": {"type": "string", "enum": ["run", "stop"]}
                }}}
            }}),
        );
        let diagnostics = validate_operation_description_enums(&[tool]);
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].missing_mentioned_value, "Run");
        assert_eq!(
            diagnostics[0].field_path,
            "inputSchema.properties.config.items.properties.operation"
        );
        assert_eq!(diagnostics[0].nearest_enum_values, ["run", "stop"]);
    }

    #[test]
    fn operation_guard_ignores_ordinary_prose() {
        let tool = operation_tool(
            "This operation can route requests through a flow, but users choose it naturally.",
            json!({"type": "object", "properties": {
                "operation": {"type": "string", "enum": ["start", "stop"]}
            }}),
        );
        assert!(validate_operation_description_enums(&[tool]).is_empty());
    }
}
