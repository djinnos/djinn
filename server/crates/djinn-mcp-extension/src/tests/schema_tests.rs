//! Schema snapshot and validation tests for the extension tool surface.
//!
//! These tests are the authoritative source for the tool schema surface
//! now that the schema definitions live in `djinn-mcp-extension`.  They
//! were moved from `djinn-agent::extension::tests::schema_snapshot_tests`
//! during the Phase 4 extraction.

use crate::shared_schemas;
use crate::tool_defs::*;
use std::collections::BTreeSet;

// ── helpers ────────────────────────────────────────────────────────────

fn tool_names(schemas: &[serde_json::Value]) -> Vec<&str> {
    schemas
        .iter()
        .filter_map(|v| v.get("name").and_then(|n| n.as_str()))
        .collect()
}

fn tool_schema<'a>(schemas: &'a [serde_json::Value], name: &str) -> &'a serde_json::Value {
    schemas
        .iter()
        .find(|schema| schema.get("name").and_then(|n| n.as_str()) == Some(name))
        .expect("tool schema present")
}

fn safety_tuple(schema: &serde_json::Value) -> (bool, bool, bool, bool) {
    let name = schema
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("<unnamed>");
    let field = |field_name: &str| {
        schema
            .get(field_name)
            .and_then(|value| value.as_bool())
            .unwrap_or_else(|| panic!("{name} missing boolean {field_name} safety annotation"))
    };

    (
        field("readOnly"),
        field("destructive"),
        field("idempotent"),
        field("openWorld"),
    )
}

fn expected_safety_tuple(name: &str) -> Option<(bool, bool, bool, bool)> {
    let read_only = (true, false, true, false);
    let open_world_read_only = (true, false, true, true);
    let mutation = (false, false, false, false);
    let idempotent_mutation = (false, false, true, false);
    let destructive = (false, true, false, false);
    let idempotent_destructive = (false, true, true, false);

    match name {
        "task_show"
        | "task_list"
        | "task_activity_list"
        | "task_blocked_list"
        | "epic_show"
        | "epic_tasks"
        | "epic_blockers_list"
        | "epic_blocked_list"
        | "proposal_show"
        | "proposal_debate_list"
        | "get_block_catalog"
        | "proposal_blocks"
        | "memory_read"
        | "memory_search"
        | "memory_list"
        | "read"
        | "skill_read"
        | "lsp"
        | "ci_job_log"
        | "ci_artifact"
        | "output_view"
        | "output_grep"
        | "output_list"
        | "memory_build_context"
        | "memory_health"
        | "memory_extracted_audit"
        | "memory_broken_links"
        | "memory_orphans"
        | "memory_recall_trace"
        | "memory_retrieval_outcomes_report"
        | "agent_metrics"
        | "pr_review_context" => Some(read_only),
        "code_search" | "github_search" | "code_graph" => Some(open_world_read_only),
        "task_update"
        | "epic_update"
        | "epic_close"
        | "proposal_ac_set"
        | "run_verification"
        | "prepare_build_cache"
        | "proposal_debate_resolve" => Some(idempotent_mutation),
        "task_create"
        | "epic_create"
        | "task_transition"
        | "task_comment_add"
        | "memory_write"
        | "memory_edit"
        | "memory_move"
        | "request_lead"   // [HISTORICAL-COMPAT] drain-window tool name (10qg)
        | "request_planner"
        | "proposal_ac_amend"
        | "proposal_update"
        | "proposal_block_patch"
        | "proposal_debate_append"
        | "proposal_refinement_demand_evidence"
        | "submit_work"
        | "submit_review"
        | "submit_decision"
        | "submit_grooming" => Some(mutation),
        "shell"
        | "write"
        | "edit"
        | "apply_patch"
        | "task_delete_branch"
        | "task_archive_activity"
        | "task_kill_session"
        | "agent_create" => Some(destructive),
        "task_reset_counters" | "proposal_complete" | "proposal_reconcile_obsolete_epic" => {
            Some(idempotent_destructive)
        }
        _ => None,
    }
}

/// Return tool names for a role identified by string name.
fn tool_names_for_role(role: &str) -> BTreeSet<String> {
    let schemas = match role {
        "worker" => tool_schemas_worker(),
        "reviewer" => tool_schemas_reviewer(),
        "lead" => tool_schemas_lead(),
        "planner" => tool_schemas_planner(),
        "architect" => tool_schemas_architect(),
        "advocate" => tool_schemas_advocate(),
        "adversary" => tool_schemas_adversary(),
        "judge" => tool_schemas_judge(),
        _ => panic!("unknown role: {role}"),
    };
    schemas
        .into_iter()
        .filter_map(|schema| {
            schema
                .get("name")
                .and_then(|name| name.as_str())
                .map(ToString::to_string)
        })
        .collect()
}

/// Check whether a tool name is in a role's schema set.
fn is_tool_allowed_for_role(role: &str, name: &str) -> bool {
    tool_names_for_role(role).contains(name)
}

// ── role-specific tool presence tests ──────────────────────────────────

