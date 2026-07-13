//! Contract tests for `memory_*` MCP tools (worktree-free).
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

use djinn_control_plane::test_support::McpTestHarness;
use djinn_core::events::EventBus;
use djinn_db::{NoteRepository, ProjectRepository};
use serde_json::json;

#[tokio::test]
async fn mcp_memory_write_success_shape_and_duplicate_permalink_error() {
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();
    let (proj, _dir) = common::create_test_project_with_dir(&db).await;
    let project = proj.slug();

    let created = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation",
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
    let project_id: String = project_repo
        .resolve(&project)
        .await
        .unwrap()
        .expect("test project should resolve");
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .get_by_permalink(&project_id, created["permalink"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(note.storage, "db");
    assert_eq!(note.file_path, "");

    let duplicate = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation",
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
    let project = proj.slug();

    let created = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation",
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
    let project = proj.slug();

    let created = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation",
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
    let project = proj.slug();

    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Rust Alpha", "content": "rust rust rust memory", "type": "reference"}),
        )
        .await
        .expect("memory_write alpha should dispatch");
    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Rust Beta", "content": "rust memory", "type": "reference"}),
        )
        .await
        .expect("memory_write beta should dispatch");
    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "ADR Gamma", "content": "rust decision", "type": "adr"}),
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
    let project = proj.slug();

    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Edit Note", "content": "middle", "type": "reference"}),
        )
        .await
        .expect("seed memory_write should dispatch");

    let appended = harness
        .call_tool(
            "memory_edit",
            json!({
                "reason": "test mutation","project": project, "identifier": "Edit Note", "operation": "append", "content": "tail"}),
        )
        .await
        .expect("memory_edit append should dispatch");
    assert!(appended["content"].as_str().unwrap().contains("tail"));

    let prepended = harness
        .call_tool(
            "memory_edit",
            json!({
                "reason": "test mutation","project": project, "identifier": "Edit Note", "operation": "prepend", "content": "head"}),
        )
        .await
        .expect("memory_edit prepend should dispatch");
    assert!(prepended["content"].as_str().unwrap().starts_with("head"));

    let replaced = harness
        .call_tool(
            "memory_edit",
            json!({
                "reason": "test mutation","project": project, "identifier": "Edit Note", "operation": "find_replace", "find_text": "middle", "content": "center"}),
        )
        .await
        .expect("memory_edit find_replace should dispatch");
    assert!(replaced["content"].as_str().unwrap().contains("center"));

    let missing = harness
        .call_tool(
            "memory_edit",
            json!({
                "reason": "test mutation","project": project, "identifier": "Missing", "operation": "append", "content": "x"}),
        )
        .await
        .expect("memory_edit missing should dispatch");
    assert!(missing.get("error").is_some());
}

#[tokio::test]
async fn mcp_memory_move_changes_folder_title_and_permalink() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = proj.slug();

    let created = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Move Me", "content": "content", "type": "reference"}),
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
async fn mcp_memory_delete_success_and_missing_note_error() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = proj.slug();

    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Delete Me", "content": "bye", "type": "reference"}),
        )
        .await
        .expect("seed memory_write should dispatch");

    let deleted = harness
        .call_tool(
            "memory_delete",
            json!({
                "reason": "test mutation","project": project, "identifier": "Delete Me"}),
        )
        .await
        .expect("memory_delete should dispatch");
    assert_eq!(deleted["ok"], true);

    let missing = harness
        .call_tool(
            "memory_delete",
            json!({
                "reason": "test mutation","project": project, "identifier": "Delete Me"}),
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
    let project = proj.slug();

    let adr = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "A", "content": "x", "type": "adr"}),
        )
        .await
        .expect("memory_write adr should dispatch");
    assert_eq!(adr["deduplicated"], false);
    let reference = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "B", "content": "different content", "type": "reference"}),
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
    let project = proj.slug();

    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Node B", "content": "b", "type": "reference"}),
        )
        .await
        .expect("seed node B should dispatch");
    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Node A", "content": "links [[Node B]] [[Node C]]", "type": "reference"}),
        )
        .await
        .expect("seed node A should dispatch");
    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Node C", "content": "links [[Node B]] [[NonExistent]]", "type": "reference"}),
        )
        .await
        .expect("seed node C should dispatch");
    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Node D", "content": "isolated", "type": "reference"}),
        )
        .await
        .expect("seed node D should dispatch");

    let graph = harness
        .call_tool("memory_graph", json!({"project": project}))
        .await
        .expect("memory_graph should dispatch");
    assert!(!graph["edges"].as_array().unwrap().is_empty());

    let nodes = graph["nodes"].as_array().unwrap();
    let node_c = nodes
        .iter()
        .find(|node| node["title"] == "Node C")
        .expect("Node C should be present in graph");
    assert_eq!(node_c["is_orphan"], false);
    assert!(
        node_c["broken_targets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|target| target == "NonExistent")
    );

    let node_d = nodes
        .iter()
        .find(|node| node["title"] == "Node D")
        .expect("Node D should be present in graph");
    assert_eq!(node_d["is_orphan"], true);
}

