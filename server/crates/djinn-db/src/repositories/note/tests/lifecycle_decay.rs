// Test-only: Instant::now is used for timing assertions; eprintln is used for
// benchmark diagnostics in this test module.
#![allow(clippy::disallowed_methods, clippy::print_stderr)]
//! Tests for [`NoteRepository::decay_stale_extracted_notes`].
//!
//! Covers the acceptance-criteria matrix:
//! - Decay eligibility for stale extracted notes.
//! - Decay ineligibility for hand-written types (safety boundary).
//! - Decay ineligibility for recently-accessed notes.
//! - Decay ineligibility for non-`active` status notes.
//! - Floor behaviour: a note at `CONFIDENCE_FLOOR` stays at the floor.
//! - Per-tick iteration cap holds under a 100-note fixture.

use std::time::Instant;

use tokio::sync::broadcast;

use crate::STALE_CITATION;
use crate::database::Database;
use crate::repositories::note::NoteRepository;
use crate::repositories::note::scoring::CONFIDENCE_FLOOR;
use crate::repositories::note::scoring::STALE_DECAY_SIGNAL;
use crate::repositories::note::scoring::bayesian_update;
use crate::repositories::test_support::{event_bus_for, make_project};

/// Default decay window used by the tests (matches the module default of 30
/// days). We pass this explicitly rather than relying on env so the tests are
/// deterministic regardless of host env.
const TEST_WINDOW_DAYS: u32 = 30;

/// An RFC3339-ish timestamp far in the past, guaranteed older than the decay
/// window.
const OLD_LAST_ACCESSED: &str = "2026-04-01T00:00:00.000Z";

