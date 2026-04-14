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
    assert_eq!(health.stale_note_count, 0);
    assert_eq!(health.low_confidence_note_count, 0);
    assert_eq!(health.duplicate_cluster_count, 0);
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

    let underspecified = repo
        .create_db_note(
            &project.id,
            "Underspecified pattern note",
            "A short note with no template sections.",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

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
    assert!(
        report
            .demote_to_working_spec
            .iter()
            .any(|finding| finding.note_id == demote.id)
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
