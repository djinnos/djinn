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
            ("prompts/base.md", include_str!("../../prompts/base.md")),
            ("prompts/dev.md", include_str!("../../prompts/dev.md")),
            (
                "prompts/worker/research.md",
                include_str!("../../prompts/worker/research.md"),
            ),
            (
                "prompts/worker/conflict.md",
                include_str!("../../prompts/worker/conflict.md"),
            ),
        ],
        AgentType::Reviewer => &[
            ("prompts/base.md", include_str!("../../prompts/base.md")),
            (
                "prompts/task-reviewer.md",
                include_str!("../../prompts/task-reviewer.md"),
            ),
        ],
        AgentType::Lead => &[
            ("prompts/base.md", include_str!("../../prompts/base.md")),
            ("prompts/lead.md", include_str!("../../prompts/lead.md")),
        ],
        AgentType::Planner => &[
            ("prompts/base.md", include_str!("../../prompts/base.md")),
            (
                "prompts/planner.md",
                include_str!("../../prompts/planner.md"),
            ),
            (
                "prompts/planner/decomposition.md",
                include_str!("../../prompts/planner/decomposition.md"),
            ),
            (
                "prompts/planner/intervention.md",
                include_str!("../../prompts/planner/intervention.md"),
            ),
            (
                "prompts/planner/proposal.md",
                include_str!("../../prompts/planner/proposal.md"),
            ),
            (
                "prompts/planner/proposal_review.md",
                include_str!("../../prompts/planner/proposal_review.md"),
            ),
        ],
        AgentType::Architect => &[
            ("prompts/base.md", include_str!("../../prompts/base.md")),
            (
                "prompts/architect.md",
                include_str!("../../prompts/architect.md"),
            ),
        ],
        AgentType::Advocate => &[
            ("prompts/base.md", include_str!("../../prompts/base.md")),
            (
                "prompts/advocate.md",
                include_str!("../../prompts/advocate.md"),
            ),
        ],
        AgentType::Adversary => &[
            ("prompts/base.md", include_str!("../../prompts/base.md")),
            (
                "prompts/adversary.md",
                include_str!("../../prompts/adversary.md"),
            ),
        ],
        AgentType::Judge => &[
            ("prompts/base.md", include_str!("../../prompts/base.md")),
            ("prompts/judge.md", include_str!("../../prompts/judge.md")),
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
    let skill_dir = project_root.path().join(".djinn").join("skills");
    std::fs::create_dir_all(&skill_dir).expect("create skill dir");
    std::fs::write(
        skill_dir.join("lockstep.md"),
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