/// Read the current confidence of a note directly from the DB.
async fn note_confidence(db: &Database, note_id: &str) -> f64 {
    sqlx::query_scalar("SELECT confidence FROM notes WHERE id = $1")
        .bind(note_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

/// Set `last_accessed`, `confidence`, and optionally `status` on a note row.
async fn patch_note(
    db: &Database,
    note_id: &str,
    last_accessed: &str,
    confidence: f64,
    status: Option<&str>,
) {
    sqlx::query("UPDATE notes SET last_accessed = $1, confidence = $2 WHERE id = $3")
        .bind(last_accessed)
        .bind(confidence)
        .bind(note_id)
        .execute(db.pool())
        .await
        .unwrap();
    if let Some(status) = status {
        sqlx::query("UPDATE notes SET status = $1 WHERE id = $2")
            .bind(status)
            .bind(note_id)
            .execute(db.pool())
            .await
            .unwrap();
    }
}

async fn setup() -> (Database, NoteRepository, String) {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let project = make_project(&db, tmp.path()).await;
    let repo = NoteRepository::new(db.clone(), event_bus_for(&tx));
    let _ = tmp;
    (db, repo, project.id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decay_eligibility_stale_extracted_note_crosses_below_stale_citation() {
    let (db, repo, project_id) = setup().await;

    let note = repo
        .create(&project_id, "Stale Case", "body", "case", "[]")
        .await
        .unwrap();
    patch_note(&db, &note.id, OLD_LAST_ACCESSED, 0.5, None).await;

    let decayed = repo
        .decay_stale_extracted_notes(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();

    // The note crossed below STALE_CITATION in this tick.
    assert_eq!(decayed, 1, "expected exactly one decayed note");

    let confidence = note_confidence(&db, &note.id).await;
    assert!(
        confidence <= STALE_CITATION,
        "expected confidence <= STALE_CITATION ({STALE_CITATION}), got {confidence}"
    );
    assert!(
        confidence >= CONFIDENCE_FLOOR,
        "expected confidence >= CONFIDENCE_FLOOR ({CONFIDENCE_FLOOR}), got {confidence}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decay_ineligibility_hand_written_type_unchanged() {
    let (db, repo, project_id) = setup().await;

    // An ADR (hand-written) with the same age and confidence as the eligible
    // case note must NOT be decayed.
    let adr = repo
        .create(&project_id, "Old ADR", "body", "adr", "[]")
        .await
        .unwrap();
    patch_note(&db, &adr.id, OLD_LAST_ACCESSED, 0.5, None).await;

    let decayed = repo
        .decay_stale_extracted_notes(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();

    assert_eq!(decayed, 0, "hand-written note must not be decayed");

    let confidence = note_confidence(&db, &adr.id).await;
    assert_eq!(
        confidence, 0.5,
        "hand-written note confidence must be unchanged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decay_ineligibility_recently_accessed_unchanged() {
    let (db, repo, project_id) = setup().await;

    let case = repo
        .create(&project_id, "Fresh Case", "body", "case", "[]")
        .await
        .unwrap();
    // Accessed 5 days ago — well within the 30-day window. Compute from the DB
    // clock so this test is immune to wall-clock date drift.
    let recent: String = sqlx::query_scalar(
        "SELECT to_char((now() at time zone 'utc') - interval '5 days', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    patch_note(&db, &case.id, &recent, 0.5, None).await;

    let decayed = repo
        .decay_stale_extracted_notes(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();

    assert_eq!(decayed, 0, "recently-accessed note must not be decayed");

    let confidence = note_confidence(&db, &case.id).await;
    assert_eq!(
        confidence, 0.5,
        "recently-accessed note confidence must be unchanged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decay_ineligibility_archived_status_unchanged() {
    let (db, repo, project_id) = setup().await;

    let case = repo
        .create(&project_id, "Archived Case", "body", "case", "[]")
        .await
        .unwrap();
    patch_note(&db, &case.id, OLD_LAST_ACCESSED, 0.5, Some("archived")).await;

    let decayed = repo
        .decay_stale_extracted_notes(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();

    assert_eq!(decayed, 0, "archived note must not be decayed");

    let confidence = note_confidence(&db, &case.id).await;
    assert_eq!(
        confidence, 0.5,
        "archived note confidence must be unchanged"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decay_floors_out_at_confidence_floor() {
    let (db, repo, project_id) = setup().await;

    let case = repo
        .create(&project_id, "Floored Case", "body", "case", "[]")
        .await
        .unwrap();
    // Start at the floor: decay must not push below it.
    patch_note(&db, &case.id, OLD_LAST_ACCESSED, CONFIDENCE_FLOOR, None).await;

    // Because confidence (floor) is below STALE_CITATION, the candidate is not
    // even selected (the SQL requires confidence > STALE_CITATION).
    let decayed = repo
        .decay_stale_extracted_notes(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();
    assert_eq!(decayed, 0, "floored note is below STALE_CITATION already");

    let confidence = note_confidence(&db, &case.id).await;
    assert!(
        (confidence - CONFIDENCE_FLOOR).abs() < f64::EPSILON,
        "confidence must remain at the floor, got {confidence}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decay_iteration_cap_holds_under_large_fixture() {
    let (db, repo, project_id) = setup().await;

    // Insert 100 stale case notes. Each decays in at most
    // DECAY_ITERATION_CAP iterations, so the whole tick should finish in well
    // under 100ms of DB time (basic timing sanity, no hard assertion).
    let mut note_ids = Vec::with_capacity(100);
    for i in 0..100u32 {
        let note = repo
            .create(
                &project_id,
                &format!("Stale Case {i}"),
                "body",
                "case",
                "[]",
            )
            .await
            .unwrap();
        patch_note(&db, &note.id, OLD_LAST_ACCESSED, 0.5, None).await;
        note_ids.push(note.id);
    }

    let start = Instant::now();
    let decayed = repo
        .decay_stale_extracted_notes(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert_eq!(decayed, 100, "all 100 stale notes should be decayed");

    // Timing sanity: the iteration cap bounds per-note work. This is a soft
    // check (no hard assertion) per the task spec — we just log it.
    eprintln!("decay of 100 notes took {elapsed:?}");

    // Every note should now be at or below STALE_CITATION.
    for id in &note_ids {
        let confidence = note_confidence(&db, id).await;
        assert!(
            confidence <= STALE_CITATION,
            "note {id} confidence {confidence} should be <= STALE_CITATION"
        );
    }
}

/// SQL defensive predicate: a note whose `last_accessed` is the empty string is
/// treated as never accessed (and therefore stale). The column is
/// `NOT NULL DEFAULT to_char(...)` so a true NULL is unreachable through the
/// normal insert path, but the SQL still carries an `IS NULL` / `= ''` safety
/// net to defend against hand-edited rows or future schema relaxations; this
/// test exercises the empty-string branch which is reachable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decay_empty_last_accessed_is_treated_as_stale() {
    let (db, repo, project_id) = setup().await;

    let case = repo
        .create(&project_id, "Never Accessed Case", "body", "case", "[]")
        .await
        .unwrap();
    // Set last_accessed to the empty string to exercise the `= ''` branch of
    // the staleness predicate. (A literal `NULL` cannot be stored because the
    // column is `NOT NULL DEFAULT to_char(...)`.)
    sqlx::query("UPDATE notes SET last_accessed = '', confidence = 0.5 WHERE id = $1")
        .bind(&case.id)
        .execute(db.pool())
        .await
        .unwrap();

    let decayed = repo
        .decay_stale_extracted_notes(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();

    assert_eq!(decayed, 1, "empty last_accessed note should be decayed");

    let confidence = note_confidence(&db, &case.id).await;
    assert!(confidence <= STALE_CITATION);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decay_pattern_and_pitfall_types_are_eligible() {
    let (db, repo, project_id) = setup().await;

    let pattern = repo
        .create(&project_id, "Stale Pattern", "body", "pattern", "[]")
        .await
        .unwrap();
    let pitfall = repo
        .create(&project_id, "Stale Pitfall", "body", "pitfall", "[]")
        .await
        .unwrap();
    patch_note(&db, &pattern.id, OLD_LAST_ACCESSED, 0.5, None).await;
    patch_note(&db, &pitfall.id, OLD_LAST_ACCESSED, 0.5, None).await;

    let decayed = repo
        .decay_stale_extracted_notes(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();

    assert_eq!(decayed, 2, "pattern and pitfall should both be decayed");

    assert!(note_confidence(&db, &pattern.id).await <= STALE_CITATION);
    assert!(note_confidence(&db, &pitfall.id).await <= STALE_CITATION);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn decay_does_not_over_decay_already_below_threshold() {
    let (db, repo, project_id) = setup().await;

    let case = repo
        .create(&project_id, "Already Stale Case", "body", "case", "[]")
        .await
        .unwrap();
    // Already below STALE_CITATION — should not be selected or modified.
    patch_note(&db, &case.id, OLD_LAST_ACCESSED, 0.15, None).await;

    let decayed = repo
        .decay_stale_extracted_notes(&project_id, TEST_WINDOW_DAYS)
        .await
        .unwrap();

    assert_eq!(decayed, 0, "already-below-threshold note not re-decayed");

    let confidence = note_confidence(&db, &case.id).await;
    assert!(
        (confidence - 0.15).abs() < f64::EPSILON,
        "confidence must be unchanged at 0.15, got {confidence}"
    );
}

/// Sanity: a single Bayesian decay step with `STALE_DECAY_SIGNAL` (0.15) from
/// 0.5 lands below `STALE_CITATION` (0.3) but above the floor. This documents
/// the per-step convergence behaviour that the iteration cap relies on.
///
/// Math: `(0.5 * 0.15) / (0.5 * 0.15 + 0.5 * 0.85) = 0.075 / 0.5 = 0.15`.
#[test]
fn single_decay_step_from_half_moves_downward_within_bounds() {
    let prior = 0.5_f64;
    let posterior = bayesian_update(prior, STALE_DECAY_SIGNAL);
    assert!(posterior < prior, "decay must reduce confidence");
    assert!(posterior >= CONFIDENCE_FLOOR);
    // One step from 0.5 with signal 0.15 already lands at 0.15 (below
    // STALE_CITATION 0.3), so the per-tick iteration cap is rarely needed in
    // practice for an injection-eligible note.
    assert!(posterior <= STALE_CITATION);
}
