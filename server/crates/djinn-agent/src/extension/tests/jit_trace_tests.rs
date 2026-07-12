use super::*;

// ─── F2: just-in-time pitfall retrieval on first write (gated) ────────────

/// Env-lock for the `DJINN_JIT_PITFALLS_ROLLOUT` rollout gate. Held across `.await` on
/// purpose: the flag is process-global, so concurrent JIT tests must not
/// observe each other's env mutation. Same pattern as the auto-code-context
/// env tests in `helpers.rs`.
static JIT_PITFALLS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

const JIT_PITFALL_OUTCOMES: [&str; 7] = [
    "disabled_default_off",
    "disabled_kill_switch",
    "non_first_modification",
    "eligible_search",
    "injected",
    "empty",
    "error",
];

fn jit_pitfall_outcome_snapshot() -> Vec<(&'static str, u64)> {
    let rendered = djinn_telemetry::render().expect("install Prometheus recorder");
    JIT_PITFALL_OUTCOMES
        .into_iter()
        .map(|outcome| {
            let sample = format!("djinn_jit_pitfall_hints_total{{outcome=\"{outcome}\"}}");
            let line = rendered
                .lines()
                .find(|line| line.starts_with(&sample))
                .unwrap();
            let value = line.rsplit_once(' ').unwrap().1.parse::<u64>().unwrap();
            (outcome, value)
        })
        .collect()
}

fn assert_jit_pitfall_outcome_deltas(before: &[(&str, u64)], expected: &[(&str, u64)]) {
    for ((outcome, before_value), (after_outcome, after_value)) in
        before.iter().zip(jit_pitfall_outcome_snapshot())
    {
        assert_eq!(outcome, &after_outcome);
        let expected_delta = expected
            .iter()
            .find(|(label, _)| label == outcome)
            .map(|(_, delta)| *delta)
            .unwrap_or(0);
        assert_eq!(
            after_value - before_value,
            expected_delta,
            "unexpected JIT Prometheus delta for {outcome}"
        );
    }
}

/// Seed a `pitfall` note scoped to `scope_path` for `project_id`. Default
/// confidence (1.0) clears the 0.3 floor in `query_by_scope_overlap`. The
/// rendered hint falls back to the note body when no overview/abstract is
/// present, so the title alone is enough to identify a note in the output.
async fn seed_pitfall(
    db: &djinn_db::Database,
    project_id: &str,
    title: &str,
    body: &str,
    scope_path: &str,
) -> String {
    let repo = NoteRepository::new(db.clone(), EventBus::noop());
    let scope_json = format!("[\"{scope_path}\"]");
    repo.create_db_note_with_scope(project_id, title, body, "pitfall", "[]", &scope_json)
        .await
        .expect("seed pitfall note")
        .id
}

async fn latest_jit_trace(
    db: &djinn_db::Database,
    project_id: &str,
) -> djinn_db::repositories::retrieval_trace::RetrievalTraceRow {
    use djinn_db::repositories::retrieval_trace::{
        RetrievalTraceEntryPoint, RetrievalTraceListFilter, RetrievalTraceRepository,
    };

    RetrievalTraceRepository::new(db.clone())
        .list_by_project(
            project_id,
            RetrievalTraceListFilter {
                entry_point: Some(RetrievalTraceEntryPoint::JitPitfalls),
                limit: Some(1),
                ..Default::default()
            },
        )
        .await
        .expect("list JIT traces")
        .into_iter()
        .next()
        .expect("JIT trace should be persisted")
}

