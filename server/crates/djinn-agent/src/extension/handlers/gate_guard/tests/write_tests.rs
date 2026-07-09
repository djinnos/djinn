use super::*;

// ═══════════════════════════════════════════════════════════════════════
// call_write GateGuard tests
// ═══════════════════════════════════════════════════════════════════════

// ─── AC 1: truncated read denies worker write, edit_forced unset ─────

#[tokio::test]
async fn truncated_read_denies_worker_write() {
    let (worktree, state) = setup_worktree("gg-write-trunc-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "existing content\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    // Simulate a truncated read.
    state
        .file_time
        .read_with_coverage(&session_id, &file, ReadCoverage::Full, true)
        .await
        .expect("record truncated read");

    let write_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "content": "new content\n",
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
    .expect_err("truncated read must deny worker write");

    assert!(
        err.contains("FORCE-TRUNCATED-READ"),
        "expected FORCE-TRUNCATED-READ, got: {err}"
    );

    // File must NOT be modified.
    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(content.contains("existing"), "file must be unchanged");

    // edit_forced must NOT be set.
    assert!(
        !state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must remain unset after truncated denial"
    );
}

// ─── AC 2: first covered write returns investigation prompt ──────────

#[tokio::test]
async fn first_covered_worker_write_returns_investigation_prompt() {
    let (worktree, state) = setup_worktree("gg-write-first-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\n")
        .await
        .expect("seed");

    // Full, non-truncated read.
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
    .expect_err("first worker write must be denied with investigation prompt");

    assert!(
        err.contains("GateGuard"),
        "prompt must mention GateGuard, got: {err}"
    );
    assert!(
        err.contains("Importers/callers"),
        "prompt must demand importers/callers, got: {err}"
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

// ─── AC 2: retry after re-read is allowed ───────────────────────────

#[tokio::test]
async fn worker_write_allowed_after_investigation_and_reread() {
    let (worktree, state) = setup_worktree("gg-write-retry-");
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
    let write_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "content": "let a = collections_query;\n",
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    // Phase 1: read → first write → investigation prompt.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");
    let err = call_write(
        &state,
        &write_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("first write must trigger investigation");
    assert!(err.contains("GateGuard"));

    // Phase 2: re-read (FileTime freshness) → retry → must succeed.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");
    let response = call_write(
        &state,
        &write_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("retry after investigation must succeed");
    assert_eq!(response["ok"], serde_json::json!(true));

    // File must be modified.
    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(
        content.contains("collections_query"),
        "file must be modified: {content}"
    );
}

// ─── AC 2: third/subsequent writes are not re-gated ──────────────────

#[tokio::test]
async fn worker_subsequent_writes_not_regated_after_investigation() {
    let (worktree, state) = setup_worktree("gg-write-steady-");
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

    // Read → first write → investigation prompt.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");
    let write1 = Some(
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
        &write1,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("first write triggers investigation");
    assert!(err.contains("GateGuard"));

    // Re-read → retry first write → succeeds.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");
    call_write(
        &state,
        &write1,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("retry must succeed");

    // Re-read → second write (different content) → must succeed without
    // re-triggering the investigation prompt.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read 2");
    let write2 = Some(
        serde_json::json!({
            "path": "svc.rs",
            "content": "let a = utility;\n",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let response = call_write(
        &state,
        &write2,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("subsequent write must not re-trigger investigation");
    assert_eq!(response["ok"], serde_json::json!(true));
}

// ─── AC 3/4: non-worker roles bypass GateGuard for write ─────────────

#[tokio::test]
async fn reviewer_bypasses_gate_guard_for_write() {
    let (worktree, state) = setup_worktree("gg-write-reviewer-");
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

    let write_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "content": "let a = collections_query;\n",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let response = call_write(
        &state,
        &write_args,
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

// ─── AC 4: new file write bypasses GateGuard ─────────────────────────

#[tokio::test]
async fn new_file_write_bypasses_gate_guard() {
    let (worktree, state) = setup_worktree("gg-write-newfile-");
    let session_id = worktree.path().display().to_string();

    // Write to a path that does NOT exist yet — should bypass GateGuard
    // entirely (no read required, no investigation prompt).
    let write_args = Some(
        serde_json::json!({
            "path": "brand_new.rs",
            "content": "fn main() {}\n",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let response = call_write(
        &state,
        &write_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("new file write must bypass GateGuard");
    assert_eq!(response["ok"], serde_json::json!(true));

    // edit_forced must NOT be set for new files.
    let new_file = worktree.path().join("brand_new.rs");
    assert!(
        !state
            .file_time
            .has_edit_forced(&session_id, &new_file)
            .await,
        "edit_forced must not be set for new file creation"
    );
}

// ─── Identical truncated write retries keep denying ───────────────────

#[tokio::test]
async fn identical_truncated_write_retries_keep_denying() {
    let (worktree, state) = setup_worktree("gg-write-retry-trunc-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "existing\n").await.expect("seed");

    let session_id = worktree.path().display().to_string();

    state
        .file_time
        .read_with_coverage(&session_id, &file, ReadCoverage::Full, true)
        .await
        .expect("record truncated read");

    let write_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "content": "overwrite\n",
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    // First attempt: denied.
    let err1 = call_write(
        &state,
        &write_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("first truncated attempt must deny");
    assert!(err1.contains("FORCE-TRUNCATED-READ"));

    // Second attempt (identical): must still deny.
    let err2 = call_write(
        &state,
        &write_args,
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

// ═══════════════════════════════════════════════════════════════════════
// call_apply_patch GateGuard tests
// ═══════════════════════════════════════════════════════════════════════

// ─── AC 3: truncated read denies worker patch update ──────────────────

#[tokio::test]
async fn truncated_read_denies_worker_patch_update() {
    let (worktree, state) = setup_worktree("gg-patch-trunc-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "existing content\n")
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
    .expect_err("truncated read must deny worker patch");

    assert!(
        err.contains("FORCE-TRUNCATED-READ"),
        "expected FORCE-TRUNCATED-READ, got: {err}"
    );

    // File must NOT be modified.
    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(content.contains("existing"), "file must be unchanged");

    // edit_forced must NOT be set.
    assert!(
        !state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must remain unset after truncated denial"
    );
}

// ─── AC 3: first covered patch update returns investigation prompt ────

#[tokio::test]
async fn first_covered_worker_patch_update_returns_investigation_prompt() {
    let (worktree, state) = setup_worktree("gg-patch-first-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\n")
        .await
        .expect("seed");

    // Full, non-truncated read.
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
    .expect_err("first worker patch must be denied with investigation prompt");

    assert!(
        err.contains("GateGuard"),
        "prompt must mention GateGuard, got: {err}"
    );
    assert!(
        err.contains("Importers/callers"),
        "prompt must demand importers/callers, got: {err}"
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

// ─── AC 3: patch retry after re-read is allowed ──────────────────────

#[tokio::test]
async fn worker_patch_allowed_after_investigation_and_reread() {
    let (worktree, state) = setup_worktree("gg-patch-retry-");
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
    let patch_args = Some(
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: svc.rs\n@@ let a = services;\n-let a = services;\n+let a = collections_query;\n*** End Patch"
        })
        .as_object()
        .unwrap()
        .clone(),
    );

    // Phase 1: read → first patch → investigation prompt.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");
    let err = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("first patch must trigger investigation");
    assert!(err.contains("GateGuard"));

    // Phase 2: re-read → retry → must succeed.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");
    let response = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("retry after investigation must succeed");
    assert_eq!(response["ok"], serde_json::json!(true));

    // File must be modified.
    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(
        content.contains("collections_query"),
        "file must be modified: {content}"
    );
}

// ─── AC 3: non-worker roles bypass GateGuard for patch ───────────────

#[tokio::test]
async fn reviewer_bypasses_gate_guard_for_patch() {
    let (worktree, state) = setup_worktree("gg-patch-reviewer-");
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

    let patch_args = Some(
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: svc.rs\n@@ let a = services;\n-let a = services;\n+let a = collections_query;\n*** End Patch"
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let response = call_apply_patch(
        &state,
        &patch_args,
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

// ─── AC 4: add-file patch operation bypasses GateGuard ────────────────

#[tokio::test]
async fn add_file_patch_bypasses_gate_guard() {
    let (worktree, state) = setup_worktree("gg-patch-addfile-");
    let session_id = worktree.path().display().to_string();

    // Add-file patch operation — no read required, no GateGuard.
    let patch_args = Some(
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Add File: new_module.rs\n+fn hello() {}\n*** End Patch"
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let response = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("add-file patch must bypass GateGuard");
    assert_eq!(response["ok"], serde_json::json!(true));

    // edit_forced must NOT be set for add-file operations.
    let new_file = worktree.path().join("new_module.rs");
    assert!(
        !state
            .file_time
            .has_edit_forced(&session_id, &new_file)
            .await,
        "edit_forced must not be set for add-file operation"
    );
}

// ─── AC 4: missing role bypasses GateGuard for write and patch ────────

#[tokio::test]
async fn missing_role_bypasses_gate_guard_for_write() {
    let (worktree, state) = setup_worktree("gg-write-norole-");
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

    let write_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "content": "let a = collections_query;\n",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    // None role — must succeed without GateGuard interference.
    let response = call_write(&state, &write_args, worktree.path(), None, None, None)
        .await
        .expect("missing role must bypass GateGuard for write");
    assert_eq!(response["ok"], serde_json::json!(true));
}

#[tokio::test]
async fn missing_role_bypasses_gate_guard_for_patch() {
    let (worktree, state) = setup_worktree("gg-patch-norole-");
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

    let patch_args = Some(
        serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: svc.rs\n@@ let a = services;\n-let a = services;\n+let a = collections_query;\n*** End Patch"
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    // None role — must succeed without GateGuard interference.
    let response = call_apply_patch(&state, &patch_args, worktree.path(), None, None, None)
        .await
        .expect("missing role must bypass GateGuard for patch");
    assert_eq!(response["ok"], serde_json::json!(true));
}
