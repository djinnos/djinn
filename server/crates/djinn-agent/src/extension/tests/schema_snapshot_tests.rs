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
    };

    prompt_sources
        .iter()
        .flat_map(|(source, content)| extract_tool_references(source, content))
        .collect()
}

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
fn code_graph_schema_embeds_workflow_guidance() {
    let schema = serde_json::to_value(tool_code_graph()).expect("serialize code_graph schema");
    let description = schema
        .get("description")
        .and_then(|value| value.as_str())
        .expect("code_graph has a description");

    assert!(description.contains("WHEN TO USE"));
    assert!(description.contains("AFTER THIS"));
    for operation in [
        "capabilities",
        "query_subgraph",
        "search",
        "describe",
        "neighbors",
        "impact",
        "context",
        "complexity",
        "refactor_candidates",
        "workspaces",
        "workspace_hint",
        "available-workspace",
        "node_count",
        "commit_sha",
        "warmed_at",
        "status",
    ] {
        assert!(
            description.contains(operation),
            "code_graph guidance should mention {operation}"
        );
    }

    let workspace_description = schema
        .pointer("/inputSchema/properties/workspace/description")
        .and_then(|value| value.as_str())
        .expect("code_graph workspace property has a description");
    for required in [
        "operation=workspaces",
        "Empty string is treated as omitted",
        "hard-scope listing/bounded ops",
        "seed/endpoint resolution",
        "cross-workspace edges remain visible",
        "workspace_hint",
    ] {
        assert!(
            workspace_description.contains(required),
            "workspace schema description should mention {required}: {workspace_description}"
        );
    }
}

#[test]
fn proposal_ac_set_schema_remains_status_only() {
    let schema = serde_json::to_value(shared_schemas::tool_proposal_ac_set())
        .expect("serialize proposal_ac_set schema");
    assert_eq!(schema["name"], "proposal_ac_set");

    let description = schema["description"]
        .as_str()
        .expect("proposal_ac_set has a description");
    for required in [
        "`met` flags",
        "criterion text is preserved automatically",
        "status annotation only",
        "does not edit the spec",
        "bump a revision",
        "clear sign-offs",
    ] {
        assert!(
            description.contains(required),
            "proposal_ac_set description should mention {required}: {description}"
        );
    }

    assert_eq!(
        schema["inputSchema"]["required"],
        serde_json::json!(["id", "acceptance_criteria"])
    );
    assert_eq!(
        schema["inputSchema"]["properties"]["acceptance_criteria"]["items"]["properties"]["met"]["type"],
        serde_json::json!("boolean")
    );
}

#[test]
fn proposal_ac_amend_schema_documents_operations_and_reasons() {
    let schema = serde_json::to_value(shared_schemas::tool_proposal_ac_amend())
        .expect("serialize proposal_ac_amend schema");
    assert_eq!(schema["name"], "proposal_ac_amend");
    assert_eq!(
        schema["inputSchema"]["required"],
        serde_json::json!(["id", "reason", "amendments"])
    );
    assert_eq!(
        schema["inputSchema"]["properties"]["reason"]["minLength"],
        serde_json::json!(1)
    );
    assert_eq!(
        schema["inputSchema"]["properties"]["amendments"]["minItems"],
        serde_json::json!(1)
    );
    let item = &schema["inputSchema"]["properties"]["amendments"]["items"];
    assert_eq!(item["required"], serde_json::json!(["operation", "index"]));
    assert_eq!(
        item["properties"]["operation"]["enum"],
        serde_json::json!(["rewrite", "drop", "waive"])
    );
    assert_eq!(
        item["properties"]["index"]["type"],
        serde_json::json!("integer")
    );
    assert_eq!(item["properties"]["index"]["minimum"], serde_json::json!(0));
    assert_eq!(
        item["properties"]["criterion"]["description"],
        "New criterion text; required and non-empty when operation is rewrite."
    );
}

