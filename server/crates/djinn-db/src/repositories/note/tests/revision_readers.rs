//! Repository-level contract tests for the bounded revision read boundary.
//!
//! These tests objectively cover pagination boundaries/ties, malformed cursors,
//! deleted-note history, empty pre-cutover live-note history, and cross-project
//! isolation using existing ledger fixtures and `mutate_with_revision` writers.

use super::*;
use crate::ProjectRepository;
use djinn_core::events::EventBus;

const FIXTURE: &str =
    include_str!("../../../../../djinn-control-plane/tests/fixtures/memory_revision_contract.json");

const EQUAL_TIME_CREATED_AT: &str = "2026-01-01T00:00:00.000Z";
const EQUAL_TIME_EVENT_IDS: [&str; 4] = [
    "00000000-0000-7000-8000-000000000001",
    "00000000-0000-7000-8000-000000000002",
    "00000000-0000-7000-8000-000000000003",
    "00000000-0000-7000-8000-000000000004",
];
const EQUAL_TIME_SESSION_ID: &str = "00000000-0000-7000-8000-000000000005";
const EQUAL_TIME_TASK_RUN_ID: &str = "00000000-0000-7000-8000-000000000006";

fn fixture() -> serde_json::Value {
    serde_json::from_str(FIXTURE).expect("valid contract fixture")
}

fn reason(input: &str) -> NoteRevisionReason {
    NoteRevisionReason::new(input).expect("fixture reason")
}

