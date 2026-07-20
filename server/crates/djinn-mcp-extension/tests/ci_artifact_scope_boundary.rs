//! MCP-layer scope boundary for the `ci_artifact` tool.
//!
//! These integration tests pin the public contract: the `CiArtifactParams`
//! schema rejects all out-of-scope inputs, and every role surface that
//! exposes `ci_job_log` also exposes `ci_artifact` as read-only.

use djinn_mcp_extension::tool_defs::{
    tool_schemas_adversary, tool_schemas_advocate, tool_schemas_architect, tool_schemas_judge,
    tool_schemas_lead, tool_schemas_planner, tool_schemas_reviewer, tool_schemas_worker,
};
use djinn_mcp_extension::types::{CiArtifactAction, CiArtifactParams};
use serde_json::json;

// ── helpers ────────────────────────────────────────────────────────────

fn deserialize(args: serde_json::Value) -> Result<CiArtifactParams, serde_json::Error> {
    serde_json::from_value(args)
}

fn validate_ok(args: serde_json::Value) {
    let params: CiArtifactParams = deserialize(args).expect("should deserialize");
    params.validate().expect("should validate");
}

fn deserialize_fails(args: serde_json::Value) {
    assert!(
        deserialize(args).is_err(),
        "should reject unknown/extra fields at deserialization"
    );
}

/// Collect the `(name, safety)` pairs from a tool-schema array so we can
/// check which tools are present and whether they carry the read-only
/// annotation.
fn tool_safety_pairs(schemas: &[serde_json::Value]) -> Vec<(String, bool, bool, bool, bool)> {
    schemas
        .iter()
        .filter_map(|s| {
            let name = s.get("name")?.as_str()?.to_string();
            let ro = s.get("readOnly").and_then(|v| v.as_bool()).unwrap_or(false);
            let destr = s
                .get("destructive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let idem = s
                .get("idempotent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let ow = s
                .get("openWorld")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            Some((name, ro, destr, idem, ow))
        })
        .collect()
}

fn assert_both_ci_tools_read_only(schemas: &[serde_json::Value]) {
    let pairs = tool_safety_pairs(schemas);
    let has_ci_job_log = pairs.iter().any(|(n, ..)| n == "ci_job_log");
    let has_ci_artifact = pairs.iter().any(|(n, ..)| n == "ci_artifact");
    assert!(
        has_ci_job_log,
        "surface must include ci_job_log (prerequisite for ci_artifact parity)"
    );
    assert!(
        has_ci_artifact,
        "surface must include ci_artifact (every role with ci_job_log must also have ci_artifact)"
    );
    for (name, ro, destr, _idem, ow) in &pairs {
        if name == "ci_artifact" || name == "ci_job_log" {
            assert!(*ro, "{name} must be read-only (readOnly=true)");
            assert!(
                !*destr,
                "{name} must not be destructive (destructive=false)"
            );
            assert!(!*ow, "{name} must not be open-world (openWorld=false)");
        }
    }
}

// ── named tests ────────────────────────────────────────────────────────

/// Reject every out-of-scope input and operation: unknown fields, zero
/// selectors, both selectors, artifact-on-list, missing/empty artifact
/// on fetch, and forbidden inputs (job_id, step, repository, lane, format,
/// mutation, deletion, retention).
#[test]
fn schema_rejects_out_of_scope_inputs_and_operations() {
    // ── Four legal shapes succeed ───────────────────────────────────
    // 1. list with no selector
    validate_ok(json!({"action":"list"}));
    // 2. list with run_id
    validate_ok(json!({"action":"list","run_id":42}));
    // 3. fetch with pr_number and artifact
    validate_ok(json!({"action":"fetch","pr_number":7,"artifact":"logs.txt"}));
    // 4. fetch with run_id and artifact
    validate_ok(json!({"action":"fetch","run_id":99,"artifact":"report.zip"}));

    // ── Unknown / out-of-scope fields rejected at deserialization ───
    deserialize_fails(json!({"action":"list","job_id":1}));
    deserialize_fails(json!({"action":"list","step":"Tests"}));
    deserialize_fails(json!({"action":"list","repository":"foo/bar"}));
    deserialize_fails(json!({"action":"list","lane":"pr-head"}));
    deserialize_fails(json!({"action":"list","format":"junit"}));
    deserialize_fails(json!({"action":"list","delete":true}));
    deserialize_fails(json!({"action":"list","retention":"7d"}));
    deserialize_fails(json!({"action":"list","owner":"octo"}));
    deserialize_fails(json!({"action":"list","repo":"cat"}));
    deserialize_fails(json!({"action":"list","extra":42}));

    // ── Missing required action ─────────────────────────────────────
    assert!(deserialize(json!({})).is_err());
    assert!(deserialize(json!({"run_id":1})).is_err());

    // ── Illegal action values ───────────────────────────────────────
    assert!(deserialize(json!({"action":"delete"})).is_err());
    assert!(deserialize(json!({"action":"LIST"})).is_err());
    assert!(deserialize(json!({"action":"download"})).is_err());

    // ── Zero selectors (non-positive) ───────────────────────────────
    deserialize_fails(json!({"action":"list","run_id":0}));
    deserialize_fails(json!({"action":"list","pr_number":0}));

    // ── Both selectors (mutual exclusion) ───────────────────────────
    deserialize_fails(json!({"action":"list","run_id":1,"pr_number":2}));
    deserialize_fails(json!({"action":"fetch","run_id":1,"pr_number":2,"artifact":"x"}));

    // ── artifact forbidden for list ─────────────────────────────────
    deserialize_fails(json!({"action":"list","artifact":"x"}));

    // ── artifact required and non-empty for fetch ───────────────────
    deserialize_fails(json!({"action":"fetch"}));
    deserialize_fails(json!({"action":"fetch","run_id":1}));
    deserialize_fails(json!({"action":"fetch","artifact":""}));

    // ── Enum parity check (internal type) ──────────────────────────
    let params: CiArtifactParams =
        deserialize(json!({"action":"fetch","run_id":1,"artifact":"x"})).unwrap();
    assert_eq!(params.action, CiArtifactAction::Fetch);
    let params: CiArtifactParams = deserialize(json!({"action":"list"})).unwrap();
    assert_eq!(params.action, CiArtifactAction::List);
}

/// Every role surface that includes `ci_job_log` must also include
/// `ci_artifact`, and both must be annotated read-only.
#[test]
fn role_surfaces_expose_only_read_only_ci_artifact() {
    assert_both_ci_tools_read_only(&tool_schemas_worker());
    assert_both_ci_tools_read_only(&tool_schemas_reviewer());
    assert_both_ci_tools_read_only(&tool_schemas_lead());
    assert_both_ci_tools_read_only(&tool_schemas_planner());
    assert_both_ci_tools_read_only(&tool_schemas_architect());
    assert_both_ci_tools_read_only(&tool_schemas_advocate());
    assert_both_ci_tools_read_only(&tool_schemas_adversary());
    assert_both_ci_tools_read_only(&tool_schemas_judge());
}
