//! Contract tests for `memory_*` + `propose_adr_*` MCP tools (worktree-free).
//!
//! Migrated from `server/src/mcp_contract_tests/memory_tools/contract_tests.rs`.
//! Four worktree-header tests (`mcp_memory_write_edit_delete_use_worktree_root_header_for_file_ops`,
//! `mcp_singleton_memory_writes_use_canonical_project_root_and_mirror_worktree`,
//! `mcp_current_requirement_edits_use_canonical_project_root_and_mirror_worktree`,
//! `mcp_proposal_pipeline_regression_recovers_worktree_draft_survives_sync_and_lists`)
//! remain in the server crate: they exercise the `x-djinn-worktree-root`
//! header handling, which the HTTP harness routes via `dispatch_tool_with_worktree`
//! — a surface our bare `call_tool(name, args)` entrypoint does not expose.

#[path = "common/mod.rs"]
mod common;

use std::path::Path;

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::events::EventBus;
use djinn_db::{NoteRepository, ProjectRepository};
use serde_json::json;

#[tokio::test]
async fn mcp_memory_write_success_shape_and_duplicate_permalink_error() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();
    let (proj, _dir) = common::create_test_project_with_dir(&db).await;
    let project = &proj.path;

    let created = harness
        .call_tool(
            "memory_write",
            json!({
                "project": project,
                "title": "Write Contract Note",
                "content": "body",
                "type": "adr"
            }),
        )
        .await
        .expect("memory_write should dispatch");

    assert!(created.get("id").and_then(|v| v.as_str()).is_some());
    assert_eq!(created["title"], "Write Contract Note");
    assert_eq!(created["note_type"], "adr");
    assert!(created.get("permalink").and_then(|v| v.as_str()).is_some());

    let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
    let project_id: String = project_repo.resolve_or_create(project).await.unwrap();
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .get_by_permalink(&project_id, created["permalink"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(
        Path::new(&note.file_path).exists(),
        "canonical DB path is stored at {} even though writes may target worktrees",
        note.file_path
    );

    let duplicate = harness
        .call_tool(
            "memory_write",
            json!({
                "project": project,
                "title": "Write Contract Note",
                "content": "body-2",
                "type": "adr"
            }),
        )
        .await
        .expect("duplicate memory_write should dispatch");

    assert!(duplicate.get("error").is_some());
}

#[tokio::test]
async fn mcp_memory_write_and_move_accept_case_and_pitfall_types() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    let created = harness
        .call_tool(
            "memory_write",
            json!({
                "project": project,
                "title": "Recovered Incident",
                "content": "body",
                "type": "case"
            }),
        )
        .await
        .expect("memory_write should dispatch");

    assert_eq!(created["note_type"], "case");
    assert_eq!(created["folder"], "cases");
    assert_eq!(created["permalink"], "cases/recovered-incident");

    let moved = harness
        .call_tool(
            "memory_move",
            json!({
                "project": project,
                "identifier": created["permalink"],
                "type": "pitfall"
            }),
        )
        .await
        .expect("memory_move should dispatch");

    assert_eq!(moved["note_type"], "pitfall");
    assert_eq!(moved["folder"], "pitfalls");
    assert_eq!(moved["permalink"], "pitfalls/recovered-incident");
}

#[tokio::test]
async fn mcp_memory_read_by_permalink_by_title_and_not_found_error() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    let created = harness
        .call_tool(
            "memory_write",
            json!({
                "project": project,
                "title": "Read Contract Note",
                "content": "read me",
                "type": "reference"
            }),
        )
        .await
        .expect("memory_write should dispatch");

    let by_permalink = harness
        .call_tool(
            "memory_read",
            json!({ "project": project, "identifier": created["permalink"] }),
        )
        .await
        .expect("memory_read by permalink should dispatch");
    assert_eq!(by_permalink["title"], "Read Contract Note");

    let by_title = harness
        .call_tool(
            "memory_read",
            json!({ "project": project, "identifier": "Read Contract Note" }),
        )
        .await
        .expect("memory_read by title should dispatch");
    assert_eq!(by_title["permalink"], created["permalink"]);

    let missing = harness
        .call_tool(
            "memory_read",
            json!({ "project": project, "identifier": "does-not-exist" }),
        )
        .await
        .expect("memory_read missing should dispatch");
    assert!(missing.get("error").is_some());
}

