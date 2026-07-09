// GateGuard dispatch-level tests.
//
// These tests exercise the full dispatch path through `call_edit`, `call_write`,
// and `call_apply_patch` with session_role/session_task_id parameters, proving
// that GateGuard is enforced at the handler level (not just in the colocated
// helper tests).  The colocated `gate_guard::tests` module covers the helper
// itself; these cover the dispatch integration.

use super::handlers::{call_apply_patch, call_edit, call_read, call_write};
use super::{agent_context_from_db, create_test_db};
use tokio_util::sync::CancellationToken;

fn setup(prefix: &str) -> (tempfile::TempDir, crate::context::AgentContext) {
    let dir = crate::test_helpers::test_tempdir(prefix);
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    (dir, state)
}

// ─── AC 1: call_edit first-deny / second-allow / third-no-regate ──────────

/// First worker edit on a fully covered file returns the FORCE investigation
/// prompt and sets edit_forced.  A re-read + retry succeeds (FileTime freshness
/// is restored but edit_forced is preserved).  A third edit on the same file
/// succeeds without re-gating.
#[tokio::test]
async fn edit_worker_first_deny_second_allow_third_no_regate() {
    let (worktree, state) = setup("gged-edit-steady-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\nlet b = helper;\nlet c = util;\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    // Phase 0: read to satisfy read-before-edit.
    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("initial read");

    // Phase 1: first worker edit → must be denied with investigation prompt.
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
        Some("task-gg-1"),
        Some("worker"),
    )
    .await
    .expect_err("first edit must trigger investigation");
    assert!(
        err.contains("GateGuard"),
        "expected GateGuard prompt, got: {err}"
    );

    // edit_forced must be set after the investigation prompt.
    assert!(
        state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must be set after first denial"
    );

    // Phase 2: re-read (FileTime freshness) → retry same edit → succeeds.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");
    let resp = call_edit(
        &state,
        &edit1,
        worktree.path(),
        None,
        Some("task-gg-1"),
        Some("worker"),
    )
    .await
    .expect("retry after investigation must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    // edit_forced must still be set.
    assert!(
        state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must remain set after successful retry"
    );

    // Phase 3: re-read → third edit (different target) → succeeds without
    // re-triggering the investigation prompt.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read 2");
    let edit3 = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "helper",
            "new_text": "utility",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let resp = call_edit(
        &state,
        &edit3,
        worktree.path(),
        None,
        Some("task-gg-1"),
        Some("worker"),
    )
    .await
    .expect("subsequent edit must not re-gate");
    assert_eq!(resp["ok"], serde_json::json!(true));
}

// ─── AC 2: call_write first-deny / second-allow ──────────────────────────

/// First worker write to an existing fully covered file returns the FORCE
/// investigation prompt.  After re-read + retry the write succeeds.
/// A subsequent write on the same path is not re-gated.
#[tokio::test]
async fn write_worker_first_deny_second_allow_third_no_regate() {
    let (worktree, state) = setup("gged-write-steady-");
    let file = worktree.path().join("config.json");
    tokio::fs::write(&file, "{ \"key\": \"old\" }\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();

    let read_args = Some(
        serde_json::json!({ "file_path": "config.json" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    // First write → denied.
    let write1 = Some(
        serde_json::json!({
            "path": "config.json",
            "content": "{ \"key\": \"new\" }\n",
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
        Some("task-gg-w1"),
        Some("worker"),
    )
    .await
    .expect_err("first write must trigger investigation");
    assert!(err.contains("GateGuard"), "expected GateGuard, got: {err}");

    assert!(
        state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must be set after first write denial"
    );

    // Re-read → retry → succeeds.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");
    let resp = call_write(
        &state,
        &write1,
        worktree.path(),
        None,
        Some("task-gg-w1"),
        Some("worker"),
    )
    .await
    .expect("retry must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    // Third write (re-read again for FileTime) → no re-gate.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read 2");
    let write3 = Some(
        serde_json::json!({
            "path": "config.json",
            "content": "{ \"key\": \"third\" }\n",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let resp = call_write(
        &state,
        &write3,
        worktree.path(),
        None,
        Some("task-gg-w1"),
        Some("worker"),
    )
    .await
    .expect("subsequent write must not re-gate");
    assert_eq!(resp["ok"], serde_json::json!(true));
}

// ─── AC 2: call_apply_patch update first-deny / second-allow ─────────────

/// First worker apply_patch (update operation) on a fully covered file returns
/// the investigation prompt.  After re-read + retry it succeeds.
#[tokio::test]
async fn patch_update_worker_first_deny_second_allow() {
    let (worktree, state) = setup("gged-patch-steady-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "fn main() {\n    services();\n}\n")
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

    // First patch → denied.
    let patch = "*** Begin Patch\n*** Update File: svc.rs\n@@ fn main() @@\n fn main() {\n-    services();\n+    collections_query();\n }\n*** End Patch";
    let patch_args1 = Some(
        serde_json::json!({ "patch": patch })
            .as_object()
            .unwrap()
            .clone(),
    );
    let err = call_apply_patch(
        &state,
        &patch_args1,
        worktree.path(),
        None,
        Some("task-gg-p1"),
        Some("worker"),
    )
    .await
    .expect_err("first patch must trigger investigation");
    assert!(err.contains("GateGuard"), "expected GateGuard, got: {err}");

    // Re-read → retry → succeeds.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");
    let patch_args2 = Some(
        serde_json::json!({ "patch": patch })
            .as_object()
            .unwrap()
            .clone(),
    );
    let resp = call_apply_patch(
        &state,
        &patch_args2,
        worktree.path(),
        None,
        Some("task-gg-p1"),
        Some("worker"),
    )
    .await
    .expect("retry must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    // Verify the file was actually modified.
    let content = tokio::fs::read_to_string(&file).await.unwrap();
    assert!(
        content.contains("collections_query"),
        "file must contain the patched content: {content}"
    );
}

// ─── AC 2: call_apply_patch delete first-deny / second-allow ─────────────

/// First worker apply_patch with a DELETE operation on a fully covered file
/// returns the investigation prompt.  After re-read + retry it succeeds.
#[tokio::test]
async fn patch_delete_worker_first_deny_second_allow() {
    let (worktree, state) = setup("gged-patch-del-");
    let file = worktree.path().join("deprecated.rs");
    tokio::fs::write(&file, "// remove me\nfn old() {}\n")
        .await
        .expect("seed");

    let read_args = Some(
        serde_json::json!({ "file_path": "deprecated.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    let patch = "*** Begin Patch\n*** Delete File: deprecated.rs\n*** End Patch";
    let patch_args1 = Some(
        serde_json::json!({ "patch": patch })
            .as_object()
            .unwrap()
            .clone(),
    );
    let err = call_apply_patch(
        &state,
        &patch_args1,
        worktree.path(),
        None,
        Some("task-gg-pd1"),
        Some("worker"),
    )
    .await
    .expect_err("first delete must trigger investigation");
    assert!(err.contains("GateGuard"), "expected GateGuard, got: {err}");

    // Re-read → retry → succeeds.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");
    let patch_args2 = Some(
        serde_json::json!({ "patch": patch })
            .as_object()
            .unwrap()
            .clone(),
    );
    let resp = call_apply_patch(
        &state,
        &patch_args2,
        worktree.path(),
        None,
        Some("task-gg-pd1"),
        Some("worker"),
    )
    .await
    .expect("retry must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    assert!(!file.exists(), "deleted file must be removed from disk");
}

// ─── AC 3: role bypass — reviewer ────────────────────────────────────────

/// Reviewer edits succeed without GateGuard interference on all three surfaces.
#[tokio::test]
async fn reviewer_bypasses_gate_guard_all_surfaces() {
    let (worktree, state) = setup("gged-reviewer-");
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

    // call_edit — must succeed.
    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "services",
            "new_text": "collections",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let resp = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-r1"),
        Some("reviewer"),
    )
    .await
    .expect("reviewer edit must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    // Re-read for FileTime freshness after the edit modified the file.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");

    // call_write — must succeed.
    let write_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "content": "let a = overwritten;\n",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let resp = call_write(
        &state,
        &write_args,
        worktree.path(),
        None,
        Some("task-r1"),
        Some("reviewer"),
    )
    .await
    .expect("reviewer write must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    // Re-read for FileTime freshness.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read 2");

    // call_apply_patch — must succeed.
    tokio::fs::write(&file, "fn main() {\n    services();\n}\n")
        .await
        .expect("re-seed for patch");
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read 3");
    let patch = "*** Begin Patch\n*** Update File: svc.rs\n@@ fn main() @@\n fn main() {\n-    services();\n+    patched();\n }\n*** End Patch";
    let patch_args = Some(
        serde_json::json!({ "patch": patch })
            .as_object()
            .unwrap()
            .clone(),
    );
    let resp = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-r1"),
        Some("reviewer"),
    )
    .await
    .expect("reviewer apply_patch must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));
}

// ─── AC 3: role bypass — planner ─────────────────────────────────────────

/// Planner edit succeeds without GateGuard.
#[tokio::test]
async fn planner_bypasses_gate_guard() {
    let (worktree, state) = setup("gged-planner-");
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
            "new_text": "planned",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let resp = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-p1"),
        Some("planner"),
    )
    .await
    .expect("planner edit must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    // edit_forced must NOT be set for non-worker roles.
    let session_id = worktree.path().display().to_string();
    assert!(
        !state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must not be set for planner"
    );
}

// ─── AC 3: role bypass — architect ───────────────────────────────────────

/// Architect apply_patch succeeds without GateGuard.
#[tokio::test]
async fn architect_bypasses_gate_guard() {
    let (worktree, state) = setup("gged-arch-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "fn main() {\n    services();\n}\n")
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

    let patch = "*** Begin Patch\n*** Update File: svc.rs\n@@ fn main() @@\n fn main() {\n-    services();\n+    architected();\n }\n*** End Patch";
    let patch_args = Some(
        serde_json::json!({ "patch": patch })
            .as_object()
            .unwrap()
            .clone(),
    );
    let resp = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-a1"),
        Some("architect"),
    )
    .await
    .expect("architect apply_patch must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    // edit_forced must NOT be set.
    let session_id = worktree.path().display().to_string();
    assert!(
        !state.file_time.has_edit_forced(&session_id, &file).await,
        "edit_forced must not be set for architect"
    );
}

// ─── AC 3: role bypass — missing role (None) ─────────────────────────────

/// Missing role (None) succeeds without GateGuard for all three surfaces.
#[tokio::test]
async fn missing_role_bypasses_gate_guard_all_surfaces() {
    let (worktree, state) = setup("gged-norole-");
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

    // call_edit with None role.
    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "services",
            "new_text": "edited",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let resp = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect("missing-role edit must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    // Re-read for FileTime freshness.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");

    // call_write with None role.
    let write_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "content": "let a = written;\n",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let resp = call_write(&state, &write_args, worktree.path(), None, None, None)
        .await
        .expect("missing-role write must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));

    // Re-read for FileTime freshness.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read 2");

    // call_apply_patch with None role.
    tokio::fs::write(&file, "fn main() {\n    services();\n}\n")
        .await
        .expect("re-seed");
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read 3");
    let patch = "*** Begin Patch\n*** Update File: svc.rs\n@@ fn main() @@\n fn main() {\n-    services();\n+    patched();\n }\n*** End Patch";
    let patch_args = Some(
        serde_json::json!({ "patch": patch })
            .as_object()
            .unwrap()
            .clone(),
    );
    let resp = call_apply_patch(&state, &patch_args, worktree.path(), None, None, None)
        .await
        .expect("missing-role apply_patch must succeed");
    assert_eq!(resp["ok"], serde_json::json!(true));
}

// ─── AC 4: FORCE prompt text contains required fact demands ──────────────

/// The investigation prompt must explicitly demand importers/callers, public
/// functions/types, data schema/shape, and the verbatim task instruction —
/// not just a generic "you can't edit yet" message.
#[tokio::test]
async fn force_prompt_demands_all_four_facts() {
    let (worktree, state) = setup("gged-prompt-");
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
    let err = call_edit(
        &state,
        &edit_args,
        worktree.path(),
        None,
        Some("task-fact-1"),
        Some("worker"),
    )
    .await
    .expect_err("first edit must trigger investigation");

    // Verify the prompt contains all four required fact demands.
    assert!(
        err.contains("Importers/callers"),
        "prompt must demand importers/callers, got: {err}"
    );
    assert!(
        err.contains("public functions"),
        "prompt must demand affected public functions/types, got: {err}"
    );
    assert!(
        err.contains("data schema") || err.contains("data shape"),
        "prompt must demand data schema/shape, got: {err}"
    );
    assert!(
        err.contains("verbatim task instruction"),
        "prompt must demand verbatim task instruction, got: {err}"
    );
}

/// Same fact-demand check for the write surface.
#[tokio::test]
async fn force_prompt_demands_facts_for_write() {
    let (worktree, state) = setup("gged-prompt-w-");
    let file = worktree.path().join("config.rs");
    tokio::fs::write(&file, "let x = 1;\n").await.expect("seed");

    let read_args = Some(
        serde_json::json!({ "file_path": "config.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    let write_args = Some(
        serde_json::json!({
            "path": "config.rs",
            "content": "let x = 2;\n",
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
        Some("task-fact-w"),
        Some("worker"),
    )
    .await
    .expect_err("first write must trigger investigation");

    assert!(
        err.contains("Importers/callers"),
        "write prompt must demand importers/callers, got: {err}"
    );
    assert!(
        err.contains("public functions"),
        "write prompt must demand public functions/types, got: {err}"
    );
    assert!(
        err.contains("data schema") || err.contains("data shape"),
        "write prompt must demand data schema/shape, got: {err}"
    );
    assert!(
        err.contains("verbatim task instruction"),
        "write prompt must demand verbatim task instruction, got: {err}"
    );
}

/// Same fact-demand check for the apply_patch surface.
#[tokio::test]
async fn force_prompt_demands_facts_for_patch() {
    let (worktree, state) = setup("gged-prompt-p-");
    let file = worktree.path().join("mod.rs");
    tokio::fs::write(&file, "fn main() {\n    run();\n}\n")
        .await
        .expect("seed");

    let read_args = Some(
        serde_json::json!({ "file_path": "mod.rs" })
            .as_object()
            .unwrap()
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    let patch = "*** Begin Patch\n*** Update File: mod.rs\n@@ fn main() @@\n fn main() {\n-    run();\n+    execute();\n }\n*** End Patch";
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
        Some("task-fact-p"),
        Some("worker"),
    )
    .await
    .expect_err("first patch must trigger investigation");

    assert!(
        err.contains("Importers/callers"),
        "patch prompt must demand importers/callers, got: {err}"
    );
    assert!(
        err.contains("public functions"),
        "patch prompt must demand public functions/types, got: {err}"
    );
    assert!(
        err.contains("data schema") || err.contains("data shape"),
        "patch prompt must demand data schema/shape, got: {err}"
    );
    assert!(
        err.contains("verbatim task instruction"),
        "patch prompt must demand verbatim task instruction, got: {err}"
    );
}
