//! Tests for archive-candidate selection and status flipping.

use std::collections::HashSet;

use tokio::sync::broadcast;

use crate::database::Database;
use crate::repositories::note::{NoteRepository, NoteSearchParams};
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

async fn mark_very_old_access(db: &Database, note_id: &str) {
    // Simulate a note whose last access is far outside any reasonable window.
    // This exercises the `last_accessed < to_char(...)` branch of the
    // staleness predicate in `extracted_archive_candidates`.  The column is
    // `NOT NULL`, so we use a real (very old) timestamp rather than NULL or
    // empty string.
    mark_old_access(db, note_id, 9999).await;
}

async fn mark_empty_last_access(db: &Database, note_id: &str) {
    // The postgres schema makes `last_accessed` NOT NULL, so missing access is
    // represented defensively with an empty string in tests instead of NULL.
    sqlx::query("UPDATE notes SET access_count = 1, last_accessed = '' WHERE id = $1")
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

    let very_old_access = repo
        .create(
            &project_id,
            "Very Old Access Pitfall",
            ARCHIVE_SHAPED_BODY,
            "pitfall",
            "[]",
        )
        .await
        .unwrap();
    mark_very_old_access(&db, &very_old_access.id).await;

    let empty_last_access = repo
        .create(
            &project_id,
            "Empty Last Access Pitfall",
            ARCHIVE_SHAPED_BODY,
            "pitfall",
            "[]",
        )
        .await
        .unwrap();
    mark_empty_last_access(&db, &empty_last_access.id).await;

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
    assert!(candidate_ids.contains(very_old_access.id.as_str()));
    assert!(candidate_ids.contains(empty_last_access.id.as_str()));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn archive_sweep_archived_notes_are_hidden_from_retrieval_but_listable_by_status() {
    let (_db, repo, project_id) = setup().await;

    let seed = repo
        .create(
            &project_id,
            "Archive Retrieval Seed",
            "Seed note about archive retrieval sentinel context.",
            "adr",
            "[]",
        )
        .await
        .unwrap();

    let archived = repo
        .create(
            &project_id,
            "Archived Retrieval Sentinel",
            "archive retrieval sentinel content linked to [[Archive Retrieval Seed]].\n\n*Extracted from session 019ed7e1-980f-7ea2-935e-6f5e9fc82c14.*",
            "case",
            "[]",
        )
        .await
        .unwrap();

    let active_peer = repo
        .create(
            &project_id,
            "Active Retrieval Sentinel",
            "archive retrieval sentinel active peer linked to [[Archive Retrieval Seed]] with enough detail to stay active.",
            "case",
            "[]",
        )
        .await
        .unwrap();

    let archived_count = repo
        .archive_audit_candidates(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();
    assert_eq!(archived_count, 1);

    let context = repo
        .build_context(
            &project_id,
            &seed.permalink,
            Some(8192),
            None,
            20,
            Some(0.0),
        )
        .await
        .unwrap();
    let context_ids = context
        .related_l1
        .iter()
        .map(|note| note.id.as_str())
        .chain(context.related_l0.iter().map(|note| note.id.as_str()))
        .collect::<HashSet<_>>();
    assert!(
        !context_ids.contains(archived.id.as_str()),
        "archived sweep note must not be injected into build_context"
    );

    let search_results = repo
        .search(NoteSearchParams {
            project_id: &project_id,
            query: "archive retrieval sentinel",
            task_id: None,
            folder: None,
            note_type: None,
            limit: 20,
            semantic_scores: None,
        })
        .await
        .unwrap();
    let search_ids = search_results
        .iter()
        .map(|note| note.id.as_str())
        .collect::<HashSet<_>>();
    assert!(
        !search_ids.contains(archived.id.as_str()),
        "archived sweep note must not be returned by default prompt retrieval search"
    );
    assert!(
        search_ids.contains(active_peer.id.as_str()),
        "active peer proves the retrieval query itself still returns live notes"
    );

    let default_list = repo.list_compact(&project_id, None, None, 0, None).await.unwrap();
    assert!(
        default_list.iter().all(|note| note.id != archived.id),
        "default compact list is active-only and should hide archived notes"
    );

    let archived_list = repo
        .list_compact_by_status(&project_id, None, None, 0, Some("archived"))
        .await
        .unwrap();
    assert!(
        archived_list.iter().any(|note| note.id == archived.id),
        "explicit archived-status list path must keep archived notes visible/restorable"
    );
}
