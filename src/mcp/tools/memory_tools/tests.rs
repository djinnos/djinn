use std::path::Path;

use serde_json::json;

use crate::db::NoteRepository;
use crate::db::repositories::project::ProjectRepository;
use crate::mcp::tools::memory_tools::types::*;
use crate::test_helpers::{
    create_test_app, create_test_app_with_db, create_test_db, create_test_epic,
    create_test_project, initialize_mcp_session, mcp_call_tool,
};
use crate::events::EventBus;

#[tokio::test]
async fn mcp_memory_write_success_shape_and_duplicate_permalink_error() {
    let db = create_test_db();
    let app = create_test_app_with_db(db.clone());
    let session_id = initialize_mcp_session(&app).await;

    let created = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({
            "project": "/tmp/mcp-memory-write",
            "title": "Write Contract Note",
            "content": "body",
            "type": "adr"
        }),
    )
    .await;

    assert!(created.get("id").and_then(|v| v.as_str()).is_some());
    assert_eq!(created["title"], "Write Contract Note");
    assert_eq!(created["note_type"], "adr");
    assert!(created.get("permalink").and_then(|v| v.as_str()).is_some());

    let project_repo = ProjectRepository::new(db.clone(), EventBus::noop());
    let project = project_repo
        .resolve_or_create("/tmp/mcp-memory-write")
        .await
        .unwrap();
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = note_repo
        .get_by_permalink(&project, created["permalink"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(Path::new(&note.file_path).exists());

    let duplicate = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({
            "project": "/tmp/mcp-memory-write",
            "title": "Write Contract Note",
            "content": "body-2",
            "type": "adr"
        }),
    )
    .await;

    assert!(duplicate.get("error").is_some());
}

#[tokio::test]
async fn mcp_memory_read_by_permalink_by_title_and_not_found_error() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;

    let created = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({
            "project": "/tmp/mcp-memory-read",
            "title": "Read Contract Note",
            "content": "read me",
            "type": "reference"
        }),
    )
    .await;

    let by_permalink = mcp_call_tool(
        &app,
        &session_id,
        "memory_read",
        json!({
            "project": "/tmp/mcp-memory-read",
            "identifier": created["permalink"]
        }),
    )
    .await;
    assert_eq!(by_permalink["title"], "Read Contract Note");

    let by_title = mcp_call_tool(
        &app,
        &session_id,
        "memory_read",
        json!({
            "project": "/tmp/mcp-memory-read",
            "identifier": "Read Contract Note"
        }),
    )
    .await;
    assert_eq!(by_title["permalink"], created["permalink"]);

    let missing = mcp_call_tool(
        &app,
        &session_id,
        "memory_read",
        json!({
            "project": "/tmp/mcp-memory-read",
            "identifier": "does-not-exist"
        }),
    )
    .await;
    assert!(missing.get("error").is_some());
}

#[tokio::test]
async fn mcp_memory_search_returns_ranked_results_with_snippets_and_filters() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-search";

    mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Rust Alpha", "content": "rust rust rust memory", "type": "reference"}),
    )
    .await;
    mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Rust Beta", "content": "rust memory", "type": "reference"}),
    )
    .await;
    mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "ADR Gamma", "content": "rust decision", "type": "adr"}),
    )
    .await;

    let searched = mcp_call_tool(
        &app,
        &session_id,
        "memory_search",
        json!({"project": project, "query": "rust", "limit": 10}),
    )
    .await;

    let results = searched["results"].as_array().unwrap();
    assert!(results.len() >= 2);
    assert!(results[0].get("snippet").is_some());

    let by_folder = mcp_call_tool(
        &app,
        &session_id,
        "memory_search",
        json!({"project": project, "query": "rust", "folder": "decisions"}),
    )
    .await;
    for r in by_folder["results"].as_array().unwrap() {
        assert_eq!(r["folder"], "decisions");
    }

    let by_type = mcp_call_tool(
        &app,
        &session_id,
        "memory_search",
        json!({"project": project, "query": "rust", "type": "adr"}),
    )
    .await;
    for r in by_type["results"].as_array().unwrap() {
        assert_eq!(r["note_type"], "adr");
    }
}

