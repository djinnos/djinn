//! PostgreSQL integration contract for the append-only memory revision boundary.
//!
//! Fixture values drive writer attribution, provenance, content, reasons, and
//! cursor order. Positive ledger writes use only `mutate_with_revision`.

use super::*;
use crate::ProjectRepository;
use djinn_core::events::EventBus;
use futures::future::join_all;
use serde_json::Value;

// This repository-level PostgreSQL contract lives in djinn-db because catalog
// and deliberate negative SQL must not cross the raw-SQL repository boundary.
const CONTRACT: &str =
    include_str!("../../../../../djinn-control-plane/tests/fixtures/memory_revision_contract.json");

#[derive(sqlx::FromRow)]
struct PersistedRow {
    id: String,
    project_id: String,
    note_id: Option<String>,
    note_seq: Option<i64>,
    event_kind: String,
    content_before: Option<String>,
    content_after: Option<String>,
    confidence_before: Option<f64>,
    confidence_after: Option<f64>,
    actor_kind: String,
    actor_id: Option<String>,
    subsystem: Option<String>,
    session_id: Option<String>,
    task_id: Option<String>,
    task_run_id: Option<String>,
    reason: String,
    created_at: String,
}

fn fixture() -> Value {
    serde_json::from_str(CONTRACT).expect("valid contract fixture")
}
fn reason(input: &str) -> NoteRevisionReason {
    NoteRevisionReason::new(input).expect("fixture reason")
}

/// Fixture writer contexts translate to the physical attribution shape: human
/// and agent have IDs, while system has a registered subsystem.
fn attribution(row: &Value) -> TrustedNoteRevisionAttribution {
    match row["actor_kind"].as_str().unwrap() {
        "human" => {
            TrustedNoteRevisionAttribution::human(row["actor_id"].as_str().unwrap()).unwrap()
        }
        "agent" => {
            TrustedNoteRevisionAttribution::agent(row["actor_id"].as_str().unwrap()).unwrap()
        }
        "system" => {
            TrustedNoteRevisionAttribution::system(match row["subsystem"].as_str().unwrap() {
                "mcp" => NoteRevisionSubsystem::Mcp,
                "dedup" => NoteRevisionSubsystem::Dedup,
                "consolidation" => NoteRevisionSubsystem::Consolidation,
                "enrichment" => NoteRevisionSubsystem::Enrichment,
                "extraction" => NoteRevisionSubsystem::Extraction,
                value => panic!("unknown fixture subsystem: {value}"),
            })
        }
        value => panic!("unknown fixture actor: {value}"),
    }
}
fn provenance(row: &Value) -> TrustedNoteRevisionProvenance {
    TrustedNoteRevisionProvenance::new(
        row["session_id"].as_str().map(str::to_owned),
        row["task_id"].as_str().map(str::to_owned),
        row["task_run_id"].as_str().map(str::to_owned),
    )
    .unwrap()
}
fn create(
    project: &str,
    id: &str,
    title: &str,
    permalink: &str,
    content: &str,
    confidence: f64,
    attribution: TrustedNoteRevisionAttribution,
    provenance: TrustedNoteRevisionProvenance,
    why: &str,
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
        provenance,
        reason: reason(why),
    }
}
fn update(
    project: &str,
    id: &str,
    kind: NoteRevisionEventKind,
    content: &str,
    confidence: f64,
    attribution: TrustedNoteRevisionAttribution,
    provenance: TrustedNoteRevisionProvenance,
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
        provenance,
        reason: reason(why),
    }
}
async fn setup(project_id: &str) -> (Database, NoteRepository) {
    let db = Database::ephemeral()
        .await
        .expect("isolated PostgreSQL database");
    ProjectRepository::new(db.clone(), EventBus::noop())
        .create_with_id(project_id, "revision-contract", "test", "revision-contract")
        .await
        .expect("project");
    (db.clone(), NoteRepository::new(db, EventBus::noop()))
}
fn assert_fixture_row(actual: &PersistedRow, expected: &Value) {
    assert!(
        uuid::Uuid::parse_str(&actual.id).is_ok(),
        "repository-generated ID is UUID"
    );
    assert_eq!(actual.project_id, expected["project_id"].as_str().unwrap());
    assert_eq!(actual.note_id.as_deref(), expected["note_id"].as_str());
    assert_eq!(actual.note_seq, expected["sequence"].as_i64());
    assert_eq!(
        actual.event_kind,
        match expected["event_kind"].as_str().unwrap() {
            "create" => "created",
            "update" => "updated",
            "delete" => "deleted",
            "confidence_bump" => "confidence_changed",
            value => value,
        }
    );
    assert_eq!(
        actual.content_before.as_deref(),
        expected["before_snapshot"]["content"].as_str()
    );
    assert_eq!(
        actual.content_after.as_deref(),
        expected["after_snapshot"]["content"].as_str()
    );
    assert_eq!(
        actual.confidence_before,
        expected["confidence_before"].as_f64()
    );
    assert_eq!(
        actual.confidence_after,
        expected["confidence_after"].as_f64()
    );
    assert_eq!(actual.actor_kind, expected["actor_kind"].as_str().unwrap());
    let system = expected["actor_kind"] == "system";
    assert_eq!(
        actual.actor_id.as_deref(),
        (!system).then(|| expected["actor_id"].as_str().unwrap())
    );
    assert_eq!(
        actual.subsystem.as_deref(),
        system.then(|| expected["subsystem"].as_str().unwrap())
    );
    assert_eq!(
        actual.session_id.as_deref(),
        expected["session_id"].as_str()
    );
    assert_eq!(actual.task_id.as_deref(), expected["task_id"].as_str());
    assert_eq!(
        actual.task_run_id.as_deref(),
        expected["task_run_id"].as_str()
    );
    assert_eq!(actual.reason, expected["reason"].as_str().unwrap());
    assert!(
        actual.created_at.ends_with('Z') && actual.created_at.len() >= 20,
        "generated cursor timestamp"
    );
}