fn attribution(row: &serde_json::Value) -> TrustedNoteRevisionAttribution {
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

fn provenance(row: &serde_json::Value) -> TrustedNoteRevisionProvenance {
    TrustedNoteRevisionProvenance::new(
        row["session_id"].as_str().map(str::to_owned),
        row["task_id"].as_str().map(str::to_owned),
        row["task_run_id"].as_str().map(str::to_owned),
    )
    .unwrap()
}

fn create_cmd(
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

fn update_cmd(
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
        .create_with_id(project_id, "revision-readers", "test", "revision-readers")
        .await
        .expect("project");
    (db.clone(), NoteRepository::new(db, EventBus::noop()))
}

/// Seed the full fixture ledger into one project, returning the project ID
/// and the primary note's first revision ID.
async fn seed_full_fixture() -> (String, Database, NoteRepository) {
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

    // Create primary, update it, confidence bump it.
    repo.mutate_with_revision(create_cmd(
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
    repo.mutate_with_revision(update_cmd(
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
    repo.mutate_with_revision(update_cmd(
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

    // Create already_known, confidence bump it.
    repo.mutate_with_revision(create_cmd(
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
    repo.mutate_with_revision(update_cmd(
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

    // Create merge_target, update it.
    repo.mutate_with_revision(create_cmd(
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
    repo.mutate_with_revision(update_cmd(
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

    // Extraction skipped (session-scoped, no note).
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

    // Unscoped create (no session/task-run provenance).
    repo.mutate_with_revision(create_cmd(
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

    (project, db, repo)
}

/// Insert controlled-time rows as an immutable ledger fixture. Supplying
/// `created_at` at insert time exercises timestamp ties without updating rows
/// or disabling the append-only trigger.
async fn seed_equal_time_scoped_events() -> (String, Database, NoteRepository) {
    let project = uuid::Uuid::now_v7().to_string();
    let (db, repo) = setup(&project).await;

    for id in EQUAL_TIME_EVENT_IDS {
        sqlx::query(
            "INSERT INTO note_revision_events \
             (id, project_id, event_kind, actor_kind, subsystem, session_id, task_run_id, reason, created_at) \
             VALUES ($1, $2, 'extraction_skipped', 'system', 'extraction', $3, $4, $5, $6)",
        )
        .bind(id)
        .bind(&project)
        .bind(EQUAL_TIME_SESSION_ID)
        .bind(EQUAL_TIME_TASK_RUN_ID)
        .bind(format!("equal-time pagination fixture {id}"))
        .bind(EQUAL_TIME_CREATED_AT)
        .execute(db.pool())
        .await
        .expect("insert immutable equal-time ledger fixture");
    }

    (project, db, repo)
}

// ── Note history pagination ──────────────────────────────────────────────────

#[tokio::test]
async fn note_history_returns_newest_first_by_note_seq() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    let page = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: primary,
            limit: 100,
            before: None,
        })
        .await
        .unwrap();

    // Primary has 3 events at this point (create, update, confidence_bump).
    // The delete hasn't been seeded yet.
    assert_eq!(page.events.len(), 3);
    // Newest-first by note_seq.
    assert_eq!(page.events[0].note_seq, Some(3));
    assert_eq!(page.events[1].note_seq, Some(2));
    assert_eq!(page.events[2].note_seq, Some(1));
    // All events are for the right note.
    assert!(
        page.events
            .iter()
            .all(|e| e.note_id.as_deref() == Some(primary))
    );
    assert!(page.note_exists, "live note row exists");
    assert!(page.next_cursor.is_none(), "no next cursor when all fit");
}

#[tokio::test]
async fn note_history_pagination_boundaries_and_cursor_round_trip() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    // Request limit=2 from 3 events.
    let page1 = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: primary,
            limit: 2,
            before: None,
        })
        .await
        .unwrap();
    assert_eq!(page1.events.len(), 2);
    assert_eq!(page1.events[0].note_seq, Some(3));
    assert_eq!(page1.events[1].note_seq, Some(2));
    assert!(page1.next_cursor.is_some(), "more pages available");

    let cursor = page1.next_cursor.unwrap();
    let page2 = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: primary,
            limit: 2,
            before: Some(&cursor),
        })
        .await
        .unwrap();
    assert_eq!(page2.events.len(), 1);
    assert_eq!(page2.events[0].note_seq, Some(1));
    assert!(page2.next_cursor.is_none(), "no more pages");
}

#[tokio::test]
async fn note_history_pagination_does_not_overshoot_limit_boundary() {
    // Exactly limit rows: next_cursor should be None.
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    let page = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: primary,
            limit: 3,
            before: None,
        })
        .await
        .unwrap();
    assert_eq!(page.events.len(), 3);
    assert!(page.next_cursor.is_none(), "exactly limit, no next cursor");
}

#[tokio::test]
async fn note_history_limit_is_clamped_to_max() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    let page = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: primary,
            limit: 0, // clamp to 1
            before: None,
        })
        .await
        .unwrap();
    assert_eq!(page.events.len(), 1);
}

#[tokio::test]
async fn note_history_malformed_cursor_returns_error() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    let result = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: primary,
            limit: 10,
            before: Some("not-a-valid-cursor!!!"),
        })
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn note_history_wrong_view_cursor_returns_error() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    // Encode a session cursor and try to use it for note history.
    let session_cursor = RevisionCursor::encode_session("2026-01-01T00:00:00.000Z", "some-id");
    let result = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: primary,
            limit: 10,
            before: Some(&session_cursor),
        })
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn note_history_retained_for_deleted_note() {
    let (project, db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    // Delete the primary note.
    repo.mutate_with_revision(NoteRevisionMutation {
        project_id: project.clone(),
        note_id: Some(primary.into()),
        event_kind: NoteRevisionEventKind::Deleted,
        desired: NoteRevisionDesiredState::Delete,
        attribution: TrustedNoteRevisionAttribution::system(NoteRevisionSubsystem::Mcp),
        provenance: TrustedNoteRevisionProvenance::default(),
        reason: reason("delete for history retention test"),
    })
    .await
    .unwrap();

    // Confirm the note row is gone.
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM notes WHERE id = $1 AND project_id = $2)")
            .bind(primary)
            .bind(&project)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert!(!exists, "note row removed");

    // History still accessible, now including the delete (4 events total).
    let page = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: primary,
            limit: 100,
            before: None,
        })
        .await
        .unwrap();
    assert_eq!(page.events.len(), 4, "deleted-note history retained");
    assert_eq!(page.events[0].event_kind, NoteRevisionEventKind::Deleted);
    assert_eq!(page.events[0].note_seq, Some(4));
    assert!(!page.note_exists, "note_exists false for deleted note");
}

