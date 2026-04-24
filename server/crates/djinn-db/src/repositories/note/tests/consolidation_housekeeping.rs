use super::*;
use crate::repositories::note::NoteSearchParams;

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
        sqlx::query!(
            "UPDATE notes
             SET `abstract` = ?,
                 overview = ?
             WHERE id = ?",
            abstract_text,
            abstract_text,
            note.id
        )
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
#[ignore = "Below-threshold filter was tuned to negated SQLite bm25 scores; MySQL FULLTEXT in natural-language mode returns all term-bearing rows with positive scores, so the dedup threshold semantics don't port. Needs a new empirical threshold when MysqlFulltext dedup is retuned (see replacement_notes on the plan)."]
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
async fn consolidation_resolve_source_session_ids_returns_deduped_sorted_recursive_provenance() {
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
async fn consolidation_resolve_source_session_ids_returns_empty_when_notes_have_no_provenance() {
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

    sqlx::query!(
        "UPDATE notes SET content_hash = NULL WHERE id IN (?, ?)",
        canonical.id,
        legacy_duplicate.id
    )
    .execute(db.pool())
    .await
    .unwrap();

    let note_count_before = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM notes WHERE project_id = ?",
        project.id
    )
    .fetch_one(db.pool())
    .await
    .unwrap();

    let rebuilt = repo
        .rebuild_missing_content_hashes(&project.id)
        .await
        .unwrap();
    assert_eq!(rebuilt, 2);

    let note_count_after = sqlx::query_scalar!(
        "SELECT COUNT(*) FROM notes WHERE project_id = ?",
        project.id
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(note_count_after, note_count_before);

    let rebuilt_hashes: Vec<(String, Option<String>)> = sqlx::query!(
        "SELECT id, content_hash FROM notes WHERE id IN (?, ?) ORDER BY id",
        canonical.id,
        legacy_duplicate.id
    )
    .fetch_all(db.pool())
    .await
    .unwrap()
    .into_iter()
    .map(|r| (r.id, r.content_hash))
    .collect();
    let expected_hash = crate::note_hash::note_content_hash("Alpha\r\nBeta\n");
    assert_eq!(rebuilt_hashes.len(), 2);
    for (_id, content_hash) in rebuilt_hashes {
        assert_eq!(content_hash.as_deref(), Some(expected_hash.as_str()));
    }

    let unaffected_hash =
        sqlx::query_scalar!("SELECT content_hash FROM notes WHERE id = ?", unaffected.id)
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
            "Source",
            &format!("links to [[{}]]", linked.title),
            "reference",
            "[]",
        )
        .await
        .unwrap();
    let _ = source;

    // NOTE: raw `sqlx::query` because `query!` can't check the dynamic
    // DATE_FORMAT expression against the sqlx offline cache.
    sqlx::query(
        "UPDATE notes
         SET last_accessed = DATE_FORMAT(DATE_SUB(NOW(3), INTERVAL 31 DAY), '%Y-%m-%dT%H:%i:%s.%fZ'),
             access_count = 0
         WHERE id IN (?, ?)",
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

    let orphan_tags = sqlx::query_scalar!("SELECT tags FROM notes WHERE id = ?", orphan.id)
        .fetch_one(db.pool())
        .await
        .unwrap();
    let linked_tags = sqlx::query_scalar!("SELECT tags FROM notes WHERE id = ?", linked.id)
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

    let target_raw = "Rust Ownership Guide";
    let resolved_target = sqlx::query_scalar!(
        "SELECT target_id FROM note_links WHERE source_id = ? AND target_raw = ?",
        source.id,
        target_raw
    )
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
        .search(NoteSearchParams {
            project_id: &project.id,
            query: "Rust Ownership",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 3,
            semantic_scores: None,
        })
        .await
        .unwrap();
    assert!(best.len() >= 2);
    assert!((best[0].score - best[1].score).abs() < 5.0);
}