#[tokio::test]
async fn mcp_memory_recent_orders_by_last_accessed() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = proj.slug();

    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Older", "content": "o", "type": "reference"}),
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
            json!({
                "reason": "test mutation","project": project, "title": "Newer", "content": "n", "type": "reference"}),
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
    let project = proj.slug();

    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Catalog Item", "content": "c", "type": "reference"}),
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
    // Use a slug-shaped reference that isn't seeded; the tool still resolves
    // and errors silently, and we only assert the response shape below.
    let project = "test/mcp-memory-health";

    // No project seeded: memory_write resolves and errors silently; the test
    // only asserts the shape of the three health / orphans / broken_links
    // responses, so that's fine.
    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Source", "content": "[[Missing Target]]", "type": "reference"}),
        )
        .await
        .expect("memory_write should dispatch");

    let health = harness
        .call_tool("memory_health", json!({"project": project}))
        .await
        .expect("memory_health should dispatch");
    assert!(health.get("orphan_note_count").is_some());
    assert!(health.get("broken_link_count").is_some());
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
    let project = proj.slug();

    let created = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "History Diff", "content": "line one", "type": "reference"}),
        )
        .await
        .expect("memory_write should dispatch");
    let permalink = created["permalink"].as_str().unwrap().to_string();

    let edited = harness
        .call_tool(
            "memory_edit",
            json!({
                "reason": "test mutation","project": project, "identifier": permalink, "operation": "append", "content": "line two"}),
        )
        .await
        .expect("memory_edit should dispatch");
    assert!(edited.get("error").is_none() || edited["error"].is_null());

    // memory_history and memory_diff: with the db-only KB cut-over both
    // tools return an empty payload and an explanatory error string for
    // db-stored notes (the only kind that exists now). Just confirm they
    // dispatch and shape-check; the git-backed history/diff content path
    // is gone.
    let history = harness
        .call_tool(
            "memory_history",
            json!({"project": project, "permalink": created["permalink"], "limit": 10}),
        )
        .await
        .expect("memory_history should dispatch");
    assert!(history["history"].is_array());

    let diff = harness
        .call_tool(
            "memory_diff",
            json!({"project": project, "permalink": created["permalink"]}),
        )
        .await
        .expect("memory_diff should dispatch");
    assert!(diff.get("diff").is_some());
}

// memory_reindex tool was deleted alongside the on-disk reindex pipeline
// (notes are db-only now). Removing the contract shape test.