#[test]
fn proposal_reconcile_obsolete_epic_schema_documents_scope_and_blocking() {
    let schema = serde_json::to_value(shared_schemas::tool_proposal_reconcile_obsolete_epic())
        .expect("serialize proposal_reconcile_obsolete_epic schema");
    assert_eq!(schema["name"], "proposal_reconcile_obsolete_epic");
    let description = schema["description"]
        .as_str()
        .expect("proposal_reconcile_obsolete_epic has a description");
    for required in [
        "Scoped proposal-reconcile teardown",
        "blocks terminally",
        "AI proposal feedback",
        "unlinks only that epic",
        "leaves unrelated graduated epics untouched",
        "instead of whole-build proposal_stop_build",
    ] {
        assert!(
            description.contains(required),
            "proposal_reconcile_obsolete_epic description should mention {required}: {description}"
        );
    }
    assert_eq!(
        schema["inputSchema"]["required"],
        serde_json::json!(["proposal_id", "epic_id"])
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

#[test]
fn shell_timeout_defaults_and_minimum() {
    fn resolve_timeout(t: Option<u64>) -> u64 {
        t.unwrap_or(120_000).max(1000)
    }
    assert_eq!(resolve_timeout(None), 120_000);
    assert_eq!(resolve_timeout(Some(0)), 1000);
}

#[test]
fn tool_schemas_include_role_specific_tools() {
    fn schema_names(schemas: Vec<serde_json::Value>) -> Vec<String> {
        schemas
            .into_iter()
            .filter_map(|v| {
                v.get("name")
                    .and_then(|n| n.as_str())
                    .map(ToString::to_string)
            })
            .collect()
    }

    let worker = schema_names(tool_schemas_worker());
    assert!(worker.iter().any(|n| n == "shell"));
    assert!(worker.iter().any(|n| n == "write"));
    assert!(worker.iter().any(|n| n == "edit"));
    // Workers may now deliberately record durable knowledge (complements the
    // automatic post-session extraction); previously Architect-only.
    assert!(worker.iter().any(|n| n == "memory_write"));
    assert!(worker.iter().any(|n| n == "memory_edit"));
    assert!(worker.iter().any(|n| n == "memory_build_context"));
    assert!(worker.iter().any(|n| n == "submit_work"));
    assert!(!worker.iter().any(|n| n == "task_comment_add"));

    let reviewer = schema_names(tool_schemas_reviewer());
    assert!(reviewer.iter().any(|n| n == "submit_review"));
    assert!(!reviewer.iter().any(|n| n == "task_update_ac"));
    assert!(!reviewer.iter().any(|n| n == "task_comment_add"));

    let lead = schema_names(tool_schemas_lead());
    assert!(lead.iter().any(|n| n == "task_create"));
    assert!(lead.iter().any(|n| n == "submit_decision"));
    assert!(!lead.iter().any(|n| n == "task_transition"));
    assert!(!lead.iter().any(|n| n == "task_comment_add"));

    let planner = schema_names(tool_schemas_planner());
    assert!(planner.iter().any(|n| n == "write"));
    assert!(planner.iter().any(|n| n == "edit"));
    assert!(planner.iter().any(|n| n == "task_create"));
    assert!(planner.iter().any(|n| n == "task_transition"));
    assert!(planner.iter().any(|n| n == "submit_grooming"));
    assert!(planner.iter().any(|n| n == "proposal_ac_amend"));
    // The Planner may curate the KB while reshaping the board.
    assert!(planner.iter().any(|n| n == "memory_write"));
    assert!(planner.iter().any(|n| n == "memory_edit"));
    // The Planner leaves diagnostic comments on stuck tasks and mutates
    // learned_prompts for specialist agents during the effectiveness review.
    assert!(planner.iter().any(|n| n == "task_comment_add"));
    assert!(planner.iter().any(|n| n == "memory_health"));
    assert!(planner.iter().any(|n| n == "memory_broken_links"));
    assert!(planner.iter().any(|n| n == "memory_orphans"));
    assert!(planner.iter().any(|n| n == "memory_build_context"));
    assert!(planner.iter().any(|n| n == "agent_metrics"));
    assert!(planner.iter().any(|n| n == "agent_create"));
    assert!(planner.iter().any(|n| n == "agent_amend_prompt"));

    let architect = schema_names(tool_schemas_architect());
    assert!(architect.iter().any(|n| n == "shell"));
    assert!(architect.iter().any(|n| n == "read"));
    assert!(architect.iter().any(|n| n == "task_create"));
    assert!(architect.iter().any(|n| n == "task_comment_add"));
    assert!(architect.iter().any(|n| n == "task_transition"));
    assert!(architect.iter().any(|n| n == "task_kill_session"));
    assert!(architect.iter().any(|n| n == "memory_write"));
    assert!(architect.iter().any(|n| n == "memory_edit"));
    assert!(architect.iter().any(|n| n == "memory_move"));
    assert!(architect.iter().any(|n| n == "submit_work"));
    // Architect must NOT have code-writing tools.
    assert!(!architect.iter().any(|n| n == "write"));
    assert!(!architect.iter().any(|n| n == "edit"));
    assert!(!architect.iter().any(|n| n == "apply_patch"));
    // Per ADR-051 §1 `agent_amend_prompt` moved from Architect to Planner
    // (agent-effectiveness review is a Planner action, not a consultant action).
    assert!(!architect.iter().any(|n| n == "agent_amend_prompt"));
}

#[test]
fn ensure_path_within_worktree_accepts_in_tree_and_rejects_traversal() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-worktree-");
    let nested = worktree.path().join("nested");
    std::fs::create_dir_all(&nested).expect("create nested");
    let in_tree = nested.join("file.txt");
    ensure_path_within_worktree(&in_tree, worktree.path()).expect("in-tree path should pass");

    let traversal = worktree.path().join("..").join("..").join("escape.txt");
    let err = ensure_path_within_worktree(&traversal, worktree.path())
        .expect_err("traversal should be rejected");
    assert!(err.contains("outside worktree"));
}

#[test]
fn ensure_path_within_worktree_rejects_symlink_escape() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-worktree-");
    let outside = crate::test_helpers::test_tempdir("djinn-ext-outside-");
    let link = worktree.path().join("escape-link");

    #[cfg(unix)]
    std::os::unix::fs::symlink(outside.path(), &link).expect("create symlink");
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(outside.path(), &link).expect("create symlink");

    let escaped = link.join("leak.txt");
    let err = ensure_path_within_worktree(&escaped, worktree.path())
        .expect_err("symlink escape should be rejected");
    assert!(err.contains("outside worktree"));
}

