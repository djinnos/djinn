use futures::future::join_all;

use super::*;

fn create_command(project_id: &str, note_id: String) -> NoteRevisionMutation {
    NoteRevisionMutation {
        project_id: project_id.to_owned(),
        note_id: Some(note_id),
        event_kind: NoteRevisionEventKind::Created,
        desired: NoteRevisionDesiredState::Create(NoteRevisionCreateState {
            title: "Ledger note".to_owned(),
            permalink: "reference/ledger-note".to_owned(),
            content: "initial content".to_owned(),
            note_type: "reference".to_owned(),
            folder: "reference".to_owned(),
            status: "active".to_owned(),
            tags: "[]".to_owned(),
            retrieval_anchor: None,
            scope_paths: "[]".to_owned(),
            confidence: 0.5,
        }),
        attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Mcp),
        provenance: TrustedNoteRevisionProvenance::default(),
        reason: NoteRevisionReason::new("create audited note").unwrap(),
    }
}

#[tokio::test]
async fn revision_mutation_commits_create_and_suppresses_canonical_noop() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(8);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let note_id = uuid::Uuid::now_v7().to_string();

    let created = repo
        .mutate_with_revision(create_command(&project.id, note_id.clone()))
        .await
        .unwrap();
    assert!(created.changed);
    assert_eq!(created.note_seq, Some(1));

    let no_op = repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project.id.clone(),
            note_id: Some(note_id.clone()),
            event_kind: NoteRevisionEventKind::Updated,
            desired: NoteRevisionDesiredState::Existing {
                content: "initial content".to_owned(),
                confidence: 0.5,
            },
            attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Mcp),
            provenance: TrustedNoteRevisionProvenance::default(),
            reason: NoteRevisionReason::new("unchanged desired state").unwrap(),
        })
        .await
        .unwrap();
    assert!(!no_op.changed);
    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM note_revision_events WHERE project_id = $1 AND note_id = $2",
    )
    .bind(&project.id)
    .bind(&note_id)
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(events, 1);
}

#[tokio::test]
async fn forced_revision_insert_failure_rolls_back_note_create() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(8);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let note_id = uuid::Uuid::now_v7().to_string();

    repo.set_revision_event_insertion_failure_for_test(true);
    assert!(
        repo.mutate_with_revision(create_command(&project.id, note_id.clone()))
            .await
            .is_err()
    );
    assert!(repo.get(&note_id).await.unwrap().is_none());
    let events: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM note_revision_events WHERE note_id = $1")
            .bind(&note_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(events, 0);
}

fn existing_command(
    project_id: &str,
    note_id: &str,
    event_kind: NoteRevisionEventKind,
    content: &str,
    confidence: f64,
) -> NoteRevisionMutation {
    NoteRevisionMutation {
        project_id: project_id.to_owned(),
        note_id: Some(note_id.to_owned()),
        event_kind,
        desired: NoteRevisionDesiredState::Existing {
            content: content.to_owned(),
            confidence,
        },
        attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Enrichment),
        provenance: TrustedNoteRevisionProvenance::default(),
        reason: NoteRevisionReason::new("fixture mutation").unwrap(),
    }
}

