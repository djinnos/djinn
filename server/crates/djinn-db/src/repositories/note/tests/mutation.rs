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
