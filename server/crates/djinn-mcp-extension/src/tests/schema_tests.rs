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
        | "output_view"
        | "output_grep"
        | "memory_build_context"
        | "memory_health"
        | "memory_extracted_audit"
        | "memory_broken_links"
        | "memory_orphans"
        | "agent_metrics"
        | "pr_review_context" => Some(read_only),
        "code_search" | "github_search" | "code_graph" => Some(open_world_read_only),
        "task_update"
        | "epic_update"
        | "epic_close"
        | "proposal_ac_set"
        | "proposal_debate_resolve" => Some(idempotent_mutation),
        "task_create"
        | "epic_create"
        | "task_transition"
        | "task_comment_add"
        | "memory_write"
        | "memory_edit"
        | "memory_move"
        | "request_lead"
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
        | "agent_create"
        | "agent_amend_prompt" => Some(destructive),
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
        "evidence_spike" => tool_schemas_evidence_spike(),
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
    assert!(planner.iter().any(|n| n == "agent_metrics"));
    assert!(planner.iter().any(|n| n == "agent_create"));
    assert!(planner.iter().any(|n| n == "agent_amend_prompt"));

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
    assert!(architect.iter().any(|n| n == "submit_work"));
    // Architect must NOT have code-writing tools.
    assert!(!architect.iter().any(|n| n == "write"));
    assert!(!architect.iter().any(|n| n == "edit"));
    assert!(!architect.iter().any(|n| n == "apply_patch"));
    assert!(!architect.iter().any(|n| n == "agent_amend_prompt"));

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
    // The advocate ONLY revises the spec. Resolution + rebuttal are the
    // Judge's job, and `proposal_ac_amend` spams AI feedback comments.
    assert!(
        !advocate.iter().any(|n| n == "proposal_ac_amend"),
        "advocate must NOT have proposal_ac_amend (it persists AI feedback noise)"
    );
    assert!(
        !advocate.iter().any(|n| n == "proposal_debate_resolve"),
        "advocate must NOT resolve objections — the Judge adjudicates resolution"
    );
    assert!(
        !advocate.iter().any(|n| n == "proposal_debate_append"),
        "advocate must NOT write the debate trail — it only revises the spec"
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
fn agent_amend_prompt_schema_embeds_revived_loop_contract() {
    let schema = serde_json::to_value(tool_role_amend_prompt()).expect("serialize tool schema");
    let description = schema
        .get("description")
        .and_then(|value| value.as_str())
        .expect("agent_amend_prompt has description");

    for required in [
        "Planner-owned",
        "evidence-based",
        "machine-managed learned_prompt",
        "learned_prompt_history",
        "system_prompt_extensions",
        "Only specialist worker/reviewer agents are eligible",
        "default roles",
        "metrics_snapshot when available",
    ] {
        assert!(
            description.contains(required),
            "agent_amend_prompt description should mention {required}: {description}"
        );
    }

    let agent_id_description = schema
        .pointer("/inputSchema/properties/agent_id/description")
        .and_then(|value| value.as_str())
        .expect("agent_id property has description");
    assert!(agent_id_description.contains("Specialist worker/reviewer"));
    assert!(agent_id_description.contains("defaults"));

    let metrics_description = schema
        .pointer("/inputSchema/properties/metrics_snapshot/description")
        .and_then(|value| value.as_str())
        .expect("metrics_snapshot property has description");
    assert!(metrics_description.contains("Optional JSON string"));
    assert!(metrics_description.contains("Planner should provide it when available"));
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
        "Scoped proposal-reconcile teardown",
        "blocks terminally",
        "AI proposal feedback",
        "unlinks only that epic",
        "leaves unrelated graduated epics untouched",
        "instead of whole-build proposal_stop_build",
    ] {
        assert!(
            description.contains(required),
            "proposal_reconcile_obsolete_epic description should mention {required}: {description}"
        );
    }
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
        ("evidence_spike", tool_schemas_evidence_spike()),
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
        tool_schemas_evidence_spike(),
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
        "agent_amend_prompt".to_string(),
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

// ── evidence-spike read-only schema tests ────────────────────────────────

#[test]
fn evidence_spike_schema_includes_read_only_investigation_tools() {
    let schemas = tool_schemas_evidence_spike();
    let names = tool_names(&schemas);
    let expected = [
        "read",
        "code_search",
        "code_graph",
        "github_search",
        "output_view",
        "output_grep",
        "lsp",
        "skill_read",
        "ci_job_log",
    ];
    for tool in expected {
        assert!(
            names.contains(&tool),
            "evidence_spike schema should include {tool}"
        );
    }
}

#[test]
fn evidence_spike_schema_includes_read_only_inspection_tools() {
    let schemas = tool_schemas_evidence_spike();
    let names = tool_names(&schemas);
    let expected = [
        "task_show",
        "task_list",
        "task_activity_list",
        "epic_show",
        "epic_tasks",
        "proposal_show",
        "proposal_debate_list",
        "memory_read",
        "memory_search",
        "memory_list",
        "memory_build_context",
        "memory_health",
        "memory_orphans",
        "memory_broken_links",
        "memory_extracted_audit",
    ];
    for tool in expected {
        assert!(
            names.contains(&tool),
            "evidence_spike schema should include {tool}"
        );
    }
}

#[test]
fn evidence_spike_schema_includes_submit_work_terminal() {
    let schemas = tool_schemas_evidence_spike();
    let names = tool_names(&schemas);
    assert!(
        names.contains(&"submit_work"),
        "evidence_spike schema should include submit_work as the terminal finalize tool"
    );
}

#[test]
fn evidence_spike_schema_excludes_mutation_and_destructive_tools() {
    let schemas = tool_schemas_evidence_spike();
    let names = tool_names(&schemas);
    let blocked = [
        "shell",
        "write",
        "edit",
        "apply_patch",
        "task_update",
        "task_comment_add",
        "task_transition",
        "task_create",
        "epic_create",
        "epic_update",
        "epic_close",
        "memory_write",
        "memory_edit",
        "memory_move",
        "proposal_update",
        "proposal_debate_append",
        "proposal_debate_resolve",
        "proposal_refinement_demand_evidence",
        "proposal_ac_set",
        "proposal_ac_amend",
        "proposal_complete",
        "proposal_block_patch",
        "proposal_reconcile_obsolete_epic",
        "task_delete_branch",
        "task_archive_activity",
        "task_reset_counters",
        "task_kill_session",
        "agent_create",
        "agent_amend_prompt",
        "request_lead",
        "request_planner",
    ];
    for tool in blocked {
        assert!(
            !names.contains(&tool),
            "evidence_spike schema must NOT include {tool}"
        );
    }
}

#[test]
fn evidence_spike_schema_all_tools_are_read_only_except_submit_work() {
    for schema in tool_schemas_evidence_spike() {
        let name = schema
            .get("name")
            .and_then(|n| n.as_str())
            .expect("tool schema has a name");
        if name == "submit_work" {
            assert_eq!(
                safety_tuple(&schema),
                (false, false, false, false),
                "submit_work must be classified as mutation"
            );
            continue;
        }
        let (read_only, destructive, _idempotent, _open_world) = safety_tuple(&schema);
        assert!(
            read_only,
            "{name} must be read-only in evidence_spike schema"
        );
        assert!(
            !destructive,
            "{name} must not be destructive in evidence_spike schema"
        );
    }
}

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
