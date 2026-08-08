// djinn:allow-oversize
use super::*;
use crate::AgentType;
use djinn_core::models::Task;
use serde_json::json;

/// Render a prompt for the given agent type using empty tool schemas.
///
/// This is the djinn-roles equivalent of `djinn_agent::prompts::render_prompt`.
/// Tests that only need template content (not tool-schema content) use this.
pub(crate) fn render_prompt(agent_type: AgentType, task: &Task, ctx: &TaskContext) -> String {
    let config = agent_type.role_config();
    render_prompt_for_role(config, Vec::new, task, ctx)
}

pub(crate) fn make_task() -> Task {
    Task {
        escalation_evidence_at: None,
        id: "task-123".into(),
        project_id: "project-1".into(),
        short_id: "t123".into(),
        epic_id: Some("epic-1".into()),
        title: "Add widget".into(),
        description: "Implement the widget feature.".into(),
        design: "Use the widget pattern.".into(),
        issue_type: "task".into(),
        status: "open".into(),
        priority: 1,
        owner: "dev@example.com".into(),
        labels: r#"["wave:1"]"#.into(),
        acceptance_criteria:
            r#"[{"criterion":"Widget exists","met":false},{"criterion":"Tests pass","met":true}]"#
                .into(),
        reopen_count: 0,
        continuation_count: 0,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".into(),
        agent_type: None,
        execution_context: None,
        created_by_user_id: "fixture-user".into(),
        ci_status: "unknown".into(),
        ci_head_sha: None,
        ci_pr_number: None,
        ci_blocking_required_check_names: "[]".into(),
        ci_primary_blocking_check: None,
        ci_failure_annotations: None,
        ci_failure_fingerprint: None,
        ci_first_seen_at: None,
        ci_last_seen_at: None,
        ci_same_signature_count: 0,
        ci_last_remediation_base_sha: None,
        ci_mirror_head_sha: None,
        ci_github_head_sha: None,
        ci_heads_diverged: None,
        ci_head_observation_error: None,
        ci_mq_state: None,
        ci_mq_run_id: None,
        ci_mq_head_sha: None,
        ci_mq_failed_check_names: None,
        ci_mq_failure_fingerprint: None,
        ci_mq_same_signature_count: None,
        ci_mq_first_seen_at: None,
        ci_mq_last_seen_at: None,
        unresolved_blocker_count: 0,
        refinement_run_id: None,
        refinement_intent_id: None,
        refinement_generation: None,
        refinement_round: None,
        refinement_phase: None,
        refinement_role: None,
        total_reopen_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
    }
}

pub(crate) fn make_ctx() -> TaskContext {
    TaskContext {
        project_path: "/home/user/project".into(),
        workspace_path: "/home/user/project/.task-runtime/worktrees/t123".into(),
        diff: None,
        commits: None,
        start_commit: None,
        end_commit: None,
        conflict_files: None,
        merge_base_branch: None,
        merge_target_branch: None,
        merge_failure_context: None,
        setup_commands: None,
        activity: None,
        worker_summary: None,
        worker_concerns: None,
        epic_context: None,
        knowledge_context: None,
        code_graph_context: None,
        reviewer_diff_context: None,
        ci_blocking_directive: None,
        worker_resume_note: None,
        arbiter_directive: None,
        ci_adjudication_bundle: None,
    }
}

#[test]
fn worker_prompt_describes_private_cargo_target_lifecycle() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(prompt.contains("CARGO_HOME=/cache/cargo"));
    assert!(prompt.contains("/cache/cargo-target-runs/<task_run_id>"));
    assert!(prompt.contains("/cache/cargo-target/<project_id>"));
    assert!(prompt.contains("seeded from `/cache/cargo-target/<project_id>`"));
    assert!(prompt.contains("removed after the run"));
    assert!(prompt.contains("Do **not** redirect Cargo to `/cache/cargo-target/<project_id>`"));
    let old_shared_target_claim =
        ["CARGO_TARGET_DIR=", "/cache/", "cargo-target/", "<project>"].concat();
    assert!(!prompt.contains(&old_shared_target_claim));
}

/// The dispatcher injects the research workflow ONLY for research tasks —
/// the model never has to detect its mode.
#[test]
fn worker_research_mode_injects_research_section() {
    let mut task = make_task();
    task.issue_type = "research".into();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("Research Deliverable"),
        "research task should inject the research workflow section"
    );
    assert!(prompt.contains("Originated from task task-123"));
    assert!(!prompt.contains("{{"));
}

/// Conflict context selects the merge-resolution workflow regardless of
/// issue_type, and a plain task without conflict context never sees it.
#[test]
fn worker_conflict_mode_injects_conflict_section() {
    let task = make_task();
    let with_conflict = TaskContext {
        conflict_files: Some("- src/main.rs".into()),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &with_conflict);
    assert!(prompt.contains("Merge Conflict — Resolve This First"));
    assert!(prompt.contains("src/main.rs"));

    let no_conflict = render_prompt(AgentType::Worker, &task, &make_ctx());
    assert!(!no_conflict.contains("Merge Conflict — Resolve This First"));
}

#[test]
fn reviewer_prompt_describes_private_cargo_target_lifecycle() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

    assert!(prompt.contains("CARGO_HOME=/cache/cargo"));
    assert!(prompt.contains("/cache/cargo-target-runs/<task_run_id>"));
    assert!(prompt.contains("/cache/cargo-target/<project_id>"));
    assert!(prompt.contains("seeded from the warm base"));
    assert!(prompt.contains("removed after the run"));
    assert!(prompt.contains("workers must not override it or point Cargo at the shared warm base"));
    let old_shared_target_claim =
        ["CARGO_TARGET_DIR=", "/cache/", "cargo-target/", "<project>"].concat();
    assert!(!prompt.contains(&old_shared_target_claim));
}

#[test]
fn reviewer_prompt_renders_diff_context_section_when_present() {
    let task = make_task();
    let ctx = TaskContext {
            reviewer_diff_context: Some(
                "## Changed symbols (HIGH risk first)\n\n- `foo::bar` (HIGH risk, 12 direct callers, 3 modules)\n  - file: src/foo.rs"
                    .into(),
            ),
            ..make_ctx()
        };
    let prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

    assert!(
        prompt.contains("## Changed Symbols"),
        "reviewer prompt should include the Changed Symbols section heading"
    );
    assert!(
        prompt.contains("`foo::bar` (HIGH risk, 12 direct callers, 3 modules)"),
        "reviewer prompt should include the diff bullet body"
    );
    assert!(
        !prompt.contains("{{reviewer_diff_context_section}}"),
        "slot should be substituted"
    );
    assert!(!prompt.contains("{{"));
}

#[test]
fn reviewer_prompt_omits_diff_context_section_when_absent() {
    let task = make_task();
    let ctx = make_ctx(); // reviewer_diff_context: None
    let prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

    assert!(
        !prompt.contains("## Changed Symbols"),
        "reviewer prompt should not include the Changed Symbols section when absent"
    );
    assert!(!prompt.contains("{{reviewer_diff_context_section}}"));
}

#[test]
fn format_acceptance_criteria_invalid_json_passthrough() {
    let result = format_acceptance_criteria("not json");
    assert_eq!(result, "not json");
}

#[test]
fn format_labels_empty_array() {
    assert_eq!(format_labels("[]"), "");
}

#[test]
fn worker_prompt_includes_setup_commands_when_present() {
    let task = make_task();
    let ctx = TaskContext {
        setup_commands: Some("- `npm install`\n- `npm run build`".into()),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(prompt.contains("Automated Commands"));
    assert!(prompt.contains("Do not run them yourself"));
    assert!(prompt.contains("npm install"));
    assert!(!prompt.contains("{{setup_commands_section}}"));
}

/// Return a small tool schema whose signature is visible in the rendered tools
/// section. Descriptions are intentionally omitted from production rendering,
/// so this fixture keeps mutations in signature-changing fields.
fn make_test_tool_schema() -> serde_json::Value {
    json!({
        "name": "test_tool",
        "description": "A tool for testing; not visible in production tool section.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "alpha": { "type": "string" },
                "beta": { "type": "string" }
            },
            "required": ["alpha"]
        }
    })
}

/// Render a prompt for the given agent type using a deterministic fixture tool
/// schema. Used by prompt-hash tests so mutations of the schema alter the
/// rendered signature.
fn render_prompt_with_test_tool(agent_type: AgentType, task: &Task, ctx: &TaskContext) -> String {
    let config = agent_type.role_config();
    render_prompt_for_role(config, tool_schema_fixture, task, ctx)
}

fn tool_schema_fixture() -> Vec<serde_json::Value> {
    vec![make_test_tool_schema()]
}

// ── Rendered-system-prompt hash tests (h0cl) ─────────────────────────────────

#[test]
fn rendered_system_prompt_hash_matches_exact_format() {
    // Known SHA-256("hello") truncated to the first 16 hex chars.
    let hash = rendered_system_prompt_hash("hello");
    assert_eq!(hash, "sha256:2cf24dba5fb0a30e");
    assert!(hash.starts_with("sha256:"));
    assert_eq!(hash.len(), "sha256:".len() + 16);
}

#[test]
fn rendered_system_prompt_hash_is_utf8_byte_exact() {
    // Hash must reflect bytes, not Unicode code points. The literal "é" (U+00E9)
    // and the escape \u{00e9} produce identical UTF-8 bytes.
    let literal_hash = rendered_system_prompt_hash("é");
    let escape_hash = rendered_system_prompt_hash("\u{00e9}");
    assert_eq!(
        literal_hash, escape_hash,
        "hash should be over identical UTF-8 bytes"
    );

    // NFD (e + combining acute) vs NFC (composed é) produce different bytes, so
    // the hashes must differ.
    let nfd = "\u{0065}\u{0301}";
    let nfc = "\u{00e9}";
    assert_ne!(nfd.as_bytes(), nfc.as_bytes(), "precondition: bytes differ");
    assert_ne!(
        rendered_system_prompt_hash(nfd),
        rendered_system_prompt_hash(nfc),
        "different UTF-8 byte sequences should yield different hashes"
    );
}

#[test]
fn identical_rendered_prompts_produce_identical_hashes() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt_a = render_prompt_with_test_tool(AgentType::Worker, &task, &ctx);
    let prompt_b = render_prompt_with_test_tool(AgentType::Worker, &task, &ctx);

    assert_eq!(prompt_a, prompt_b);
    assert_eq!(
        rendered_system_prompt_hash(&prompt_a),
        rendered_system_prompt_hash(&prompt_b),
        "identical rendered strings must hash to the same identifier"
    );
}

#[test]
fn different_task_title_changes_rendered_hash() {
    let ctx = make_ctx();
    let base_task = make_task();
    let base_prompt = render_prompt_with_test_tool(AgentType::Worker, &base_task, &ctx);
    let base_hash = rendered_system_prompt_hash(&base_prompt);

    let mut mutated_task = make_task();
    mutated_task.title = "Add a totally different widget".into();
    let mutated_prompt = render_prompt_with_test_tool(AgentType::Worker, &mutated_task, &ctx);
    let mutated_hash = rendered_system_prompt_hash(&mutated_prompt);

    assert_ne!(
        base_hash, mutated_hash,
        "a prompt-visible task mutation should change the hash"
    );
}

fn tool_schema_for_renamed_tool() -> Vec<serde_json::Value> {
    vec![json!({
        "name": "renamed_tool",
        "inputSchema": {
            "type": "object",
            "properties": {
                "alpha": { "type": "string" },
                "beta": { "type": "string" }
            },
            "required": ["alpha"]
        }
    })]
}

fn tool_schema_for_property_mutated() -> Vec<serde_json::Value> {
    vec![json!({
        "name": "test_tool",
        "inputSchema": {
            "type": "object",
            "properties": {
                "alpha": { "type": "string" },
                "gamma": { "type": "string" }
            },
            "required": ["alpha"]
        }
    })]
}

#[test]
fn tool_schema_signature_mutation_changes_rendered_hash() {
    let task = make_task();
    let ctx = make_ctx();

    let base_prompt = render_prompt_for_role(
        AgentType::Worker.role_config(),
        tool_schema_for_once,
        &task,
        &ctx,
    );
    let base_hash = rendered_system_prompt_hash(&base_prompt);

    // Mutate the tool name: this changes the rendered `- `test_tool(...)` line.
    let mutated_prompt = render_prompt_for_role(
        AgentType::Worker.role_config(),
        tool_schema_for_renamed_tool,
        &task,
        &ctx,
    );
    let mutated_hash = rendered_system_prompt_hash(&mutated_prompt);
    assert_ne!(
        base_hash, mutated_hash,
        "a tool name mutation visible in the rendered signature should change the hash"
    );

    // Mutate a property name in the schema: this changes the rendered signature.
    let property_prompt = render_prompt_for_role(
        AgentType::Worker.role_config(),
        tool_schema_for_property_mutated,
        &task,
        &ctx,
    );
    let property_hash = rendered_system_prompt_hash(&property_prompt);
    assert_ne!(
        base_hash, property_hash,
        "a visible property-name mutation should change the hash"
    );
}

