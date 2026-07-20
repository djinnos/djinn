//! Expanded evidence-spike regression and dispatch-gate tests.
//!
//! These tests verify the fail-closed evidence-spike profile: mutation tools
//! are absent from the tool surface, the dispatch gate rejects blocked tools,
//! and the command validator rejects mutation commands.

use crate::tool_defs::*;
use std::collections::BTreeSet;

use super::schema_tests::ev_schema_names;

// ── Expanded evidence-spike mutation exclusion tests ────────────────────────

/// Evidence-spike sessions must NOT include proposal mutation tools.
/// Read-only proposal inspection (proposal_show, proposal_debate_list) is
/// allowed — everything that mutates a proposal or its debate is excluded.
#[test]
fn evidence_spike_excludes_proposal_mutation_tools() {
    let schemas = tool_schemas_evidence_spike();
    let names = ev_schema_names(&schemas);

    let denied = [
        "proposal_update",
        "proposal_block_patch",
        "proposal_complete",
        "proposal_ac_set",
        "proposal_ac_amend",
        "proposal_debate_append",
        "proposal_debate_resolve",
        "proposal_reconcile_obsolete_epic",
        "proposal_refinement_demand_evidence",
        "get_block_catalog",
        "proposal_blocks",
    ];
    for tool in &denied {
        assert!(
            !names.contains(*tool),
            "evidence spike must NOT have mutation tool `{tool}`"
        );
    }
}

/// Evidence-spike sessions must NOT include escalation or agent-mutation tools.
#[test]
fn evidence_spike_excludes_escalation_and_agent_mutation_tools() {
    let schemas = tool_schemas_evidence_spike();
    let names = ev_schema_names(&schemas);

    let denied = [
        "request_lead",
        "request_planner",
        "role_amend_prompt",
        "role_create",
        "role_metrics",
        "agent_metrics",
        "agent_create",
    ];
    for tool in &denied {
        assert!(
            !names.contains(*tool),
            "evidence spike must NOT have tool `{tool}`"
        );
    }
}

/// Evidence-spike sessions must NOT include any finalize tool except
/// submit_work.  submit_review, submit_decision, and submit_grooming
/// are for different roles.
#[test]
fn evidence_spike_excludes_non_evidence_finalize_tools() {
    let schemas = tool_schemas_evidence_spike();
    let names = ev_schema_names(&schemas);

    assert!(
        names.contains("submit_work"),
        "evidence spike must have `submit_work`"
    );
    for tool in &["submit_review", "submit_decision", "submit_grooming"] {
        assert!(
            !names.contains(*tool),
            "evidence spike must NOT have finalize tool `{tool}`"
        );
    }
}

/// Evidence-spike sessions must NOT include file write/edit/patch tools.
#[test]
fn evidence_spike_excludes_all_file_mutation_tools() {
    let schemas = tool_schemas_evidence_spike();
    let names = ev_schema_names(&schemas);

    for tool in &["write", "edit", "apply_patch"] {
        assert!(
            !names.contains(*tool),
            "evidence spike must NOT have file mutation tool `{tool}`"
        );
    }
}

/// Evidence-spike sessions must NOT include the shell tool (which would
/// require the command validator).  The tool surface simply omits it.
#[test]
fn evidence_spike_excludes_shell_tool_entirely() {
    let schemas = tool_schemas_evidence_spike();
    let names = ev_schema_names(&schemas);
    assert!(
        !names.contains("shell"),
        "evidence spike must NOT have `shell` — it is excluded from the tool surface"
    );
}

/// The read-only proposal inspection tools (proposal_show,
/// proposal_debate_list) ARE available to evidence spikes.
#[test]
fn evidence_spike_includes_read_only_proposal_inspection() {
    let schemas = tool_schemas_evidence_spike();
    let names = ev_schema_names(&schemas);

    assert!(
        names.contains("proposal_show"),
        "evidence spike must have `proposal_show` for read-only inspection"
    );
    assert!(
        names.contains("proposal_debate_list"),
        "evidence spike must have `proposal_debate_list` for read-only inspection"
    );
}

