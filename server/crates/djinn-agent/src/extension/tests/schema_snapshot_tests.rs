//! Façade smoke tests for extension schema and prompt integration.
//!
//! The detailed schema snapshot and validation tests now live in
//! `djinn-mcp-extension::tests::schema_tests` where they test the
//! extracted code directly.  These façade tests verify that the
//! `djinn-agent` role registry, prompt files, and skills machinery
//! stay in lockstep with the tool schema surface owned by the
//! extension crate.

use super::*;
use std::collections::BTreeSet;

#[derive(Debug)]
struct ToolReference {
    source: String,
    name: String,
}

fn extract_tool_references(source: &str, content: &str) -> Vec<ToolReference> {
    // Conservative extractor: backticked identifiers that look like tool calls
    // (`name(...)`) are always considered; exact backticked identifiers are
    // considered only when they appear in a registry-derived tool-name union so
    // prose/code identifiers do not need to be mirrored in the test.
    let known_tool_names = all_registered_tool_names();
    let mut references = Vec::new();
    for capture in regex::Regex::new(r"`([a-z][a-z0-9_]*)(\s*\([^`]*\))?`")
        .expect("valid tool reference regex")
        .captures_iter(content)
    {
        let Some(name) = capture.get(1).map(|m| m.as_str()) else {
            continue;
        };
        if capture.get(2).is_some() || known_tool_names.contains(name) {
            references.push(ToolReference {
                source: source.to_string(),
                name: name.to_string(),
            });
        }
    }
    references
}

fn all_registered_tool_names() -> BTreeSet<String> {
    [
        AgentType::Worker,
        AgentType::Reviewer,
        AgentType::Lead,
        AgentType::Planner,
        AgentType::Architect,
        AgentType::Advocate,
        AgentType::Adversary,
        AgentType::Judge,
    ]
    .into_iter()
    .flat_map(tool_names_for_agent)
    .collect()
}

fn schema_name_set(schemas: &[serde_json::Value]) -> BTreeSet<String> {
    schemas
        .iter()
        .filter_map(|schema| {
            schema
                .get("name")
                .and_then(|name| name.as_str())
                .map(ToString::to_string)
        })
        .collect()
}

fn assert_tool_references_registered(agent_type: AgentType, references: &[ToolReference]) {
    let registered = tool_names_for_agent(agent_type);
    let missing: Vec<String> = references
        .iter()
        .filter(|reference| !registered.contains(&reference.name))
        .map(|reference| format!("{} references `{}`", reference.source, reference.name))
        .collect();
    assert!(
        missing.is_empty(),
        "{} prompt/skill references tools absent from its registered schema set:\n{}",
        agent_type.as_str(),
        missing.join("\n")
    );
}