fn tool_schema_for_once() -> Vec<serde_json::Value> {
    vec![json!({
        "name": "test_tool",
        "inputSchema": {
            "type": "object",
            "properties": {
                "alpha": { "type": "string" },
                "beta": { "type": "string" }
            },
            "required": ["alpha"]
        }
    })]
}

fn tool_schema_for_description_test() -> Vec<serde_json::Value> {
    vec![json!({
        "name": "test_tool",
        "description": "first description",
        "inputSchema": {
            "type": "object",
            "properties": {
                "alpha": { "type": "string" }
            },
            "required": ["alpha"]
        }
    })]
}

#[test]
fn tool_description_mutation_does_not_affect_hash_when_descriptions_omitted() {
    // `format_tools_section` omits description bodies, so mutating only the
    // description should not change the rendered tools section. This is a
    // negative regression guard: if descriptions are ever included, the helper
    // must also pick them up and this test will need to be updated.
    let task = make_task();
    let ctx = make_ctx();

    let base_prompt = render_prompt_for_role(
        AgentType::Worker.role_config(),
        tool_schema_for_description_test,
        &task,
        &ctx,
    );
    let mutated_prompt = render_prompt_for_role(
        AgentType::Worker.role_config(),
        tool_schema_for_description_mutated,
        &task,
        &ctx,
    );

    assert_eq!(
        base_prompt, mutated_prompt,
        "description-only mutation must not alter rendered prompt"
    );
    assert_eq!(
        rendered_system_prompt_hash(&base_prompt),
        rendered_system_prompt_hash(&mutated_prompt),
        "hash should stay stable when rendered prompt is unchanged"
    );
}

fn tool_schema_for_description_mutated() -> Vec<serde_json::Value> {
    vec![json!({
        "name": "test_tool",
        "description": "second completely different description",
        "inputSchema": {
            "type": "object",
            "properties": {
                "alpha": { "type": "string" }
            },
            "required": ["alpha"]
        }
    })]
}

#[test]
fn hash_boundary_is_after_truncation() {
    let task = make_task();
    // Inject a huge activity log that forces `MAX_SYSTEM_PROMPT_CHARS` truncation.
    let huge_activity = "x".repeat(40_000);
    let ctx = TaskContext {
        activity: Some(huge_activity.clone()),
        ..make_ctx()
    };
    let truncated_prompt = render_prompt(AgentType::Worker, &task, &ctx);
    assert!(
        truncated_prompt.len() <= MAX_SYSTEM_PROMPT_CHARS + 200,
        "prompt should be truncated within the expected budget"
    );
    assert!(truncated_prompt.contains("omitted") || truncated_prompt.contains("truncated"));

    let hash = rendered_system_prompt_hash(&truncated_prompt);
    assert!(
        hash.starts_with("sha256:"),
        "hash must use the required prefix"
    );
    assert_eq!(
        hash.len(),
        "sha256:".len() + 16,
        "hash must be 16 hex chars"
    );

    // The hash of the truncated output must differ from the hash of an
    // unbounded version of the same inputs, proving the boundary is applied to
    // the final returned string.
    let longer = format!("{truncated_prompt}\n{huge_activity}");
    assert_ne!(
        rendered_system_prompt_hash(&longer),
        hash,
        "hash must reflect the final truncated string, not a longer unbounded version"
    );
}

#[test]
fn worker_prompt_omits_setup_section_when_no_commands() {
    let task = make_task();
    let ctx = make_ctx(); // setup_commands: None
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(!prompt.contains("Automated Commands"));
    assert!(!prompt.contains("{{setup_commands_section}}"));
}

#[test]
fn system_prompt_truncated_when_exceeding_hard_cap() {
    let task = make_task();
    // Inject a massive activity log that blows past the 48k char cap.
    let huge_activity = "x".repeat(40_000);
    let ctx = TaskContext {
        activity: Some(huge_activity),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.len() <= MAX_SYSTEM_PROMPT_CHARS + 200, // +200 for the truncation notice
        "prompt should be truncated to ~48k chars, got {}",
        prompt.len()
    );
    // smart_truncate uses "bytes omitted" or "truncated" markers
    assert!(prompt.contains("omitted") || prompt.contains("truncated"));
}

#[test]
fn system_prompt_not_truncated_when_under_cap() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(!prompt.contains("bytes omitted"));
    assert!(!prompt.contains("[truncated"));
}

#[test]
fn worker_prompt_includes_merge_failure_context() {
    let task = make_task();
    let ctx = TaskContext {
        merge_failure_context: Some(
            "**Merge Conflict Detected**\n\nFile `src/main.rs` has conflicts.".into(),
        ),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("Merge Conflict Detected"),
        "worker prompt should include merge failure context"
    );
    assert!(
        !prompt.contains("{{merge_failure_context}}"),
        "template placeholder should be replaced"
    );
}

#[test]
fn worker_prompt_includes_conflict_files_for_conflict_context() {
    let task = make_task();
    let ctx = TaskContext {
        conflict_files: Some("- src/main.rs\n- src/lib.rs".into()),
        merge_base_branch: Some("task/abc123".into()),
        merge_target_branch: Some("main".into()),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("src/main.rs"),
        "worker prompt should include conflict files"
    );
    assert!(
        prompt.contains("task/abc123"),
        "worker prompt should include merge base branch"
    );
    assert!(
        prompt.contains("main"),
        "worker prompt should include merge target branch"
    );
}

#[test]
fn worker_prompt_contains_scoped_build_guidance() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("scoped"),
        "worker prompt should mention scoped build/check commands"
    );
}

#[test]
fn planner_prompt_prunes_unverifiable_acceptance_criteria() {
    let mut decomposition_task = make_task();
    decomposition_task.issue_type = "planning".into();
    let ctx = make_ctx();
    let decomposition_prompt = render_prompt(AgentType::Planner, &decomposition_task, &ctx);

    assert!(
        decomposition_prompt.contains("unavailable external tools, external infrastructure"),
        "decomposition planner should recognize unavailable external proof as invalid spec"
    );
    assert!(
        decomposition_prompt
            .contains("Lack of Djinn tool/environment access is NOT a reason to `escalate`"),
        "decomposition planner should prune unverifiable AC instead of escalating for missing tools"
    );
    assert!(
        decomposition_prompt
            .contains("Rewrite or drop invalid task acceptance criteria with `task_update`"),
        "decomposition planner should rewrite or drop invalid task AC"
    );
    assert!(
        decomposition_prompt.contains("submit_grooming(decision=\"close\")"),
        "decomposition planner should close planning when pruning leaves no implementable work"
    );
    assert!(
        decomposition_prompt
            .contains("objectively checkable by the executing role's actual tool surface"),
        "task AC authoring should require criteria checkable by the executing role"
    );
    assert!(
        decomposition_prompt.contains("Do not create retry worker tasks")
            && decomposition_prompt
                .contains("Docker/Postgres/Kubernetes/operator/Djinn-authenticated proof"),
        "decomposition planner should not create retry worker tasks for external proof"
    );

    let mut intervention_task = make_task();
    intervention_task.issue_type = "review".into();
    let intervention_prompt = render_prompt(AgentType::Planner, &intervention_task, &ctx);

    assert!(
        intervention_prompt
            .contains("requires tools/environment outside Djinn's available tool surface"),
        "intervention planner should detect unverifiable AC loops"
    );
    assert!(
        intervention_prompt
            .contains("Prune or repair the criterion with `task_update` instead of escalating"),
        "intervention planner should prune or repair unverifiable AC instead of escalating"
    );
    assert!(
        intervention_prompt.contains("Tripwire adjudication"),
        "intervention planner must document the tripwire-adjudication case"
    );
    assert!(
        intervention_prompt.contains("CLOSE this escalation")
            && intervention_prompt.contains("tripwire.hold.released"),
        "tripwire adjudication must explain the close-releases-the-hold semantic"
    );
    assert!(
        intervention_prompt.contains("do NOT close-release")
            && intervention_prompt.contains("supersedes the hold"),
        "tripwire adjudication must explain the reopen-with-directive alternative"
    );
    // 4etb: the ceiling is still finite, but exhausting it no longer means an
    // unconditional terminal fail. The prompt must now name BOTH branches of
    // the exhausted-ladder ownership contract, because a planner that believes
    // an exhausted source is always force-closed will not understand why one
    // with an open PR reappears in `pr_review`. Asserting both branches plus
    // the exact terminal reason is strictly stronger than the old single word.
    assert!(
        intervention_prompt.contains("escalation ladder is FINITE"),
        "intervention planner must document the finite escalation ceiling"
    );
    assert!(
        intervention_prompt.contains("never a fourth"),
        "the terminal-rung round ceiling must be explicit"
    );
    assert!(
        intervention_prompt.contains("pr_review"),
        "the exhausted-ladder open-PR branch must name its owner"
    );
    assert!(
        intervention_prompt.contains("adjudication ladder exhausted without an actionable PR"),
        "the exhausted-ladder no-PR branch must name its exact terminal reason"
    );

    let mut proposal_task = make_task();
    proposal_task.issue_type = "epic_breakdown".into();
    let proposal_prompt = render_prompt(AgentType::Planner, &proposal_task, &ctx);

    assert!(
        proposal_prompt.contains(
            "Only translate proposal AC into epic descriptions/AC when they are checkable"
        ),
        "proposal decomposition should only translate verifiable AC into epics"
    );
    assert!(
            proposal_prompt.contains("Do not convert external-infra/operator-only proof requirements into acceptance criteria"),
            "proposal decomposition should redirect external proof requirements out of AC"
        );
}

// ── s3z7: verify-after-lands planner-policy cleanup ──────────────────────────
//
// AC1: decomposition prompt must tell planners not to create standalone
// deterministic verify-after-lands worker tasks once required CI is the
// coordinator gate.
// AC2: guidance must preserve the existing rule that workers write code and
// are not used merely to verify/close tasks/epics, while still allowing focused
// tests during implementation work.
// AC3: prompt regression tests must fail if guidance asks for CI-green as
// planner-authored acceptance criteria or reintroduces verify-only terminal
// worker slices.

#[test]
fn planner_decomposition_prohibits_verify_after_lands_worker_tasks() {
    let mut decomposition_task = make_task();
    decomposition_task.issue_type = "planning".into();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Planner, &decomposition_task, &ctx);

    // AC1: The prompt must explicitly prohibit standalone verify-after-lands tasks.
    assert!(
        prompt.contains("verify-after-lands"),
        "decomposition prompt must mention verify-after-lands tasks"
    );
    assert!(
        prompt.contains("do NOT create standalone deterministic"),
        "decomposition prompt must tell planners not to create standalone deterministic verify tasks"
    );
    assert!(
        prompt.contains("required CI is the coordinator gate"),
        "decomposition prompt must frame required CI as the coordinator gate"
    );
}

#[test]
fn planner_decomposition_preserves_workers_write_code_rule() {
    let mut decomposition_task = make_task();
    decomposition_task.issue_type = "planning".into();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Planner, &decomposition_task, &ctx);

    // AC2: The existing rule that workers write code must be preserved.
    assert!(
        prompt.contains("workers write code"),
        "decomposition prompt must preserve the rule that workers write code"
    );
    assert!(
        prompt.contains("Never create a worker task merely to verify or close"),
        "decomposition prompt must preserve the rule against verify/close-only worker tasks"
    );

    // AC2: Focused tests during implementation work must still be allowed.
    assert!(
        prompt.contains("Workers MAY run focused"),
        "decomposition prompt must allow focused test runs during implementation"
    );
    assert!(
        prompt.contains("implementation-local test commands"),
        "decomposition prompt must distinguish implementation-local test commands"
    );
}

#[test]
fn planner_decomposition_distinguishes_local_tests_from_verify_only_slices() {
    let mut decomposition_task = make_task();
    decomposition_task.issue_type = "planning".into();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Planner, &decomposition_task, &ctx);

    // The prompt must draw a clear line between running tests for code you
    // wrote and a standalone verify-only terminal slice.
    assert!(
        prompt.contains("Allowed"),
        "decomposition prompt must mark allowed test-running behavior"
    );
    assert!(
        prompt.contains("Prohibited"),
        "decomposition prompt must mark prohibited verify-only slice behavior"
    );
    assert!(
        prompt.contains("wait for or re-run deterministic post-land CI"),
        "decomposition prompt must describe the prohibited verify-only shape"
    );
}

