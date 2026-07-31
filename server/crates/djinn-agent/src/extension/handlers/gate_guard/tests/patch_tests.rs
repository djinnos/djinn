use super::*;

// ═══════════════════════════════════════════════════════════════════════
// Truncated / windowed-read enforcement: call_edit
// (AC 1: truncated/windowed denials are hard denials, edit_forced unset,
//  retries keep denying, covering read transitions to investigation)
// ═══════════════════════════════════════════════════════════════════════

/// Windowed (Range) read that does not cover the edit match span denies
/// the worker, leaves `edit_forced` empty in `gateguard_snapshot`, and
/// does NOT mutate the file.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn windowed_read_denies_worker_edit_and_keeps_gateguard_clean() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-win-edit-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "AAAA\nBBBB\n").await.expect("seed");

    let session_id = worktree.path().display().to_string();

    // Record a partial read covering only bytes 0..5 ("AAAA\n").
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
        .expect("record windowed read");

    // Edit targets "BBBB" at bytes 5..9 — outside the window.
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
    .expect_err("windowed read must deny worker edit");

    assert!(
        err.contains("FORCE-UNCOVERED-READ"),
        "expected FORCE-UNCOVERED-READ, got: {err}"
    );

    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(content.contains("BBBB"), "file must be unchanged");

    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        snap.edit_forced.is_empty(),
        "edit_forced must be empty after windowed denial, got: {:?}",
        snap.edit_forced
    );
}

/// Identical uncovered/windowed retries for `call_edit` keep denying
/// until a covering non-truncated read occurs.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn identical_windowed_read_retries_keep_denying_for_edit() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-win-retry-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "AAAA\nBBBB\n").await.expect("seed");

    let session_id = worktree.path().display().to_string();

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
        .expect("record windowed read");

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

    let err1 = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("first windowed attempt must deny");
    assert!(err1.contains("FORCE-UNCOVERED-READ"));

    let err2 = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("second windowed attempt must still deny");
    assert!(err2.contains("FORCE-UNCOVERED-READ"));

    let err3 = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("third windowed attempt must still deny");
    assert!(err3.contains("FORCE-UNCOVERED-READ"));

    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        snap.edit_forced.is_empty(),
        "edit_forced must remain empty after repeated windowed denials"
    );
}

/// After a windowed-read denial, a covering non-truncated full-file read
/// transitions the denial to the normal first-edit FORCE investigation
/// prompt (not continued UNCOVERED denial).
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn full_read_after_windowed_denial_transitions_edit_to_investigation() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-win-full-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    state
        .file_time
        .read_with_coverage(
            &session_id,
            &file,
            ReadCoverage::Range {
                start: 0,
                end: Some(10),
            },
            false,
        )
        .await
        .expect("record windowed read");

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
    .expect_err("windowed read denies");
    assert!(err.contains("FORCE-UNCOVERED-READ"));

    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("full re-read");

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
        !err.contains("FORCE-UNCOVERED-READ"),
        "must no longer be an uncovered denial"
    );
    assert!(
        !err.contains("FORCE-TRUNCATED-READ"),
        "must not be a truncated denial"
    );

    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        snap.edit_forced.len() == 1,
        "edit_forced must contain exactly one path after investigation, got: {:?}",
        snap.edit_forced
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Truncated / windowed-read enforcement: call_write
// (AC 2: existing-file overwrite requires full non-truncated coverage,
//  edit_forced unset for windowed reads, transitions to investigation
//  only after full non-truncated read)
// ═══════════════════════════════════════════════════════════════════════

/// Windowed (Range) read denies worker write to existing file.
/// Write overwrites the entire file so requires full coverage.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn windowed_read_denies_worker_write_and_keeps_gateguard_clean() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-win-write-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "line one\nline two\nline three\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    state
        .file_time
        .read_with_coverage(
            &session_id,
            &file,
            ReadCoverage::Range {
                start: 0,
                end: Some(10),
            },
            false,
        )
        .await
        .expect("record windowed read");

    let write_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "content": "overwrite everything\n",
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let err = call_write(
        &state,
        &write_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("windowed read must deny worker write");

    assert!(
        err.contains("FORCE-UNCOVERED-READ") || err.contains("FORCE-TRUNCATED-READ"),
        "expected coverage denial, got: {err}"
    );

    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(content.contains("line one"), "file must be unchanged");

    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        snap.edit_forced.is_empty(),
        "edit_forced must be empty after windowed write denial, got: {:?}",
        snap.edit_forced
    );
}