/// The normal JIT path preserves the rendered top-two hint while recording its
/// complete candidate universe for MCP trace consumers.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_pitfalls_trace_preserves_top_two_output_and_candidate_outcomes() {
    use djinn_db::repositories::retrieval_trace::{CandidateOutcome, SkippedReason};

    let _guard = JIT_PITFALLS_ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "cohort");
    }
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-jit-trace-normal-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    let first = seed_pitfall(&db, pid, "Trace First", "first body", "src").await;
    let second = seed_pitfall(&db, pid, "Trace Second", "second body", "src").await;
    let third = seed_pitfall(&db, pid, "Trace Third", "third body", "src").await;
    let below = seed_pitfall(&db, pid, "Trace Below", "below body", "src").await;
    let note_repo = NoteRepository::new(db.clone(), EventBus::noop());
    for (id, confidence) in [
        (&first, 0.95),
        (&second, 0.90),
        (&third, 0.85),
        (&below, 0.10),
    ] {
        note_repo
            .set_confidence(id, confidence)
            .await
            .expect("set deterministic confidence");
    }

    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    let telemetry_before = jit_pitfall_outcome_snapshot();
    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// trace\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let response = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("write");
    let hint = response["jit_pitfalls"].as_str().expect("rendered hint");
    assert!(hint.contains("Trace First") && hint.contains("Trace Second"));
    assert!(!hint.contains("Trace Third") && !hint.contains("Trace Below"));

    let trace = latest_jit_trace(&db, pid).await;
    assert_eq!(trace.entry_point, "jit_pitfalls");
    assert_eq!(trace.candidate_cap, 50);
    assert!(!trace.candidate_cap_exceeded);
    assert!(trace.estimated_injected_tokens > 0);
    assert!(trace.durations_ms.get("search_elapsed_ms").is_some());
    assert!(trace.durations_ms.get("trace_search_elapsed_ms").is_some());
    let trigger = trace.trigger.as_ref().expect("trigger metadata");
    assert_eq!(trigger["rollout_mode"], "cohort");
    assert_eq!(trigger["rendered_note_count"], 2);
    assert_eq!(trigger["production_limit"], 8);

    let candidates = trace.candidates_typed();
    let candidate = |id: &str| {
        candidates
            .iter()
            .find(|c| c.note_id == id)
            .expect("traced note")
    };
    for id in [&first, &second] {
        assert_eq!(candidate(id).outcome, CandidateOutcome::Injected);
        assert_eq!(candidate(id).skipped_reason, None);
    }
    assert_eq!(candidate(&third).outcome, CandidateOutcome::Skipped);
    assert_eq!(
        candidate(&third).skipped_reason,
        Some(SkippedReason::NotTopK)
    );
    assert_eq!(
        candidate(&below).skipped_reason,
        Some(SkippedReason::MinConfidence)
    );
    assert_jit_pitfall_outcome_deltas(
        &telemetry_before,
        &[("eligible_search", 1), ("injected", 1)],
    );

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
    }
}

/// A failed trace insert is strictly observational: normal JIT rendering stays
/// successful even after persistence is made unavailable.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_pitfalls_trace_insert_failure_is_fail_open_for_rendered_hint() {
    let _guard = JIT_PITFALLS_ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "enabled");
    }
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-jit-trace-insert-fail-");
    let baseline_worktree = crate::test_helpers::test_tempdir("djinn-ext-jit-trace-insert-ok-");
    for path in [worktree.path(), baseline_worktree.path()] {
        tokio::fs::create_dir_all(path.join("src"))
            .await
            .expect("mkdir src");
    }
    seed_pitfall(&db, pid, "Insert Failure Pitfall", "still rendered", "src").await;

    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// x\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let baseline = call_write(
        &state,
        &args,
        baseline_worktree.path(),
        Some(pid),
        None,
        None,
    )
    .await
    .expect("normal trace write");
    let baseline_hint = baseline["jit_pitfalls"].clone();
    djinn_db::test_support::drop_table_for_test(&db, "retrieval_traces").await;

    let telemetry_before = jit_pitfall_outcome_snapshot();
    let response = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("trace insert failure must not fail write");
    assert_eq!(
        response["jit_pitfalls"], baseline_hint,
        "repository insert failure must return the normal-path hint unchanged"
    );
    assert_jit_pitfall_outcome_deltas(
        &telemetry_before,
        &[("eligible_search", 1), ("injected", 1)],
    );

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
    }
}