#[tokio::test]
async fn note_history_empty_for_pre_cutover_live_note() {
    let (project, db, repo) = seed_full_fixture().await;
    let f = fixture();
    let cutover = f["migration_cutover"]["pre_migration_note_id"]
        .as_str()
        .unwrap();

    // Insert a live note row directly without ledger events.
    sqlx::query(
        "INSERT INTO notes (id, project_id, permalink, title, file_path) \
         VALUES ($1, $2, 'cutover', 'Cutover', 'cutover.md')",
    )
    .bind(cutover)
    .bind(&project)
    .execute(db.pool())
    .await
    .unwrap();

    let page = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: cutover,
            limit: 100,
            before: None,
        })
        .await
        .unwrap();
    assert!(page.events.is_empty(), "no events for pre-migration note");
    assert!(
        page.note_exists,
        "note_exists true for live pre-migration note"
    );
    assert!(page.next_cursor.is_none());
}

#[tokio::test]
async fn note_history_empty_for_unknown_note_and_note_exists_false() {
    let (project, _db, repo) = seed_full_fixture().await;
    let unknown = uuid::Uuid::now_v7().to_string();

    let page = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: &unknown,
            limit: 100,
            before: None,
        })
        .await
        .unwrap();
    assert!(page.events.is_empty());
    assert!(!page.note_exists, "note_exists false for unknown note");
}

// ── Cross-project isolation ──────────────────────────────────────────────────

#[tokio::test]
async fn note_history_cross_project_isolation() {
    let (project_a, _db_a, repo_a) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    // Create a second project.
    let project_b = uuid::Uuid::now_v7().to_string();
    let (_db_b, repo_b) = setup(&project_b).await;

    // Query project B for project A's note ID — must return empty.
    let page = repo_b
        .note_revision_history(NoteHistoryRequest {
            project_id: &project_b,
            note_id: primary,
            limit: 100,
            before: None,
        })
        .await
        .unwrap();
    assert!(page.events.is_empty());

    // Verify project A still returns events.
    let page_a = repo_a
        .note_revision_history(NoteHistoryRequest {
            project_id: &project_a,
            note_id: primary,
            limit: 100,
            before: None,
        })
        .await
        .unwrap();
    assert!(!page_a.events.is_empty());
}

// ── Revision lookup ──────────────────────────────────────────────────────────

#[tokio::test]
async fn revision_lookup_returns_scoped_event() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    // Get the first event for the primary note.
    let page = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: primary,
            limit: 100,
            before: None,
        })
        .await
        .unwrap();
    let first_event_id = &page.events.last().unwrap().id; // seq=1 is last in descending order

    let looked_up = repo
        .revision_lookup(RevisionLookupRequest {
            project_id: &project,
            note_id: primary,
            revision_id: first_event_id,
        })
        .await
        .unwrap();
    assert!(looked_up.is_some());
    let row = looked_up.unwrap();
    assert_eq!(row.id, *first_event_id);
    assert_eq!(row.note_seq, Some(1));
    assert_eq!(row.event_kind, NoteRevisionEventKind::Created);
}

#[tokio::test]
async fn revision_lookup_returns_none_for_wrong_note() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();
    let known = f["ids"]["notes"]["already_known"].as_str().unwrap();

    // Get a revision from the primary note.
    let page = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: primary,
            limit: 100,
            before: None,
        })
        .await
        .unwrap();
    let primary_revision = &page.events[0].id;

    // Try to look it up scoped to a different note — must return None.
    let result = repo
        .revision_lookup(RevisionLookupRequest {
            project_id: &project,
            note_id: known,
            revision_id: primary_revision,
        })
        .await
        .unwrap();
    assert!(result.is_none(), "cross-note lookup must not disclose");
}

#[tokio::test]
async fn revision_lookup_returns_none_for_wrong_project() {
    let (project_a, _db_a, repo_a) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    // Get a revision from project A.
    let page = repo_a
        .note_revision_history(NoteHistoryRequest {
            project_id: &project_a,
            note_id: primary,
            limit: 100,
            before: None,
        })
        .await
        .unwrap();
    let revision_id = &page.events[0].id;

    // Try to look it up in a different project.
    let project_b = uuid::Uuid::now_v7().to_string();
    let (_db_b, repo_b) = setup(&project_b).await;
    let result = repo_b
        .revision_lookup(RevisionLookupRequest {
            project_id: &project_b,
            note_id: primary,
            revision_id: revision_id,
        })
        .await
        .unwrap();
    assert!(result.is_none(), "cross-project lookup must not disclose");
}