#[test]
fn planner_decomposition_must_not_require_ci_green_as_acceptance_criteria() {
    let mut decomposition_task = make_task();
    decomposition_task.issue_type = "planning".into();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Planner, &decomposition_task, &ctx);

    // AC3: The prompt must explicitly prohibit CI-green as task AC.
    assert!(
        prompt.contains("Do NOT put `CI must be green` in task acceptance criteria"),
        "decomposition prompt must prohibit CI-green as task AC"
    );
    assert!(
        prompt.contains("Required CI pass/fail is coordinator control flow"),
        "decomposition prompt must frame required CI as coordinator control flow, not worker AC"
    );
}

#[test]
fn planner_decomposition_verify_after_lands_guidance_has_no_unresolved_placeholders() {
    let mut decomposition_task = make_task();
    decomposition_task.issue_type = "planning".into();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Planner, &decomposition_task, &ctx);

    assert!(
        !prompt.contains("{{"),
        "decomposition prompt should have no unresolved placeholders"
    );
}

#[test]
fn planner_decomposition_verify_guidance_present_only_in_decomposition_mode() {
    // The verify-after-lands guidance lives in decomposition.md and should
    // appear for the planning (decomposition) mode. Other planner modes
    // (intervention, proposal) should not duplicate it since they operate on
    // existing tasks, not wave decomposition.
    let ctx = make_ctx();

    let mut decomposition_task = make_task();
    decomposition_task.issue_type = "planning".into();
    let decomposition_prompt = render_prompt(AgentType::Planner, &decomposition_task, &ctx);
    assert!(
        decomposition_prompt.contains("verify-after-lands"),
        "decomposition mode must include verify-after-lands guidance"
    );

    // The general planner.md "workers write code" / decision rules apply to all
    // modes, but the specific verify-after-lands decomposition guidance should
    // not be duplicated in intervention/proposal modes.
    let mut intervention_task = make_task();
    intervention_task.issue_type = "review".into();
    let intervention_prompt = render_prompt(AgentType::Planner, &intervention_task, &ctx);
    assert!(
        !intervention_prompt.contains("verify-after-lands"),
        "intervention mode should not include verify-after-lands decomposition guidance"
    );

    let mut proposal_task = make_task();
    proposal_task.issue_type = "epic_breakdown".into();
    let proposal_prompt = render_prompt(AgentType::Planner, &proposal_task, &ctx);
    assert!(
        !proposal_prompt.contains("verify-after-lands"),
        "proposal mode should not include verify-after-lands decomposition guidance"
    );
}

#[test]
fn externally_blocked_replay_prunes_and_closes_without_loop_outcomes() {
    let externally_blocked_criteria = [
        "Prove the migration in Docker Compose against Postgres",
        "Validate rollout in Kubernetes with operator-only cluster access",
        "Confirm Djinn-authenticated production API access from the task pod",
    ];
    let invalid_spec_summary = externally_blocked_criteria.join("; ");
    assert!(invalid_spec_summary.contains("Docker"));
    assert!(invalid_spec_summary.contains("Postgres"));
    assert!(invalid_spec_summary.contains("Kubernetes"));
    assert!(invalid_spec_summary.contains("operator-only"));
    assert!(invalid_spec_summary.contains("Djinn-authenticated"));

    let converged_replay = [
        "task_update(id=\"4lzx-worker\", acceptance_criteria=[\"external proof pruned; no implementable work remains\"])",
        "task_comment_add(id=\"4lzx-worker\", body=\"Docker/Postgres/Kubernetes/operator/Djinn-authenticated access is unavailable to task pods; invalid spec pruned, not escalated\")",
        "memory_edit(identifier=\"01r3-roadmap\", operation=\"append\", content=\"External proof moved to runbook/checklist rationale\")",
        "epic_update(id=\"01r3\", description=\"External proof requirements repaired out of worker AC\")",
        "task_transition(id=\"4lzx-worker\", status=\"close\", reason=\"no implementable work remains after pruning invalid external proof AC\")",
        "epic_close(id=\"01r3\")",
        "submit_grooming(summary=\"Pruned externally-blocked criteria and closed epic\", decision=\"close\")",
    ];

    assert!(
        converged_replay
            .iter()
            .any(|action| action.starts_with("epic_close")),
        "converged replay must close the epic after pruning invalid external-proof criteria"
    );
    assert!(
        converged_replay
            .iter()
            .any(|action| action.contains("submit_grooming")
                && action.contains("decision=\"close\"")),
        "converged replay must close this planning task with submit_grooming(decision=\"close\")"
    );

    assert!(
        !converged_replay
            .iter()
            .any(|action| action.contains("submit_grooming")
                && action.contains("decision=\"escalate\"")),
        "missing Docker/Postgres/Kubernetes/operator/Djinn access is invalid spec, not escalation"
    );
    assert!(
        !converged_replay.iter().any(|action| {
            action.starts_with("task_create")
                && (action.contains("external proof")
                    || action.contains("Docker")
                    || action.contains("Postgres")
                    || action.contains("Kubernetes")
                    || action.contains("operator")
                    || action.contains("Djinn-authenticated"))
        }),
        "external proof requirements must be pruned/rewritten or documented, not converted into retry worker tasks"
    );

    let final_action = converged_replay
        .last()
        .expect("synthetic replay should include final planner action");
    assert!(
        final_action.contains("submit_grooming") && final_action.contains("decision=\"close\""),
        "final planner action must not omit decision=\"close\" or the planning task remains redispatchable"
    );
    assert!(
        !final_action.contains("decision=\"approve\"")
            && !final_action.contains("decision=\"reopen\"")
            && !final_action.contains("decision=\"escalate\""),
        "final planner action must not leave an open-ended redispatch path"
    );
}

/// Architect spike notes must still carry task traceability (per ADR-051
/// Contract 2 / §9 "Spike and Research Findings — Memory Writes").
/// The traceability text is in the architect template, not the tool section.
#[test]
fn architect_prompt_requires_read_back_verification_before_file_comments() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Architect, &task, &ctx);

    assert!(
            prompt.contains("Never use it to claim a file exists, was copied, or was moved until you have read that exact path back successfully in the current session"),
            "architect prompt should forbid file-existence comments before read-back verification"
        );
    assert!(
            prompt.contains("Never add a task comment claiming a file exists, was copied, or was moved unless you have just verified that exact path by reading it back successfully"),
            "architect prompt should require read-back verification immediately before file-placement comments"
        );
}

/// The learned-prompt amendment guidance has been removed from the planner
/// template (3x0w). Verify the section does NOT appear in any Planner mode.
#[test]
fn planner_learned_prompt_guidance_absent_across_modes() {
    let ctx = make_ctx();

    for issue_type in ["planning", "review", "epic_breakdown", "task"] {
        let mut task = make_task();
        task.issue_type = issue_type.into();
        let prompt = render_prompt(AgentType::Planner, &task, &ctx);
        assert!(
            !prompt.contains("Learned-prompt amendments"),
            "planner prompt for issue_type={issue_type} must NOT contain the removed learned-prompt amendment section"
        );
    }
}

// ── Tribunal roles (k9zw) ────────────────────────────────────────────────

#[test]
fn advocate_prompt_contains_role_responsibilities() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Advocate, &task, &ctx);

    assert!(
        prompt.contains("Advocate"),
        "advocate prompt should identify the Advocate role"
    );
    assert!(
        prompt.contains("proposal"),
        "advocate prompt should mention proposals"
    );
    assert!(
        prompt.contains("Adversary") || prompt.contains("adversary"),
        "advocate prompt should reference the Adversary role"
    );
    assert!(
        prompt.contains("proposal_blocks") || prompt.contains("block catalog"),
        "advocate prompt should reference proposal_blocks / block catalog"
    );
    // MDX enrichment must not be mandatory for DoR.
    assert!(
        prompt.contains("optional") || prompt.contains("not required") || prompt.contains("not a"),
        "advocate prompt should frame MDX enrichment as optional"
    );
    assert!(
        !prompt.contains("{{"),
        "advocate prompt should have no unresolved placeholders"
    );
}

#[test]
fn adversary_prompt_contains_objection_contract() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Adversary, &task, &ctx);

    assert!(
        prompt.contains("Adversary") || prompt.contains("adversary"),
        "adversary prompt should identify the Adversary role"
    );
    assert!(
        prompt.contains("blocking") || prompt.contains("non-blocking"),
        "adversary prompt should mention blocking/non-blocking objections"
    );
    assert!(
        prompt.contains("falsifiable"),
        "adversary prompt should require falsifiable objections"
    );
    assert!(
        prompt.contains("dry") || prompt.contains("no new"),
        "adversary prompt should mention the dry signal"
    );
    assert!(
        !prompt.contains("{{"),
        "adversary prompt should have no unresolved placeholders"
    );
}

/// A blocking judge verdict now routes to the Advocate (it is the only role
/// that rewrites the proposal body). The Advocate's prompt must therefore tell
/// it to read judge verdicts and treat a needs-work verdict's prescribed remedy
/// as a work item — the prompt previously framed `proposal_debate_list` purely
/// as an objection list and mentioned "verdict" only to forbid writing one, so
/// a verdict-only round read as "nothing to do".
#[test]
fn advocate_prompt_treats_a_needs_work_verdict_as_a_work_item() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Advocate, &task, &ctx);

    assert!(
        prompt.contains("kind=\"verdict\"") || prompt.contains("`verdict`"),
        "advocate prompt must name the verdict entry kind it has to read"
    );
    assert!(
        prompt.contains("needs-work"),
        "advocate prompt must name the needs-work verdict it has to act on"
    );
    assert!(
        prompt.contains("latest"),
        "advocate prompt must tell the Advocate to take the LATEST verdict \
         (verdicts are never marked resolved, so an unresolved old one is not a signal)"
    );
    // A verdict-only round — zero unresolved objections, one outstanding
    // needs-work verdict — is exactly the state that used to dead-end.
    assert!(
        prompt.contains("zero unresolved objections"),
        "advocate prompt must state that a round can carry only a verdict"
    );
    // The write path the Advocate must use must still be named.
    assert!(
        prompt.contains("proposal_update"),
        "advocate prompt must name the body-revision tool"
    );
    // The existing authority boundary must survive: rebuttal-only appends, and
    // no verdicts or resolutions from the Advocate.
    assert!(
        prompt.contains("kind=\"rebuttal\"") && prompt.contains("ONLY kind you may append"),
        "advocate prompt must keep the rebuttal-only append rule"
    );
    assert!(
        prompt.contains("Never file objections or verdicts"),
        "advocate prompt must keep the never-file-verdicts rule"
    );
    assert!(
        !prompt.contains("{{"),
        "advocate prompt should have no unresolved placeholders"
    );
}

/// The Adversary's dedup guidance must stay scoped to its *own* prior
/// objections. Framing judge verdicts as part of the dedup filter ("so you do
/// NOT re-raise an objection already filed or already resolved") suppressed
/// judge-originated requirements: the Adversary read the verdict as something
/// already handled and filed nothing, the round went dry, and the Advocate was
/// never dispatched.
#[test]
fn adversary_prompt_scopes_dedup_to_objections_not_judge_verdicts() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Adversary, &task, &ctx);

    // Dedup survives.
    assert!(
        prompt.contains("re-raise an objection already filed"),
        "adversary prompt must keep the objection dedup rule"
    );
    // But it is explicitly scoped away from verdicts.
    assert!(
        prompt.contains("Judge verdicts are not your dedup list"),
        "adversary prompt must carry the verdict-dedup carve-out"
    );
    assert!(
        prompt.contains("never marked resolved"),
        "adversary prompt must explain why an unresolved verdict is not a dedup signal"
    );
    assert!(
        prompt.contains("verdict the Advocate did not implement"),
        "adversary prompt must license filing an objection for an unimplemented verdict"
    );
    // The anti-filler rule must survive so the carve-out is not read as a
    // license to paraphrase the verdict into a manufactured objection.
    assert!(
        prompt.contains("manufactured blockers are worse than a dry round"),
        "adversary prompt must keep the anti-filler rule"
    );
    assert!(
        !prompt.contains("{{"),
        "adversary prompt should have no unresolved placeholders"
    );
}