/// A forced candidate-serialization failure is equally observational: the
/// normal rendered hint and its exact Prometheus outcome pair remain intact.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_pitfalls_trace_serialization_failure_is_fail_open() {
    let _guard = JIT_PITFALLS_ENV_LOCK.lock().unwrap();
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "enabled");
    }
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-jit-trace-serialize-fail-");
    let baseline_worktree = crate::test_helpers::test_tempdir("djinn-ext-jit-trace-serialize-ok-");
    for path in [worktree.path(), baseline_worktree.path()] {
        tokio::fs::create_dir_all(path.join("src"))
            .await
            .expect("mkdir src");
    }
    seed_pitfall(
        &db,
        pid,
        "Serialization Failure Pitfall",
        "still rendered",
        "src",
    )
    .await;
    let state = crate::test_helpers::agent_context_from_db(db, CancellationToken::new());
    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// x\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );

    let baseline = call_write(
        &state,
        &args,
        baseline_worktree.path(),
        Some(pid),
        None,
        None,
    )
    .await
    .expect("normal trace write");
    let baseline_hint = baseline["jit_pitfalls"].clone();

    let telemetry_before = jit_pitfall_outcome_snapshot();
    crate::extension::handlers::force_trace_candidate_serialization_failure_for_test(true);
    let response = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("serialization failure must not fail write");
    crate::extension::handlers::force_trace_candidate_serialization_failure_for_test(false);
    assert_eq!(
        response["jit_pitfalls"], baseline_hint,
        "serialization failure must return the normal-path hint unchanged"
    );
    assert_jit_pitfall_outcome_deltas(
        &telemetry_before,
        &[("eligible_search", 1), ("injected", 1)],
    );

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
    }
}

/// With the gate OFF (default), a write must produce byte-identical output:
/// no scoped search, no `jit_pitfalls` field — even when matching pitfall
/// notes exist for the touched path.
// Holds `JIT_PITFALLS_ENV_LOCK` across `.await` on purpose — the lock
// serializes process-env mutation for the duration of the async test (same
// rationale as the auto-code-context env tests). Deliberate test-only guard.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_pitfalls_off_by_default_no_hint() {
    let _guard = JIT_PITFALLS_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by the env-lock mutex.
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
        std::env::remove_var("DJINN_JIT_PITFALLS");
    }

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-jit-off-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    // A pitfall scoped to `src` overlaps `src/a.rs` — it WOULD surface if the
    // gate were on.
    seed_pitfall(&db, pid, "Off-Path Pitfall", "do not foo the bar", "src").await;

    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// x\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    let telemetry_before = jit_pitfall_outcome_snapshot();
    let response = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("write");

    assert!(
        response.get("jit_pitfalls").is_none(),
        "gate OFF must not append jit_pitfalls, got {response:?}"
    );
    assert_jit_pitfall_outcome_deltas(&telemetry_before, &[("disabled_default_off", 1)]);
}

/// An explicit rollout kill switch must suppress hints even if the legacy
/// one-bit opt-in remains set during migration.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_pitfalls_kill_switch_overrides_legacy_opt_in() {
    let _guard = JIT_PITFALLS_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by the env-lock mutex.
    unsafe {
        std::env::set_var("DJINN_JIT_PITFALLS", "1");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "kill-switch");
    }

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-jit-kill-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    seed_pitfall(&db, pid, "Killed Pitfall", "would have rendered", "src").await;

    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// x\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    let telemetry_before = jit_pitfall_outcome_snapshot();
    let response = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("write");

    assert_eq!(response.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert!(
        response.get("jit_pitfalls").is_none(),
        "kill switch must not append jit_pitfalls, got {response:?}"
    );
    assert_jit_pitfall_outcome_deltas(&telemetry_before, &[("disabled_kill_switch", 1)]);

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
        std::env::remove_var("DJINN_JIT_PITFALLS");
    }
}

