use super::*;
use crate::database::Database;
use crate::repositories::retrieval_trace::{
    CreateRetrievalTraceParams, RetrievalTraceEntryPoint, RetrievalTraceRepository, TraceCandidate,
};
use djinn_core::events::EventBus;
use serde_json::json;
use std::collections::HashSet;

async fn make_repo_and_project() -> (NoteRepository, tempfile::TempDir, String) {
    let tmp = crate::database::test_tempdir().unwrap();
    let db = Database::open_in_memory().unwrap();
    db.ensure_initialized().await.unwrap();
    let id = uuid::Uuid::now_v7().to_string();
    let owner = "test";
    let repo_slug = format!("scope-overlap-{id}");
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
    )
    .bind(&id)
    .bind("test")
    .bind(owner)
    .bind(repo_slug)
    .execute(db.pool())
    .await
    .unwrap();
    (NoteRepository::new(db, EventBus::noop()), tmp, id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_scoped_by_path_overlap_matches_parent_and_child_scopes_only() {
    let (repo, _tmp, project_id) = make_repo_and_project().await;

    let parent = repo
        .create_with_scope(
            &project_id,
            "Parent Scope",
            "content",
            "pattern",
            None,
            "[]",
            r#"["server/src"]"#,
        )
        .await
        .unwrap();
    let child = repo
        .create_with_scope(
            &project_id,
            "Child Scope",
            "content",
            "pattern",
            None,
            "[]",
            r#"["server/src/server/state"]"#,
        )
        .await
        .unwrap();
    let unrelated = repo
        .create_with_scope(
            &project_id,
            "Unrelated Scope",
            "content",
            "pattern",
            None,
            "[]",
            r#"["desktop/src"]"#,
        )
        .await
        .unwrap();
    let global = repo
        .create(&project_id, "Global Note", "content", "pattern", "[]")
        .await
        .unwrap();

    let matches = repo
        .query_scoped_by_path_overlap(
            &project_id,
            &["server/src/server/state/mod.rs".to_string()],
            20,
        )
        .await
        .unwrap();

    let ids: HashSet<String> = matches.into_iter().map(|note| note.id).collect();
    assert!(ids.contains(&parent.id));
    assert!(ids.contains(&child.id));
    assert!(!ids.contains(&unrelated.id));
    assert!(!ids.contains(&global.id));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_scoped_by_path_overlap_is_noop_for_empty_changed_paths() {
    let (repo, _tmp, project_id) = make_repo_and_project().await;
    repo.create_with_scope(
        &project_id,
        "Scoped Note",
        "content",
        "pattern",
        None,
        "[]",
        r#"["server/src"]"#,
    )
    .await
    .unwrap();

    let matches = repo
        .query_scoped_by_path_overlap(&project_id, &[], 20)
        .await
        .unwrap();
    assert!(matches.is_empty());
}

async fn set_scope_trace_signals(
    repo: &NoteRepository,
    note_id: &str,
    confidence: f64,
    updated_at: &str,
) {
    sqlx::query("UPDATE notes SET confidence = $1, updated_at = $2 WHERE id = $3")
        .bind(confidence)
        .bind(updated_at)
        .bind(note_id)
        .execute(repo.db.pool())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_by_scope_overlap_trace_candidates_keeps_unfiltered_ordered_candidates() {
    let (repo, _tmp, project_id) = make_repo_and_project().await;
    let other_project_id = uuid::Uuid::now_v7().to_string();
    sqlx::query(
        "INSERT INTO projects (id, name, github_owner, github_repo) VALUES ($1, $2, $3, $4)",
    )
    .bind(&other_project_id)
    .bind("other")
    .bind("test")
    .bind(format!("scope-overlap-other-{other_project_id}"))
    .execute(repo.db.pool())
    .await
    .unwrap();

    let task_paths = vec!["server/src/server/state/mod.rs".to_string()];
    let cases = [
        (
            "High Parent",
            0.95,
            "2026-01-07T00:00:00.000Z",
            r#"["server/src"]"#,
        ),
        (
            "Recent Tie",
            0.90,
            "2026-01-08T00:00:00.000Z",
            r#"["server/src/server/state"]"#,
        ),
        (
            "Older Tie",
            0.90,
            "2026-01-06T00:00:00.000Z",
            r#"["server/src/server/state"]"#,
        ),
        (
            "Below Production Limit",
            0.80,
            "2026-01-05T00:00:00.000Z",
            "[]",
        ),
        (
            "Below Threshold",
            0.40,
            "2026-01-04T00:00:00.000Z",
            r#"["server/src/server/state/mod.rs"]"#,
        ),
        (
            "Capped Out",
            0.30,
            "2026-01-03T00:00:00.000Z",
            r#"["server/src"]"#,
        ),
    ];

    let mut notes = Vec::new();
    for (title, confidence, updated_at, scope_paths) in cases {
        let note = repo
            .create_with_scope(
                &project_id,
                title,
                "content",
                "pattern",
                None,
                "[]",
                scope_paths,
            )
            .await
            .unwrap();
        set_scope_trace_signals(&repo, &note.id, confidence, updated_at).await;
        notes.push(note);
    }

    let unrelated = repo
        .create_with_scope(
            &project_id,
            "Unrelated Scope",
            "content",
            "pattern",
            None,
            "[]",
            r#"["desktop/src"]"#,
        )
        .await
        .unwrap();
    set_scope_trace_signals(&repo, &unrelated.id, 0.99, "2026-01-09T00:00:00.000Z").await;

    let archived = repo
        .create_with_scope(
            &project_id,
            "Archived Scope",
            "content",
            "pattern",
            None,
            "[]",
            r#"["server/src"]"#,
        )
        .await
        .unwrap();
    set_scope_trace_signals(&repo, &archived.id, 0.98, "2026-01-09T00:00:00.000Z").await;
    sqlx::query("UPDATE notes SET status = 'archived' WHERE id = $1")
        .bind(&archived.id)
        .execute(repo.db.pool())
        .await
        .unwrap();

    let wrong_type = repo
        .create_with_scope(
            &project_id,
            "Wrong Type",
            "content",
            "adr",
            None,
            "[]",
            r#"["server/src"]"#,
        )
        .await
        .unwrap();
    set_scope_trace_signals(&repo, &wrong_type.id, 0.97, "2026-01-09T00:00:00.000Z").await;

    let other_project = repo
        .create_with_scope(
            &other_project_id,
            "Other Project",
            "content",
            "pattern",
            None,
            "[]",
            r#"["server/src"]"#,
        )
        .await
        .unwrap();
    set_scope_trace_signals(&repo, &other_project.id, 0.96, "2026-01-09T00:00:00.000Z").await;

    let production = repo
        .query_by_scope_overlap(&project_id, &task_paths, &["pattern"], 0.5, 3)
        .await
        .unwrap();
    let production_titles: Vec<_> = production.iter().map(|note| note.title.as_str()).collect();
    assert_eq!(
        production_titles,
        vec!["High Parent", "Recent Tie", "Older Tie"]
    );

    let candidates = repo
        .query_by_scope_overlap_trace_candidates(&project_id, &task_paths, &["pattern"], 5)
        .await
        .unwrap();
    let candidate_titles: Vec<_> = candidates
        .iter()
        .map(|candidate| candidate.title.as_str())
        .collect();
    assert_eq!(
        candidate_titles,
        vec![
            "High Parent",
            "Recent Tie",
            "Older Tie",
            "Below Production Limit",
            "Below Threshold",
        ]
    );
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.rank)
            .collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
    assert_eq!(candidates[4].confidence, 0.40);
    assert_eq!(candidates[3].scope_paths, "[]");
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.note_type == "pattern")
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.id != unrelated.id)
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.id != archived.id)
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.id != wrong_type.id)
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.id != other_project.id)
    );
    assert!(
        candidates
            .iter()
            .all(|candidate| candidate.id != notes[5].id)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn query_by_scope_overlap_trace_candidates_empty_task_paths_matches_global_only() {
    let (repo, _tmp, project_id) = make_repo_and_project().await;
    let global = repo
        .create_with_scope(
            &project_id,
            "Global Trace Candidate",
            "content",
            "pattern",
            None,
            "[]",
            "[]",
        )
        .await
        .unwrap();
    set_scope_trace_signals(&repo, &global.id, 0.20, "2026-01-01T00:00:00.000Z").await;
    let scoped = repo
        .create_with_scope(
            &project_id,
            "Scoped Trace Candidate",
            "content",
            "pattern",
            None,
            "[]",
            r#"["server/src"]"#,
        )
        .await
        .unwrap();
    set_scope_trace_signals(&repo, &scoped.id, 0.99, "2026-01-02T00:00:00.000Z").await;

    let candidates = repo
        .query_by_scope_overlap_trace_candidates(&project_id, &[], &["pattern"], 10)
        .await
        .unwrap();

    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].id, global.id);
    assert_eq!(candidates[0].rank, 1);
}