#[test]
fn is_tool_allowed_for_schemas_handles_empty_and_invalid_entries() {
    assert!(!is_tool_allowed_for_schemas(&[], "shell"));

    let schemas = vec![
        serde_json::json!({}),
        serde_json::json!({"name": null}),
        serde_json::json!({"name": 42}),
        serde_json::json!({"name": "shell"}),
    ];
    assert!(is_tool_allowed_for_schemas(&schemas, "shell"));
    assert!(!is_tool_allowed_for_schemas(&schemas, "read"));
}

#[test]
fn resolve_path_handles_relative_absolute_and_normalization() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-resolve-");
    let base = worktree.path();

    let relative = resolve_path("src/main.rs", base);
    assert_eq!(relative, base.join("src/main.rs"));

    let absolute = resolve_path("/etc/hosts", base);
    assert_eq!(absolute, PathBuf::from("/etc/hosts"));

    let normalized = resolve_path("./src/../Cargo.toml", base);
    assert_eq!(normalized, base.join("Cargo.toml"));
}

#[test]
fn tool_schemas_include_concurrency_metadata() {
    let worker = tool_schemas_worker();
    assert_eq!(tool_schema(&worker, "task_show")["concurrent_safe"], true);
    assert_eq!(tool_schema(&worker, "read")["concurrent_safe"], true);
    assert_eq!(
        tool_schema(&worker, "github_search")["concurrent_safe"],
        true
    );
    assert_eq!(tool_schema(&worker, "shell")["concurrent_safe"], false);
    assert_eq!(tool_schema(&worker, "write")["concurrent_safe"], false);

    let architect = tool_schemas_architect();
    assert_eq!(
        tool_schema(&architect, "code_graph")["concurrent_safe"],
        true
    );
    assert_eq!(
        tool_schema(&architect, "memory_build_context")["concurrent_safe"],
        true
    );
    assert_eq!(
        tool_schema(&architect, "task_comment_add")["concurrent_safe"],
        false
    );
}

