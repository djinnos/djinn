use super::*;
use crate::database::Database;
use crate::repositories::retrieval_trace::{
    CandidateOutcome, CreateRetrievalTraceParams, RetrievalTraceEntryPoint,
    RetrievalTraceRepository, SkippedReason, TraceCandidate, validate_candidates,
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

/// Data-layer contract fixture: convert a single `ScopeOverlapTraceCandidate`
/// row into a `TraceCandidate` ready for `retrieval_traces` JSONB persistence.
///
/// This is a deliberately deterministic, test-only conversion. It does **not**
/// implement dispatch classification logic — the production classifier that
/// decides which candidates are injected vs. skipped lives in the sibling epic
/// `mwtv`. The rule here is: a candidate ranked at or below `injected_top_n` is
/// marked `Injected` (without a `skipped_reason`); everything else is marked
/// `Skipped` with a `NotTopK` reason. The exact reason vocabulary is fixed by
/// the proposal (`SKIPPED_REASON_VALUES`) and is exercised by the existing
/// `validate_candidates` invariant.
fn scope_overlap_candidate_to_trace_candidate_for_data_layer_contract(
    candidate: &super::super::ScopeOverlapTraceCandidate,
    injected_top_n: u32,
) -> TraceCandidate {
    let rank_i32 = i32::try_from(candidate.rank)
        .expect("scope-overlap rank must fit in i32; trace candidate_cap is bounded");
    let scope_value = serde_json::from_str::<serde_json::Value>(&candidate.scope_paths)
        .unwrap_or_else(|_| serde_json::Value::String(candidate.scope_paths.clone()));

    // Scope metadata kept in a single object so downstream instrumentation has
    // the matched scope paths plus the original note-type/folder for context.
    let scope_object = json!({
        "scope_paths": scope_value,
        "note_type": candidate.note_type,
        "folder": candidate.folder,
    });

    if candidate.rank > 0 && (candidate.rank as u32) <= injected_top_n {
        TraceCandidate {
            note_id: candidate.id.clone(),
            permalink: Some(candidate.permalink.clone()),
            title: Some(candidate.title.clone()),
            outcome: CandidateOutcome::Injected,
            rank: Some(rank_i32),
            confidence: Some(candidate.confidence),
            skipped_reason: None,
            source: Some("scope_overlap".to_string()),
            scope: Some(scope_object),
        }
    } else {
        TraceCandidate {
            note_id: candidate.id.clone(),
            permalink: Some(candidate.permalink.clone()),
            title: Some(candidate.title.clone()),
            outcome: CandidateOutcome::Skipped,
            rank: Some(rank_i32),
            confidence: Some(candidate.confidence),
            skipped_reason: Some(SkippedReason::NotTopK),
            source: Some("scope_overlap".to_string()),
            scope: Some(scope_object),
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scope_overlap_trace_candidates_round_trip_through_retrieval_traces_jsonb() {
    // This test exercises the clean integration hardening that the force-closed
    // `jhc8` task was trying to land, building on the optional permalink/title
    // fields from `dy9z`. It must stay a data-layer contract fixture: it does
    // not implement dispatch classification, drop-reason logic, MCP tools, or
    // any change to the production `query_by_scope_overlap` path.
    let (repo, _tmp, project_id) = make_repo_and_project().await;
    let task_paths = vec!["server/src/server/state/mod.rs".to_string()];

    // Seed a deterministic set of in-scope notes so we can assert the exact
    // (note_id, permalink, title) ↔ trace-candidate mapping.
    let fixtures = [
        (
            "Top Injected",
            0.95,
            "2026-02-01T00:00:00.000Z",
            r#"["server/src"]"#,
            "perm-top-injected",
        ),
        (
            "Second Injected",
            0.85,
            "2026-02-02T00:00:00.000Z",
            r#"["server/src/server/state"]"#,
            "perm-second-injected",
        ),
        (
            "Skipped Not Top K",
            0.30,
            "2026-02-03T00:00:00.000Z",
            r#"["server/src/server/state/mod.rs"]"#,
            "perm-skipped-not-top-k",
        ),
    ];

    let mut expected_by_id: HashSet<String> = HashSet::new();
    let mut expected_injected: HashSet<String> = HashSet::new();
    let mut expected_skipped: HashSet<String> = HashSet::new();
    let mut permalinks_by_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut titles_by_id: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();

    for (title, confidence, updated_at, scope_paths, _permalink_label) in fixtures {
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
        // The repository's `create_with_scope` derives a permalink that includes
        // the project folder, so the exact value is opaque. We capture what
        // the repository produced so the round-trip assertion compares
        // identity, not a hard-coded string.
        permalinks_by_id.insert(note.id.clone(), note.permalink.clone());
        titles_by_id.insert(note.id.clone(), note.title.clone());
        expected_by_id.insert(note.id.clone());
        // Top 2 (by rank 1, 2) → Injected; rank 3 → Skipped.
        if confidence >= 0.80 {
            expected_injected.insert(note.id.clone());
        } else {
            expected_skipped.insert(note.id.clone());
        }
    }

    // Pull the unfiltered/capped candidate set.
    let scope_candidates = repo
        .query_by_scope_overlap_trace_candidates(&project_id, &task_paths, &["pattern"], 10)
        .await
        .unwrap();
    assert_eq!(
        scope_candidates.len(),
        3,
        "expected exactly the three seeded in-scope notes as trace candidates",
    );
    assert!(
        scope_candidates
            .iter()
            .all(|c| expected_by_id.contains(&c.id)),
        "scope candidates must be the seeded notes only",
    );

    // Convert each `ScopeOverlapTraceCandidate` into a `TraceCandidate` via the
    // deterministic data-layer contract fixture above. Production
    // classification belongs to a different epic; we hard-code "top 2 injected,
    // rest skipped with NotTopK" for this contract test.
    let injected_top_n: u32 = 2;
    let trace_candidates: Vec<TraceCandidate> = scope_candidates
        .iter()
        .map(|c| {
            scope_overlap_candidate_to_trace_candidate_for_data_layer_contract(c, injected_top_n)
        })
        .collect();

    // Sanity: the helper's invariants must pass before we persist.
    validate_candidates(&trace_candidates).expect("trace candidates must satisfy invariants");

    let candidates_json = serde_json::to_value(&trace_candidates).unwrap();

    // Persist through `RetrievalTraceRepository`.
    let trace_repo = RetrievalTraceRepository::new(repo.db.clone());
    let row = trace_repo
        .insert(CreateRetrievalTraceParams {
            project_id: &project_id,
            session_id: Some("sess-scope-overlap-roundtrip"),
            task_run_id: Some("run-scope-overlap-roundtrip"),
            task_id: Some("task-scope-overlap-roundtrip"),
            entry_point: RetrievalTraceEntryPoint::Dispatch,
            trigger: Some(&json!({
                "task_paths": task_paths,
                "fixture": "scope_overlap_trace_candidates_round_trip",
            })),
            candidates: &candidates_json,
            candidate_cap: 50,
            candidate_cap_exceeded: false,
            sampling_metadata: None,
            durations_ms: &json!({}),
            estimated_injected_tokens: 0,
        })
        .await
        .expect("trace insert must succeed for data-layer contract");

    // Fetch the detail row by id and assert the candidate JSONB survived
    // round-trip with every required field intact.
    let fetched = trace_repo
        .get_by_id(&row.id)
        .await
        .expect("get_by_id must not error")
        .expect("row just inserted must exist");

    let persisted = fetched.candidates_typed();
    assert_eq!(persisted.len(), trace_candidates.len());

    // Build lookup by note_id for stable assertions regardless of JSONB order.
    let mut by_note: std::collections::HashMap<String, TraceCandidate> =
        std::collections::HashMap::new();
    for c in &persisted {
        // The contract test must not persist duplicate note_ids; the source
        // query has a stable rank and a single note per row.
        assert!(
            by_note.insert(c.note_id.clone(), c.clone()).is_none(),
            "duplicate note_id {} in persisted candidates",
            c.note_id,
        );
    }

    for original in &scope_candidates {
        let stored = by_note
            .get(&original.id)
            .unwrap_or_else(|| panic!("missing persisted candidate for note {}", original.id));

        // Identity: note id, permalink, title.
        assert_eq!(stored.note_id, original.id);
        let expected_permalink = permalinks_by_id
            .get(&original.id)
            .expect("seeded permalink")
            .as_str();
        let expected_title = titles_by_id
            .get(&original.id)
            .expect("seeded title")
            .as_str();
        assert_eq!(stored.permalink.as_deref(), Some(expected_permalink));
        assert_eq!(stored.title.as_deref(), Some(expected_title));

        // Classification metadata: outcome + skipped_reason.
        if expected_injected.contains(&original.id) {
            assert_eq!(
                stored.outcome,
                CandidateOutcome::Injected,
                "rank-{} note {} should be Injected",
                original.rank,
                original.id,
            );
            assert!(
                stored.skipped_reason.is_none(),
                "Injected candidate {} must not carry a skipped_reason",
                original.id,
            );
        } else {
            assert_eq!(
                stored.outcome,
                CandidateOutcome::Skipped,
                "rank-{} note {} should be Skipped",
                original.rank,
                original.id,
            );
            assert_eq!(
                stored.skipped_reason,
                Some(SkippedReason::NotTopK),
                "Skipped candidate {} must carry the deterministic NotTopK reason",
                original.id,
            );
        }

        // Ranking + score.
        assert_eq!(stored.rank, Some(original.rank as i32));
        assert!(
            (stored.confidence.expect("persisted confidence") - original.confidence).abs()
                < f64::EPSILON,
            "persisted confidence must match source for note {}",
            original.id,
        );

        // Source + scope metadata must survive the JSONB row round-trip.
        assert_eq!(stored.source.as_deref(), Some("scope_overlap"));

        let scope = stored
            .scope
            .as_ref()
            .expect("persisted scope metadata must be present");
        let scope_obj = scope
            .as_object()
            .expect("scope must round-trip as a JSON object");

        // scope_paths is stored as a JSON value (parsed from the original
        // JSONB-text string) and must equal the original parsed shape.
        let stored_scope_paths = scope_obj
            .get("scope_paths")
            .expect("scope object must include scope_paths");
        let expected_scope_paths: serde_json::Value =
            serde_json::from_str(&original.scope_paths).unwrap();
        assert_eq!(stored_scope_paths, &expected_scope_paths);

        // Note type and folder from the source row must round-trip too.
        assert_eq!(
            scope_obj.get("note_type").and_then(|v| v.as_str()),
            Some(original.note_type.as_str()),
        );
        assert_eq!(
            scope_obj.get("folder").and_then(|v| v.as_str()),
            Some(original.folder.as_str()),
        );
    }

    // Defensive: the candidate JSONB must have produced the same set of
    // note_ids the source query returned (no loss, no extras).
    let persisted_ids: HashSet<String> = persisted.iter().map(|c| c.note_id.clone()).collect();
    let source_ids: HashSet<String> = scope_candidates.iter().map(|c| c.id.clone()).collect();
    assert_eq!(persisted_ids, source_ids);

    // Defensive: invariant check on the *fetched* candidates, not just the
    // pre-persistence set, to prove the JSONB row is internally consistent.
    validate_candidates(&persisted).expect("fetched candidates must satisfy invariants");

    // Sanity: expected_injected / expected_skipped are mutually exclusive and
    // cover every seeded note; the contract test would otherwise silently pass
    // if the fixture's confidence buckets were inconsistent.
    assert!(expected_injected.is_disjoint(&expected_skipped));
    assert_eq!(
        expected_injected.len() + expected_skipped.len(),
        expected_by_id.len(),
    );
}
