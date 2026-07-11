//! JIT pitfall retrieval-trace regression coverage (epic mwtv / Wave 2 task 7m6s).
//!
//! These tests assert that the JIT pitfall handler writes `jit_pitfalls`
//! retrieval-trace rows to the database for the three production paths —
//! **injected**, **empty/miss**, and **search-error** — while preserving
//! existing counters/logging/response behavior. They complement the pure
//! classification unit tests in [`super::super::handlers::jit_pitfalls::trace`]
//! and the response-shape assertions in [`super::tool_dispatch_tests`] by
//! verifying the **persisted** trace rows and their metadata.
//!
//! ## How trace rows are verified
//!
//! Each test drives the JIT pitfall handler through the real `call_write`
//! dispatch path (identical to what a live agent session uses), then queries
//! the `retrieval_traces` table via [`RetrievalTraceRepository`] to confirm
//! a `jit_pitfalls`-entry-point row was persisted with the expected trigger
//! shape, candidate outcomes, cap metadata, and per-phase durations.
//!
//! ## Default cap and token estimation
//!
//! The JIT trace universe is capped at [`DEFAULT_CANDIDATE_CAP`] (50). The
//! estimated injected tokens use `ceil(chars / 4)` from the rendered
//! `<relevant-pitfalls>` block. Both are asserted here and documented in the
//! [`jit_pitfalls::trace`] module constants.

use super::*;

use djinn_db::repositories::retrieval_trace::{
    CandidateOutcome, DEFAULT_CANDIDATE_CAP, RetrievalTraceEntryPoint, RetrievalTraceListFilter,
    RetrievalTraceRepository, SkippedReason,
};

/// Seed a `pitfall` note scoped to `scope_path` for `project_id`.
/// Default confidence (1.0) clears the 0.3 floor in `query_by_scope_overlap`.
async fn seed_pitfall(
    db: &djinn_db::Database,
    project_id: &str,
    title: &str,
    body: &str,
    scope_path: &str,
) {
    let repo = NoteRepository::new(db.clone(), EventBus::noop());
    let scope_json = format!("[\"{scope_path}\"]");
    repo.create_db_note_with_scope(project_id, title, body, "pitfall", "[]", &scope_json)
        .await
        .expect("seed pitfall note");
}

/// Fetch the most recent `jit_pitfalls` trace row for a project.
async fn latest_jit_trace(
    db: &djinn_db::Database,
    project_id: &str,
) -> Option<djinn_db::repositories::retrieval_trace::RetrievalTraceRow> {
    let repo = RetrievalTraceRepository::new(db.clone());
    repo.list_by_project(
        project_id,
        RetrievalTraceListFilter {
            entry_point: Some(RetrievalTraceEntryPoint::JitPitfalls),
            limit: Some(1),
            ..Default::default()
        },
    )
    .await
    .ok()?
    .into_iter()
    .next()
}

/// Extract candidate outcomes from a trace row as (note_id, outcome, skipped_reason).
fn candidate_outcomes(
    row: &djinn_db::repositories::retrieval_trace::RetrievalTraceRow,
) -> Vec<(String, CandidateOutcome, Option<SkippedReason>)> {
    row.candidates_typed()
        .into_iter()
        .map(|c| (c.note_id, c.outcome, c.skipped_reason))
        .collect()
}

/// Same env-lock as the dispatch JIT tests — serializes process-env mutation.
static JIT_TRACE_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ── Injected path: trace row written with injected candidates ───────────────

