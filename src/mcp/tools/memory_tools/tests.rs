use serde_json::json;

use crate::mcp::tools::memory_tools::*;
use crate::test_helpers::{create_test_app, initialize_mcp_session, mcp_call_tool};

#[tokio::test]
async fn mcp_memory_write_success_shape_and_duplicate_permalink_error() {
    let app = create_test_app().await;
    let session_id = initialize_mcp_session(&app).await;

    let created = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({
            "project": "/tmp/mcp-memory-write",
            "title": "Write Contract Note",
            "content": "body",
            "folder": "decisions",
            "type": "adr"
        }),
    )
    .await;

    assert!(created.get("id").and_then(|v| v.as_str()).is_some());
    assert_eq!(created["title"], "Write Contract Note");
    assert_eq!(created["folder"], "decisions");
    assert_eq!(created["note_type"], "adr");
    assert!(created.get("permalink").and_then(|v| v.as_str()).is_some());

    let duplicate = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({
            "project": "/tmp/mcp-memory-write",
            "title": "Write Contract Note",
            "content": "body-2",
            "folder": "decisions",
            "type": "adr",
            "permalink": created["permalink"]
        }),
    )
    .await;

    assert!(duplicate.get("error").is_some());
}

#[tokio::test]
async fn mcp_memory_read_by_permalink_by_title_and_not_found_error() {
    let app = create_test_app().await;
    let session_id = initialize_mcp_session(&app).await;

    let created = mcp_call_tool(
        &app,
        &session_id,
        "memory_write",
        json!({
            "project": "/tmp/mcp-memory-read",
            "title": "Read Contract Note",
            "content": "read me",
            "folder": "reference",
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
    let app = create_test_app().await;
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
    let app = create_test_app().await;
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
    let app = create_test_app().await;
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
            "folder": "research"
        }),
    )
    .await;

    assert_eq!(moved["title"], "Moved Title");
    assert_eq!(moved["folder"], "research");
    assert_ne!(moved["permalink"], created["permalink"]);
}

#[tokio::test]
async fn mcp_memory_delete_success_and_missing_note_error() {
    let app = create_test_app().await;
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
    let app = create_test_app().await;
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-list";

    mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "A", "content": "x", "type": "adr"})).await;
    mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "B", "content": "x", "type": "reference"})).await;

    let all = mcp_call_tool(&app, &session_id, "memory_list", json!({"project": project})).await;
    assert!(all["notes"].as_array().unwrap().len() >= 2);

    let folder = mcp_call_tool(&app, &session_id, "memory_list", json!({"project": project, "folder": "decisions"})).await;
    for n in folder["notes"].as_array().unwrap() {
        assert_eq!(n["folder"], "decisions");
    }

    let typed = mcp_call_tool(&app, &session_id, "memory_list", json!({"project": project, "type": "reference"})).await;
    for n in typed["notes"].as_array().unwrap() {
        assert_eq!(n["note_type"], "reference");
    }
}

#[tokio::test]
async fn mcp_memory_graph_returns_wikilink_edges() {
    let app = create_test_app().await;
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-graph";

    mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Node B", "content": "b", "type": "reference"})).await;
    mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Node A", "content": "links [[Node B]]", "type": "reference"})).await;

    let graph = mcp_call_tool(&app, &session_id, "memory_graph", json!({"project": project})).await;
    let edges = graph["edges"].as_array().unwrap();
    assert!(!edges.is_empty());
}

#[tokio::test]
async fn mcp_memory_recent_orders_by_last_accessed() {
    let app = create_test_app().await;
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-recent";

    mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Older", "content": "o", "type": "reference"})).await;
    mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Newer", "content": "n", "type": "reference"})).await;

    mcp_call_tool(&app, &session_id, "memory_read", json!({"project": project, "identifier": "Older"})).await;
    mcp_call_tool(&app, &session_id, "memory_read", json!({"project": project, "identifier": "Newer"})).await;

    let recent = mcp_call_tool(&app, &session_id, "memory_recent", json!({"project": project, "timeframe": "7d", "limit": 2})).await;
    let notes = recent["notes"].as_array().unwrap();
    assert_eq!(notes[0]["title"], "Newer");
}

