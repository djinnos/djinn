use super::*;

/// G5: with progressive disclosure OFF (the default), `apply_skills`
/// inlines every skill's full body — byte-identical to the pre-G5 form.
#[test]
fn apply_skills_off_inlines_full_content() {
    if std::env::var_os("DJINN_PROGRESSIVE_SKILLS").is_some() {
        return; // env-driven default under test; don't race an external override
    }
    let skills = vec![
        crate::skills::ResolvedSkill {
            name: "alpha".into(),
            description: "Alpha desc".into(),
            content: "Alpha body.".into(),
            required: false,
            trust_level: "project".into(),
            recommended_for_roles: vec![],
            tags: vec![],
        },
        crate::skills::ResolvedSkill {
            name: "beta".into(),
            description: "Beta desc".into(),
            content: "Beta body.".into(),
            required: true,
            trust_level: "project".into(),
            recommended_for_roles: vec![],
            tags: vec![],
        },
    ];
    let out = apply_skills("BASE", &skills);
    assert_eq!(
        out,
        concat!(
            "BASE\n\n",
            "## Available Skills\n\n",
            "**alpha**: Alpha desc\n\n",
            "Alpha body.\n\n",
            "**beta**: Beta desc\n\n",
            "Beta body."
        )
    );
    // No disclosure note when OFF.
    assert!(!out.contains("skill_read"));
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
        verification_failure_count: 0,
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
        total_verification_failure_count: 0,
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
        verification_commands: None,
        verification_rules: None,
        activity: None,
        worker_summary: None,
        worker_concerns: None,
        verification_failure: None,
        epic_context: None,
        knowledge_context: None,
        code_graph_context: None,
        reviewer_diff_context: None,
    }
}