/// With the gate ON and matching notes, the first write renders a hint AND
/// persists a `jit_pitfalls` trace row with the expected trigger shape, cap
/// metadata, per-phase durations, and estimated tokens. The rendered hint
/// payload is unchanged by trace instrumentation (AC3: "existing response
/// payloads and hint rendering remain unchanged while `jit_pitfalls` trace rows
/// are written").
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_trace_row_written_on_injected_path() {
    let _guard = JIT_TRACE_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by the env-lock mutex.
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "enabled");
    }

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-trace-inj-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    // Seed two pitfall notes scoped to `src`; both are above threshold so
    // both should be classified as Injected (top-K = 2).
    seed_pitfall(&db, pid, "Injected Pitfall A", "body-a", "src").await;
    seed_pitfall(&db, pid, "Injected Pitfall B", "body-b", "src").await;

    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// x\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let response = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("write");

    // Response shape unchanged: hint present with top-2 bullets.
    let hint = response
        .get("jit_pitfalls")
        .and_then(|v| v.as_str())
        .expect("hint should be present on injected path");
    assert!(hint.starts_with("<relevant-pitfalls>"), "got: {hint}");
    let bullets = hint.lines().filter(|l| l.starts_with("- [")).count();
    assert_eq!(bullets, 2, "expected top-2 bullets");

    // Trace row persisted.
    let trace = latest_jit_trace(&db, pid)
        .await
        .expect("trace row should exist");

    // Entry point and trigger shape.
    assert_eq!(trace.entry_point, "jit_pitfalls");
    let trigger = trace.trigger.as_ref().expect("trigger present");
    assert_eq!(trigger["shape"], "touched_file");

    // Cap metadata.
    assert_eq!(trace.candidate_cap, DEFAULT_CANDIDATE_CAP);

    // Per-phase durations: search_elapsed_ms is always present;
    // persist_elapsed_ms is added post-insert.
    let durations = &trace.durations_ms;
    assert!(
        durations.get("search_elapsed_ms").is_some(),
        "durations should include search_elapsed_ms"
    );
    assert!(
        durations.get("persist_elapsed_ms").is_some(),
        "durations should include persist_elapsed_ms (measured across insert)"
    );

    // Estimated tokens: ceil(hint_chars / 4), so strictly positive.
    assert!(
        trace.estimated_injected_tokens > 0,
        "estimated_injected_tokens should be positive on injected path"
    );

    // Candidate outcomes: both notes should be Injected.
    let outcomes = candidate_outcomes(&trace);
    let injected = outcomes
        .iter()
        .filter(|(_, o, _)| *o == CandidateOutcome::Injected)
        .count();
    assert_eq!(
        injected, 2,
        "both seeded notes should be classified Injected"
    );

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
        std::env::remove_var("DJINN_JIT_PITFALLS");
    }
}

// ── Empty/miss path: trace row written with empty/miss outcomes ─────────────

/// With the gate ON but NO matching notes (search "miss"), the write still
/// succeeds with no hint. The handler persists a `jit_pitfalls` trace row for
/// the empty production result. Because no notes match, the trace candidate
/// universe is also empty, so the trace row carries an empty candidates array.
///
/// AC3: "JIT regression tests assert existing counters/logging-observable
/// behavior, response payloads, and hint rendering remain unchanged while
/// `jit_pitfalls` trace rows are written for [...] empty/miss [...] paths
/// where applicable."
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_trace_row_written_on_empty_miss_path() {
    let _guard = JIT_TRACE_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by the env-lock mutex.
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "enabled");
    }

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-trace-miss-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    // No pitfall notes seeded → production search returns empty.
    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// x\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let response = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("write must still succeed on search miss");

    // Response shape unchanged: no hint.
    assert_eq!(response.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert!(
        response.get("jit_pitfalls").is_none(),
        "miss must not append a hint"
    );

    // The empty path persists a trace row via `persist_jit_empty_trace`.
    let trace = latest_jit_trace(&db, pid)
        .await
        .expect("trace row should exist on miss path");

    assert_eq!(trace.entry_point, "jit_pitfalls");
    let trigger = trace.trigger.expect("trigger present");
    assert_eq!(trigger["shape"], "touched_file");
    // The empty path records rendered_note_count = 0.
    assert_eq!(trigger["rendered_note_count"], 0);

    // Estimated tokens is 0 because no notes were rendered.
    assert_eq!(
        trace.estimated_injected_tokens, 0,
        "no notes rendered → 0 estimated tokens"
    );

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
        std::env::remove_var("DJINN_JIT_PITFALLS");
    }
}

