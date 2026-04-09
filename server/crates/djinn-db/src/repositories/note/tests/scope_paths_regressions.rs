use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_with_scope_writes_file_to_disk_for_adr() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create_with_scope(
            &project.id,
            tmp.path(),
            "ADR-100 Scoped Decision",
            "This is a scoped ADR body.",
            "adr",
            "[]",
            r#"["crates/foo"]"#,
        )
        .await
        .unwrap();

    assert_eq!(note.storage, "file");
    assert!(
        !note.file_path.is_empty(),
        "file_path must not be empty for scope_paths-backed ADR"
    );
    assert!(
        Path::new(&note.file_path).exists(),
        "markdown file must exist on disk"
    );
    assert_eq!(note.scope_paths, r#"["crates/foo"]"#);

    let on_disk = std::fs::read_to_string(&note.file_path).unwrap();
    assert!(on_disk.contains("ADR-100 Scoped Decision"));
    assert!(on_disk.contains("This is a scoped ADR body."));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_with_scope_empty_array_also_writes_file_to_disk() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let note = repo
        .create_with_scope(
            &project.id,
            tmp.path(),
            "ADR-101 Global Decision",
            "Global body.",
            "adr",
            "[]",
            "[]",
        )
        .await
        .unwrap();

    assert_eq!(note.storage, "file");
    assert!(!note.file_path.is_empty());
    assert!(Path::new(&note.file_path).exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heal_on_edit_upgrades_db_only_adr_to_file_storage() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    // Simulate a pre-fix broken row: storage="db", empty file_path,
    // file-backed note type (adr).
    let broken = repo
        .create_db_note_with_scope(
            &project.id,
            "ADR-200 Broken",
            "Original body.",
            "adr",
            "[]",
            r#"["crates/bar"]"#,
        )
        .await
        .unwrap();

    assert_eq!(broken.storage, "db");
    assert_eq!(broken.file_path, "");

    // Next edit should heal the row: storage becomes "file", file_path
    // is populated, and the markdown file lands on disk with the new content.
    let healed = repo
        .update(&broken.id, "ADR-200 Broken", "Healed body.", "[]")
        .await
        .unwrap();

    assert_eq!(healed.storage, "file");
    assert!(
        !healed.file_path.is_empty(),
        "healed row must have file_path"
    );
    assert!(
        Path::new(&healed.file_path).exists(),
        "healed row must have file on disk"
    );
    let on_disk = std::fs::read_to_string(&healed.file_path).unwrap();
    assert!(on_disk.contains("Healed body."));
    assert!(!on_disk.contains("Original body."));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn heal_on_edit_does_not_upgrade_consolidation_db_pattern() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    // Consolidation produces db-only rows of type "pattern"/"case"/"pitfall".
    let consolidated = repo
        .create_db_note_with_scope(
            &project.id,
            "Consolidated Pattern",
            "Body.",
            "pattern",
            "[]",
            "[]",
        )
        .await
        .unwrap();
    assert_eq!(consolidated.storage, "db");

    let after = repo
        .update(&consolidated.id, "Consolidated Pattern", "New body.", "[]")
        .await
        .unwrap();

    // Pattern must stay db-only — consolidation owns this row type.
    assert_eq!(after.storage, "db");
    assert_eq!(after.file_path, "");
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
