use super::*;
use crate::repositories::note::NoteSearchParams;
use djinn_memory::GraphOptions;

/// Pin every non-lexical RRF signal (temporal recency, access_count,
/// confidence) to a fixed value across all notes in a project so a
/// lexical-ranking assertion is not perturbed by creation-order timestamp
/// differences. Used by the title/tags-over-content ranking tests.
async fn equalize_non_lexical_signals(db: &Database, project_id: &str) {
    sqlx::query!(
        "UPDATE notes
         SET created_at = '2026-01-01T00:00:00.000Z',
             updated_at = '2026-01-01T00:00:00.000Z',
             last_accessed = '2026-01-01T00:00:00.000Z',
             access_count = 0,
             confidence = 1.0
         WHERE project_id = $1",
        project_id
    )
    .execute(db.pool())
    .await
    .unwrap();
}

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
        .create_fixture_with_ac(
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
        "Rust Database Choice",
        "We chose rusqlite for its simplicity and bundled SQLite.",
        "adr",
        "[]",
    )
    .await
    .unwrap();
    repo.create(
        &project.id,
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
            edge_kinds: None,
            entity_types: None,
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

    repo.create(&project.id, "Design Note", "common term", "design", "[]")
        .await
        .unwrap();
    repo.create(
        &project.id,
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
            edge_kinds: None,
            entity_types: None,
        })
        .await
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].folder, "design");
}

// Postgres tsvector ranking: the generated `notes.search_vector` weights
// title=A above content=C (migration 29), so `ts_rank` scores a title match
// above a content match. We equalize every *non-lexical* RRF signal (temporal
// recency, access_count, confidence) across the two notes — exactly like the
// `search_rrf_*` tests below isolate their own signal — so the lexical
// title-over-content weighting is the sole differentiator the fused ranking
// can act on. (Without this, the temporal-recency boost on the
// later-created note ties the RRF score and the assertion races on float
// noise.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fts_search_prefers_title_over_content() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    repo.create(
        &project.id,
        "rankneedle in title",
        "unrelated body",
        "research",
        "[]",
    )
    .await
    .unwrap();
    repo.create(
        &project.id,
        "different title",
        "This content has rankneedle.",
        "research",
        "[]",
    )
    .await
    .unwrap();

    equalize_non_lexical_signals(&db, &project.id).await;

    let results = repo
        .search(NoteSearchParams {
            project_id: &project.id,
            query: "rankneedle",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: None,
        })
        .await
        .unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "rankneedle in title");
}

