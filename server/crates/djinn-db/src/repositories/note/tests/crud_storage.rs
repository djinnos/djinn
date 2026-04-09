use super::*;

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
async fn singleton_worktree_updates_keep_canonical_and_worktree_files_in_sync() {
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
            "Project Brief",
            "See [[Old Missing Link]].",
            "brief",
            "[]",
        )
        .await
        .unwrap();

    let canonical_path = project_tmp.path().join(".djinn/brief.md");
    let worktree_path = worktree_tmp.path().join(".djinn/brief.md");

    assert_eq!(
        std::path::Path::new(&note.file_path),
        canonical_path.as_path()
    );
    assert!(canonical_path.exists());
    assert!(worktree_path.exists());
    assert!(
        std::fs::read_to_string(&canonical_path)
            .unwrap()
            .contains("[[Old Missing Link]]")
    );

    let updated = repo
        .update(&note.id, &note.title, "See [[Resolved Link]].", &note.tags)
        .await
        .unwrap();

    assert_eq!(updated.content, "See [[Resolved Link]].");
    let canonical_file = std::fs::read_to_string(&canonical_path).unwrap();
    let worktree_file = std::fs::read_to_string(&worktree_path).unwrap();
    assert!(canonical_file.contains("[[Resolved Link]]"));
    assert!(worktree_file.contains("[[Resolved Link]]"));
    assert!(!canonical_file.contains("[[Old Missing Link]]"));

    repo.delete(&note.id).await.unwrap();
    assert!(!canonical_path.exists());
    assert!(!worktree_path.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_worktree_notes_to_canonical_promotes_worktree_only_note() {
    let project_tmp = crate::database::test_tempdir().unwrap();
    let worktree_tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, project_tmp.path()).await;

    let worktree_repo = NoteRepository::new(db.clone(), event_bus_for(&tx))
        .with_worktree_root(Some(worktree_tmp.path().to_path_buf()));
    let canonical_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let created = worktree_repo
        .create(
            &project.id,
            project_tmp.path(),
            "Persist Me",
            "survives close",
            "research",
            "[]",
        )
        .await
        .unwrap();

    let canonical_path = project_tmp.path().join(".djinn/research/persist-me.md");
    let worktree_path = worktree_tmp.path().join(".djinn/research/persist-me.md");
    assert!(!canonical_path.exists());
    assert!(worktree_path.exists());
    assert_eq!(
        canonical_repo
            .get_by_permalink(&project.id, &created.permalink)
            .await
            .unwrap()
            .unwrap()
            .content,
        "survives close"
    );

    let synced = worktree_repo
        .sync_worktree_notes_to_canonical(&project.id, project_tmp.path(), worktree_tmp.path())
        .await
        .unwrap();
    assert_eq!(synced, 1);
    assert!(canonical_path.exists());

    let canonical = canonical_repo
        .get_by_permalink(&project.id, "research/persist-me")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(canonical.content, "survives close");
    assert_eq!(Path::new(&canonical.file_path), canonical_path.as_path());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sync_worktree_proposed_adr_survives_close_and_lands_in_proposed_folder() {
    let project_tmp = crate::database::test_tempdir().unwrap();
    let worktree_tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, project_tmp.path()).await;

    let worktree_repo = NoteRepository::new(db.clone(), event_bus_for(&tx))
        .with_worktree_root(Some(worktree_tmp.path().to_path_buf()));
    let canonical_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let created = worktree_repo
        .create(
            &project.id,
            project_tmp.path(),
            "Pipeline Draft",
            "# Draft survives close\n",
            "adr",
            "[]",
        )
        .await
        .unwrap();

    let moved = worktree_repo
        .move_note(&created.id, project_tmp.path(), "Pipeline Draft", "proposed_adr")
        .await
        .unwrap();

    let worktree_path = worktree_tmp
        .path()
        .join(".djinn/decisions/proposed/pipeline-draft.md");
    assert_eq!(moved.note_type, "proposed_adr");
    assert_eq!(moved.folder, "decisions/proposed");
    assert_eq!(moved.permalink, "decisions/proposed/pipeline-draft");
    assert!(worktree_path.exists());

    let synced = worktree_repo
        .sync_worktree_notes_to_canonical(&project.id, project_tmp.path(), worktree_tmp.path())
        .await
        .unwrap();
    assert_eq!(synced, 1);

    let canonical = canonical_repo
        .get_by_permalink(&project.id, "decisions/proposed/pipeline-draft")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(canonical.note_type, "proposed_adr");
    assert_eq!(canonical.folder, "decisions/proposed");
    assert!(canonical.file_path.ends_with(".djinn/decisions/proposed/pipeline-draft.md"));
    assert!(Path::new(&canonical.file_path).exists());
}