// ── Data-layer integration contract helpers ──────────────────────────────────
//
// The `memory_recall_trace` detail mode (epic liso) and dispatch
// instrumentation (epic mwtv) consume persisted trace candidates through
// [`TraceCandidate`] DTOs.  The conversion helper below documents the
// expected shape transformation from scope-overlap query results to
// trace-candidate JSONB so downstream consumers can rely on:
//
//   - `note_id`: stable note identifier (from `ScopeOverlapTraceCandidate.id`)
//   - `rank`: 1-based rank from the production ordering
//   - `confidence`: retrieval score (0.0–1.0)
//   - `skipped_reason`: `None` for injected candidates; one of
//     [`SkippedReason`] for those filtered by top-K, min-confidence,
//     budget, dedup, or search-error
//   - `source`: `"scope_overlap"` — identifies the retrieval method
//   - `scope`: note scope_paths (carried forward for later classification)

/// Convert scope-overlap trace candidates into [`TraceCandidate`] DTOs for
/// persistence through [`RetrievalTraceRepository`].
///
/// Each candidate is mapped with `skipped_reason = None` (injected).  Callers
/// that apply production top-K, min-confidence, or budget pruning should set
/// the appropriate [`SkippedReason`] on non-injected entries before
/// persistence.
///
/// This helper documents the data-layer contract that downstream
/// instrumentation and `memory_recall_trace` tooling depend on.
fn scope_overlap_candidates_to_trace_candidates(
    candidates: &[super::ScopeOverlapTraceCandidate],
) -> Vec<TraceCandidate> {
    candidates
        .iter()
        .map(|c| TraceCandidate {
            note_id: c.id.clone(),
            permalink: Some(c.permalink.clone()),
            title: Some(c.title.clone()),
            rank: Some(c.rank as i32),
            confidence: Some(c.confidence),
            skipped_reason: None,
            source: Some("scope_overlap".to_string()),
            scope: serde_json::from_str(&c.scope_paths).ok(),
        })
        .collect()
}