#[tokio::test]
async fn mcp_memory_search_returns_ranked_results_with_snippets_and_filters() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Rust Alpha", "content": "rust rust rust memory", "type": "reference"}),
        )
        .await
        .expect("memory_write alpha should dispatch");
    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Rust Beta", "content": "rust memory", "type": "reference"}),
        )
        .await
        .expect("memory_write beta should dispatch");
    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "ADR Gamma", "content": "rust decision", "type": "adr"}),
        )
        .await
        .expect("memory_write gamma should dispatch");

    let searched = harness
        .call_tool(
            "memory_search",
            json!({"project": project, "query": "rust", "limit": 10}),
        )
        .await
        .expect("memory_search should dispatch");
    let results = searched["results"].as_array().unwrap();
    assert!(results.len() >= 2);
    assert!(results[0].get("snippet").is_some());

    let by_folder = harness
        .call_tool(
            "memory_search",
            json!({"project": project, "query": "rust", "folder": "decisions"}),
        )
        .await
        .expect("memory_search by folder should dispatch");
    for r in by_folder["results"].as_array().unwrap() {
        assert_eq!(r["folder"], "decisions");
    }

    let by_type = harness
        .call_tool(
            "memory_search",
            json!({"project": project, "query": "rust", "type": "adr"}),
        )
        .await
        .expect("memory_search by type should dispatch");
    for r in by_type["results"].as_array().unwrap() {
        assert_eq!(r["note_type"], "adr");
    }
}

#[tokio::test]
async fn mcp_memory_edit_append_prepend_replace_and_missing_note_error() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Edit Note", "content": "middle", "type": "reference"}),
        )
        .await
        .expect("seed memory_write should dispatch");

    let appended = harness
        .call_tool(
            "memory_edit",
            json!({"project": project, "identifier": "Edit Note", "operation": "append", "content": "tail"}),
        )
        .await
        .expect("memory_edit append should dispatch");
    assert!(appended["content"].as_str().unwrap().contains("tail"));

    let prepended = harness
        .call_tool(
            "memory_edit",
            json!({"project": project, "identifier": "Edit Note", "operation": "prepend", "content": "head"}),
        )
        .await
        .expect("memory_edit prepend should dispatch");
    assert!(prepended["content"].as_str().unwrap().starts_with("head"));

    let replaced = harness
        .call_tool(
            "memory_edit",
            json!({"project": project, "identifier": "Edit Note", "operation": "find_replace", "find_text": "middle", "content": "center"}),
        )
        .await
        .expect("memory_edit find_replace should dispatch");
    assert!(replaced["content"].as_str().unwrap().contains("center"));

    let missing = harness
        .call_tool(
            "memory_edit",
            json!({"project": project, "identifier": "Missing", "operation": "append", "content": "x"}),
        )
        .await
        .expect("memory_edit missing should dispatch");
    assert!(missing.get("error").is_some());
}

#[tokio::test]
async fn mcp_memory_move_changes_folder_title_and_permalink() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    let created = harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Move Me", "content": "content", "type": "reference"}),
        )
        .await
        .expect("memory_write should dispatch");

    let moved = harness
        .call_tool(
            "memory_move",
            json!({"project": project, "identifier": created["permalink"], "title": "Moved Title", "type": "research"}),
        )
        .await
        .expect("memory_move should dispatch");
    assert_eq!(moved["title"], "Moved Title");
    assert_eq!(moved["folder"], "research");
    assert_ne!(moved["permalink"], created["permalink"]);
}