#[test]
fn tool_schemas_include_typed_safety_annotations() {
    let worker = tool_schemas_worker();

    let github_search = tool_schema(&worker, "github_search");
    assert_eq!(github_search["readOnly"], true);
    assert_eq!(github_search["destructive"], false);
    assert_eq!(github_search["idempotent"], true);
    assert_eq!(github_search["openWorld"], true);
    assert_eq!(github_search["concurrent_safe"], true);

    let write = tool_schema(&worker, "write");
    assert_eq!(write["readOnly"], false);
    assert_eq!(write["destructive"], true);
    assert_eq!(write["idempotent"], false);
    assert_eq!(write["openWorld"], false);
    assert_eq!(write["concurrent_safe"], false);
}

fn safety_tuple(schema: &serde_json::Value) -> (bool, bool, bool, bool) {
    let name = schema
        .get("name")
        .and_then(|value| value.as_str())
        .unwrap_or("<unnamed>");
    let field = |field_name: &str| {
        schema
            .get(field_name)
            .and_then(|value| value.as_bool())
            .unwrap_or_else(|| panic!("{name} missing boolean {field_name} safety annotation"))
    };

    (
        field("readOnly"),
        field("destructive"),
        field("idempotent"),
        field("openWorld"),
    )
}

fn expected_safety_tuple(name: &str) -> Option<(bool, bool, bool, bool)> {
    let read_only = (true, false, true, false);
    let open_world_read_only = (true, false, true, true);
    let mutation = (false, false, false, false);
    let idempotent_mutation = (false, false, true, false);
    let destructive = (false, true, false, false);
    let idempotent_destructive = (false, true, true, false);

    match name {
        "task_show"
        | "task_list"
        | "task_activity_list"
        | "task_blocked_list"
        | "epic_show"
        | "epic_tasks"
        | "epic_blockers_list"
        | "epic_blocked_list"
        | "proposal_show"
        | "memory_read"
        | "memory_search"
        | "memory_list"
        | "read"
        | "skill_read"
        | "lsp"
        | "ci_job_log"
        | "output_view"
        | "output_grep"
        | "memory_build_context"
        | "memory_health"
        | "memory_extracted_audit"
        | "memory_broken_links"
        | "memory_orphans"
        | "agent_metrics"
        | "pr_review_context" => Some(read_only),
        "code_search" | "github_search" | "code_graph" => Some(open_world_read_only),
        "task_update" | "epic_update" | "epic_close" | "proposal_ac_set" => {
            Some(idempotent_mutation)
        }
        "task_create" | "epic_create" | "task_transition" | "task_comment_add" | "memory_write"
        | "memory_edit" | "memory_move" | "request_lead" | "request_planner"
        | "proposal_ac_amend" | "submit_work" | "submit_review" | "submit_decision"
        | "submit_grooming" => Some(mutation),
        "shell"
        | "write"
        | "edit"
        | "apply_patch"
        | "task_delete_branch"
        | "task_archive_activity"
        | "task_kill_session"
        | "agent_create"
        | "agent_amend_prompt" => Some(destructive),
        "task_reset_counters" | "proposal_complete" => Some(idempotent_destructive),
        _ => None,
    }
}