/// The Judge closes every round — `record_advocate_revision` has always routed
/// straight to `JudgeAdjudication` — but `judge.md` claimed it was dispatched
/// "ONLY after the Adversary produces no new blocking objections for N=2
/// consecutive rounds" and did "NOT participate in the revision loop". Both were
/// false, and `dry_rounds_required` is not enforced anywhere in the tree, so no
/// prompt may restate that as a live contract. The Judge must also know its
/// needs-work verdict is the Advocate's work order on a dry round.
#[test]
fn tribunal_prompts_do_not_restate_the_unenforced_dry_round_contract() {
    let ctx = make_ctx();
    let task = make_task();

    for agent in [AgentType::Judge, AgentType::Adversary, AgentType::Advocate] {
        let prompt = render_prompt(agent, &task, &ctx);
        assert!(
            !prompt.contains("N=2"),
            "{} prompt must not restate the unenforced dry-round contract",
            agent.as_str()
        );
        assert!(
            !prompt.contains("consecutive rounds"),
            "{} prompt must not restate the unenforced dry-round contract",
            agent.as_str()
        );
    }

    let judge = render_prompt(AgentType::Judge, &task, &ctx);
    assert!(
        !judge.contains("do NOT participate in the revision loop")
            && !judge.contains("dispatched ONLY after"),
        "judge prompt must not claim it sits outside the revision loop"
    );
    assert!(
        judge.contains("close **every** round") || judge.contains("You close every round"),
        "judge prompt must state that it rules at the end of every round"
    );
    // The needs-work verdict is the Advocate's only instruction on a dry round,
    // so the Judge has to be told to make it concrete.
    assert!(
        judge.contains("work order"),
        "judge prompt must frame a needs-work verdict as the Advocate's work order"
    );
    assert!(
        !judge.contains("{{"),
        "judge prompt should have no unresolved placeholders"
    );
}

/// Human approval, authorization and organizational structure are categorically
/// outside the agent's model: djinn writes code and opens PRs, and approval and
/// merge are enforced by the forge and its configured owners. Without this rule
/// an Adversary can demand an approval control and a Judge can accept one as the
/// fix for an objection, and neither notices the category error — which is how a
/// signed-delegation / CODEOWNERS / identity-separation acceptance criterion
/// reached a `ready` proposal. Every role that can author, demand or bless an AC
/// must carry the rule, so it cannot silently vanish from one of them.
#[test]
fn tribunal_and_planner_prompts_exclude_human_approval_machinery() {
    const FORGE_RULE: &str = "enforced by the forge and its configured owners";

    let ctx = make_ctx();

    let mut cases: Vec<(&str, String)> = Vec::new();

    for agent in [AgentType::Adversary, AgentType::Judge, AgentType::Advocate] {
        let task = make_task();
        cases.push((agent.as_str(), render_prompt(agent, &task, &ctx)));
    }

    let mut decomposition_task = make_task();
    decomposition_task.issue_type = "planning".into();
    cases.push((
        "planner/decomposition",
        render_prompt(AgentType::Planner, &decomposition_task, &ctx),
    ));

    let mut proposal_task = make_task();
    proposal_task.issue_type = "epic_breakdown".into();
    cases.push((
        "planner/proposal",
        render_prompt(AgentType::Planner, &proposal_task, &ctx),
    ));

    let mut proposal_review_task = make_task();
    proposal_review_task.issue_type = "epic_breakdown".into();
    proposal_review_task.title = format!(
        "{} 89bb",
        djinn_core::models::task::PROPOSAL_REVIEW_TITLE_PREFIX
    );
    cases.push((
        "planner/proposal_review",
        render_prompt(AgentType::Planner, &proposal_review_task, &ctx),
    ));

    for (label, prompt) in &cases {
        assert!(
            prompt.contains(FORGE_RULE),
            "{label} prompt must state that approval and merge are {FORGE_RULE}, \
             so no role demands, accepts, or authors human-approval machinery"
        );
        assert!(
            prompt.contains("CODEOWNERS"),
            "{label} prompt must name CODEOWNERS mapping as out-of-model machinery"
        );
        assert!(
            prompt.contains("separation of duties"),
            "{label} prompt must name separation of duties as out-of-model machinery"
        );
        assert!(
            prompt.contains("runbook"),
            "{label} prompt must route a required human approval to a runbook \
             instead of an acceptance criterion"
        );
    }
}

#[test]
fn judge_prompt_contains_adjudication_contract() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Judge, &task, &ctx);

    assert!(
        prompt.contains("Judge") || prompt.contains("judge"),
        "judge prompt should identify the Judge role"
    );
    assert!(
        prompt.contains("adjudicate") || prompt.contains("verdict") || prompt.contains("approve"),
        "judge prompt should mention adjudication or verdict"
    );
    assert!(
        prompt.contains("dry") || prompt.contains("no new blocking"),
        "judge prompt should reference the Adversary dry condition"
    );
    assert!(
        prompt.contains("independent"),
        "judge prompt should emphasize independence"
    );
    assert!(
        !prompt.contains("{{"),
        "judge prompt should have no unresolved placeholders"
    );
}

/// The Judge prompt must contain the DoR-blocking rule that prevents autonomous
/// approval when deterministic Definition of Ready status is failing. This
/// contract test fails if the rule is silently removed.
#[test]
fn judge_prompt_blocks_on_failing_dor_status() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Judge, &task, &ctx);

    // The prompt must call out the injected DoR status field.
    assert!(
        prompt.contains("Current DoR status"),
        "judge prompt must reference the injected 'Current DoR status'"
    );

    // Any non-clean/non-pass status is treated as blocking.
    assert!(
        prompt.contains("anything other than the clean/pass message")
            || prompt.contains("anything other than `Proposal currently meets all DoR checks.`"),
        "judge prompt must treat any non-clean DoR status as blocking"
    );

    // Failing DoR requires blocking=true in the verdict.
    assert!(
        prompt.contains("blocking=true"),
        "judge prompt must require blocking=true when DoR is failing"
    );

    // The verdict body must name the missing required coverage from the injected DoR status.
    assert!(
        prompt.contains("name the missing required coverage")
            || prompt.contains("missing required coverage reported by the injected DoR status"),
        "judge prompt must require the verdict body to name the missing required coverage from the DoR status"
    );

    // The Judge must not file an approve/ready verdict while DoR is failing.
    assert!(
        prompt.contains("must not file an approve/ready verdict")
            || prompt.contains("do not file an approve/ready verdict")
            || prompt.contains("You must not file an approve/ready verdict"),
        "judge prompt must forbid approve/ready verdict while DoR is failing"
    );
}

#[test]
fn tribunal_roles_are_not_routed_through_reviewer() {
    // Verify tribunal roles have distinct config names, not aliases for "reviewer".
    let advocate_cfg = AgentType::Advocate.role_config();
    let adversary_cfg = AgentType::Adversary.role_config();
    let judge_cfg = AgentType::Judge.role_config();

    assert_eq!(advocate_cfg.name, "advocate");
    assert_eq!(adversary_cfg.name, "adversary");
    assert_eq!(judge_cfg.name, "judge");

    assert_ne!(advocate_cfg.name, "reviewer");
    assert_ne!(adversary_cfg.name, "reviewer");
    assert_ne!(judge_cfg.name, "reviewer");

    // Dispatch roles must also be distinct.
    assert_eq!(advocate_cfg.dispatch_role, "advocate");
    assert_eq!(adversary_cfg.dispatch_role, "adversary");
    assert_eq!(judge_cfg.dispatch_role, "judge");
}

// ── sa4x: CI blocking directive tests ──────────────────────────────────────

#[test]
fn ci_blocking_directive_rendered_when_present() {
    let task = make_task();
    let ctx = TaskContext {
        ci_blocking_directive: Some(
            "**PR:** #42\n\
             **Failing head SHA:** `abc123`\n\
             **Blocking checks:** Quality Gate\n\n\
             > REQUIRED CI is failing on the current PR head."
                .into(),
        ),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("## ⛔ BLOCKING: Required CI Failing"),
        "prompt should include the BLOCKING section heading"
    );
    assert!(
        prompt.contains("**PR:** #42"),
        "prompt should include the concrete PR number"
    );
    assert!(
        prompt.contains("**Failing head SHA:** `abc123`"),
        "prompt should include the failing head SHA"
    );
    assert!(
        prompt.contains("**Blocking checks:** Quality Gate"),
        "prompt should include the blocking check names"
    );
    assert!(
        !prompt.contains("{{ci_blocking_directive_section}}"),
        "template placeholder should be replaced"
    );
}

#[test]
fn ci_blocking_directive_omitted_when_absent() {
    let task = make_task();
    let ctx = make_ctx(); // ci_blocking_directive: None
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        !prompt.contains("BLOCKING: Required CI Failing"),
        "prompt should not include BLOCKING section when directive is None"
    );
    assert!(
        !prompt.contains("{{ci_blocking_directive_section}}"),
        "placeholder should be replaced with empty string"
    );
}