/// Integration test (AC1): persist scope-overlap trace candidates through the
/// retrieval trace repository and verify the detail round-trip preserves all
/// metadata fields consumed by downstream `memory_recall_trace` detail mode
/// and dispatch instrumentation.
///
/// Verifies:
/// - note_id/permalink/title are present in the trace candidate set
/// - rank is 1-based and matches the production ordering
/// - confidence/score metadata round-trips through JSONB
/// - skipped_reason is `None` for injected candidates
/// - candidate_cap and candidate_cap_exceeded metadata round-trip
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trace_candidates_persist_and_fetch_detail_with_full_metadata() {
    let (note_repo, _tmp, project_id) = make_repo_and_project().await;
    let trace_repo = RetrievalTraceRepository::new(note_repo.db.clone());

    let task_paths = vec!["server/src/server/state/mod.rs".to_string()];

    // Seed notes: 3 above production threshold (0.5), 1 below threshold, 2 over
    // the production limit of 3 — all within the trace cap of 6.
    let cases = [
        (
            "High Parent",
            0.95,
            "2026-01-07T00:00:00.000Z",
            r#"["server/src"]"#,
        ),
        (
            "Recent Tie",
            0.90,
            "2026-01-08T00:00:00.000Z",
            r#"["server/src/server/state"]"#,
        ),
        (
            "Older Tie",
            0.90,
            "2026-01-06T00:00:00.000Z",
            r#"["server/src/server/state"]"#,
        ),
        (
            "Below Threshold",
            0.40,
            "2026-01-04T00:00:00.000Z",
            r#"["server/src/server/state/mod.rs"]"#,
        ),
        (
            "Over Limit A",
            0.60,
            "2026-01-03T00:00:00.000Z",
            r#"["server/src"]"#,
        ),
        (
            "Over Limit B",
            0.55,
            "2026-01-02T00:00:00.000Z",
            r#"["server/src/server/state"]"#,
        ),
    ];

    for (title, confidence, updated_at, scope_paths) in cases {
        let note = note_repo
            .create_with_scope(
                &project_id,
                title,
                "content",
                "pattern",
                None,
                "[]",
                scope_paths,
            )
            .await
            .unwrap();
        set_scope_trace_signals(&note_repo, &note.id, confidence, updated_at).await;
    }

    // Query trace candidates — no confidence filter, capped at 6.
    let raw_candidates = note_repo
        .query_by_scope_overlap_trace_candidates(&project_id, &task_paths, &["pattern"], 6)
        .await
        .unwrap();
    assert_eq!(
        raw_candidates.len(),
        6,
        "all 6 scope-overlap notes returned"
    );

    // Convert to TraceCandidate DTOs (all injected — no skipped_reason).
    let trace_candidates = scope_overlap_candidates_to_trace_candidates(&raw_candidates);
    let candidates_json = serde_json::to_value(&trace_candidates).unwrap();

    // Persist through retrieval trace repository.
    let trace_row = trace_repo
        .insert(CreateRetrievalTraceParams {
            project_id: &project_id,
            session_id: Some("sess-integration"),
            task_run_id: Some("run-integration"),
            task_id: Some("task-integration"),
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: Some(&json!({"query": "integration test"})),
            candidates: &candidates_json,
            candidate_cap: 6,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({"retrieval_ms": 15}),
            estimated_injected_tokens: 768,
        })
        .await
        .unwrap();

    // ── Detail round-trip assertions ──────────────────────────────────────

    // Cap metadata preserved.
    assert_eq!(trace_row.candidate_cap, 6);
    assert!(!trace_row.candidate_cap_exceeded);
    assert_eq!(
        trace_row.entry_point_enum(),
        Some(RetrievalTraceEntryPoint::Dispatch)
    );
    assert_eq!(trace_row.estimated_injected_tokens, 768);

    // Deserialize persisted candidates and verify each field.
    let persisted = trace_row.candidates_typed();
    assert_eq!(persisted.len(), 6);

    // Rank 1: High Parent (confidence 0.95).
    let c0 = &persisted[0];
    assert_eq!(c0.rank, Some(1));
    assert_eq!(c0.confidence, Some(0.95));
    assert!(
        c0.skipped_reason.is_none(),
        "injected candidates have no skipped_reason"
    );
    assert_eq!(c0.source.as_deref(), Some("scope_overlap"));
    assert_eq!(c0.note_id, raw_candidates[0].id);
    // Permalink/title persisted for downstream memory_recall_trace detail mode.
    assert_eq!(
        c0.permalink.as_deref(),
        Some(raw_candidates[0].permalink.as_str()),
        "permalink must round-trip through JSONB"
    );
    assert_eq!(
        c0.title.as_deref(),
        Some(raw_candidates[0].title.as_str()),
        "title must round-trip through JSONB"
    );
    // Scope metadata carried through for later classification.
    assert!(
        c0.scope.is_some(),
        "scope_paths should be preserved in trace candidate"
    );

    // Rank 4: Over Limit A (confidence 0.60) — present in trace despite being
    // outside production limit (3), would be classified as `not_top_k` by
    // downstream dispatch instrumentation.
    let over_a = &persisted[3];
    assert_eq!(over_a.rank, Some(4));
    assert_eq!(over_a.confidence, Some(0.60));
    assert!(
        over_a.skipped_reason.is_none(),
        "trace persists all candidates; classification happens downstream"
    );
    // Permalink/title also present on non-injected-bound candidates.
    assert!(
        over_a.permalink.is_some(),
        "over-limit candidates must have permalink persisted"
    );
    assert!(
        over_a.title.is_some(),
        "over-limit candidates must have title persisted"
    );
    assert_eq!(over_a.title.as_deref(), Some("Over Limit A"));

    // Rank 5: Over Limit B (confidence 0.55) — also outside production limit.
    assert_eq!(persisted[4].rank, Some(5));
    assert_eq!(persisted[4].confidence, Some(0.55));

    // Rank 6: Below Threshold (confidence 0.40) — present in trace despite
    // being below production confidence threshold (0.5), would be classified
    // as `min_confidence` by downstream dispatch instrumentation.
    assert_eq!(persisted[5].rank, Some(6));
    assert_eq!(persisted[5].confidence, Some(0.40));
    assert_eq!(persisted[5].title.as_deref(), Some("Below Threshold"));
    assert!(
        persisted[5].permalink.is_some(),
        "below-threshold candidates must have permalink persisted"
    );

    // Verify detail fetch returns the same row with all candidate fields intact.
    let detail = trace_repo
        .get_by_id(&trace_row.id)
        .await
        .unwrap()
        .expect("row must exist");
    assert_eq!(detail.id, trace_row.id);
    let detail_candidates = detail.candidates_typed();
    assert_eq!(detail_candidates.len(), 6);
    // Permalink/title survive the detail round-trip.
    assert_eq!(detail_candidates[0].permalink, persisted[0].permalink);
    assert_eq!(detail_candidates[0].title, persisted[0].title);
    assert_eq!(
        detail_candidates[5].title.as_deref(),
        Some("Below Threshold")
    );
}