#[tokio::test]
async fn mcp_memory_build_context_follows_wikilinks() {
    let harness = McpTestHarness::new().await;
    let (proj, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = proj.slug();

    let target = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Context Target", "content": "target body", "type": "reference"}),
        )
        .await
        .expect("memory_write target should dispatch");
    let seed = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Context Seed", "content": "see [[Context Target]]", "type": "reference"}),
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
    let project = project_row.slug();

    let note = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Task Ref Note", "content": "task refs seed", "type": "reference"}),
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

#[tokio::test]
async fn mcp_memory_associations_returns_kind_field() {
    // Wave-1 graph canvas: the `MemoryAssociationEntry.kind` column is projected
    // straight from `note_associations.kind` (migration 35, default `'co_access'`).
    // The MCP response must surface the kind so the UI can switch on edge
    // styling without a follow-up contract change. Future wave-1 graph-typed
    // edges (builds_on / contradicts / supersedes / exemplifies) will widen
    // the value set — see Epic 2chl.
    let harness = McpTestHarness::new().await;
    let db = harness.db().clone();
    let (proj, _dir) = common::create_test_project_with_dir(&db).await;
    let project = proj.slug();

    let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
    let project_id: String = project_repo
        .resolve(&project)
        .await
        .unwrap()
        .expect("test project should resolve");
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());

    // Seed two notes directly via the repo so we can capture their IDs without
    // paying the memory_write contract path.
    let note_a = note_repo
        .create(&project_id, "Source Note", "source body", "reference", "[]")
        .await
        .expect("seed note A should be created");
    let note_b = note_repo
        .create(&project_id, "Target Note", "target body", "reference", "[]")
        .await
        .expect("seed note B should be created");

    // Seed a co-access association. Migration 35 sets the kind default to
    // 'co_access', so the row will have the value we expect to see in the
    // MCP response.
    note_repo
        .upsert_association(&note_a.id, &note_b.id, 1)
        .await
        .expect("seed association should upsert");

    let response = harness
        .call_tool(
            "memory_associations",
            json!({
                "project": project,
                "identifier": note_a.permalink,
            }),
        )
        .await
        .expect("memory_associations should dispatch");

    assert!(response.get("error").is_none() || response["error"].is_null());
    let associations = response["associations"].as_array().unwrap();
    assert_eq!(associations.len(), 1, "expected one association row");

    let entry = &associations[0];
    assert_eq!(entry["note_permalink"], note_b.permalink);
    assert_eq!(
        entry["kind"], "co_access",
        "kind should be projected from note_associations.kind; got {:?}",
        entry["kind"]
    );
}

// ── Wave 1: proposal ↔ memory linkage integration tests ─────────────────────

