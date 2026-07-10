// ── Integrated native visual-spec regressions (epic 5uzr) ───────────────
//
// These tests verify end-to-end behavior across the native skill registry,
// lifecycle skill resolution, and prompt rendering.  They exercise the
// acceptance criteria for task jll3: native visual-spec is included in
// planner prompts by default with active version and authoring guidance,
// non-planner roles do not receive it, project/worktree skills cannot
// shadow the native body, and the rendered prompt exposes the expected
// content.

use super::{ensure_registry, make_ctx, make_task};
use crate::AgentType;
use crate::prompts::{apply_skills, render_prompt};
use djinn_core::models::Task;

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
        lower.contains("filetree") && lower.contains("annotatedcode"),
        "visual-spec content must map content kinds to concrete MDX blocks"
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
    // Note: "progressive" was previously asserted here but was sourced from
    // a tool description (skill_read), not the visual-spec SKILL.md body.
    // After wzz6 item 1 removed per-tool descriptions from the prompt tools
    // section, this line was removed.  The SKILL.md body says "enrich it
    // toward MDX" (not "progressive").
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