#[tokio::test]
async fn mcp_memory_catalog_returns_structured_catalog() {
    let app = create_test_app().await;
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-catalog";

    mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Catalog Item", "content": "c", "type": "reference"})).await;
    let catalog = mcp_call_tool(&app, &session_id, "memory_catalog", json!({"project": project})).await;
    assert!(catalog["catalog"].as_str().unwrap().contains("Catalog Item"));
}

#[tokio::test]
async fn mcp_memory_health_orphans_and_broken_links_shapes() {
    let app = create_test_app().await;
    let session_id = initialize_mcp_session(&app).await;
    let project = "/tmp/mcp-memory-health";

    mcp_call_tool(&app, &session_id, "memory_write", json!({"project": project, "title": "Source", "content": "[[Missing Target]]", "type": "reference"})).await;

    let health = mcp_call_tool(&app, &session_id, "memory_health", json!({"project": project})).await;
    assert!(health.get("orphan_note_count").is_some());
    assert!(health.get("broken_link_count").is_some());

    let orphans = mcp_call_tool(&app, &session_id, "memory_orphans", json!({"project": project})).await;
    assert!(orphans["orphans"].is_array());

    let broken = mcp_call_tool(&app, &session_id, "memory_broken_links", json!({"project": project})).await;
    assert!(broken["broken_links"].is_array());
}

#[test]
fn memory_write_params_deserialize() {
    let params: MemoryWriteParams =
        serde_json::from_value(json!({"project":"/tmp/p","title":"T","content":"C"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.title, "T");
    assert_eq!(params.content, "C");
    assert!(params.permalink.is_none());
    assert!(params.folder.is_none());
    assert!(params.note_type.is_none());
}

#[test]
fn memory_read_params_deserialize() {
    let params: MemoryReadParams =
        serde_json::from_value(json!({"project":"/tmp/p","identifier":"abc"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.identifier, "abc");
}

#[test]
fn memory_search_params_deserialize() {
    let params: MemorySearchParams =
        serde_json::from_value(json!({"project":"/tmp/p","query":"rust"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.query, "rust");
    assert!(params.limit.is_none());
    assert!(params.folder.is_none());
    assert!(params.note_type.is_none());
}

#[test]
fn memory_edit_params_deserialize() {
    let params: MemoryEditParams = serde_json::from_value(json!({
        "project":"/tmp/p",
        "identifier":"a",
        "operation":"append",
        "content":"x"
    }))
    .unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.identifier, "a");
    assert_eq!(params.operation, "append");
    assert_eq!(params.content.as_deref(), Some("x"));
}

#[test]
fn memory_move_params_deserialize() {
    let params: MemoryMoveParams = serde_json::from_value(json!({
        "project":"/tmp/p",
        "identifier":"a",
        "title":"new"
    }))
    .unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.identifier, "a");
    assert_eq!(params.title.as_deref(), Some("new"));
    assert!(params.folder.is_none());
    assert!(params.note_type.is_none());
}

#[test]
fn memory_delete_params_deserialize() {
    let params: MemoryDeleteParams =
        serde_json::from_value(json!({"project":"/tmp/p","identifier":"a"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.identifier, "a");
}

#[test]
fn memory_list_params_deserialize() {
    let params: MemoryListParams = serde_json::from_value(json!({
        "project":"/tmp/p",
        "folder":"decisions",
        "type":"adr",
        "limit":10
    }))
    .unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.folder.as_deref(), Some("decisions"));
    assert_eq!(params.note_type.as_deref(), Some("adr"));
    assert_eq!(params.limit, Some(10));
}

#[test]
fn memory_graph_params_deserialize() {
    let params: MemoryGraphParams = serde_json::from_value(json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
}

#[test]
fn memory_recent_params_deserialize() {
    let params: MemoryRecentParams = serde_json::from_value(json!({
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
fn memory_catalog_params_deserialize() {
    let params: MemoryCatalogParams =
        serde_json::from_value(json!({"project":"/tmp/p","folder":"research"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
    assert_eq!(params.folder.as_deref(), Some("research"));
}

#[test]
fn memory_health_params_deserialize() {
    let params: MemoryHealthParams = serde_json::from_value(json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
}

#[test]
fn memory_orphans_params_deserialize() {
    let params: MemoryOrphansParams = serde_json::from_value(json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
}

#[test]
fn memory_broken_links_params_deserialize() {
    let params: MemoryBrokenLinksParams =
        serde_json::from_value(json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(params.project, "/tmp/p");
}