/// Helper: returns `true` when every char in `s` is a hex digit (0-9 or a-f).
/// Such short_ids are filtered out by `extract_short_id_candidates` and would
/// make the resolved-mention test unreliable, so we retry until non-hex.
fn is_hex_only(s: &str) -> bool {
    s.chars()
        .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

/// Create a proposal whose 4-char short_id is guaranteed to contain at least
/// one non-hex character (so the short_id mention regex picks it up).
async fn create_proposal_with_non_hex_short_id(harness: &McpTestHarness) -> String {
    for _ in 0..32 {
        let created = harness
            .call_tool(
                "proposal_create",
                json!({"title": "Linkage test proposal", "body": "test body"}),
            )
            .await
            .expect("proposal_create should dispatch");
        assert!(
            created.get("error").is_none(),
            "proposal_create returned error: {created}"
        );
        let short_id = created
            .get("short_id")
            .and_then(|v| v.as_str())
            .expect("proposal short_id");
        if !is_hex_only(short_id) {
            return created
                .get("id")
                .and_then(|v| v.as_str())
                .expect("proposal id")
                .to_string();
        }
    }
    panic!("could not generate a non-hex short_id after 32 attempts");
}

/// Build a graduated-proposal fixture: 1 proposal with 2 graduated epics,
/// each epic carrying memory_refs, and tasks under those epics with their
/// own memory_refs. Returns the proposal id plus the three notes' permalinks.
struct GraduatedProposalFixture {
    project_slug: String,
    proposal_id: String,
    proposal_short_id: String,
    epic_note_permalink: String,
    task_note_permalink: String,
    shared_note_permalink: String,
}

async fn build_graduated_proposal_fixture(harness: &McpTestHarness) -> GraduatedProposalFixture {
    let (project_row, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = project_row.slug();

    // Three notes: one on an epic, one on a task, one shared across both.
    let epic_note = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Epic Ref Note", "content": "epic level note", "type": "adr"}),
        )
        .await
        .expect("memory_write epic_note should dispatch");
    let epic_note_permalink = epic_note["permalink"]
        .as_str()
        .expect("epic note permalink")
        .to_string();

    let task_note = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Task Ref Note", "content": "task level note", "type": "pitfall"}),
        )
        .await
        .expect("memory_write task_note should dispatch");
    let task_note_permalink = task_note["permalink"]
        .as_str()
        .expect("task note permalink")
        .to_string();

    let shared_note = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Shared Ref Note", "content": "shared across epic and task", "type": "reference"}),
        )
        .await
        .expect("memory_write shared_note should dispatch");
    let shared_note_permalink = shared_note["permalink"]
        .as_str()
        .expect("shared note permalink")
        .to_string();

    // Create a proposal.
    let proposal_id = create_proposal_with_non_hex_short_id(harness).await;

    // Get the proposal short_id for downstream assertion.
    let shown_proposal = harness
        .call_tool("proposal_show", json!({"id": &proposal_id}))
        .await
        .expect("proposal_show should dispatch");
    let proposal_short_id = shown_proposal["proposal"]["short_id"]
        .as_str()
        .expect("proposal short_id")
        .to_string();

    // Create two epics linked to the proposal via `proposal_id`.
    // Epic 1 carries epic_note + shared_note in its memory_refs.
    let epic_one = harness
        .call_tool(
            "epic_create",
            json!({
                "project": project,
                "title": "Graduated Epic One",
                "description": "first graduated epic",
                "memory_refs": [epic_note_permalink, shared_note_permalink],
                "proposal_id": &proposal_id,
            }),
        )
        .await
        .expect("epic_create one should dispatch");
    assert!(
        epic_one.get("error").is_none(),
        "epic_create returned error: {epic_one}"
    );
    let epic_one_id = epic_one["id"].as_str().expect("epic one id").to_string();

    // Epic 2 has no epic-level memory_refs (just a task-level one below).
    let epic_two = harness
        .call_tool(
            "epic_create",
            json!({
                "project": project,
                "title": "Graduated Epic Two",
                "description": "second graduated epic",
                "proposal_id": &proposal_id,
            }),
        )
        .await
        .expect("epic_create two should dispatch");
    assert!(
        epic_two.get("error").is_none(),
        "epic_create returned error: {epic_two}"
    );
    let epic_two_id = epic_two["id"].as_str().expect("epic two id").to_string();

    // Task under epic 1: carries task_note + shared_note (shared also on the epic → dedup check).
    let task_one = harness
        .call_tool(
            "task_create",
            json!({
                "project": project,
                "epic_id": &epic_one_id,
                "title": "Task with memory refs",
                "issue_type": "task",
                "priority": 2,
                "status": "open",
                "memory_refs": [task_note_permalink, shared_note_permalink],
                "acceptance_criteria": ["task has memory refs"],
            }),
        )
        .await
        .expect("task_create one should dispatch");
    assert!(
        task_one.get("error").is_none(),
        "task_create returned error: {task_one}"
    );

    // Task under epic 2: references the task_note independently.
    let _task_two = harness
        .call_tool(
            "task_create",
            json!({
                "project": project,
                "epic_id": &epic_two_id,
                "title": "Second task with memory refs",
                "issue_type": "task",
                "priority": 2,
                "status": "open",
                "memory_refs": [task_note_permalink],
                "acceptance_criteria": ["task has memory refs"],
            }),
        )
        .await
        .expect("task_create two should dispatch");

    GraduatedProposalFixture {
        project_slug: project,
        proposal_id,
        proposal_short_id,
        epic_note_permalink,
        task_note_permalink,
        shared_note_permalink,
    }
}

