//! PostgreSQL integration contract for the append-only memory revision boundary.
//!
//! The fixture is the deterministic source of content, reasons, and event order.
//! Positive ledger writes deliberately use only `mutate_with_revision`.

#[path = "common/mod.rs"]
mod common;

use djinn_db::{
    Database, NoteRepository, NoteRevisionCreateState, NoteRevisionDesiredState,
    NoteRevisionEventKind, NoteRevisionMutation, NoteRevisionReason, NoteRevisionSubsystem,
    ProjectRepository, TrustedNoteRevisionAttribution, TrustedNoteRevisionProvenance,
};
use futures::future::join_all;
use serde_json::Value;

const CONTRACT: &str = include_str!("fixtures/memory_revision_contract.json");

fn fixture() -> Value {
    serde_json::from_str(CONTRACT).expect("valid contract fixture")
}
fn reason(input: &str) -> NoteRevisionReason {
    NoteRevisionReason::new(input).expect("fixture reason")
}

fn create(
    project: &str,
    id: &str,
    title: &str,
    permalink: &str,
    content: &str,
    confidence: f64,
    attribution: TrustedNoteRevisionAttribution,
    reason_input: &str,
) -> NoteRevisionMutation {
    NoteRevisionMutation {
        project_id: project.into(),
        note_id: Some(id.into()),
        event_kind: NoteRevisionEventKind::Created,
        desired: NoteRevisionDesiredState::Create(NoteRevisionCreateState {
            title: title.into(),
            permalink: permalink.into(),
            content: content.into(),
            note_type: "reference".into(),
            folder: "reference".into(),
            status: "active".into(),
            tags: "[]".into(),
            retrieval_anchor: None,
            scope_paths: "[]".into(),
            confidence,
        }),
        attribution,
        provenance: TrustedNoteRevisionProvenance::default(),
        reason: reason(reason_input),
    }
}
fn update(
    project: &str,
    id: &str,
    kind: NoteRevisionEventKind,
    content: &str,
    confidence: f64,
    attribution: TrustedNoteRevisionAttribution,
    why: &str,
) -> NoteRevisionMutation {
    NoteRevisionMutation {
        project_id: project.into(),
        note_id: Some(id.into()),
        event_kind: kind,
        desired: NoteRevisionDesiredState::Existing {
            content: content.into(),
            confidence,
        },
        attribution,
        provenance: TrustedNoteRevisionProvenance::default(),
        reason: reason(why),
    }
}
async fn setup() -> (Database, NoteRepository, String) {
    let db = Database::ephemeral()
        .await
        .expect("isolated PostgreSQL database");
    let project = ProjectRepository::new(db.clone(), common::test_events())
        .create("revision-contract", "test", "revision-contract")
        .await
        .expect("project");
    (
        db.clone(),
        NoteRepository::new(db, common::test_events()),
        project.id,
    )
}