#[tokio::test]
async fn mcp_memory_move_can_recover_proposed_adr_and_make_it_visible_to_proposal_list() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    let created = harness
        .call_tool(
            "memory_write",
            json!({
                "project": project,
                "title": "Recover Me",
                "content": "---\nwork_shape: epic\n---\n\n# Recover Me\n",
                "type": "adr"
            }),
        )
        .await
        .expect("memory_write should dispatch");

    let moved = harness
        .call_tool(
            "memory_move",
            json!({
                "project": project,
                "identifier": created["permalink"],
                "type": "proposed_adr"
            }),
        )
        .await
        .expect("memory_move should dispatch");

    assert_eq!(moved["note_type"], "proposed_adr");
    assert_eq!(moved["folder"], "decisions/proposed");
    assert!(Path::new(moved["file_path"].as_str().unwrap()).exists());

    let proposals = harness
        .call_tool("propose_adr_list", json!({"project": project}))
        .await
        .expect("propose_adr_list should dispatch");

    assert!(proposals["items"].as_array().is_some_and(|items| {
        items.iter().any(|item| item["title"] == "Recover Me")
    }));
}

#[tokio::test]
async fn mcp_memory_delete_success_and_missing_note_error() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Delete Me", "content": "bye", "type": "reference"}),
        )
        .await
        .expect("seed memory_write should dispatch");

    let deleted = harness
        .call_tool(
            "memory_delete",
            json!({"project": project, "identifier": "Delete Me"}),
        )
        .await
        .expect("memory_delete should dispatch");
    assert_eq!(deleted["ok"], true);

    let missing = harness
        .call_tool(
            "memory_delete",
            json!({"project": project, "identifier": "Delete Me"}),
        )
        .await
        .expect("memory_delete missing should dispatch");
    assert_eq!(missing["ok"], false);
    assert!(missing.get("error").is_some());
}

#[tokio::test]
async fn mcp_memory_list_all_and_filters_by_folder_and_type() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    let adr = harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "A", "content": "x", "type": "adr"}),
        )
        .await
        .expect("memory_write adr should dispatch");
    assert_eq!(adr["deduplicated"], false);
    let reference = harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "B", "content": "different content", "type": "reference"}),
        )
        .await
        .expect("memory_write reference should dispatch");
    assert_eq!(reference["deduplicated"], false);

    let all = harness
        .call_tool("memory_list", json!({"project": project}))
        .await
        .expect("memory_list should dispatch");
    assert_eq!(all["notes"].as_array().unwrap().len(), 2);

    let folder = harness
        .call_tool(
            "memory_list",
            json!({"project": project, "folder": "decisions"}),
        )
        .await
        .expect("memory_list by folder should dispatch");
    for n in folder["notes"].as_array().unwrap() {
        assert_eq!(n["folder"], "decisions");
    }

    let typed = harness
        .call_tool(
            "memory_list",
            json!({"project": project, "type": "reference"}),
        )
        .await
        .expect("memory_list by type should dispatch");
    for n in typed["notes"].as_array().unwrap() {
        assert_eq!(n["note_type"], "reference");
    }
}

#[tokio::test]
async fn mcp_memory_graph_returns_wikilink_edges() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Node B", "content": "b", "type": "reference"}),
        )
        .await
        .expect("seed node B should dispatch");
    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Node A", "content": "links [[Node B]]", "type": "reference"}),
        )
        .await
        .expect("seed node A should dispatch");

    let graph = harness
        .call_tool("memory_graph", json!({"project": project}))
        .await
        .expect("memory_graph should dispatch");
    assert!(!graph["edges"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn mcp_memory_recent_orders_by_last_accessed() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Older", "content": "o", "type": "reference"}),
        )
        .await
        .expect("memory_write older should dispatch");
    // `memory_recent` orders by `updated_at` (3ms precision); without a gap
    // the two writes below can land in the same millisecond and the secondary
    // sort is implementation-defined.  Under parallel cargo-test contention
    // 100ms is tight — 500ms gives the DB a clear timestamp boundary while
    // still keeping total runtime sub-second.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Newer", "content": "n", "type": "reference"}),
        )
        .await
        .expect("memory_write newer should dispatch");
    harness
        .call_tool(
            "memory_read",
            json!({"project": project, "identifier": "Older"}),
        )
        .await
        .expect("memory_read older should dispatch");
    harness
        .call_tool(
            "memory_read",
            json!({"project": project, "identifier": "Newer"}),
        )
        .await
        .expect("memory_read newer should dispatch");

    let recent = harness
        .call_tool(
            "memory_recent",
            json!({"project": project, "timeframe": "7d", "limit": 2}),
        )
        .await
        .expect("memory_recent should dispatch");
    let notes = recent["notes"].as_array().unwrap();
    assert_eq!(
        notes.len(),
        2,
        "expected both notes in recent result, got: {recent}"
    );
    assert_eq!(notes[0]["title"], "Newer");
}

