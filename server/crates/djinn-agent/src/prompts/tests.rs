use super::*;
use crate::AgentType;
use djinn_core::models::Task;

/// Ensure the tool schema registry is initialized for tests that need it.
fn ensure_registry() {
    crate::test_helpers::ensure_tool_schemas_registered();
}

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
        ci_mq_state: None,
        ci_mq_run_id: None,
        ci_mq_head_sha: None,
        ci_mq_failed_check_names: None,
        ci_mq_failure_fingerprint: None,
        ci_mq_same_signature_count: None,
        ci_mq_first_seen_at: None,
        ci_mq_last_seen_at: None,
        unresolved_blocker_count: 0,
        total_reopen_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
    }
}

fn make_ctx() -> TaskContext {
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
    }
}

// ── Facade parity test ──────────────────────────────────────────────────

/// Verify that `djinn_agent` facade paths still provide access to `AgentType`,
/// role config APIs, and prompt APIs — ensuring downstream callers that import
/// through the old paths keep compiling.
#[test]
fn facade_parity_agent_type_and_role_config() {
    // AgentType re-exported from djinn-roles
    let agent_type = AgentType::Worker;
    assert_eq!(agent_type.as_str(), "worker");
    assert_eq!(agent_type.dispatch_role(), "worker");

    // RoleConfig re-exported from djinn-roles through the roles facade
    let config = crate::roles::config_for(agent_type);
    assert_eq!(config.name, "worker");
    assert_eq!(config.display_name, "Developer");

    // Prompt templates re-exported from djinn-roles through the prompts facade
    assert!(!super::BASE_TEMPLATE.is_empty());
    assert!(!super::DEV_TEMPLATE.is_empty());
    assert!(!super::REVIEWER_TEMPLATE.is_empty());
    assert!(!super::LEAD_TEMPLATE.is_empty());
    assert!(!super::PLANNER_TEMPLATE.is_empty());
    assert!(!super::ARCHITECT_TEMPLATE.is_empty());

    // format_acceptance_criteria / format_labels re-exported
    assert_eq!(super::format_labels("[]"), "");
    assert_eq!(super::format_acceptance_criteria("not json"), "not json");

    // MAX_SYSTEM_PROMPT_CHARS re-exported
    const { assert!(super::MAX_SYSTEM_PROMPT_CHARS > 0) }
}

// ── Tests that require extension tool-schema registry ────────────────────

#[test]
fn worker_prompt_contains_task_fields() {
    ensure_registry();
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
    assert!(prompt.contains("/home/user/project/.task-runtime/worktrees/t123"));
    assert!(prompt.contains("memory_write"));
    assert!(prompt.contains("memory_edit"));
    // A plain `task` runs the implement flow — no research/spike section.
    assert!(!prompt.contains("Research Deliverable"));
    // No un-substituted placeholders
    assert!(!prompt.contains("{{"));
}

/// glqk: the worker prompt must instruct consulting index coverage before
/// asserting an absence (no callers / unused / safe to remove) from the graph,
/// and falling back to grep for an unindexed workspace.
#[test]
fn worker_prompt_carries_coverage_fallback_guidance() {
    ensure_registry();
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("coverage"),
        "worker prompt must reference the coverage advisory/op"
    );
    assert!(
        prompt.contains("safe to remove"),
        "worker prompt must name the absence-assertion it guards"
    );
    assert!(
        prompt.contains("false negative") && prompt.to_lowercase().contains("grep"),
        "worker prompt must instruct the grep fallback for an unindexed workspace"
    );
    assert!(!prompt.contains("{{"));
}

/// glqk: the planner prompt must steer scoping of graph-based removal/rename
/// tasks through index coverage (honor `needs_spike`, spike the uncovered
/// workspace).
#[test]
fn planner_prompt_carries_coverage_scoping_guidance() {
    ensure_registry();
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Planner, &task, &ctx);

    assert!(
        prompt.contains("coverage"),
        "planner prompt must reference the coverage advisory/op"
    );
    assert!(
        prompt.contains("needs_spike"),
        "planner prompt must tie the uncovered-workspace case to needs_spike"
    );
    assert!(!prompt.contains("{{"));
}