#[test]
fn ci_blocking_directive_appears_for_reviewer_role() {
    let task = make_task();
    let ctx = TaskContext {
        ci_blocking_directive: Some(
            "**PR:** #99\n\
             **Failing head SHA:** `def456`\n\
             **Blocking checks:** unit tests, lint\n\n\
             > REQUIRED CI is failing on the current PR head."
                .into(),
        ),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

    assert!(
        prompt.contains("## ⛔ BLOCKING: Required CI Failing"),
        "reviewer prompt should include the BLOCKING section"
    );
    assert!(
        prompt.contains("**PR:** #99"),
        "reviewer prompt should include the concrete PR number"
    );
    assert!(
        prompt.contains("**Blocking checks:** unit tests, lint"),
        "reviewer prompt should include the blocking check names"
    );
}

#[test]
fn ci_blocking_directive_omitted_for_empty_string() {
    let task = make_task();
    let ctx = TaskContext {
        ci_blocking_directive: Some("   ".into()),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        !prompt.contains("BLOCKING: Required CI Failing"),
        "prompt should not include BLOCKING section for whitespace-only directive"
    );
}

#[test]
fn ci_blocking_directive_does_not_appear_in_activity_section() {
    let task = make_task();
    let ctx = TaskContext {
        ci_blocking_directive: Some("**PR:** #42\n**Failing head SHA:** `abc123`".into()),
        activity: Some("Some activity log text".into()),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    // The directive should be in its own section, not duplicated in the activity log
    let blocking_count = prompt
        .matches("## ⛔ BLOCKING: Required CI Failing")
        .count();
    assert_eq!(
        blocking_count, 1,
        "BLOCKING directive should appear exactly once, got {blocking_count}"
    );
}

// ── sa4x: Cross-role directive deduplication with concrete values ────────────
//
// AC3: These tests verify the promoted BLOCKING directive deduplication for
// worker and reviewer dispatch contexts using the same concrete PR/head/check/
// fingerprint values used across the guardrail test suite. The directive text
// must be identical for both roles when the underlying CI gate snapshot is the
// same.

/// Concrete directive text matching the sa4x guardrail test values.
/// Derived from: PR #42, head SHA abc123..., checks Quality Gate + Server Clippy,
/// fingerprint fp-e2e-sa4x-regression, base SHA abc123...
const SA4X_CONCRETE_DIRECTIVE: &str = "**PR:** #42\n\
    **Failing head SHA:** `abc123def456789012345678901234567890abcd`\n\
    **Blocking checks:** Quality Gate, Server Clippy\n\
    **Failure fingerprint:** `fp-e2e-sa4x-regression`\n\
    **Remediation baseline SHA:** `abc123def456789012345678901234567890abcd`\n\n\
    > REQUIRED CI is failing on the current PR head. You MUST fix the \
    failing required checks listed above before this task can proceed. \
    The task will remain in remediation until all blocking checks pass \
    on a new commit pushed to the PR branch.";

/// AC3: Worker dispatch context renders the BLOCKING directive with all
/// concrete values from the durable CI gate snapshot.
#[test]
fn sa4x_worker_prompt_renders_concrete_blocking_directive() {
    let task = make_task();
    let ctx = TaskContext {
        ci_blocking_directive: Some(SA4X_CONCRETE_DIRECTIVE.into()),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("## ⛔ BLOCKING: Required CI Failing"),
        "worker prompt must contain the BLOCKING section heading"
    );
    assert!(
        prompt.contains("**PR:** #42"),
        "worker prompt must contain the concrete PR number"
    );
    assert!(
        prompt.contains("`abc123def456789012345678901234567890abcd`"),
        "worker prompt must contain the failing head SHA"
    );
    assert!(
        prompt.contains("Quality Gate"),
        "worker prompt must contain blocking check 'Quality Gate'"
    );
    assert!(
        prompt.contains("Server Clippy"),
        "worker prompt must contain blocking check 'Server Clippy'"
    );
    assert!(
        prompt.contains("fp-e2e-sa4x-regression"),
        "worker prompt must contain the failure fingerprint"
    );
    assert!(
        prompt.contains("REQUIRED CI is failing"),
        "worker prompt must contain the blocking instruction"
    );
    // Exactly one BLOCKING section — no duplication.
    assert_eq!(
        prompt
            .matches("## ⛔ BLOCKING: Required CI Failing")
            .count(),
        1,
        "BLOCKING directive must appear exactly once in worker prompt"
    );
}

/// AC3: Reviewer dispatch context renders the identical BLOCKING directive
/// with the same concrete values. The directive text is the same because
/// it's derived from the durable snapshot, not from role-specific logic.
#[test]
fn sa4x_reviewer_prompt_renders_concrete_blocking_directive() {
    let task = make_task();
    let ctx = TaskContext {
        ci_blocking_directive: Some(SA4X_CONCRETE_DIRECTIVE.into()),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

    assert!(
        prompt.contains("## ⛔ BLOCKING: Required CI Failing"),
        "reviewer prompt must contain the BLOCKING section heading"
    );
    assert!(
        prompt.contains("**PR:** #42"),
        "reviewer prompt must contain the concrete PR number"
    );
    assert!(
        prompt.contains("`abc123def456789012345678901234567890abcd`"),
        "reviewer prompt must contain the failing head SHA"
    );
    assert!(
        prompt.contains("Quality Gate"),
        "reviewer prompt must contain blocking check 'Quality Gate'"
    );
    assert!(
        prompt.contains("Server Clippy"),
        "reviewer prompt must contain blocking check 'Server Clippy'"
    );
    assert!(
        prompt.contains("fp-e2e-sa4x-regression"),
        "reviewer prompt must contain the failure fingerprint"
    );
    // Exactly one BLOCKING section — no duplication.
    assert_eq!(
        prompt
            .matches("## ⛔ BLOCKING: Required CI Failing")
            .count(),
        1,
        "BLOCKING directive must appear exactly once in reviewer prompt"
    );
}

/// AC3: Deduplication verification — the same directive text rendered in
/// both worker and reviewer prompts produces the same BLOCKING section.
/// This is by construction (same input → same output) but we verify it
/// explicitly to guard against role-specific injection bugs.
#[test]
fn sa4x_directive_text_identical_across_worker_and_reviewer() {
    let task = make_task();
    let ctx = TaskContext {
        ci_blocking_directive: Some(SA4X_CONCRETE_DIRECTIVE.into()),
        ..make_ctx()
    };
    let worker_prompt = render_prompt(AgentType::Worker, &task, &ctx);
    let reviewer_prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

    // Both prompts must contain the directive.
    assert!(worker_prompt.contains("## ⛔ BLOCKING: Required CI Failing"));
    assert!(reviewer_prompt.contains("## ⛔ BLOCKING: Required CI Failing"));

    // Extract the directive body (up to the next ## heading or end of prompt).
    fn extract_blocking_body(prompt: &str) -> String {
        let start = prompt
            .find("## ⛔ BLOCKING: Required CI Failing")
            .expect("must have BLOCKING section");
        let rest = &prompt[start..];
        // Find the next section heading after BLOCKING.
        match rest[3..].find("\n## ") {
            Some(end) => rest[..3 + end].to_string(),
            None => rest.to_string(),
        }
    }

    let worker_body = extract_blocking_body(&worker_prompt);
    let reviewer_body = extract_blocking_body(&reviewer_prompt);

    assert_eq!(
        worker_body, reviewer_body,
        "BLOCKING directive body must be identical for worker and reviewer"
    );
}

// ── Worker resume note (y8pv / 48ru) ───────────────────────────────────────

#[test]
fn worker_resume_section_appears_when_note_present() {
    let task = make_task();
    let ctx = TaskContext {
        worker_resume_note: Some(
            "**Resuming from prior session.** prior session `s1`; checkpoint `abc123`; \
             terminated: no-progress checkpoint; prev model `anthropic/claude-opus-4.7`; \
             last progress: Implemented core feature; verify: `cargo test`"
                .into(),
        ),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("## Resume Context"),
        "worker prompt should include Resume Context section when note is present"
    );
    assert!(prompt.contains("Resuming from prior session"));
    assert!(prompt.contains("abc123"));
    assert!(prompt.contains("claude-opus-4.7"));
    assert!(
        !prompt.contains("{{worker_resume_section}}"),
        "placeholder should be replaced"
    );
}

#[test]
fn worker_resume_section_omitted_when_note_absent() {
    let task = make_task();
    let ctx = make_ctx(); // worker_resume_note: None
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        !prompt.contains("## Resume Context"),
        "prompt should not include Resume Context section when note is None"
    );
    assert!(
        !prompt.contains("{{worker_resume_section}}"),
        "placeholder should be replaced with empty string"
    );
}

#[test]
fn worker_resume_section_omitted_when_note_is_whitespace() {
    let task = make_task();
    let ctx = TaskContext {
        worker_resume_note: Some("   ".into()),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        !prompt.contains("## Resume Context"),
        "prompt should not include Resume Context for whitespace-only note"
    );
}

// ── Arbiter directive rendering (zkk9 / monitored reopen) ──────────────────

#[test]
fn arbiter_directive_injected_into_worker_prompt() {
    let task = make_task();
    let ctx = TaskContext {
        arbiter_directive: Some("Fix the retry loop in dispatch.rs".into()),
        ci_adjudication_bundle: None,
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("Arbiter Directive"),
        "worker prompt should include Arbiter Directive section when directive is present"
    );
    assert!(
        prompt.contains("Fix the retry loop in dispatch.rs"),
        "worker prompt should contain the directive text verbatim"
    );
    assert!(
        !prompt.contains("{{arbiter_directive_section}}"),
        "placeholder should be replaced"
    );
}

#[test]
fn arbiter_directive_omitted_when_none() {
    let task = make_task();
    let ctx = make_ctx(); // arbiter_directive: None
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        !prompt.contains("Arbiter Directive"),
        "prompt should not include Arbiter Directive section when directive is None"
    );
    assert!(
        !prompt.contains("{{arbiter_directive_section}}"),
        "placeholder should be replaced with empty string"
    );
}

#[test]
fn arbiter_directive_omitted_when_whitespace() {
    let task = make_task();
    let ctx = TaskContext {
        arbiter_directive: Some("   ".into()),
        ci_adjudication_bundle: None,
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        !prompt.contains("Arbiter Directive"),
        "prompt should not include Arbiter Directive for whitespace-only text"
    );
}

#[test]
fn arbiter_directive_not_injected_into_planner_prompt() {
    // The directive is gated to worker-only at the prompt-context layer
    // (load_arbiter_directive returns None for non-worker roles), so a
    // planner prompt with arbiter_directive=None should never contain it.
    let task = make_task();
    let ctx = make_ctx(); // arbiter_directive: None (non-worker)
    let prompt = render_prompt(AgentType::Planner, &task, &ctx);

    assert!(
        !prompt.contains("Arbiter Directive"),
        "planner prompt must not include Arbiter Directive section"
    );
}

#[test]
fn arbiter_directive_not_injected_into_reviewer_prompt() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

    assert!(
        !prompt.contains("Arbiter Directive"),
        "reviewer prompt must not include Arbiter Directive section"
    );
}

// ── Attempt history rendering (2v3k) ────────────────────────────────────────

/// When activity_text includes attempt history, it renders inside the Activity
/// Log section — not as a new top-level section.
#[test]
fn worker_prompt_includes_attempt_history_inside_activity_section() {
    let task = make_task();
    let activity_with_attempts = Some(
        "Some activity log text\n\n---\n\n\
         **Prior attempts (newest first):**\n\
         - Attempt #1 (worker): crashed\n\
           created: 2026-01-01T00:00:00Z\n\
           terminal: 2026-01-01T01:00:00Z\n\
           summary: attempt crashed (no summary recorded)\n\
           submit_ref: `abc123`\n\
           PR: https://github.com/pr/1"
            .into(),
    );
    let ctx = TaskContext {
        activity: activity_with_attempts,
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    // Attempt history should appear inside the Activity Log section.
    assert!(
        prompt.contains("**Prior attempts (newest first):**"),
        "prompt should contain attempt history inside activity section"
    );
    assert!(
        prompt.contains("Attempt #1 (worker): crashed"),
        "prompt should contain the attempt entry"
    );
    assert!(
        prompt.contains("submit_ref: `abc123`"),
        "prompt should contain the submit_ref"
    );
    // Should NOT be a separate top-level section.
    assert!(
        !prompt.contains("## Prior Attempts"),
        "attempt history should not be a separate top-level heading"
    );
}

/// When activity_text is None, the prompt should not have the Activity Log
/// section or attempt history.
#[test]
fn worker_prompt_omits_attempt_history_when_no_activity() {
    let task = make_task();
    let ctx = make_ctx(); // activity: None
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        !prompt.contains("Prior attempts"),
        "prompt should not contain attempt history when no activity"
    );
}

/// Attempt history with summary_json fields renders inside the activity section.
#[test]
fn worker_prompt_includes_attempt_history_with_summary_json_fields() {
    let task = make_task();
    let activity_with_attempts = Some(
        "**Prior attempts (newest first):**\n\
         - Attempt #1 (worker): completed\n\
           created: 2026-01-01T00:00:00Z\n\
           summary: done\n\
           failure_class: compile_error\n\
           last_verify: cargo clippy"
            .into(),
    );
    let ctx = TaskContext {
        activity: activity_with_attempts,
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(prompt.contains("failure_class: compile_error"));
    assert!(prompt.contains("last_verify: cargo clippy"));
}

// ── Attempt history budget, redaction, and dedup in prompt (16vq) ─────────────

/// Attempt history with a truncation note renders inside the Activity Log.
#[test]
fn worker_prompt_includes_attempt_history_truncation_note() {
    let task = make_task();
    let activity_with_truncation = Some(
        "**Prior attempts (newest first):**\n\
         - Attempt #2 (worker): completed\n\
           summary: newest\n\
         \n\
         [... older attempt entries dropped to fit feedback budget ...]"
            .into(),
    );
    let ctx = TaskContext {
        activity: activity_with_truncation,
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("dropped to fit feedback budget"),
        "prompt should contain truncation note"
    );
    assert!(
        prompt.contains("Attempt #2 (worker): completed"),
        "prompt should contain the surviving attempt"
    );
}

/// Deduped attempt history with placeholder renders correctly in prompt.
#[test]
fn worker_prompt_renders_deduped_attempt_history() {
    let task = make_task();
    let activity_with_dedup = Some(
        "**Prior attempts (newest first):**\n\
         - Attempt #1 (worker): reopened\n\
           created: 2026-01-01T00:00:00Z\n\
           summary: (see rejection/feedback above)\n\
           submit_ref: `abc123`"
            .into(),
    );
    let ctx = TaskContext {
        activity: activity_with_dedup,
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("(see rejection/feedback above)"),
        "prompt should contain dedup placeholder"
    );
    assert!(
        prompt.contains("submit_ref: `abc123`"),
        "prompt should still contain refs"
    );
}

/// Attempt history with redacted summary renders in prompt.
#[test]
fn worker_prompt_renders_redacted_attempt_summary() {
    let task = make_task();
    let activity_with_redacted = Some(
        "**Prior attempts (newest first):**\n\
         - Attempt #1 (worker): crashed\n\
           summary: thread 'main' panicked at src/main.rs:42"
            .into(),
    );
    let ctx = TaskContext {
        activity: activity_with_redacted,
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("panicked at"),
        "prompt should contain the redacted panic message"
    );
    assert!(
        !prompt.contains("RUST_BACKTRACE="),
        "prompt should not contain raw backtrace"
    );
}

/// Attempt history section does not render as a separate top-level heading.
#[test]
fn attempt_history_is_not_separate_top_level_section() {
    let task = make_task();
    let activity_with_attempts = Some(
        "Some activity text\n\n---\n\n\
         **Prior attempts (newest first):**\n\
         - Attempt #1 (worker): completed\n\
           summary: done"
            .into(),
    );
    let ctx = TaskContext {
        activity: activity_with_attempts,
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    // Should be inside the Activity Log section, not a separate section.
    assert!(
        !prompt.contains("## Prior Attempts"),
        "attempt history should not be a separate top-level heading"
    );
    assert!(
        prompt.contains("**Prior attempts (newest first):**"),
        "attempt history should appear inside activity section"
    );
}

// ── Lead prompt: forensic arbiter mandate regression (10qg) ────────────

/// The Lead prompt must describe the forensic arbiter mandate after the
/// role/tool/model-policy cut-over.  These assertions pin the prompt
/// content so a future regression cannot silently revert the Lead to a
/// generic intervention handler.
#[test]
fn lead_prompt_contains_forensic_arbiter_mandate() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Lead, &task, &ctx);

    assert!(
        prompt.contains("Forensic Arbiter"),
        "lead prompt must declare the forensic arbiter mandate"
    );
    assert!(
        prompt.contains("Evidence-Gated Decisions"),
        "lead prompt must contain the evidence-gated decisions section"
    );
    assert!(
        prompt.contains("submit_decision"),
        "lead prompt must reference submit_decision as the finalize tool"
    );
    assert!(
        prompt.contains("verifiable evidence"),
        "lead prompt must require verifiable evidence for decisions"
    );
    assert!(
        prompt.contains("dossier"),
        "lead prompt must reference dossier for park decisions"
    );
    assert!(
        prompt.contains("directive"),
        "lead prompt must reference directive for reopen decisions"
    );
    assert!(
        prompt.contains("verification_command"),
        "lead prompt must reference verification_command for reopen"
    );
    assert!(
        prompt.contains("supersede"),
        "lead prompt must reference the supersede decision"
    );
    assert!(
        prompt.contains("created_tasks"),
        "lead prompt must reference created_tasks for supersede decisions"
    );
}