#[test]
fn all_role_tool_schemas_have_pinned_safety_annotations() {
    for (role, schemas) in [
        ("worker", tool_schemas_worker()),
        ("reviewer", tool_schemas_reviewer()),
        ("lead", tool_schemas_lead()),
        ("planner", tool_schemas_planner()),
        ("architect", tool_schemas_architect()),
    ] {
        for schema in schemas {
            let name = schema
                .get("name")
                .and_then(|value| value.as_str())
                .expect("tool schema has a string name");
            let expected = expected_safety_tuple(name).unwrap_or_else(|| {
                panic!("{role} tool {name} is missing pinned safety classification")
            });
            assert_eq!(
                safety_tuple(&schema),
                expected,
                "{role} tool {name} safety classification changed without updating the invariant"
            );
        }
    }
}

#[test]
fn role_tool_schemas_pin_destructive_and_open_world_sets() {
    let mut destructive_tools = std::collections::BTreeSet::new();
    let mut open_world_read_only_tools = std::collections::BTreeSet::new();

    for schemas in [
        tool_schemas_worker(),
        tool_schemas_reviewer(),
        tool_schemas_lead(),
        tool_schemas_planner(),
        tool_schemas_architect(),
    ] {
        for schema in schemas {
            let name = schema
                .get("name")
                .and_then(|value| value.as_str())
                .expect("tool schema has a string name");
            let (read_only, destructive, idempotent, open_world) = safety_tuple(&schema);
            if destructive {
                destructive_tools.insert(name.to_string());
            }
            if read_only && idempotent && open_world {
                open_world_read_only_tools.insert(name.to_string());
            }
        }
    }

    let expected_destructive = std::collections::BTreeSet::from([
        "agent_amend_prompt".to_string(),
        "agent_create".to_string(),
        "apply_patch".to_string(),
        "edit".to_string(),
        "proposal_complete".to_string(),
        "shell".to_string(),
        "task_archive_activity".to_string(),
        "task_delete_branch".to_string(),
        "task_kill_session".to_string(),
        "task_reset_counters".to_string(),
        "write".to_string(),
    ]);
    assert_eq!(destructive_tools, expected_destructive);

    let expected_open_world_read_only = std::collections::BTreeSet::from([
        "code_graph".to_string(),
        "code_search".to_string(),
        "github_search".to_string(),
    ]);
    assert_eq!(open_world_read_only_tools, expected_open_world_read_only);
}

#[test]
fn snapshot_worker_tool_names() {
    let schemas = tool_schemas_worker();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("worker_tool_names", names);
}

#[test]
fn snapshot_worker_tool_schemas() {
    insta::assert_json_snapshot!("worker_tool_schemas", tool_schemas_worker());
}

#[test]
fn snapshot_reviewer_tool_names() {
    let schemas = tool_schemas_reviewer();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("reviewer_tool_names", names);
}

#[test]
fn snapshot_reviewer_tool_schemas() {
    insta::assert_json_snapshot!("reviewer_tool_schemas", tool_schemas_reviewer());
}

#[test]
fn snapshot_lead_tool_names() {
    let schemas = tool_schemas_lead();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("lead_tool_names", names);
}

#[test]
fn snapshot_lead_tool_schemas() {
    insta::assert_json_snapshot!("lead_tool_schemas", tool_schemas_lead());
}

#[test]
fn snapshot_planner_tool_names() {
    let schemas = tool_schemas_planner();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("planner_tool_names", names);
}

#[test]
fn snapshot_planner_tool_schemas() {
    insta::assert_json_snapshot!("planner_tool_schemas", tool_schemas_planner());
}

#[test]
fn snapshot_architect_tool_names() {
    let schemas = tool_schemas_architect();
    let names = tool_names(&schemas);
    insta::assert_json_snapshot!("architect_tool_names", names);
}

#[test]
fn snapshot_architect_tool_schemas() {
    insta::assert_json_snapshot!("architect_tool_schemas", tool_schemas_architect());
}

#[test]
fn snapshot_lsp_tool_schema() {
    insta::assert_json_snapshot!("lsp_tool_schema", serde_json::to_value(tool_lsp()).unwrap());
}

#[test]
fn snapshot_code_graph_tool_schema() {
    insta::assert_json_snapshot!(
        "code_graph_tool_schema",
        serde_json::to_value(tool_code_graph()).unwrap()
    );
}