#[tokio::test]
async fn mcp_memory_edit_append_prepend_replace_and_missing_note_error() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-edit";

    mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Edit Note", "content": "middle", "type": "reference"}),
    )
    .await;

    let appended = mcp_call_tool(
        &app,
        &session_id,
        "memory_edit",
        json!({"project": project, "identifier": "Edit Note", "operation": "append", "content": "tail"}),
    )
    .await;
    assert!(appended["content"].as_str().unwrap().contains("tail"));

    let prepended = mcp_call_tool(
        &app,
        &session_id,
        "memory_edit",
        json!({"project": project, "identifier": "Edit Note", "operation": "prepend", "content": "head"}),
    )
    .await;
    assert!(prepended["content"].as_str().unwrap().starts_with("head"));

    let replaced = mcp_call_tool(
        &app,
        &session_id,
        "memory_edit",
        json!({
            "project": project,
            "identifier": "Edit Note",
            "operation": "find_replace",
            "find_text": "middle",
            "content": "center"
        }),
    )
    .await;
    assert!(replaced["content"].as_str().unwrap().contains("center"));

    let missing = mcp_call_tool(
        &app,
        &session_id,
        "memory_edit",
        json!({"project": project, "identifier": "Missing", "operation": "append", "content": "x"}),
    )
    .await;
    assert!(missing.get("error").is_some());
}

#[tokio::test]
async fn mcp_memory_move_changes_folder_title_and_permalink() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-move";

    let created = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Move Me", "content": "content", "type": "reference"}),
    )
    .await;

    let moved = mcp_call_tool(
        &app,
        &session_id,
        "memory_move",
        json!({
            "project": project,
            "identifier": created["permalink"],
            "title": "Moved Title",
            "type": "research"
        }),
    )
    .await;

    assert_eq!(moved["title"], "Moved Title");
    assert_eq!(moved["folder"], "research");
    assert_ne!(moved["permalink"], created["permalink"]);
}

#[tokio::test]
async fn mcp_memory_delete_success_and_missing_note_error() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-delete";

    mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Delete Me", "content": "bye", "type": "reference"}),
    )
    .await;

    let deleted = mcp_call_tool(
        &app,
        &session_id,
        "memory_delete",
        json!({"project": project, "identifier": "Delete Me"}),
    )
    .await;
    assert_eq!(deleted["ok"], true);

    let missing = mcp_call_tool(
        &app,
        &session_id,
        "memory_delete",
        json!({"project": project, "identifier": "Delete Me"}),
    )
    .await;
    assert_eq!(missing["ok"], false);
    assert!(missing.get("error").is_some());
}

#[tokio::test]
async fn mcp_memory_list_all_and_filters_by_folder_and_type() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-list";

    mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "A", "content": "x", "type": "adr"}),
    )
    .await;
    mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "B", "content": "x", "type": "reference"}),
    )
    .await;

    let all = mcp_call_tool(
        &app,
        &session_id,
        "memory_list",
        json!({"project": project}),
    )
    .await;
    assert!(all["notes"].as_array().unwrap().len() >= 2);

    let folder = mcp_call_tool(
        &app,
        &session_id,
        "memory_list",
        json!({"project": project, "folder": "decisions"}),
    )
    .await;
    for n in folder["notes"].as_array().unwrap() {
        assert_eq!(n["folder"], "decisions");
    }

    let typed = mcp_call_tool(
        &app,
        &session_id,
        "memory_list",
        json!({"project": project, "type": "reference"}),
    )
    .await;
    for n in typed["notes"].as_array().unwrap() {
        assert_eq!(n["note_type"], "reference");
    }
}