/// With the gate ON: the FIRST write to a session runs the scoped search and
/// appends the top-2 pitfalls as a `<relevant-pitfalls>` block; a SECOND write
/// in the same session does NOT re-append (once-per-session).
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_pitfalls_on_first_write_appends_then_not_again() {
    let _guard = JIT_PITFALLS_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by the env-lock mutex.
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "cohort");
    }

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-jit-on-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    // Three matching pitfalls scoped to `src`; only the top 2 should render.
    seed_pitfall(&db, pid, "First Pitfall", "watch the lock ordering", "src").await;
    seed_pitfall(&db, pid, "Second Pitfall", "flush before close", "src").await;
    seed_pitfall(&db, pid, "Third Pitfall", "never block in drop", "src").await;

    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    let telemetry_before = jit_pitfall_outcome_snapshot();

    let args1 = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// first\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let r1 = call_write(&state, &args1, worktree.path(), Some(pid), None, None)
        .await
        .expect("first write");

    let hint = r1
        .get("jit_pitfalls")
        .and_then(|v| v.as_str())
        .expect("first write appends jit_pitfalls");
    assert!(hint.starts_with("<relevant-pitfalls>"), "got: {hint}");
    assert!(hint.contains("</relevant-pitfalls>"), "got: {hint}");
    // Top-2 only: exactly two bullet lines.
    let bullets = hint.lines().filter(|l| l.starts_with("- [")).count();
    assert_eq!(bullets, 2, "expected top-2 only, got hint:\n{hint}");

    // Cleanup the OTHER session-keyed entries can't leak: a SECOND write to the
    // SAME worktree (session) must NOT re-append.
    let args2 = Some(
        serde_json::json!({ "path": "src/b.rs", "content": "// second\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let r2 = call_write(&state, &args2, worktree.path(), Some(pid), None, None)
        .await
        .expect("second write");
    assert!(
        r2.get("jit_pitfalls").is_none(),
        "second write in same session must NOT re-append, got {r2:?}"
    );
    assert_jit_pitfall_outcome_deltas(
        &telemetry_before,
        &[
            ("eligible_search", 1),
            ("injected", 1),
            ("non_first_modification", 1),
        ],
    );

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
        std::env::remove_var("DJINN_JIT_PITFALLS");
    }
}

/// With the gate ON but NO matching notes (a search "miss"), the write must
/// still succeed with no `jit_pitfalls` field — the hint is skipped, never
/// fatal.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_pitfalls_on_miss_leaves_write_succeeding() {
    let _guard = JIT_PITFALLS_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by the env-lock mutex.
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "enabled");
    }

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-jit-miss-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    // No pitfall notes seeded → scoped search returns empty.
    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// x\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    let response = call_write(&state, &args, worktree.path(), Some(pid), None, None)
        .await
        .expect("write must still succeed on search miss");

    assert_eq!(response.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert!(
        response.get("jit_pitfalls").is_none(),
        "miss must not append a hint, got {response:?}"
    );

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
        std::env::remove_var("DJINN_JIT_PITFALLS");
    }
}