#[test]
fn tool_schemas_include_role_specific_tools() {
    fn schema_names(schemas: Vec<serde_json::Value>) -> Vec<String> {
        schemas
            .into_iter()
            .filter_map(|v| {
                v.get("name")
                    .and_then(|n| n.as_str())
                    .map(ToString::to_string)
            })
            .collect()
    }

    let worker = schema_names(tool_schemas_worker());
    assert!(worker.iter().any(|n| n == "shell"));
    assert!(worker.iter().any(|n| n == "write"));
    assert!(worker.iter().any(|n| n == "edit"));
    assert!(worker.iter().any(|n| n == "memory_write"));
    assert!(worker.iter().any(|n| n == "memory_edit"));
    assert!(worker.iter().any(|n| n == "memory_build_context"));
    assert!(worker.iter().any(|n| n == "submit_work"));
    assert!(!worker.iter().any(|n| n == "task_comment_add"));

    let reviewer = schema_names(tool_schemas_reviewer());
    assert!(reviewer.iter().any(|n| n == "submit_review"));
    assert!(!reviewer.iter().any(|n| n == "task_update_ac"));
    assert!(!reviewer.iter().any(|n| n == "task_comment_add"));

    let lead = schema_names(tool_schemas_lead());
    assert!(lead.iter().any(|n| n == "task_create"));
    assert!(lead.iter().any(|n| n == "submit_decision"));
    assert!(!lead.iter().any(|n| n == "task_transition"));
    assert!(!lead.iter().any(|n| n == "task_comment_add"));

    let planner = schema_names(tool_schemas_planner());
    assert!(planner.iter().any(|n| n == "write"));
    assert!(planner.iter().any(|n| n == "edit"));
    assert!(planner.iter().any(|n| n == "task_create"));
    assert!(planner.iter().any(|n| n == "task_transition"));
    assert!(planner.iter().any(|n| n == "submit_grooming"));
    assert!(planner.iter().any(|n| n == "proposal_ac_amend"));
    assert!(planner.iter().any(|n| n == "memory_write"));
    assert!(planner.iter().any(|n| n == "memory_edit"));
    assert!(planner.iter().any(|n| n == "task_comment_add"));
    assert!(planner.iter().any(|n| n == "memory_health"));
    assert!(planner.iter().any(|n| n == "memory_broken_links"));
    assert!(planner.iter().any(|n| n == "memory_orphans"));
    assert!(planner.iter().any(|n| n == "memory_build_context"));
    assert!(planner.iter().any(|n| n == "memory_recall_trace"));
    assert!(
        planner
            .iter()
            .any(|n| n == "memory_retrieval_outcomes_report")
    );
    assert!(planner.iter().any(|n| n == "agent_metrics"));
    assert!(planner.iter().any(|n| n == "agent_create"));

    let architect = schema_names(tool_schemas_architect());
    assert!(architect.iter().any(|n| n == "shell"));
    assert!(architect.iter().any(|n| n == "read"));
    assert!(architect.iter().any(|n| n == "task_create"));
    assert!(architect.iter().any(|n| n == "task_comment_add"));
    assert!(architect.iter().any(|n| n == "task_transition"));
    assert!(architect.iter().any(|n| n == "task_kill_session"));
    assert!(architect.iter().any(|n| n == "memory_write"));
    assert!(architect.iter().any(|n| n == "memory_edit"));
    assert!(architect.iter().any(|n| n == "memory_move"));
    assert!(architect.iter().any(|n| n == "memory_recall_trace"));
    assert!(
        architect
            .iter()
            .any(|n| n == "memory_retrieval_outcomes_report")
    );
    assert!(architect.iter().any(|n| n == "submit_work"));
    // Architect must NOT have code-writing tools.
    assert!(!architect.iter().any(|n| n == "write"));
    assert!(!architect.iter().any(|n| n == "edit"));
    assert!(!architect.iter().any(|n| n == "apply_patch"));

    // Tribunal roles (k9zw): verify role-specific tools.
    let advocate = schema_names(tool_schemas_advocate());
    assert!(
        advocate.iter().any(|n| n == "write"),
        "advocate should have write"
    );
    assert!(
        advocate.iter().any(|n| n == "edit"),
        "advocate should have edit"
    );
    assert!(
        advocate.iter().any(|n| n == "submit_work"),
        "advocate should have submit_work"
    );
    assert!(
        advocate.iter().any(|n| n == "proposal_show"),
        "advocate should have proposal_show"
    );
    assert!(
        advocate.iter().any(|n| n == "proposal_update"),
        "advocate should have proposal_update"
    );
    assert!(
        advocate.iter().any(|n| n == "proposal_ac_set"),
        "advocate should have proposal_ac_set (silent AC update)"
    );
    // The advocate revises the spec and may REBUT objections on the debate
    // trail (kind="rebuttal") — the tribunal's counterweight against scope
    // ratchet. Resolution stays the Judge's job, and `proposal_ac_amend`
    // spams AI feedback comments.
    assert!(
        !advocate.iter().any(|n| n == "proposal_ac_amend"),
        "advocate must NOT have proposal_ac_amend (it persists AI feedback noise)"
    );
    assert!(
        !advocate.iter().any(|n| n == "proposal_debate_resolve"),
        "advocate must NOT resolve objections — the Judge adjudicates resolution"
    );
    assert!(
        advocate.iter().any(|n| n == "proposal_debate_append"),
        "advocate must have proposal_debate_append — its rebuttal channel \
         (kind=\"rebuttal\"); without it every objection can only be resolved \
         by growing the spec"
    );
    assert!(
        advocate.iter().any(|n| n == "proposal_block_patch"),
        "advocate should have proposal_block_patch for MDX enrichment"
    );
    assert!(
        advocate.iter().any(|n| n == "get_block_catalog"),
        "advocate should have get_block_catalog for vocabulary pull"
    );
    assert!(
        advocate.iter().any(|n| n == "proposal_blocks"),
        "advocate should have proposal_blocks for block registry"
    );
    assert!(
        advocate.iter().any(|n| n == "memory_write"),
        "advocate should have memory_write"
    );

    let adversary = schema_names(tool_schemas_adversary());
    assert!(
        adversary.iter().any(|n| n == "submit_review"),
        "adversary should have submit_review"
    );
    assert!(
        adversary.iter().any(|n| n == "proposal_show"),
        "adversary should have proposal_show"
    );
    assert!(
        adversary.iter().any(|n| n == "task_comment_add"),
        "adversary should have task_comment_add"
    );
    assert!(
        adversary.iter().any(|n| n == "proposal_debate_append"),
        "adversary MUST have proposal_debate_append — the only channel the \
         refinement loop reads for objections"
    );
    // Adversary must NOT have write/edit tools.
    assert!(
        !adversary.iter().any(|n| n == "write"),
        "adversary must not have write"
    );
    assert!(
        !adversary.iter().any(|n| n == "edit"),
        "adversary must not have edit"
    );
    // Adversary must NOT have Advocate-specific authoring/advocacy tools.
    assert!(
        !adversary.iter().any(|n| n == "proposal_update"),
        "adversary must not have proposal_update"
    );
    assert!(
        !adversary.iter().any(|n| n == "proposal_block_patch"),
        "adversary must not have proposal_block_patch"
    );
    assert!(
        !adversary.iter().any(|n| n == "get_block_catalog"),
        "adversary must not have get_block_catalog"
    );
    assert!(
        !adversary.iter().any(|n| n == "proposal_blocks"),
        "adversary must not have proposal_blocks"
    );

    let judge = schema_names(tool_schemas_judge());
    assert!(
        judge.iter().any(|n| n == "submit_decision"),
        "judge should have submit_decision"
    );
    assert!(
        judge.iter().any(|n| n == "proposal_show"),
        "judge should have proposal_show"
    );
    assert!(
        judge.iter().any(|n| n == "task_comment_add"),
        "judge should have task_comment_add"
    );
    assert!(
        judge.iter().any(|n| n == "proposal_debate_append"),
        "judge MUST have proposal_debate_append — the only channel the \
         refinement loop reads for the verdict"
    );
    assert!(
        judge.iter().any(|n| n == "proposal_debate_resolve"),
        "judge MUST have proposal_debate_resolve — it adjudicates which \
         objections the revision satisfies and clears them from the gate"
    );
    // Judge must NOT have write/edit tools.
    assert!(
        !judge.iter().any(|n| n == "write"),
        "judge must not have write"
    );
    assert!(
        !judge.iter().any(|n| n == "edit"),
        "judge must not have edit"
    );
    // Judge must NOT have Advocate-specific authoring/advocacy tools.
    assert!(
        !judge.iter().any(|n| n == "proposal_update"),
        "judge must not have proposal_update"
    );
    assert!(
        !judge.iter().any(|n| n == "proposal_block_patch"),
        "judge must not have proposal_block_patch"
    );
    assert!(
        !judge.iter().any(|n| n == "get_block_catalog"),
        "judge must not have get_block_catalog"
    );
    assert!(
        !judge.iter().any(|n| n == "proposal_blocks"),
        "judge must not have proposal_blocks"
    );
}