#[tokio::test]
async fn mcp_memory_graph_returns_wikilink_edges() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-graph";

    mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Node B", "content": "b", "type": "reference"}),
    )
    .await;
    mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Node A", "content": "links [[Node B]]", "type": "reference"})).await;

    let graph = mcp_call_tool(
        &app,
        &session_id,
        "memory_graph",
        json!({"project": project}),
    )
    .await;
    let edges = graph["edges"].as_array().unwrap();
    assert!(!edges.is_empty());
}

#[tokio::test]
async fn mcp_memory_recent_orders_by_last_accessed() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-recent";

    mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Older", "content": "o", "type": "reference"}),
    )
    .await;
    mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Newer", "content": "n", "type": "reference"}),
    )
    .await;

    mcp_call_tool(
        &app,
        &session_id,
        "memory_read",
        json!({"project": project, "identifier": "Older"}),
    )
    .await;
    mcp_call_tool(
        &app,
        &session_id,
        "memory_read",
        json!({"project": project, "identifier": "Newer"}),
    )
    .await;

    let recent = mcp_call_tool(
        &app,
        &session_id,
        "memory_recent",
        json!({"project": project, "timeframe": "7d", "limit": 2}),
    )
    .await;
    let notes = recent["notes"].as_array().unwrap();
    assert_eq!(notes[0]["title"], "Newer");
}

#[tokio::test]
async fn mcp_memory_catalog_returns_structured_catalog() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-catalog";

    mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Catalog Item", "content": "c", "type": "reference"}),
    )
    .await;
    let catalog = mcp_call_tool(
        &app,
        &session_id,
        "memory_catalog",
        json!({"project": project}),
    )
    .await;
    assert!(
        catalog["catalog"]
            .as_str()
            .unwrap()
            .contains("Catalog Item")
    );
}

#[tokio::test]
async fn mcp_memory_health_orphans_and_broken_links_shapes() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-health";

    mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Source", "content": "[[Missing Target]]", "type": "reference"})).await;

    let health = mcp_call_tool(
        &app,
        &session_id,
        "memory_health",
        json!({"project": project}),
    )
    .await;
    assert!(health.get("orphan_note_count").is_some());
    assert!(health.get("broken_link_count").is_some());

    let orphans = mcp_call_tool(
        &app,
        &session_id,
        "memory_orphans",
        json!({"project": project}),
    )
    .await;
    assert!(orphans["orphans"].is_array());

    let broken = mcp_call_tool(
        &app,
        &session_id,
        "memory_broken_links",
        json!({"project": project}),
    )
    .await;
    assert!(broken["broken_links"].is_array());
}

// ── Param deserialization unit tests ─────────────────────────────────────────

#[test]
fn write_params_deserialize() {
    let params: WriteParams =
        serde_json::from_value(json!({"project":"/tmp/p","title":"T","content":"C","type":"adr"}))
            .unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.title, "T");
    assert_eq!(params.content, "C");
    assert_eq!(params.note_type, "adr");
    assert!(params.tags.is_none());
}