// ── Search-error path: trace row written with error metadata ────────────────

/// With the gate ON but NO project id available (the JIT path records the
/// safe error outcome and skips the hint), no trace row is written because
/// the error exit happens before the search. However, when the production
/// search query itself fails (e.g. table dropped), the error path persists a
/// `jit_pitfalls` trace row carrying the error in the trigger metadata.
///
/// We simulate a search error by dropping the `notes` table, which causes
/// `query_by_scope_overlap` to fail. The write must still succeed (fail-open)
/// and a `jit_pitfalls` trace row must be persisted with `search_error` in
/// the trigger.
///
/// AC3: "JIT regression tests assert existing counters/logging-observable
/// behavior, response payloads, and hint rendering remain unchanged while
/// `jit_pitfalls` trace rows are written for [...] search-error paths
/// where applicable."
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_trace_row_written_on_search_error_path() {
    let _guard = JIT_TRACE_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by the env-lock mutex.
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "enabled");
    }

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-trace-err-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    // Drop the notes table to force the production search query to fail.
    // This simulates a real search error in the JIT handler's error path.
    djinn_db::test_support::drop_table_for_test(&db, "notes").await;

    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// x\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let response = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("write must still succeed on search error");

    // Response shape unchanged: no hint on error path.
    assert_eq!(response.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert!(
        response.get("jit_pitfalls").is_none(),
        "error path must not append a hint"
    );

    // The error path persists a trace row via `persist_jit_error_trace`.
    let trace = latest_jit_trace(&db, pid)
        .await
        .expect("trace row should exist on error path");

    assert_eq!(trace.entry_point, "jit_pitfalls");
    let trigger = trace.trigger.expect("trigger present");
    assert_eq!(trigger["shape"], "touched_file");
    // The error path records a search_error string in the trigger.
    assert!(
        trigger["search_error"].as_str().is_some(),
        "trigger should carry a search_error string on the error path"
    );

    // Estimated tokens is 0 because no notes were rendered.
    assert_eq!(trace.estimated_injected_tokens, 0);

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
        std::env::remove_var("DJINN_JIT_PITFALLS");
    }
}

// ── Fail-open: trace persistence failure does not change behavior ───────────

/// When the `retrieval_traces` table is dropped (forcing trace persistence to
/// fail), the JIT handler must still produce the correct hint and response —
/// fail-open behavior. The write succeeds and the hint is rendered normally.
///
/// AC3 (via AC4 trace persistence fail-open): "JIT regression tests assert
/// existing counters/logging-observable behavior, response payloads, and hint
/// rendering remain unchanged while `jit_pitfalls` trace rows are written [...]."
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_trace_persistence_failure_does_not_change_hint() {
    let _guard = JIT_TRACE_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by the env-lock mutex.
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "enabled");
    }

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-trace-fail-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    seed_pitfall(&db, pid, "Fail-Open Pitfall", "body", "src").await;

    // Drop the retrieval_traces table to force trace persistence failure.
    // The hint should still be rendered and the response unchanged.
    djinn_db::test_support::drop_table_for_test(&db, "retrieval_traces").await;

    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// x\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let response = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("write must succeed even when trace persistence fails");

    // Hint is still rendered correctly despite trace persistence failure.
    let hint = response
        .get("jit_pitfalls")
        .and_then(|v| v.as_str())
        .expect("hint should still be rendered when trace persistence fails");
    assert!(
        hint.contains("Fail-Open Pitfall"),
        "hint content unchanged by trace failure"
    );

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
        std::env::remove_var("DJINN_JIT_PITFALLS");
    }
}

// ── Default cap and trigger-shape metadata regression ───────────────────────