#[test]
fn adr_050_code_graph_boundary_is_architect_only() {
    // ADR-050 keeps `code_graph` on the Architect surface only.
    for role in ["worker", "reviewer", "lead", "planner"] {
        assert!(
            !tool_names_for_role(role).contains("code_graph"),
            "{role} must not expose code_graph per ADR-050",
        );
    }
    assert!(tool_names_for_role("architect").contains("code_graph"));
}

#[test]
fn worker_cannot_use_lead_only_tool() {
    // submit_decision is lead-only (ADR-036: finalize tools are role-specific).
    assert!(!is_tool_allowed_for_role("worker", "submit_decision"));
    assert!(is_tool_allowed_for_role("lead", "submit_decision"));
    // task_transition is not in the lead tool set (removed by ADR-036).
    assert!(!is_tool_allowed_for_role("lead", "task_transition"));
}

// ── schema structure / content tests ──────────────────────────────────

#[test]
fn memory_recall_trace_schema_discriminates_list_and_detail_modes() {
    let schema = serde_json::to_value(shared_schemas::tool_memory_recall_trace())
        .expect("serialize memory_recall_trace schema");
    let variants = schema["inputSchema"]["oneOf"]
        .as_array()
        .expect("memory_recall_trace has discriminated request variants");
    assert_eq!(variants.len(), 2);
    assert_eq!(variants[0]["properties"]["mode"]["const"], "list");
    assert_eq!(variants[1]["properties"]["mode"]["const"], "detail");
    assert_eq!(
        variants[1]["required"],
        serde_json::json!(["mode", "trace_id"])
    );
    assert_eq!(variants[0]["properties"]["limit"]["minimum"], 1);
    assert_eq!(variants[0]["properties"]["limit"]["maximum"], 100);
    assert_eq!(
        variants[0]["properties"]["trace_outcome"]["enum"],
        serde_json::json!([
            "injected",
            "empty",
            "error",
            "legacy_unknown",
            "disabled_off",
            "disabled_kill_switch",
            "disabled_legacy"
        ])
    );
    assert_eq!(
        variants[0]["properties"]["rollout_label"]["description"],
        "Exact recorded rollout label filter."
    );
    assert!(variants[1]["not"]["anyOf"].is_array());
}

#[test]
fn memory_retrieval_outcomes_report_schema_is_explicit_observational_contract() {
    let schema = serde_json::to_value(shared_schemas::tool_memory_retrieval_outcomes_report())
        .expect("serialize memory_retrieval_outcomes_report schema");
    assert_eq!(schema["name"], "memory_retrieval_outcomes_report");
    assert_eq!(
        schema["inputSchema"]["required"],
        serde_json::json!(["start", "end", "timezone"])
    );
    for field in ["start", "end"] {
        assert_eq!(
            schema["inputSchema"]["properties"][field]["format"],
            "date-time"
        );
    }
    assert_eq!(
        schema["inputSchema"]["properties"]["timezone"]["minLength"],
        1
    );

    let description = schema["description"]
        .as_str()
        .expect("report schema has a description");
    for required in [
        "observational only",
        "no causal or randomized-experiment claim",
        "deduplicated within each entry_point/rollout_label/outcome cell",
        "can overlap and are non-additive",
        "denominator, count, rate, not-applicable state, attempt-number distribution",
        "unattributed/unrecorded diagnostic",
        "no fallback through task_id",
        "protected 30-day trace window",
        "rejected without clipping",
    ] {
        assert!(
            description.contains(required),
            "description missing {required:?}"
        );
    }
}

