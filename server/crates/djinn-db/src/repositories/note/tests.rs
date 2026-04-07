use std::path::Path;

use djinn_core::models::Note;
use tokio::sync::broadcast;

use crate::TaskRepository;
use crate::database::Database;
use crate::repositories::test_support::{
    build_multi_project_housekeeping_fixture, event_bus_for, make_project,
};

use super::*;

async fn make_epic(db: &Database, project_id: &str) -> String {
    let epic_id = uuid::Uuid::now_v7().to_string();
    let short_id = format!("ep-{}", epic_id);
    sqlx::query(
        "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(&epic_id)
    .bind(project_id)
    .bind(short_id)
    .bind("Epic")
    .bind("")
    .bind("")
    .bind("")
    .bind("")
    .bind("[]")
    .execute(db.pool())
    .await
    .unwrap();
    epic_id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_and_get_note() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create(
            &project.id,
            tmp.path(),
            "My ADR",
            "This is the content.",
            "adr",
            "[]",
        )
        .await
        .unwrap();

    assert_eq!(note.title, "My ADR");
    assert_eq!(note.note_type, "adr");
    assert_eq!(note.storage, "file");
    assert_eq!(note.folder, "decisions");
    assert_eq!(note.permalink, "decisions/my-adr");
    assert!(note.file_path.ends_with("decisions/my-adr.md"));

    // File should exist on disk.
    assert!(Path::new(&note.file_path).exists());

    // Should be retrievable.
    let fetched = repo.get(&note.id).await.unwrap().unwrap();
    assert_eq!(fetched.title, "My ADR");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn singleton_brief() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create(
            &project.id,
            tmp.path(),
            "Project Brief",
            "...",
            "brief",
            "[]",
        )
        .await
        .unwrap();

    assert_eq!(note.permalink, "brief");
    assert!(note.file_path.ends_with(".djinn/brief.md"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_by_permalink() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create(
            &project.id,
            tmp.path(),
            "A Pattern",
            "body",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

    let found = repo
        .get_by_permalink(&project.id, &note.permalink)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.id, note.id);
}

#[test]
fn mergeable_note_types_map_to_expected_folders_and_round_trip() {
    assert_eq!(folder_for_type("pattern"), "patterns");
    assert_eq!(folder_for_type("case"), "cases");
    assert_eq!(folder_for_type("pitfall"), "pitfalls");
    assert_eq!(folder_for_type("repo_map"), "reference/repo-maps");

    assert_eq!(
        permalink_for("case", "Task Recovery Example"),
        "cases/task-recovery-example"
    );
    assert_eq!(
        permalink_for("pitfall", "Retry Storm"),
        "pitfalls/retry-storm"
    );
    assert_eq!(
        permalink_for("repo_map", "Repository Map abc123"),
        "reference/repo-maps/repository-map-abc123"
    );

    assert_eq!(
        file_helpers::infer_note_type("patterns/reusable-flow"),
        "pattern"
    );
    assert_eq!(
        file_helpers::infer_note_type("cases/task-recovery-example"),
        "case"
    );
    assert_eq!(
        file_helpers::infer_note_type("pitfalls/retry-storm"),
        "pitfall"
    );
    assert_eq!(
        file_helpers::infer_note_type("reference/repo-maps/repository-map-abc123"),
        "repo_map"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_supports_case_and_pitfall_note_types() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let case_note = repo
        .create(
            &project.id,
            tmp.path(),
            "Incident Recovery",
            "Case details",
            "case",
            "[]",
        )
        .await
        .unwrap();
    assert_eq!(case_note.note_type, "case");
    assert_eq!(case_note.folder, "cases");
    assert_eq!(case_note.permalink, "cases/incident-recovery");
    assert!(case_note.file_path.ends_with("cases/incident-recovery.md"));

    let pitfall_note = repo
        .create(
            &project.id,
            tmp.path(),
            "Retry Storm",
            "Pitfall details",
            "pitfall",
            "[]",
        )
        .await
        .unwrap();
    assert_eq!(pitfall_note.note_type, "pitfall");
    assert_eq!(pitfall_note.folder, "pitfalls");
    assert_eq!(pitfall_note.permalink, "pitfalls/retry-storm");
    assert!(pitfall_note.file_path.ends_with("pitfalls/retry-storm.md"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_backed_notes_skip_filesystem_and_round_trip_storage() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create_db_note(&project.id, "Extracted Pattern", "body", "pattern", "[]")
        .await
        .unwrap();

    assert_eq!(note.storage, "db");
    assert_eq!(note.file_path, "");
    assert!(
        !tmp.path()
            .join(".djinn/patterns/extracted-pattern.md")
            .exists()
    );

    let fetched = repo.get(&note.id).await.unwrap().unwrap();
    assert_eq!(fetched.storage, "db");
    assert_eq!(fetched.file_path, "");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_note() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, mut rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create(
            &project.id,
            tmp.path(),
            "Original",
            "old content",
            "research",
            "[]",
        )
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap(); // NoteCreated

    let updated = repo
        .update(&note.id, "Original", "new content", r#"["updated"]"#)
        .await
        .unwrap();
    assert_eq!(updated.content, "new content");
    assert_eq!(updated.tags, r#"["updated"]"#);

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "note");
    assert_eq!(envelope.action, "updated");
    let n: Note = envelope.parse_payload().unwrap();
    assert_eq!(n.content, "new content");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_db_backed_note_skips_filesystem_writes() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create_db_note(&project.id, "DB Note", "old content", "case", "[]")
        .await
        .unwrap();

    let updated = repo
        .update(&note.id, "DB Note", "new content", r#"["updated"]"#)
        .await
        .unwrap();

    assert_eq!(updated.storage, "db");
    assert_eq!(updated.content, "new content");
    assert!(!tmp.path().join(".djinn/cases/db-note.md").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upsert_db_note_by_permalink_creates_and_updates_repo_map_note() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let created = repo
        .upsert_db_note_by_permalink(
            &project.id,
            "reference/repo-maps/head",
            "Repository Map head",
            "src/main.rs",
            "repo_map",
            r#"["repo-map"]"#,
        )
        .await
        .unwrap();

    assert_eq!(created.note_type, "repo_map");
    assert_eq!(created.folder, "reference/repo-maps");
    assert_eq!(created.permalink, "reference/repo-maps/head");
    assert_eq!(created.storage, "db");

    let updated = repo
        .upsert_db_note_by_permalink(
            &project.id,
            "reference/repo-maps/head",
            "Repository Map head",
            "src/lib.rs",
            "repo_map",
            r#"["repo-map","updated"]"#,
        )
        .await
        .unwrap();

    assert_eq!(updated.id, created.id);
    assert_eq!(updated.content, "src/lib.rs");
    assert_eq!(updated.tags, r#"["repo-map","updated"]"#);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_note() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, mut rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create(
            &project.id,
            tmp.path(),
            "To Delete",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap();
    let file_path = note.file_path.clone();

    repo.delete(&note.id).await.unwrap();
    assert!(repo.get(&note.id).await.unwrap().is_none());
    assert!(!Path::new(&file_path).exists());

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "note");
    assert_eq!(envelope.action, "deleted");
    assert_eq!(envelope.payload["id"].as_str().unwrap(), note.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_db_backed_note_keeps_missing_files_irrelevant() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create_db_note(&project.id, "DB Delete", "body", "pitfall", "[]")
        .await
        .unwrap();

    repo.delete(&note.id).await.unwrap();
    assert!(repo.get(&note.id).await.unwrap().is_none());
    assert!(!tmp.path().join(".djinn/pitfalls/db-delete.md").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_backed_note_crud_persists_db_and_filesystem_state_changes() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let created = repo
        .create(
            &project.id,
            tmp.path(),
            "Persistence Note",
            "created body",
            "reference",
            r#"["initial"]"#,
        )
        .await
        .unwrap();

    let persisted_created = repo.get(&created.id).await.unwrap().unwrap();
    assert_eq!(persisted_created.content, "created body");
    assert_eq!(persisted_created.tags, r#"["initial"]"#);
    assert_eq!(persisted_created.storage, "file");
    assert_eq!(persisted_created.file_path, created.file_path);
    assert!(Path::new(&persisted_created.file_path).exists());
    let created_disk = std::fs::read_to_string(&persisted_created.file_path).unwrap();
    assert!(created_disk.contains("created body"));

    let updated = repo
        .update(
            &created.id,
            "Persistence Note",
            "updated body",
            r#"["updated"]"#,
        )
        .await
        .unwrap();
    assert_eq!(updated.content, "updated body");
    assert_eq!(updated.tags, r#"["updated"]"#);

    let persisted_updated = sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
        .bind(&created.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(persisted_updated.content, "updated body");
    assert_eq!(persisted_updated.tags, r#"["updated"]"#);
    assert_eq!(persisted_updated.updated_at, updated.updated_at);
    let updated_disk = std::fs::read_to_string(&persisted_updated.file_path).unwrap();
    assert!(updated_disk.contains("updated body"));
    assert!(!updated_disk.contains("created body"));

    repo.delete(&created.id).await.unwrap();
    assert!(repo.get(&created.id).await.unwrap().is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM notes WHERE id = ?1")
            .bind(&created.id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        0
    );
    assert!(!Path::new(&persisted_updated.file_path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_backed_note_crud_persists_state_without_filesystem_side_effects() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let created = repo
        .create_db_note(
            &project.id,
            "DB Persistence",
            "db body",
            "case",
            r#"["tagged"]"#,
        )
        .await
        .unwrap();
    assert_eq!(created.storage, "db");
    assert_eq!(created.file_path, "");

    let persisted_created = sqlx::query_as::<_, Note>(NOTE_SELECT_WHERE_ID)
        .bind(&created.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(persisted_created.content, "db body");
    assert_eq!(persisted_created.tags, r#"["tagged"]"#);
    assert!(!tmp.path().join(".djinn/cases/db-persistence.md").exists());

    let updated = repo
        .update(
            &created.id,
            "DB Persistence",
            "db body updated",
            r#"["retagged"]"#,
        )
        .await
        .unwrap();
    assert_eq!(updated.content, "db body updated");
    assert_eq!(updated.tags, r#"["retagged"]"#);

    let fetched = repo
        .get_by_permalink(&project.id, &created.permalink)
        .await
        .unwrap();
    assert_eq!(fetched.unwrap().content, "db body updated");

    repo.delete(&created.id).await.unwrap();
    assert!(repo.get(&created.id).await.unwrap().is_none());
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM notes WHERE id = ?1")
            .bind(&created.id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        0
    );
    assert!(!tmp.path().join(".djinn/cases/db-persistence.md").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_update_delete_use_worktree_disk_path_but_keep_canonical_db_path() {
    let project_tmp = crate::database::test_tempdir().unwrap();
    let worktree_tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, project_tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx))
        .with_worktree_root(Some(worktree_tmp.path().to_path_buf()));

    let note = repo
        .create(
            &project.id,
            project_tmp.path(),
            "Worktree Note",
            "body",
            "research",
            "[]",
        )
        .await
        .unwrap();

    let canonical_path = project_tmp.path().join(".djinn/research/worktree-note.md");
    let worktree_path = worktree_tmp.path().join(".djinn/research/worktree-note.md");

    assert_eq!(
        std::path::Path::new(&note.file_path),
        canonical_path.as_path()
    );
    assert!(worktree_path.exists());
    assert!(!canonical_path.exists());

    repo.update(&note.id, &note.title, "updated body", &note.tags)
        .await
        .unwrap();
    let updated_file = std::fs::read_to_string(&worktree_path).unwrap();
    assert!(updated_file.contains("updated body"));

    repo.delete(&note.id).await.unwrap();
    assert!(!worktree_path.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fts5_search() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        tmp.path(),
        "Rust Database Choice",
        "We chose rusqlite for its simplicity and bundled SQLite.",
        "adr",
        "[]",
    )
    .await
    .unwrap();
    repo.create(
        &project.id,
        tmp.path(),
        "Connection Strategy",
        "Use direct MCP connections for local operation.",
        "adr",
        "[]",
    )
    .await
    .unwrap();

    // Search for "rusqlite" — should hit only the first note.
    let results = repo
        .search(&project.id, "rusqlite", None, None, None, 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "Rust Database Choice");
    assert!(results[0].snippet.contains("rusqlite"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fts5_search_folder_filter() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        tmp.path(),
        "Design Note",
        "common term",
        "design",
        "[]",
    )
    .await
    .unwrap();
    repo.create(
        &project.id,
        tmp.path(),
        "Research Note",
        "common term",
        "research",
        "[]",
    )
    .await
    .unwrap();

    let results = repo
        .search(&project.id, "common", None, Some("design"), None, 10)
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].folder, "design");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fts5_search_prefers_title_over_content() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        tmp.path(),
        "rankneedle in title",
        "unrelated body",
        "research",
        "[]",
    )
    .await
    .unwrap();
    repo.create(
        &project.id,
        tmp.path(),
        "different title",
        "This content has rankneedle.",
        "research",
        "[]",
    )
    .await
    .unwrap();

    let results = repo
        .search(&project.id, "rankneedle", None, None, None, 10)
        .await
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "rankneedle in title");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fts5_search_prefers_tags_over_content() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        tmp.path(),
        "tag-ranked note",
        "unrelated body",
        "research",
        r#"["ranktag"]"#,
    )
    .await
    .unwrap();
    repo.create(
        &project.id,
        tmp.path(),
        "content-ranked note",
        "This content has ranktag.",
        "research",
        "[]",
    )
    .await
    .unwrap();

    let results = repo
        .search(&project.id, "ranktag", None, None, None, 10)
        .await
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "tag-ranked note");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedup_candidates_returns_empty_for_empty_project() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let results = repo
        .dedup_candidates(&project.id, "decisions", "adr", "shared term", 10)
        .await
        .unwrap();

    assert!(results.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedup_candidates_returns_no_matches_when_query_has_no_hits() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        tmp.path(),
        "Rust Database Choice",
        "We chose rusqlite for local simplicity.",
        "adr",
        "[]",
    )
    .await
    .unwrap();

    let results = repo
        .dedup_candidates(
            &project.id,
            "decisions",
            "adr",
            "completely unrelated phrase",
            10,
        )
        .await
        .unwrap();

    assert!(results.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dedup_candidates_filter_by_folder_and_note_type() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let matching = repo
        .create(
            &project.id,
            tmp.path(),
            "Repository Dedup Strategy",
            "shared dedup token appears here",
            "adr",
            "[]",
        )
        .await
        .unwrap();
    repo.update_summaries(
        &matching.id,
        Some("matching abstract"),
        Some("matching overview"),
    )
    .await
    .unwrap();

    repo.create(
        &project.id,
        tmp.path(),
        "Repository Dedup Research",
        "shared dedup token appears here",
        "research",
        "[]",
    )
    .await
    .unwrap();

    repo.create(
        &project.id,
        tmp.path(),
        "Design Dedup Strategy",
        "shared dedup token appears here",
        "design",
        "[]",
    )
    .await
    .unwrap();

    let results = repo
        .dedup_candidates(&project.id, "decisions", "adr", "shared dedup token", 10)
        .await
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, matching.id);
    assert_eq!(results[0].folder, "decisions");
    assert_eq!(results[0].note_type, "adr");
    assert_eq!(results[0].abstract_.as_deref(), Some("matching abstract"));
    assert_eq!(results[0].overview.as_deref(), Some("matching overview"));
    assert!(results[0].score > -3.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_rrf_prefers_higher_access_count_for_equivalent_matches() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let high = repo
        .create(
            &project.id,
            tmp.path(),
            "sharedterm alpha",
            "same content",
            "research",
            "[]",
        )
        .await
        .unwrap();
    let low = repo
        .create(
            &project.id,
            tmp.path(),
            "sharedterm beta",
            "same content",
            "research",
            "[]",
        )
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET access_count = 10 WHERE id = ?1")
        .bind(&high.id)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE notes SET access_count = 0 WHERE id = ?1")
        .bind(&low.id)
        .execute(db.pool())
        .await
        .unwrap();

    let results = repo
        .search(&project.id, "sharedterm", None, None, None, 10)
        .await
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].id, high.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_confidence_reads_updates_and_persists() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note = repo
        .create(
            &project.id,
            tmp.path(),
            "Confidence Note",
            "body",
            "research",
            "[]",
        )
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET confidence = 0.5 WHERE id = ?1")
        .bind(&note.id)
        .execute(db.pool())
        .await
        .unwrap();

    let updated = repo
        .update_confidence(&note.id, scoring::TASK_SUCCESS)
        .await
        .unwrap();
    assert!(updated > 0.5);

    let stored: f64 = sqlx::query_scalar("SELECT confidence FROM notes WHERE id = ?1")
        .bind(&note.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert!((stored - updated).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_rrf_confidence_lowers_equivalent_match_ranking() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let high = repo
        .create(
            &project.id,
            tmp.path(),
            "sharedconfidence alpha",
            "same content",
            "research",
            "[]",
        )
        .await
        .unwrap();
    let low = repo
        .create(
            &project.id,
            tmp.path(),
            "sharedconfidence beta",
            "same content",
            "research",
            "[]",
        )
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET access_count = 0, confidence = 1.0 WHERE id = ?1")
        .bind(&high.id)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE notes SET access_count = 0, confidence = 0.5 WHERE id = ?1")
        .bind(&low.id)
        .execute(db.pool())
        .await
        .unwrap();

    let results = repo
        .search(&project.id, "sharedconfidence", None, None, None, 10)
        .await
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].id, high.id);
    assert_eq!(results[1].id, low.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn catalog_generation() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(&project.id, tmp.path(), "ADR One", "body", "adr", "[]")
        .await
        .unwrap();
    repo.create(
        &project.id,
        tmp.path(),
        "Research One",
        "body",
        "research",
        "[]",
    )
    .await
    .unwrap();

    let decisions = repo.list(&project.id, Some("decisions")).await.unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0].title, "ADR One");

    let all = repo.list(&project.id, None).await.unwrap();
    assert_eq!(all.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_emits_event() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, mut rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        tmp.path(),
        "Event Note",
        "body",
        "design",
        "[]",
    )
    .await
    .unwrap();

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "note");
    assert_eq!(envelope.action, "created");
    let n: Note = envelope.parse_payload().unwrap();
    assert_eq!(n.title, "Event Note");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slugify_roundtrip() {
    assert_eq!(slugify("My ADR Title"), "my-adr-title");
    assert_eq!(slugify("Hello  World"), "hello-world");
    assert_eq!(slugify("--leading dashes--"), "leading-dashes");
    assert_eq!(slugify("rust/database"), "rust-database");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn touch_accessed_does_not_emit_event() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, mut rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create(
            &project.id,
            tmp.path(),
            "Touch Me",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap(); // NoteCreated

    repo.update_summaries(&note.id, Some("short"), Some("longer summary"))
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap(); // NoteUpdated

    repo.touch_accessed(&note.id).await.unwrap();

    // No event should be in the channel when summaries already exist.
    assert!(rx.try_recv().is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn touch_accessed_increments_access_count() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create(
            &project.id,
            tmp.path(),
            "Touch Count",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    for _ in 0..3 {
        repo.touch_accessed(&note.id).await.unwrap();
    }

    let updated = repo.get(&note.id).await.unwrap().unwrap();
    assert_eq!(updated.access_count, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn touch_accessed_emits_missing_summary_signal_when_summaries_are_missing() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, mut rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create(
            &project.id,
            tmp.path(),
            "Needs Summary",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap(); // NoteCreated

    repo.touch_accessed(&note.id).await.unwrap();

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "note");
    assert_eq!(envelope.action, "missing_summary");
    assert_eq!(envelope.id.as_deref(), Some(note.id.as_str()));
    assert_eq!(envelope.project_id.as_deref(), Some(project.id.as_str()));
    assert_eq!(envelope.payload["id"].as_str(), Some(note.id.as_str()));
    assert_eq!(
        envelope.payload["project_id"].as_str(),
        Some(project.id.as_str())
    );
    assert_eq!(envelope.payload["missing_abstract"].as_bool(), Some(true));
    assert_eq!(envelope.payload["missing_overview"].as_bool(), Some(true));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn update_summaries_persists_summary_fields() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, mut rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create(
            &project.id,
            tmp.path(),
            "Summarize Me",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap(); // NoteCreated

    let updated = repo
        .update_summaries(&note.id, Some("abstract"), Some("overview"))
        .await
        .unwrap();

    assert_eq!(updated.abstract_.as_deref(), Some("abstract"));
    assert_eq!(updated.overview.as_deref(), Some("overview"));

    let persisted = repo.get_summary_state(&note.id).await.unwrap().unwrap();
    assert_eq!(persisted.abstract_.as_deref(), Some("abstract"));
    assert_eq!(persisted.overview.as_deref(), Some("overview"));

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "note");
    assert_eq!(envelope.action, "updated");
}

// ── Wikilink graph tests ──────────────────────────────────────────────────

#[test]
fn extract_wikilinks_basic() {
    let links = indexing::extract_wikilinks("See [[Rust Database Choice]] for details.");
    assert_eq!(links, vec![("Rust Database Choice".to_string(), None)]);
}

#[test]
fn extract_wikilinks_with_display() {
    let links = indexing::extract_wikilinks("See [[Rust DB|the ADR]] for details.");
    assert_eq!(
        links,
        vec![("Rust DB".to_string(), Some("the ADR".to_string()))]
    );
}

#[test]
fn extract_wikilinks_multiple() {
    let links = indexing::extract_wikilinks("[[A]] and [[B|Bee]] and [[C]]");
    assert_eq!(links.len(), 3);
    assert_eq!(links[0], ("A".to_string(), None));
    assert_eq!(links[1], ("B".to_string(), Some("Bee".to_string())));
    assert_eq!(links[2], ("C".to_string(), None));
}

#[test]
fn extract_wikilinks_empty_and_none() {
    let links = indexing::extract_wikilinks("No links here. [[]] empty.");
    assert!(links.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wikilink_resolves_on_create() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    // Create target first.
    let target = repo
        .create(
            &project.id,
            tmp.path(),
            "Connection Strategy",
            "body",
            "adr",
            "[]",
        )
        .await
        .unwrap();

    // Create source with a wikilink to the target by title.
    repo.create(
        &project.id,
        tmp.path(),
        "Overview",
        "See [[Connection Strategy]] for details.",
        "research",
        "[]",
    )
    .await
    .unwrap();

    let graph = repo.graph(&project.id).await.unwrap();
    assert_eq!(graph.edges.len(), 1);
    assert_eq!(graph.edges[0].target_id, target.id);
    assert_eq!(graph.edges[0].raw_text, "Connection Strategy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn broken_link_detection() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        tmp.path(),
        "Source Note",
        "Links to [[Missing Note]] which does not exist.",
        "research",
        "[]",
    )
    .await
    .unwrap();

    let broken = repo.broken_links(&project.id, None).await.unwrap();
    assert_eq!(broken.len(), 1);
    assert_eq!(broken[0].raw_text, "Missing Note");
    assert_eq!(broken[0].source_title, "Source Note");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_detection() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    // Two notes: source links to target, isolated is orphaned.
    let target = repo
        .create(&project.id, tmp.path(), "Target", "body", "adr", "[]")
        .await
        .unwrap();
    repo.create(
        &project.id,
        tmp.path(),
        "Source",
        "See [[Target]].",
        "research",
        "[]",
    )
    .await
    .unwrap();
    repo.create(
        &project.id,
        tmp.path(),
        "Isolated",
        "no links",
        "pattern",
        "[]",
    )
    .await
    .unwrap();

    let orphans = repo.orphans(&project.id, None).await.unwrap();
    // Target has an inbound link; Source and Isolated do not.
    let orphan_titles: Vec<&str> = orphans.iter().map(|o| o.title.as_str()).collect();
    assert!(
        !orphan_titles.contains(&target.title.as_str()),
        "target should not be orphan"
    );
    assert!(
        orphan_titles.contains(&"Source"),
        "Source has no inbound links"
    );
    assert!(
        orphan_titles.contains(&"Isolated"),
        "Isolated has no inbound links"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphan_detection_excludes_singletons_and_catalog_from_listing_and_health() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        tmp.path(),
        "Project Brief",
        "brief body",
        "brief",
        "[]",
    )
    .await
    .unwrap();
    repo.create(
        &project.id,
        tmp.path(),
        "Project Roadmap",
        "roadmap body",
        "roadmap",
        "[]",
    )
    .await
    .unwrap();
    repo.create_db_note(&project.id, "Catalog", "generated catalog", "catalog", "[]")
        .await
        .unwrap();
    repo.create(
        &project.id,
        tmp.path(),
        "Reachable Target",
        "body",
        "adr",
        "[]",
    )
    .await
    .unwrap();
    repo.create(
        &project.id,
        tmp.path(),
        "Linked Source",
        "See [[Reachable Target]].",
        "research",
        "[]",
    )
    .await
    .unwrap();
    repo.create(
        &project.id,
        tmp.path(),
        "Real Orphan",
        "no inbound links",
        "pattern",
        "[]",
    )
    .await
    .unwrap();

    let orphans = repo.orphans(&project.id, None).await.unwrap();
    let orphan_titles: Vec<&str> = orphans.iter().map(|o| o.title.as_str()).collect();
    assert!(orphan_titles.contains(&"Linked Source"));
    assert!(orphan_titles.contains(&"Real Orphan"));

    let health = repo.health(&project.id).await.unwrap();
    assert_eq!(health.orphan_note_count, orphans.len() as i64);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_previously_broken_links_on_create() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    // Create source first (target doesn't exist yet → broken link).
    repo.create(
        &project.id,
        tmp.path(),
        "Source",
        "See [[Future Note]].",
        "research",
        "[]",
    )
    .await
    .unwrap();
    assert_eq!(repo.broken_links(&project.id, None).await.unwrap().len(), 1);

    // Now create the target → broken link should be resolved.
    repo.create(&project.id, tmp.path(), "Future Note", "body", "adr", "[]")
        .await
        .unwrap();
    assert_eq!(repo.broken_links(&project.id, None).await.unwrap().len(), 0);
    assert_eq!(repo.graph(&project.id).await.unwrap().edges.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reindex_from_disk_detects_created_updated_and_deleted() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let decisions_dir = tmp.path().join(".djinn").join("decisions");
    std::fs::create_dir_all(&decisions_dir).unwrap();

    let existing_path = decisions_dir.join("existing.md");
    std::fs::write(
        &existing_path,
        "---\ntitle: Existing\ntype: adr\ntags: []\n---\n\noriginal content",
    )
    .unwrap();

    let first = repo
        .reindex_from_disk(&project.id, tmp.path())
        .await
        .unwrap();
    assert_eq!(first.created, 1);
    assert_eq!(first.updated, 0);
    assert_eq!(first.deleted, 0);

    // Modify existing + add one new file.
    std::fs::write(
        &existing_path,
        "---\ntitle: Existing\ntype: adr\ntags: []\n---\n\nupdated content",
    )
    .unwrap();
    std::fs::write(
        decisions_dir.join("new-note.md"),
        "---\ntitle: New Note\ntype: adr\ntags: []\n---\n\nhello",
    )
    .unwrap();

    let second = repo
        .reindex_from_disk(&project.id, tmp.path())
        .await
        .unwrap();
    assert_eq!(second.created, 1);
    assert_eq!(second.updated, 1);
    assert_eq!(second.deleted, 0);

    // Delete one file from disk.
    std::fs::remove_file(decisions_dir.join("new-note.md")).unwrap();
    let third = repo
        .reindex_from_disk(&project.id, tmp.path())
        .await
        .unwrap();
    assert_eq!(third.deleted, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reindex_from_disk_keeps_db_backed_notes_when_files_are_missing() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let db_note = repo
        .create_db_note(&project.id, "Extracted Case", "db body", "case", "[]")
        .await
        .unwrap();
    let file_note = repo
        .create(
            &project.id,
            tmp.path(),
            "File Note",
            "file body",
            "adr",
            "[]",
        )
        .await
        .unwrap();

    std::fs::remove_file(&file_note.file_path).unwrap();

    let summary = repo
        .reindex_from_disk(&project.id, tmp.path())
        .await
        .unwrap();
    assert_eq!(summary.deleted, 1);
    assert!(repo.get(&db_note.id).await.unwrap().is_some());
    assert!(repo.get(&file_note.id).await.unwrap().is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reindex_from_disk_backfill_can_normalize_extracted_notes_to_db_storage() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let legacy_case = repo
        .create(
            &project.id,
            tmp.path(),
            "Legacy Extracted Case",
            "legacy db migration body",
            "case",
            "[]",
        )
        .await
        .unwrap();
    let legacy_pattern = repo
        .create(
            &project.id,
            tmp.path(),
            "Legacy Extracted Pattern",
            "legacy pattern body",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let legacy_pitfall = repo
        .create(
            &project.id,
            tmp.path(),
            "Legacy Extracted Pitfall",
            "legacy pitfall body",
            "pitfall",
            "[]",
        )
        .await
        .unwrap();

    for note in [&legacy_case, &legacy_pattern, &legacy_pitfall] {
        assert!(
            Path::new(&note.file_path).exists(),
            "legacy extracted note should start on disk"
        );
    }

    sqlx::query(
        "UPDATE notes
         SET storage = 'db',
             file_path = ''
         WHERE project_id = ?1 AND note_type IN ('case', 'pattern', 'pitfall')",
    )
    .bind(&project.id)
    .execute(db.pool())
    .await
    .unwrap();

    for note in [&legacy_case, &legacy_pattern, &legacy_pitfall] {
        let path = Path::new(&note.file_path);
        if path.exists() {
            std::fs::remove_file(path).unwrap();
        }
    }

    let summary = repo
        .reindex_from_disk(&project.id, tmp.path())
        .await
        .unwrap();
    assert_eq!(
        summary.deleted, 0,
        "db-backed migrated notes should survive reindex"
    );

    let notes = repo.list(&project.id, None).await.unwrap();
    let migrated: Vec<_> = notes
        .iter()
        .filter(|note| matches!(note.note_type.as_str(), "case" | "pattern" | "pitfall"))
        .collect();
    assert_eq!(migrated.len(), 3);
    for note in migrated {
        assert_eq!(note.storage, "db");
        assert!(note.file_path.is_empty());
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_affinity_scores_task_epic_blocker_and_max() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let task_note = repo
        .create(
            &project.id,
            tmp.path(),
            "Task Note",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let epic_note = repo
        .create(
            &project.id,
            tmp.path(),
            "Epic Note",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let blocker_note = repo
        .create(
            &project.id,
            tmp.path(),
            "Blocker Note",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let overlap_note = repo
        .create(
            &project.id,
            tmp.path(),
            "Overlap Note",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let epic_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO epics (id, project_id, short_id, title, description, emoji, color, owner, memory_refs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(&epic_id)
    .bind(&project.id)
    .bind("EP-1")
    .bind("Epic")
    .bind("")
    .bind("")
    .bind("")
    .bind("")
    .bind(serde_json::json!([epic_note.id.clone(), task_note.id.clone(), overlap_note.id.clone()]).to_string())
    .execute(db.pool())
    .await
    .unwrap();

    let task_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                            issue_type, priority, owner, status, continuation_count, memory_refs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
    )
    .bind(&task_id)
    .bind(&project.id)
    .bind("T-1")
    .bind(&epic_id)
    .bind("Task")
    .bind("")
    .bind("")
    .bind("task")
    .bind(0_i64)
    .bind("")
    .bind("open")
    .bind(0_i64)
    .bind(serde_json::json!([task_note.id.clone(), overlap_note.id.clone()]).to_string())
    .execute(db.pool())
    .await
    .unwrap();

    let blocker_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                            issue_type, priority, owner, status, continuation_count, memory_refs)
         VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )
    .bind(&blocker_id)
    .bind(&project.id)
    .bind("T-2")
    .bind("Blocker")
    .bind("")
    .bind("")
    .bind("task")
    .bind(0_i64)
    .bind("")
    .bind("open")
    .bind(0_i64)
    .bind(serde_json::json!([blocker_note.id.clone(), epic_note.id.clone(), overlap_note.id.clone()]).to_string())
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query("INSERT INTO blockers (task_id, blocking_task_id) VALUES (?1, ?2)")
        .bind(&task_id)
        .bind(&blocker_id)
        .execute(db.pool())
        .await
        .unwrap();

    let none_scores = repo.task_affinity_scores(&project.id, None).await.unwrap();
    assert!(none_scores.is_empty());

    let scores = repo
        .task_affinity_scores(&project.id, Some(&task_id))
        .await
        .unwrap();

    let score_map: std::collections::HashMap<String, f64> = scores.into_iter().collect();
    assert_eq!(score_map.get(&task_note.id), Some(&1.0));
    assert_eq!(score_map.get(&epic_note.id), Some(&0.7));
    assert_eq!(score_map.get(&blocker_note.id), Some(&0.5));
    assert_eq!(score_map.get(&overlap_note.id), Some(&1.0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_affinity_scores_include_repo_map_neighbors_for_task_memory_refs() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let adr = repo
        .create(
            &project.id,
            tmp.path(),
            "Repository Map ADR",
            "See [[reference/repo-maps/repository-map-head]] and keep structural layout current.",
            "adr",
            "[]",
        )
        .await
        .unwrap();
    let repo_map = repo
        .upsert_db_note_by_permalink(
            &project.id,
            "reference/repo-maps/repository-map-head",
            "Repository Map head",
            "server/src/repo_map.rs\nserver/crates/djinn-db/src/repositories/note/search.rs",
            "repo_map",
            r#"["repo-map"]"#,
        )
        .await
        .unwrap();

    let task_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO tasks (id, project_id, short_id, epic_id, title, description, design,
                            issue_type, priority, owner, status, continuation_count, memory_refs)
         VALUES (?1, ?2, ?3, NULL, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )
    .bind(&task_id)
    .bind(&project.id)
    .bind("T-RM")
    .bind("Task")
    .bind("")
    .bind("")
    .bind("task")
    .bind(0_i64)
    .bind("")
    .bind("open")
    .bind(0_i64)
    .bind(serde_json::json!([adr.permalink.clone()]).to_string())
    .execute(db.pool())
    .await
    .unwrap();

    let scores = repo
        .task_affinity_scores(&project.id, Some(&task_id))
        .await
        .unwrap();

    let score_map: std::collections::HashMap<String, f64> = scores.into_iter().collect();
    assert_eq!(score_map.get(&adr.id), Some(&1.0));
    assert!(
        (score_map.get(&repo_map.id).copied().unwrap() - 0.245).abs() < 1e-9,
        "expected repo-map affinity score of 0.245, got {:?}",
        score_map.get(&repo_map.id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unrelated_search_query_does_not_return_repo_map_notes() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    repo.create(
        &project.id,
        tmp.path(),
        "Decision Log",
        "ordinary product planning note",
        "adr",
        "[]",
    )
    .await
    .unwrap();
    repo.upsert_db_note_by_permalink(
        &project.id,
        "reference/repo-maps/repository-map-head",
        "Repository Map head",
        "server/src/repo_map.rs\nserver/crates/djinn-db/src/repositories/note/search.rs",
        "repo_map",
        r#"["repo-map"]"#,
    )
    .await
    .unwrap();

    let results = repo
        .search(
            &project.id,
            "ordinary product planning",
            None,
            None,
            None,
            10,
        )
        .await
        .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].note_type, "adr");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_empty_for_seed_without_links() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let seed = repo
        .create(
            &project.id,
            tmp.path(),
            "Seed",
            "no links",
            "research",
            "[]",
        )
        .await
        .unwrap();

    let scores = repo.graph_proximity_scores(&[seed.id], 2).await.unwrap();
    assert!(scores.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_linear_chain_hop_decay() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(&project.id, tmp.path(), "A", "[[B]]", "research", "[]")
        .await
        .unwrap();
    let b = repo
        .create(&project.id, tmp.path(), "B", "[[C]]", "research", "[]")
        .await
        .unwrap();
    let c = repo
        .create(&project.id, tmp.path(), "C", "", "research", "[]")
        .await
        .unwrap();

    repo.reindex_from_disk(&project.id, tmp.path())
        .await
        .unwrap();

    let seed_id = a.id.clone();
    let scores = repo
        .graph_proximity_scores(std::slice::from_ref(&seed_id), 2)
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert_eq!(m.get(&b.id).copied().unwrap(), 0.7);
    assert!((m.get(&c.id).copied().unwrap() - 0.49).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_diamond_keeps_max_path_score_not_sum() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(
            &project.id,
            tmp.path(),
            "A",
            "[[B]] [[D]]",
            "research",
            "[]",
        )
        .await
        .unwrap();
    repo.create(&project.id, tmp.path(), "B", "[[C]]", "research", "[]")
        .await
        .unwrap();
    let c = repo
        .create(&project.id, tmp.path(), "C", "", "research", "[]")
        .await
        .unwrap();
    repo.create(&project.id, tmp.path(), "D", "[[C]]", "research", "[]")
        .await
        .unwrap();

    repo.reindex_from_disk(&project.id, tmp.path())
        .await
        .unwrap();

    let seed_id = a.id.clone();
    let scores = repo.graph_proximity_scores(&[seed_id], 2).await.unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert!((m.get(&c.id).copied().unwrap() - 0.49).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_excludes_beyond_max_hops() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(&project.id, tmp.path(), "A", "[[B]]", "research", "[]")
        .await
        .unwrap();
    repo.create(&project.id, tmp.path(), "B", "[[C]]", "research", "[]")
        .await
        .unwrap();
    let d = repo
        .create(&project.id, tmp.path(), "D", "", "research", "[]")
        .await
        .unwrap();
    repo.update(&d.id, "D", "[[A]]", "[]").await.unwrap();

    repo.reindex_from_disk(&project.id, tmp.path())
        .await
        .unwrap();

    let seed_id = a.id.clone();
    let scores = repo
        .graph_proximity_scores(std::slice::from_ref(&seed_id), 2)
        .await
        .unwrap();
    let ids: std::collections::HashSet<_> = scores.into_iter().map(|(id, _)| id).collect();
    // no 3-hop specific assertion target; ensure algorithm bounded and excludes seed
    assert!(!ids.contains(&seed_id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_association_applies_weighted_decay() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(&project.id, tmp.path(), "A", "", "research", "[]")
        .await
        .unwrap();
    let b = repo
        .create(&project.id, tmp.path(), "B", "", "research", "[]")
        .await
        .unwrap();

    let (note_a_id, note_b_id) = if a.id < b.id {
        (a.id.clone(), b.id.clone())
    } else {
        (b.id.clone(), a.id.clone())
    };

    sqlx::query(
        "INSERT INTO note_associations (note_a_id, note_b_id, weight, co_access_count, last_co_access)
         VALUES (?1, ?2, ?3, 1, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
    )
    .bind(&note_a_id)
    .bind(&note_b_id)
    .bind(0.5_f64)
    .execute(repo.db.pool())
    .await
    .unwrap();

    let scores = repo
        .graph_proximity_scores(std::slice::from_ref(&a.id), 2)
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert!((m.get(&b.id).copied().unwrap() - 0.35).abs() < 1e-9);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_proximity_ignores_low_weight_association_noise() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let a = repo
        .create(&project.id, tmp.path(), "A", "", "research", "[]")
        .await
        .unwrap();
    let b = repo
        .create(&project.id, tmp.path(), "B", "", "research", "[]")
        .await
        .unwrap();

    let (note_a_id, note_b_id) = if a.id < b.id {
        (a.id.clone(), b.id.clone())
    } else {
        (b.id.clone(), a.id.clone())
    };

    sqlx::query(
        "INSERT INTO note_associations (note_a_id, note_b_id, weight, co_access_count, last_co_access)
         VALUES (?1, ?2, ?3, 1, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
    )
    .bind(&note_a_id)
    .bind(&note_b_id)
    .bind(0.01_f64)
    .execute(repo.db.pool())
    .await
    .unwrap();

    let scores = repo.graph_proximity_scores(&[a.id], 2).await.unwrap();
    assert!(scores.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_scores_empty_candidates_returns_empty() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let scores = repo.temporal_scores(&project.id, &[]).await.unwrap();
    assert!(scores.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_scores_higher_access_count_wins_same_age() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let high = repo
        .create(
            &project.id,
            tmp.path(),
            "High Access",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let low = repo
        .create(
            &project.id,
            tmp.path(),
            "Low Access",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    sqlx::query(
        "UPDATE notes
         SET created_at = datetime('now', '-1 day'),
             updated_at = datetime('now', '-1 day')
         WHERE id IN (?1, ?2)",
    )
    .bind(&high.id)
    .bind(&low.id)
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query("UPDATE notes SET access_count = 10 WHERE id = ?1")
        .bind(&high.id)
        .execute(db.pool())
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET access_count = 0 WHERE id = ?1")
        .bind(&low.id)
        .execute(db.pool())
        .await
        .unwrap();

    let scores = repo
        .temporal_scores(&project.id, &[high.id.clone(), low.id.clone()])
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert!(m[&high.id] > m[&low.id]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_scores_recent_update_wins_same_access_count() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let recent = repo
        .create(&project.id, tmp.path(), "Recent", "body", "reference", "[]")
        .await
        .unwrap();
    let stale = repo
        .create(&project.id, tmp.path(), "Stale", "body", "reference", "[]")
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET access_count = 3 WHERE id IN (?1, ?2)")
        .bind(&recent.id)
        .bind(&stale.id)
        .execute(db.pool())
        .await
        .unwrap();

    sqlx::query(
        "UPDATE notes SET created_at = datetime('now', '-30 day') WHERE id IN (?1, ?2)",
    )
    .bind(&recent.id)
    .bind(&stale.id)
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query("UPDATE notes SET updated_at = datetime('now') WHERE id = ?1")
        .bind(&recent.id)
        .execute(db.pool())
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET updated_at = datetime('now', '-30 day') WHERE id = ?1")
        .bind(&stale.id)
        .execute(db.pool())
        .await
        .unwrap();

    let scores = repo
        .temporal_scores(&project.id, &[recent.id.clone(), stale.id.clone()])
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();
    assert!(m[&recent.id] > m[&stale.id]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn temporal_scores_edge_cases_are_finite() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let zero_age = repo
        .create(
            &project.id,
            tmp.path(),
            "Zero Age",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let old = repo
        .create(&project.id, tmp.path(), "Old", "body", "reference", "[]")
        .await
        .unwrap();

    sqlx::query(
        "UPDATE notes
         SET access_count = 0,
             created_at = datetime('now'),
             updated_at = datetime('now')
         WHERE id = ?1",
    )
    .bind(&zero_age.id)
    .execute(db.pool())
    .await
    .unwrap();

    sqlx::query(
        "UPDATE notes
         SET access_count = 0,
             created_at = datetime('now', '-365 day'),
             updated_at = datetime('now', '-365 day')
         WHERE id = ?1",
    )
    .bind(&old.id)
    .execute(db.pool())
    .await
    .unwrap();

    let scores = repo
        .temporal_scores(&project.id, &[zero_age.id.clone(), old.id.clone()])
        .await
        .unwrap();
    let m: std::collections::HashMap<_, _> = scores.into_iter().collect();

    assert!(m[&zero_age.id].is_finite());
    assert!(m[&old.id].is_finite());
    assert!(m[&zero_age.id] > m[&old.id]);
}

async fn make_session(
    db: &Database,
    project_id: &str,
    task_id: Option<&str>,
    branch: &str,
) -> String {
    let id = uuid::Uuid::now_v7().to_string();
    let task_id = match task_id {
        Some(task_id) => Some(task_id.to_string()),
        None => {
            let epic_id = make_epic(db, project_id).await;
            Some(
                TaskRepository::new(db.clone(), EventBus::noop())
                    .create_with_ac(
                        &epic_id,
                        "Session Task",
                        "session task",
                        "session task design",
                        "task",
                        1,
                        "worker",
                        None,
                        Some(r#"[{"title":"session-ac"}]"#),
                    )
                    .await
                    .unwrap()
                    .id,
            )
        }
    };
    let has_branch: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('sessions') WHERE name = 'branch'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();

    if has_branch > 0 {
        sqlx::query(
            "INSERT INTO sessions (id, project_id, task_id, branch, status, started_at)
             VALUES (?1, ?2, ?3, ?4, 'completed', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
        )
        .bind(&id)
        .bind(project_id)
        .bind(task_id.as_deref())
        .bind(branch)
        .execute(db.pool())
        .await
        .unwrap();
    } else {
        sqlx::query(
            "INSERT INTO sessions (
                id,
                project_id,
                task_id,
                model_id,
                agent_type,
                started_at,
                status,
                tokens_in,
                tokens_out,
                worktree_path
            )
            VALUES (
                ?1,
                ?2,
                ?3,
                'test-model',
                ?4,
                strftime('%Y-%m-%dT%H:%M:%fZ', 'now'),
                'completed',
                0,
                0,
                NULL
            )",
        )
        .bind(&id)
        .bind(project_id)
        .bind(task_id.as_deref())
        .bind(branch)
        .execute(db.pool())
        .await
        .unwrap();
    }
    id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consolidation_lists_db_note_groups_and_clusters_deterministically() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let project_a = make_project(&db, tmp.path()).await;
    let project_b_root = crate::database::test_tempdir().unwrap();
    let project_b = make_project(&db, project_b_root.path()).await;
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    let alpha = repo
        .create_db_note(
            &project_a.id,
            "Schema seam prerequisite check",
            "Verify the prerequisite seam exists before wiring the schema seam. prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let beta = repo
        .create_db_note(
            &project_a.id,
            "Verify prerequisite seam before schema wiring",
            "Always verify the prerequisite seam exists before wiring the schema seam. prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let gamma = repo
        .create_db_note(
            &project_a.id,
            "Deterministic seam verification for schema query",
            "Use deterministic verification to confirm the prerequisite seam before schema query wiring. prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let _singleton = repo
        .create_db_note(
            &project_a.id,
            "Unrelated pitfall",
            "This content is unrelated and should not cluster with the prerequisite seam notes.",
            "pitfall",
            "[]",
        )
        .await
        .unwrap();
    let _other_project = repo
        .create_db_note(
            &project_b.id,
            "Project B duplicate cluster seed",
            "project b duplicate cluster seed prerequisite seam schema seam check",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

    for note in [&alpha, &beta, &gamma] {
        let abstract_text = format!(
            "{} prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            note.title
        );
        sqlx::query(
            "UPDATE notes
             SET abstract = ?2,
                 overview = ?3
             WHERE id = ?1",
        )
        .bind(&note.id)
        .bind(&abstract_text)
        .bind(&abstract_text)
        .execute(db.pool())
        .await
        .unwrap();
    }

    let groups = consolidation_repo.list_db_note_groups().await.unwrap();
    let mut got = groups
        .into_iter()
        .map(|group| (group.project_id, group.note_type, group.note_count))
        .collect::<Vec<_>>();
    got.sort();
    let mut expected = vec![
        (project_a.id.clone(), "pattern".to_string(), 3),
        (project_a.id.clone(), "pitfall".to_string(), 1),
        (project_b.id.clone(), "pattern".to_string(), 1),
    ];
    expected.sort();
    assert_eq!(got, expected);

    let clusters = consolidation_repo
        .likely_duplicate_clusters(&project_a.id, "pattern")
        .await
        .unwrap();
    assert_eq!(clusters.len(), 1);
    let cluster = &clusters[0];
    let cluster_note_ids = cluster
        .notes
        .iter()
        .map(|note| note.id.clone())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(cluster_note_ids.len(), 3);
    assert!(cluster_note_ids.contains(&alpha.id));
    assert!(cluster_note_ids.contains(&beta.id));
    assert!(cluster_note_ids.contains(&gamma.id));
    assert_eq!(
        cluster.note_ids,
        cluster
            .notes
            .iter()
            .map(|note| note.id.clone())
            .collect::<Vec<_>>()
    );
    assert!(!cluster.edges.is_empty());
    assert!(
        cluster
            .edges
            .windows(2)
            .all(|window| window[0].left_note_id <= window[1].left_note_id
                && (window[0].left_note_id < window[1].left_note_id
                    || window[0].right_note_id <= window[1].right_note_id))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consolidation_clusters_ignore_below_threshold_inputs() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let project = make_project(&db, tmp.path()).await;

    repo.create_db_note(
        &project.id,
        "Sparse note one",
        "alpha unique tokens only",
        "pattern",
        "[]",
    )
    .await
    .unwrap();
    repo.create_db_note(
        &project.id,
        "Sparse note two",
        "omega unrelated language only",
        "pattern",
        "[]",
    )
    .await
    .unwrap();
    repo.create_db_note(
        &project.id,
        "Sparse note three",
        "zeta distinct vocabulary only",
        "pattern",
        "[]",
    )
    .await
    .unwrap();

    let candidates = repo
        .dedup_candidates(
            &project.id,
            "patterns",
            "pattern",
            "alpha OR omega OR zeta",
            16,
        )
        .await
        .unwrap();
    assert!(candidates.len() <= 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consolidation_create_canonical_note_persists_db_note_confidence_and_provenance() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    let _source_note_a = note_repo
        .create_db_note(
            &project.id,
            "Source Pattern A",
            "source body a",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let _source_note_b = note_repo
        .create_db_note(
            &project.id,
            "Source Pattern B",
            "source body b",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let session_a = make_session(&db, &project.id, None, "worker/source-a").await;
    let session_b = make_session(&db, &project.id, None, "worker/source-b").await;

    let created = consolidation_repo
        .create_canonical_consolidated_note(CreateCanonicalConsolidatedNote {
            project_id: &project.id,
            note_type: "pattern",
            title: "Canonical Consolidated Pattern",
            content: "synthesized canonical content",
            tags: "[\"canonical\",\"consolidated\"]",
            abstract_: Some("short abstract"),
            overview: Some("overview summary"),
            confidence: 1.2,
            source_session_ids: &[&session_a, &session_b],
            scope_paths: "[]",
        })
        .await
        .unwrap();

    assert_eq!(created.note.storage, "db");
    assert_eq!(created.note.note_type, "pattern");
    assert_eq!(created.note.title, "Canonical Consolidated Pattern");
    assert_eq!(created.note.content, "synthesized canonical content");
    assert_eq!(created.note.abstract_.as_deref(), Some("short abstract"));
    assert_eq!(created.note.overview.as_deref(), Some("overview summary"));
    assert_eq!(created.note.confidence, CONFIDENCE_CEILING);
    assert_eq!(created.provenance.len(), 2);
    assert_eq!(created.provenance[0].session_id, session_a);
    assert_eq!(created.provenance[1].session_id, session_b);

    let fetched = note_repo.get(&created.note.id).await.unwrap().unwrap();
    assert_eq!(fetched.storage, "db");
    assert_eq!(fetched.confidence, CONFIDENCE_CEILING);
    assert_eq!(fetched.abstract_.as_deref(), Some("short abstract"));
    assert_eq!(fetched.overview.as_deref(), Some("overview summary"));

    let provenance = consolidation_repo
        .list_provenance(&created.note.id)
        .await
        .unwrap();
    assert_eq!(provenance.len(), 2);
    assert_eq!(provenance[0].session_id, session_a);
    assert_eq!(provenance[1].session_id, session_b);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consolidation_resolve_source_session_ids_returns_deduped_sorted_recursive_provenance()
{
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    let direct_source = note_repo
        .create_db_note(&project.id, "Direct source", "body", "pattern", "[]")
        .await
        .unwrap();
    let canonical_source = note_repo
        .create_db_note(&project.id, "Canonical source", "body", "pattern", "[]")
        .await
        .unwrap();

    let session_a = make_session(&db, &project.id, None, "worker/source-a").await;
    let session_b = make_session(&db, &project.id, None, "worker/source-b").await;
    let session_c = make_session(&db, &project.id, None, "worker/source-c").await;

    consolidation_repo
        .add_provenance(&direct_source.id, &session_b)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&canonical_source.id, &session_c)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&canonical_source.id, &session_a)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&canonical_source.id, &session_b)
        .await
        .unwrap();

    let resolved = consolidation_repo
        .resolve_source_session_ids(
            &project.id,
            &[
                canonical_source.id.clone(),
                direct_source.id.clone(),
                canonical_source.id.clone(),
            ],
        )
        .await
        .unwrap();

    assert_eq!(resolved, vec![session_a, session_b, session_c]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consolidation_resolve_source_session_ids_returns_empty_when_notes_have_no_provenance()
{
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    let source_a = note_repo
        .create_db_note(&project.id, "Source A", "body", "pattern", "[]")
        .await
        .unwrap();
    let source_b = note_repo
        .create_db_note(&project.id, "Source B", "body", "pattern", "[]")
        .await
        .unwrap();

    let resolved = consolidation_repo
        .resolve_source_session_ids(&project.id, &[source_b.id.clone(), source_a.id.clone()])
        .await
        .unwrap();

    assert!(resolved.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consolidation_resolve_source_session_ids_validates_project_scope() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let other_root = crate::database::test_tempdir().unwrap();
    let other_project = make_project(&db, other_root.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    let foreign_note = note_repo
        .create_db_note(&other_project.id, "Foreign source", "body", "pattern", "[]")
        .await
        .unwrap();

    let err = consolidation_repo
        .resolve_source_session_ids(&project.id, &[foreign_note.id])
        .await
        .unwrap_err();

    assert!(matches!(err, Error::InvalidData(_)));
    assert!(err.to_string().contains(&format!(
        "one or more source notes not found in project {}",
        project.id
    )));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consolidation_provenance_round_trips_in_stable_order() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let note_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    let note = note_repo
        .create_db_note(&project.id, "Consolidated Pattern", "body", "pattern", "[]")
        .await
        .unwrap();

    let earlier_session = make_session(&db, &project.id, None, "worker/earlier").await;
    let later_session = make_session(&db, &project.id, None, "worker/later").await;

    let first = consolidation_repo
        .add_provenance(&note.id, &earlier_session)
        .await
        .unwrap();
    let second = consolidation_repo
        .add_provenance(&note.id, &later_session)
        .await
        .unwrap();

    assert_eq!(first.note_id, note.id);
    assert_eq!(first.session_id, earlier_session);
    assert_eq!(second.session_id, later_session);

    let listed = consolidation_repo.list_provenance(&note.id).await.unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].session_id, earlier_session);
    assert_eq!(listed[1].session_id, later_session);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consolidation_run_metrics_round_trip_and_filter() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let project = make_project(&db, tmp.path()).await;
    let other_root = crate::database::test_tempdir().unwrap();
    let other_project = make_project(&db, other_root.path()).await;
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    let first = consolidation_repo
        .create_run_metric(CreateConsolidationRunMetric {
            project_id: &project.id,
            note_type: "pattern",
            status: "completed",
            scanned_note_count: 7,
            candidate_cluster_count: 2,
            consolidated_cluster_count: 1,
            consolidated_note_count: 1,
            source_note_count: 3,
            started_at: "2026-03-25T10:00:00.000Z",
            completed_at: Some("2026-03-25T10:01:00.000Z"),
            error_message: None,
        })
        .await
        .unwrap();

    let second = consolidation_repo
        .create_run_metric(CreateConsolidationRunMetric {
            project_id: &project.id,
            note_type: "pitfall",
            status: "failed",
            scanned_note_count: 4,
            candidate_cluster_count: 1,
            consolidated_cluster_count: 0,
            consolidated_note_count: 0,
            source_note_count: 0,
            started_at: "2026-03-25T11:00:00.000Z",
            completed_at: Some("2026-03-25T11:02:00.000Z"),
            error_message: Some("llm timeout"),
        })
        .await
        .unwrap();

    consolidation_repo
        .create_run_metric(CreateConsolidationRunMetric {
            project_id: &other_project.id,
            note_type: "pattern",
            status: "completed",
            scanned_note_count: 9,
            candidate_cluster_count: 3,
            consolidated_cluster_count: 1,
            consolidated_note_count: 1,
            source_note_count: 4,
            started_at: "2026-03-25T12:00:00.000Z",
            completed_at: Some("2026-03-25T12:03:00.000Z"),
            error_message: None,
        })
        .await
        .unwrap();

    let listed = consolidation_repo
        .list_run_metrics(&project.id, None, 10)
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, second.id);
    assert_eq!(listed[0].error_message.as_deref(), Some("llm timeout"));
    assert_eq!(listed[1].id, first.id);

    let filtered = consolidation_repo
        .list_run_metrics(&project.id, Some("pattern"), 10)
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].id, first.id);
    assert_eq!(filtered[0].consolidated_cluster_count, 1);
    assert_eq!(filtered[0].consolidated_note_count, 1);
    assert_eq!(filtered[0].source_note_count, 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn housekeeping_rebuild_missing_content_hashes_repairs_legacy_null_hashes_without_creating_duplicates()
 {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let canonical = repo
        .create_db_note(
            &project.id,
            "Canonical",
            "Alpha\r\nBeta\n",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let legacy_duplicate = repo
        .create_db_note(
            &project.id,
            "Legacy Duplicate",
            " Alpha\nBeta ",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let unaffected = repo
        .create_db_note(&project.id, "Unaffected", "Gamma", "reference", "[]")
        .await
        .unwrap();

    sqlx::query("UPDATE notes SET content_hash = NULL WHERE id IN (?1, ?2)")
        .bind(&canonical.id)
        .bind(&legacy_duplicate.id)
        .execute(db.pool())
        .await
        .unwrap();

    let note_count_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM notes WHERE project_id = ?1")
            .bind(&project.id)
            .fetch_one(db.pool())
            .await
            .unwrap();

    let rebuilt = repo
        .rebuild_missing_content_hashes(&project.id)
        .await
        .unwrap();
    assert_eq!(rebuilt, 2);

    let note_count_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM notes WHERE project_id = ?1")
            .bind(&project.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(note_count_after, note_count_before);

    let rebuilt_hashes: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, content_hash FROM notes WHERE id IN (?1, ?2) ORDER BY id")
            .bind(&canonical.id)
            .bind(&legacy_duplicate.id)
            .fetch_all(db.pool())
            .await
            .unwrap();
    let expected_hash = crate::note_hash::note_content_hash("Alpha\r\nBeta\n");
    assert_eq!(rebuilt_hashes.len(), 2);
    for (_id, content_hash) in rebuilt_hashes {
        assert_eq!(content_hash.as_deref(), Some(expected_hash.as_str()));
    }

    let unaffected_hash: Option<String> =
        sqlx::query_scalar("SELECT content_hash FROM notes WHERE id = ?1")
            .bind(&unaffected.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        unaffected_hash.as_deref(),
        Some(crate::note_hash::note_content_hash("Gamma").as_str())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn housekeeping_prune_associations_returns_stable_multi_project_counts() {
    let db = Database::open_in_memory().unwrap();
    let fixture = build_multi_project_housekeeping_fixture(&db).await;
    let (tx, _rx) = broadcast::channel(256);
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    for fixture_project in &fixture.projects {
        let deleted = repo
            .prune_associations(&fixture_project.project.id)
            .await
            .unwrap();
        assert_eq!(deleted, fixture_project.expected.prune_associations);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn housekeeping_flag_orphan_notes_tags_stale_unlinked_notes_only() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let orphan = repo
        .create(
            &project.id,
            tmp.path(),
            "Old orphan",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let linked = repo
        .create(
            &project.id,
            tmp.path(),
            "Linked target",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let source = repo
        .create(
            &project.id,
            tmp.path(),
            "Source",
            &format!("links to [[{}]]", linked.title),
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _ = source;

    sqlx::query(
        "UPDATE notes
         SET last_accessed = datetime('now', '-31 days'), access_count = 0
         WHERE id IN (?1, ?2)",
    )
    .bind(&orphan.id)
    .bind(&linked.id)
    .execute(db.pool())
    .await
    .unwrap();

    let flagged = repo
        .flag_orphan_notes(&project.id, tmp.path(), "orphan")
        .await
        .unwrap();
    assert_eq!(flagged, 1);

    let orphan_tags: String = sqlx::query_scalar("SELECT tags FROM notes WHERE id = ?1")
        .bind(&orphan.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let linked_tags: String = sqlx::query_scalar("SELECT tags FROM notes WHERE id = ?1")
        .bind(&linked.id)
        .fetch_one(db.pool())
        .await
        .unwrap();

    assert_eq!(orphan_tags, "[\"orphan\"]");
    assert_eq!(linked_tags, "[]");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn housekeeping_repair_broken_wikilinks_does_not_force_low_confidence_matches() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let _target = repo
        .create(
            &project.id,
            tmp.path(),
            "Rust Ownership Guide",
            "Rust ownership guide. Rust ownership guide. Rust ownership guide. Rust ownership guide. Borrowing and lifetimes details.",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let source = repo
        .create(
            &project.id,
            tmp.path(),
            "Broken link source",
            "Read [[Rust Ownership]] before editing.",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let repaired = repo
        .repair_broken_wikilinks(&project.id, tmp.path(), 0.0)
        .await
        .unwrap();
    assert_eq!(repaired, 1);

    let updated = repo.get(&source.id).await.unwrap().unwrap();
    assert!(updated.content.contains("[[Rust Ownership Guide]]"));
    assert!(!updated.content.contains("[[Rust Ownership]]"));

    let resolved_target: Option<String> = sqlx::query_scalar(
        "SELECT target_id FROM note_links WHERE source_id = ?1 AND target_raw = ?2",
    )
    .bind(&source.id)
    .bind("Rust Ownership Guide")
    .fetch_optional(db.pool())
    .await
    .unwrap();
    assert!(resolved_target.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn housekeeping_repair_broken_wikilinks_skips_ambiguous_matches() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let _ = repo
        .create(
            &project.id,
            tmp.path(),
            "Rust Ownership Guide",
            "guide for rust ownership",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _ = repo
        .create(
            &project.id,
            tmp.path(),
            "Rust Ownership Rules",
            "rules for rust ownership",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let source = repo
        .create(
            &project.id,
            tmp.path(),
            "Ambiguous link source",
            "Compare [[Rust Ownership]] options.",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let repaired = repo
        .repair_broken_wikilinks(&project.id, tmp.path(), 0.1)
        .await
        .unwrap();
    assert_eq!(repaired, 0);

    let updated = repo.get(&source.id).await.unwrap().unwrap();
    assert!(updated.content.contains("[[Rust Ownership]]"));
    assert!(!updated.content.contains("[[Rust Ownership Guide]]"));

    let best = repo
        .search(&project.id, "Rust Ownership", None, None, None, 3)
        .await
        .unwrap();
    assert!(best.len() >= 2);
    assert!((best[0].score - best[1].score).abs() < 5.0);
}

// ── Session-scoped consolidation ─────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_sessions_with_provenance_returns_distinct_session_ids() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    // No provenance yet
    let sessions = consolidation_repo
        .list_sessions_with_provenance()
        .await
        .unwrap();
    assert!(sessions.is_empty());

    let note_a = repo
        .create_db_note(&project.id, "Note A", "body a", "pattern", "[]")
        .await
        .unwrap();
    let note_b = repo
        .create_db_note(&project.id, "Note B", "body b", "pattern", "[]")
        .await
        .unwrap();
    let session_x = make_session(&db, &project.id, None, "worker/x").await;
    let session_y = make_session(&db, &project.id, None, "worker/y").await;

    consolidation_repo
        .add_provenance(&note_a.id, &session_x)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&note_b.id, &session_x)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&note_b.id, &session_y)
        .await
        .unwrap();

    let sessions = consolidation_repo
        .list_sessions_with_provenance()
        .await
        .unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(sessions.contains(&session_x));
    assert!(sessions.contains(&session_y));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_db_note_groups_for_session_scopes_to_session_provenance() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    let note_a = repo
        .create_db_note(&project.id, "Pattern A", "body a", "pattern", "[]")
        .await
        .unwrap();
    let note_b = repo
        .create_db_note(&project.id, "Pattern B", "body b", "pattern", "[]")
        .await
        .unwrap();
    let note_c = repo
        .create_db_note(&project.id, "Case C", "body c", "case", "[]")
        .await
        .unwrap();

    let session_1 = make_session(&db, &project.id, None, "worker/s1").await;
    let session_2 = make_session(&db, &project.id, None, "worker/s2").await;

    // Session 1 produced 2 patterns
    consolidation_repo
        .add_provenance(&note_a.id, &session_1)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&note_b.id, &session_1)
        .await
        .unwrap();

    // Session 2 produced only 1 case note (below minimum of 2 for grouping)
    consolidation_repo
        .add_provenance(&note_c.id, &session_2)
        .await
        .unwrap();

    let groups_s1 = consolidation_repo
        .list_db_note_groups_for_session(&session_1)
        .await
        .unwrap();
    assert_eq!(groups_s1.len(), 1);
    assert_eq!(groups_s1[0].project_id, project.id);
    assert_eq!(groups_s1[0].note_type, "pattern");
    assert_eq!(groups_s1[0].note_count, 2);

    // Session 2 has only 1 case note, below the 2-note threshold
    let groups_s2 = consolidation_repo
        .list_db_note_groups_for_session(&session_2)
        .await
        .unwrap();
    assert!(groups_s2.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_db_notes_in_group_for_session_returns_only_session_linked_notes() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    let note_a = repo
        .create_db_note(&project.id, "Pattern A", "body a", "pattern", "[]")
        .await
        .unwrap();
    let note_b = repo
        .create_db_note(&project.id, "Pattern B", "body b", "pattern", "[]")
        .await
        .unwrap();
    let note_c = repo
        .create_db_note(&project.id, "Pattern C", "body c", "pattern", "[]")
        .await
        .unwrap();

    let session_1 = make_session(&db, &project.id, None, "worker/s1").await;
    let session_2 = make_session(&db, &project.id, None, "worker/s2").await;

    // Session 1: note_a and note_b
    consolidation_repo
        .add_provenance(&note_a.id, &session_1)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&note_b.id, &session_1)
        .await
        .unwrap();

    // Session 2: note_c only
    consolidation_repo
        .add_provenance(&note_c.id, &session_2)
        .await
        .unwrap();

    let session_1_notes = consolidation_repo
        .list_db_notes_in_group_for_session(&project.id, "pattern", &session_1)
        .await
        .unwrap();
    let session_1_ids: std::collections::HashSet<_> =
        session_1_notes.iter().map(|n| n.id.clone()).collect();
    assert_eq!(session_1_ids.len(), 2);
    assert!(session_1_ids.contains(&note_a.id));
    assert!(session_1_ids.contains(&note_b.id));
    assert!(!session_1_ids.contains(&note_c.id));

    let session_2_notes = consolidation_repo
        .list_db_notes_in_group_for_session(&project.id, "pattern", &session_2)
        .await
        .unwrap();
    assert_eq!(session_2_notes.len(), 1);
    assert_eq!(session_2_notes[0].id, note_c.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_scoped_clusters_do_not_include_cross_session_notes() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    // Create 3 similar notes in session 1 (should cluster)
    let note_a = repo
        .create_db_note(
            &project.id,
            "Schema seam prerequisite check",
            "Verify the prerequisite seam exists before wiring the schema seam. prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let note_b = repo
        .create_db_note(
            &project.id,
            "Verify prerequisite seam before schema wiring",
            "Always verify the prerequisite seam exists before wiring the schema seam. prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let note_c = repo
        .create_db_note(
            &project.id,
            "Deterministic seam verification for schema query",
            "Use deterministic verification to confirm the prerequisite seam before schema query wiring. prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

    // Create a similar note in session 2 (should NOT be included in session 1 clusters)
    let note_d = repo
        .create_db_note(
            &project.id,
            "Schema prerequisite seam external origin",
            "External origin prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

    // Set abstract/overview for FTS matching
    for note in [&note_a, &note_b, &note_c, &note_d] {
        let abstract_text = format!(
            "{} prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            note.title
        );
        sqlx::query("UPDATE notes SET abstract = ?2, overview = ?3 WHERE id = ?1")
            .bind(&note.id)
            .bind(&abstract_text)
            .bind(&abstract_text)
            .execute(db.pool())
            .await
            .unwrap();
    }

    let session_1 = make_session(&db, &project.id, None, "worker/s1").await;
    let session_2 = make_session(&db, &project.id, None, "worker/s2").await;

    // Session 1 produced note_a, note_b, note_c
    consolidation_repo
        .add_provenance(&note_a.id, &session_1)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&note_b.id, &session_1)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&note_c.id, &session_1)
        .await
        .unwrap();

    // Session 2 produced only note_d
    consolidation_repo
        .add_provenance(&note_d.id, &session_2)
        .await
        .unwrap();

    // Session-scoped clusters for session 1 should contain note_a, note_b, note_c
    // but NOT note_d
    let clusters_s1 = consolidation_repo
        .likely_duplicate_clusters_for_session(&project.id, "pattern", &session_1)
        .await
        .unwrap();
    assert_eq!(clusters_s1.len(), 1, "session 1 should have 1 cluster");
    let cluster = &clusters_s1[0];
    let cluster_ids: std::collections::HashSet<_> = cluster.note_ids.iter().cloned().collect();
    assert!(cluster_ids.contains(&note_a.id));
    assert!(cluster_ids.contains(&note_b.id));
    assert!(cluster_ids.contains(&note_c.id));
    assert!(
        !cluster_ids.contains(&note_d.id),
        "cross-session note_d must not appear in session 1 cluster"
    );

    // Session 2 has only 1 note, so no clusters
    let clusters_s2 = consolidation_repo
        .likely_duplicate_clusters_for_session(&project.id, "pattern", &session_2)
        .await
        .unwrap();
    assert!(
        clusters_s2.is_empty(),
        "session 2 has only 1 note so should produce no clusters"
    );

    // Compare with the unscoped query which WOULD include note_d
    let clusters_all = consolidation_repo
        .likely_duplicate_clusters(&project.id, "pattern")
        .await
        .unwrap();
    assert_eq!(clusters_all.len(), 1);
    let all_ids: std::collections::HashSet<_> =
        clusters_all[0].note_ids.iter().cloned().collect();
    assert!(
        all_ids.contains(&note_d.id),
        "unscoped query should include the cross-session note"
    );
}
