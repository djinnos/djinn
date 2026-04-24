use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_with_scope_persists_scope_paths_on_db_row() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create_with_scope(
            &project.id,
            "ADR-100 Scoped Decision",
            "This is a scoped ADR body.",
            "adr",
            None,
            "[]",
            r#"["crates/foo"]"#,
        )
        .await
        .unwrap();

    assert_eq!(note.storage, "db");
    assert_eq!(note.file_path, "");
    assert_eq!(note.scope_paths, r#"["crates/foo"]"#);
    assert_eq!(note.note_type, "adr");
    assert_eq!(note.permalink, "decisions/adr-100-scoped-decision");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_with_scope_empty_array_works() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create_with_scope(
            &project.id,
            "ADR-101 Global Decision",
            "Global body.",
            "adr",
            None,
            "[]",
            "[]",
        )
        .await
        .unwrap();

    assert_eq!(note.storage, "db");
    assert_eq!(note.scope_paths, "[]");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn db_create_does_not_promote_to_file_storage_on_edit() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let created = repo
        .create_db_note_with_scope(
            &project.id,
            "ADR-200 Stored",
            "Original body.",
            "adr",
            "[]",
            r#"["crates/bar"]"#,
        )
        .await
        .unwrap();

    assert_eq!(created.storage, "db");
    assert_eq!(created.file_path, "");

    // Edits stay db-only — there is no on-disk file storage anymore.
    let edited = repo
        .update(&created.id, "ADR-200 Stored", "Healed body.", "[]")
        .await
        .unwrap();

    assert_eq!(edited.storage, "db");
    assert_eq!(edited.file_path, "");
    assert_eq!(edited.content, "Healed body.");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_db_note_with_scope_still_produces_db_row_for_consolidation() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create_db_note_with_scope(
            &project.id,
            "Consolidation Output",
            "Body.",
            "pattern",
            "[]",
            r#"["crates/baz"]"#,
        )
        .await
        .unwrap();

    assert_eq!(note.storage, "db");
    assert_eq!(note.file_path, "");
    assert_eq!(note.scope_paths, r#"["crates/baz"]"#);
}
