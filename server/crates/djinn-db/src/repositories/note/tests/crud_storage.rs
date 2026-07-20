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
    assert_eq!(note.lifecycle_changed_at, None);

    let fetched = repo.get(&note.id).await.unwrap().unwrap();
    assert_eq!(fetched.title, "My ADR");
    assert_eq!(fetched.status, djinn_memory::note_status::ACTIVE);
    assert_eq!(fetched.lifecycle_changed_at, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn note_status_archives_filters_and_restores_without_delete() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let active = repo
        .create(&project.id, "Active Note", "body", "reference", "[]")
        .await
        .unwrap();
    let archived = repo
        .create(
            &project.id,
            "Archived Note",
            "original body",
            "reference",
            r#"["original-tag"]"#,
        )
        .await
        .unwrap();

    assert_eq!(active.status, djinn_memory::note_status::ACTIVE);
    assert_eq!(archived.status, djinn_memory::note_status::ACTIVE);

    // Creating a note with an explicit non-default status must persist that
    // status (regression guard: the INSERT must bind the caller-supplied
    // status rather than always falling back to the column default).
    let created_archived = repo
        .create_with_status(
            &project.id,
            "Created Archived",
            "body",
            "reference",
            Some(djinn_memory::note_status::ARCHIVED),
            "[]",
        )
        .await
        .unwrap();
    assert_eq!(created_archived.status, djinn_memory::note_status::ARCHIVED);
    assert_eq!(created_archived.lifecycle_changed_at, None);

    // Seed a known legacy timestamp so the same-status call can prove exact
    // preservation without relying on clock resolution or a sleep.
    let seeded_active_timestamp = "2000-01-01T00:00:00.000Z";
    sqlx::query("UPDATE notes SET lifecycle_changed_at = $1 WHERE id = $2")
        .bind(seeded_active_timestamp)
        .bind(&archived.id)
        .execute(db.pool())
        .await
        .unwrap();

    let same_status = repo.update_status(&archived.id, " ACTIVE ").await.unwrap();
    assert_eq!(same_status.status, djinn_memory::note_status::ACTIVE);
    assert_eq!(
        same_status.lifecycle_changed_at.as_deref(),
        Some(seeded_active_timestamp)
    );

    let archived = repo
        .update_status(&archived.id, djinn_memory::note_status::ARCHIVED)
        .await
        .unwrap();
    assert_eq!(archived.status, djinn_memory::note_status::ARCHIVED);
    assert_ne!(
        archived.lifecycle_changed_at.as_deref(),
        Some(seeded_active_timestamp)
    );
    assert!(repo.get(&archived.id).await.unwrap().is_some());

    let default_list = repo.list(&project.id, None).await.unwrap();
    assert!(default_list.iter().any(|note| note.id == active.id));
    assert!(default_list.iter().all(|note| note.id != archived.id));

    let archived_list = repo
        .list_with_status(&project.id, None, Some(djinn_memory::note_status::ARCHIVED))
        .await
        .unwrap();
    assert_eq!(archived_list.len(), 2);
    assert!(archived_list.iter().any(|n| n.id == archived.id));
    assert!(archived_list.iter().any(|n| n.id == created_archived.id));

    let archived_timestamp = archived.lifecycle_changed_at.clone();
    let updated = repo
        .update(
            &archived.id,
            "Restored Note",
            "updated body",
            r#"["updated-tag"]"#,
        )
        .await
        .unwrap();
    assert_eq!(updated.lifecycle_changed_at, archived_timestamp);

    // Use another fixed predecessor timestamp to make the restoration
    // transition comparison deterministic even on coarse database clocks.
    let seeded_archived_timestamp = "2000-01-02T00:00:00.000Z";
    sqlx::query("UPDATE notes SET lifecycle_changed_at = $1 WHERE id = $2")
        .bind(seeded_archived_timestamp)
        .bind(&archived.id)
        .execute(db.pool())
        .await
        .unwrap();

    let restored = repo
        .update_status(&archived.id, djinn_memory::note_status::ACTIVE)
        .await
        .unwrap();
    assert_eq!(restored.status, djinn_memory::note_status::ACTIVE);
    assert_ne!(
        restored.lifecycle_changed_at.as_deref(),
        Some(seeded_archived_timestamp)
    );
    assert_eq!(restored.id, archived.id);
    assert_eq!(restored.permalink, archived.permalink);
    assert_eq!(restored.title, "Restored Note");
    assert_eq!(restored.content, "updated body");
    assert_eq!(restored.tags, r#"["updated-tag"]"#);
    let restored_list = repo.list(&project.id, None).await.unwrap();
    assert!(restored_list.iter().any(|note| note.id == archived.id));
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
        .create(
            &project.id,
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

    let pitfall_note = repo
        .create(
            &project.id,
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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_supports_entity_and_claim_note_types() {
    // diei (LLM enrichment) writes entity + claim rows via the existing
    // `NoteRepository` lifecycle; verify both kinds round-trip through
    // create / get / list with their `note_type` preserved and land in
    // distinct, identifiable subfolders so the Memory graph UI can style
    // them separately.
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    // Create an entity note (recurring system / concept surfaced by the
    // enrichment pass).
    let entity_note = repo
        .create(
            &project.id,
            "Dispatch Gate",
            "Recurring subsystem that gates dispatch.",
            "entity",
            r#"["enrichment","system"]"#,
        )
        .await
        .unwrap();
    assert_eq!(entity_note.note_type, "entity");
    assert_eq!(entity_note.folder, "reference/entities");
    assert_eq!(entity_note.permalink, "reference/entities/dispatch-gate");
    assert_eq!(entity_note.storage, "db");

    // Create a claim note (decision the memory records).
    let claim_note = repo
        .create(
            &project.id,
            "Use Circuit Breaker",
            "Always pair the dispatch gate with a circuit breaker.",
            "claim",
            r#"["enrichment","decision"]"#,
        )
        .await
        .unwrap();
    assert_eq!(claim_note.note_type, "claim");
    assert_eq!(claim_note.folder, "reference/claims");
    assert_eq!(claim_note.permalink, "reference/claims/use-circuit-breaker");
    assert_eq!(claim_note.storage, "db");

    // `get` round-trips the canonical `note_type` exactly.
    let fetched_entity = repo.get(&entity_note.id).await.unwrap().unwrap();
    assert_eq!(fetched_entity.note_type, "entity");
    assert_eq!(fetched_entity.folder, "reference/entities");

    let fetched_claim = repo.get(&claim_note.id).await.unwrap().unwrap();
    assert_eq!(fetched_claim.note_type, "claim");
    assert_eq!(fetched_claim.folder, "reference/claims");

    // `get_by_permalink` works for both (same lookup path the rest of the
    // knowledge base uses).
    let by_permalink_entity = repo
        .get_by_permalink(&project.id, &entity_note.permalink)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_permalink_entity.id, entity_note.id);
    assert_eq!(by_permalink_entity.note_type, "entity");

    let by_permalink_claim = repo
        .get_by_permalink(&project.id, &claim_note.permalink)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(by_permalink_claim.id, claim_note.id);
    assert_eq!(by_permalink_claim.note_type, "claim");

    // `list` scoped by folder returns enrichment rows alongside any other
    // notes in `reference/entities` / `reference/claims` so the Memory
    // browser surface can render them in their own sections.
    let entity_section = repo
        .list(&project.id, Some("reference/entities"))
        .await
        .unwrap();
    assert!(
        entity_section.iter().any(|n| n.id == entity_note.id),
        "entity note missing from `list` under reference/entities: {entity_section:?}"
    );
    let claim_section = repo
        .list(&project.id, Some("reference/claims"))
        .await
        .unwrap();
    assert!(
        claim_section.iter().any(|n| n.id == claim_note.id),
        "claim note missing from `list` under reference/claims: {claim_section:?}"
    );

    // Unscoped `list` also returns them — they're regular notes.
    let all_notes = repo.list(&project.id, None).await.unwrap();
    assert!(
        all_notes
            .iter()
            .any(|n| n.note_type == "entity" && n.id == entity_note.id)
    );
    assert!(
        all_notes
            .iter()
            .any(|n| n.note_type == "claim" && n.id == claim_note.id)
    );

    // `infer_note_type` round-trips the new permalinks back to the canonical
    // `note_type` strings — without this, `resolve` would mis-classify
    // enrichment rows on the read path.
    assert_eq!(
        file_helpers::infer_note_type(&entity_note.permalink),
        "entity"
    );
    assert_eq!(
        file_helpers::infer_note_type(&claim_note.permalink),
        "claim"
    );
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
    // JSONB normalizes array whitespace on round-trip; compare parsed values.
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&updated.tags).unwrap(),
        serde_json::from_str::<serde_json::Value>(r#"["repo-map","updated"]"#).unwrap(),
    );
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
        sqlx::query_scalar!(
            r#"SELECT COUNT(*) AS "count!: i64" FROM notes WHERE id = $1"#,
            created.id
        )
        .fetch_one(db.pool())
        .await
        .unwrap(),
        0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retrieval_anchor_persists_and_legacy_null_hydrates() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let legacy = repo
        .create(
            &project.id,
            "Legacy Anchor",
            "legacy body",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let legacy = repo.get(&legacy.id).await.unwrap().unwrap();
    assert_eq!(legacy.retrieval_anchor, None);
    assert_eq!(
        legacy.to_value()["retrieval_anchor"],
        serde_json::Value::Null
    );

    let anchored = repo
        .create_with_retrieval_anchor(
            &project.id,
            "Anchored Note",
            "anchored body",
            "pattern",
            r#"["anchor"]"#,
            Some("When a worker needs anchor persistence."),
        )
        .await
        .unwrap();
    assert_eq!(
        anchored.retrieval_anchor.as_deref(),
        Some("When a worker needs anchor persistence.")
    );
    assert_eq!(
        anchored.to_value()["retrieval_anchor"],
        serde_json::json!("When a worker needs anchor persistence.")
    );

    let changed_anchor = repo
        .update_retrieval_anchor(&anchored.id, Some("When updating an existing note anchor."))
        .await
        .unwrap();
    assert_eq!(
        changed_anchor.retrieval_anchor.as_deref(),
        Some("When updating an existing note anchor.")
    );

    let retagged = repo
        .update_tags(&changed_anchor.id, r#"["anchor","retagged"]"#)
        .await
        .unwrap();
    assert_eq!(
        retagged.retrieval_anchor.as_deref(),
        Some("When updating an existing note anchor.")
    );

    let scoped = repo
        .update_scope_paths(&changed_anchor.id, r#"["server/crates"]"#)
        .await
        .unwrap();
    assert_eq!(
        scoped.retrieval_anchor.as_deref(),
        Some("When updating an existing note anchor.")
    );
}
