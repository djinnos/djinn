use super::*;

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
        .create(&project.id, "Connection Strategy", "body", "adr", "[]")
        .await
        .unwrap();

    // Create source with a wikilink to the target by title.
    repo.create(
        &project.id,
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
        .create(&project.id, "Target", "body", "adr", "[]")
        .await
        .unwrap();
    repo.create(&project.id, "Source", "See [[Target]].", "research", "[]")
        .await
        .unwrap();
    repo.create(&project.id, "Isolated", "no links", "pattern", "[]")
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

    repo.create(&project.id, "Project Brief", "brief body", "brief", "[]")
        .await
        .unwrap();
    repo.create(
        &project.id,
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
    repo.create(&project.id, "Reachable Target", "body", "adr", "[]")
        .await
        .unwrap();
    repo.create(
        &project.id,
        "Linked Source",
        "See [[Reachable Target]].",
        "research",
        "[]",
    )
    .await
    .unwrap();
    repo.create(
        &project.id,
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
    assert_eq!(health.stale_note_count, 0);
    assert_eq!(health.low_confidence_note_count, 0);
    assert_eq!(health.lifecycle.active_notes, 6);
    assert_eq!(health.lifecycle.archived_notes, 0);
    assert_eq!(health.lifecycle.deprecated_notes, 0);
    assert_eq!(health.recent_sweep.last_decayed_count, 0);
    assert_eq!(health.recent_sweep.last_archived_count, 0);
    assert_eq!(health.recent_sweep.last_superseded_source_count, 0);
    assert!(health.recent_sweep.last_sweep_at.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_reports_lifecycle_counts_and_recent_sweep_metrics() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo =
        crate::repositories::note::NoteConsolidationRepository::new(db.clone());

    // Create notes with different statuses.
    let _active1 = repo
        .create(&project.id, "Active Note One", "body", "adr", "[]")
        .await
        .unwrap();
    let _active2 = repo
        .create(&project.id, "Active Note Two", "body", "pattern", "[]")
        .await
        .unwrap();
    let archived = repo
        .create(&project.id, "Archived Note", "body", "case", "[]")
        .await
        .unwrap();
    let deprecated = repo
        .create(&project.id, "Deprecated Note", "body", "pitfall", "[]")
        .await
        .unwrap();

    // Flip statuses.
    sqlx::query("UPDATE notes SET status = 'archived' WHERE id = $1")
        .bind(&archived.id)
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query("UPDATE notes SET status = 'deprecated' WHERE id = $1")
        .bind(&deprecated.id)
        .execute(db.pool())
        .await
        .unwrap();

    // Insert a lifecycle sweep metric row.
    consolidation_repo
        .create_run_metric(crate::repositories::note::CreateConsolidationRunMetric {
            project_id: &project.id,
            note_type: "lifecycle_sweep",
            status: "completed",
            scanned_note_count: 10,
            candidate_cluster_count: 0,
            consolidated_cluster_count: 0,
            consolidated_note_count: 0,
            source_note_count: 0,
            decayed_note_count: 2,
            archived_note_count: 1,
            superseded_source_note_count: 3,
            admission_dropped_note_count: 0,
            started_at: "2026-06-19T10:00:00.000Z",
            completed_at: Some("2026-06-19T10:01:00.000Z"),
            error_message: None,
        })
        .await
        .unwrap();

    let health = repo.health(&project.id).await.unwrap();

    assert_eq!(health.total_notes, 4);
    assert_eq!(health.lifecycle.active_notes, 2);
    assert_eq!(health.lifecycle.archived_notes, 1);
    assert_eq!(health.lifecycle.deprecated_notes, 1);
    assert_eq!(health.recent_sweep.last_decayed_count, 2);
    assert_eq!(health.recent_sweep.last_archived_count, 1);
    assert_eq!(health.recent_sweep.last_superseded_source_count, 3);
    assert_eq!(
        health.recent_sweep.last_sweep_at,
        Some("2026-06-19T10:01:00.000Z".to_string())
    );
    assert_eq!(health.admission_dropped_note_count, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extracted_note_audit_groups_merge_strengthen_demote_and_archive_backlogs() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let merge_a = repo
        .create_db_note(
            &project.id,
            "Schema seam prerequisite check",
            "Verify the prerequisite seam exists before wiring the schema seam. prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let merge_b = repo
        .create_db_note(
            &project.id,
            "Verify prerequisite seam before schema wiring",
            "Always verify the prerequisite seam exists before wiring the schema seam. prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

    for note in [&merge_a, &merge_b] {
        let abstract_text = format!(
            "{} prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            note.title
        );
        sqlx::query!(
            "UPDATE notes
             SET abstract = $1,
                 overview = $2
             WHERE id = $3",
            abstract_text,
            abstract_text,
            note.id
        )
        .execute(db.pool())
        .await
        .unwrap();
    }

    let underspecified_id = uuid::Uuid::now_v7().to_string();
    let underspecified_revision = repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project.id.clone(),
            note_id: Some(underspecified_id.clone()),
            event_kind: NoteRevisionEventKind::Created,
            desired: NoteRevisionDesiredState::Create(NoteRevisionCreateState {
                title: "Underspecified pattern note".into(),
                permalink: "patterns/underspecified-pattern-note".into(),
                content: "A short note with no template sections.".into(),
                note_type: "pattern".into(),
                folder: "patterns".into(),
                status: "active".into(),
                tags: "[]".into(),
                retrieval_anchor: None,
                scope_paths: "[]".into(),
                confidence: 0.5,
            }),
            attribution: TrustedNoteRevisionAttribution::agent("audit-fixture-agent").unwrap(),
            provenance: TrustedNoteRevisionProvenance::new(
                Some("audit-fixture-session".into()),
                Some("audit-fixture-task".into()),
                Some("audit-fixture-run".into()),
            )
            .unwrap(),
            reason: NoteRevisionReason::new("create low-quality audit fixture").unwrap(),
        })
        .await
        .unwrap();
    let underspecified = repo
        .get(&underspecified_id)
        .await
        .unwrap()
        .expect("attributed note");
    let underspecified_event = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project.id,
            note_id: &underspecified.id,
            limit: 1,
            before: None,
        })
        .await
        .unwrap()
        .events
        .into_iter()
        .next()
        .expect("attributed revision event");
    assert_eq!(
        underspecified_revision.revision_id.as_deref(),
        Some(underspecified_event.id.as_str())
    );

    let demote = repo
        .create_db_note(
            &project.id,
            "Current task roadmap note",
            "This session captured the current task status and drafted locally what to do next session if follow-up work remains.",
            "case",
            "[]",
        )
        .await
        .unwrap();

    let archive = repo
        .create_db_note(
            &project.id,
            "Footer-only extracted note",
            "Single paragraph extracted note.\n\n---\n*Extracted from session 123. Confidence: 0.2 (session-extracted).*",
            "pitfall",
            "[]",
        )
        .await
        .unwrap();
    repo.set_confidence(&archive.id, 0.2).await.unwrap();

    let report = repo.extracted_note_audit(&project.id).await.unwrap();

    assert_eq!(report.scanned_note_count, 5);
    assert!(
        report
            .rerun_hint
            .contains("Rerun `memory_extracted_audit()`")
    );
    assert!(
        report
            .merge_candidates
            .iter()
            .any(|finding| finding.note_id == merge_a.id
                && finding.related_note_ids.contains(&merge_b.id))
    );
    assert!(
        report
            .underspecified
            .iter()
            .any(|finding| finding.note_id == underspecified.id)
    );
    let attributed = report
        .underspecified
        .iter()
        .find(|finding| finding.note_id == underspecified.id)
        .and_then(|finding| finding.attribution.as_ref())
        .expect("ledger-backed finding attribution");
    assert_eq!(attributed.revision_id, underspecified_event.id);
    assert_eq!(attributed.revision_kind, "created");
    assert_eq!(attributed.revision_seq, Some(1));
    assert_eq!(
        attributed.revision_created_at,
        underspecified_event.created_at
    );
    assert_eq!(attributed.actor_kind, "agent");
    assert_eq!(attributed.actor_id.as_deref(), Some("audit-fixture-agent"));
    assert_eq!(attributed.subsystem, None);
    assert_eq!(
        attributed.session_id.as_deref(),
        Some("audit-fixture-session")
    );
    assert_eq!(attributed.task_id.as_deref(), Some("audit-fixture-task"));
    assert_eq!(attributed.task_run_id.as_deref(), Some("audit-fixture-run"));
    assert_eq!(attributed.reason, "create low-quality audit fixture");
    assert!(
        report
            .demote_to_working_spec
            .iter()
            .any(|finding| finding.note_id == demote.id)
    );
    assert_eq!(
        report
            .demote_to_working_spec
            .iter()
            .find(|finding| finding.note_id == demote.id)
            .expect("pre-migration finding")
            .attribution,
        None,
        "notes without ledger events must not receive fabricated attribution"
    );
    assert!(
        report
            .archive_candidates
            .iter()
            .any(|finding| finding.note_id == archive.id)
    );
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
        "Source",
        "See [[Future Note]].",
        "research",
        "[]",
    )
    .await
    .unwrap();
    assert_eq!(repo.broken_links(&project.id, None).await.unwrap().len(), 1);

    // Now create the target → broken link should be resolved.
    repo.create(&project.id, "Future Note", "body", "adr", "[]")
        .await
        .unwrap();
    assert_eq!(repo.broken_links(&project.id, None).await.unwrap().len(), 0);
    assert_eq!(repo.graph(&project.id).await.unwrap().edges.len(), 1);
}

// reindex_from_disk tests removed: the on-disk reindex pipeline is gone.