#[tokio::test]
async fn revision_lookup_returns_none_for_unknown_id() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();
    let unknown = uuid::Uuid::now_v7().to_string();

    let result = repo
        .revision_lookup(RevisionLookupRequest {
            project_id: &project,
            note_id: primary,
            revision_id: &unknown,
        })
        .await
        .unwrap();
    assert!(result.is_none());
}

// ── Revision range ───────────────────────────────────────────────────────────

#[tokio::test]
async fn revision_range_includes_endpoints_and_intervening() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    // Primary note has seq 1=created, 2=updated, 3=confidence_changed.
    // Range from 1 to 3 should include all three.
    let range = repo
        .revision_range(RevisionRangeRequest {
            project_id: &project,
            note_id: primary,
            from_seq: 1,
            to_seq: 3,
        })
        .await
        .unwrap();
    assert_eq!(range.len(), 3);
    // Newest-first ordering.
    assert_eq!(range[0].note_seq, Some(3));
    assert_eq!(range[1].note_seq, Some(2));
    assert_eq!(range[2].note_seq, Some(1));

    // Range includes non-content events (confidence_changed has no content).
    assert_eq!(
        range[0].event_kind,
        NoteRevisionEventKind::ConfidenceChanged
    );
    assert!(range[0].snapshot.content_before.is_none());
    assert!(range[0].snapshot.content_after.is_none());

    // Content events have snapshots.
    assert_eq!(range[2].event_kind, NoteRevisionEventKind::Created);
    assert!(range[2].snapshot.content_after.is_some());
}

#[tokio::test]
async fn revision_range_swaps_endpoints_if_reversed() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    // Pass to_seq < from_seq — should still work.
    let range = repo
        .revision_range(RevisionRangeRequest {
            project_id: &project,
            note_id: primary,
            from_seq: 3,
            to_seq: 1,
        })
        .await
        .unwrap();
    assert_eq!(range.len(), 3);
}

#[tokio::test]
async fn revision_range_single_endpoint() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    let range = repo
        .revision_range(RevisionRangeRequest {
            project_id: &project,
            note_id: primary,
            from_seq: 2,
            to_seq: 2,
        })
        .await
        .unwrap();
    assert_eq!(range.len(), 1);
    assert_eq!(range[0].note_seq, Some(2));
}

#[tokio::test]
async fn revision_range_cross_project_isolation() {
    let (_project_a, _db_a, _repo_a) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    let project_b = uuid::Uuid::now_v7().to_string();
    let (_db_b, repo_b) = setup(&project_b).await;

    let range = repo_b
        .revision_range(RevisionRangeRequest {
            project_id: &project_b,
            note_id: primary,
            from_seq: 1,
            to_seq: 5,
        })
        .await
        .unwrap();
    assert!(range.is_empty(), "cross-project range must be empty");
}

// ── Session/task-run history ─────────────────────────────────────────────────

#[tokio::test]
async fn session_history_returns_newest_first_with_all_event_kinds() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let session_id = f["ids"]["session_id"].as_str().unwrap();

    let page = repo
        .session_revision_history(
            session_id,
            SessionRevisionRequest {
                project_id: &project,
                limit: 100,
                before: None,
            },
        )
        .await
        .unwrap();

    // All seeded events except the unscoped note have the fixture session_id.
    assert_eq!(page.events.len(), 8);
    // Includes extraction_skipped (no note_id).
    assert!(
        page.events
            .iter()
            .any(|e| e.event_kind == NoteRevisionEventKind::ExtractionSkipped),
        "session history includes extraction_skipped"
    );
    // Includes multiple note creates.
    assert!(
        page.events
            .iter()
            .any(|e| e.event_kind == NoteRevisionEventKind::Created),
        "session history includes created"
    );
    assert!(page.next_cursor.is_none(), "all events fit");
}