/// Regression test (AC2): production `query_by_scope_overlap` remains
/// unchanged while the trace candidate source includes below-threshold
/// and over-limit active notes for downstream `min_confidence` / `not_top_k`
/// classification.
///
/// Seeds 6 notes: 3 above the production confidence threshold (≥ 0.5) and
/// within the production limit (3), 1 below the confidence threshold, and 2
/// above the production limit but below the trace cap.  Verifies:
///
/// - Production query returns exactly the 3 top-scoring notes that meet both
///   the confidence threshold and the limit.
/// - Trace candidate query returns all 6 eligible notes including the
///   below-threshold and over-limit notes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn production_scope_overlap_unchanged_while_trace_includes_all_eligible() {
    let (repo, _tmp, project_id) = make_repo_and_project().await;
    let task_paths = vec!["server/src/server/state/mod.rs".to_string()];

    // Notes ordered by expected confidence DESC, updated_at DESC:
    let cases = [
        (
            "Prod Top 1",
            0.95,
            "2026-01-07T00:00:00.000Z",
            r#"["server/src"]"#,
        ),
        (
            "Prod Top 2",
            0.90,
            "2026-01-08T00:00:00.000Z",
            r#"["server/src/server/state"]"#,
        ),
        (
            "Prod Top 3",
            0.90,
            "2026-01-06T00:00:00.000Z",
            r#"["server/src/server/state"]"#,
        ),
        // Below production threshold (0.5) — excluded by production, included in trace.
        (
            "Below Threshold",
            0.40,
            "2026-01-05T00:00:00.000Z",
            r#"["server/src/server/state/mod.rs"]"#,
        ),
        // Over production limit (3) but above threshold — excluded by production limit, included in trace.
        (
            "Over Limit A",
            0.60,
            "2026-01-04T00:00:00.000Z",
            r#"["server/src"]"#,
        ),
        (
            "Over Limit B",
            0.55,
            "2026-01-03T00:00:00.000Z",
            r#"["server/src/server/state"]"#,
        ),
    ];

    for (title, confidence, updated_at, scope_paths) in cases {
        let note = repo
            .create_with_scope(
                &project_id,
                title,
                "content",
                "pattern",
                None,
                "[]",
                scope_paths,
            )
            .await
            .unwrap();
        set_scope_trace_signals(&repo, &note.id, confidence, updated_at).await;
    }

    // Production retrieval: confidence >= 0.5, limit 3.
    let production = repo
        .query_by_scope_overlap(&project_id, &task_paths, &["pattern"], 0.5, 3)
        .await
        .unwrap();
    assert_eq!(
        production.len(),
        3,
        "production returns exactly 3 notes (confidence + limit)"
    );
    assert_eq!(production[0].title, "Prod Top 1");
    assert_eq!(production[1].title, "Prod Top 2");
    assert_eq!(production[2].title, "Prod Top 3");

    // Trace candidates: no confidence filter, cap 6.
    let trace = repo
        .query_by_scope_overlap_trace_candidates(&project_id, &task_paths, &["pattern"], 6)
        .await
        .unwrap();
    assert_eq!(
        trace.len(),
        6,
        "trace candidates include all 6 eligible notes"
    );

    let trace_titles: Vec<&str> = trace.iter().map(|c| c.title.as_str()).collect();
    assert_eq!(
        trace_titles,
        vec![
            "Prod Top 1",
            "Prod Top 2",
            "Prod Top 3",
            "Over Limit A",
            "Over Limit B",
            "Below Threshold",
        ],
        "trace preserves production ordering (confidence DESC, updated_at DESC)"
    );

    // Below-threshold note is present in trace.
    let below = trace.iter().find(|c| c.title == "Below Threshold").unwrap();
    assert!(
        below.confidence < 0.5,
        "below-threshold note has low confidence"
    );
    assert_eq!(below.rank, 6, "below-threshold note has rank 6");

    // Over-limit notes are present in trace with ranks > 3.
    let over_a = trace.iter().find(|c| c.title == "Over Limit A").unwrap();
    assert!(
        over_a.rank > 3,
        "over-limit note ranked outside production top-3"
    );
    let over_b = trace.iter().find(|c| c.title == "Over Limit B").unwrap();
    assert!(
        over_b.rank > 3,
        "over-limit note ranked outside production top-3"
    );

    // None of the trace-only notes appear in production results.
    let production_ids: HashSet<&str> = production.iter().map(|n| n.id.as_str()).collect();
    assert!(!production_ids.contains(below.id.as_str()));
    assert!(!production_ids.contains(over_a.id.as_str()));
    assert!(!production_ids.contains(over_b.id.as_str()));
}