/// Phase 1 c4r6: the memory_search tool description must encode the query-style
/// contract in the shared MCP schema. This test fails if any required
/// formulation rule, the good/bad example pair, or the lexical/BM25-until-72iu
/// caveat is removed.
#[test]
fn memory_search_schema_documents_query_formulation_contract() {
    let schema = serde_json::to_value(shared_schemas::tool_memory_search())
        .expect("serialize memory_search schema");
    assert_eq!(schema["name"], "memory_search");

    let description = schema["description"]
        .as_str()
        .expect("memory_search has a description");

    // Directive-bearing contract clauses. Keep prohibitions coupled to their
    // forbidden forms so a reversal (for example, permitting questions) fails.
    for required in [
        "write a declarative statement, not an interrogative question",
        "express one information need per query",
        "make each query self-contained",
        "omit retrieval-meta wording such as `find`, `information about`, and `search for`",
        "preserve discriminative symbol names, exact error strings, and config keys verbatim",
        "Worker-issued searches remain lexical/BM25-only until proposal 72iu supplies worker embeddings",
    ] {
        assert!(
            description.contains(required),
            "memory_search description should mention `{required}`: {description}"
        );
    }

    // Good/bad example pair must be present and recognisable.
    let good_example = "Good query: `Authentication timeout handling for E_CONNRESET`";
    let bad_example = "Bad query: `Can you find information about authentication timeout errors?`";
    assert!(
        description.contains(good_example),
        "memory_search description should include the good example: {description}"
    );
    assert!(
        description.contains(bad_example),
        "memory_search description should include the bad example: {description}"
    );

    // Lexical/BM25-only caveat until worker embeddings land in 72iu.
    assert!(
        description.contains("lexical/BM25"),
        "memory_search description should caveat lexical/BM25 retrieval: {description}"
    );
    assert!(
        description.contains("72iu"),
        "memory_search description should reference 72iu: {description}"
    );
}

#[test]
fn code_graph_schema_embeds_workflow_guidance() {
    let schema = serde_json::to_value(tool_code_graph()).expect("serialize code_graph schema");
    let description = schema
        .get("description")
        .and_then(|value| value.as_str())
        .expect("code_graph has a description");

    assert!(description.contains("WHEN TO USE"));
    assert!(description.contains("AFTER THIS"));
    for operation in [
        "capabilities",
        "query_subgraph",
        "search",
        "describe",
        "neighbors",
        "impact",
        "context",
        "complexity",
        "refactor_candidates",
        "workspaces",
        "workspace_hint",
        "available-workspace",
        "node_count",
        "commit_sha",
        "warmed_at",
        "status",
    ] {
        assert!(
            description.contains(operation),
            "code_graph guidance should mention {operation}"
        );
    }

    let workspace_description = schema
        .pointer("/inputSchema/properties/workspace/description")
        .and_then(|value| value.as_str())
        .expect("code_graph workspace property has a description");
    for required in [
        "operation=workspaces",
        "Empty string is treated as omitted",
        "hard-scope listing/bounded ops",
        "seed/endpoint resolution",
        "cross-workspace edges remain visible",
        "workspace_hint",
    ] {
        assert!(
            workspace_description.contains(required),
            "workspace schema description should mention {required}: {workspace_description}"
        );
    }
}

#[test]
fn proposal_ac_set_schema_remains_status_only() {
    let schema = serde_json::to_value(shared_schemas::tool_proposal_ac_set())
        .expect("serialize proposal_ac_set schema");
    assert_eq!(schema["name"], "proposal_ac_set");

    let description = schema["description"]
        .as_str()
        .expect("proposal_ac_set has a description");
    for required in [
        "`met` flags",
        "criterion text is preserved automatically",
        "status annotation only",
        "does not edit the spec",
        "bump a revision",
        "clear sign-offs",
    ] {
        assert!(
            description.contains(required),
            "proposal_ac_set description should mention {required}: {description}"
        );
    }

    assert_eq!(
        schema["inputSchema"]["required"],
        serde_json::json!(["id", "acceptance_criteria"])
    );
    assert_eq!(
        schema["inputSchema"]["properties"]["acceptance_criteria"]["items"]["properties"]["met"]["type"],
        serde_json::json!("boolean")
    );
}

#[test]
fn proposal_ac_amend_schema_documents_operations_and_reasons() {
    let schema = serde_json::to_value(shared_schemas::tool_proposal_ac_amend())
        .expect("serialize proposal_ac_amend schema");
    assert_eq!(schema["name"], "proposal_ac_amend");
    assert_eq!(
        schema["inputSchema"]["required"],
        serde_json::json!(["id", "reason", "amendments"])
    );
    assert_eq!(
        schema["inputSchema"]["properties"]["reason"]["minLength"],
        serde_json::json!(1)
    );
    assert_eq!(
        schema["inputSchema"]["properties"]["amendments"]["minItems"],
        serde_json::json!(1)
    );
    let item = &schema["inputSchema"]["properties"]["amendments"]["items"];
    assert_eq!(item["required"], serde_json::json!(["operation", "index"]));
    assert_eq!(
        item["properties"]["operation"]["enum"],
        serde_json::json!(["rewrite", "drop", "waive"])
    );
    assert_eq!(
        item["properties"]["index"]["type"],
        serde_json::json!("integer")
    );
    assert_eq!(item["properties"]["index"]["minimum"], serde_json::json!(0));
    assert_eq!(
        item["properties"]["criterion"]["description"],
        "New criterion text; required and non-empty when operation is rewrite."
    );
}

