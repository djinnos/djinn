use super::*;

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
    assert!(worker.iter().any(|n| n == "memory_write"));
    assert!(worker.iter().any(|n| n == "memory_edit"));
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
    assert!(planner.iter().any(|n| n == "task_create"));
    assert!(planner.iter().any(|n| n == "task_transition"));
    assert!(planner.iter().any(|n| n == "submit_grooming"));
    assert!(planner.iter().any(|n| n == "memory_write"));
    assert!(planner.iter().any(|n| n == "memory_edit"));
    // Per ADR-051 §1 the Planner now runs patrol mode, which needs to leave
    // diagnostic comments on stuck tasks and mutate learned_prompts for
    // specialist agents during the effectiveness review.
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
    // (agent-effectiveness review is a patrol action, not a consultant action).
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