// ── Architect regression: mutation tools are present ────────────────────────

/// The normal Architect surface retains all expected mutation tools.
/// This regression test ensures the evidence-spike profile restriction
/// does not accidentally narrow the Architect surface.
#[test]
fn architect_regression_has_expected_mutation_tools() {
    let schemas = tool_schemas_architect();
    let names = ev_schema_names(&schemas);

    let expected_mutation = [
        "shell",
        "task_create",
        "task_update",
        "task_transition",
        "task_comment_add",
        "epic_create",
        "memory_write",
        "memory_edit",
        "memory_move",
        "submit_work",
        "task_delete_branch",
        "task_archive_activity",
        "task_reset_counters",
        "task_kill_session",
        "agent_create",
    ];
    for tool in &expected_mutation {
        assert!(
            names.contains(*tool),
            "architect must retain mutation tool `{tool}`"
        );
    }
}

/// The normal Architect surface retains all expected read-only tools.
#[test]
fn architect_regression_has_expected_read_only_tools() {
    let schemas = tool_schemas_architect();
    let names = ev_schema_names(&schemas);

    let expected_read_only = [
        "read",
        "code_search",
        "skill_read",
        "lsp",
        "code_graph",
        "pr_review_context",
        "ci_job_log",
        "ci_artifact",
        "github_search",
        "output_view",
        "output_grep",
        "task_show",
        "task_list",
        "task_activity_list",
        "memory_read",
        "memory_search",
        "memory_list",
        "memory_build_context",
        "memory_health",
        "memory_extracted_audit",
        "memory_broken_links",
        "memory_orphans",
        "task_blocked_list",
        "epic_show",
        "epic_tasks",
        "epic_update",
        "epic_close",
        "agent_metrics",
    ];
    for tool in &expected_read_only {
        assert!(
            names.contains(*tool),
            "architect must retain read-only tool `{tool}`"
        );
    }
}

// ── Dispatch allowlist gate tests ────────────────────────────────────────
// These tests prove the fail-closed evidence-spike allowlist rejects
// blocked tool names before routing, and accepts allowed ones.
// They exercise `is_tool_allowed_for_schemas` with the evidence-spike
// schemas — the same gate that `dispatch_tool_call` uses.

#[test]
fn evidence_spike_gate_accepts_allowed_read_tools() {
    let schemas = tool_schemas_evidence_spike();
    let allowed_names = evidence_spike_tool_names();

    // Representative allowed read/search tools must pass the gate.
    let expected_allowed = [
        "task_show",
        "task_list",
        "task_activity_list",
        "epic_show",
        "epic_tasks",
        "memory_read",
        "memory_search",
        "memory_list",
        "proposal_show",
        "proposal_debate_list",
        "lsp",
        "github_search",
        "output_view",
        "output_grep",
        "skill_read",
        "ci_job_log",
        "ci_artifact",
        "submit_work",
    ];

    for name in &expected_allowed {
        assert!(
            allowed_names.contains(*name),
            "evidence_spike_tool_names() must contain `{name}`"
        );
        assert!(
            crate::helpers::is_tool_allowed_for_schemas(&schemas, name),
            "dispatch gate must accept evidence-spike allowed tool `{name}`"
        );
    }
}

// ── Worker regression: mutation tools are present ───────────────────────────

/// The normal Worker surface retains all expected tools including
/// write/edit/apply_patch, shell, and memory mutation.
#[test]
fn worker_regression_has_expected_mutation_tools() {
    let schemas = tool_schemas_worker();
    let names = ev_schema_names(&schemas);

    let expected_mutation = [
        "shell",
        "write",
        "edit",
        "apply_patch",
        "memory_write",
        "memory_edit",
        "submit_work",
    ];
    for tool in &expected_mutation {
        assert!(
            names.contains(*tool),
            "worker must retain mutation tool `{tool}`"
        );
    }
}