/// With the gate ON but no project id available, the JIT path records the
/// safe error outcome and skips the hint without failing the write. This covers
/// the search/error-path contract without needing to force a database failure.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_pitfalls_on_missing_project_id_leaves_write_succeeding() {
    let _guard = JIT_PITFALLS_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by the env-lock mutex.
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "enabled");
    }

    let db = create_test_db();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-jit-error-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    let args = Some(
        serde_json::json!({ "path": "src/a.rs", "content": "// x\\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    let response = call_write(&state, &args, worktree.path(), None, None, None)
        .await
        .expect("write must still succeed when JIT search cannot be scoped");

    assert_eq!(response.get("ok").and_then(|v| v.as_bool()), Some(true));
    assert!(
        response.get("jit_pitfalls").is_none(),
        "error path must not append a hint, got {response:?}"
    );

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
        std::env::remove_var("DJINN_JIT_PITFALLS");
    }
}

/// With the gate ON, `call_edit`'s FIRST modification also surfaces the hint
/// (parity with `call_write`) — confirms the wiring isn't write-only.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn jit_pitfalls_on_edit_first_modification_appends() {
    let _guard = JIT_PITFALLS_ENV_LOCK.lock().unwrap();
    // SAFETY: single-threaded section guarded by the env-lock mutex.
    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS");
        std::env::set_var("DJINN_JIT_PITFALLS_ROLLOUT", "enabled");
    }

    let db = create_test_db();
    let project = create_test_project(&db).await;
    let pid = project.id.as_str();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-jit-edit-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");
    let file = worktree.path().join("src/a.rs");
    tokio::fs::write(&file, "fn a() {}\n").await.expect("seed");

    seed_pitfall(&db, pid, "Edit Pitfall", "mind the borrow", "src").await;

    let state = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());

    // edit requires a prior read in-session.
    let read_args = Some(
        serde_json::json!({ "file_path": "src/a.rs" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    let edit_args = Some(
        serde_json::json!({
            "path": "src/a.rs",
            "old_text": "fn a() {}",
            "new_text": "fn a() { /* edited */ }"
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let response = call_edit(&state, &edit_args, worktree.path(), Some(pid), None, None)
        .await
        .expect("edit");

    let hint = response
        .get("jit_pitfalls")
        .and_then(|v| v.as_str())
        .expect("edit appends jit_pitfalls on first modification");
    assert!(hint.contains("Edit Pitfall"), "got: {hint}");

    unsafe {
        std::env::remove_var("DJINN_JIT_PITFALLS_ROLLOUT");
        std::env::remove_var("DJINN_JIT_PITFALLS");
    }
}

/// Regression: `call_write` accepts `session_task_id` and `session_role`
/// parameters (plumbed consistently with `call_edit`) and still succeeds
/// when invoked with a worker role. This proves the widened signature
/// compiles and is callable through the worker-role plumbing path without
/// changing runtime behavior.
#[tokio::test]
async fn write_accepts_worker_role_plumbing() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-write-worker-");
    tokio::fs::create_dir_all(worktree.path().join("src"))
        .await
        .expect("mkdir src");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "path": "src/hello.rs", "content": "fn main() {}\n" })
            .as_object()
            .expect("obj")
            .clone(),
    );

    // Invoke with worker role and a task id — must succeed (no GateGuard
    // enforcement in this epic).
    let response = call_write(
        &state,
        &args,
        worktree.path(),
        None,
        Some("task-abc-123"),
        Some("worker"),
    )
    .await
    .expect("call_write with worker role must succeed");

    assert_eq!(
        response.get("ok").and_then(|v| v.as_bool()),
        Some(true),
        "write must return ok=true, got: {response:?}"
    );
    assert_eq!(
        response.get("path").and_then(|v| v.as_str()),
        Some(
            worktree
                .path()
                .join("src/hello.rs")
                .display()
                .to_string()
                .as_str()
        ),
        "write must report the written path"
    );
}

// ─── call_read: `.djinn/memory/` NotFound teaching hint ───────────────────

/// A NotFound read whose path contains `.djinn/memory/` must return the
/// teaching error that names `memory_read` and `memory_search` with
/// concrete example invocations, rather than the generic
/// file-not-found / similar-filename error.
#[tokio::test]
async fn read_memory_path_not_found_returns_teaching_hint() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-read-mem-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "file_path": ".djinn/memory/pitfalls/some-slug.md" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let err = call_read(&state, &args, worktree.path())
        .await
        .expect_err("memory path read must fail with NotFound");
    assert!(
        err.contains("memory_read"),
        "hint must name memory_read, got: {err}"
    );
    assert!(
        err.contains("memory_search"),
        "hint must name memory_search, got: {err}"
    );
    assert!(
        err.contains("memory_read(identifier="),
        "hint must include a concrete memory_read example, got: {err}"
    );
    assert!(
        err.contains("memory_search(query="),
        "hint must include a concrete memory_search example, got: {err}"
    );
    // Must NOT include the generic similar-filename suffix.
    assert!(
        !err.contains("similar filenames"),
        "memory path must not show similar-filename suggestions, got: {err}"
    );
}

