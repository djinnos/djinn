// djinn:allow-oversize
use super::*;
use crate::AgentType;
use djinn_core::models::Task;

/// Render a prompt for the given agent type using empty tool schemas.
///
/// This is the djinn-roles equivalent of `djinn_agent::prompts::render_prompt`.
/// Tests that only need template content (not tool-schema content) use this.
fn render_prompt(agent_type: AgentType, task: &Task, ctx: &TaskContext) -> String {
    let config = agent_type.role_config();
    render_prompt_for_role(config, Vec::new, task, ctx)
}

fn make_task() -> Task {
    Task {
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
        created_by_user_id: None,
        ci_status: "unknown".into(),
        ci_head_sha: None,
        ci_pr_number: None,
        ci_blocking_required_check_names: "[]".into(),
        ci_failure_fingerprint: None,
        ci_first_seen_at: None,
        ci_last_seen_at: None,
        ci_same_signature_count: 0,
        ci_last_remediation_base_sha: None,
        ci_mirror_head_sha: None,
        ci_github_head_sha: None,
        ci_heads_diverged: None,
        ci_head_observation_error: None,
        unresolved_blocker_count: 0,
        total_reopen_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
    }
}

fn make_ctx() -> TaskContext {
    TaskContext {
        project_path: "/home/user/project".into(),
        workspace_path: "/home/user/project/.djinn/worktrees/t123".into(),
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

/// The amendment guidance must appear in every Planner mode (the section is in
/// the top-level planner.md template, injected for all issue types), not just
/// one workflow. Spot-check decomposition, intervention, and proposal modes.
#[test]
fn planner_learned_prompt_guidance_present_across_modes() {
    let ctx = make_ctx();

    for issue_type in ["planning", "review", "epic_breakdown", "task"] {
        let mut task = make_task();
        task.issue_type = issue_type.into();
        let prompt = render_prompt(AgentType::Planner, &task, &ctx);
        assert!(
            prompt.contains("Learned-prompt amendments"),
            "planner prompt for issue_type={issue_type} should include the learned-prompt amendment section"
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
