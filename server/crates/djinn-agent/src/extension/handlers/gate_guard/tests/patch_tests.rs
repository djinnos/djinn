use super::*;

// ═══════════════════════════════════════════════════════════════════════
// Truncated / windowed-read enforcement: call_edit
// (AC 1: truncated/windowed denials are hard denials, edit_forced unset,
//  retries keep denying, covering read transitions to investigation)
// ═══════════════════════════════════════════════════════════════════════

/// Windowed (Range) read that does not cover the edit match span denies
/// the worker, leaves `edit_forced` empty in `gateguard_snapshot`, and
/// does NOT mutate the file.
#[tokio::test]
async fn windowed_read_denies_worker_edit_and_keeps_gateguard_clean() {
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
#[tokio::test]
async fn identical_windowed_read_retries_keep_denying_for_edit() {
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
#[tokio::test]
async fn full_read_after_windowed_denial_transitions_edit_to_investigation() {
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
#[tokio::test]
async fn windowed_read_denies_worker_write_and_keeps_gateguard_clean() {
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
#[tokio::test]
async fn full_read_after_windowed_denial_transitions_write_to_investigation() {
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
#[tokio::test]
async fn windowed_read_denies_worker_patch_update_and_keeps_gateguard_clean() {
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
#[tokio::test]
async fn truncated_read_denies_worker_patch_delete_no_mutation() {
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
#[tokio::test]
async fn windowed_read_denies_worker_patch_delete_no_mutation() {
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
#[tokio::test]
async fn full_read_after_truncated_denial_transitions_patch_to_investigation() {
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
#[tokio::test]
async fn gateguard_snapshot_proves_no_edit_forced_from_deficient_reads() {
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