/// The JIT trace trigger always carries the same fields regardless of path.
/// This test verifies that the injected-path trigger includes the production
/// constants that are documented as the lock-step boundary:
/// - `min_confidence` = 0.3
/// - `production_limit` = 8 (top_k * overfetch)
/// - `candidate_cap` = DEFAULT_CANDIDATE_CAP (50)
/// - `candidate_cap_source` = "DEFAULT_CANDIDATE_CAP"
/// - `note_types` = ["pitfall", "pattern"]
///
/// AC5: "Any introduced or clarified config, env var, candidate cap, sampling,
/// trigger-shape, persist-duration, or token-estimation defaults are
/// documented in code comments and covered by focused tests."
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_trace_trigger_carries_documented_production_constants() {
    let _guard = JIT_TRACE_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by the env-lock mutex.
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "cohort");
    }

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-trace-meta-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    seed_pitfall(&db, pid, "Meta Pitfall", "body", "src").await;

    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// x\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let _ = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("write");

    let trace = latest_jit_trace(&db, pid)
        .await
        .expect("trace row should exist");
    let trigger = trace.trigger.expect("trigger present");

    // Documented production constants (lock-step with the JIT handler).
    assert_eq!(trigger["min_confidence"], 0.3, "min_confidence floor");
    assert_eq!(trigger["production_limit"], 8, "top_k * overfetch = 2*4");
    assert_eq!(trigger["candidate_cap"], DEFAULT_CANDIDATE_CAP);
    assert_eq!(trigger["candidate_cap_source"], "DEFAULT_CANDIDATE_CAP");
    assert_eq!(
        trigger["note_types"],
        serde_json::json!(["pitfall", "pattern"])
    );
    assert_eq!(trigger["shape"], "touched_file");
    // rollout_mode reflects the env gate.
    assert_eq!(trigger["rollout_mode"], "cohort");

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
        std::env::remove_var("DJINN_JIT_PITFALLS");
    }
}

// ── Below-threshold and over-limit candidate classification ─────────────────

/// When the trace candidate universe contains notes below the confidence
/// threshold, the JIT trace classifies them as `MinConfidence`. Notes above
/// the threshold but not in the production top-K are classified as `NotTopK`.
///
/// This seeds below-threshold notes so the empty production result path
/// triggers `persist_jit_empty_trace`, which fetches the trace candidate
/// universe and classifies it. Below-threshold notes surface as
/// `MinConfidence` in the trace row.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_trace_empty_path_classifies_below_threshold_as_min_confidence() {
    let _guard = JIT_TRACE_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by the env-lock mutex.
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "enabled");
    }

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-trace-minconf-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    // Seed a pitfall note below the 0.3 threshold. The production query
    // (which filters confidence >= 0.3) excludes it → production result is
    // empty → the empty-path trace is persisted.
    let repo = NoteRepository::new(db.clone(), EventBus::noop());
    let note = repo
        .create_db_note_with_scope(
            pid,
            "Below Threshold Pitfall",
            "body",
            "pitfall",
            "[]",
            r#"["src"]"#,
        )
        .await
        .expect("seed note");
    // NULL confidence defaults to 0.0 in the DB which is < 0.3.
    // Explicitly set a low confidence.
    repo.set_confidence(&note.id, 0.1)
        .await
        .expect("set confidence");

    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// x\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let response = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("write");

    // Production result empty → no hint.
    assert!(response.get("jit_pitfalls").is_none());

    let trace = latest_jit_trace(&db, pid)
        .await
        .expect("trace row should exist on empty path");

    // The empty path fetches the trace candidate universe, which includes
    // the below-threshold note. It should be classified as MinConfidence.
    let outcomes = candidate_outcomes(&trace);
    let min_conf = outcomes
        .iter()
        .filter(|(_, _, r)| *r == Some(SkippedReason::MinConfidence))
        .count();
    assert!(
        min_conf >= 1,
        "below-threshold note should be MinConfidence in trace (got outcomes: {outcomes:?})"
    );

    // Validate the candidates satisfy the 5wdh TraceCandidate invariants.
    let typed = trace.candidates_typed();
    assert!(
        djinn_db::repositories::retrieval_trace::validate_candidates(&typed).is_ok(),
        "trace candidates must satisfy 5wdh invariants"
    );

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
        std::env::remove_var("DJINN_JIT_PITFALLS");
    }
}