#[test]
fn worker_prompt_contains_task_fields() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(prompt.contains("task-123"));
    assert!(prompt.contains("Add widget"));
    assert!(prompt.contains("Implement the widget feature."));
    assert!(prompt.contains("Use the widget pattern."));
    assert!(prompt.contains("wave:1"));
    assert!(prompt.contains("- [ ] Widget exists"));
    assert!(prompt.contains("- [x] Tests pass"));
    assert!(prompt.contains("/home/user/project"));
    assert!(prompt.contains("/home/user/project/.djinn/worktrees/t123"));
    assert!(prompt.contains("memory_write"));
    assert!(prompt.contains("memory_edit"));
    // A plain `task` runs the implement flow — no research/spike section.
    assert!(!prompt.contains("Research Deliverable"));
    // No un-substituted placeholders
    assert!(!prompt.contains("{{"));
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
fn task_reviewer_prompt_contains_task_fields() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

    // Task ID and title are substituted.
    assert!(prompt.contains(&task.id));
    // The reviewer is instructed to run git diff itself, not receive it injected.
    assert!(prompt.contains("git diff"));
    // Reviewer uses the role-specific finalize tool for verdict.
    assert!(prompt.contains("submit_review"));
    assert!(!prompt.contains("{{"));
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
        verification_commands: Some("- `npm test`".into()),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(prompt.contains("Automated Commands"));
    assert!(prompt.contains("Do not run them yourself"));
    assert!(prompt.contains("npm install"));
    assert!(!prompt.contains("{{setup_commands_section}}"));
    assert!(!prompt.contains("{{verification_section}}"));
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
fn reviewer_prompt_includes_verification_section_when_present() {
    let task = make_task();
    let ctx = TaskContext {
        diff: Some("+ fn foo() {}".into()),
        commits: Some("abc1234 Add widget".into()),
        start_commit: Some("abc0000".into()),
        end_commit: Some("abc1234".into()),
        verification_commands: Some("- `cargo test`".into()),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

    assert!(prompt.contains("Automated Verification"));
    assert!(prompt.contains("Focus on acceptance criteria"));
    assert!(prompt.contains("cargo test"));
    assert!(!prompt.contains("{{verification_section}}"));
}

#[test]
fn reviewer_prompt_omits_verification_section_when_no_commands() {
    let task = make_task();
    let ctx = TaskContext {
        diff: Some("+ fn foo() {}".into()),
        commits: Some("abc1234 Add widget".into()),
        start_commit: Some("abc0000".into()),
        end_commit: Some("abc1234".into()),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Reviewer, &task, &ctx);

    assert!(!prompt.contains("Automated Verification"));
    assert!(!prompt.contains("{{verification_section}}"));
}

#[test]
fn system_prompt_truncated_when_exceeding_hard_cap() {
    let task = make_task();
    // Inject a massive activity log that blows past the 30k char cap.
    let huge_activity = "x".repeat(40_000);
    let ctx = TaskContext {
        activity: Some(huge_activity),
        ..make_ctx()
    };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.len() <= super::MAX_SYSTEM_PROMPT_CHARS + 200, // +200 for the truncation notice
        "prompt should be truncated to ~30k chars, got {}",
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
fn worker_prompt_includes_verification_rules_when_present() {
    let task = make_task();
    let ctx = TaskContext {
            verification_rules: Some(
                "- `crates/djinn-control-plane/**`: `cargo test -p djinn-control-plane`, `cargo clippy -p djinn-control-plane -- -D warnings`".into(),
            ),
            ..make_ctx()
        };
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("Verification Rules"),
        "worker prompt should include verification rules section heading"
    );
    assert!(
        prompt.contains("crates/djinn-control-plane/**"),
        "worker prompt should include rule pattern"
    );
    assert!(
        prompt.contains("cargo test -p djinn-control-plane"),
        "worker prompt should include rule commands"
    );
    assert!(!prompt.contains("{{verification_rules_section}}"));
}

#[test]
fn worker_prompt_omits_verification_rules_when_empty() {
    let task = make_task();
    let ctx = make_ctx(); // verification_rules: None
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        !prompt.contains("Verification Rules"),
        "worker prompt should not include verification rules section when empty"
    );
    assert!(!prompt.contains("{{verification_rules_section}}"));
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

#[derive(Debug)]
struct ExternallyBlockedCriterion {
    criterion: &'static str,
    missing_access: &'static str,
}

#[derive(Debug)]
struct Planner4lzxExternallyBlockedReplay {
    epic_id: &'static str,
    open_planning_task_id: &'static str,
    remaining_criteria: Vec<ExternallyBlockedCriterion>,
}

#[derive(Debug, PartialEq, Eq)]
enum PlannerReplayAction {
    RepairCriteriaWithTaskUpdate,
    AddPruningRationaleComment,
    CloseEpic,
    SubmitGroomingClose,
}

impl Planner4lzxExternallyBlockedReplay {
    fn synthetic_4lzx_externally_blocked_fixture() -> Self {
        Self {
            epic_id: "4lzx-epic",
            open_planning_task_id: "4lzx-planning",
            remaining_criteria: vec![
                ExternallyBlockedCriterion {
                    criterion: "Run the Docker/Postgres integration stack and prove migrations succeed against a live database.",
                    missing_access: "Docker and Postgres are unavailable to task-run pods",
                },
                ExternallyBlockedCriterion {
                    criterion: "Deploy the operator to Kubernetes and verify the rollout from cluster state.",
                    missing_access: "Kubernetes/operator access is unavailable to Djinn agents",
                },
                ExternallyBlockedCriterion {
                    criterion: "Authenticate to a live Djinn deployment and capture operator-only proof of the production workflow.",
                    missing_access: "production Djinn credentials and operator-only proof are unavailable",
                },
            ],
        }
    }

    fn all_remaining_work_is_external_or_operator_only(&self) -> bool {
        !self.remaining_criteria.is_empty()
            && self.remaining_criteria.iter().all(|criterion| {
                [
                    "Docker",
                    "Postgres",
                    "Kubernetes",
                    "operator",
                    "credentials",
                    "Djinn",
                ]
                .iter()
                .any(|blocked_term| {
                    criterion.criterion.contains(blocked_term)
                        || criterion.missing_access.contains(blocked_term)
                })
            })
    }

    fn replay_against_prompt_policy(&self, planner_prompt: &str) -> Vec<PlannerReplayAction> {
        let prompt_routes_external_proof_to_repair = planner_prompt
            .contains("unavailable external tools, external infrastructure")
            && planner_prompt
                .contains("Rewrite or drop invalid task acceptance criteria with `task_update`")
            && planner_prompt
                .contains("Lack of Djinn tool/environment access is NOT a reason to `escalate`");
        let prompt_requires_close_after_pruning = planner_prompt.contains("`epic_close(")
            && planner_prompt.contains("submit_grooming(decision=\"close\")");

        if self.all_remaining_work_is_external_or_operator_only()
            && prompt_routes_external_proof_to_repair
            && prompt_requires_close_after_pruning
        {
            vec![
                PlannerReplayAction::RepairCriteriaWithTaskUpdate,
                PlannerReplayAction::AddPruningRationaleComment,
                PlannerReplayAction::CloseEpic,
                PlannerReplayAction::SubmitGroomingClose,
            ]
        } else {
            Vec::new()
        }
    }
}

#[test]
fn planner_4lzx_externally_blocked_replay_prunes_and_closes_in_one_session() {
    let replay = Planner4lzxExternallyBlockedReplay::synthetic_4lzx_externally_blocked_fixture();
    let mut planning_task = make_task();
    planning_task.id = replay.open_planning_task_id.into();
    planning_task.issue_type = "planning".into();
    planning_task.epic_id = Some(replay.epic_id.into());
    planning_task.title = "Plan next wave for 4lzx externally blocked epic".into();
    planning_task.description = "All implementation tasks are closed; only external-infrastructure/operator-only proof criteria remain.".into();
    planning_task.acceptance_criteria = serde_json::json!(
        replay
            .remaining_criteria
            .iter()
            .map(|criterion| serde_json::json!({
                "criterion": criterion.criterion,
                "met": false,
            }))
            .collect::<Vec<_>>()
    )
    .to_string();

    let planner_prompt = render_prompt(AgentType::Planner, &planning_task, &make_ctx());
    let actions = replay.replay_against_prompt_policy(&planner_prompt);

    assert!(
        replay.all_remaining_work_is_external_or_operator_only(),
        "fixture should model only Docker/Postgres/Kubernetes/operator/Djinn-auth blocked criteria"
    );
    assert_eq!(
        actions,
        vec![
            PlannerReplayAction::RepairCriteriaWithTaskUpdate,
            PlannerReplayAction::AddPruningRationaleComment,
            PlannerReplayAction::CloseEpic,
            PlannerReplayAction::SubmitGroomingClose,
        ],
        "4lzx-style replay must converge by repairing/pruning criteria, closing the epic, and closing the planning task in one Planner session"
    );
    assert!(
        planner_prompt.contains("`task_update(")
            && planner_prompt.contains("`epic_close(")
            && planner_prompt.contains("`submit_grooming("),
        "Planner tool surface must expose the complete prune-and-close path"
    );
    assert!(
        !planner_prompt.contains("submit_grooming(decision=\"escalate\")"),
        "externally-blocked criteria should not route to Planner escalation"
    );
    assert!(
        !planner_prompt.contains("create retry worker tasks for external proof"),
        "externally-blocked proof should be repaired/pruned, not redispatched as worker implementation"
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

#[test]
fn worker_prompt_routes_memory_crud_through_mcp() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("memory_*") && prompt.contains("Memory notes live in Dolt"),
        "worker prompt should direct note CRUD through the memory_* MCP tools"
    );
    assert!(
        prompt.contains("memory_write")
            && prompt.contains("memory_read")
            && prompt.contains("memory_edit"),
        "worker prompt should call out the memory CRUD MCP tools by name"
    );
    assert!(
        prompt.contains("memory_build_context"),
        "worker prompt should retain registered analytical memory retrieval"
    );
}
/// Architect spike notes must still carry task traceability (per ADR-051
/// Contract 2 / §9 "Spike and Research Findings — Memory Writes").
#[test]
fn architect_prompt_requires_spike_note_traceability() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Architect, &task, &ctx);

    assert!(
        prompt.contains("Originated from task task-123"),
        "architect prompt should require task traceability in persisted spike notes"
    );
    assert!(
        prompt.contains("task objective"),
        "architect prompt should ask for enough context to explain why a memory note exists"
    );
    assert!(
        prompt.contains("memory_graph")
            && prompt.contains("memory_associations")
            && prompt.contains("memory_confirm"),
        "architect prompt should document the retained analytical memory tools"
    );
    assert!(
        prompt.contains("memory_write") && prompt.contains("memory_edit"),
        "architect prompt should route note CRUD through memory_* MCP tools"
    );
}

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