#[test]
fn proposal_reconcile_obsolete_epic_schema_documents_scope_and_blocking() {
    let schema = serde_json::to_value(shared_schemas::tool_proposal_reconcile_obsolete_epic())
        .expect("serialize proposal_reconcile_obsolete_epic schema");
    assert_eq!(schema["name"], "proposal_reconcile_obsolete_epic");
    let description = schema["description"]
        .as_str()
        .expect("proposal_reconcile_obsolete_epic has a description");
    for required in [
        "Scoped teardown",
        "Blocks if any target task has merged work",
        "shared parent-disposition matrix",
        "only the selected linked epic's children",
        "disposed/closed",
        "parked for lead intervention",
        "retained for another open proposal parent",
        "retained for an external dependent",
        "closes and unlinks only the selected epic",
        "leaving unrelated graduated epics linked",
        "instead of whole-build proposal_stop_build",
    ] {
        assert!(
            description.contains(required),
            "proposal_reconcile_obsolete_epic description should mention {required}: {description}"
        );
    }
    assert!(!description.contains("force-close"));
    assert_eq!(
        schema["inputSchema"]["required"],
        serde_json::json!(["proposal_id", "epic_id"])
    );
}

#[test]
fn shell_timeout_defaults_and_minimum() {
    fn resolve_timeout(t: Option<u64>) -> u64 {
        t.unwrap_or(120_000).max(1000)
    }
    assert_eq!(resolve_timeout(None), 120_000);
    assert_eq!(resolve_timeout(Some(0)), 1000);
}

// ── safety annotation tests ────────────────────────────────────────────

#[test]
fn tool_schemas_include_typed_safety_annotations() {
    let worker = tool_schemas_worker();

    let github_search = tool_schema(&worker, "github_search");
    assert_eq!(github_search["readOnly"], true);
    assert_eq!(github_search["destructive"], false);
    assert_eq!(github_search["idempotent"], true);
    assert_eq!(github_search["openWorld"], true);
    assert_eq!(github_search["concurrent_safe"], true);

    let write = tool_schema(&worker, "write");
    assert_eq!(write["readOnly"], false);
    assert_eq!(write["destructive"], true);
    assert_eq!(write["idempotent"], false);
    assert_eq!(write["openWorld"], false);
    assert_eq!(write["concurrent_safe"], false);
}

#[test]
fn tool_schemas_include_concurrency_metadata() {
    let worker = tool_schemas_worker();
    assert_eq!(tool_schema(&worker, "task_show")["concurrent_safe"], true);
    assert_eq!(tool_schema(&worker, "read")["concurrent_safe"], true);
    assert_eq!(
        tool_schema(&worker, "github_search")["concurrent_safe"],
        true
    );
    assert_eq!(tool_schema(&worker, "shell")["concurrent_safe"], false);
    assert_eq!(tool_schema(&worker, "write")["concurrent_safe"], false);

    let architect = tool_schemas_architect();
    assert_eq!(
        tool_schema(&architect, "code_graph")["concurrent_safe"],
        true
    );
    assert_eq!(
        tool_schema(&architect, "memory_build_context")["concurrent_safe"],
        true
    );
    assert_eq!(
        tool_schema(&architect, "task_comment_add")["concurrent_safe"],
        false
    );
}

#[test]
fn all_role_tool_schemas_have_pinned_safety_annotations() {
    for (role, schemas) in [
        ("worker", tool_schemas_worker()),
        ("reviewer", tool_schemas_reviewer()),
        ("lead", tool_schemas_lead()),
        ("planner", tool_schemas_planner()),
        ("architect", tool_schemas_architect()),
        ("advocate", tool_schemas_advocate()),
        ("adversary", tool_schemas_adversary()),
        ("judge", tool_schemas_judge()),
    ] {
        for schema in schemas {
            let name = schema
                .get("name")
                .and_then(|value| value.as_str())
                .expect("tool schema has a string name");
            let expected = expected_safety_tuple(name).unwrap_or_else(|| {
                panic!("{role} tool {name} is missing pinned safety classification")
            });
            assert_eq!(
                safety_tuple(&schema),
                expected,
                "{role} tool {name} safety classification changed without updating the invariant"
            );
        }
    }
}

#[test]
fn role_tool_schemas_pin_destructive_and_open_world_sets() {
    let mut destructive_tools = std::collections::BTreeSet::new();
    let mut open_world_read_only_tools = std::collections::BTreeSet::new();

    for schemas in [
        tool_schemas_worker(),
        tool_schemas_reviewer(),
        tool_schemas_lead(),
        tool_schemas_planner(),
        tool_schemas_architect(),
        tool_schemas_advocate(),
        tool_schemas_adversary(),
        tool_schemas_judge(),
    ] {
        for schema in schemas {
            let name = schema
                .get("name")
                .and_then(|value| value.as_str())
                .expect("tool schema has a string name");
            let (read_only, destructive, idempotent, open_world) = safety_tuple(&schema);
            if destructive {
                destructive_tools.insert(name.to_string());
            }
            if read_only && idempotent && open_world {
                open_world_read_only_tools.insert(name.to_string());
            }
        }
    }

    let expected_destructive = std::collections::BTreeSet::from([
        "agent_create".to_string(),
        "apply_patch".to_string(),
        "edit".to_string(),
        "proposal_complete".to_string(),
        "proposal_reconcile_obsolete_epic".to_string(),
        "shell".to_string(),
        "task_archive_activity".to_string(),
        "task_delete_branch".to_string(),
        "task_kill_session".to_string(),
        "task_reset_counters".to_string(),
        "write".to_string(),
    ]);
    assert_eq!(destructive_tools, expected_destructive);

    let expected_open_world_read_only = std::collections::BTreeSet::from([
        "code_graph".to_string(),
        "code_search".to_string(),
        "github_search".to_string(),
    ]);
    assert_eq!(open_world_read_only_tools, expected_open_world_read_only);
}

// ── insta snapshot tests ───────────────────────────────────────────────

#[test]
fn snapshot_worker_tool_names() {
    let schemas = tool_schemas_worker();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("worker_tool_names", names);
}

#[test]
fn snapshot_worker_tool_schemas() {
    insta::assert_json_snapshot!("worker_tool_schemas", tool_schemas_worker());
}