/// After a windowed-read denial on write, a full non-truncated read
/// transitions to the first-edit FORCE investigation prompt.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn full_read_after_windowed_denial_transitions_write_to_investigation() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-win-wtrans-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    state
        .file_time
        .read_with_coverage(
            &session_id,
            &file,
            ReadCoverage::Range {
                start: 0,
                end: Some(10),
            },
            false,
        )
        .await
        .expect("record windowed read");

    let write_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "content": "let a = collections_query;\n",
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let err = call_write(
        &state,
        &write_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("windowed read denies write");
    assert!(
        err.contains("FORCE-UNCOVERED-READ") || err.contains("FORCE-TRUNCATED-READ"),
        "expected coverage denial, got: {err}"
    );

    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("full re-read");

    let err = call_write(
        &state,
        &write_args,
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
        !err.contains("FORCE-UNCOVERED-READ"),
        "must not be an uncovered denial after full read"
    );
    assert!(
        !err.contains("FORCE-TRUNCATED-READ"),
        "must not be a truncated denial after full read"
    );

    assert!(
        state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must be set after investigation prompt"
    );
}

// ═══════════════════════════════════════════════════════════════════════
// Truncated / windowed-read enforcement: call_apply_patch
// (AC 3: update/delete paths conservatively deny truncated/windowed
//  reads when full-file coverage is required, no mutation on denial)
// ═══════════════════════════════════════════════════════════════════════

/// Windowed (Range) read denies worker patch update. The conservative
/// gate uses `0..usize::MAX` so only `ReadCoverage::Full` can pass.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn windowed_read_denies_worker_patch_update_and_keeps_gateguard_clean() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-win-pupd-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "existing content\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

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
        .expect("record windowed read");

    let patch_args = Some(
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: svc.rs\n@@ existing content\n-existing content\n+new content\n*** End Patch"
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let err = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("windowed read must deny worker patch update");

    assert!(
        err.contains("FORCE-UNCOVERED-READ") || err.contains("FORCE-TRUNCATED-READ"),
        "expected coverage denial, got: {err}"
    );

    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(
        content.contains("existing content"),
        "file must be unchanged after denial"
    );

    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        snap.edit_forced.is_empty(),
        "edit_forced must be empty after windowed patch denial, got: {:?}",
        snap.edit_forced
    );
}

/// Truncated read denies worker patch delete. The file must not be
/// deleted, and `edit_forced` must remain empty.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn truncated_read_denies_worker_patch_delete_no_mutation() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-trunc-pdel-");
    let file = worktree.path().join("doomed.rs");
    tokio::fs::write(&file, "I must not be deleted\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    state
        .file_time
        .read_with_coverage(&session_id, &file, ReadCoverage::Full, true)
        .await
        .expect("record truncated read");

    let patch_args = Some(
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Delete File: doomed.rs\n*** End Patch"
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let err = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("truncated read must deny worker patch delete");

    assert!(
        err.contains("FORCE-TRUNCATED-READ"),
        "expected FORCE-TRUNCATED-READ, got: {err}"
    );

    assert!(
        file.exists(),
        "file must still exist after truncated-read denial"
    );
    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(
        content.contains("I must not be deleted"),
        "file content must be unchanged"
    );

    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        snap.edit_forced.is_empty(),
        "edit_forced must be empty after truncated delete denial"
    );
}

/// Windowed (Range) read denies worker patch delete. Same behavior as
/// truncated: conservatively requires full-file coverage for delete ops.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn windowed_read_denies_worker_patch_delete_no_mutation() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-win-pdel-");
    let file = worktree.path().join("doomed.rs");
    tokio::fs::write(&file, "line one\nline two\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    state
        .file_time
        .read_with_coverage(
            &session_id,
            &file,
            ReadCoverage::Range {
                start: 0,
                end: Some(10),
            },
            false,
        )
        .await
        .expect("record windowed read");

    let patch_args = Some(
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Delete File: doomed.rs\n*** End Patch"
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let err = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("windowed read must deny worker patch delete");

    assert!(
        err.contains("FORCE-UNCOVERED-READ") || err.contains("FORCE-TRUNCATED-READ"),
        "expected coverage denial, got: {err}"
    );

    assert!(
        file.exists(),
        "file must still exist after windowed-read denial"
    );

    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        snap.edit_forced.is_empty(),
        "edit_forced must be empty after windowed delete denial"
    );
}

