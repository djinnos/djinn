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
    assert!(super::MAX_SYSTEM_PROMPT_CHARS > 0);
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
    assert!(prompt.contains("/home/user/project/.djinn/worktrees/t123"));
    assert!(prompt.contains("memory_write"));
    assert!(prompt.contains("memory_edit"));
    // A plain `task` runs the implement flow — no research/spike section.
    assert!(!prompt.contains("Research Deliverable"));
    // No un-substituted placeholders
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

/// The Planner prompt must carry explicit guidance for evidence-based
/// `learned_prompt` amendments.
#[test]
fn planner_prompt_contains_learned_prompt_amendment_guidance() {
    ensure_registry();
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

/// Architect must NOT carry the learned-prompt amendment guidance or the
/// `agent_amend_prompt` tool — that ownership moved to the Planner per ADR-051.
#[test]
fn architect_prompt_omits_learned_prompt_amendment_guidance_and_tool() {
    ensure_registry();
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

    let planner_tools = crate::roles::tool_schemas_for(AgentType::Planner);
    let planner_has_amend_tool = planner_tools.iter().any(|schema| {
        schema.get("name").and_then(|name| name.as_str()) == Some("agent_amend_prompt")
    });

    let architect_tools = crate::roles::tool_schemas_for(AgentType::Architect);
    let architect_has_amend_tool = architect_tools.iter().any(|schema| {
        schema.get("name").and_then(|name| name.as_str()) == Some("agent_amend_prompt")
    });

    // Per ADR-051 §1 `role_amend_prompt` moved from Architect to Planner
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

// ── Integrated native visual-spec regressions (epic 5uzr) ───────────────
//
// These tests verify end-to-end behavior across the native skill registry,
// lifecycle skill resolution, and prompt rendering.  They exercise the
// acceptance criteria for task jll3: native visual-spec is included in
// planner prompts by default with active version and authoring guidance,
// non-planner roles do not receive it, project/worktree skills cannot
// shadow the native body, and the rendered prompt exposes the expected
// content.

/// Helper: render the base prompt for `agent_type`, then merge native
/// skills for `role_name` alongside any project skills, and apply the
/// merged skills to the prompt.  Returns the final system prompt string.
fn render_prompt_with_skills(
    agent_type: AgentType,
    role_name: &str,
    project_skills: Vec<crate::skills::ResolvedSkill>,
    authoring_trigger: Option<crate::actors::slot::lifecycle::task_classifier::NativeSkillTrigger>,
) -> String {
    ensure_registry();
    let task = make_task();
    let ctx = make_ctx();
    let base = render_prompt(agent_type, &task, &ctx);
    let (merged, _native_names) = crate::actors::slot::lifecycle::mcp_resolve::merge_native_skills(
        role_name,
        project_skills,
        authoring_trigger,
    );
    apply_skills(&base, &merged)
}

/// AC1: Planner prompt includes native `visual-spec` by default with active
/// version stamp and the key authoring guidance sections visible.
#[test]
fn planner_prompt_includes_native_visual_spec_with_version_and_guidance() {
    let prompt = render_prompt_with_skills(
        AgentType::Planner,
        "planner",
        Vec::new(),
        Some(
            crate::actors::slot::lifecycle::task_classifier::NativeSkillTrigger::ProposalAuthoring,
        ),
    );

    // Native visual-spec must be present in the planner prompt.
    assert!(
        prompt.contains("visual-spec"),
        "planner prompt must contain visual-spec skill name"
    );
    assert!(
        prompt.contains("platform"),
        "planner prompt must show native skill trust_level 'platform'"
    );

    // The active version stamp must be exposed through the registry.
    let version = crate::native_skills::VISUAL_SPEC_VERSION;
    assert!(
        !version.is_empty(),
        "VISUAL_SPEC_VERSION must be a non-empty string"
    );

    // Key authoring guidance must be present in the inlined content.
    let lower = prompt.to_lowercase();
    assert!(
        prompt.contains("backtick"),
        "visual-spec content must mention the bare-angle backtick constraint"
    );
    assert!(
        lower.contains("progressive"),
        "visual-spec content must teach progressive markdown-to-MDX enrichment"
    );
    assert!(
        lower.contains("mdx"),
        "visual-spec content must mention MDX enrichment"
    );
    assert!(
        lower.contains("memory"),
        "visual-spec content must mention memory as the learned layer"
    );
    assert!(
        lower.contains("learned") || lower.contains("refinement"),
        "visual-spec content must teach memory as the learned/refinement layer"
    );
    assert!(
        lower.contains("block"),
        "visual-spec content must address block authoring quality"
    );
    assert!(
        prompt.contains("## Available Skills"),
        "planner prompt must include the Available Skills section header"
    );
}

/// AC2: Worker prompt does NOT include native `visual-spec` by default.
#[test]
fn worker_prompt_does_not_include_native_visual_spec() {
    let prompt = render_prompt_with_skills(
        AgentType::Worker,
        "worker",
        Vec::new(),
        Some(
            crate::actors::slot::lifecycle::task_classifier::NativeSkillTrigger::ProposalAuthoring,
        ),
    );
    assert!(
        !prompt.contains("visual-spec"),
        "worker prompt must not contain visual-spec — it is planner-only"
    );
}

/// AC2: Reviewer prompt does NOT include native `visual-spec` by default.
#[test]
fn reviewer_prompt_does_not_include_native_visual_spec() {
    let prompt = render_prompt_with_skills(
        AgentType::Reviewer,
        "reviewer",
        Vec::new(),
        Some(
            crate::actors::slot::lifecycle::task_classifier::NativeSkillTrigger::ProposalAuthoring,
        ),
    );
    assert!(
        !prompt.contains("visual-spec"),
        "reviewer prompt must not contain visual-spec — it is planner-only"
    );
}

/// AC2: Lead prompt does NOT include native `visual-spec` by default.
#[test]
fn lead_prompt_does_not_include_native_visual_spec() {
    let prompt = render_prompt_with_skills(
        AgentType::Lead,
        "lead",
        Vec::new(),
        Some(
            crate::actors::slot::lifecycle::task_classifier::NativeSkillTrigger::ProposalAuthoring,
        ),
    );
    assert!(
        !prompt.contains("visual-spec"),
        "lead prompt must not contain visual-spec — it is planner-only"
    );
}

/// AC2: Architect prompt does NOT include native `visual-spec` by default.
#[test]
fn architect_prompt_does_not_include_native_visual_spec() {
    let prompt = render_prompt_with_skills(
        AgentType::Architect,
        "architect",
        Vec::new(),
        Some(
            crate::actors::slot::lifecycle::task_classifier::NativeSkillTrigger::ProposalAuthoring,
        ),
    );
    assert!(
        !prompt.contains("visual-spec"),
        "architect prompt must not contain visual-spec — it is planner-only"
    );
}

/// AC3: A project/worktree skill named `visual-spec` with a different body
/// cannot shadow or mutate the native planner default.  The rendered prompt
/// must contain the native body (compiled-in content), not the project body.
#[test]
fn project_visual_spec_skill_cannot_shadow_native_body_in_planner_prompt() {
    // Build a project skill named "visual-spec" with intentionally different
    // content that would be obvious if it replaced the native body.
    let project_visual_spec = crate::skills::ResolvedSkill {
        name: "visual-spec".to_string(),
        description: "Fake project visual-spec".to_string(),
        content: "THIS_IS_THE_PROJECT_BODY_NOT_NATIVE".to_string(),
        required: false,
        trust_level: "project".to_string(),
        recommended_for_roles: Vec::new(),
        tags: Vec::new(),
    };
    let other_project_skill = crate::skills::ResolvedSkill {
        name: "git".to_string(),
        description: "Git workflow".to_string(),
        content: "Git best practices from project.".to_string(),
        required: false,
        trust_level: "project".to_string(),
        recommended_for_roles: Vec::new(),
        tags: Vec::new(),
    };

    let prompt = render_prompt_with_skills(
        AgentType::Planner,
        "planner",
        vec![project_visual_spec, other_project_skill],
        Some(
            crate::actors::slot::lifecycle::task_classifier::NativeSkillTrigger::ProposalAuthoring,
        ),
    );

    // The native body must be present (contains the backtick constraint).
    assert!(
        prompt.contains("backtick"),
        "planner prompt must contain native visual-spec backtick guidance, not project body"
    );
    // The project body must NOT be present.
    assert!(
        !prompt.contains("THIS_IS_THE_PROJECT_BODY_NOT_NATIVE"),
        "project visual-spec body must not appear in the planner prompt — native body is authoritative"
    );
    // The other project skill should still be present.
    assert!(
        prompt.contains("Git best practices from project"),
        "non-colliding project skills must be preserved alongside native skills"
    );
}

/// AC3 variant: even when the project skill is `required: true`, the native
/// body takes precedence.
#[test]
fn required_project_visual_spec_still_cannot_shadow_native() {
    let project_visual_spec = crate::skills::ResolvedSkill {
        name: "visual-spec".to_string(),
        description: "Required project visual-spec".to_string(),
        content: "REQUIRED_PROJECT_BODY_SHOULD_NOT_APPEAR".to_string(),
        required: true,
        trust_level: "project".to_string(),
        recommended_for_roles: Vec::new(),
        tags: Vec::new(),
    };

    let prompt = render_prompt_with_skills(
        AgentType::Planner,
        "planner",
        vec![project_visual_spec],
        Some(
            crate::actors::slot::lifecycle::task_classifier::NativeSkillTrigger::ProposalAuthoring,
        ),
    );

    assert!(
        !prompt.contains("REQUIRED_PROJECT_BODY_SHOULD_NOT_APPEAR"),
        "even a required project visual-spec cannot shadow the native body"
    );
    assert!(
        prompt.contains("backtick"),
        "native backtick guidance must still be present"
    );
}

/// Non-planner roles that happen to have a project `visual-spec` skill in
/// their skills list should still see it — it's just the project version,
/// not the native one.  This confirms native filtering only applies to the
/// planner role where the native skill is recommended.
#[test]
fn non_planner_with_project_visual_spec_sees_project_body() {
    let project_visual_spec = crate::skills::ResolvedSkill {
        name: "visual-spec".to_string(),
        description: "Worker visual-spec".to_string(),
        content: "WORKER_PROJECT_VISUAL_SPEC_BODY".to_string(),
        required: false,
        trust_level: "project".to_string(),
        recommended_for_roles: Vec::new(),
        tags: Vec::new(),
    };

    let prompt = render_prompt_with_skills(
        AgentType::Worker,
        "worker",
        vec![project_visual_spec],
        Some(
            crate::actors::slot::lifecycle::task_classifier::NativeSkillTrigger::ProposalAuthoring,
        ),
    );

    assert!(
        prompt.contains("WORKER_PROJECT_VISUAL_SPEC_BODY"),
        "non-planner roles should see the project visual-spec body unmodified"
    );
}

/// The native skill's `required: true` flag ensures it is always inlined
/// even under progressive disclosure.  Verify the to_resolved conversion
/// preserves this flag.
#[test]
fn native_visual_spec_resolved_skill_is_marked_required() {
    let resolved = crate::native_skills::resolved_native_skills_for_role("planner");
    assert_eq!(resolved.len(), 1);
    assert!(
        resolved[0].required,
        "native visual-spec must be marked required so it is always inlined"
    );
    assert_eq!(
        resolved[0].trust_level, "platform",
        "native visual-spec trust_level must be 'platform'"
    );
}

/// Verify the native registry version stamp is consistent: the version
/// returned by `native_skill_version` matches `VISUAL_SPEC_VERSION` and
/// the version embedded in the `NativeSkill` entry.
#[test]
fn native_registry_version_stamp_is_consistent() {
    let version = crate::native_skills::VISUAL_SPEC_VERSION;
    assert_eq!(
        crate::native_skills::native_skill_version("visual-spec"),
        Some(version),
        "native_skill_version must match VISUAL_SPEC_VERSION constant"
    );

    let skill = crate::native_skills::native_skill("visual-spec")
        .expect("visual-spec must exist in native registry");
    assert_eq!(
        skill.version, version,
        "NativeSkill.version must match VISUAL_SPEC_VERSION constant"
    );
}

/// Verify that the native skill name "visual-spec" is recognized by the
/// control-plane's `is_native_skill_name` helper.  This is a cross-crate
/// alignment check that ensures the local allowlist in `djinn-control-plane`
/// stays in sync with the native registry.
#[test]
fn native_skill_name_recognized_by_control_plane() {
    assert!(
        djinn_control_plane::tools::agent_tools::is_native_skill_name("visual-spec"),
        "control-plane is_native_skill_name must recognize 'visual-spec'"
    );
    assert!(
        !djinn_control_plane::tools::agent_tools::is_native_skill_name("my-skill"),
        "control-plane is_native_skill_name must reject non-native names"
    );
}

// ── Planner prompt regressions for authoring-only visual-spec loading (y8p2) ──
//
// These end-to-end regression tests verify lazy native-skill behavior:
// `visual-spec` appears in proposal-authoring planner prompts (task shape
// drives the classifier → trigger fires → skill merged) and is absent from
// ordinary wave-planning/dispatch planner prompts (classifier returns None
// → skill not merged).
//
// Unlike the 5uzr tests above that pass an explicit trigger value, these
// tests use task fixtures + `classify_native_skill_trigger` to exercise the
// full classifier → merge → prompt pipeline.  Stable content-marker
// assertions are preferred over large brittle snapshots.

use crate::actors::slot::lifecycle::mcp_resolve::merge_native_skills;
use crate::actors::slot::lifecycle::task_classifier::NativeSkillTrigger;

/// Fixture: a proposal-authoring planner task (`epic_breakdown`).
fn make_authoring_planner_task() -> Task {
    let mut task = make_task();
    task.issue_type = "epic_breakdown".into();
    task.title = "Decompose proposal r0io into epics".into();
    task.description =
        "Break the graduated proposal r0io into implementation epics with acceptance criteria."
            .into();
    task
}

/// Fixture: a non-authoring wave-planning/dispatch planner task.
fn make_wave_planning_task() -> Task {
    let mut task = make_task();
    task.issue_type = "planning".into();
    task.title = "Plan next wave: Lazy native-skill prompt loading".into();
    task.description = "Plan the next wave of worker tasks for epic y8p2.".into();
    task
}

/// Render a planner prompt for `task`, using the task classifier to derive
/// the authoring trigger and merging native + project skills.
///
/// Mirrors the production pipeline: `classify_native_skill_trigger` →
/// `merge_native_skills` → `apply_skills`.
fn render_planner_prompt_for_task(task: &Task) -> String {
    ensure_registry();
    let ctx = make_ctx();
    let base = render_prompt(AgentType::Planner, task, &ctx);
    let trigger = crate::actors::slot::lifecycle::task_classifier::classify_native_skill_trigger(
        "planner", task,
    );
    let (merged, _native_names) = merge_native_skills("planner", Vec::new(), trigger);
    apply_skills(&base, &merged)
}

/// Regression (y8p2 AC1): an authoring planner prompt for an `epic_breakdown`
/// task contains the `visual-spec` native body and stable content markers.
///
/// The classifier fires `ProposalAuthoring` for `epic_breakdown`, so native
/// skills recommended for planner are merged into the prompt.
#[test]
fn authoring_planner_prompt_regression_contains_visual_spec() {
    let task = make_authoring_planner_task();

    // Verify the classifier fires for this task shape.
    assert_eq!(
        crate::actors::slot::lifecycle::task_classifier::classify_native_skill_trigger(
            "planner", &task,
        ),
        Some(NativeSkillTrigger::ProposalAuthoring),
        "epic_breakdown task must trigger ProposalAuthoring"
    );

    let prompt = render_planner_prompt_for_task(&task);

    // Skill name and section header present.
    assert!(
        prompt.contains("visual-spec"),
        "authoring planner prompt must contain visual-spec skill name"
    );
    assert!(
        prompt.contains("## Available Skills"),
        "authoring planner prompt must include the skills section header"
    );

    // Native body stable markers (from embedded SKILL.md).
    let lower = prompt.to_lowercase();
    assert!(
        prompt.contains("backtick"),
        "visual-spec body must contain the bare-angle backtick constraint"
    );
    assert!(
        lower.contains("progressive"),
        "visual-spec body must teach progressive markdown-to-MDX enrichment"
    );
    assert!(
        lower.contains("mdx"),
        "visual-spec body must mention MDX enrichment"
    );
    assert!(
        lower.contains("self-contained"),
        "visual-spec body must mention block authoring quality"
    );
    assert!(
        lower.contains("constitution") || lower.contains("case law"),
        "visual-spec body must contain the constitution/case-law memory analogy"
    );

    // Trust level marker for native skill.
    assert!(
        prompt.contains("platform"),
        "authoring planner prompt must show native skill trust_level 'platform'"
    );

    // Version stamp is available from the registry (stable marker).
    let version = crate::native_skills::VISUAL_SPEC_VERSION;
    assert!(!version.is_empty(), "VISUAL_SPEC_VERSION must be non-empty");
}

/// Regression (y8p2 AC2): a non-authoring wave-planning planner prompt for a
/// `planning` task does NOT contain `visual-spec` body, references, or
/// availability text.
///
/// The classifier returns `None` for `planning` tasks, so native skills are
/// not merged and the prompt pays no context cost for visual-spec.
#[test]
fn wave_planning_planner_prompt_regression_omits_visual_spec() {
    let task = make_wave_planning_task();

    // Verify the classifier does NOT fire for this task shape.
    assert_eq!(
        crate::actors::slot::lifecycle::task_classifier::classify_native_skill_trigger(
            "planner", &task,
        ),
        None,
        "planning task must not trigger native skill loading"
    );

    let prompt = render_planner_prompt_for_task(&task);

    // visual-spec name must be completely absent.
    assert!(
        !prompt.contains("visual-spec"),
        "non-authoring planner prompt must not contain visual-spec"
    );

    // No heavyweight native body markers.
    let lower = prompt.to_lowercase();
    assert!(
        !prompt.contains("backtick"),
        "non-authoring planner prompt must not contain visual-spec backtick guidance"
    );
    assert!(
        !lower.contains("progressive markdown-to-mdx"),
        "non-authoring planner prompt must not contain visual-spec enrichment guidance"
    );
    assert!(
        !lower.contains("constitution") && !lower.contains("case law"),
        "non-authoring planner prompt must not contain visual-spec memory analogy"
    );

    // If a skills section exists (from project skills), visual-spec must not
    // appear within it.
    if let Some(idx) = prompt.find("## Available Skills") {
        let skills_section = &prompt[idx..];
        assert!(
            !skills_section.contains("visual-spec"),
            "skills section must not list visual-spec in non-authoring prompt"
        );
    }
}

/// Regression (y8p2 AC2 variant): a non-authoring planner prompt with
/// project skills present still omits visual-spec while preserving the
/// project skills.
#[test]
fn wave_planning_with_project_skills_omits_visual_spec_but_keeps_project() {
    let task = make_wave_planning_task();
    ensure_registry();
    let ctx = make_ctx();
    let base = render_prompt(AgentType::Planner, &task, &ctx);

    let trigger = crate::actors::slot::lifecycle::task_classifier::classify_native_skill_trigger(
        "planner", &task,
    );
    let project_skills = vec![crate::skills::ResolvedSkill {
        name: "git".into(),
        description: "Git workflow".into(),
        content: "Git best practices from project.".into(),
        required: false,
        trust_level: "project".into(),
        recommended_for_roles: vec![],
        tags: vec![],
    }];
    let (merged, native_names) = merge_native_skills("planner", project_skills, trigger);
    let prompt = apply_skills(&base, &merged);

    // No native skills merged.
    assert!(
        native_names.is_empty(),
        "non-authoring planner must have no native skills"
    );

    // Project skill preserved.
    assert!(
        prompt.contains("Git best practices from project"),
        "project skills must be preserved in non-authoring planner prompt"
    );

    // visual-spec absent.
    assert!(
        !prompt.contains("visual-spec"),
        "non-authoring planner prompt must not contain visual-spec even with project skills"
    );
}

/// Regression (y8p2): the classifier end-to-end matches both positive and
/// negative task shapes used in the prompt regressions above.  This is a
/// focused sanity check that the two fixtures exercise distinct classifier
/// paths.
#[test]
fn classifier_distinguishes_authoring_from_wave_planning_tasks() {
    let authoring = make_authoring_planner_task();
    let planning = make_wave_planning_task();

    assert_eq!(
        crate::actors::slot::lifecycle::task_classifier::classify_native_skill_trigger(
            "planner", &authoring,
        ),
        Some(NativeSkillTrigger::ProposalAuthoring),
        "epic_breakdown task must classify as ProposalAuthoring"
    );
    assert_eq!(
        crate::actors::slot::lifecycle::task_classifier::classify_native_skill_trigger(
            "planner", &planning,
        ),
        None,
        "planning task must not classify as authoring"
    );
}

// ── Proposal-address prompt regressions (y4td) ───────────────────────────
//
// Verify that the proposal_address.md prompt text contains the expected
// workflow guidance for progressive markdown-to-MDX enrichment via targeted
// block patches, without inlining block vocabulary or forcing non-authoring
// planner prompts to pay the catalog/skill body cost.

#[test]
fn proposal_address_prompt_contains_block_patch_workflow_guidance() {
    let prompt = include_str!("../prompts/proposal_address.md");

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
    let prompt = include_str!("../prompts/proposal_address.md");

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
    let prompt = include_str!("../prompts/proposal_address.md");

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
