use super::*;
use crate::repositories::note::NoteSearchParams;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn semantic_candidate_branch_resolution_tracks_task_and_canonical_metadata() {
    let _guard = super::sqlite_vec_test_lock().lock().await;
    crate::database::set_sqlite_vec_disabled_for_tests(false);

    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let epic_id = make_epic(&db, &project.id).await;
    let task = TaskRepository::new(db.clone(), EventBus::noop())
        .create_with_ac(
            &epic_id,
            "Branch-aware semantic retrieval",
            "exercise branch-aware embeddings",
            "design",
            "task",
            1,
            "worker",
            None,
            Some(r#"[{"title":"semantic"}]"#),
        )
        .await
        .unwrap();

    let canonical = repo
        .create_db_note(&project.id, "Canonical Semantic", "body", "reference", "[]")
        .await
        .unwrap();
    let branch_local = repo
        .create_db_note(&project.id, "Task Semantic", "body", "reference", "[]")
        .await
        .unwrap();
    let unrelated = repo
        .create_db_note(
            &project.id,
            "Unrelated Task Semantic",
            "body",
            "reference",
            "[]",
        )
        .await
        .unwrap();

    let embedding = vec![0.33_f32; 768];
    repo.upsert_embedding(UpsertNoteEmbedding {
        note_id: &canonical.id,
        content_hash: "canonical-hash",
        model_version: "model-v1",
        embedding: &embedding,
        branch: "main",
    })
    .await
    .unwrap();
    repo.upsert_embedding(UpsertNoteEmbedding {
        note_id: &branch_local.id,
        content_hash: "branch-hash",
        model_version: "model-v1",
        embedding: &embedding,
        branch: &task_branch_name(&task.short_id),
    })
    .await
    .unwrap();
    repo.upsert_embedding(UpsertNoteEmbedding {
        note_id: &unrelated.id,
        content_hash: "unrelated-hash",
        model_version: "model-v1",
        embedding: &embedding,
        branch: "task/other",
    })
    .await
    .unwrap();

    let branch_name = task_branch_name(&task.short_id);
    assert_eq!(
        repo.embedding_branch_for_note(&canonical.id)
            .await
            .unwrap()
            .as_deref(),
        Some("main")
    );
    assert_eq!(
        repo.embedding_branch_for_note(&branch_local.id)
            .await
            .unwrap()
            .as_deref(),
        Some(branch_name.as_str())
    );

    let scores = repo
        .semantic_candidate_scores(&project.id, &embedding, Some(&task.id), None, None, 10)
        .await
        .unwrap();
    if !repo.db.sqlite_vec_status().await.unwrap().available {
        assert!(scores.is_empty());
    } else {
        assert!(
            scores.iter().all(|(id, _)| id != &unrelated.id),
            "semantic retrieval should exclude unrelated task-branch embeddings"
        );
    }
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
        .search(NoteSearchParams {
            project_id: &project.id,
            query: "rusqlite",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
        })
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
        .search(NoteSearchParams {
            project_id: &project.id,
            query: "common",
            task_id: None,
            folder: Some("design"),
            note_type: None,
            limit: 10,
            semantic_scores: None,
        })
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
        .search(NoteSearchParams {
            project_id: &project.id,
            query: "rankneedle",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
        })
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
        .search(NoteSearchParams {
            project_id: &project.id,
            query: "ranktag",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
        })
        .await
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "tag-ranked note");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resolve_prefers_exact_permalink_before_title_search_fallback() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db, event_bus_for(&tx));

    let design = repo
        .create_db_note_with_permalink(
            &project.id,
            "design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy",
            "ADR-054 Roadmap Memory Extraction Quality Gates and Note Taxonomy",
            "Canonical design note wins exact permalink resolution.",
            "design",
            "[]",
        )
        .await
        .unwrap();

    repo.create(
        &project.id,
        tmp.path(),
        "ADR-054 Roadmap Memory Extraction Quality Gates and Note Taxonomy",
        "Archived case note that would otherwise rank via title/content fallback.",
        "case",
        "[]",
    )
    .await
    .unwrap();

    let resolved = repo
        .resolve(
            &project.id,
            "memory://design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy.md",
        )
        .await
        .unwrap()
        .expect("exact permalink should resolve");

    assert_eq!(resolved.id, design.id);
    assert_eq!(
        resolved.permalink,
        "design/adr-054-roadmap-memory-extraction-quality-gates-and-note-taxonomy"
    );
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

    sqlx::query!("UPDATE notes SET access_count = 10 WHERE id = ?", high.id)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query!("UPDATE notes SET access_count = 0 WHERE id = ?", low.id)
        .execute(db.pool())
        .await
        .unwrap();

    let results = repo
        .search(NoteSearchParams {
            project_id: &project.id,
            query: "sharedterm",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
        })
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

    sqlx::query!("UPDATE notes SET confidence = 0.5 WHERE id = ?", note.id)
        .execute(db.pool())
        .await
        .unwrap();

    let updated = repo
        .update_confidence(&note.id, scoring::TASK_SUCCESS)
        .await
        .unwrap();
    assert!(updated > 0.5);

    let stored = sqlx::query_scalar!("SELECT confidence FROM notes WHERE id = ?", note.id)
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

    sqlx::query!(
        "UPDATE notes SET access_count = 0, confidence = 1.0 WHERE id = ?",
        high.id
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE notes SET access_count = 0, confidence = 0.5 WHERE id = ?",
        low.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    let results = repo
        .search(NoteSearchParams {
            project_id: &project.id,
            query: "sharedconfidence",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
        })
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
