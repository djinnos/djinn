//! Pure-serde deserialization contract tests for memory-tool params.
//!
//! Asserts the JSON shape of each `*Params` struct (field names, optional
//! fields, renames such as `type` -> `note_type`) without touching AppState
//! or HTTP. Moved out of `djinn-server`'s `mcp_contract_tests` because these
//! never needed the server's test harness — only `djinn_control_plane`'s public types.

use djinn_control_plane::tools::memory_tools::{
    BrokenLinksParams, BuildContextParams, CatalogParams, DeleteParams, DiffParams, EditParams,
    GraphParams, HealthParams, HistoryParams, ListParams, MoveParams, OrphansParams, ReadParams,
    RecentParams, ReindexParams, SearchParams, TaskRefsParams, WriteParams,
};

// ── Param deserialization ─────────────────────────────────────────────────

#[test]
fn write_params_deserialize() {
    let p: WriteParams = serde_json::from_value(
        serde_json::json!({"project":"/tmp/p","title":"T","content":"C","type":"adr"}),
    )
    .unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert_eq!(p.title, "T");
    assert_eq!(p.content, "C");
    assert_eq!(p.note_type, "adr");
    assert!(p.tags.is_none());
}

#[test]
fn write_and_move_params_accept_mergeable_case_and_pitfall_types() {
    let write: WriteParams = serde_json::from_value(
        serde_json::json!({"project":"/tmp/p","title":"T","content":"C","type":"case"}),
    )
    .unwrap();
    assert_eq!(write.note_type, "case");

    let moved: MoveParams = serde_json::from_value(
        serde_json::json!({"project":"/tmp/p","identifier":"a","type":"pitfall"}),
    )
    .unwrap();
    assert_eq!(moved.note_type, "pitfall");
}

#[test]
fn read_params_deserialize() {
    let p: ReadParams =
        serde_json::from_value(serde_json::json!({"project":"/tmp/p","identifier":"abc"}))
            .unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert_eq!(p.identifier, "abc");
}

#[test]
fn search_params_deserialize() {
    let p: SearchParams =
        serde_json::from_value(serde_json::json!({"project":"/tmp/p","query":"rust"})).unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert_eq!(p.query, "rust");
    assert!(p.limit.is_none());
    assert!(p.folder.is_none());
    assert!(p.note_type.is_none());
}

#[test]
fn edit_params_deserialize() {
    let p: EditParams = serde_json::from_value(serde_json::json!({"project":"/tmp/p","identifier":"a","operation":"append","content":"x"})).unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert_eq!(p.identifier, "a");
    assert_eq!(p.operation, "append");
    assert_eq!(p.content, "x");
}

#[test]
fn move_params_deserialize() {
    let p: MoveParams = serde_json::from_value(
        serde_json::json!({"project":"/tmp/p","identifier":"a","type":"adr","title":"new"}),
    )
    .unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert_eq!(p.identifier, "a");
    assert_eq!(p.title.as_deref(), Some("new"));
    assert_eq!(p.note_type, "adr");
}

#[test]
fn delete_params_deserialize() {
    let p: DeleteParams =
        serde_json::from_value(serde_json::json!({"project":"/tmp/p","identifier":"a"}))
            .unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert_eq!(p.identifier, "a");
}

#[test]
fn list_params_deserialize() {
    let p: ListParams = serde_json::from_value(
        serde_json::json!({"project":"/tmp/p","folder":"decisions","type":"adr","depth":2}),
    )
    .unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert_eq!(p.folder.as_deref(), Some("decisions"));
    assert_eq!(p.note_type.as_deref(), Some("adr"));
    assert_eq!(p.depth, Some(2));
}

#[test]
fn list_params_deserialize_minimal() {
    let p: ListParams =
        serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert!(p.folder.is_none());
    assert!(p.note_type.is_none());
    assert!(p.depth.is_none());
}

#[test]
fn graph_params_deserialize() {
    let p: GraphParams =
        serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(p.project, "/tmp/p");
}

#[test]
fn recent_params_deserialize() {
    let p: RecentParams = serde_json::from_value(
        serde_json::json!({"project":"/tmp/p","timeframe":"7d","limit":5}),
    )
    .unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert_eq!(p.timeframe.as_deref(), Some("7d"));
    assert_eq!(p.limit, Some(5));
}

#[test]
fn catalog_params_deserialize() {
    let p: CatalogParams =
        serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(p.project, "/tmp/p");
}

#[test]
fn health_params_deserialize() {
    let p: HealthParams =
        serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(p.project.as_deref(), Some("/tmp/p"));
}

#[test]
fn orphans_params_deserialize() {
    let p: OrphansParams =
        serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(p.project, "/tmp/p");
}

#[test]
fn broken_links_params_deserialize() {
    let p: BrokenLinksParams =
        serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(p.project, "/tmp/p");
}

#[test]
fn history_params_deserialize() {
    let p: HistoryParams = serde_json::from_value(
        serde_json::json!({"project":"/tmp/p","permalink":"decisions/a","limit":10}),
    )
    .unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert_eq!(p.permalink, "decisions/a");
    assert_eq!(p.limit, Some(10));
}

#[test]
fn diff_params_deserialize() {
    let p: DiffParams = serde_json::from_value(
        serde_json::json!({"project":"/tmp/p","permalink":"decisions/a","sha":"abc"}),
    )
    .unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert_eq!(p.permalink, "decisions/a");
    assert_eq!(p.sha.as_deref(), Some("abc"));
}

#[test]
fn build_context_params_deserialize() {
    let p: BuildContextParams = serde_json::from_value(serde_json::json!({"project":"/tmp/p","url":"memory://references/note","depth":2,"max_related":3})).unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert_eq!(p.url, "memory://references/note");
    assert_eq!(p.depth, Some(2));
    assert_eq!(p.max_related, Some(3));
}

#[test]
fn reindex_params_deserialize() {
    let p: ReindexParams =
        serde_json::from_value(serde_json::json!({"project":"/tmp/p"})).unwrap();
    assert_eq!(p.project, "/tmp/p");
}

#[test]
fn task_refs_params_deserialize() {
    let p: TaskRefsParams = serde_json::from_value(
        serde_json::json!({"project":"/tmp/p","permalink":"references/n"}),
    )
    .unwrap();
    assert_eq!(p.project, "/tmp/p");
    assert_eq!(p.permalink, "references/n");
}