// Postgres tsvector ranking: migration 29 weights tags=B above content=C, so
// a note carrying the query term in its *tag* outranks one that only mentions
// it in body prose. As in `fts_search_prefers_title_over_content`, we
// equalize the non-lexical RRF signals so the tag-over-content weighting is
// the deciding factor.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fts_search_prefers_tags_over_content() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    repo.create(
        &project.id,
        "tag-ranked note",
        "unrelated body",
        "research",
        r#"["ranktag"]"#,
    )
    .await
    .unwrap();
    repo.create(
        &project.id,
        "content-ranked note",
        "This content has ranktag.",
        "research",
        "[]",
    )
    .await
    .unwrap();

    equalize_non_lexical_signals(&db, &project.id).await;

    let results = repo
        .search(NoteSearchParams {
            project_id: &project.id,
            query: "ranktag",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: None,
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
        "Repository Dedup Research",
        "shared dedup token appears here",
        "research",
        "[]",
    )
    .await
    .unwrap();

    repo.create(
        &project.id,
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
    assert_eq!(results[0].content, "shared dedup token appears here");
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
            "sharedterm beta",
            "same content",
            "research",
            "[]",
        )
        .await
        .unwrap();

    sqlx::query!("UPDATE notes SET access_count = 10 WHERE id = $1", high.id)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query!("UPDATE notes SET access_count = 0 WHERE id = $1", low.id)
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
            edge_kinds: None,
            entity_types: None,
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
        .create(&project.id, "Confidence Note", "body", "research", "[]")
        .await
        .unwrap();

    sqlx::query!("UPDATE notes SET confidence = 0.5 WHERE id = $1", note.id)
        .execute(db.pool())
        .await
        .unwrap();

    // A bare medium-positive signal literal. `scoring::TASK_SUCCESS` was
    // deleted by 9xih along with its only production writer; this test is about
    // `update_confidence` persisting a posterior, not about task outcomes.
    let updated = repo.update_confidence(&note.id, 0.65).await.unwrap();
    assert!(updated > 0.5);

    let stored = sqlx::query_scalar!("SELECT confidence FROM notes WHERE id = $1", note.id)
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
            "sharedconfidence beta",
            "same content",
            "research",
            "[]",
        )
        .await
        .unwrap();

    sqlx::query!(
        "UPDATE notes SET access_count = 0, confidence = 1.0 WHERE id = $1",
        high.id
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query!(
        "UPDATE notes SET access_count = 0, confidence = 0.5 WHERE id = $1",
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
            edge_kinds: None,
            entity_types: None,
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

    repo.create(&project.id, "ADR One", "body", "adr", "[]")
        .await
        .unwrap();
    repo.create(&project.id, "Research One", "body", "research", "[]")
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

    repo.create(&project.id, "Event Note", "body", "design", "[]")
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
        .create(&project.id, "Touch Me", "body", "reference", "[]")
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap(); // NoteCreated

    repo.update_summaries(&note.id, Some("short"), Some("longer summary"))
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap(); // NoteUpdated

    repo.touch_accessed(
        &note.id,
        crate::repositories::note::NoteAccessSource::MemoryRead,
        &crate::repositories::note::NoteAccessAttribution::unattributed(),
    )
    .await
    .unwrap();

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
        .create(&project.id, "Touch Count", "body", "reference", "[]")
        .await
        .unwrap();

    for _ in 0..3 {
        repo.touch_accessed(
            &note.id,
            crate::repositories::note::NoteAccessSource::MemoryRead,
            &crate::repositories::note::NoteAccessAttribution::unattributed(),
        )
        .await
        .unwrap();
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
        .create(&project.id, "Needs Summary", "body", "reference", "[]")
        .await
        .unwrap();
    let _ = rx.recv().await.unwrap(); // NoteCreated

    repo.touch_accessed(
        &note.id,
        crate::repositories::note::NoteAccessSource::MemoryRead,
        &crate::repositories::note::NoteAccessAttribution::unattributed(),
    )
    .await
    .unwrap();

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
        .create(&project.id, "Summarize Me", "body", "reference", "[]")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lifecycle_graph_read_does_not_leak_into_unified_lexical_search() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let active = repo
        .create(
            &project.id,
            "Active lifecycle isolation marker",
            "lifecycle isolation marker",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let archived = repo
        .create(
            &project.id,
            "Archived lifecycle isolation marker",
            "lifecycle isolation marker",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let deprecated = repo
        .create(
            &project.id,
            "Deprecated lifecycle isolation marker",
            "lifecycle isolation marker",
            "reference",
            "[]",
        )
        .await
        .unwrap();
    for (id, status) in [(&archived.id, "archived"), (&deprecated.id, "deprecated")] {
        sqlx::query("UPDATE notes SET status = $1 WHERE id = $2")
            .bind(status)
            .bind(id)
            .execute(db.pool())
            .await
            .unwrap();
    }
    // This is the explicit lifecycle visualization request, distinct from the
    // active-only `graph()` compatibility path.
    let lifecycle_graph = repo
        .graph_with_options(
            &project.id,
            GraphOptions {
                statuses: vec!["active".into(), "archived".into(), "deprecated".into()],
                lifecycle_limit: 500,
                include_lifecycle_summary: true,
            },
        )
        .await
        .unwrap();
    let graph_ids: std::collections::HashSet<_> = lifecycle_graph
        .nodes
        .iter()
        .map(|node| node.id.as_str())
        .collect();
    assert!(graph_ids.contains(archived.id.as_str()));
    assert!(graph_ids.contains(deprecated.id.as_str()));
    // `search` returns the unified note/proposal rows consumed by normal
    // memory_search, rather than a graph-specific result shape.
    let unified_results = repo
        .search(NoteSearchParams {
            project_id: &project.id,
            query: "lifecycle isolation marker",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 10,
            semantic_scores: None,
            edge_kinds: None,
            entity_types: None,
        })
        .await
        .unwrap();
    assert!(unified_results.iter().all(|result| result.entity == "note"));
    let result_ids: std::collections::HashSet<_> = unified_results
        .iter()
        .map(|result| result.id.as_str())
        .collect();
    assert!(result_ids.contains(active.id.as_str()));
    assert!(!result_ids.contains(archived.id.as_str()));
    assert!(!result_ids.contains(deprecated.id.as_str()));
}

// ── 9xih: invocation-keyed explicit access accounting ────────────────────────

use crate::repositories::note::{
    ExplicitAccessOutcome, NoteAccessAttribution, access_event_timestamp,
    note_access_events_for_note,
};

/// AC5: replay idempotency at the repository boundary.
///
/// The same `(invocation_id, note_id)` is recorded twice. The second call must
/// report `Replay` AND leave the ledger, the counter, and the timestamp exactly
/// as the first call left them — all three read back from Postgres.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_explicit_access_replay_writes_no_second_event_or_increment() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note = repo
        .create(&project.id, "Replay Note", "body", "reference", "[]")
        .await
        .unwrap();
    let before = repo.get(&note.id).await.unwrap().unwrap();

    let first_stamp = access_event_timestamp();
    let first = repo
        .record_explicit_access(
            &note.id,
            "invocation-alpha",
            &first_stamp,
            &NoteAccessAttribution::unattributed(),
        )
        .await
        .unwrap();
    assert_eq!(first, ExplicitAccessOutcome::Counted);

    let after_first = repo.get(&note.id).await.unwrap().unwrap();
    assert_eq!(after_first.access_count, before.access_count + 1);
    assert_eq!(after_first.last_accessed, first_stamp);

    // A retry of the SAME logical invocation, carrying a strictly later
    // timestamp so a naive implementation that always writes would be caught by
    // the `last_accessed` assertion below rather than only by the counter.
    let replay_stamp = "2099-12-31T23:59:59.999Z";
    let replay = repo
        .record_explicit_access(
            &note.id,
            "invocation-alpha",
            replay_stamp,
            &NoteAccessAttribution::unattributed(),
        )
        .await
        .unwrap();
    assert_eq!(replay, ExplicitAccessOutcome::Replay);

    let after_replay = repo.get(&note.id).await.unwrap().unwrap();
    assert_eq!(
        after_replay.access_count, after_first.access_count,
        "a replay must not increment access_count"
    );
    assert_eq!(
        after_replay.last_accessed, first_stamp,
        "a replay must not advance last_accessed even with a later timestamp"
    );

    let events = note_access_events_for_note(&db, &project.id, &note.id)
        .await
        .unwrap();
    assert_eq!(events.len(), 1, "a replay must not append a ledger row");
    assert_eq!(events[0].invocation_id.as_deref(), Some("invocation-alpha"));
    assert_eq!(events[0].source, "memory_read");
    assert_eq!(events[0].created_at, first_stamp);
}

/// AC5: `last_accessed = max(last_accessed, event_timestamp)`.
///
/// A distinct invocation whose event timestamp is OLDER than the stored value
/// still counts (it is a real, separate access) but must not rewind the
/// timestamp. This is what distinguishes `GREATEST` from plain assignment; a
/// `SET last_accessed = $ts` implementation fails here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn record_explicit_access_never_rewinds_last_accessed() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note = repo
        .create(&project.id, "Ordering Note", "body", "reference", "[]")
        .await
        .unwrap();

    let newer = "2030-06-06T06:06:06.000Z";
    let older = "2020-01-01T00:00:00.000Z";

    repo.record_explicit_access(
        &note.id,
        "invocation-newer",
        newer,
        &NoteAccessAttribution::unattributed(),
    )
    .await
    .unwrap();
    repo.record_explicit_access(
        &note.id,
        "invocation-older",
        older,
        &NoteAccessAttribution::unattributed(),
    )
    .await
    .unwrap();

    let stored = repo.get(&note.id).await.unwrap().unwrap();
    assert_eq!(
        stored.access_count, 2,
        "both distinct invocations must count, regardless of timestamp order"
    );
    assert_eq!(
        stored.last_accessed, newer,
        "a late-committing older event must not rewind last_accessed"
    );

    let events = note_access_events_for_note(&db, &project.id, &note.id)
        .await
        .unwrap();
    assert_eq!(events.len(), 2);
}

/// AC5: concurrent distinct invocations lose no increments.
///
/// Twelve distinct invocation ids are recorded from twelve concurrently spawned
/// tasks against one shared pool. The final `access_count` must be exactly 12.
///
/// This is the assertion an application-side read-modify-write cannot satisfy:
/// a `SELECT access_count` / `UPDATE ... SET access_count = n + 1` pair would
/// interleave and lose updates, landing below 12. The increment is written as
/// `access_count = access_count + 1` inside Postgres precisely so it cannot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_distinct_invocations_lose_no_access_increments() {
    const CONCURRENT_READS: usize = 12;

    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let note = repo
        .create(&project.id, "Concurrent Note", "body", "reference", "[]")
        .await
        .unwrap();

    // Deterministic, strictly increasing stamps so the expected maximum is
    // known without depending on scheduling order.
    let stamps: Vec<String> = (0..CONCURRENT_READS)
        .map(|index| format!("2031-01-01T00:00:{index:02}.000Z"))
        .collect();
    let latest = stamps.last().cloned().unwrap();

    let mut handles = Vec::new();
    for (index, stamp) in stamps.into_iter().enumerate() {
        let task_repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
        let note_id = note.id.clone();
        handles.push(tokio::spawn(async move {
            task_repo
                .record_explicit_access(
                    &note_id,
                    &format!("concurrent-invocation-{index}"),
                    &stamp,
                    &NoteAccessAttribution::unattributed(),
                )
                .await
        }));
    }

    let mut counted = 0usize;
    for handle in handles {
        let outcome = handle.await.expect("join").expect("record explicit access");
        if outcome == ExplicitAccessOutcome::Counted {
            counted += 1;
        }
    }
    assert_eq!(
        counted, CONCURRENT_READS,
        "distinct invocation ids must all be counted, none deduplicated"
    );

    let stored = repo.get(&note.id).await.unwrap().unwrap();
    assert_eq!(
        stored.access_count, CONCURRENT_READS as i64,
        "the final counter must rise by the number of committed events; \
         a lower value means an increment was lost to a read-modify-write race"
    );
    assert_eq!(
        stored.last_accessed, latest,
        "last_accessed must equal the latest committed event timestamp"
    );

    let events = note_access_events_for_note(&db, &project.id, &note.id)
        .await
        .unwrap();
    assert_eq!(events.len(), CONCURRENT_READS);
}