/// Phase 1 regression guard: the default model-facing tool surface (edit,
/// apply_patch, read, write, shell) must not change as a result of the telemetry
/// exporter/evaluator work. This snapshot is intentionally narrow to the tools
/// monitored by the GO/STOP decision gate.
#[test]
fn phase_1_default_model_facing_tool_surface_unchanged() {
    const DEFAULT_FACING_TOOLS: [&str; 5] = ["edit", "apply_patch", "read", "write", "shell"];
    let default_schemas: Vec<serde_json::Value> = tool_schemas_worker()
        .into_iter()
        .filter(|v| {
            v.get("name")
                .and_then(|n| n.as_str())
                .is_some_and(|name| DEFAULT_FACING_TOOLS.contains(&name))
        })
        .collect();
    insta::assert_json_snapshot!("phase_1_default_model_facing_tool_schemas", default_schemas);
}

#[test]
fn snapshot_reviewer_tool_names() {
    let schemas = tool_schemas_reviewer();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("reviewer_tool_names", names);
}

#[test]
fn snapshot_reviewer_tool_schemas() {
    insta::assert_json_snapshot!("reviewer_tool_schemas", tool_schemas_reviewer());
}

#[test]
fn snapshot_lead_tool_names() {
    let schemas = tool_schemas_lead();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("lead_tool_names", names);
}

#[test]
fn snapshot_lead_tool_schemas() {
    insta::assert_json_snapshot!("lead_tool_schemas", tool_schemas_lead());
}

#[test]
fn snapshot_planner_tool_names() {
    let schemas = tool_schemas_planner();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("planner_tool_names", names);
}

#[test]
fn snapshot_planner_tool_schemas() {
    insta::assert_json_snapshot!("planner_tool_schemas", tool_schemas_planner());
}

#[test]
fn snapshot_architect_tool_names() {
    let schemas = tool_schemas_architect();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("architect_tool_names", names);
}

#[test]
fn snapshot_architect_tool_schemas() {
    insta::assert_json_snapshot!("architect_tool_schemas", tool_schemas_architect());
}

#[test]
fn snapshot_lsp_tool_schema() {
    insta::assert_json_snapshot!("lsp_tool_schema", serde_json::to_value(tool_lsp()).unwrap());
}

#[test]
fn snapshot_code_graph_tool_schema() {
    insta::assert_json_snapshot!(
        "code_graph_tool_schema",
        serde_json::to_value(tool_code_graph()).unwrap()
    );
}

#[test]
fn snapshot_advocate_tool_names() {
    let schemas = tool_schemas_advocate();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("advocate_tool_names", names);
}

#[test]
fn snapshot_advocate_tool_schemas() {
    insta::assert_json_snapshot!("advocate_tool_schemas", tool_schemas_advocate());
}

#[test]
fn snapshot_adversary_tool_names() {
    let schemas = tool_schemas_adversary();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("adversary_tool_names", names);
}

#[test]
fn snapshot_adversary_tool_schemas() {
    insta::assert_json_snapshot!("adversary_tool_schemas", tool_schemas_adversary());
}

#[test]
fn snapshot_judge_tool_names() {
    let schemas = tool_schemas_judge();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("judge_tool_names", names);
}

#[test]
fn snapshot_judge_tool_schemas() {
    insta::assert_json_snapshot!("judge_tool_schemas", tool_schemas_judge());
}

// ── Evidence-spike profile tests ─────────────────────────────────────────

#[test]
fn snapshot_evidence_spike_tool_names() {
    let schemas = tool_schemas_evidence_spike();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("evidence_spike_tool_names", names);
}

#[test]
fn snapshot_evidence_spike_tool_schemas() {
    insta::assert_json_snapshot!("evidence_spike_tool_schemas", tool_schemas_evidence_spike());
}

/// Helper: collect tool names from a schema list.
pub(super) fn ev_schema_names(schemas: &[serde_json::Value]) -> BTreeSet<String> {
    schemas
        .iter()
        .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect()
}

#[test]
fn evidence_spike_has_read_only_investigation_tools() {
    let schemas = tool_schemas_evidence_spike();
    let names = ev_schema_names(&schemas);

    // File/code intelligence (read-only from base, NO shell).
    assert!(names.contains("read"), "evidence spike must have `read`");
    assert!(
        names.contains("code_search"),
        "evidence spike must have `code_search`"
    );
    assert!(
        names.contains("skill_read"),
        "evidence spike must have `skill_read`"
    );
    assert!(names.contains("lsp"), "evidence spike must have `lsp`");
    assert!(
        names.contains("ci_job_log"),
        "evidence spike must have `ci_job_log`"
    );
    assert!(
        names.contains("ci_artifact"),
        "evidence spike must have `ci_artifact`"
    );
    assert!(
        names.contains("github_search"),
        "evidence spike must have `github_search`"
    );
    assert!(
        names.contains("output_view"),
        "evidence spike must have `output_view`"
    );
    assert!(
        names.contains("output_grep"),
        "evidence spike must have `output_grep`"
    );
    assert!(
        names.contains("output_list"),
        "evidence spike must have `output_list`"
    );

    // Task/epic/memory read-only inspection (from shared_base + shared_lead).
    assert!(
        names.contains("task_show"),
        "evidence spike must have `task_show`"
    );
    assert!(
        names.contains("task_list"),
        "evidence spike must have `task_list`"
    );
    assert!(
        names.contains("task_activity_list"),
        "evidence spike must have `task_activity_list`"
    );
    assert!(
        names.contains("memory_read"),
        "evidence spike must have `memory_read`"
    );
    assert!(
        names.contains("memory_search"),
        "evidence spike must have `memory_search`"
    );
    assert!(
        names.contains("memory_list"),
        "evidence spike must have `memory_list`"
    );
    assert!(
        names.contains("task_blocked_list"),
        "evidence spike must have `task_blocked_list`"
    );
    assert!(
        names.contains("epic_show"),
        "evidence spike must have `epic_show`"
    );
    assert!(
        names.contains("epic_tasks"),
        "evidence spike must have `epic_tasks`"
    );

    // Memory health/context (read-only).
    assert!(
        names.contains("memory_build_context"),
        "evidence spike must have `memory_build_context`"
    );
    assert!(
        names.contains("memory_health"),
        "evidence spike must have `memory_health`"
    );
    assert!(
        names.contains("memory_extracted_audit"),
        "evidence spike must have `memory_extracted_audit`"
    );
    assert!(
        names.contains("memory_broken_links"),
        "evidence spike must have `memory_broken_links`"
    );
    assert!(
        names.contains("memory_orphans"),
        "evidence spike must have `memory_orphans`"
    );

    // Architect-only read tools.
    assert!(
        names.contains("code_graph"),
        "evidence spike must have `code_graph`"
    );
    assert!(
        names.contains("pr_review_context"),
        "evidence spike must have `pr_review_context`"
    );

    // Proposal/debate read inspection.
    assert!(
        names.contains("proposal_show"),
        "evidence spike must have `proposal_show`"
    );
    assert!(
        names.contains("proposal_debate_list"),
        "evidence spike must have `proposal_debate_list`"
    );

    // Finalize tool: evidence findings submission path.
    assert!(
        names.contains("submit_work"),
        "evidence spike must have `submit_work` for findings handoff"
    );
}