fn prompt_references_for_agent(agent_type: AgentType) -> Vec<ToolReference> {
    let prompt_sources: &[(&str, &str)] = match agent_type {
        AgentType::Worker => &[
            (
                "prompts/base.md",
                include_str!("../../../../djinn-roles/src/prompts/base.md"),
            ),
            (
                "prompts/dev.md",
                include_str!("../../../../djinn-roles/src/prompts/dev.md"),
            ),
            (
                "prompts/worker/research.md",
                include_str!("../../../../djinn-roles/src/prompts/worker/research.md"),
            ),
            (
                "prompts/worker/conflict.md",
                include_str!("../../../../djinn-roles/src/prompts/worker/conflict.md"),
            ),
        ],
        AgentType::Reviewer => &[
            (
                "prompts/base.md",
                include_str!("../../../../djinn-roles/src/prompts/base.md"),
            ),
            (
                "prompts/task-reviewer.md",
                include_str!("../../../../djinn-roles/src/prompts/task-reviewer.md"),
            ),
        ],
        AgentType::Lead => &[
            (
                "prompts/base.md",
                include_str!("../../../../djinn-roles/src/prompts/base.md"),
            ),
            (
                "prompts/lead.md",
                include_str!("../../../../djinn-roles/src/prompts/lead.md"),
            ),
        ],
        AgentType::Planner => &[
            (
                "prompts/base.md",
                include_str!("../../../../djinn-roles/src/prompts/base.md"),
            ),
            (
                "prompts/planner.md",
                include_str!("../../../../djinn-roles/src/prompts/planner.md"),
            ),
            (
                "prompts/planner/decomposition.md",
                include_str!("../../../../djinn-roles/src/prompts/planner/decomposition.md"),
            ),
            (
                "prompts/planner/intervention.md",
                include_str!("../../../../djinn-roles/src/prompts/planner/intervention.md"),
            ),
            (
                "prompts/planner/proposal.md",
                include_str!("../../../../djinn-roles/src/prompts/planner/proposal.md"),
            ),
            (
                "prompts/planner/proposal_review.md",
                include_str!("../../../../djinn-roles/src/prompts/planner/proposal_review.md"),
            ),
        ],
        AgentType::Architect => &[
            (
                "prompts/base.md",
                include_str!("../../../../djinn-roles/src/prompts/base.md"),
            ),
            (
                "prompts/architect.md",
                include_str!("../../../../djinn-roles/src/prompts/architect.md"),
            ),
        ],
        AgentType::Advocate => &[
            (
                "prompts/base.md",
                include_str!("../../../../djinn-roles/src/prompts/base.md"),
            ),
            (
                "prompts/advocate.md",
                include_str!("../../../../djinn-roles/src/prompts/advocate.md"),
            ),
        ],
        AgentType::Adversary => &[
            (
                "prompts/base.md",
                include_str!("../../../../djinn-roles/src/prompts/base.md"),
            ),
            (
                "prompts/adversary.md",
                include_str!("../../../../djinn-roles/src/prompts/adversary.md"),
            ),
        ],
        AgentType::Judge => &[
            (
                "prompts/base.md",
                include_str!("../../../../djinn-roles/src/prompts/base.md"),
            ),
            (
                "prompts/judge.md",
                include_str!("../../../../djinn-roles/src/prompts/judge.md"),
            ),
        ],
    };

    prompt_sources
        .iter()
        .flat_map(|(source, content)| extract_tool_references(source, content))
        .collect()
}

// ── façade smoke tests ─────────────────────────────────────────────────

#[test]
fn tool_reference_extractor_catches_stale_tool_call_names() {
    let names: BTreeSet<String> = extract_tool_references(
        "fixture",
        "Ignore `NotATool` and `String`, but catch `stale_tool_name()`.",
    )
    .into_iter()
    .map(|reference| reference.name)
    .collect();

    assert!(names.contains("stale_tool_name"));
    assert!(!names.contains("NotATool"));
    assert!(!names.contains("String"));
}

#[test]
fn role_prompts_reference_only_registered_tools() {
    for agent_type in [
        AgentType::Worker,
        AgentType::Reviewer,
        AgentType::Lead,
        AgentType::Planner,
        AgentType::Architect,
        AgentType::Advocate,
        AgentType::Adversary,
        AgentType::Judge,
    ] {
        assert_tool_references_registered(agent_type, &prompt_references_for_agent(agent_type));
    }
}

#[test]
fn role_schema_snapshots_match_registered_role_name_source() {
    for (agent_type, schemas) in [
        (AgentType::Worker, tool_schemas_worker()),
        (AgentType::Reviewer, tool_schemas_reviewer()),
        (AgentType::Lead, tool_schemas_lead()),
        (AgentType::Planner, tool_schemas_planner()),
        (AgentType::Architect, tool_schemas_architect()),
        (AgentType::Advocate, tool_schemas_advocate()),
        (AgentType::Adversary, tool_schemas_adversary()),
        (AgentType::Judge, tool_schemas_judge()),
    ] {
        assert_eq!(
            schema_name_set(&schemas),
            tool_names_for_agent(agent_type),
            "{} schema snapshot helpers drifted from the registered role tool-name source",
            agent_type.as_str()
        );
    }
}

#[test]
fn adr_050_code_graph_boundary_is_architect_and_chat_only() {
    // ADR-050 keeps `code_graph` on the Architect/Chat surfaces only. Prompt
    // lockstep must not mask a worker/reviewer/lead/planner mention by treating
    // it as valid for those role schema sets.
    for agent_type in [
        AgentType::Worker,
        AgentType::Reviewer,
        AgentType::Lead,
        AgentType::Planner,
    ] {
        assert!(
            !tool_names_for_agent(agent_type).contains("code_graph"),
            "{} must not expose code_graph per ADR-050",
            agent_type.as_str()
        );
    }
    assert!(tool_names_for_agent(AgentType::Architect).contains("code_graph"));

    let chat_names = schema_name_set(&crate::chat_tools::chat_extension_tool_schemas());
    assert!(
        chat_names.contains("code_graph"),
        "chat must expose code_graph per ADR-050"
    );
}