#[tokio::test]
async fn proposal_show_includes_memory_refs_from_graduated_epics() {
    let harness = McpTestHarness::new().await;
    let fixture = build_graduated_proposal_fixture(&harness).await;

    let shown = harness
        .call_tool("proposal_show", json!({"id": &fixture.proposal_id}))
        .await
        .expect("proposal_show should dispatch");
    assert!(
        shown.get("error").is_none(),
        "proposal_show returned error: {shown}"
    );

    let memory_refs = shown
        .get("memory_refs")
        .and_then(|v| v.as_array())
        .expect("memory_refs should be present and an array");
    assert!(
        !memory_refs.is_empty(),
        "memory_refs should be non-empty for a graduated proposal fixture"
    );

    // Collect permalinks for assertions.
    let permalinks: Vec<String> = memory_refs
        .iter()
        .filter_map(|r| {
            r.get("permalink")
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .collect();

    // All three expected notes should appear.
    assert!(
        permalinks.contains(&fixture.epic_note_permalink),
        "epic note should be in memory_refs: {permalinks:?}"
    );
    assert!(
        permalinks.contains(&fixture.task_note_permalink),
        "task note should be in memory_refs: {permalinks:?}"
    );
    assert!(
        permalinks.contains(&fixture.shared_note_permalink),
        "shared note should be in memory_refs: {permalinks:?}"
    );

    // No duplicate permalinks (dedup invariant).
    let unique: std::collections::HashSet<&str> = permalinks.iter().map(|s| s.as_str()).collect();
    assert_eq!(
        unique.len(),
        permalinks.len(),
        "no duplicate permalinks in memory_refs: {permalinks:?}"
    );

    // source_entity_type must be "epic" or "task".
    for r in memory_refs {
        let entity_type = r
            .get("source_entity_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(
            entity_type == "epic" || entity_type == "task",
            "source_entity_type should be 'epic' or 'task', got {entity_type}"
        );
        // Each ref should have all required fields populated.
        assert!(
            r.get("title")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "source title should be non-empty"
        );
        assert!(
            r.get("note_type")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "note_type should be non-empty"
        );
        assert!(
            r.get("source_short_id")
                .and_then(|v| v.as_str())
                .map(|s| !s.is_empty())
                .unwrap_or(false),
            "source_short_id should be non-empty"
        );
    }

    // Verify the epic-level note has source_entity_type "epic".
    let epic_note_ref = memory_refs
        .iter()
        .find(|r| r.get("permalink").and_then(|v| v.as_str()) == Some(&fixture.epic_note_permalink))
        .expect("epic note ref should exist");
    assert_eq!(
        epic_note_ref["source_entity_type"], "epic",
        "epic note should have source_entity_type 'epic'"
    );

    // Verify source_short_id is present and 4 chars.
    let source_short_id = epic_note_ref["source_short_id"]
        .as_str()
        .expect("source_short_id string");
    assert_eq!(
        source_short_id.len(),
        4,
        "source_short_id should be a 4-char base36 id"
    );
}

#[tokio::test]
async fn memory_task_refs_returns_proposals_through_epics() {
    let harness = McpTestHarness::new().await;
    let fixture = build_graduated_proposal_fixture(&harness).await;

    // Query memory_task_refs for the task-level note.
    // This note is attached to tasks under both graduated epics → the proposal
    // should be reachable.
    let refs = harness
        .call_tool(
            "memory_task_refs",
            json!({"project": &fixture.project_slug, "permalink": &fixture.task_note_permalink}),
        )
        .await
        .expect("memory_task_refs should dispatch");
    assert!(
        refs.get("error").is_none() || refs["error"].is_null(),
        "memory_task_refs returned error: {refs}"
    );

    // Tasks array should be non-empty.
    let tasks = refs
        .get("tasks")
        .and_then(|v| v.as_array())
        .expect("tasks should be an array");
    assert!(
        !tasks.is_empty(),
        "tasks should be non-empty for the task note: {tasks:?}"
    );

    // Proposals array should contain the graduated proposal.
    let proposals = refs
        .get("proposals")
        .and_then(|v| v.as_array())
        .expect("proposals should be an array");
    assert!(
        !proposals.is_empty(),
        "proposals should be non-empty: the note's task belongs to a graduated proposal"
    );

    let found_proposal = proposals
        .iter()
        .find(|p| p.get("id").and_then(|v| v.as_str()) == Some(&fixture.proposal_id))
        .unwrap_or_else(|| {
            panic!(
                "graduated proposal {} not found in proposals: {proposals:?}",
                fixture.proposal_id
            )
        });

    // Assert the proposal has the correct short_id, title, and status.
    assert_eq!(
        found_proposal["short_id"].as_str().unwrap(),
        fixture.proposal_short_id,
        "proposal short_id should match"
    );
    assert_eq!(
        found_proposal["title"].as_str().unwrap(),
        "Linkage test proposal",
        "proposal title should match"
    );
    assert!(
        found_proposal
            .get("status")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "proposal status should be non-empty"
    );
}

#[tokio::test]
async fn memory_read_surfaces_tasks_and_proposals_for_note_under_graduated_proposal() {
    let harness = McpTestHarness::new().await;
    let fixture = build_graduated_proposal_fixture(&harness).await;

    // Use the shared note permalink, which is attached to both an epic and a task under the graduated proposal.
    let permalink = &fixture.shared_note_permalink;

    // Call memory_read for this note.
    let resp = harness
        .call_tool(
            "memory_read",
            json!({"project": &fixture.project_slug, "identifier": permalink}),
        )
        .await
        .expect("memory_read should dispatch");
    assert!(
        resp.get("error").is_none() || resp["error"].is_null(),
        "memory_read returned error: {resp}"
    );

    // Assert tasks array is non-empty.
    let tasks = resp["tasks"].as_array().expect("tasks should be an array");
    assert!(
        !tasks.is_empty(),
        "memory_read should surface tasks referencing this note"
    );

    // Assert proposals array is non-empty and contains the graduated proposal.
    let proposals = resp["proposals"]
        .as_array()
        .expect("proposals should be an array");
    assert!(
        !proposals.is_empty(),
        "memory_read should surface proposals reachable through tasks/epics"
    );
    let found = proposals
        .iter()
        .any(|p| p["short_id"].as_str() == Some(&fixture.proposal_short_id));
    assert!(
        found,
        "graduated proposal {} should appear in memory_read proposals",
        fixture.proposal_short_id
    );
}

#[tokio::test]
async fn memory_read_regression_resolved_mentions_still_works() {
    let harness = McpTestHarness::new().await;
    let fixture = build_graduated_proposal_fixture(&harness).await;

    // The proposal's short_id is non-hex (guaranteed by the fixture helper).
    // Write a note whose body contains the proposal short_id as dead prose.
    // Use a fresh project so the note body mention resolution is clean.
    let (project_row, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = project_row.slug();
    let note = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation",
                "project": project,
                "title": "Note Mentioning Proposal",
                "content": format!("This pitfall relates to proposal {} which covers the design.", fixture.proposal_short_id),
                "type": "pitfall",
            }),
        )
        .await
        .expect("memory_write should dispatch");
    assert!(
        note.get("error").is_none() || note["error"].is_null(),
        "memory_write returned error: {note}"
    );

    // memory_read should resolve the short_id mention.
    let read = harness
        .call_tool(
            "memory_read",
            json!({"project": project, "identifier": note["permalink"]}),
        )
        .await
        .expect("memory_read should dispatch");
    assert!(
        read.get("error").is_none() || read["error"].is_null(),
        "memory_read returned error: {read}"
    );

    let mentions = read
        .get("resolved_mentions")
        .and_then(|v| v.as_array())
        .expect("resolved_mentions should be an array");
    assert!(
        !mentions.is_empty(),
        "resolved_mentions should be non-empty — the note body contains a valid short_id"
    );

    // Find the mention matching our proposal short_id.
    let proposal_mention = mentions
        .iter()
        .find(|m| m.get("short_id").and_then(|v| v.as_str()) == Some(&fixture.proposal_short_id))
        .unwrap_or_else(|| {
            panic!(
                "expected resolved mention for short_id {} in: {mentions:?}",
                fixture.proposal_short_id
            )
        });

    assert_eq!(
        proposal_mention["entity_type"].as_str().unwrap(),
        "proposal",
        "entity_type should be 'proposal'"
    );
    assert!(
        proposal_mention
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "title should be non-empty"
    );
    // permalink should be the proposal's UUID id.
    assert!(
        proposal_mention
            .get("permalink")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "permalink should be non-empty"
    );
}