#[tokio::test]
async fn mcp_memory_catalog_returns_structured_catalog() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Catalog Item", "content": "c", "type": "reference"}),
        )
        .await
        .expect("memory_write should dispatch");
    let catalog = harness
        .call_tool("memory_catalog", json!({"project": project}))
        .await
        .expect("memory_catalog should dispatch");
    assert!(
        catalog["catalog"]
            .as_str()
            .unwrap()
            .contains("Catalog Item")
    );
}

#[tokio::test]
async fn mcp_memory_health_orphans_and_broken_links_shapes() {
    let harness = McpTestHarness::new().await;
    let project = "/tmp/mcp-memory-health";

    // No project seeded: memory_write resolves and errors silently; the test
    // only asserts the shape of the three health / orphans / broken_links
    // responses, so that's fine.
    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Source", "content": "[[Missing Target]]", "type": "reference"}),
        )
        .await
        .expect("memory_write should dispatch");

    let health = harness
        .call_tool("memory_health", json!({"project": project}))
        .await
        .expect("memory_health should dispatch");
    assert!(health.get("orphan_note_count").is_some());
    assert!(health.get("broken_link_count").is_some());
    assert!(health.get("duplicate_cluster_count").is_some());
    assert!(health.get("low_confidence_note_count").is_some());
    assert!(health.get("stale_note_count").is_some());

    let orphans = harness
        .call_tool("memory_orphans", json!({"project": project}))
        .await
        .expect("memory_orphans should dispatch");
    assert!(orphans["orphans"].is_array());

    let broken = harness
        .call_tool("memory_broken_links", json!({"project": project}))
        .await
        .expect("memory_broken_links should dispatch");
    assert!(broken["broken_links"].is_array());
}

#[tokio::test]
async fn mcp_memory_history_and_diff_round_trip() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    let created = harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "History Diff", "content": "line one", "type": "reference"}),
        )
        .await
        .expect("memory_write should dispatch");
    let permalink = created["permalink"].as_str().unwrap().to_string();

    let edited = harness
        .call_tool(
            "memory_edit",
            json!({"project": project, "identifier": permalink, "operation": "append", "content": "line two"}),
        )
        .await
        .expect("memory_edit should dispatch");
    assert!(edited.get("error").is_none() || edited["error"].is_null());

    let history = harness
        .call_tool(
            "memory_history",
            json!({"project": project, "permalink": created["permalink"], "limit": 10}),
        )
        .await
        .expect("memory_history should dispatch");
    assert!(history.get("error").is_none() || history["error"].is_null());

    let entries = history["history"]
        .as_array()
        .or_else(|| history["entries"].as_array())
        .expect("memory_history should return history/entries array");

    if entries.is_empty() {
        let diff = harness
            .call_tool(
                "memory_diff",
                json!({"project": project, "permalink": created["permalink"]}),
            )
            .await
            .expect("memory_diff should dispatch");
        assert!(diff.get("error").is_none() || diff["error"].is_null());
        let d = diff["diff"].as_str().unwrap();
        assert!(d.contains("@@") || d.contains("diff --git") || d.is_empty());
        return;
    }

    let latest_sha = entries
        .first()
        .and_then(|e| e["sha"].as_str())
        .unwrap()
        .to_string();
    let diff = harness
        .call_tool(
            "memory_diff",
            json!({"project": project, "permalink": created["permalink"], "sha": latest_sha}),
        )
        .await
        .expect("memory_diff should dispatch");
    assert!(diff.get("error").is_none() || diff["error"].is_null());
    let d = diff["diff"].as_str().unwrap();
    assert!(d.contains("@@") || d.contains("diff --git") || !d.is_empty());
}