#[tokio::test]
async fn fixture_drives_all_repository_event_kinds_and_exact_values() {
    let f = fixture();
    let ids = &f["ids"];
    let content = &f["canonical_contents"];
    let rows = f["repository_expected_rows"].as_array().unwrap();
    let (db, repo, project) = setup().await;
    let primary = ids["notes"]["primary"].as_str().unwrap();
    let known = ids["notes"]["already_known"].as_str().unwrap();
    let target = ids["notes"]["merge_target"].as_str().unwrap();
    repo.mutate_with_revision(create(
        &project,
        primary,
        "Memory revision ledger",
        "reference/memory-revision-ledger",
        content["primary_initial"].as_str().unwrap(),
        0.5,
        TrustedNoteRevisionAttribution::human("fixture-human").unwrap(),
        rows[0]["reason_input"].as_str().unwrap(),
    ))
    .await
    .unwrap();
    repo.mutate_with_revision(update(
        &project,
        primary,
        NoteRevisionEventKind::Updated,
        content["primary_updated"].as_str().unwrap(),
        0.5,
        TrustedNoteRevisionAttribution::agent("fixture-agent").unwrap(),
        rows[1]["reason_input"].as_str().unwrap(),
    ))
    .await
    .unwrap();
    repo.mutate_with_revision(update(
        &project,
        primary,
        NoteRevisionEventKind::ConfidenceChanged,
        content["primary_updated"].as_str().unwrap(),
        0.75,
        TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Enrichment),
        rows[2]["reason_input"].as_str().unwrap(),
    ))
    .await
    .unwrap();
    repo.mutate_with_revision(create(
        &project,
        known,
        "Known fact",
        "reference/known-fact",
        content["already_known"].as_str().unwrap(),
        0.6,
        TrustedNoteRevisionAttribution::agent("fixture-agent").unwrap(),
        rows[3]["reason_input"].as_str().unwrap(),
    ))
    .await
    .unwrap();
    repo.mutate_with_revision(update(
        &project,
        known,
        NoteRevisionEventKind::ConfidenceChanged,
        content["already_known"].as_str().unwrap(),
        0.9,
        TrustedNoteRevisionAttribution::agent("fixture-agent").unwrap(),
        rows[4]["reason_input"].as_str().unwrap(),
    ))
    .await
    .unwrap();
    repo.mutate_with_revision(create(
        &project,
        target,
        "Durable guidance",
        "reference/durable-guidance",
        content["merge_target_before"].as_str().unwrap(),
        0.7,
        TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Consolidation),
        rows[5]["reason_input"].as_str().unwrap(),
    ))
    .await
    .unwrap();
    repo.mutate_with_revision(update(
        &project,
        target,
        NoteRevisionEventKind::Updated,
        content["merge_target_after"].as_str().unwrap(),
        0.95,
        TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Consolidation),
        rows[6]["reason_input"].as_str().unwrap(),
    ))
    .await
    .unwrap();
    repo.mutate_with_revision(NoteRevisionMutation {
        project_id: project.clone(),
        note_id: None,
        event_kind: NoteRevisionEventKind::ExtractionSkipped,
        desired: NoteRevisionDesiredState::ExtractionSkipped,
        attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Extraction),
        provenance: TrustedNoteRevisionProvenance::new(Some("fixture-session".into()), None, None)
            .unwrap(),
        reason: reason(rows[8]["reason_input"].as_str().unwrap()),
    })
    .await
    .unwrap();
    repo.mutate_with_revision(NoteRevisionMutation {
        project_id: project.clone(),
        note_id: Some(primary.into()),
        event_kind: NoteRevisionEventKind::Deleted,
        desired: NoteRevisionDesiredState::Delete,
        attribution: TrustedNoteRevisionAttribution::human("fixture-human").unwrap(),
        provenance: TrustedNoteRevisionProvenance::default(),
        reason: reason(rows[7]["reason_input"].as_str().unwrap()),
    })
    .await
    .unwrap();

    let persisted: Vec<(Option<String>, Option<i64>, String, Option<String>, Option<String>, Option<f64>, Option<f64>, String)> = sqlx::query_as("SELECT note_id, note_seq, event_kind, content_before, content_after, confidence_before, confidence_after, reason FROM note_revision_events WHERE project_id = $1 ORDER BY created_at, id").bind(&project).fetch_all(db.pool()).await.unwrap();
    assert_eq!(persisted.len(), 9);
    assert_eq!(persisted.iter().filter(|r| r.2 == "created").count(), 3);
    assert_eq!(persisted.iter().filter(|r| r.2 == "updated").count(), 2);
    assert_eq!(
        persisted
            .iter()
            .filter(|r| r.2 == "confidence_changed")
            .count(),
        2
    );
    assert_eq!(persisted.iter().filter(|r| r.2 == "deleted").count(), 1);
    assert_eq!(
        persisted
            .iter()
            .filter(|r| r.2 == "extraction_skipped" && r.0.is_none() && r.1.is_none())
            .count(),
        1
    );
    assert!(
        persisted.iter().any(|r| r.1 == Some(1)
            && r.4.as_deref() == Some(content["primary_initial"].as_str().unwrap()))
    );
    assert!(persisted.iter().any(|r| r.2 == "deleted"
        && r.3.as_deref() == Some(content["primary_updated"].as_str().unwrap())));
    assert!(persisted.iter().all(|r| r.7 == r.7.trim()));
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM note_revision_events WHERE project_id = $1")
            .bind(&project)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(
        !repo
            .mutate_with_revision(update(
                &project,
                known,
                NoteRevisionEventKind::ConfidenceChanged,
                content["already_known"].as_str().unwrap(),
                0.9,
                TrustedNoteRevisionAttribution::agent("fixture-agent").unwrap(),
                " unchanged "
            ))
            .await
            .unwrap()
            .changed
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM note_revision_events WHERE project_id = $1"
        )
        .bind(&project)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        count
    );
    let cutover = f["migration_cutover"]["pre_migration_note_id"]
        .as_str()
        .unwrap();
    sqlx::query("INSERT INTO notes (id, project_id, permalink, title, file_path) VALUES ($1, $2, 'cutover', 'Cutover', 'cutover.md')").bind(cutover).bind(&project).execute(db.pool()).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM note_revision_events WHERE note_id = $1"
        )
        .bind(cutover)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        0
    );
}

