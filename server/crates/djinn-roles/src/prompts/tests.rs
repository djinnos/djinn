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