/// A NotFound read of a `.djinn/memory/` path expressed with an absolute
/// prefix must also trigger the hint (matching the resolved absolute form).
#[tokio::test]
async fn read_memory_path_not_found_absolute_form_triggers_hint() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-read-mem-abs-");
    let abs_memory = worktree
        .path()
        .join(".djinn/memory/decisions/adr-xyz.md")
        .display()
        .to_string();
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "file_path": abs_memory })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let err = call_read(&state, &args, worktree.path())
        .await
        .expect_err("absolute memory path read must fail with NotFound");
    assert!(err.contains("memory_read"), "got: {err}");
    assert!(err.contains("memory_search"), "got: {err}");
}

/// A NotFound read of a NON-memory path that has sibling files must keep
/// the existing generic file-not-found + similar-filename behavior
/// unchanged.
#[tokio::test]
async fn read_non_memory_not_found_keeps_similar_filename_suggestion() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-read-sim-");
    // Seed a sibling file so the similar-filename suggestion is non-empty.
    tokio::fs::write(worktree.path().join("sibling.txt"), "hi")
        .await
        .expect("seed sibling");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "file_path": "missing.txt" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let err = call_read(&state, &args, worktree.path())
        .await
        .expect_err("non-memory missing path must fail with NotFound");
    assert!(err.contains("file not found"), "got: {err}");
    assert!(err.contains("similar filenames"), "got: {err}");
    // Must NOT trigger the memory hint for a non-memory path.
    assert!(
        !err.contains("memory_read"),
        "non-memory path must not trigger memory hint, got: {err}"
    );
}

/// A NotFound read of a NON-memory path with no siblings must keep the
/// plain "file not found" message (no similar-filename suffix) and must
/// not trigger the memory hint.
#[tokio::test]
async fn read_non_memory_not_found_empty_parent_keeps_plain_message() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-read-empty-");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let args = Some(
        serde_json::json!({ "file_path": "gone.rs" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let err = call_read(&state, &args, worktree.path())
        .await
        .expect_err("non-memory missing path must fail with NotFound");
    assert!(err.contains("file not found"), "got: {err}");
    assert!(
        !err.contains("similar filenames"),
        "empty parent must not add similar-filename suffix, got: {err}"
    );
    assert!(
        !err.contains("memory_read"),
        "non-memory path must not trigger memory hint, got: {err}"
    );
}

/// A genuinely readable file must NOT trigger the memory NotFound hint —
/// the hint only fires in the NotFound branch, so a readable file returns
/// its content normally. This preserves ADR-057/FUSE readable-path
/// behavior by construction: even a readable file placed under
/// `.djinn/memory/` must return content, not the hint.
#[tokio::test]
async fn read_readable_file_does_not_trigger_memory_hint() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-read-ok-");
    // Place a readable file at a `.djinn/memory/` path to prove the hint
    // only fires on NotFound, not on any memory-looking path. If the file
    // exists and is readable, content is returned normally.
    let mem_dir = worktree.path().join(".djinn/memory/pitfalls");
    tokio::fs::create_dir_all(&mem_dir).await.expect("mkdir");
    let readable = mem_dir.join("readable.md");
    tokio::fs::write(&readable, "# real content\nline two\n")
        .await
        .expect("seed readable file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let rel = readable
        .strip_prefix(worktree.path())
        .unwrap()
        .to_string_lossy()
        .to_string();
    let args = Some(
        serde_json::json!({ "file_path": rel })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let result = call_read(&state, &args, worktree.path())
        .await
        .expect("readable file must return content, not an error");
    let content = result
        .get("content")
        .and_then(|v| v.as_str())
        .expect("content field");
    assert!(
        content.contains("real content"),
        "readable file content must be returned, got: {content}"
    );
}