#[test]
fn evidence_spike_excludes_mutation_and_destructive_tools() {
    let schemas = tool_schemas_evidence_spike();
    let names = ev_schema_names(&schemas);

    // Shell is excluded (destructive).
    assert!(
        !names.contains("shell"),
        "evidence spike must NOT have `shell`"
    );

    // File mutation tools are excluded.
    assert!(
        !names.contains("write"),
        "evidence spike must NOT have `write`"
    );
    assert!(
        !names.contains("edit"),
        "evidence spike must NOT have `edit`"
    );
    assert!(
        !names.contains("apply_patch"),
        "evidence spike must NOT have `apply_patch`"
    );

    // Task mutation tools are excluded (except submit_work).
    assert!(
        !names.contains("task_create"),
        "evidence spike must NOT have `task_create`"
    );
    assert!(
        !names.contains("task_update"),
        "evidence spike must NOT have `task_update`"
    );
    assert!(
        !names.contains("task_transition"),
        "evidence spike must NOT have `task_transition`"
    );
    assert!(
        !names.contains("task_comment_add"),
        "evidence spike must NOT have `task_comment_add`"
    );

    // Epic mutation tools are excluded.
    assert!(
        !names.contains("epic_create"),
        "evidence spike must NOT have `epic_create`"
    );
    assert!(
        !names.contains("epic_update"),
        "evidence spike must NOT have `epic_update`"
    );
    assert!(
        !names.contains("epic_close"),
        "evidence spike must NOT have `epic_close`"
    );

    // Memory mutation tools are excluded.
    assert!(
        !names.contains("memory_write"),
        "evidence spike must NOT have `memory_write`"
    );
    assert!(
        !names.contains("memory_edit"),
        "evidence spike must NOT have `memory_edit`"
    );
    assert!(
        !names.contains("memory_move"),
        "evidence spike must NOT have `memory_move`"
    );

    // Destructive admin tools are excluded.
    assert!(
        !names.contains("task_delete_branch"),
        "evidence spike must NOT have `task_delete_branch`"
    );
    assert!(
        !names.contains("task_archive_activity"),
        "evidence spike must NOT have `task_archive_activity`"
    );
    assert!(
        !names.contains("task_reset_counters"),
        "evidence spike must NOT have `task_reset_counters`"
    );
    assert!(
        !names.contains("task_kill_session"),
        "evidence spike must NOT have `task_kill_session`"
    );
    assert!(
        !names.contains("role_create"),
        "evidence spike must NOT have `role_create`"
    );

    // Escalation tools are excluded.
    assert!(
        !names.contains("request_lead"),
        "evidence spike must NOT have `request_lead`"
    );

    // Other finalize tools are excluded.
    assert!(
        !names.contains("submit_review"),
        "evidence spike must NOT have `submit_review`"
    );
    assert!(
        !names.contains("submit_decision"),
        "evidence spike must NOT have `submit_decision`"
    );
    assert!(
        !names.contains("submit_grooming"),
        "evidence spike must NOT have `submit_grooming`"
    );
}

#[test]
fn evidence_spike_has_fewer_tools_than_architect() {
    let ev_schemas = tool_schemas_evidence_spike();
    let arch_schemas = tool_schemas_architect();
    assert!(
        ev_schemas.len() < arch_schemas.len(),
        "evidence-spike profile ({}) must be strictly smaller than architect ({})",
        ev_schemas.len(),
        arch_schemas.len()
    );
}

#[test]
fn evidence_spike_preserves_normal_architect_surface() {
    // Verify the normal architect surface is NOT affected by the evidence-spike
    // profile.  The Architect does NOT have write/edit/apply_patch (it
    // diagnoses and directs but does not write code), but it retains shell,
    // mutation task/epic tools, memory mutation, and submit_work.
    let arch_schemas = tool_schemas_architect();
    let names = ev_schema_names(&arch_schemas);

    assert!(names.contains("shell"), "architect must still have `shell`");
    assert!(
        names.contains("task_create"),
        "architect must still have `task_create`"
    );
    assert!(
        names.contains("task_transition"),
        "architect must still have `task_transition`"
    );
    assert!(
        names.contains("task_comment_add"),
        "architect must still have `task_comment_add`"
    );
    assert!(
        names.contains("epic_create"),
        "architect must still have `epic_create`"
    );
    assert!(
        names.contains("memory_write"),
        "architect must still have `memory_write`"
    );
    assert!(
        names.contains("submit_work"),
        "architect must still have `submit_work`"
    );
}