/// The Lead prompt's Decision Matrix must present five decisions including the
/// new `supersede` rung, and must steer the arbiter toward supersede (not park)
/// once it has produced replacement subtasks.
#[test]
fn lead_prompt_decision_matrix_includes_supersede_and_prefers_it_over_park() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Lead, &task, &ctx);

    assert!(
        prompt.contains("five possible decisions"),
        "decision matrix must declare five possible decisions"
    );
    assert!(
        prompt.contains("Supersede (`decision=\"supersede\"`)"),
        "decision matrix must contain the Supersede entry"
    );
    // Park must be explicitly scoped to the no-autonomous-resolution case so the
    // arbiter stops parking pure-administration decompositions (the zcsl bug).
    assert!(
        prompt.contains("no autonomous resolution exists even in principle"),
        "park entry must scope park to the no-autonomous-resolution case"
    );
}

/// The Lead prompt must NOT surface `request_planner` or `escalate` as
/// tools the Lead agent should call — the Lead only uses `submit_decision`.
#[test]
fn lead_prompt_does_not_mention_request_planner_or_escalate_as_decision_tools() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Lead, &task, &ctx);

    // The lead prompt must not instruct the agent to call request_planner.
    // It is OK for the prompt to mention "planner" in context (e.g. explaining
    // the board), but not as a tool call instruction.
    assert!(
        !prompt.contains("`request_planner`"),
        "lead prompt must NOT reference request_planner as a callable tool"
    );
    assert!(
        !prompt.contains("`escalate`"),
        "lead prompt must NOT reference escalate as a decision option"
    );
}

/// The Lead finalize tool config must be `submit_decision` only —
/// no `request_planner`, no `request_lead`, no `escalate`.
#[test]
fn lead_config_finalize_tool_is_submit_decision_only() {
    use crate::config::LEAD_CONFIG;

    assert_eq!(
        LEAD_CONFIG.finalize_tool_names,
        &["submit_decision"],
        "lead finalize tool must be submit_decision only, got: {:?}",
        LEAD_CONFIG.finalize_tool_names
    );
}

/// Worker finalize tools must include `request_planner` and must NOT
/// include `request_lead`.
#[test]
fn worker_config_finalize_tools_include_request_planner_not_request_lead() {
    use crate::config::WORKER_CONFIG;

    assert!(
        WORKER_CONFIG
            .finalize_tool_names
            .contains(&"request_planner"),
        "worker finalize tools must include request_planner, got: {:?}",
        WORKER_CONFIG.finalize_tool_names
    );
    assert!(
        !WORKER_CONFIG.finalize_tool_names.contains(&"request_lead"),
        "worker finalize tools must NOT include request_lead, got: {:?}",
        WORKER_CONFIG.finalize_tool_names
    );
}

/// Reviewer finalize tools must include `request_planner` and must NOT
/// include `request_lead`.
#[test]
fn reviewer_config_finalize_tools_include_request_planner_not_request_lead() {
    use crate::config::REVIEWER_CONFIG;

    assert!(
        REVIEWER_CONFIG
            .finalize_tool_names
            .contains(&"request_planner"),
        "reviewer finalize tools must include request_planner, got: {:?}",
        REVIEWER_CONFIG.finalize_tool_names
    );
    assert!(
        !REVIEWER_CONFIG
            .finalize_tool_names
            .contains(&"request_lead"),
        "reviewer finalize tools must NOT include request_lead, got: {:?}",
        REVIEWER_CONFIG.finalize_tool_names
    );
}

// ── Phase 1 c4r6: memory_search query-style contract regression ─────────────

/// Assert that the shared `memory_search` query-formulation contract is present
/// in a prompt. This pins the Phase 1 text-only contract: declarative (not
/// question/interrogative), one self-contained information need per query, no
/// retrieval-meta phrases (`find`, `information about`, `search for`), preserve
/// discriminative symbol names / exact errors / config keys, and the
/// lexical/BM25-until-72iu caveat.
fn assert_memory_search_contract(prompt: &str, source: &str) {
    let header = "**`memory_search` query contract:**";
    assert!(
        prompt.contains(header),
        "{source} must declare the memory_search query contract header"
    );

    // Require complete directive-bearing clauses rather than loose keywords:
    // this catches reversed prohibitions such as "use question wording".
    for required in [
        "Formulate each query as a declarative, self-contained statement of one information need.",
        "Do not use question wording or retrieval-meta phrases such as `find`, `information about`, or `search for`.",
        "Preserve discriminative symbol names, exact errors, and config keys.",
        "Worker-issued searches remain lexical/BM25-only until 72iu; do not assume embeddings.",
    ] {
        assert!(
            prompt.contains(required),
            "{source} memory_search contract must mention `{required}`"
        );
    }
}

/// Every prompt that operationally instructs `memory_search` usage must carry
/// the same query-style contract and lexical/BM25-until-72iu caveat. This test
/// covers both rendered role prompts (tool schemas are empty so this is
/// text-only) and the source `proposal_address.md` chat prompt.
#[test]
fn memory_search_contract_present_in_all_operational_prompts() {
    let task = make_task();
    let ctx = make_ctx();

    assert_memory_search_contract(
        &render_prompt(AgentType::Worker, &task, &ctx),
        "dev.md (worker default)",
    );

    let mut research_task = make_task();
    research_task.issue_type = "research".into();
    assert_memory_search_contract(
        &render_prompt(AgentType::Worker, &research_task, &ctx),
        "worker/research.md",
    );

    assert_memory_search_contract(
        &render_prompt(AgentType::Architect, &task, &ctx),
        "architect.md",
    );

    assert_memory_search_contract(
        &render_prompt(AgentType::Reviewer, &task, &ctx),
        "task-reviewer.md",
    );

    let mut planning_task = make_task();
    planning_task.issue_type = "planning".into();
    assert_memory_search_contract(
        &render_prompt(AgentType::Planner, &planning_task, &ctx),
        "planner/decomposition.md",
    );

    let mut proposal_task = make_task();
    proposal_task.issue_type = "epic_breakdown".into();
    assert_memory_search_contract(
        &render_prompt(AgentType::Planner, &proposal_task, &ctx),
        "planner/proposal.md",
    );

    assert_memory_search_contract(include_str!("proposal_address.md"), "proposal_address.md");
}

// ── u46i R3: the injected-knowledge pull scaffold ──────────────────────────────

/// The fixture note rendered as the first injected entry. Its slug is unique so
/// the scaffold's own worked-example entry can never be mistaken for it.
const KNOWLEDGE_FIXTURE_ENTRY: &str = "- **[Pitfall] pitfalls/u46i-r3-fixture-entry**";

fn knowledge_fixture() -> String {
    format!(
        "{KNOWLEDGE_FIXTURE_ENTRY}: applies when the fixture condition holds\n  \
         action: … truncated; memory_read(pitfalls/u46i-r3-fixture-entry)"
    )
}

/// Render a worker prompt with injected notes and return only the scaffold —
/// the text between the `## Relevant Knowledge` header and the first injected
/// entry. Asserting on this slice proves the scaffold *precedes* the notes
/// rather than merely appearing somewhere in the prompt.
fn render_knowledge_scaffold() -> String {
    let task = make_task();
    let mut ctx = make_ctx();
    ctx.knowledge_context = Some(knowledge_fixture());
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    let Some(section_start) = prompt.find("## Relevant Knowledge") else {
        panic!("the knowledge section must render when knowledge_context is present");
    };
    let Some(notes_offset) = prompt[section_start..].find(KNOWLEDGE_FIXTURE_ENTRY) else {
        panic!("the injected notes must render inside the knowledge section");
    };
    prompt[section_start..section_start + notes_offset].to_string()
}

/// R3 element 1 — coverage map: what is in context vs. what is one call away.
#[test]
fn knowledge_scaffold_carries_a_coverage_map() {
    let scaffold = render_knowledge_scaffold();
    assert!(
        scaffold.contains("Coverage map"),
        "the coverage map must be labelled so it is identifiable: {scaffold}"
    );
    assert!(
        scaffold.contains("In context:") && scaffold.contains("one call away"),
        "the coverage map must name both sides of the boundary: {scaffold}"
    );
    assert!(
        scaffold.contains("action:") && scaffold.contains("condition under"),
        "the in-context side must name the applicability condition and action excerpt: {scaffold}"
    );
    assert!(
        scaffold.contains("reproduction steps")
            && scaffold.contains("diagnostics")
            && scaffold.contains("related notes"),
        "the one-call-away side must enumerate the full note body: {scaffold}"
    );
}

/// R3 element 2 — enumerated pull triggers.
#[test]
fn knowledge_scaffold_enumerates_pull_triggers() {
    let scaffold = render_knowledge_scaffold();
    assert!(
        scaffold.contains("Pull triggers"),
        "the trigger list must be labelled: {scaffold}"
    );
    for trigger in [
        "… truncated; memory_read(<permalink>)",
        "condition matches what you are about to do",
        "regeneration, migration, deploy",
        "CI failure",
    ] {
        assert!(
            scaffold.contains(trigger),
            "the trigger list must enumerate `{trigger}`: {scaffold}"
        );
    }
}

/// R3 element 3 — the negative list, distinguishable from the trigger list.
#[test]
fn knowledge_scaffold_carries_a_negative_list_distinct_from_the_triggers() {
    let scaffold = render_knowledge_scaffold();
    let Some(triggers_at) = scaffold.find("Pull triggers") else {
        panic!("the trigger list must exist before the negative list can be distinguished");
    };
    let Some(negatives_at) = scaffold.find("Negative list") else {
        panic!("the scaffold must carry a separately labelled negative list: {scaffold}");
    };
    assert!(
        negatives_at > triggers_at,
        "the negative list must be its own section after the triggers, not interleaved: {scaffold}"
    );
    assert!(
        scaffold.contains("do NOT pull when"),
        "the negative list must state the skip condition explicitly: {scaffold}"
    );
    for skip in [
        "already fully answers",
        "does not match this task",
        "already read that permalink",
        "just in case",
    ] {
        assert!(
            scaffold.contains(skip),
            "the negative list must enumerate `{skip}`: {scaffold}"
        );
    }
}

/// R3 element 4 — a worked example that includes the empty-result branch.
#[test]
fn knowledge_scaffold_works_an_example_including_the_empty_result_branch() {
    let scaffold = render_knowledge_scaffold();
    assert!(
        scaffold.contains("Worked example"),
        "the worked example must be labelled: {scaffold}"
    );
    assert!(
        scaffold.contains("memory_read(identifier=\"pitfalls/"),
        "the worked example must show the exact call with a real-shaped handle: {scaffold}"
    );
    assert!(
        scaffold.contains("Empty-result branch"),
        "the worked example must branch on an empty result: {scaffold}"
    );
    assert!(
        scaffold.contains("returns nothing or errors"),
        "the empty-result branch must name the empty/error outcome: {scaffold}"
    );
    assert!(
        scaffold.contains("never loop") && scaffold.contains("invent its contents"),
        "the empty-result branch must forbid retry loops and fabrication: {scaffold}"
    );
}

/// R3 element 5 — the anti-refusal clause, naming the literal refusal strings.
#[test]
fn knowledge_scaffold_names_the_literal_refusal_strings() {
    let scaffold = render_knowledge_scaffold();
    assert!(
        scaffold.contains("Anti-refusal"),
        "the anti-refusal clause must be labelled: {scaffold}"
    );
    for refusal in [
        "\"I don't have access to that note\"",
        "\"I cannot read files\"",
        "\"this appears to be truncated\"",
    ] {
        assert!(
            scaffold.contains(refusal),
            "the anti-refusal clause must name the literal string {refusal}: {scaffold}"
        );
    }
    assert!(
        scaffold.contains("Make the call first"),
        "the anti-refusal clause must state the required behaviour instead: {scaffold}"
    );
}