#[test]
fn task_reviewer_prompt_contains_task_fields() {
    ensure_registry();
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

/// Architect spike notes must still carry task traceability (per ADR-051
/// Contract 2 / §9 "Spike and Research Findings — Memory Writes").
#[test]
fn architect_prompt_requires_spike_note_traceability() {
    ensure_registry();
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
fn worker_prompt_routes_memory_crud_through_mcp() {
    ensure_registry();
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Worker, &task, &ctx);

    assert!(
        prompt.contains("memory_*") && prompt.contains("Notes are accessed through"),
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

/// The Planner prompt must NOT carry the learned-prompt amendment guidance —
/// the amendment runtime and `agent_amend_prompt` tool have been removed (3x0w).
#[test]
fn planner_prompt_omits_learned_prompt_amendment_guidance() {
    ensure_registry();
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Planner, &task, &ctx);

    assert!(
        !prompt.contains("Learned-prompt amendments"),
        "planner prompt must NOT contain the removed learned-prompt amendment section"
    );
    assert!(
        !prompt.contains("agent_amend_prompt"),
        "planner prompt must NOT reference the removed agent_amend_prompt tool"
    );
}

/// Architect must NOT carry the learned-prompt amendment guidance or the
/// `agent_amend_prompt` tool — the amendment path has been removed.
#[test]
fn architect_prompt_omits_learned_prompt_amendment_guidance_and_tool() {
    ensure_registry();
    let task = make_task();
    let ctx = make_ctx();
    let prompt = render_prompt(AgentType::Architect, &task, &ctx);

    assert!(
        !prompt.contains("Learned-prompt amendments"),
        "architect prompt must NOT contain the learned-prompt amendment section"
    );
    assert!(
        !prompt.contains("`agent_amend_prompt("),
        "architect prompt must NOT contain agent_amend_prompt tool"
    );
}

// ── Planner 4lzx replay (needs tool schemas for tool-name assertions) ───

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
    ensure_registry();
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

// ── Tools section snapshot tests ─────────────────────────────────────────

#[test]
fn worker_tools_section_snapshot() {
    ensure_registry();
    let schemas = crate::roles::tool_schemas_for(AgentType::Worker);
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn reviewer_tools_section_snapshot() {
    ensure_registry();
    let schemas = crate::roles::tool_schemas_for(AgentType::Reviewer);
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn lead_tools_section_snapshot() {
    ensure_registry();
    let schemas = crate::roles::tool_schemas_for(AgentType::Lead);
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn planner_tools_section_snapshot() {
    ensure_registry();
    let schemas = crate::roles::tool_schemas_for(AgentType::Planner);
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn architect_tools_section_snapshot() {
    ensure_registry();
    let schemas = crate::roles::tool_schemas_for(AgentType::Architect);
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn advocate_tools_section_snapshot() {
    ensure_registry();
    let schemas = crate::roles::tool_schemas_for(AgentType::Advocate);
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn adversary_tools_section_snapshot() {
    ensure_registry();
    let schemas = crate::roles::tool_schemas_for(AgentType::Adversary);
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn judge_tools_section_snapshot() {
    ensure_registry();
    let schemas = crate::roles::tool_schemas_for(AgentType::Judge);
    let section = format_tools_section(&schemas);
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(section);
    });
}

#[test]
fn tools_section_injected_into_rendered_prompt() {
    ensure_registry();
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

    // The agent_amend_prompt tool has been removed from all roles.
    let planner_tools = crate::roles::tool_schemas_for(AgentType::Planner);
    let planner_has_amend_tool = planner_tools.iter().any(|schema| {
        schema.get("name").and_then(|name| name.as_str()) == Some("agent_amend_prompt")
    });
    assert!(
        !planner_prompt.contains("`agent_amend_prompt("),
        "planner prompt should NOT contain agent_amend_prompt — the tool has been removed"
    );
    assert!(
        !planner_has_amend_tool,
        "planner tool schemas should NOT expose agent_amend_prompt — the tool has been removed"
    );

    let architect_tools = crate::roles::tool_schemas_for(AgentType::Architect);
    let architect_has_amend_tool = architect_tools.iter().any(|schema| {
        schema.get("name").and_then(|name| name.as_str()) == Some("agent_amend_prompt")
    });
    assert!(
        !architect_prompt.contains("`agent_amend_prompt("),
        "architect prompt should NOT contain agent_amend_prompt — the tool has been removed"
    );
    assert!(
        !architect_has_amend_tool,
        "architect tool schemas should NOT expose agent_amend_prompt — the tool has been removed"
    );

    // Tribunal roles (k9zw): verify each role renders its finalize tool.
    let advocate_prompt = render_prompt(AgentType::Advocate, &task, &ctx);
    assert!(
        advocate_prompt.contains("`submit_work("),
        "advocate prompt should contain submit_work"
    );
    assert!(
        !advocate_prompt.contains("{{"),
        "advocate prompt should have no unresolved placeholders"
    );

    let adversary_prompt = render_prompt(AgentType::Adversary, &task, &ctx);
    assert!(
        adversary_prompt.contains("`submit_review("),
        "adversary prompt should contain submit_review"
    );
    assert!(
        !adversary_prompt.contains("{{"),
        "adversary prompt should have no unresolved placeholders"
    );

    let judge_prompt = render_prompt(AgentType::Judge, &task, &ctx);
    assert!(
        judge_prompt.contains("`submit_decision("),
        "judge prompt should contain submit_decision"
    );
    assert!(
        !judge_prompt.contains("{{"),
        "judge prompt should have no unresolved placeholders"
    );
}

// ── Advocate enrichment guidance regressions (k9zw) ─────────────────────
//
// Verify that the advocate prompt contains the expected enrichment guidance
// for progressive MDX block-catalog consumption, and that the rendered prompt
// includes the new block-catalog tools while adversary/judge do not.

#[test]
fn advocate_prompt_contains_enrichment_guidance() {
    let prompt = include_str!("../../../djinn-roles/src/prompts/advocate.md");

    // Must reference proposal_block_patch for targeted enrichment.
    assert!(
        prompt.contains("proposal_block_patch"),
        "advocate.md must mention proposal_block_patch for MDX enrichment"
    );
    // Must point the Advocate at the visual-spec native skill so the refined
    // spec is rich MDX rather than shallow prose (the skill is now injected
    // for the advocate role).
    assert!(
        prompt.contains("visual-spec") && prompt.contains("skill_read"),
        "advocate.md must instruct loading the visual-spec skill via skill_read"
    );
    // Must instruct get_block_catalog pull on demand.
    assert!(
        prompt.contains("get_block_catalog"),
        "advocate.md must instruct get_block_catalog pull on demand"
    );
    // Must declare enrichment as default-but-optional, not DoR gate.
    assert!(
        prompt.contains("default behavior, not a deterministic DoR gate"),
        "advocate.md must state enrichment is default-but-optional"
    );
    // Must bound enrichment to at most one block per revision.
    assert!(
        prompt.contains("at most one stable block"),
        "advocate.md must bound enrichment to one block per revision"
    );
    // Must not require MDX for readiness.
    assert!(
        prompt.contains("prose grounding remains sufficient"),
        "advocate.md must not require MDX for DoR readiness"
    );
}

#[test]
fn advocate_rendered_prompt_includes_block_catalog_tools() {
    ensure_registry();
    let task = make_task();
    let ctx = make_ctx();

    let advocate_prompt = render_prompt(AgentType::Advocate, &task, &ctx);
    // Advocate prompt must include block-catalog enrichment tools.
    assert!(
        advocate_prompt.contains("`proposal_block_patch("),
        "advocate rendered prompt should include proposal_block_patch tool"
    );
    assert!(
        advocate_prompt.contains("`get_block_catalog("),
        "advocate rendered prompt should include get_block_catalog tool"
    );
    assert!(
        advocate_prompt.contains("`proposal_blocks("),
        "advocate rendered prompt should include proposal_blocks tool"
    );
    assert!(
        advocate_prompt.contains("`proposal_update("),
        "advocate rendered prompt should include proposal_update tool"
    );
}

#[test]
fn adversary_judge_do_not_include_block_catalog_tools() {
    ensure_registry();
    let task = make_task();
    let ctx = make_ctx();

    // Adversary and Judge must NOT have block-catalog/advocacy tools.
    let adversary_prompt = render_prompt(AgentType::Adversary, &task, &ctx);
    assert!(
        !adversary_prompt.contains("`proposal_block_patch("),
        "adversary rendered prompt must NOT include proposal_block_patch"
    );
    assert!(
        !adversary_prompt.contains("`get_block_catalog("),
        "adversary rendered prompt must NOT include get_block_catalog"
    );
    assert!(
        !adversary_prompt.contains("`proposal_update("),
        "adversary rendered prompt must NOT include proposal_update"
    );

    let judge_prompt = render_prompt(AgentType::Judge, &task, &ctx);
    assert!(
        !judge_prompt.contains("`proposal_block_patch("),
        "judge rendered prompt must NOT include proposal_block_patch"
    );
    assert!(
        !judge_prompt.contains("`get_block_catalog("),
        "judge rendered prompt must NOT include get_block_catalog"
    );
    assert!(
        !judge_prompt.contains("`proposal_update("),
        "judge rendered prompt must NOT include proposal_update"
    );
}

/// Regression guard for the tribunal debate-trail wiring bug: the refinement
/// loop detects an Adversary's objections and a Judge's verdict by reading
/// `proposal_debate_append` entries from the debate trail. If those roles are
/// not given `proposal_debate_append`, their structured output (filed via
/// `submit_review`/`submit_decision`) is dropped, the round looks "dry", and
/// the tribunal hollow-converges. Both roles MUST carry the tool the loop reads.
#[test]
fn adversary_and_judge_can_file_debate_trail_entries() {
    ensure_registry();
    let task = make_task();
    let ctx = make_ctx();

    let adversary_prompt = render_prompt(AgentType::Adversary, &task, &ctx);
    assert!(
        adversary_prompt.contains("`proposal_debate_append("),
        "adversary MUST have proposal_debate_append — it is the only channel the \
         refinement loop reads for objections"
    );

    let judge_prompt = render_prompt(AgentType::Judge, &task, &ctx);
    assert!(
        judge_prompt.contains("`proposal_debate_append("),
        "judge MUST have proposal_debate_append — it is the only channel the \
         refinement loop reads for the verdict"
    );
}

/// The Advocate's rebuttal channel (scope-ratchet counterweight): its prompt
/// must teach filing `kind="rebuttal"` entries via `proposal_debate_append`,
/// and the Judge's prompt must teach adjudicating them. Without the prompt
/// wiring the tool grant in `tool_schemas_advocate` is dead weight and every
/// objection can only be resolved by growing the spec.
#[test]
fn advocate_rebuttal_channel_is_prompt_wired() {
    ensure_registry();
    let task = make_task();
    let ctx = make_ctx();

    let advocate_prompt = render_prompt(AgentType::Advocate, &task, &ctx);
    assert!(
        advocate_prompt.contains("proposal_debate_append("),
        "advocate prompt must teach the proposal_debate_append rebuttal call"
    );
    assert!(
        advocate_prompt.contains("kind=\"rebuttal\"")
            || advocate_prompt.contains("kind                  = \"rebuttal\""),
        "advocate prompt must pin kind=\"rebuttal\" as its only debate-trail kind"
    );

    let judge_prompt = render_prompt(AgentType::Judge, &task, &ctx);
    assert!(
        judge_prompt.contains("Adjudicating rebuttals"),
        "judge prompt must carry the rebuttal adjudication section"
    );
    assert!(
        judge_prompt.contains("Minimality"),
        "judge prompt must carry the minimality DoD dimension"
    );
}

mod visual_spec;

// ── Proposal-address prompt regressions (y4td) ───────────────────────────
//
// Verify that the proposal_address.md prompt text contains the expected
// workflow guidance for progressive markdown-to-MDX enrichment via targeted
// block patches, without inlining block vocabulary or forcing non-authoring
// planner prompts to pay the catalog/skill body cost.

#[test]
fn proposal_address_prompt_contains_block_patch_workflow_guidance() {
    let prompt = include_str!("../../../djinn-roles/src/prompts/proposal_address.md");

    // Must reference the targeted block-patch primitive.
    assert!(
        prompt.contains("proposal_block_patch"),
        "proposal_address.md must mention proposal_block_patch for targeted enrichment"
    );

    // Must reference visual-spec skill loading.
    assert!(
        prompt.contains("visual-spec"),
        "proposal_address.md must mention visual-spec native skill for authoring sessions"
    );
    assert!(
        prompt.contains("skill_read"),
        "proposal_address.md must instruct skill_read to load visual-spec on demand"
    );

    // Must reference catalog pull on demand.
    assert!(
        prompt.contains("get_block_catalog"),
        "proposal_address.md must instruct get_block_catalog pull on demand"
    );

    // Must reference memory retrieval for learned refinements.
    assert!(
        prompt.contains("memory_search") || prompt.contains("memory_build_context"),
        "proposal_address.md must instruct memory retrieval for learned refinements"
    );

    // Must mention revision sequencing / latest_revision_seq inspection.
    assert!(
        prompt.contains("latest_revision_seq"),
        "proposal_address.md must mention latest_revision_seq for patch sequencing"
    );

    // Must mention attribution fields.
    assert!(
        prompt.contains("native_skill_version"),
        "proposal_address.md must mention native_skill_version for attribution"
    );
    assert!(
        prompt.contains("native_skill_name"),
        "proposal_address.md must mention native_skill_name for attribution"
    );

    // Lazy semantics: must NOT inline the full block vocabulary (no concrete
    // block tag names from the catalog).
    let forbidden_block_tags = [
        "AnnotatedCode",
        "ApiEndpoint",
        "Callout",
        "Checklist",
        "Columns",
        "Decisions",
        "Diagram",
        "Diff",
        "FileTree",
        "JsonExplorer",
        "QuestionForm",
        "RichText",
        "Tabs",
        "Wireframe",
    ];
    for tag in &forbidden_block_tags {
        assert!(
            !prompt.contains(tag),
            "proposal_address.md must not inline block vocabulary tag {tag}"
        );
    }

    // Must not embed a giant catalog or list of block types.
    assert!(
        !prompt.contains("block_types"),
        "proposal_address.md must not embed a block_types catalog list"
    );
}

#[test]
fn proposal_address_prompt_distinguishes_simple_update_from_block_patch() {
    let prompt = include_str!("../../../djinn-roles/src/prompts/proposal_address.md");

    // Both paths (simple update and block-patch) should be mentioned.
    assert!(
        prompt.contains("proposal_update"),
        "proposal_address.md must still mention proposal_update for simple edits"
    );
    assert!(
        prompt.contains("proposal_block_patch"),
        "proposal_address.md must mention proposal_block_patch for MDX enrichment"
    );

    // The block-patch path should be framed as progressive enrichment.
    let lower = prompt.to_lowercase();
    assert!(
        lower.contains("progressive") || lower.contains("one patch per revision"),
        "proposal_address.md must frame block-patch as progressive enrichment"
    );
}

#[test]
fn proposal_address_prompt_preserves_existing_feedback_rules() {
    let prompt = include_str!("../../../djinn-roles/src/prompts/proposal_address.md");

    // Existing rules must survive.
    assert!(
        prompt.contains("proposal_feedback_resolve"),
        "proposal_address.md must still mention proposal_feedback_resolve"
    );
    assert!(
        prompt.contains("building"),
        "proposal_address.md must still mention the building guard"
    );
    assert!(
        prompt.contains("{{PROPOSAL_CONTEXT}}"),
        "proposal_address.md must keep the PROPOSAL_CONTEXT substitution marker"
    );
}

// ── Tool section: signatures only (wzz6 item 1) ─────────────────────────

/// AC2: A known tool description string is absent from the prompt-side tools
/// section while the signature (name + parameters) remains present.
#[test]
fn tools_section_omits_descriptions_and_retains_signatures() {
    ensure_registry();
    let schemas = crate::roles::tool_schemas_for(AgentType::Worker);
    let section = format_tools_section(&schemas);

    // A known tool description should NOT appear in the section.
    assert!(
        !section.contains("Execute shell commands in the task worktree"),
        "tools section must omit tool descriptions; found 'Execute shell commands'"
    );
    assert!(
        !section.contains("Show details of a work item"),
        "tools section must omit task_show description"
    );
    assert!(
        !section.contains("Search notes and proposals"),
        "tools section must omit memory_search description"
    );

    // Tool signatures (name + params) must still be present.
    assert!(
        section.contains("`shell(command, timeout_ms?)`"),
        "tools section must retain shell signature"
    );
    assert!(
        section.contains("`task_show(id)`"),
        "tools section must retain task_show signature"
    );
    assert!(
        section.contains("`memory_search(query,"),
        "tools section must retain memory_search signature"
    );
}

/// AC2: Same check for the planner tools section — descriptions absent.
#[test]
fn planner_tools_section_omits_descriptions() {
    ensure_registry();
    let schemas = crate::roles::tool_schemas_for(AgentType::Planner);
    let section = format_tools_section(&schemas);

    assert!(
        !section.contains("Reconcile a proposal's acceptance-criteria"),
        "planner tools section must omit proposal_ac_set description"
    );
    assert!(
        section.contains("`proposal_ac_set("),
        "planner tools section must retain proposal_ac_set signature"
    );
}

/// AC3: Rendered planner prompt shrinks by at least 8KB vs the old format
/// baseline (which included per-tool descriptions).
#[test]
fn planner_prompt_shrinks_by_at_least_8kb() {
    ensure_registry();
    let schemas = crate::roles::tool_schemas_for(AgentType::Planner);

    // Build a baseline tools section using the old format (with descriptions).
    let baseline_tools = schemas
        .iter()
        .map(format_tool_line_with_description)
        .collect::<Vec<_>>()
        .join("\n");
    let current_tools = format_tools_section(&schemas);
    let tools_savings = baseline_tools.len().saturating_sub(current_tools.len());

    // The shrinkage in the tools section propagates directly to the rendered
    // prompt. Assert the tools section alone shrinks by >= 8KB.
    assert!(
        tools_savings >= 8 * 1024,
        "planner tools section must shrink by >= 8KB; got {tools_savings} bytes savings \
         (baseline {}, current {})",
        baseline_tools.len(),
        current_tools.len()
    );
}

/// AC3: Rendered worker prompt shrinks by at least 4KB vs the old format
/// baseline (which included per-tool descriptions).
#[test]
fn worker_prompt_shrinks_by_at_least_4kb() {
    ensure_registry();
    let schemas = crate::roles::tool_schemas_for(AgentType::Worker);

    // Build a baseline tools section using the old format.
    let baseline_tools = schemas
        .iter()
        .map(format_tool_line_with_description)
        .collect::<Vec<_>>()
        .join("\n");
    let current_tools = format_tools_section(&schemas);
    let tools_savings = baseline_tools.len().saturating_sub(current_tools.len());

    assert!(
        tools_savings >= 4 * 1024,
        "worker tools section must shrink by >= 4KB; got {tools_savings} bytes savings \
         (baseline {}, current {})",
        baseline_tools.len(),
        current_tools.len()
    );
}

/// AC4: Verify that the tool schemas (passed to the provider at stream time)
/// still carry `name`, `description`, and `inputSchema`.  The removal of
/// descriptions from the prompt-side tools section must NOT affect the
/// provider-side schemas.
#[test]
fn provider_tool_schemas_still_carry_descriptions() {
    ensure_registry();

    for agent_type in [
        AgentType::Worker,
        AgentType::Planner,
        AgentType::Reviewer,
        AgentType::Lead,
    ] {
        let schemas = crate::roles::tool_schemas_for(agent_type);
        assert!(
            !schemas.is_empty(),
            "{:?} must have tool schemas",
            agent_type
        );
        for schema in &schemas {
            assert!(
                schema.get("name").and_then(|v| v.as_str()).is_some(),
                "{:?} tool schema must have 'name'",
                agent_type
            );
            assert!(
                schema.get("description").and_then(|v| v.as_str()).is_some(),
                "{:?} tool schema must have 'description'",
                agent_type
            );
            assert!(
                schema.get("inputSchema").is_some(),
                "{:?} tool schema must have 'inputSchema'",
                agent_type
            );
        }
    }
}