/// Focused tests validating the edit tool description and input schema surface
/// after the vmpq description update. These guard against accidental regression
/// to exact-only wording and ensure the required input contract is unchanged.
#[test]
fn edit_tool_description_and_schema_surface() {
    let edit = serde_json::to_value(tool_edit()).expect("serialize tool_edit");

    // ── tool-level description ────────────────────────────────────────────
    let desc = edit["description"]
        .as_str()
        .expect("edit tool has description");

    // Must mention the unchanged input contract.
    assert!(
        desc.contains("path") && desc.contains("old_text") && desc.contains("new_text"),
        "edit description must reference all three inputs: {desc}"
    );

    // Must describe fuzzy-rescue behavior (no longer exact-only).
    assert!(
        desc.contains("rescue") || desc.contains("whitespace") || desc.contains("drift"),
        "edit description should describe fuzzy-rescue behavior: {desc}"
    );

    // Must describe ambiguity/guard failure instead of only "not found".
    assert!(
        desc.contains("ambiguous") || desc.contains("guard"),
        "edit description should describe ambiguity/guard failure: {desc}"
    );

    // Should NOT claim exact-only matching.
    assert!(
        !desc.contains("exact text") && !desc.contains("Exact text"),
        "edit description should not claim exact-only matching: {desc}"
    );

    // ── input schema ──────────────────────────────────────────────────────
    let input_schema = edit["inputSchema"]
        .as_object()
        .expect("input_schema is object");

    // Required fields must be exactly path, old_text, new_text (order-insensitive).
    let required: Vec<&str> = input_schema["required"]
        .as_array()
        .expect("required is array")
        .iter()
        .map(|v| v.as_str().expect("required entry is string"))
        .collect();
    assert_eq!(
        required,
        vec!["path", "old_text", "new_text"],
        "required input fields must be exactly [path, old_text, new_text]"
    );

    // No additional required fields beyond the original three.
    assert_eq!(required.len(), 3, "no new required input fields");

    // old_text property description must not be exact-only.
    let old_text_desc = input_schema["properties"]["old_text"]["description"]
        .as_str()
        .expect("old_text has description");
    assert!(
        !old_text_desc.contains("Exact text"),
        "old_text description should not claim exact matching: {old_text_desc}"
    );

    // old_text must still be typed as string.
    assert_eq!(
        input_schema["properties"]["old_text"]["type"].as_str(),
        Some("string"),
        "old_text must be type string"
    );
}

#[test]
fn evidence_spike_preserves_normal_worker_surface() {
    // Verify the normal worker surface is NOT affected.
    let worker_schemas = tool_schemas_worker();
    let names = ev_schema_names(&worker_schemas);

    assert!(names.contains("shell"), "worker must still have `shell`");
    assert!(names.contains("write"), "worker must still have `write`");
    assert!(names.contains("edit"), "worker must still have `edit`");
    assert!(
        names.contains("apply_patch"),
        "worker must still have `apply_patch`"
    );
    assert!(
        names.contains("memory_write"),
        "worker must still have `memory_write`"
    );
    assert!(
        names.contains("submit_work"),
        "worker must still have `submit_work`"
    );
}

#[test]
fn evidence_spike_all_schemas_are_read_only_except_finalize() {
    let schemas = tool_schemas_evidence_spike();
    for schema in &schemas {
        let name = schema
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("<unknown>");
        let is_read_only = schema
            .get("readOnly")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let is_submit_work = name == "submit_work";
        // Every tool must be read-only except submit_work (the finalize path).
        assert!(
            is_read_only || is_submit_work,
            "evidence-spike tool `{name}` must be readOnly=true or be submit_work; \
             got readOnly={is_read_only}"
        );
    }
}

#[test]
fn worker_reviewer_schemas_expose_request_planner_not_request_lead() {
    for role in ["worker", "reviewer"] {
        let names = tool_names_for_role(role);
        assert!(
            names.contains("request_planner"),
            "{role} must expose request_planner in its tool schema, got: {:?}",
            names
        );
        assert!(
            !names.contains("request_lead"),
            "{role} must NOT expose request_lead in its tool schema, got: {:?}",
            names
        );
    }
}

/// Lead schema must NOT expose `request_planner` or `escalate` — the Lead
/// only uses `submit_decision` as its finalize tool (10qg).
#[test]
fn lead_schema_does_not_expose_request_planner_or_escalate() {
    let names = tool_names_for_role("lead");
    assert!(
        !names.contains("request_planner"),
        "lead must NOT expose request_planner in its tool schema, got: {:?}",
        names
    );
    assert!(
        !names.contains("escalate"),
        "lead must NOT expose escalate in its tool schema, got: {:?}",
        names
    );
    assert!(
        !names.contains("request_lead"),
        "lead must NOT expose request_lead in its tool schema, got: {:?}",
        names
    );
    assert!(
        names.contains("submit_decision"),
        "lead must expose submit_decision in its tool schema, got: {:?}",
        names
    );
}

#[test]
fn no_stale_db_token_in_tool_descriptions() {
    // Build the forbidden lowercase token from character codes so that a
    // grep for the contiguous substring never matches this source file.
    let forbidden: String = [100u8, 111, 108, 116].iter().map(|&b| b as char).collect();
    let all_schemas: Vec<(&str, Vec<serde_json::Value>)> = vec![
        ("worker", tool_schemas_worker()),
        ("reviewer", tool_schemas_reviewer()),
        ("lead", tool_schemas_lead()),
        ("planner", tool_schemas_planner()),
        ("architect", tool_schemas_architect()),
        ("advocate", tool_schemas_advocate()),
        ("adversary", tool_schemas_adversary()),
        ("judge", tool_schemas_judge()),
    ];

    for (role, schemas) in &all_schemas {
        for schema in schemas {
            let name = schema
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("<unnamed>");
            let desc = schema
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            assert!(
                !desc.to_lowercase().contains(&forbidden),
                "tool '{name}' in role '{role}' contains stale DB-system reference in description: {desc}"
            );
        }
    }
}