/// R3 element 6 — handles are copied from the index, never fabricated.
#[test]
fn knowledge_scaffold_requires_handles_to_come_from_the_index() {
    let scaffold = render_knowledge_scaffold();
    assert!(
        scaffold.contains("Handles come from this index"),
        "the handle rule must be labelled: {scaffold}"
    );
    assert!(
        scaffold.contains("copied verbatim"),
        "the handle rule must require verbatim copying: {scaffold}"
    );
    assert!(
        scaffold.contains("Never guess one") && scaffold.contains("never slugify a title"),
        "the handle rule must forbid guessing and slugifying titles: {scaffold}"
    );
    assert!(
        scaffold.contains("memory_search(query=...)"),
        "the handle rule must route uncovered needs to `memory_search`: {scaffold}"
    );
}

/// R3 element 7 — asymmetric budget: grounded pulls free, search metered.
#[test]
fn knowledge_scaffold_states_an_asymmetric_budget() {
    let scaffold = render_knowledge_scaffold();
    assert!(
        scaffold.contains("Budget is asymmetric"),
        "the budget rule must be labelled: {scaffold}"
    );
    assert!(
        scaffold.contains("are unlimited and encouraged"),
        "grounded pulls must be declared unlimited: {scaffold}"
    );
    assert!(
        scaffold.contains("metered: at most 3"),
        "speculative search must carry a concrete small budget: {scaffold}"
    );
    assert!(
        scaffold.contains("prefer a grounded pull to a speculative search"),
        "the budget rule must state the preference order: {scaffold}"
    );
}

/// The whole point of the scaffold is that it is a pointer expander — it must
/// still name the pull tool and the truncation marker it keys off.
#[test]
fn injected_knowledge_section_documents_the_memory_read_pull_path() {
    let scaffold = render_knowledge_scaffold();
    assert!(
        scaffold.contains("memory_read"),
        "the copy above injected notes must name `memory_read`: {scaffold}"
    );
    assert!(
        scaffold.contains("permalink"),
        "the copy must tell the agent to pass the shown permalink: {scaffold}"
    );
    assert!(
        scaffold.contains("truncated"),
        "the copy must explain that excerpts can be truncated: {scaffold}"
    );
}

/// The scaffold is a per-dispatch token cost. It must not render at all when
/// there are no notes to pull.
#[test]
fn knowledge_scaffold_does_not_render_without_injected_notes() {
    let task = make_task();

    for (label, knowledge) in [
        ("None", None),
        ("empty", Some(String::new())),
        ("whitespace", Some("   \n\t \n".to_string())),
    ] {
        let mut ctx = make_ctx();
        ctx.knowledge_context = knowledge;
        let prompt = render_prompt(AgentType::Worker, &task, &ctx);
        assert!(
            !prompt.contains("## Relevant Knowledge"),
            "the knowledge header must not render for {label} knowledge_context"
        );
        assert!(
            !prompt.contains("Pull triggers"),
            "the pull scaffold must not render for {label} knowledge_context"
        );
        assert!(
            !prompt.contains("Budget is asymmetric"),
            "the budget clause must not render for {label} knowledge_context"
        );
    }
}

/// Size guard: the scaffold rides on every dispatch that has notes, so it must
/// not silently bloat. Proposal u46i targets ~1.5-3 KB.
#[test]
fn knowledge_scaffold_stays_under_its_byte_ceiling() {
    /// Ceiling for the R3 pull scaffold, in bytes.
    const SCAFFOLD_BYTE_CEILING: usize = 3_200;

    let size = KNOWLEDGE_PULL_SCAFFOLD.len();
    assert!(
        size <= SCAFFOLD_BYTE_CEILING,
        "the pull scaffold is {size} bytes, over the {SCAFFOLD_BYTE_CEILING}-byte ceiling; \
         tighten it or raise the ceiling deliberately"
    );
    assert!(
        size >= 1_200,
        "the pull scaffold is only {size} bytes — it has lost required R3 elements"
    );

    // The rendered section is the scaffold plus the header and the notes; it must
    // not have grown a second copy of the scaffold anywhere.
    let scaffold = render_knowledge_scaffold();
    assert_eq!(
        scaffold.matches("Coverage map").count(),
        1,
        "the scaffold must render exactly once"
    );
}

// ── 3asv: the merge test (achievability) across the tribunal and the planner ──
//
// The acceptance-criteria rubric only ever tested *decidability* — can a domain
// outsider confirm this from its own tool surface. It never tested
// *achievability* — can the pull request that implements the work actually make
// the criterion true. A criterion can pass the first and fail the second, which
// is how a live-pod transcript and a live-data backfill both reached approval,
// were implemented, and then could not close.
//
// Each test renders the prompt and asserts on the rendered string, so every
// assertion proves the text reaches a real session rather than merely existing
// in a file.

/// The merge test's first sentence, identical in every surface that carries it.
const MERGE_TEST_PROPERTY: &str = "An acceptance criterion states a property of the merged tree.";

/// The proof clause: the merged tree, or the pull request's own CI.
const MERGE_TEST_PROOF: &str =
    "It must be provable by inspecting that tree, or by a check the pull request's own CI runs.";

/// The exclusion clause.
const MERGE_TEST_NEGATIVE: &str = "If making it true requires an execution the pull request does not perform, it is not an acceptance criterion";

/// The counterfactual, with its tense.
const MERGE_TEST_COUNTERFACTUAL: &str =
    "if this pull request merged right now, would the criterion become true?";

/// The discriminating pair's rule: a gate that exists in code is legal, an
/// observation interval is not.
const MERGE_TEST_DISCRIMINATOR: &str =
    "A gate that exists and is enforced in code passes; an observation interval fails.";

/// Disposal ladder rung 1, in the title-case form used by the surfaces that
/// render the ladder as a numbered list.
const LADDER_RUNG_1: &str = "Convert it to a check the pull request's CI runs";

/// Disposal ladder rung 2.
const LADDER_RUNG_2: &str = "Convert it to a mechanism criterion";

/// Disposal ladder rung 3.
const LADDER_RUNG_3: &str =
    "Remove it from the acceptance criteria and name where the intent was rehomed";

/// The rung order is normative, not a menu.
const LADDER_FIRST_APPLICABLE: &str = "in order and take the first applicable rung";

/// Skipping an applicable earlier rung is invalid.
const LADDER_NO_SKIP: &str = "Skipping an applicable earlier rung is invalid";

/// Rung 3 without a destination is not a disposal.
const LADDER_NAMED_DESTINATION: &str =
    "a criterion dropped without a named destination is not a valid disposal";

/// Assert the three ladder rungs render in rung order in `prompt`.
///
/// Presence alone is not enough: the whole point of the ladder is that the
/// cheapest rung is last, so an author under round pressure cannot reach for
/// deletion first. Order is the load-bearing property.
fn assert_ladder_rungs_in_order(prompt: &str, label: &str) {
    let rung_1 = prompt
        .find(LADDER_RUNG_1)
        .unwrap_or_else(|| panic!("{label} prompt must carry ladder rung 1: {LADDER_RUNG_1}"));
    let rung_2 = prompt
        .find(LADDER_RUNG_2)
        .unwrap_or_else(|| panic!("{label} prompt must carry ladder rung 2: {LADDER_RUNG_2}"));
    let rung_3 = prompt
        .find(LADDER_RUNG_3)
        .unwrap_or_else(|| panic!("{label} prompt must carry ladder rung 3: {LADDER_RUNG_3}"));

    assert!(
        rung_1 < rung_2 && rung_2 < rung_3,
        "{label} prompt must render the disposal ladder in rung order (CI check, \
         then mechanism criterion, then rehomed removal); got offsets \
         {rung_1}, {rung_2}, {rung_3}"
    );
}

/// AC1 — the Judge carries the merge test as a NAMED Definition-of-Done
/// dimension, the operator-only bullet is RELOCATED into it rather than
/// duplicated beside it, and ordered-ladder / named-destination enforcement
/// lives on the Judge's side of the loop.
#[test]
fn judge_prompt_carries_the_merge_test_definition_of_done_dimension() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Judge, &task, &ctx);

    // The dimension is NAMED, so a Judge can cite it rather than infer it.
    assert!(
        prompt.contains("Achievability — the merge test"),
        "judge prompt must name the achievability / merge-test DoD dimension"
    );

    // The rule itself.
    assert!(
        prompt.contains(MERGE_TEST_PROPERTY),
        "judge prompt must state that an AC is a property of the merged tree"
    );
    assert!(
        prompt.contains(MERGE_TEST_PROOF),
        "judge prompt must state the merged-tree-or-own-CI proof rule"
    );
    assert!(
        prompt.contains(MERGE_TEST_NEGATIVE),
        "judge prompt must exclude criteria needing an execution the PR does not perform"
    );
    assert!(
        prompt.contains(MERGE_TEST_COUNTERFACTUAL),
        "judge prompt must carry the merge-test counterfactual"
    );

    // Decidability and achievability are independent axes and the Judge must
    // apply both — the criteria that survived review passed the first cleanly.
    assert!(
        prompt.contains("a criterion must pass both"),
        "judge prompt must state that decidability and achievability are both required"
    );

    // RELOCATED, not duplicated: the phrase must occur exactly once.
    assert_eq!(
        prompt.matches("External / operator-only proofs").count(),
        1,
        "the operator-only bullet must be relocated into the merge-test dimension, \
         not duplicated beside it in the confirmability list"
    );

    // The discriminating pair: a gate enforced in code passes, an observation
    // interval fails. Both halves must render, so the rule is not applied by
    // keyword against a legitimate mechanism criterion.
    assert!(
        prompt.contains(MERGE_TEST_DISCRIMINATOR),
        "judge prompt must state that a gate enforced in code passes while an \
         observation interval fails"
    );
    assert!(
        prompt.contains(
            "New writers cannot run until all readers use the contract; rollback cannot begin until route work and provider futures are drained"
        ),
        "judge prompt must carry the passing half of the discriminating pair (a gate in code)"
    );
    assert!(
        prompt.contains("for two consecutive inventory intervals"),
        "judge prompt must carry the failing half of the discriminating pair \
         (an observation interval over a live fleet)"
    );
    assert!(
        prompt.contains("Do not pattern-match on vocabulary"),
        "judge prompt must forbid rejecting a legitimate mechanism criterion by keyword"
    );

    // Enforcing the ladder's order is the Judge's job.
    assert!(
        prompt.contains("work three rungs in order and take the **first applicable** rung"),
        "judge prompt must require the disposal ladder be applied in order, first applicable rung"
    );
    assert!(
        prompt.contains("Reject a disposal that skipped an applicable earlier rung"),
        "judge prompt must reject an out-of-order disposal"
    );
    assert!(
        prompt.contains("reject a rung 3 disposal that does not name where the intent was rehomed"),
        "judge prompt must reject an unnamed rung-3 disposal"
    );
    assert!(
        prompt.contains(LADDER_NAMED_DESTINATION),
        "judge prompt must state that a bare drop is not a valid disposal"
    );

    assert!(
        !prompt.contains("{{"),
        "judge prompt should have no unresolved placeholders"
    );
}

/// AC2 — the Advocate is the role that disposes of a failing criterion, so it
/// carries the ladder. Without it, the loop's cheapest resolution is deletion,
/// which loses the concern that motivated the criterion.
#[test]
fn advocate_prompt_carries_the_ordered_disposal_ladder() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Advocate, &task, &ctx);

    // The rule that triggers the ladder.
    assert!(
        prompt.contains(MERGE_TEST_PROPERTY) && prompt.contains(MERGE_TEST_PROOF),
        "advocate prompt must state the merge test"
    );
    assert!(
        prompt.contains(MERGE_TEST_NEGATIVE),
        "advocate prompt must exclude criteria needing an execution the PR does not perform"
    );
    assert!(
        prompt.contains(MERGE_TEST_COUNTERFACTUAL),
        "advocate prompt must carry the merge-test counterfactual"
    );

    // The ladder, in rung order.
    assert_ladder_rungs_in_order(&prompt, "advocate");

    // Ordered and first-applicable, not free choice.
    assert!(
        prompt.contains(LADDER_FIRST_APPLICABLE),
        "advocate prompt must instruct taking the first applicable rung, in order"
    );
    assert!(
        prompt.contains("This is a ladder, not a menu"),
        "advocate prompt must state the ladder is not a menu of equal options"
    );
    assert!(
        prompt.contains(LADDER_NO_SKIP),
        "advocate prompt must state that skipping an applicable earlier rung is invalid"
    );

    // Rung 3 must name where the intent went.
    assert!(
        prompt.contains(LADDER_NAMED_DESTINATION),
        "advocate prompt must require a named rehoming destination for rung 3"
    );

    assert!(
        !prompt.contains("{{"),
        "advocate prompt should have no unresolved placeholders"
    );
}