#[tokio::test]
async fn revision_mutation_persists_every_repository_event_shape() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(8);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let note_id = uuid::Uuid::now_v7().to_string();

    repo.mutate_with_revision(create_command(&project.id, note_id.clone()))
        .await
        .unwrap();
    repo.mutate_with_revision(existing_command(
        &project.id,
        &note_id,
        NoteRevisionEventKind::Updated,
        "fixture updated content",
        0.5,
    ))
    .await
    .unwrap();
    repo.mutate_with_revision(existing_command(
        &project.id,
        &note_id,
        NoteRevisionEventKind::ConfidenceChanged,
        "fixture updated content",
        0.75,
    ))
    .await
    .unwrap();
    repo.mutate_with_revision(NoteRevisionMutation {
        project_id: project.id.clone(),
        note_id: None,
        event_kind: NoteRevisionEventKind::ExtractionSkipped,
        desired: NoteRevisionDesiredState::ExtractionSkipped,
        attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Extraction),
        provenance: TrustedNoteRevisionProvenance::new(None, None, Some("fixture-run".into()))
            .unwrap(),
        reason: NoteRevisionReason::new("fixture extraction produced no note").unwrap(),
    })
    .await
    .unwrap();
    repo.mutate_with_revision(NoteRevisionMutation {
        project_id: project.id.clone(),
        note_id: Some(note_id.clone()),
        event_kind: NoteRevisionEventKind::Deleted,
        desired: NoteRevisionDesiredState::Delete,
        attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Mcp),
        provenance: TrustedNoteRevisionProvenance::default(),
        reason: NoteRevisionReason::new("fixture delete").unwrap(),
    })
    .await
    .unwrap();

    let rows: Vec<(String, Option<i64>, Option<String>, Option<String>, Option<f64>, Option<f64>)> = sqlx::query_as("SELECT event_kind, note_seq, content_before, content_after, confidence_before, confidence_after FROM note_revision_events WHERE project_id = $1 ORDER BY note_seq NULLS LAST")
        .bind(&project.id).fetch_all(db.pool()).await.unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(
        rows[0],
        (
            "created".into(),
            Some(1),
            None,
            Some("initial content".into()),
            None,
            Some(0.5)
        )
    );
    assert_eq!(
        rows[1],
        (
            "updated".into(),
            Some(2),
            Some("initial content".into()),
            Some("fixture updated content".into()),
            Some(0.5),
            Some(0.5)
        )
    );
    assert_eq!(
        rows[2],
        (
            "confidence_changed".into(),
            Some(3),
            None,
            None,
            Some(0.5),
            Some(0.75)
        )
    );
    assert_eq!(
        rows[3],
        (
            "deleted".into(),
            Some(4),
            Some("fixture updated content".into()),
            None,
            Some(0.75),
            None
        )
    );
    assert_eq!(
        rows[4],
        ("extraction_skipped".into(), None, None, None, None, None)
    );

    let task_only = repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project.id,
            note_id: None,
            event_kind: NoteRevisionEventKind::ExtractionSkipped,
            desired: NoteRevisionDesiredState::ExtractionSkipped,
            attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Extraction),
            provenance: TrustedNoteRevisionProvenance::new(None, Some("task-only".into()), None)
                .unwrap(),
            reason: NoteRevisionReason::new("invalid task only extraction").unwrap(),
        })
        .await;
    assert!(task_only.is_err());
}

#[tokio::test]
async fn revision_mutation_replaces_wikilinks_and_rolls_back_failed_updates() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(8);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));

    let create_named = |title: &str, content: &str| {
        let mut command = create_command(&project.id, uuid::Uuid::now_v7().to_string());
        let NoteRevisionDesiredState::Create(state) = &mut command.desired else {
            unreachable!("create_command has create state");
        };
        state.title = title.to_owned();
        state.permalink = format!("reference/{}", title.to_lowercase());
        state.content = content.to_owned();
        command
    };

    let old_target = repo
        .mutate_with_revision(create_named("OldTarget", "old target"))
        .await
        .unwrap()
        .note
        .unwrap();
    let new_target = repo
        .mutate_with_revision(create_named("NewTarget", "new target"))
        .await
        .unwrap()
        .note
        .unwrap();
    let source = repo
        .mutate_with_revision(create_named("Source", "links [[OldTarget]]"))
        .await
        .unwrap()
        .note
        .unwrap();

    repo.mutate_with_revision(existing_command(
        &project.id,
        &source.id,
        NoteRevisionEventKind::Updated,
        "links [[NewTarget]]",
        0.5,
    ))
    .await
    .unwrap();
    let graph = repo.graph(&project.id).await.unwrap();
    assert!(
        graph
            .edges
            .iter()
            .any(|edge| edge.source_id == source.id && edge.target_id == new_target.id)
    );
    assert!(
        !graph
            .edges
            .iter()
            .any(|edge| edge.source_id == source.id && edge.target_id == old_target.id)
    );

    repo.set_revision_event_insertion_failure_for_test(true);
    assert!(
        repo.mutate_with_revision(existing_command(
            &project.id,
            &source.id,
            NoteRevisionEventKind::Updated,
            "links [[OldTarget]]",
            0.5,
        ))
        .await
        .is_err()
    );
    repo.set_revision_event_insertion_failure_for_test(false);

    assert_eq!(
        repo.get(&source.id).await.unwrap().unwrap().content,
        "links [[NewTarget]]"
    );
    let graph_after_rollback = repo.graph(&project.id).await.unwrap();
    assert!(
        graph_after_rollback
            .edges
            .iter()
            .any(|edge| edge.source_id == source.id && edge.target_id == new_target.id)
    );
    assert!(
        !graph_after_rollback
            .edges
            .iter()
            .any(|edge| edge.source_id == source.id && edge.target_id == old_target.id)
    );
}