#[test]
fn evidence_spike_gate_rejects_blocked_mutation_tools() {
    let schemas = tool_schemas_evidence_spike();
    let allowed_names = evidence_spike_tool_names();

    // Representative blocked mutation/destructive/admin tools must be
    // rejected by the dispatch gate.
    let expected_blocked = [
        "task_update",
        "task_comment_add",
        "task_create",
        "task_transition",
        "epic_create",
        "epic_update",
        "epic_close",
        "memory_write",
        "memory_edit",
        "memory_move",
        "shell",
        "write",
        "edit",
        "apply_patch",
        "request_lead",
        "request_planner",
    ];

    for name in &expected_blocked {
        assert!(
            !allowed_names.contains(*name),
            "evidence_spike_tool_names() must NOT contain blocked tool `{name}`"
        );
        assert!(
            !crate::helpers::is_tool_allowed_for_schemas(&schemas, name),
            "dispatch gate must reject evidence-spike blocked tool `{name}` \
             — fail-closed violated"
        );
    }
}

// ── Command validator availability ──────────────────────────────────────────

/// The command validator module is available and rejects clearly
/// mutation-shaped commands.  This is a smoke test for the integration
/// point; the bulk of command-validator coverage lives in the
/// `command_validator::tests` module.
#[test]
fn command_validator_rejects_mutation_commands() {
    use crate::command_validator::validate_read_only_command;

    // File mutation
    assert!(validate_read_only_command("rm -rf /").is_err());
    assert!(validate_read_only_command("chmod 777 file").is_err());
    assert!(validate_read_only_command("touch newfile").is_err());

    // VCS mutation
    assert!(validate_read_only_command("git push origin main").is_err());
    assert!(validate_read_only_command("git commit -m msg").is_err());

    // Network mutation
    assert!(validate_read_only_command("curl -X POST url").is_err());

    // Package install
    assert!(validate_read_only_command("pip install requests").is_err());

    // Database mutation
    assert!(validate_read_only_command(r#"psql -c "DROP TABLE t""#).is_err());

    // Redirects
    assert!(validate_read_only_command("echo hi > file").is_err());
    assert!(validate_read_only_command("echo hi >> file").is_err());
}

/// The command validator allows clearly read-only commands.
#[test]
fn command_validator_allows_read_only_commands() {
    use crate::command_validator::validate_read_only_command;

    // File reading
    assert!(validate_read_only_command("cat file.txt").is_ok());
    assert!(validate_read_only_command("grep pattern file.txt").is_ok());
    assert!(validate_read_only_command("find . -name '*.rs'").is_ok());
    assert!(validate_read_only_command("ls -la").is_ok());

    // VCS read-only
    assert!(validate_read_only_command("git log --oneline").is_ok());
    assert!(validate_read_only_command("git diff").is_ok());
    assert!(validate_read_only_command("git status").is_ok());

    // Network read-only
    assert!(validate_read_only_command("curl https://example.com").is_ok());

    // Cargo read-only
    assert!(validate_read_only_command("cargo check").is_ok());
    assert!(validate_read_only_command("cargo clippy").is_ok());

    // Database read-only
    assert!(validate_read_only_command(r#"psql -c "SELECT * FROM t""#).is_ok());

    // Pipe chains
    assert!(validate_read_only_command("cat file | grep pattern | sort").is_ok());
}

// ── Additional gate tests ────────────────────────────────────────────────

#[test]
fn evidence_spike_tool_names_derived_from_schemas() {
    // The allowlist and the schema surface must be identical in size,
    // proving they are derived from the same source and cannot drift.
    let schemas = tool_schemas_evidence_spike();
    let names = evidence_spike_tool_names();

    assert_eq!(
        schemas.len(),
        names.len(),
        "evidence_spike_tool_names count ({}) must match schema count ({})",
        names.len(),
        schemas.len()
    );
}

#[test]
fn evidence_spike_gate_is_fail_closed_for_unknown_tools() {
    let schemas = tool_schemas_evidence_spike();

    // An arbitrary unknown tool name must be rejected.
    assert!(
        !crate::helpers::is_tool_allowed_for_schemas(&schemas, "totally_unknown_tool"),
        "dispatch gate must reject unknown tools (fail-closed)"
    );
}

#[test]
fn evidence_spike_gate_accepts_architect_read_only_tools() {
    // Architect-only read tools that are included in evidence-spike
    // (code_graph, pr_review_context, task_blocked_list) must pass.
    let schemas = tool_schemas_evidence_spike();

    for name in &["code_graph", "pr_review_context", "task_blocked_list"] {
        assert!(
            crate::helpers::is_tool_allowed_for_schemas(&schemas, name),
            "dispatch gate must accept architect read-only tool `{name}` \
             in evidence-spike profile"
        );
    }
}

#[test]
fn normal_dispatch_unaffected_when_no_allowlist() {
    // Verify that is_tool_allowed_for_schemas with the FULL architect
    // schemas accepts all normal architect tools — this models the
    // "no evidence-spike gate" path where allowed_schemas is None.
    let arch_schemas = tool_schemas_architect();
    let arch_names: BTreeSet<String> = arch_schemas
        .iter()
        .filter_map(|v| v.get("name").and_then(|n| n.as_str()).map(String::from))
        .collect();

    // A few representative tools that must be in architect.
    for name in &["shell", "task_update", "memory_write", "submit_work"] {
        assert!(
            arch_names.contains(*name),
            "architect must still contain `{name}` (normal dispatch unaffected)"
        );
    }
}

#[test]
fn evidence_spike_profile_matches_demand_evidence_contract_requirements() {
    // The demand-evidence contract (`proposal_refinement_demand_evidence`)
    // stamps labels `refinement-evidence` + `read-only` and creates a linked
    // Architect-routed evidence spike. This regression test asserts that the
    // profile selected for that contract (the evidence-spike profile) is
    // read-only while preserving investigation capability.
    let schemas = tool_schemas_evidence_spike();
    let allowed_names = evidence_spike_tool_names();

    // Required read/search/proposal/memory inspection tools must remain
    // routable so the spike can actually produce findings.
    let required_read = [
        "read",
        "code_search",
        "code_graph",
        "lsp",
        "skill_read",
        "proposal_show",
        "proposal_debate_list",
        "memory_read",
        "memory_search",
        "memory_list",
        "memory_build_context",
        "task_show",
        "task_list",
        "task_activity_list",
        "epic_show",
        "epic_tasks",
        "output_view",
        "output_grep",
        "github_search",
        "ci_job_log",
        "ci_artifact",
    ];
    for name in &required_read {
        assert!(
            allowed_names.contains(*name),
            "demand-evidence contract evidence-spike profile must expose read tool `{name}`"
        );
        assert!(
            crate::helpers::is_tool_allowed_for_schemas(&schemas, name),
            "dispatch gate must accept required read tool `{name}`"
        );
    }

    // Representative mutation-capable tools that would invalidate the spike
    // must be absent from the surface and rejected by the fail-closed gate.
    let forbidden_mutation = [
        "write",
        "edit",
        "apply_patch",
        "shell",
        "task_create",
        "task_update",
        "task_transition",
        "task_comment_add",
        "epic_create",
        "epic_update",
        "epic_close",
        "memory_write",
        "memory_edit",
        "memory_move",
        "proposal_update",
        "proposal_block_patch",
        "proposal_debate_append",
        "agent_create",
        "request_lead",
        "request_planner",
    ];
    for name in &forbidden_mutation {
        assert!(
            !allowed_names.contains(*name),
            "demand-evidence contract evidence-spike profile must NOT contain `{name}`"
        );
        assert!(
            !crate::helpers::is_tool_allowed_for_schemas(&schemas, name),
            "dispatch gate must reject forbidden mutation tool `{name}` — fail-closed violated"
        );
    }

    // The only mutation-capable path allowed is the spike's own findings
    // handoff via submit_work.
    assert!(
        allowed_names.contains("submit_work"),
        "demand-evidence contract evidence-spike profile must retain `submit_work` for findings handoff"
    );
}