/// After a truncated patch denial, a full non-truncated re-read
/// transitions the patch gate to the investigation prompt.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn full_read_after_truncated_denial_transitions_patch_to_investigation() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-trunc-ptrans-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    state
        .file_time
        .read_with_coverage(&session_id, &file, ReadCoverage::Full, true)
        .await
        .expect("record truncated read");

    let patch_args = Some(
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: svc.rs\n@@ let a = services;\n-let a = services;\n+let a = collections_query;\n*** End Patch"
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    let err = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("truncated read denies patch");
    assert!(err.contains("FORCE-TRUNCATED-READ"));

    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("full re-read");

    let err = call_apply_patch(
        &state,
        &patch_args,
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
        "must no longer be truncated denial"
    );

    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        snap.edit_forced.len() == 1,
        "edit_forced must contain exactly one path after investigation, got: {:?}",
        snap.edit_forced
    );
}

// ═══════════════════════════════════════════════════════════════════════
// AC 4: gateguard_snapshot proof that truncated/uncovered denials
//       do NOT insert the path into edit_forced
// ═══════════════════════════════════════════════════════════════════════

/// Exhaustively verify through `gateguard_snapshot` that a sequence of
/// truncated and uncovered denials across all three edit surfaces never
/// inserts the path into `edit_forced`. Then verify that a covering
/// non-truncated read + first-edit investigation is the *only* path
/// that populates `edit_forced`.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn gateguard_snapshot_proves_no_edit_forced_from_deficient_reads() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-snap-proof-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "AAAA\nBBBB\nCCCC\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    // ── Phase 1: truncated read → edit denial ──────────────────────
    state
        .file_time
        .read_with_coverage(&session_id, &file, ReadCoverage::Full, true)
        .await
        .expect("record truncated read");

    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "AAAA",
            "new_text": "ZZZZ",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let _ = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("truncated edit denial");

    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        snap.edit_forced.is_empty(),
        "truncated edit denial must not set edit_forced"
    );

    // ── Phase 2: windowed read → write denial ──────────────────────
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
        .expect("record windowed read");

    let write_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "content": "overwrite\n",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let _ = call_write(
        &state,
        &write_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("windowed write denial");

    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        snap.edit_forced.is_empty(),
        "windowed write denial must not set edit_forced"
    );

    // ── Phase 3: truncated read → patch update denial ──────────────
    state
        .file_time
        .read_with_coverage(&session_id, &file, ReadCoverage::Full, true)
        .await
        .expect("record truncated read for patch");

    let patch_args = Some(
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: svc.rs\n@@ AAAA\n-AAAA\n+ZZZZ\n*** End Patch"
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let _ = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("truncated patch denial");

    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        snap.edit_forced.is_empty(),
        "truncated patch denial must not set edit_forced"
    );

    // ── Phase 4: covering full read → first edit → investigation ───
    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("full covering read");

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
    assert!(err.contains("GateGuard"), "must be investigation prompt");

    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        !snap.edit_forced.is_empty(),
        "investigation prompt must set edit_forced"
    );
    assert_eq!(snap.edit_forced.len(), 1, "exactly one path in edit_forced");
}

// ═══════════════════════════════════════════════════════════════════════
// apply_patch gates on the span it rewrites, not the whole file
//
// `call_apply_patch` used to declare `0..usize::MAX` for Update ops, which
// only `ReadCoverage::Full` can satisfy. No read of a file past the 2000-line
// cap — or past the tool-result budget — can produce `Full`, so patching such
// a file was unsatisfiable: the gate demanded a whole-file read while
// `patch.rs` told the worker not to re-read the whole file. Confirmed live:
// 18 `FORCE-UNCOVERED-READ` denials with byte range [0, 18446744073709551615)
// across 8 of 10 sampled worker sessions, apply_patch failing 35/50 calls, and
// 12 `python3`/`cat` heredoc source rewrites routed through `shell` to escape.
// ═══════════════════════════════════════════════════════════════════════