#[tokio::test]
async fn session_history_pagination_cursor_round_trip() {
    let (project, _db, repo) = seed_equal_time_scoped_events().await;
    let expected_ids: Vec<&str> = EQUAL_TIME_EVENT_IDS.iter().rev().copied().collect();

    let page1 = repo
        .session_revision_history(
            EQUAL_TIME_SESSION_ID,
            SessionRevisionRequest {
                project_id: &project,
                limit: 2,
                before: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page1
            .events
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        expected_ids[..2]
    );
    assert!(
        page1
            .events
            .iter()
            .all(|event| event.created_at == EQUAL_TIME_CREATED_AT)
    );
    assert!(page1.next_cursor.is_some());

    let cursor = page1.next_cursor.unwrap();
    let page2 = repo
        .session_revision_history(
            EQUAL_TIME_SESSION_ID,
            SessionRevisionRequest {
                project_id: &project,
                limit: 2,
                before: Some(&cursor),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page2
            .events
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        expected_ids[2..]
    );
    assert!(page2.next_cursor.is_none());

    let all_ids: Vec<&str> = page1
        .events
        .iter()
        .chain(page2.events.iter())
        .map(|event| event.id.as_str())
        .collect();
    assert_eq!(all_ids, expected_ids);
    let unique: std::collections::HashSet<&str> = all_ids.iter().copied().collect();
    assert_eq!(unique.len(), EQUAL_TIME_EVENT_IDS.len());
}

#[tokio::test]
async fn session_history_malformed_cursor_returns_error() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let session_id = f["ids"]["session_id"].as_str().unwrap();

    let result = repo
        .session_revision_history(
            session_id,
            SessionRevisionRequest {
                project_id: &project,
                limit: 10,
                before: Some("garbage!!!"),
            },
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn session_history_wrong_view_cursor_returns_error() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let session_id = f["ids"]["session_id"].as_str().unwrap();

    // Encode a note-history cursor and try to use it for session history.
    let note_cursor = RevisionCursor::encode_note_history(5);
    let result = repo
        .session_revision_history(
            session_id,
            SessionRevisionRequest {
                project_id: &project,
                limit: 10,
                before: Some(&note_cursor),
            },
        )
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn session_history_excludes_unscoped_events() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let session_id = f["ids"]["session_id"].as_str().unwrap();
    let unscoped = f["ids"]["notes"]["unscoped"].as_str().unwrap();

    // The unscoped note has no session_id, so it should not appear in session history.
    let page = repo
        .session_revision_history(
            session_id,
            SessionRevisionRequest {
                project_id: &project,
                limit: 100,
                before: None,
            },
        )
        .await
        .unwrap();
    assert!(
        !page
            .events
            .iter()
            .any(|e| e.note_id.as_deref() == Some(unscoped)),
        "unscoped event excluded from session history"
    );
}

#[tokio::test]
async fn session_history_cross_project_isolation() {
    let (_project_a, _db_a, _repo_a) = seed_full_fixture().await;
    let f = fixture();
    let session_id = f["ids"]["session_id"].as_str().unwrap();

    let project_b = uuid::Uuid::now_v7().to_string();
    let (_db_b, repo_b) = setup(&project_b).await;

    let page = repo_b
        .session_revision_history(
            session_id,
            SessionRevisionRequest {
                project_id: &project_b,
                limit: 100,
                before: None,
            },
        )
        .await
        .unwrap();
    assert!(page.events.is_empty(), "cross-project session isolation");
}

#[tokio::test]
async fn task_run_history_returns_events_with_cursor_pagination() {
    let (project, _db, repo) = seed_equal_time_scoped_events().await;
    let expected_ids: Vec<&str> = EQUAL_TIME_EVENT_IDS.iter().rev().copied().collect();

    let page1 = repo
        .task_run_revision_history(
            EQUAL_TIME_TASK_RUN_ID,
            SessionRevisionRequest {
                project_id: &project,
                limit: 2,
                before: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page1
            .events
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        expected_ids[..2]
    );
    assert!(
        page1
            .events
            .iter()
            .all(|event| event.created_at == EQUAL_TIME_CREATED_AT)
    );
    assert!(page1.next_cursor.is_some());

    let cursor = page1.next_cursor.unwrap();
    let page2 = repo
        .task_run_revision_history(
            EQUAL_TIME_TASK_RUN_ID,
            SessionRevisionRequest {
                project_id: &project,
                limit: 2,
                before: Some(&cursor),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        page2
            .events
            .iter()
            .map(|event| event.id.as_str())
            .collect::<Vec<_>>(),
        expected_ids[2..]
    );
    assert!(page2.next_cursor.is_none());

    let all_ids: Vec<&str> = page1
        .events
        .iter()
        .chain(page2.events.iter())
        .map(|event| event.id.as_str())
        .collect();
    assert_eq!(all_ids, expected_ids);
    let unique: std::collections::HashSet<&str> = all_ids.iter().copied().collect();
    assert_eq!(unique.len(), EQUAL_TIME_EVENT_IDS.len());
}

#[tokio::test]
async fn task_run_history_empty_for_unknown_task_run() {
    let (project, _db, repo) = seed_full_fixture().await;
    let unknown = uuid::Uuid::now_v7().to_string();

    let page = repo
        .task_run_revision_history(
            &unknown,
            SessionRevisionRequest {
                project_id: &project,
                limit: 100,
                before: None,
            },
        )
        .await
        .unwrap();
    assert!(page.events.is_empty());
    assert!(page.next_cursor.is_none());
}

// ── Cursor type tests ─────────────────────────────────────────────────────────

#[test]
fn note_history_cursor_round_trips() {
    let encoded = RevisionCursor::encode_note_history(42);
    let decoded = RevisionCursor::decode_note_history(&encoded).unwrap();
    assert_eq!(decoded, RevisionCursor::NoteHistory { note_seq: 42 });
}

#[test]
fn session_cursor_round_trips() {
    let encoded = RevisionCursor::encode_session("2026-01-01T00:00:00.000Z", "abc-123");
    let decoded = RevisionCursor::decode_session(&encoded).unwrap();
    assert_eq!(
        decoded,
        RevisionCursor::Session {
            created_at: "2026-01-01T00:00:00.000Z".to_owned(),
            id: "abc-123".to_owned(),
        }
    );
}

#[test]
fn cursor_cross_view_rejected() {
    let note_cursor = RevisionCursor::encode_note_history(42);
    assert!(RevisionCursor::decode_session(&note_cursor).is_err());

    let session_cursor = RevisionCursor::encode_session("2026-01-01T00:00:00.000Z", "abc");
    assert!(RevisionCursor::decode_note_history(&session_cursor).is_err());
}

#[test]
fn malformed_cursor_rejected() {
    assert!(RevisionCursor::decode_note_history("!!!invalid").is_err());
    assert!(RevisionCursor::decode_session("!!!invalid").is_err());
    assert!(RevisionCursor::decode_note_history("").is_err());
}

// ── Typed metadata ────────────────────────────────────────────────────────────

#[tokio::test]
async fn read_rows_carry_full_typed_metadata() {
    let (project, _db, repo) = seed_full_fixture().await;
    let f = fixture();
    let primary = f["ids"]["notes"]["primary"].as_str().unwrap();

    let page = repo
        .note_revision_history(NoteHistoryRequest {
            project_id: &project,
            note_id: primary,
            limit: 100,
            before: None,
        })
        .await
        .unwrap();

    // The first event (seq=3, confidence_changed) should have non-content metadata.
    let conf_event = &page.events[0];
    assert_eq!(
        conf_event.event_kind,
        NoteRevisionEventKind::ConfidenceChanged
    );
    assert_eq!(conf_event.snapshot.content_before, None);
    assert_eq!(conf_event.snapshot.content_after, None);
    assert_eq!(conf_event.snapshot.confidence_before, Some(0.5));
    assert_eq!(conf_event.snapshot.confidence_after, Some(0.75));
    assert!(!conf_event.reason.as_str().is_empty());
    assert!(!conf_event.created_at.is_empty());

    // The created event (seq=1) should have content metadata.
    let created = page.events.last().unwrap();
    assert_eq!(created.event_kind, NoteRevisionEventKind::Created);
    assert!(created.snapshot.content_before.is_none());
    assert!(created.snapshot.content_after.is_some());
    assert_eq!(created.snapshot.confidence_before, None);
    assert_eq!(created.snapshot.confidence_after, Some(0.5));

    // Attribution metadata.
    assert_eq!(
        conf_event.attribution.actor_kind(),
        NoteRevisionActorKind::System
    );
    assert_eq!(conf_event.attribution.subsystem(), Some("enrichment"));
}