// ── Tools section snapshot tests ─────────────────────────────────────────

#[test]
fn worker_tools_section_snapshot() {
    let schemas = (AgentType::Worker.role_config().tool_schemas)();
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn reviewer_tools_section_snapshot() {
    let schemas = (AgentType::Reviewer.role_config().tool_schemas)();
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn lead_tools_section_snapshot() {
    let schemas = (AgentType::Lead.role_config().tool_schemas)();
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn planner_tools_section_snapshot() {
    let schemas = (AgentType::Planner.role_config().tool_schemas)();
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn architect_tools_section_snapshot() {
    let schemas = (AgentType::Architect.role_config().tool_schemas)();
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn tools_section_injected_into_rendered_prompt() {
    let task = make_task();
    let ctx = make_ctx();

    // Verify each role's prompt contains its tools and NOT other roles' tools.
    let worker_prompt = render_prompt(AgentType::Worker, &task, &ctx);
    assert!(
        worker_prompt.contains("`submit_work("),
        "worker prompt should contain submit_work"
    );
    assert!(
        worker_prompt.contains("`github_search("),
        "worker prompt should contain github_search"
    );
    assert!(
        !worker_prompt.contains("`submit_review("),
        "worker prompt should NOT contain submit_review"
    );
    assert!(
        !worker_prompt.contains("{{tools_section}}"),
        "tools_section placeholder should be replaced"
    );

    let reviewer_prompt = render_prompt(AgentType::Reviewer, &task, &ctx);
    assert!(
        reviewer_prompt.contains("`submit_review("),
        "reviewer prompt should contain submit_review"
    );
    assert!(
        reviewer_prompt.contains("`github_search("),
        "reviewer prompt should contain github_search"
    );
    assert!(
        !reviewer_prompt.contains("`submit_work("),
        "reviewer prompt should NOT contain submit_work"
    );

    let lead_prompt = render_prompt(AgentType::Lead, &task, &ctx);
    assert!(
        lead_prompt.contains("`submit_decision("),
        "lead prompt should contain submit_decision"
    );
    assert!(
        lead_prompt.contains("`task_create("),
        "lead prompt should contain task_create"
    );
    assert!(
        !lead_prompt.contains("`submit_work("),
        "lead prompt should NOT contain submit_work"
    );

    let planner_prompt = render_prompt(AgentType::Planner, &task, &ctx);
    assert!(
        planner_prompt.contains("`submit_grooming("),
        "planner prompt should contain submit_grooming"
    );
    assert!(
        planner_prompt.contains("`epic_tasks("),
        "planner prompt should contain epic_tasks"
    );

    let architect_prompt = render_prompt(AgentType::Architect, &task, &ctx);
    assert!(
        architect_prompt.contains("`submit_work("),
        "architect prompt should contain submit_work"
    );
    assert!(
        architect_prompt.contains("`memory_health("),
        "architect prompt should contain memory_health"
    );

    let planner_tools = (AgentType::Planner.role_config().tool_schemas)();
    let planner_has_amend_tool = planner_tools.iter().any(|schema| {
        schema.get("name").and_then(|name| name.as_str()) == Some("agent_amend_prompt")
    });

    let architect_tools = (AgentType::Architect.role_config().tool_schemas)();
    let architect_has_amend_tool = architect_tools.iter().any(|schema| {
        schema.get("name").and_then(|name| name.as_str()) == Some("agent_amend_prompt")
    });

    // Per ADR-051 §1 `role_amend_prompt` moved from Architect to Planner
    // (agent-effectiveness review is a Planner action, not a consultant
    // action). Architect keeps `role_metrics` (read) and `role_create`
    // (structural proposal) but cannot mutate existing learned_prompts.
    assert!(
        !architect_prompt.contains("`agent_amend_prompt("),
        "architect prompt should NOT contain agent_amend_prompt — it moved to Planner per ADR-051"
    );
    assert!(
        !architect_has_amend_tool,
        "architect tool schemas should NOT expose agent_amend_prompt — learned-prompt amendments are Planner-owned"
    );
    assert!(
        planner_prompt.contains("`agent_amend_prompt("),
        "planner prompt should contain agent_amend_prompt — it moved here per ADR-051"
    );
    assert!(
        planner_has_amend_tool,
        "planner tool schemas should expose agent_amend_prompt for evidence-based learned-prompt amendments"
    );
}

// ── Learned-prompt amendment guidance (zr1e) ─────────────────────────────

/// The Planner prompt must carry explicit guidance for evidence-based
/// `learned_prompt` amendments, covering triggers, evidence requirements,
/// eligible roles, amendment shape, and evaluator follow-up semantics.
/// See decision `design/zr1e-roadmap`.
#[test]
fn planner_prompt_contains_learned_prompt_amendment_guidance() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Planner, &task, &ctx);

    // The section heading must be present.
    assert!(
        prompt.contains("Learned-prompt amendments"),
        "planner prompt should have an explicit learned-prompt amendment section"
    );

    // learned_prompt is machine-managed; human customization is system_prompt_extensions.
    assert!(
        prompt.contains("machine-managed"),
        "planner prompt must label learned_prompt as machine-managed"
    );
    assert!(
        prompt.contains("system_prompt_extensions"),
        "planner prompt must route human customization to system_prompt_extensions"
    );

    // Triggers: rare, evidence-based, agent-effectiveness grooming.
    assert!(
        prompt.contains("agent-effectiveness grooming"),
        "planner prompt must scope amendments to agent-effectiveness grooming"
    );
    assert!(
        prompt.contains("repeated, stable"),
        "planner prompt must require a repeated, stable failure pattern"
    );

    // Evidence requirements — at least one of the named evidence sources.
    assert!(
        prompt.contains("agent_metrics"),
        "planner prompt must list agent_metrics as valid evidence"
    );
    assert!(
        prompt.contains("reviewer or Lead feedback"),
        "planner prompt must list repeated reviewer/lead feedback as valid evidence"
    );
    assert!(
        prompt.contains("verification/reopen patterns"),
        "planner prompt must list verification/reopen patterns as valid evidence"
    );
    assert!(
        prompt.contains("Session reflections"),
        "planner prompt must list session reflections as valid evidence"
    );

    // Eligible roles: specialist worker/reviewer only.
    assert!(
        prompt.contains("specialist agents"),
        "planner prompt must restrict amendments to specialist agents"
    );
    assert!(
        prompt.contains("NOT eligible"),
        "planner prompt must call out ineligible roles explicitly"
    );
    assert!(
        prompt.contains("lead") && prompt.contains("planner") && prompt.contains("architect"),
        "planner prompt must list lead, planner, and architect as ineligible amendment targets"
    );

    // Amendment shape: concise, behavioral, metrics snapshot.
    assert!(
        prompt.contains("concise, behavioral"),
        "planner prompt must require concise, behavioral amendment text"
    );
    assert!(
        prompt.contains("metrics_snapshot"),
        "planner prompt must mention the metrics_snapshot parameter for audit"
    );
    assert!(
        prompt.contains("JSON metrics snapshot"),
        "planner prompt must require JSON metrics snapshots when available"
    );

    // Evaluator follow-up semantics: confirm / probation / discard.
    assert!(
        prompt.contains("confirms"),
        "planner prompt must explain that the evaluator confirms successful amendments"
    );
    assert!(
        prompt.contains("probation"),
        "planner prompt must explain that ambiguous amendments stay on probation"
    );
    assert!(
        prompt.contains("discarded and reverted"),
        "planner prompt must explain that regressions are discarded and reverted"
    );

    // The Planner must not self-confirm/discard — the coordinator evaluator does.
    assert!(
        prompt.contains("You do not confirm or discard amendments yourself"),
        "planner prompt must make clear the Planner proposes, the evaluator disposes"
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

/// Architect must NOT carry the learned-prompt amendment guidance or the
/// `agent_amend_prompt` tool — that ownership moved to the Planner per ADR-051.
/// This guards the "preserving Architect non-ownership" criterion.
#[test]
fn architect_prompt_omits_learned_prompt_amendment_guidance_and_tool() {
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Architect, &task, &ctx);

    assert!(
        !prompt.contains("Learned-prompt amendments"),
        "architect prompt must NOT contain the learned-prompt amendment section — it is Planner-owned per ADR-051"
    );
    assert!(
        !prompt.contains("`agent_amend_prompt("),
        "architect prompt must NOT contain agent_amend_prompt tool — it is Planner-owned per ADR-051"
    );
}