#[tokio::test]
async fn catalog_concurrency_immutability_and_project_erasure_hold() {
    let (db, repo, project) = setup().await;
    let note = uuid::Uuid::now_v7().to_string();
    repo.mutate_with_revision(create(
        &project,
        &note,
        "Concurrent",
        "reference/concurrent",
        "initial",
        0.5,
        TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Mcp),
        " create ",
    ))
    .await
    .unwrap();
    let updates = (0..6).map(|n| {
        let repo = repo.clone();
        let project = project.clone();
        let note = note.clone();
        async move {
            repo.mutate_with_revision(update(
                &project,
                &note,
                NoteRevisionEventKind::Updated,
                &format!("content {n}"),
                0.5,
                TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Enrichment),
                " concurrent ",
            ))
            .await
            .unwrap()
            .note_seq
            .unwrap()
        }
    });
    let mut seq = join_all(updates).await;
    seq.sort_unstable();
    assert_eq!(seq, (2..=7).collect::<Vec<_>>());
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM information_schema.columns WHERE table_name = 'note_revision_events' AND column_name IN ('id','project_id','note_id','note_seq','event_kind','reason','created_at')").fetch_one(db.pool()).await.unwrap(), 7);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM pg_indexes WHERE tablename = 'note_revision_events' AND indexname IN ('note_revision_events_project_note_seq_unique','note_revision_events_project_note_history')").fetch_one(db.pool()).await.unwrap(), 2);
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM information_schema.table_constraints tc JOIN information_schema.constraint_column_usage ccu ON tc.constraint_name = ccu.constraint_name WHERE tc.table_name = 'note_revision_events' AND tc.constraint_type = 'FOREIGN KEY' AND ccu.table_name = 'notes'").fetch_one(db.pool()).await.unwrap(), 0);
    let id: String =
        sqlx::query_scalar("SELECT id FROM note_revision_events WHERE note_id = $1 LIMIT 1")
            .bind(&note)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(
        sqlx::query("UPDATE note_revision_events SET reason = 'tampered' WHERE id = $1")
            .bind(&id)
            .execute(db.pool())
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM note_revision_events WHERE id = $1")
            .bind(&id)
            .execute(db.pool())
            .await
            .is_err()
    );
    ProjectRepository::new(db.clone(), common::test_events())
        .delete(&project)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM note_revision_events WHERE project_id = $1"
        )
        .bind(&project)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        0
    );
}