#[test]
fn loaded_skills_and_progressive_disclosure_reference_only_registered_worker_tools() {
    let project_root = crate::test_helpers::test_tempdir("djinn-skill-lockstep-");
    let skill_dir = project_root
        .path()
        .join(".claude")
        .join("skills")
        .join("lockstep");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    std::fs::write(
        skill_dir.join("SKILL.md"),
        r#"---
name: lockstep
	description: Fixture that references real worker tools.
---
Use `read(file_path="Cargo.toml")` before edits, then finish with `submit_work(...)`.
"#,
    )
    .expect("write skill fixture");

    let skills = crate::skills::load_skills(project_root.path(), &["lockstep".to_string()]);
    assert_eq!(skills.len(), 1);

    let loaded_skill_references = extract_tool_references("skill:lockstep", &skills[0].content);
    assert_tool_references_registered(AgentType::Worker, &loaded_skill_references);

    let progressive_section = crate::skills::format_skills_section_with(&skills, true);
    let disclosure_references =
        extract_tool_references("skills progressive disclosure", &progressive_section);
    assert!(
        disclosure_references
            .iter()
            .any(|reference| reference.name == "skill_read"),
        "progressive disclosure guidance should reference the skill_read tool"
    );
    assert_tool_references_registered(AgentType::Worker, &disclosure_references);
}

#[test]
fn worker_cannot_use_lead_only_tool() {
    // submit_decision is lead-only (ADR-036: finalize tools are role-specific).
    assert!(!is_tool_allowed_for_agent(
        AgentType::Worker,
        "submit_decision"
    ));
    assert!(is_tool_allowed_for_agent(
        AgentType::Lead,
        "submit_decision"
    ));
    // task_transition is not in the lead tool set (removed by ADR-036).
    assert!(!is_tool_allowed_for_agent(
        AgentType::Lead,
        "task_transition"
    ));
}

// ── insta schema snapshot tests ──────────────────────────────────────
// These keep the `djinn-agent` extension schema snapshots in lockstep with
// the `djinn-mcp-extension` schema tests.  Both surfaces expose the same
// tool_edit() description and `path`/`old_text`/`new_text` input contract.

#[test]
fn worker_tool_schemas() {
    insta::assert_json_snapshot!(tool_schemas_worker());
}

#[test]
fn planner_tool_schemas() {
    insta::assert_json_snapshot!(tool_schemas_planner());
}

#[test]
fn lead_tool_schemas() {
    insta::assert_json_snapshot!(tool_schemas_lead());
}

// ── Cut-over regression guards (10qg) ───────────────────────────────────
// These assertions pin the role tool surfaces so future changes cannot
// regress the request_lead → request_planner cut-over or the Lead
// arbiter-only surface.

/// Worker and reviewer schemas must expose `request_planner` and must NOT
/// expose `request_lead` (epic 10qg).
#[test]
fn worker_reviewer_schemas_expose_request_planner_not_request_lead() {
    for agent_type in [AgentType::Worker, AgentType::Reviewer] {
        let names = tool_names_for_agent(agent_type);
        assert!(
            names.contains("request_planner"),
            "{} must expose request_planner in its tool schema, got: {:?}",
            agent_type.as_str(),
            names
        );
        assert!(
            !names.contains("request_lead"),
            "{} must NOT expose request_lead in its tool schema, got: {:?}",
            agent_type.as_str(),
            names
        );
    }
}

