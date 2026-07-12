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
use std::collections::BTreeMap;
use std::collections::HashMap;

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
}
