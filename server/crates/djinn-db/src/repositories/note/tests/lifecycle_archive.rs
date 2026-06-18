//! Tests for archive-candidate selection and status flipping.

use std::collections::HashSet;

use tokio::sync::broadcast;

use crate::database::Database;
use crate::repositories::note::NoteRepository;
use crate::repositories::test_support::{event_bus_for, make_project};

const TEST_WINDOW_DAYS: u32 = 30;
const ARCHIVE_SHAPED_BODY: &str = "One short extracted paragraph.\n\n*Extracted from session 019ed7e1-980f-7ea2-935e-6f5e9fc82c14.*";

async fn setup() -> (Database, NoteRepository, String) {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let _ = tmp;
    (db, repo, project.id)
}

async fn mark_recently_accessed(db: &Database, note_id: &str) {
    sqlx::query(
        r#"UPDATE notes
           SET access_count = 1,
               last_accessed = to_char(now() at time zone 'utc', 'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"')
           WHERE id = $1"#,
    )
    .bind(note_id)
    .execute(db.pool())
    .await
    .unwrap();
}

async fn mark_old_access(db: &Database, note_id: &str, days: u32) {
    sqlx::query(
        r#"UPDATE notes
           SET access_count = 1,
               last_accessed = to_char(
                   (now() at time zone 'utc') - ($2 || ' days')::interval,
                   'YYYY-MM-DD"T"HH24:MI:SS.MS"Z"'
               )
           WHERE id = $1"#,
    )
    .bind(note_id)
    .bind(days.to_string())
    .execute(db.pool())
    .await
    .unwrap();
}

async fn mark_missing_last_access(db: &Database, note_id: &str) {
    sqlx::query(
        r#"UPDATE notes
           SET access_count = 1,
               last_accessed = NULL
           WHERE id = $1"#,
    )
    .bind(note_id)
    .execute(db.pool())
    .await
    .unwrap();
}

async fn set_status(db: &Database, note_id: &str, status: &str) {
    sqlx::query("UPDATE notes SET status = $1 WHERE id = $2")
        .bind(status)
        .bind(note_id)
        .execute(db.pool())
        .await
        .unwrap();
}

async fn note_status(db: &Database, note_id: &str) -> String {
    sqlx::query_scalar("SELECT status FROM notes WHERE id = $1")
        .bind(note_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extracted_archive_candidates_require_active_extracted_audit_candidate_and_no_recent_access()
 {
    let (db, repo, project_id) = setup().await;

    let zero_access = repo
        .create(
            &project_id,
            "Zero Access Case",
            ARCHIVE_SHAPED_BODY,
            "case",
            "[]",
        )
        .await
        .unwrap();
    let old_access = repo
        .create(
            &project_id,
            "Old Access Pattern",
            ARCHIVE_SHAPED_BODY,
            "pattern",
            "[]",
        )
        .await
        .unwrap();
    mark_old_access(&db, &old_access.id, TEST_WINDOW_DAYS + 5).await;

    let missing_last_access = repo
        .create(
            &project_id,
            "Missing Last Access Pitfall",
            ARCHIVE_SHAPED_BODY,
            "pitfall",
            "[]",
        )
        .await
        .unwrap();
    mark_missing_last_access(&db, &missing_last_access.id).await;

    let recent_access = repo
        .create(
            &project_id,
            "Recent Access Case",
            ARCHIVE_SHAPED_BODY,
            "case",
            "[]",
        )
        .await
        .unwrap();
    mark_recently_accessed(&db, &recent_access.id).await;

    let not_archive_shaped = repo
        .create(
            &project_id,
            "Durable Case",
            "## Situation\nA sufficiently described situation.\n\n## Constraint\nA concrete constraint.\n\n## Approach taken\nA reusable approach.\n\n## Result\nA result.\n\n## Why it worked / failed\nA rationale.\n\n## Reusable lesson\nA lesson.\n\n## Related\nNone.",
            "case",
            "[]",
        )
        .await
        .unwrap();

    let archived = repo
        .create(
            &project_id,
            "Already Archived Case",
            ARCHIVE_SHAPED_BODY,
            "case",
            "[]",
        )
        .await
        .unwrap();
    set_status(&db, &archived.id, "archived").await;

    let candidates = repo
        .extracted_archive_candidates(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();
    let candidate_ids = candidates
        .iter()
        .map(|finding| finding.note_id.as_str())
        .collect::<HashSet<_>>();

    assert!(candidate_ids.contains(zero_access.id.as_str()));
    assert!(candidate_ids.contains(old_access.id.as_str()));
    assert!(candidate_ids.contains(missing_last_access.id.as_str()));
    assert!(!candidate_ids.contains(recent_access.id.as_str()));
    assert!(!candidate_ids.contains(not_archive_shaped.id.as_str()));
    assert!(!candidate_ids.contains(archived.id.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn extracted_archive_candidates_exclude_hand_written_note_types_by_predicate() {
    let (_db, repo, project_id) = setup().await;

    let adr = repo
        .create(
            &project_id,
            "Archive Shaped ADR",
            ARCHIVE_SHAPED_BODY,
            "adr",
            "[]",
        )
        .await
        .unwrap();

    let candidates = repo
        .extracted_archive_candidates(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();

    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.note_id != adr.id),
        "hand-written ADR must not be selected even with archive-shaped content"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_audit_candidates_flips_only_active_candidates_to_archived() {
    let (db, repo, project_id) = setup().await;

    let eligible_case = repo
        .create(
            &project_id,
            "Eligible Case",
            ARCHIVE_SHAPED_BODY,
            "case",
            "[]",
        )
        .await
        .unwrap();
    let eligible_pitfall = repo
        .create(
            &project_id,
            "Eligible Pitfall",
            ARCHIVE_SHAPED_BODY,
            "pitfall",
            "[]",
        )
        .await
        .unwrap();
    let recent_access = repo
        .create(
            &project_id,
            "Recent Access Candidate",
            ARCHIVE_SHAPED_BODY,
            "case",
            "[]",
        )
        .await
        .unwrap();
    mark_recently_accessed(&db, &recent_access.id).await;

    let archived_count = repo
        .archive_audit_candidates(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();

    assert_eq!(archived_count, 2);
    assert_eq!(note_status(&db, &eligible_case.id).await, "archived");
    assert_eq!(note_status(&db, &eligible_pitfall.id).await, "archived");
    assert_eq!(note_status(&db, &recent_access.id).await, "active");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_audit_candidates_is_idempotent_for_already_archived_candidates() {
    let (db, repo, project_id) = setup().await;

    let already_archived = repo
        .create(
            &project_id,
            "Already Archived Candidate",
            ARCHIVE_SHAPED_BODY,
            "case",
            "[]",
        )
        .await
        .unwrap();
    set_status(&db, &already_archived.id, "archived").await;

    let archived_count = repo
        .archive_audit_candidates(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();

    assert_eq!(archived_count, 0);
    assert_eq!(note_status(&db, &already_archived.id).await, "archived");
}
