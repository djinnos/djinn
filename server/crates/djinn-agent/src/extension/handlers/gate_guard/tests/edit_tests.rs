use super::*;

// ─── AC 1: truncated read denies worker edit, leaves edit_forced unset ──

#[tokio::test]
async fn truncated_read_denies_worker_edit() {
    let (worktree, state) = setup_worktree("gg-trunc-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "fn main() {\n    println!(\"hello\");\n}\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    // Simulate a truncated read: record coverage for the file but mark
    // it as truncated (byte budget hit, etc.).
    state
        .file_time
        .read_with_coverage(
            &session_id,
            &file,
            ReadCoverage::Full,
            true, // truncated
        )
        .await
        .expect("record truncated read");

    // Attempt a worker edit — must be denied.
    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "main",
            "new_text": "entry",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let err = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("truncated read must deny worker edit");

    assert!(
        err.contains("FORCE-TRUNCATED-READ"),
        "expected FORCE-TRUNCATED-READ, got: {err}"
    );

    // File must NOT be modified.
    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(content.contains("main"), "file must be unchanged");

    // edit_forced must NOT be set (truncated denials don't mark it).
    assert!(
        !state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must remain unset after truncated denial"
    );
}

// ─── AC 1: uncovered read denies worker edit, leaves edit_forced unset ──

#[tokio::test]
async fn uncovered_read_denies_worker_edit() {
    let (worktree, state) = setup_worktree("gg-uncovered-");
    let file = worktree.path().join("svc.rs");
    // File content: "AAAA" at bytes 0-4, "\n" at 4-5, "BBBB" at 5-9.
    // Edit targets "BBBB" which is at bytes 5..9.
    tokio::fs::write(&file, "AAAA\nBBBB\n").await.expect("seed");

    let session_id = worktree.path().display().to_string();

    // Record a partial read that only covers bytes 0..5 ("AAAA\n").
    state
        .file_time
        .read_with_coverage(
            &session_id,
            &file,
            ReadCoverage::Range {
                start: 0,
                end: Some(5),
            },
            false,
        )
        .await
        .expect("record partial read");

    // Edit targets bytes 5..9 ("BBBB") — outside the read coverage.
    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "BBBB",
            "new_text": "CCCC",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let err = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("uncovered span must deny worker edit");

    assert!(
        err.contains("FORCE-UNCOVERED-READ"),
        "expected FORCE-UNCOVERED-READ, got: {err}"
    );

    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(content.contains("BBBB"), "file must be unchanged");
    assert!(
        !state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must remain unset after uncovered denial"
    );
}

// ─── AC 2: first covered worker edit returns investigation prompt ───────

#[tokio::test]
async fn first_covered_worker_edit_returns_investigation_prompt() {
    let (worktree, state) = setup_worktree("gg-first-edit-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "fn main() {\n    services();\n}\n")
        .await
        .expect("seed");

    // Full, non-truncated read via the public API.
    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    let session_id = worktree.path().display().to_string();

    // First worker edit — must return investigation prompt.
    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "services",
            "new_text": "collections_query",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let err = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("first worker edit must be denied with investigation prompt");

    assert!(
        err.contains("GateGuard"),
        "prompt must mention GateGuard, got: {err}"
    );
    assert!(
        err.contains("Importers/callers"),
        "prompt must demand importers/callers, got: {err}"
    );
    assert!(
        err.contains("public functions"),
        "prompt must demand affected public functions, got: {err}"
    );
    assert!(
        err.contains("data schema") || err.contains("data shape"),
        "prompt must demand data schema/shape, got: {err}"
    );
    assert!(
        err.contains("verbatim task instruction"),
        "prompt must demand verbatim task instruction, got: {err}"
    );

    // edit_forced MUST be set after the investigation prompt.
    assert!(
        state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must be set after investigation prompt"
    );

    // File must NOT have been modified.
    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(content.contains("services"), "file must be unchanged");
}

// ─── AC 2: retry after re-read is allowed when edit_forced is set ───────

#[tokio::test]
async fn worker_edit_allowed_after_investigation_prompt_and_reread() {
    let (worktree, state) = setup_worktree("gg-retry-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\n")
        .await
        .expect("seed");

    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "services",
            "new_text": "collections_query",
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    // Phase 1: read → first edit → investigation prompt.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");
    let err = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("first edit must trigger investigation");
    assert!(err.contains("GateGuard"));

    // Phase 2: re-read (to satisfy FileTime freshness after investigation)
    // and retry — must succeed.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");
    let response = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("retry after investigation must succeed");
    assert_eq!(response["ok"], serde_json::json!(true));
    assert!(response.get("edit_match").is_some());

    // File must be modified.
    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(
        content.contains("collections_query"),
        "file must be modified: {content}"
    );
}

// ─── AC 2: third/subsequent edits are not re-gated ──────────────────────

#[tokio::test]
async fn worker_subsequent_edits_not_regated_after_investigation() {
    let (worktree, state) = setup_worktree("gg-steady-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\nlet b = helper;\n")
        .await
        .expect("seed");

    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );

    // Read → first edit → investigation prompt.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");
    let edit1 = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "services",
            "new_text": "collections_query",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let err = call_edit(
        &state,
        &edit1,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("first edit triggers investigation");
    assert!(err.contains("GateGuard"));

    // Re-read → retry first edit → succeeds (edit_forced is set).
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");
    call_edit(
        &state,
        &edit1,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("retry must succeed");

    // Re-read → second edit (different target) → must succeed without
    // re-triggering the investigation prompt.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read 2");
    let edit2 = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "helper",
            "new_text": "utility",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let response = call_edit(
        &state,
        &edit2,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("subsequent edit must not re-trigger investigation");
    assert_eq!(response["ok"], serde_json::json!(true));
}

// ─── AC 3: non-worker roles bypass GateGuard entirely ───────────────────

#[tokio::test]
async fn reviewer_role_bypasses_gate_guard() {
    let (worktree, state) = setup_worktree("gg-reviewer-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\n")
        .await
        .expect("seed");

    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    let session_id = worktree.path().display().to_string();

    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "services",
            "new_text": "collections_query",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let response = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("reviewer"),
    )
    .await
    .expect("reviewer must bypass GateGuard");
    assert_eq!(response["ok"], serde_json::json!(true));

    // edit_forced must NOT be set for non-worker roles.
    assert!(
        !state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must not be set for reviewer"
    );
}

#[tokio::test]
async fn missing_role_bypasses_gate_guard() {
    let (worktree, state) = setup_worktree("gg-norole-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\n")
        .await
        .expect("seed");

    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "services",
            "new_text": "collections_query",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    // None role — must succeed without GateGuard interference.
    let response = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect("missing role must bypass GateGuard");
    assert_eq!(response["ok"], serde_json::json!(true));
}

// ─── Identical retries keep denying until covering read ─────────────────

#[tokio::test]
async fn identical_truncated_retries_keep_denying() {
    let (worktree, state) = setup_worktree("gg-retry-trunc-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "fn main() {\n    println!(\"hello\");\n}\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    state
        .file_time
        .read_with_coverage(&session_id, &file, ReadCoverage::Full, true)
        .await
        .expect("record truncated read");

    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "main",
            "new_text": "entry",
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    // First attempt: denied.
    let err1 = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("first truncated attempt must deny");
    assert!(err1.contains("FORCE-TRUNCATED-READ"));

    // Second attempt (identical): must still deny.
    let err2 = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("second truncated attempt must still deny");
    assert!(err2.contains("FORCE-TRUNCATED-READ"));

    // edit_forced still not set.
    assert!(
        !state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must remain unset after repeated truncated denials"
    );
}

// ─── Re-read after truncated denial resolves the gate ───────────────────

#[tokio::test]
async fn non_truncated_reread_after_truncated_denial_resolves_gate() {
    let (worktree, state) = setup_worktree("gg-resolve-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    // Initial truncated read.
    state
        .file_time
        .read_with_coverage(&session_id, &file, ReadCoverage::Full, true)
        .await
        .expect("record truncated read");

    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "services",
            "new_text": "collections_query",
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    // Denied: truncated.
    let err = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("truncated read denies");
    assert!(err.contains("FORCE-TRUNCATED-READ"));

    // Non-truncated re-read via the public API.
    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("non-truncated re-read");

    // Now the edit triggers the investigation prompt (not truncated denial).
    let err = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("investigation prompt expected");
    assert!(
        err.contains("GateGuard"),
        "expected investigation prompt, got: {err}"
    );
    assert!(
        !err.contains("FORCE-TRUNCATED-READ"),
        "must no longer be a truncated denial"
    );

    // edit_forced now set.
    assert!(
        state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must be set after investigation prompt"
    );
}