#[tokio::test]
async fn memory_read_regression_memory_task_refs_behavior_unchanged() {
    let harness = McpTestHarness::new().await;
    let fixture = build_graduated_proposal_fixture(&harness).await;

    // Query memory_task_refs for the shared note using the same permalink as the new memory_read test.
    let refs = harness
        .call_tool(
            "memory_task_refs",
            json!({"project": &fixture.project_slug, "permalink": &fixture.shared_note_permalink}),
        )
        .await
        .expect("memory_task_refs should dispatch");
    assert!(
        refs.get("error").is_none() || refs["error"].is_null(),
        "memory_task_refs returned error: {refs}"
    );

    // Tasks array should be non-empty.
    let tasks = refs
        .get("tasks")
        .and_then(|v| v.as_array())
        .expect("tasks should be an array");
    assert!(
        !tasks.is_empty(),
        "memory_task_refs should still return tasks referencing the note"
    );

    // Proposals array should contain the graduated proposal.
    let proposals = refs
        .get("proposals")
        .and_then(|v| v.as_array())
        .expect("proposals should be an array");
    assert!(
        !proposals.is_empty(),
        "memory_task_refs should still return proposals reachable through tasks/epics"
    );
    let found = proposals
        .iter()
        .any(|p| p.get("id").and_then(|v| v.as_str()) == Some(&fixture.proposal_id));
    assert!(
        found,
        "graduated proposal {} should still appear in memory_task_refs proposals",
        fixture.proposal_id
    );
}