/// AC3 — the Adversary gets a merge-test objection category so the defect is
/// raised in round 1 and resolved by revision, instead of consuming a full
/// adjudication cycle. It sits ALONGSIDE the human-approval exclusion, which
/// must keep rendering unchanged.
#[test]
fn adversary_prompt_carries_the_merge_test_objection_category() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Adversary, &task, &ctx);

    // The new category.
    assert!(
        prompt.contains(
            "An acceptance criterion no pull request can satisfy is a blocking objection"
        ),
        "adversary prompt must carry the merge-test objection category"
    );
    assert!(
        prompt.contains(MERGE_TEST_PROPERTY) && prompt.contains(MERGE_TEST_PROOF),
        "adversary prompt must state the merge test"
    );
    assert!(
        prompt.contains(MERGE_TEST_NEGATIVE),
        "adversary prompt must exclude criteria needing an execution the PR does not perform"
    );
    assert!(
        prompt.contains(MERGE_TEST_COUNTERFACTUAL),
        "adversary prompt must carry the merge-test counterfactual"
    );
    assert!(
        prompt.contains("This is a **different axis** from decidability"),
        "adversary prompt must separate achievability from decidability"
    );

    // The resolution criterion must point at a ladder rung, so the objection is
    // falsifiable and cannot resolve into a silent deletion.
    assert!(
        prompt.contains("convert it to a check the pull request's CI runs")
            && prompt.contains("convert it to a mechanism criterion")
            && prompt.contains(
                "remove it from the acceptance criteria and name where the intent was rehomed"
            ),
        "adversary prompt must point the resolution criterion at the disposal ladder"
    );

    // And it must not license a manufactured objection against a legal gate.
    assert!(
        prompt.contains(MERGE_TEST_DISCRIMINATOR),
        "adversary prompt must state that a gate enforced in code passes while an \
         observation interval fails"
    );

    // The existing human-approval exclusion must still render, unchanged.
    assert!(
        prompt.contains("### Human approval and organizational structure are out of scope"),
        "the human-approval exclusion heading must still render"
    );
    assert!(
        prompt.contains("enforced by the forge and its configured owners"),
        "the human-approval exclusion must keep its forge rule"
    );
    assert!(
        prompt.contains(
            "Do not file an objection that a proposal lacks authorization, sign-off, separation of duties"
        ),
        "the human-approval exclusion must keep its prohibition"
    );
    assert!(
        prompt.contains(
            "A spec that omits those is complete, not incomplete — demanding them is a category error, not a blocking objection."
        ),
        "the human-approval exclusion must keep its category-error sentence"
    );

    assert!(
        !prompt.contains("{{"),
        "adversary prompt should have no unresolved placeholders"
    );
}

/// AC4 (proposal mode) — the Planner authors epic criteria that no tribunal
/// ever reviews, so step D4 must carry the same rule and the same ladder.
#[test]
fn planner_proposal_mode_carries_the_merge_test_and_the_ladder() {
    let mut proposal_task = make_task();
    proposal_task.issue_type = "epic_breakdown".into();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Planner, &proposal_task, &ctx);

    assert!(
        prompt.contains(MERGE_TEST_PROPERTY) && prompt.contains(MERGE_TEST_PROOF),
        "planner proposal mode must state the merge test"
    );
    assert!(
        prompt.contains(MERGE_TEST_NEGATIVE),
        "planner proposal mode must exclude criteria needing an execution the PR does not perform"
    );
    assert!(
        prompt.contains(MERGE_TEST_COUNTERFACTUAL),
        "planner proposal mode must carry the merge-test counterfactual"
    );

    assert_ladder_rungs_in_order(&prompt, "planner/proposal");

    assert!(
        prompt.contains(LADDER_FIRST_APPLICABLE),
        "planner proposal mode must instruct taking the first applicable rung, in order"
    );
    assert!(
        prompt.contains(LADDER_NO_SKIP),
        "planner proposal mode must state that skipping an applicable earlier rung is invalid"
    );
    assert!(
        prompt.contains(LADDER_NAMED_DESTINATION),
        "planner proposal mode must require a named rehoming destination for rung 3"
    );

    assert!(
        !prompt.contains("{{"),
        "planner proposal prompt should have no unresolved placeholders"
    );
}

/// AC4 (decomposition mode) — step B4 mints the task criteria that are never
/// adjudicated at all, so it carries the same rule and the same ladder.
#[test]
fn planner_decomposition_mode_carries_the_merge_test_and_the_ladder() {
    let mut decomposition_task = make_task();
    decomposition_task.issue_type = "planning".into();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Planner, &decomposition_task, &ctx);

    assert!(
        prompt.contains(MERGE_TEST_PROPERTY) && prompt.contains(MERGE_TEST_PROOF),
        "planner decomposition mode must state the merge test"
    );
    assert!(
        prompt.contains(MERGE_TEST_NEGATIVE),
        "planner decomposition mode must exclude criteria needing an execution \
         the PR does not perform"
    );
    assert!(
        prompt.contains(MERGE_TEST_COUNTERFACTUAL),
        "planner decomposition mode must carry the merge-test counterfactual"
    );

    assert_ladder_rungs_in_order(&prompt, "planner/decomposition");

    assert!(
        prompt.contains(LADDER_FIRST_APPLICABLE),
        "planner decomposition mode must instruct taking the first applicable rung, in order"
    );
    assert!(
        prompt.contains(LADDER_NO_SKIP),
        "planner decomposition mode must state that skipping an applicable earlier rung is invalid"
    );
    assert!(
        prompt.contains(LADDER_NAMED_DESTINATION),
        "planner decomposition mode must require a named rehoming destination for rung 3"
    );

    assert!(
        !prompt.contains("{{"),
        "planner decomposition prompt should have no unresolved placeholders"
    );
}

// ── AC5: what actually detects prompt-cap truncation ─────────────────────────
//
// `render_prompt_for_role` applies `MAX_SYSTEM_PROMPT_CHARS` **before** it
// returns, so a post-cap length assertion proves nothing: a truncated prompt is
// exactly as long as the cap. Only the prompt's *content* can reveal it.
//
// WHICH content is the part that is easy to get backwards. `smart_truncate`
// (`mod.rs`) preserves a HEAD and a TAIL and drops the MIDDLE. With the 48,000
// cap:
//
//     usable      = 48_000 - 80 (separator reserve) = 47_920
//     head_budget = 47_920 * 60 / 100              = 28_752   (absolute offset)
//     tail_budget = 47_920 - 28_752                = 19_168   (distance from end)
//     dropped     = [28_752, len - 19_168)
//
// The head bound is an ABSOLUTE offset and the tail bound is a DISTANCE FROM
// THE END. So a byte survives if it is either within the first 28,752 bytes or
// within the last 19,168 — and a sentinel taken from the FINAL section is, by
// construction, in the second set. It can never detect truncation. Measured on
// the current renders, the final-section sentinels sit 351 bytes (Judge) and
// 3,119 bytes (Planner decomposition) from the end, both far inside the 19,168
// always-preserved tail. AC5 names those sentinels literally, so they are
// asserted below for literal compliance — but they are NOT the guard.
//
// The two checks that can actually fire are:
//
//   1. The MIDDLE sentinel — a stable literal in the droppable region, i.e. at
//      a distance GREATER than 19,168 from the end. Only the first
//      `len - 19_168` bytes of each prompt qualify (7,468 for the Judge, 4,101
//      for Planner decomposition), which is why the sentinels chosen below are
//      early-file section headings rather than late ones.
//   2. `!prompt.contains("bytes omitted")` — `smart_truncate` always injects
//      that marker when it fires, so this catches truncation unconditionally,
//      wherever the growth lands.
//
// KEEP BOTH. They are the load-bearing assertions in these two tests; the
// final-section sentinels are the AC-literal ones and are inert at current
// sizes. Do not delete either as redundant.
//
// One honest limit: `render_prompt` here passes `Vec::new` for tool schemas, so
// these renders (~26.6K Judge, ~23.3K Planner decomposition) carry ~21-24K of
// slack against the cap. Production renders include the real tool section —
// `mod.rs` documents the decomposition prompt at ~35K — so the real headroom is
// closer to ~10K. These tests therefore under-report truncation risk by roughly
// the size of the tool section, which is inherent to testing the template layer
// in isolation.

/// AC5 — truncation sentinels for the Planner decomposition prompt.
///
/// This is the documented failure mode: the old 31K cap silently truncated the
/// tail of `planner/decomposition.md`, and the planner never saw guidance it had
/// explicitly been given.
#[test]
fn planner_decomposition_tail_survives_the_prompt_cap() {
    let mut decomposition_task = make_task();
    decomposition_task.issue_type = "planning".into();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Planner, &decomposition_task, &ctx);

    // LOAD-BEARING (1): a sentinel in the region `smart_truncate` actually
    // drops. `## Workflow B: Wave Decomposition` heads the mode section and
    // `### B1.` follows it; B1 sits at offset ~3,302, which is 19,967 bytes from
    // the end — 799 bytes past the 19,168-byte preserved tail, so it is inside
    // the droppable middle. Do not replace it with a later heading: everything
    // after offset 4,101 is in the always-preserved tail and cannot fail.
    assert!(
        prompt.contains("### B1. Orient to the Epic (keep brief)"),
        "the middle of the decomposition prompt must still render — this heading \
         sits in smart_truncate's dropped region, so losing it means prompt growth \
         pushed the render past MAX_SYSTEM_PROMPT_CHARS"
    );

    // LOAD-BEARING (2): `smart_truncate` always injects this marker when it
    // fires, so this catches truncation wherever the growth landed.
    assert!(
        !prompt.contains("bytes omitted"),
        "the decomposition prompt must not be truncated"
    );

    // AC-literal, and inert by construction: both of these sit inside the
    // always-preserved 19,168-byte tail (3,119 and 3,048 bytes from the end).
    // They are asserted because AC5 names the final stable section, not because
    // they can detect truncation.
    assert!(
        prompt.contains("### B5. Submit Planning"),
        "the final section of decomposition.md must still render"
    );
    assert!(
        prompt.contains("Wave N: created X tasks"),
        "the final instruction of decomposition.md must still render"
    );

    // The SAME render must carry the merge-test guidance, so a future addition
    // cannot buy its own visibility by evicting other guidance.
    assert!(
        prompt.contains(MERGE_TEST_PROOF),
        "the same decomposition render must carry the merge-test rule"
    );
    assert!(
        prompt.contains(LADDER_FIRST_APPLICABLE),
        "the same decomposition render must carry the ordered disposal ladder"
    );
}

/// AC5 — truncation sentinels for the Judge prompt, same mechanism: the cap is
/// applied before `render_prompt_for_role` returns, so only content can reveal
/// truncation, and only content in the dropped middle can reveal it positionally.
#[test]
fn judge_prompt_tail_survives_the_prompt_cap() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Judge, &task, &ctx);

    // LOAD-BEARING (1): a sentinel in the region `smart_truncate` actually
    // drops. This heading sits at offset ~6,020, which is 20,616 bytes from the
    // end — 1,448 bytes past the 19,168-byte preserved tail, so it is inside the
    // droppable middle. Do not replace it with a later heading: everything after
    // offset 7,468 is in the always-preserved tail and cannot fail.
    assert!(
        prompt.contains("### 2. Reject / needs-work (not ready)"),
        "the middle of the judge prompt must still render — this heading sits in \
         smart_truncate's dropped region, so losing it means prompt growth pushed \
         the render past MAX_SYSTEM_PROMPT_CHARS"
    );

    // LOAD-BEARING (2): the unconditional truncation marker.
    assert!(
        !prompt.contains("bytes omitted"),
        "the judge prompt must not be truncated"
    );

    // AC-literal, and inert by construction: both of these sit inside the
    // always-preserved 19,168-byte tail (351 and 216 bytes from the end).
    assert!(
        prompt.contains("## Session Completion"),
        "the final section of judge.md must still render"
    );
    assert!(
        prompt.contains(
            "end your session by calling `submit_decision` with a short summary of your adjudication"
        ),
        "the final instruction of judge.md must still render"
    );

    // The SAME render must carry the merge-test dimension.
    assert!(
        prompt.contains(MERGE_TEST_PROOF),
        "the same judge render must carry the merge-test rule"
    );
    assert!(
        prompt.contains("Achievability — the merge test"),
        "the same judge render must carry the named merge-test DoD dimension"
    );
}
