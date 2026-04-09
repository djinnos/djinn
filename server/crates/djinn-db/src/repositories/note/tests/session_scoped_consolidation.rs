use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_sessions_with_provenance_returns_distinct_session_ids() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    // No provenance yet
    let sessions = consolidation_repo
        .list_sessions_with_provenance()
        .await
        .unwrap();
    assert!(sessions.is_empty());

    let note_a = repo
        .create_db_note(&project.id, "Note A", "body a", "pattern", "[]")
        .await
        .unwrap();
    let note_b = repo
        .create_db_note(&project.id, "Note B", "body b", "pattern", "[]")
        .await
        .unwrap();
    let session_x = make_session(&db, &project.id, None, "worker/x").await;
    let session_y = make_session(&db, &project.id, None, "worker/y").await;

    consolidation_repo
        .add_provenance(&note_a.id, &session_x)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&note_b.id, &session_x)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&note_b.id, &session_y)
        .await
        .unwrap();

    let sessions = consolidation_repo
        .list_sessions_with_provenance()
        .await
        .unwrap();
    assert_eq!(sessions.len(), 2);
    assert!(sessions.contains(&session_x));
    assert!(sessions.contains(&session_y));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_db_note_groups_for_session_scopes_to_session_provenance() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    let note_a = repo
        .create_db_note(&project.id, "Pattern A", "body a", "pattern", "[]")
        .await
        .unwrap();
    let note_b = repo
        .create_db_note(&project.id, "Pattern B", "body b", "pattern", "[]")
        .await
        .unwrap();
    let note_c = repo
        .create_db_note(&project.id, "Case C", "body c", "case", "[]")
        .await
        .unwrap();

    let session_1 = make_session(&db, &project.id, None, "worker/s1").await;
    let session_2 = make_session(&db, &project.id, None, "worker/s2").await;

    // Session 1 produced 2 patterns
    consolidation_repo
        .add_provenance(&note_a.id, &session_1)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&note_b.id, &session_1)
        .await
        .unwrap();

    // Session 2 produced only 1 case note (below minimum of 2 for grouping)
    consolidation_repo
        .add_provenance(&note_c.id, &session_2)
        .await
        .unwrap();

    let groups_s1 = consolidation_repo
        .list_db_note_groups_for_session(&session_1)
        .await
        .unwrap();
    assert_eq!(groups_s1.len(), 1);
    assert_eq!(groups_s1[0].project_id, project.id);
    assert_eq!(groups_s1[0].note_type, "pattern");
    assert_eq!(groups_s1[0].note_count, 2);

    // Session 2 has only 1 case note, below the 2-note threshold
    let groups_s2 = consolidation_repo
        .list_db_note_groups_for_session(&session_2)
        .await
        .unwrap();
    assert!(groups_s2.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_db_notes_in_group_for_session_returns_only_session_linked_notes() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    let note_a = repo
        .create_db_note(&project.id, "Pattern A", "body a", "pattern", "[]")
        .await
        .unwrap();
    let note_b = repo
        .create_db_note(&project.id, "Pattern B", "body b", "pattern", "[]")
        .await
        .unwrap();
    let note_c = repo
        .create_db_note(&project.id, "Pattern C", "body c", "pattern", "[]")
        .await
        .unwrap();

    let session_1 = make_session(&db, &project.id, None, "worker/s1").await;
    let session_2 = make_session(&db, &project.id, None, "worker/s2").await;

    // Session 1: note_a and note_b
    consolidation_repo
        .add_provenance(&note_a.id, &session_1)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&note_b.id, &session_1)
        .await
        .unwrap();

    // Session 2: note_c only
    consolidation_repo
        .add_provenance(&note_c.id, &session_2)
        .await
        .unwrap();

    let session_1_notes = consolidation_repo
        .list_db_notes_in_group_for_session(&project.id, "pattern", &session_1)
        .await
        .unwrap();
    let session_1_ids: std::collections::HashSet<_> =
        session_1_notes.iter().map(|n| n.id.clone()).collect();
    assert_eq!(session_1_ids.len(), 2);
    assert!(session_1_ids.contains(&note_a.id));
    assert!(session_1_ids.contains(&note_b.id));
    assert!(!session_1_ids.contains(&note_c.id));

    let session_2_notes = consolidation_repo
        .list_db_notes_in_group_for_session(&project.id, "pattern", &session_2)
        .await
        .unwrap();
    assert_eq!(session_2_notes.len(), 1);
    assert_eq!(session_2_notes[0].id, note_c.id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn session_scoped_clusters_do_not_include_cross_session_notes() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let consolidation_repo = NoteConsolidationRepository::new(db.clone());

    // Create 3 similar notes in session 1 (should cluster)
    let note_a = repo
        .create_db_note(
            &project.id,
            "Schema seam prerequisite check",
            "Verify the prerequisite seam exists before wiring the schema seam. prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let note_b = repo
        .create_db_note(
            &project.id,
            "Verify prerequisite seam before schema wiring",
            "Always verify the prerequisite seam exists before wiring the schema seam. prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    let note_c = repo
        .create_db_note(
            &project.id,
            "Deterministic seam verification for schema query",
            "Use deterministic verification to confirm the prerequisite seam before schema query wiring. prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

    // Create a similar note in session 2 (should NOT be included in session 1 clusters)
    let note_d = repo
        .create_db_note(
            &project.id,
            "Schema prerequisite seam external origin",
            "External origin prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            "pattern",
            "[]",
        )
        .await
        .unwrap();

    // Set abstract/overview for FTS matching
    for note in [&note_a, &note_b, &note_c, &note_d] {
        let abstract_text = format!(
            "{} prerequisite seam schema seam check duplication clustering deterministic query api stable ordering repeated tokens cross note match alpha beta gamma",
            note.title
        );
        sqlx::query("UPDATE notes SET abstract = ?2, overview = ?3 WHERE id = ?1")
            .bind(&note.id)
            .bind(&abstract_text)
            .bind(&abstract_text)
            .execute(db.pool())
            .await
            .unwrap();
    }

    let session_1 = make_session(&db, &project.id, None, "worker/s1").await;
    let session_2 = make_session(&db, &project.id, None, "worker/s2").await;

    // Session 1 produced note_a, note_b, note_c
    consolidation_repo
        .add_provenance(&note_a.id, &session_1)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&note_b.id, &session_1)
        .await
        .unwrap();
    consolidation_repo
        .add_provenance(&note_c.id, &session_1)
        .await
        .unwrap();

    // Session 2 produced only note_d
    consolidation_repo
        .add_provenance(&note_d.id, &session_2)
        .await
        .unwrap();

    // Session-scoped clusters for session 1 should contain note_a, note_b, note_c
    // but NOT note_d
    let clusters_s1 = consolidation_repo
        .likely_duplicate_clusters_for_session(&project.id, "pattern", &session_1)
        .await
        .unwrap();
    assert_eq!(clusters_s1.len(), 1, "session 1 should have 1 cluster");
    let cluster = &clusters_s1[0];
    let cluster_ids: std::collections::HashSet<_> = cluster.note_ids.iter().cloned().collect();
    assert!(cluster_ids.contains(&note_a.id));
    assert!(cluster_ids.contains(&note_b.id));
    assert!(cluster_ids.contains(&note_c.id));
    assert!(
        !cluster_ids.contains(&note_d.id),
        "cross-session note_d must not appear in session 1 cluster"
    );

    // Session 2 has only 1 note, so no clusters
    let clusters_s2 = consolidation_repo
        .likely_duplicate_clusters_for_session(&project.id, "pattern", &session_2)
        .await
        .unwrap();
    assert!(
        clusters_s2.is_empty(),
        "session 2 has only 1 note so should produce no clusters"
    );

    // Compare with the unscoped query which WOULD include note_d
    let clusters_all = consolidation_repo
        .likely_duplicate_clusters(&project.id, "pattern")
        .await
        .unwrap();
    assert_eq!(clusters_all.len(), 1);
    let all_ids: std::collections::HashSet<_> = clusters_all[0].note_ids.iter().cloned().collect();
    assert!(
        all_ids.contains(&note_d.id),
        "unscoped query should include the cross-session note"
    );
}