#[tokio::test]
async fn mcp_memory_reindex_returns_expected_contract_shape() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Reindex Seed", "content": "seed", "type": "reference"}),
        )
        .await
        .expect("memory_write should dispatch");

    let reindex = harness
        .call_tool("memory_reindex", json!({"project": project}))
        .await
        .expect("memory_reindex should dispatch");
    assert!(reindex.get("error").is_none() || reindex["error"].is_null());
    assert!(reindex.get("updated").and_then(|v| v.as_i64()).is_some());
    assert!(reindex.get("created").and_then(|v| v.as_i64()).is_some());
    assert!(reindex.get("deleted").and_then(|v| v.as_i64()).is_some());
    assert!(reindex.get("unchanged").and_then(|v| v.as_i64()).is_some());
}

#[tokio::test]
async fn mcp_memory_build_context_follows_wikilinks() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = &proj.path;

    let target = harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Context Target", "content": "target body", "type": "reference"}),
        )
        .await
        .expect("memory_write target should dispatch");
    let seed = harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Context Seed", "content": "see [[Context Target]]", "type": "reference"}),
        )
        .await
        .expect("memory_write seed should dispatch");

    let built = harness
        .call_tool(
            "memory_build_context",
            json!({"project": project, "url": seed["permalink"], "depth": 1, "max_related": 5}),
        )
        .await
        .expect("memory_build_context should dispatch");
    assert!(built.get("error").is_none() || built["error"].is_null());
    let primary = built["primary"].as_array().unwrap();
    let related_l1 = built["related_l1"].as_array().unwrap();
    let related_l0 = built["related_l0"].as_array().unwrap();
    assert_eq!(primary[0]["permalink"], seed["permalink"]);
    // Check both L1 and L0 tiered fields for the target note
    let in_l1 = related_l1
        .iter()
        .any(|n| n["permalink"] == target["permalink"]);
    let in_l0 = related_l0
        .iter()
        .any(|n| n["permalink"] == target["permalink"]);
    assert!(
        in_l1 || in_l0,
        "target permalink should be in related_l1 or related_l0"
    );
}

#[tokio::test]
async fn mcp_memory_task_refs_returns_tasks_for_permalink() {
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let (project_row, _dir) = common::create_test_project_with_dir(db).await;
    let epic = common::create_test_epic(db, &project_row.id).await;
    let project = project_row.path.clone();

    let note = harness
        .call_tool(
            "memory_write",
            json!({"project": project, "title": "Task Ref Note", "content": "task refs seed", "type": "reference"}),
        )
        .await
        .expect("memory_write should dispatch");

    let task = harness
        .call_tool(
            "task_create",
            json!({"project": project, "epic_id": epic.id, "title": "Task referencing memory note", "issue_type": "task", "priority": 2, "status": "open", "memory_refs": [note["permalink"]], "acceptance_criteria": ["note is attached to task"]}),
        )
        .await
        .expect("task_create should dispatch");
    assert!(task.get("error").is_none() || task["error"].is_null());

    let refs = harness
        .call_tool(
            "memory_task_refs",
            json!({"project": project, "permalink": note["permalink"]}),
        )
        .await
        .expect("memory_task_refs should dispatch");
    assert!(refs.get("error").is_none() || refs["error"].is_null());
    let tasks = refs["tasks"].as_array().unwrap();
    assert!(
        tasks
            .iter()
            .any(|t| t["id"] == task["id"] && t["title"] == "Task referencing memory note")
    );
}
