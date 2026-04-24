use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_and_get_note() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create(&project.id, "My ADR", "This is the content.", "adr", "[]")
        .await
        .unwrap();

    assert_eq!(note.title, "My ADR");
    assert_eq!(note.note_type, "adr");
    assert_eq!(note.storage, "db");
    assert_eq!(note.folder, "decisions");
    assert_eq!(note.permalink, "decisions/my-adr");
    // Notes are now stored db-only; `file_path` is the empty-string vestige.
    assert_eq!(note.file_path, "");

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
        .create(&project.id, "Project Brief", "...", "brief", "[]")
        .await
        .unwrap();

    assert_eq!(note.permalink, "brief");
    assert_eq!(note.note_type, "brief");
    assert_eq!(note.file_path, "");
    let _ = tmp;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn get_by_permalink() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create(&project.id, "A Pattern", "body", "pattern", "[]")
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
    assert_eq!(folder_for_type("proposed_adr"), "decisions/proposed");
    assert_eq!(folder_for_type("pattern"), "patterns");
    assert_eq!(folder_for_type("case"), "cases");
    assert_eq!(folder_for_type("pitfall"), "pitfalls");
    assert_eq!(folder_for_type("repo_map"), "reference/repo-maps");

    assert_eq!(
        permalink_for("proposed_adr", "Proposal Draft"),
        "decisions/proposed/proposal-draft"
    );
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
        file_helpers::infer_note_type("decisions/proposed/proposal-draft"),
        "proposed_adr"
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
        .create(&project.id, "Incident Recovery", "Case details", "case", "[]")
        .await
        .unwrap();
    assert_eq!(case_note.note_type, "case");
    assert_eq!(case_note.folder, "cases");
    assert_eq!(case_note.permalink, "cases/incident-recovery");

    let pitfall_note = repo
        .create(&project.id, "Retry Storm", "Pitfall details", "pitfall", "[]")
        .await
        .unwrap();
    assert_eq!(pitfall_note.note_type, "pitfall");
    assert_eq!(pitfall_note.folder, "pitfalls");
    assert_eq!(pitfall_note.permalink, "pitfalls/retry-storm");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_backed_notes_round_trip_storage() {
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
        .create(&project.id, "Original", "old content", "research", "[]")
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
async fn update_db_backed_note() {
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
        .create(&project.id, "To Delete", "body", "reference", "[]")
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap();

    repo.delete(&note.id).await.unwrap();
    assert!(repo.get(&note.id).await.unwrap().is_none());

    let envelope = rx.recv().await.unwrap();
    assert_eq!(envelope.entity_type, "note");
    assert_eq!(envelope.action, "deleted");
    assert_eq!(envelope.payload["id"].as_str().unwrap(), note.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_create_and_delete_persists_state() {
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

    let persisted_created = note_select_where_id!(&created.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(persisted_created.content, "db body");
    assert_eq!(persisted_created.tags, r#"["tagged"]"#);

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

    repo.delete(&created.id).await.unwrap();
    assert!(repo.get(&created.id).await.unwrap().is_none());
    assert_eq!(
        sqlx::query_scalar!("SELECT COUNT(*) FROM notes WHERE id = ?", created.id)
            .fetch_one(db.pool())
            .await
            .unwrap(),
        0
    );
}