#[tokio::test]
async fn memory_read_resolves_short_id_mentions() {
    let harness = McpTestHarness::new().await;
    let fixture = build_graduated_proposal_fixture(&harness).await;

    // The proposal's short_id is non-hex (guaranteed by the fixture helper).
    // Write a note whose body contains the proposal short_id as dead prose.
    // Use a fresh project so the note body mention resolution is clean.
    let (project_row, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = project_row.slug();
    let note = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation",
                "project": project,
                "title": "Note Mentioning Proposal",
                "content": format!("This pitfall relates to proposal {} which covers the design.", fixture.proposal_short_id),
                "type": "pitfall",
            }),
        )
        .await
        .expect("memory_write should dispatch");
    assert!(
        note.get("error").is_none() || note["error"].is_null(),
        "memory_write returned error: {note}"
    );

    // memory_read should resolve the short_id mention.
    let read = harness
        .call_tool(
            "memory_read",
            json!({"project": project, "identifier": note["permalink"]}),
        )
        .await
        .expect("memory_read should dispatch");
    assert!(
        read.get("error").is_none() || read["error"].is_null(),
        "memory_read returned error: {read}"
    );

    let mentions = read
        .get("resolved_mentions")
        .and_then(|v| v.as_array())
        .expect("resolved_mentions should be an array");
    assert!(
        !mentions.is_empty(),
        "resolved_mentions should be non-empty — the note body contains a valid short_id"
    );

    // Find the mention matching our proposal short_id.
    let proposal_mention = mentions
        .iter()
        .find(|m| m.get("short_id").and_then(|v| v.as_str()) == Some(&fixture.proposal_short_id))
        .unwrap_or_else(|| {
            panic!(
                "expected resolved mention for short_id {} in: {mentions:?}",
                fixture.proposal_short_id
            )
        });

    assert_eq!(
        proposal_mention["entity_type"].as_str().unwrap(),
        "proposal",
        "entity_type should be 'proposal'"
    );
    assert!(
        proposal_mention
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "title should be non-empty"
    );
    // permalink should be the proposal's UUID id.
    assert!(
        proposal_mention
            .get("permalink")
            .and_then(|v| v.as_str())
            .map(|s| !s.is_empty())
            .unwrap_or(false),
        "permalink should be non-empty"
    );
}

#[tokio::test]
async fn no_regression_memory_refs_autolink() {
    // Regression guard: the existing task↔note memory_refs autolink behavior
    // (memory_task_refs returns tasks whose memory_refs contain the permalink)
    // must still work. This duplicates the core assertion of
    // `mcp_memory_task_refs_returns_tasks_for_permalink` but with a fresh
    // fixture, so a future change that breaks the autolink path fails here too.
    let harness = McpTestHarness::new().await;
    let db = harness.db();
    let (project_row, _dir) = common::create_test_project_with_dir(db).await;
    let epic = common::create_test_epic(db, &project_row.id).await;
    let project = project_row.slug();

    let note = harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Autolink Note", "content": "autolink seed", "type": "reference"}),
        )
        .await
        .expect("memory_write should dispatch");

    let task = harness
        .call_tool(
            "task_create",
            json!({
                "project": project,
                "epic_id": epic.id,
                "title": "Autolink Task",
                "issue_type": "task",
                "priority": 2,
                "status": "open",
                "memory_refs": [note["permalink"]],
                "acceptance_criteria": ["note attached"],
            }),
        )
        .await
        .expect("task_create should dispatch");
    assert!(task.get("error").is_none(), "task_create error: {task}");

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
            .any(|t| t["id"] == task["id"] && t["title"] == "Autolink Task"),
        "autolink should still find the task referencing the note: {tasks:?}"
    );
}