/// Lead schema must NOT expose `request_planner` or `escalate` — the Lead
/// only uses `submit_decision` as its finalize tool (10qg).
#[test]
fn lead_schema_does_not_expose_request_planner_or_escalate() {
    let names = tool_names_for_agent(AgentType::Lead);
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

/// The `call_request_lead` handler is test-only (`#[cfg(test)]` re-export)
/// and is NOT in the production fallback dispatch.  This structural guard
/// ensures that if someone adds `request_lead` to the production dispatch
/// match, this test catches the drift.
#[test]
fn production_dispatch_does_not_handle_request_lead() {
    // Read the local fallback dispatch source to verify request_lead is absent.
    let dispatch_src = include_str!("../handlers.rs");

    // The production dispatch match must handle request_planner.
    assert!(
        dispatch_src.contains("\"request_planner\""),
        "production dispatch must handle request_planner"
    );

    // The production dispatch match must NOT contain an active
    // request_lead arm.  A cfg(test)-only re-export is fine (it's
    // dead code in production), but the dispatch match itself must
    // not route request_lead.
    let in_dispatch_match = dispatch_src
        .match_indices("\"request_lead\"")
        .filter(|(idx, _)| {
            // Count only occurrences inside the dispatch match block,
            // not in comments or cfg(test) re-exports.  A naive but
            // effective guard: every occurrence in the match block is
            // preceded by whitespace+| or whitespace+=>.
            let before = &dispatch_src[..*idx];
            let last_line = before.lines().last().unwrap_or("");
            let trimmed = last_line.trim();
            trimmed.starts_with('"') || trimmed.starts_with('|')
        })
        .count();

    assert_eq!(
        in_dispatch_match, 0,
        "production dispatch match must NOT route request_lead (found {in_dispatch_match} active arms)"
    );
}

/// The `deprecated_request_lead` drain path is cfg(test)-only in the
/// handler re-export.  This structural assertion documents that the
/// handler is unreachable from production sessions.
#[test]
fn request_lead_handler_is_test_only_reexport() {
    // The handlers.rs file re-exports call_request_lead under #[cfg(test)].
    let handlers_src = include_str!("../handlers.rs");

    assert!(
        handlers_src.contains("#[cfg(test)]"),
        "handlers.rs must contain cfg(test) gate for test-only re-exports"
    );

    // Verify that call_request_lead appears only in cfg(test) context,
    // not in the main production re-export block.
    let lines: Vec<&str> = handlers_src.lines().collect();
    let mut in_cfg_test = false;
    let mut found_test_only = false;
    for line in &lines {
        let trimmed = line.trim();
        if trimmed == "#[cfg(test)]" {
            in_cfg_test = true;
        } else if in_cfg_test && trimmed.contains("call_request_lead") {
            found_test_only = true;
            break;
        } else if !trimmed.starts_with("//") && !trimmed.is_empty() && !trimmed.starts_with("#[") {
            in_cfg_test = false;
        }
    }
    assert!(
        found_test_only,
        "call_request_lead must be re-exported under #[cfg(test)] only"
    );
}

/// The production handler dispatch body (`handlers.rs`) must NOT contain
/// any code that transitions a task to `needs_lead_intervention`.
///
/// The only production path into `needs_lead_intervention` is the
/// coordinator arbiter park-rung / second-strike `Escalate` transition
/// (retry.rs / session_recovery.rs).  Worker/reviewer handler code must
/// never directly transition a task there.
#[test]
fn production_handler_dispatch_does_not_transition_to_needs_lead_intervention() {
    let dispatch_src = include_str!("../handlers.rs");
    // Strip `//` comment lines — only actual code usage triggers failure.
    let code_only: String = dispatch_src
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect();
    assert!(
        !code_only.contains("needs_lead_intervention"),
        "production handler dispatch (handlers.rs) must NOT contain \
         needs_lead_intervention in code — only the coordinator arbiter \
         park-rung path may transition tasks there"
    );
}

/// The deprecated `call_request_lead` handler (`task_epic.rs`) must NOT
/// contain any code that transitions a task to `needs_lead_intervention`.
/// It routes to `dispatch_planner_escalation` instead.
#[test]
fn deprecated_request_lead_handler_does_not_transition_to_needs_lead_intervention() {
    let src = include_str!("../handlers/task_epic.rs");

    // Find the call_request_lead function body.
    let fn_start = src
        .find("async fn call_request_lead")
        .expect("call_request_lead must exist");
    let after_fn = &src[fn_start..];
    // Find the next pub fn / async fn to bound the search.
    let fn_body_end = after_fn[28..]
        .find("\npub")
        .or_else(|| after_fn[28..].find("\nasync fn"))
        .map(|p| p + 28)
        .unwrap_or(after_fn.len());
    let fn_body = &after_fn[..fn_body_end];

    // Strip comment lines before searching.
    let code_only: String = fn_body
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect();
    assert!(
        !code_only.contains("needs_lead_intervention"),
        "call_request_lead must NOT transition task to needs_lead_intervention; \
         it should route to dispatch_planner_escalation only"
    );
    assert!(
        code_only.contains("dispatch_planner_escalation"),
        "call_request_lead must route through dispatch_planner_escalation"
    );
}