#[test]
fn read_params_deserialize() {
    let params: ReadParams =
        serde_json::from_value(json!({"project":"/tmp/p","identifier":"abc"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.identifier, "abc");
}

#[test]
fn search_params_deserialize() {
    let params: SearchParams =
        serde_json::from_value(json!({"project":"/tmp/p","query":"rust"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.query, "rust");
    assert!(params.limit.is_none());
    assert!(params.folder.is_none());
    assert!(params.note_type.is_none());
}

#[test]
fn edit_params_deserialize() {
    let params: EditParams = serde_json::from_value(json!({
        "project":"/tmp/p",
        "identifier":"a",
        "operation":"append",
        "content":"x"
    }))
    .unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.identifier, "a");
    assert_eq!(params.operation, "append");
    assert_eq!(params.content, "x");
}

#[test]
fn move_params_deserialize() {
    let params: MoveParams = serde_json::from_value(json!({
        "project":"/tmp/p",
        "identifier":"a",
        "type":"adr",
        "title":"new"
    }))
    .unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.identifier, "a");
    assert_eq!(params.title.as_deref(), Some("new"));
    assert_eq!(params.note_type, "adr");
}

#[test]
fn delete_params_deserialize() {
    let params: DeleteParams =
        serde_json::from_value(json!({"project":"/tmp/p","identifier":"a"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.identifier, "a");
}

#[test]
fn list_params_deserialize() {
    let params: ListParams = serde_json::from_value(json!({
        "project":"/tmp/p",
        "folder":"decisions",
        "type":"adr",
        "depth":2
    }))
    .unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.folder.as_deref(), Some("decisions"));
    assert_eq!(params.note_type.as_deref(), Some("adr"));
    assert_eq!(params.depth, Some(2));
}

#[test]
fn list_params_deserialize_minimal() {
    let params: ListParams = serde_json::from_value(json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert!(params.folder.is_none());
    assert!(params.note_type.is_none());
    assert!(params.depth.is_none());
}

#[test]
fn graph_params_deserialize() {
    let params: GraphParams = serde_json::from_value(json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
}

#[test]
fn recent_params_deserialize() {
    let params: RecentParams = serde_json::from_value(json!({
        "project":"/tmp/p",
        "timeframe":"7d",
        "limit":5
    }))
    .unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.timeframe.as_deref(), Some("7d"));
    assert_eq!(params.limit, Some(5));
}

#[test]
fn catalog_params_deserialize() {
    let params: CatalogParams = serde_json::from_value(json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
}

#[test]
fn health_params_deserialize() {
    let params: HealthParams = serde_json::from_value(json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(params.project.as_deref(), Some("/tmp/p"));
}

#[test]
fn orphans_params_deserialize() {
    let params: OrphansParams = serde_json::from_value(json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
}

#[test]
fn broken_links_params_deserialize() {
    let params: BrokenLinksParams = serde_json::from_value(json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
}

#[tokio::test]
async fn mcp_memory_history_and_diff_round_trip() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-history-diff";

    let created = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "History Diff", "content": "line one", "type": "reference"}),
    )
    .await;
    let permalink = created["permalink"].as_str().unwrap().to_string();

    let edited = mcp_call_tool(
        &app,
        &session_id,
        "memory_edit",
        json!({"project": project, "identifier": permalink, "operation": "append", "content": "line two"}),
    )
    .await;
    assert!(edited.get("error").is_none() || edited["error"].is_null());

    let history = mcp_call_tool(
        &app,
        &session_id,
        "memory_history",
        json!({"project": project, "permalink": created["permalink"], "limit": 10}),
    )
    .await;

    assert!(history.get("error").is_none() || history["error"].is_null());
    let entries = history["history"]
        .as_array()
        .or_else(|| history["entries"].as_array())
        .expect("memory_history should return history/entries array");

    if entries.is_empty() {
        let diff = mcp_call_tool(
            &app,
            &session_id,
            "memory_diff",
            json!({
                "project": project,
                "permalink": created["permalink"]
            }),
        )
        .await;

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

    let diff = mcp_call_tool(
        &app,
        &session_id,
        "memory_diff",
        json!({
            "project": project,
            "permalink": created["permalink"],
            "sha": latest_sha
        }),
    )
    .await;

    assert!(diff.get("error").is_none() || diff["error"].is_null());
    let d = diff["diff"].as_str().unwrap();
    assert!(d.contains("@@") || d.contains("diff --git") || !d.is_empty());
}

#[tokio::test]
async fn mcp_memory_reindex_returns_expected_contract_shape() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-reindex";

    let _ = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Reindex Seed", "content": "seed", "type": "reference"}),
    )
    .await;

    let reindex = mcp_call_tool(
        &app,
        &session_id,
        "memory_reindex",
        json!({"project": project}),
    )
    .await;

    assert!(reindex.get("error").is_none() || reindex["error"].is_null());
    assert!(reindex.get("updated").and_then(|v| v.as_i64()).is_some());
    assert!(reindex.get("created").and_then(|v| v.as_i64()).is_some());
    assert!(reindex.get("deleted").and_then(|v| v.as_i64()).is_some());
    assert!(reindex.get("unchanged").and_then(|v| v.as_i64()).is_some());
}

#[tokio::test]
async fn mcp_memory_build_context_follows_wikilinks() {
    let app = create_test_app();
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-build-context";

    let target = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Context Target", "content": "target body", "type": "reference"}),
    )
    .await;
    let seed = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Context Seed", "content": "see [[Context Target]]", "type": "reference"}),
    )
    .await;

    let built = mcp_call_tool(
        &app,
        &session_id,
        "memory_build_context",
        json!({"project": project, "url": seed["permalink"], "depth": 1, "max_related": 5}),
    )
    .await;

    assert!(built.get("error").is_none() || built["error"].is_null());
    let primary = built["primary"].as_array().unwrap();
    let related = built["related"].as_array().unwrap();
    assert_eq!(primary[0]["permalink"], seed["permalink"]);
    assert!(
        related
            .iter()
            .any(|n| n["permalink"] == target["permalink"])
    );
}

#[tokio::test]
async fn mcp_memory_task_refs_returns_tasks_for_permalink() {
    let db = create_test_db();
    let project_row = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project_row.id).await;
    let app = create_test_app_with_db(db);
    let session_id = initialize_mcp_session(&app).await;
    let project = project_row.path.clone();

    let note = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({"project": project, "title": "Task Ref Note", "content": "task refs seed", "type": "reference"}),
    )
    .await;

    let task = mcp_call_tool(
        &app,
        &session_id,
        "task_create",
        json!({
            "project": project,
            "epic_id": epic.id,
            "title": "Task referencing memory note",
            "issue_type": "task",
            "priority": 2,
            "status": "open",
            "memory_refs": [note["permalink"]]
        }),
    )
    .await;

    assert!(task.get("error").is_none() || task["error"].is_null());

    let refs = mcp_call_tool(
        &app,
        &session_id,
        "memory_task_refs",
        json!({"project": project, "permalink": note["permalink"]}),
    )
    .await;

    assert!(refs.get("error").is_none() || refs["error"].is_null());
    let tasks = refs["tasks"].as_array().unwrap();
    assert!(
        tasks
            .iter()
            .any(|t| { t["id"] == task["id"] && t["title"] == "Task referencing memory note" })
    );
}

#[test]
fn history_params_deserialize() {
    let params: HistoryParams =
        serde_json::from_value(json!({"project":"/tmp/p","permalink":"decisions/a","limit":10}))
            .unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.permalink, "decisions/a");
    assert_eq!(params.limit, Some(10));
}

#[test]
fn diff_params_deserialize() {
    let params: DiffParams =
        serde_json::from_value(json!({"project":"/tmp/p","permalink":"decisions/a","sha":"abc"}))
            .unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.permalink, "decisions/a");
    assert_eq!(params.sha.as_deref(), Some("abc"));
}

#[test]
fn build_context_params_deserialize() {
    let params: BuildContextParams = serde_json::from_value(
        json!({"project":"/tmp/p","url":"memory://references/note","depth":2,"max_related":3}),
    )
    .unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.url, "memory://references/note");
    assert_eq!(params.depth, Some(2));
    assert_eq!(params.max_related, Some(3));
}

#[test]
fn reindex_params_deserialize() {
    let params: ReindexParams = serde_json::from_value(json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
}

#[test]
fn task_refs_params_deserialize() {
    let params: TaskRefsParams =
        serde_json::from_value(json!({"project":"/tmp/p","permalink":"references/n"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.permalink, "references/n");
}