#[tokio::test]
async fn no_regression_memory_search_ranking_notes_only() {
    // Regression guard: memory_search returns notes ranked correctly.
    //
    // Wave 2 changed the default entity_types from notes-only to "both" —
    // proposals are now interleaved in search results when entity_types is
    // unset.  This test verifies that:
    // (a) the two notes are still found in the result set, and
    // (b) notes retain their expected fields (note_type, folder, permalink).
    //
    // Proposals MAY appear alongside notes in the default (unfiltered)
    // result set — that is by design.  The test does not assert their
    // absence.
    let harness = McpTestHarness::new().await;
    let (project_row, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = project_row.slug();

    // Create a proposal (may appear in default memory_search results).
    let proposal = harness
        .call_tool(
            "proposal_create",
            json!({"title": "Search Excluded Proposal", "body": "rust rust rust"}),
        )
        .await
        .expect("proposal_create should dispatch");
    let _proposal_id = proposal["id"].as_str().expect("proposal id").to_string();

    // Create notes that should be searchable.
    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Rust Note One", "content": "rust memory test", "type": "reference"}),
        )
        .await
        .expect("memory_write one should dispatch");
    harness
        .call_tool(
            "memory_write",
            json!({
                "reason": "test mutation","project": project, "title": "Rust Note Two", "content": "another rust note", "type": "adr"}),
        )
        .await
        .expect("memory_write two should dispatch");

    let searched = harness
        .call_tool(
            "memory_search",
            json!({"project": project, "query": "rust", "limit": 10}),
        )
        .await
        .expect("memory_search should dispatch");

    let results = searched["results"]
        .as_array()
        .expect("results should be an array");

    // At least the two notes should be present (proposals may also appear).
    let note_results: Vec<_> = results
        .iter()
        .filter(|r| {
            r.get("note_type")
                .and_then(|v| v.as_str())
                .is_some_and(|nt| nt != "proposal")
        })
        .collect();
    assert!(
        note_results.len() >= 2,
        "should find at least 2 notes (got {}): {results:?}",
        note_results.len()
    );

    // Every note result must carry the expected note fields.
    for r in &note_results {
        assert!(
            r.get("note_type").is_some(),
            "every note result should have note_type: {r:?}"
        );
        assert!(
            r.get("folder").is_some(),
            "every note result should have folder: {r:?}"
        );
    }
}

#[tokio::test]
async fn orphan_note_count_unchanged_by_proposal_memory_refs() {
    // Invariant: calling proposal_show must not create any new standalone note
    // files. The feature is read-only — it walks existing memory_refs but does
    // not write notes. The orphan-note count should be identical before and
    // after proposal_show.
    let harness = McpTestHarness::new().await;
    let fixture = build_graduated_proposal_fixture(&harness).await;
    let (project_row, _dir) = common::create_test_project_with_dir(harness.db()).await;
    let project = project_row.slug();

    // Record orphan count before.
    let health_before = harness
        .call_tool("memory_health", json!({"project": project}))
        .await
        .expect("memory_health before should dispatch");
    let orphans_before = health_before["orphan_note_count"]
        .as_i64()
        .expect("orphan_note_count should be an integer");

    // Call proposal_show — this walks memory_refs but must not create notes.
    let _ = harness
        .call_tool("proposal_show", json!({"id": &fixture.proposal_id}))
        .await
        .expect("proposal_show should dispatch");

    // Record orphan count after.
    let health_after = harness
        .call_tool("memory_health", json!({"project": project}))
        .await
        .expect("memory_health after should dispatch");
    let orphans_after = health_after["orphan_note_count"]
        .as_i64()
        .expect("orphan_note_count should be an integer");

    assert_eq!(
        orphans_before, orphans_after,
        "orphan_note_count must not change after proposal_show (read-only feature, no new notes created)"
    );
}