#[tokio::test]
async fn concurrent_revision_updates_allocate_contiguous_sequences() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(8);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let note_id = uuid::Uuid::now_v7().to_string();
    repo.mutate_with_revision(create_command(&project.id, note_id.clone()))
        .await
        .unwrap();

    let updates = (0..8).map(|index| {
        let repo = repo.clone();
        let project_id = project.id.clone();
        let note_id = note_id.clone();
        async move {
            repo.mutate_with_revision(existing_command(
                &project_id,
                &note_id,
                NoteRevisionEventKind::Updated,
                &format!("concurrent fixture content {index}"),
                0.5,
            ))
            .await
            .unwrap()
            .note_seq
            .unwrap()
        }
    });
    let mut sequences = join_all(updates).await;
    sequences.sort_unstable();
    assert_eq!(sequences, (2..=9).collect::<Vec<_>>());
    let persisted: Vec<i64> = sqlx::query_scalar("SELECT note_seq FROM note_revision_events WHERE project_id = $1 AND note_id = $2 ORDER BY note_seq")
        .bind(&project.id).bind(&note_id).fetch_all(db.pool()).await.unwrap();
    assert_eq!(persisted, (1..=9).collect::<Vec<_>>());
}

#[tokio::test]
async fn guarded_patch_records_transition_and_rejects_invalid_requests() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(8);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let note_id = uuid::Uuid::now_v7().to_string();
    repo.mutate_with_revision(create_command(&project.id, note_id.clone()))
        .await
        .unwrap();
    let patched = repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project.id.clone(),
            note_id: Some(note_id.clone()),
            event_kind: NoteRevisionEventKind::Updated,
            desired: NoteRevisionDesiredState::GuardedPatch {
                expected_content: "initial content".into(),
                content: "patched content".into(),
                confidence: 0.7,
            },
            attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Extraction),
            provenance: TrustedNoteRevisionProvenance::new(
                Some("session".into()),
                Some("task".into()),
                Some("run".into()),
            )
            .unwrap(),
            reason: NoteRevisionReason::new("guarded extraction patch").unwrap(),
        })
        .await
        .unwrap();
    assert!(patched.changed);
    let patch_event = repo
        .revision_events_for_note(&project.id, &note_id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        patch_event.content_before.as_deref(),
        Some("initial content")
    );
    assert_eq!(
        patch_event.content_after.as_deref(),
        Some("patched content")
    );
    assert_eq!(patch_event.confidence_before, Some(0.5));
    assert_eq!(patch_event.confidence_after, Some(0.7));

    for (expected_content, confidence) in [("stale", 0.7), ("patched content", 1.0)] {
        assert!(
            repo.mutate_with_revision(NoteRevisionMutation {
                project_id: project.id.clone(),
                note_id: Some(note_id.clone()),
                event_kind: NoteRevisionEventKind::Updated,
                desired: NoteRevisionDesiredState::GuardedPatch {
                    expected_content: expected_content.into(),
                    content: "patched".into(),
                    confidence
                },
                attribution: TrustedNoteRevisionAttribution::system(
                    NoteRevisionSubsystem::Extraction
                ),
                provenance: TrustedNoteRevisionProvenance::new(
                    Some("session".into()),
                    Some("task".into()),
                    Some("run".into())
                )
                .unwrap(),
                reason: NoteRevisionReason::new("guarded extraction patch").unwrap(),
            })
            .await
            .is_err()
        );
    }
    assert_eq!(
        repo.get(&note_id).await.unwrap().unwrap().content,
        "patched content"
    );
    assert_eq!(
        repo.revision_events_for_note(&project.id, &note_id)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn deprecate_with_supersedes_is_atomic_and_returns_auditable_metadata() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(8);
    let project = make_project(&db, tmp.path()).await;
    // Notes are unique by `(project_id, permalink)`, so the foreign-note
    // fixture needs an independent project root while retaining this database.
    let other_project_root = crate::database::test_tempdir().unwrap();
    let other_project = make_project(&db, other_project_root.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    // Reverse creation order so this command proves locking is by ID rather
    // than by old/new role: `old_id` is deterministically locked second.
    let new_id = uuid::Uuid::now_v7().to_string();
    let old_id = uuid::Uuid::now_v7().to_string();
    let foreign_id = uuid::Uuid::now_v7().to_string();
    let mut old_create = create_command(&project.id, old_id.clone());
    let NoteRevisionDesiredState::Create(state) = &mut old_create.desired else {
        unreachable!("create_command has create state");
    };
    state.permalink = "reference/old-ledger-note".to_owned();
    repo.mutate_with_revision(old_create).await.unwrap();
    let mut new_create = create_command(&project.id, new_id.clone());
    let NoteRevisionDesiredState::Create(state) = &mut new_create.desired else {
        unreachable!("create_command has create state");
    };
    state.permalink = "reference/new-ledger-note".to_owned();
    repo.mutate_with_revision(new_create).await.unwrap();
    repo.mutate_with_revision(create_command(&other_project.id, foreign_id.clone()))
        .await
        .unwrap();
    let command = |superseding_note_id: String| NoteRevisionMutation {
        project_id: project.id.clone(),
        note_id: Some(old_id.clone()),
        event_kind: NoteRevisionEventKind::Updated,
        desired: NoteRevisionDesiredState::DeprecateWithSupersedes {
            superseding_note_id,
            association_weight: 0.9,
        },
        attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Extraction),
        provenance: TrustedNoteRevisionProvenance::new(
            Some("session".into()),
            Some("task".into()),
            Some("run".into()),
        )
        .unwrap(),
        reason: NoteRevisionReason::new("superseded by canonical extraction").unwrap(),
    };

    assert!(
        repo.mutate_with_revision(command(foreign_id.clone()))
            .await
            .is_err()
    );
    assert_eq!(repo.get(&old_id).await.unwrap().unwrap().status, "active");
    assert_eq!(
        repo.revision_events_for_note(&project.id, &old_id)
            .await
            .unwrap()
            .len(),
        1
    );

    repo.set_supersedes_association_failure_for_test(true);
    assert!(
        repo.mutate_with_revision(command(new_id.clone()))
            .await
            .is_err()
    );
    repo.set_supersedes_association_failure_for_test(false);
    assert_eq!(repo.get(&old_id).await.unwrap().unwrap().status, "active");
    assert!(
        repo.get_association_kind(&old_id, &new_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repo.revision_events_for_note(&project.id, &old_id)
            .await
            .unwrap()
            .len(),
        1
    );

    repo.set_revision_event_insertion_failure_for_test(true);
    assert!(
        repo.mutate_with_revision(command(new_id.clone()))
            .await
            .is_err()
    );
    repo.set_revision_event_insertion_failure_for_test(false);
    assert_eq!(repo.get(&old_id).await.unwrap().unwrap().status, "active");
    assert!(
        repo.get_association_kind(&old_id, &new_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        repo.revision_events_for_note(&project.id, &old_id)
            .await
            .unwrap()
            .len(),
        1
    );

    // A stronger existing edge must be reported as the committed edge, not the
    // lower requested replacement weight.
    repo.record_supersedes(&new_id, &old_id, 1.0).await.unwrap();
    let result = repo
        .mutate_with_revision(command(new_id.clone()))
        .await
        .unwrap();
    assert_eq!(result.deprecated_note_id.as_deref(), Some(old_id.as_str()));
    assert_eq!(result.superseding_note_id.as_deref(), Some(new_id.as_str()));
    let association = result.supersedes_association.unwrap();
    assert_eq!(association.kind, NoteAssociationKind::Supersedes);
    assert_eq!(association.weight, 1.0);
    let (expected_a, expected_b) = if old_id < new_id {
        (&old_id, &new_id)
    } else {
        (&new_id, &old_id)
    };
    assert_eq!(association.note_a_id, *expected_a);
    assert_eq!(association.note_b_id, *expected_b);
    assert_eq!(
        repo.get_association_kind(&old_id, &new_id).await.unwrap(),
        Some((1.0, "supersedes".to_owned()))
    );
    assert_eq!(
        repo.get(&old_id).await.unwrap().unwrap().status,
        "deprecated"
    );
    let event = repo
        .revision_events_for_note(&project.id, &old_id)
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(event.content_before.as_deref(), Some("initial content"));
    assert_eq!(event.content_after.as_deref(), Some("initial content"));
    assert_eq!(event.confidence_before, Some(0.5));
    assert_eq!(event.confidence_after, Some(0.5));
}

#[tokio::test]
async fn guarded_deprecation_is_not_resurrected_by_decay_or_archive_housekeeping() {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(8);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let deprecated_id = uuid::Uuid::now_v7().to_string();
    let replacement_id = uuid::Uuid::now_v7().to_string();

    let mut deprecated = create_command(&project.id, deprecated_id.clone());
    let NoteRevisionDesiredState::Create(state) = &mut deprecated.desired else {
        unreachable!()
    };
    state.note_type = "case".to_owned();
    state.folder = "cases".to_owned();
    state.permalink = "cases/deprecated-guarded-note".to_owned();
    state.content =
        "One short extracted paragraph.\n\n*Extracted from session fixture.*".to_owned();
    repo.mutate_with_revision(deprecated).await.unwrap();
    let mut replacement = create_command(&project.id, replacement_id.clone());
    let NoteRevisionDesiredState::Create(state) = &mut replacement.desired else {
        unreachable!()
    };
    state.note_type = "case".to_owned();
    state.folder = "cases".to_owned();
    state.permalink = "cases/guarded-replacement-note".to_owned();
    repo.mutate_with_revision(replacement).await.unwrap();

    assert!(
        repo.mutate_with_revision(NoteRevisionMutation {
            project_id: project.id.clone(),
            note_id: Some(deprecated_id.clone()),
            event_kind: NoteRevisionEventKind::Updated,
            desired: NoteRevisionDesiredState::DeprecateWithSupersedes {
                superseding_note_id: replacement_id,
                association_weight: 1.0
            },
            attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Extraction),
            provenance: TrustedNoteRevisionProvenance::new(
                Some("session".into()),
                Some("task".into()),
                Some("run".into())
            )
            .unwrap(),
            reason: NoteRevisionReason::new("guarded extraction replacement").unwrap(),
        })
        .await
        .unwrap()
        .changed
    );
    assert_eq!(
        repo.get(&deprecated_id).await.unwrap().unwrap().status,
        "deprecated"
    );
    assert_eq!(
        repo.decay_stale_extracted_notes(&project.id, 30)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        repo.archive_audit_candidates(&project.id, 30)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        repo.get(&deprecated_id).await.unwrap().unwrap().status,
        "deprecated"
    );
}