#[tokio::test]
async fn fixture_drives_all_repository_event_kinds_and_exact_values() {
    let f = fixture();
    let ids = &f["ids"];
    let content = &f["canonical_contents"];
    let rows = f["repository_expected_rows"].as_array().unwrap();
    let project = ids["project_id"].as_str().unwrap().to_owned();
    let (db, repo) = setup(&project).await;
    let primary = ids["notes"]["primary"].as_str().unwrap();
    let known = ids["notes"]["already_known"].as_str().unwrap();
    let target = ids["notes"]["merge_target"].as_str().unwrap();
    let unscoped = ids["notes"]["unscoped"].as_str().unwrap();
    repo.mutate_with_revision(create(
        &project,
        primary,
        "Memory revision ledger",
        "reference/memory-revision-ledger",
        content["primary_initial"].as_str().unwrap(),
        0.5,
        attribution(&rows[0]),
        provenance(&rows[0]),
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
        attribution(&rows[1]),
        provenance(&rows[1]),
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
        attribution(&rows[2]),
        provenance(&rows[2]),
        rows[2]["reason_input"].as_str().unwrap(),
    ))
    .await
    .unwrap();
    let events_before_unchanged_content: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM note_revision_events WHERE project_id = $1")
            .bind(&project)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(
        !repo
            .mutate_with_revision(update(
                &project,
                primary,
                NoteRevisionEventKind::Updated,
                content["primary_updated"].as_str().unwrap(),
                0.75,
                attribution(&rows[2]),
                provenance(&rows[2]),
                " unchanged content ",
            ))
            .await
            .unwrap()
            .changed,
        "an unchanged content mutation is a semantic no-op"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM note_revision_events WHERE project_id = $1",
        )
        .bind(&project)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        events_before_unchanged_content,
        "unchanged content must not append a revision"
    );
    repo.mutate_with_revision(create(
        &project,
        known,
        "Known fact",
        "reference/known-fact",
        content["already_known"].as_str().unwrap(),
        0.6,
        attribution(&rows[3]),
        provenance(&rows[3]),
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
        attribution(&rows[4]),
        provenance(&rows[4]),
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
        attribution(&rows[5]),
        provenance(&rows[5]),
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
        attribution(&rows[6]),
        provenance(&rows[6]),
        rows[6]["reason_input"].as_str().unwrap(),
    ))
    .await
    .unwrap();
    let delete_result = repo
        .mutate_with_revision(NoteRevisionMutation {
            project_id: project.clone(),
            note_id: Some(primary.into()),
            event_kind: NoteRevisionEventKind::Deleted,
            desired: NoteRevisionDesiredState::Delete,
            attribution: attribution(&rows[7]),
            provenance: provenance(&rows[7]),
            reason: reason(rows[7]["reason_input"].as_str().unwrap()),
        })
        .await
        .unwrap();
    assert!(
        delete_result.note.is_none(),
        "delete returns no primary note"
    );
    let retained_history = &f["retained_deleted_note_history"];
    assert_eq!(retained_history["note_id"].as_str(), Some(primary));
    let note_row_exists_after_delete: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM notes WHERE id = $1 AND project_id = $2)")
            .bind(primary)
            .bind(&project)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(
        note_row_exists_after_delete,
        retained_history["note_row_exists_after_delete"]
            .as_bool()
            .unwrap(),
        "primary note row is removed while delete revision history is retained"
    );
    repo.mutate_with_revision(NoteRevisionMutation {
        project_id: project.clone(),
        note_id: None,
        event_kind: NoteRevisionEventKind::ExtractionSkipped,
        desired: NoteRevisionDesiredState::ExtractionSkipped,
        attribution: attribution(&rows[8]),
        provenance: provenance(&rows[8]),
        reason: reason(rows[8]["reason_input"].as_str().unwrap()),
    })
    .await
    .unwrap();
    repo.mutate_with_revision(create(
        &project,
        unscoped,
        "Unscoped memory mutation",
        "reference/unscoped-memory-mutation",
        content["unscoped"].as_str().unwrap(),
        0.5,
        attribution(&rows[9]),
        provenance(&rows[9]),
        rows[9]["reason_input"].as_str().unwrap(),
    ))
    .await
    .unwrap();
    let persisted: Vec<PersistedRow> = sqlx::query_as("SELECT id, project_id, note_id, note_seq, event_kind, content_before, content_after, confidence_before, confidence_after, actor_kind, actor_id, subsystem, session_id, task_id, task_run_id, reason, created_at FROM note_revision_events WHERE project_id = $1 ORDER BY created_at, id").bind(&project).fetch_all(db.pool()).await.unwrap();
    assert_eq!(persisted.len(), rows.len());
    let expected_cursor_ids = f["cursor_order"]["expected_revision_ids"]
        .as_array()
        .unwrap();
    for ((actual, expected), cursor_id) in persisted.iter().zip(rows).zip(expected_cursor_ids) {
        assert_eq!(expected["id"].as_str(), cursor_id.as_str());
        assert_fixture_row(actual, expected);
    }
    assert!(
        rows.windows(2)
            .all(|pair| pair[0]["created_at"].as_str() <= pair[1]["created_at"].as_str())
    );
    assert_eq!(persisted.len(), expected_cursor_ids.len());
    assert!(
        persisted
            .windows(2)
            .all(|pair| (pair[0].created_at.clone(), pair[0].id.clone())
                <= (pair[1].created_at.clone(), pair[1].id.clone()))
    );
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
                attribution(&rows[4]),
                provenance(&rows[4]),
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
async fn catalog_concurrency_rollback_immutability_and_project_erasure_hold() {
    let project = uuid::Uuid::now_v7().to_string();
    let (db, repo) = setup(&project).await;
    let note = uuid::Uuid::now_v7().to_string();
    repo.mutate_with_revision(create(
        &project,
        &note,
        "Concurrent",
        "reference/concurrent",
        "initial",
        0.5,
        TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Mcp),
        TrustedNoteRevisionProvenance::default(),
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
                TrustedNoteRevisionProvenance::default(),
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
    let columns: Vec<String> = sqlx::query_scalar("SELECT column_name FROM information_schema.columns WHERE table_name = 'note_revision_events' ORDER BY ordinal_position").fetch_all(db.pool()).await.unwrap();
    assert_eq!(
        columns,
        [
            "id",
            "project_id",
            "note_id",
            "note_seq",
            "event_kind",
            "content_before",
            "content_after",
            "confidence_before",
            "confidence_after",
            "actor_kind",
            "actor_id",
            "subsystem",
            "session_id",
            "task_id",
            "task_run_id",
            "reason",
            "created_at"
        ]
    );
    let constraints: Vec<String> = sqlx::query_scalar("SELECT constraint_name FROM information_schema.table_constraints WHERE table_name = 'note_revision_events' ORDER BY constraint_name").fetch_all(db.pool()).await.unwrap();
    for name in [
        "chk_note_revision_events_actor_attribution",
        "chk_note_revision_events_actor_kind",
        "chk_note_revision_events_id_uuid",
        "chk_note_revision_events_kind",
        "chk_note_revision_events_note_identity",
        "chk_note_revision_events_reason_trimmed",
        "chk_note_revision_events_shape",
        "chk_note_revision_events_system_subsystem",
        "fk_note_revision_events_project",
        "note_revision_events_pkey",
    ] {
        assert!(
            constraints.iter().any(|actual| actual == name),
            "missing {name}"
        );
    }
    let indexes: Vec<String> = sqlx::query_scalar("SELECT indexname FROM pg_indexes WHERE tablename = 'note_revision_events' ORDER BY indexname").fetch_all(db.pool()).await.unwrap();
    for name in [
        "note_revision_events_project_note_seq_unique",
        "note_revision_events_project_note_history",
        "note_revision_events_project_session_cursor",
        "note_revision_events_project_task_cursor",
        "note_revision_events_project_task_run_cursor",
    ] {
        assert!(
            indexes.iter().any(|actual| actual == name),
            "missing {name}"
        );
    }
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM information_schema.table_constraints tc JOIN information_schema.constraint_column_usage ccu ON tc.constraint_name = ccu.constraint_name WHERE tc.table_name = 'note_revision_events' AND tc.constraint_type = 'FOREIGN KEY' AND ccu.table_name = 'notes'").fetch_one(db.pool()).await.unwrap(), 0);
    assert!(
        NoteRevisionReason::new(" \t\n ").is_err(),
        "boundary rejects blank reasons"
    );
    assert!(sqlx::query("INSERT INTO note_revision_events (id, project_id, event_kind, actor_kind, subsystem, session_id, reason) VALUES ($1, $2, 'extraction_skipped', 'system', 'extraction', 'session', '   ')").bind(uuid::Uuid::now_v7().to_string()).bind(&project).execute(db.pool()).await.is_err(), "database rejects whitespace reasons");
    let events_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM note_revision_events WHERE note_id = $1")
            .bind(&note)
            .fetch_one(db.pool())
            .await
            .unwrap();
    repo.set_revision_event_insertion_failure_for_test(true);
    assert!(
        repo.mutate_with_revision(update(
            &project,
            &note,
            NoteRevisionEventKind::Updated,
            "must roll back",
            0.5,
            TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Enrichment),
            TrustedNoteRevisionProvenance::default(),
            " forced failure "
        ))
        .await
        .is_err()
    );
    repo.set_revision_event_insertion_failure_for_test(false);
    assert!(
        sqlx::query_scalar::<_, String>("SELECT content FROM notes WHERE id = $1")
            .bind(&note)
            .fetch_one(db.pool())
            .await
            .unwrap()
            .starts_with("content ")
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM note_revision_events WHERE note_id = $1"
        )
        .bind(&note)
        .fetch_one(db.pool())
        .await
        .unwrap(),
        events_before
    );
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
    ProjectRepository::new(db.clone(), EventBus::noop())
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