/// Build a file large enough that no single read can record `Full` coverage.
async fn seed_large_file(path: &std::path::Path, n: usize) {
    let mut contents = String::new();
    for i in 1..=n {
        contents.push_str(&format!(
            "    let value_{i} = compute_something(input_{i});\n"
        ));
    }
    tokio::fs::write(path, &contents).await.expect("seed");
}

/// A windowed read that covers the patched region must let the patch through
/// (reaching the ordinary first-edit investigation FORCE), not deny it as
/// uncovered.
///
/// Non-vacuity: with the old `0..usize::MAX` span this returns
/// FORCE-UNCOVERED-READ, because a windowed read can never be `Full`.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn apply_patch_accepts_a_read_window_covering_the_patched_span() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-patch-span-ok-");
    let file = worktree.path().join("big.rs");
    seed_large_file(&file, 900).await;

    // Read the first window of the file — this is what a worker gets from a
    // whole-file read of a file this size.
    let read_args = Some(
        serde_json::json!({ "file_path": "big.rs", "offset": 0, "limit": 100 })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("windowed read");

    let session_id = worktree.path().display().to_string();
    let rec = state
        .file_time
        .latest_record(&session_id, &file)
        .await
        .expect("record");
    assert!(
        !rec.is_full(),
        "a 900-line file must not record Full coverage; the test would be vacuous otherwise"
    );

    // Patch line 10 — inside the window that was read.
    let patch = "*** Begin Patch\n\
                 *** Update File: big.rs\n\
                 @@     let value_10 = compute_something(input_10);\n\
                 -    let value_10 = compute_something(input_10);\n\
                 +    let value_10 = compute_something_else(input_10);\n\
                 *** End Patch\n";
    let patch_args = Some(
        serde_json::json!({ "patch": patch })
            .as_object()
            .unwrap()
            .clone(),
    );
    let err = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("first covered edit still hits the investigation FORCE");

    assert!(
        !err.contains("FORCE-UNCOVERED-READ"),
        "a read window covering the patched span must not be denied as uncovered: {err}"
    );
    assert!(
        err.contains("GateGuard"),
        "expected the ordinary first-edit investigation prompt, got: {err}"
    );
}

/// The gate still bites: a patch aimed outside the read window is denied.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn apply_patch_still_denies_a_span_outside_the_read_window() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-patch-span-deny-");
    let file = worktree.path().join("big.rs");
    seed_large_file(&file, 900).await;

    // Read only the first 50 lines.
    let read_args = Some(
        serde_json::json!({ "file_path": "big.rs", "offset": 0, "limit": 50 })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("windowed read");

    // Patch line 800 — far outside the window.
    let patch = "*** Begin Patch\n\
                 *** Update File: big.rs\n\
                 @@     let value_800 = compute_something(input_800);\n\
                 -    let value_800 = compute_something(input_800);\n\
                 +    let value_800 = compute_something_else(input_800);\n\
                 *** End Patch\n";
    let patch_args = Some(
        serde_json::json!({ "patch": patch })
            .as_object()
            .unwrap()
            .clone(),
    );
    let err = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("patch outside the read window must be denied");

    assert!(
        err.contains("FORCE-UNCOVERED-READ"),
        "expected FORCE-UNCOVERED-READ for an unread region, got: {err}"
    );

    let contents = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(
        contents.contains("let value_800 = compute_something(input_800);"),
        "file must be unchanged"
    );
}

/// A `Delete` still requires whole-file coverage — deleting destroys every
/// byte, so the conservative span is the honest one.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn apply_patch_delete_still_requires_full_coverage() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-patch-del-");
    let file = worktree.path().join("big.rs");
    seed_large_file(&file, 900).await;

    let read_args = Some(
        serde_json::json!({ "file_path": "big.rs", "offset": 0, "limit": 100 })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("windowed read");

    let patch = "*** Begin Patch\n\
                 *** Delete File: big.rs\n\
                 *** End Patch\n";
    let patch_args = Some(
        serde_json::json!({ "patch": patch })
            .as_object()
            .unwrap()
            .clone(),
    );
    let err = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("delete on a partially-read file must be denied");

    assert!(
        err.contains("FORCE-UNCOVERED-READ"),
        "delete must still demand whole-file coverage, got: {err}"
    );
    assert!(file.exists(), "file must not be deleted");
}
