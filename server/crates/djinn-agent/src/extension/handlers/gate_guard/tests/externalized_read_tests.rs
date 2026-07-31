//! The per-turn inline-character budget in `djinn-slot` can replace an
//! already-rendered tool result with a stash stub *after* `call_read` recorded
//! coverage for it. These tests pin the agent side of that seam: the recorded
//! coverage must describe the stub the model receives, not the payload the
//! handler produced.

use super::*;
use crate::extension::handlers::downgrade_externalized_read_coverage;
use crate::output_stash::{OutputStash, externalize_rendered_tool_result, render_tool_result};

/// Default preview floor the turn budget hands to the externalization seam
/// (`djinn_slot::reply_loop::turn_budget::DEFAULT_TURN_INLINE_PREVIEW_FLOOR`).
/// Mirrored rather than imported: `djinn-agent` depends on `djinn-slot`, and
/// the constant is crate-private there.
const TURN_BUDGET_PREVIEW_FLOOR: usize = 10_000;

/// Seed a file whose whole-file read fits inside the tool-result clamp (so it
/// legitimately records `Full`) but is comfortably larger than the turn
/// budget's preview floor (so the turn budget can select it).
fn seed_modest_file(path: &std::path::Path, n: usize) {
    let mut contents = String::new();
    for i in 1..=n {
        contents.push_str(&format!(
            "    let some_reasonable_line_of_rust_code_{i} = compute(value_{i});\n"
        ));
    }
    std::fs::write(path, &contents).expect("seed file");
}

fn read_args(file_name: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    Some(
        serde_json::json!({ "file_path": file_name, "offset": 0, "limit": 2000 })
            .as_object()
            .unwrap()
            .clone(),
    )
}

fn edit_args(file_name: &str) -> Option<serde_json::Map<String, serde_json::Value>> {
    Some(
        serde_json::json!({
            "path": file_name,
            "old_text": "let some_reasonable_line_of_rust_code_7 = compute(value_7);",
            "new_text": "let some_reasonable_line_of_rust_code_7 = compute(value_8);",
        })
        .as_object()
        .unwrap()
        .clone(),
    )
}

/// Reproduce the production rendering chain for one tool result: the dispatch
/// path's `render_result`, then the turn budget's externalization seam.
/// Returns `(rendered, stub)`.
fn render_then_externalize(value: &serde_json::Value, tool_name: &str) -> (String, String) {
    let stash = std::sync::Mutex::new(OutputStash::new());
    let rendered = render_tool_result(&stash, "call-1", tool_name, value);
    let stub = externalize_rendered_tool_result(
        &stash,
        "call-1",
        tool_name,
        &rendered,
        TURN_BUDGET_PREVIEW_FLOOR,
    );
    (rendered, stub)
}

/// A read the turn budget re-externalizes must not keep `Full` coverage, and
/// the edit gate must deny on the strength of the downgrade.
///
/// Non-vacuity: the read here is a legitimate `Full` (asserted before the
/// downgrade), and the stub is asserted to carry zero lines of the file — so
/// every post-downgrade assertion is about the externalization alone.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn turn_budget_externalization_downgrades_read_coverage_and_denies_the_edit() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-ext-downgrade-");
    let file = worktree.path().join("modest.rs");
    seed_modest_file(&file, 300);
    let session_id = worktree.path().display().to_string();

    let value = call_read(&state, &read_args("modest.rs"), worktree.path())
        .await
        .expect("read should succeed");

    // Precondition: this read is a legitimate whole-file read.
    let rec = state
        .file_time
        .latest_record(&session_id, &file)
        .await
        .expect("read record");
    assert!(
        rec.is_full(),
        "a read inside both clamps must record Full, or this test proves nothing: {:?}",
        rec.coverage
    );

    let (rendered, stub) = render_then_externalize(&value, "read");
    assert!(
        stub.starts_with("[djinn-output-stash"),
        "the turn budget must have externalized this result: {}",
        &stub[..stub.len().min(120)]
    );
    // The whole point: the stub the model receives carries none of the file.
    assert_eq!(
        stub.matches("let some_reasonable_line_of_rust_code_")
            .count(),
        0,
        "the externalized stub must not carry file lines, or the coverage claim \
         would not be a lie: {stub}"
    );

    downgrade_externalized_read_coverage(&state, "read", &rendered, worktree.path()).await;

    let rec = state
        .file_time
        .latest_record(&session_id, &file)
        .await
        .expect("read record survives the downgrade");
    assert!(
        !rec.is_full(),
        "coverage must not stay Full for a result the model never received: {:?}",
        rec.coverage
    );
    assert!(
        rec.truncated,
        "an externalized read observed nothing and must be flagged truncated"
    );

    // The side effect that matters: the edit gate denies.
    let err = call_edit(
        &state,
        &edit_args("modest.rs"),
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("an externalized read must not authorize an edit");
    assert!(
        err.contains("FORCE-TRUNCATED-READ"),
        "expected the truncated-read denial, got: {err}"
    );

    let content = std::fs::read_to_string(&file).expect("read back");
    assert!(
        content.contains("let some_reasonable_line_of_rust_code_7 = compute(value_7);"),
        "the file must be unchanged after the denial"
    );
    let snap = state.file_time.gateguard_snapshot(&session_id).await;
    assert!(
        snap.edit_forced.is_empty(),
        "a denied edit must not populate edit_forced, got: {:?}",
        snap.edit_forced
    );
}

/// The legitimate `Full` case is untouched: a read that survives both clamps
/// and is *not* externalized keeps `Full` coverage and reaches the ordinary
/// first-edit investigation prompt. If this breaks, every edit in the system
/// starts demanding a re-read.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn read_that_is_not_externalized_keeps_full_coverage_and_admits_the_edit() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-ext-untouched-");
    let file = worktree.path().join("modest.rs");
    seed_modest_file(&file, 300);
    let session_id = worktree.path().display().to_string();

    let value = call_read(&state, &read_args("modest.rs"), worktree.path())
        .await
        .expect("read should succeed");

    // Under budget, the turn budget never runs the seam at all.
    let stash = std::sync::Mutex::new(OutputStash::new());
    let rendered = render_tool_result(&stash, "call-1", "read", &value);
    assert!(
        !rendered.contains("djinn-output-stash"),
        "an under-clamp read must not be stashed by render_result"
    );

    let rec = state
        .file_time
        .latest_record(&session_id, &file)
        .await
        .expect("read record");
    assert!(rec.is_full(), "expected Full, got {:?}", rec.coverage);
    assert!(!rec.truncated, "an unclamped read is not truncated");

    let err = call_edit(
        &state,
        &edit_args("modest.rs"),
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect_err("the first covered edit still hits the investigation FORCE");
    assert!(
        !err.contains("FORCE-TRUNCATED-READ") && !err.contains("FORCE-UNCOVERED-READ"),
        "a whole-file read must not be denied for coverage: {err}"
    );
    assert!(
        err.contains("GateGuard"),
        "expected the ordinary investigation prompt, got: {err}"
    );
}

/// Only `read` results carry coverage. Externalizing anything else must leave
/// the read record alone — otherwise a large `shell` result in the same turn
/// would silently revoke an unrelated read.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn externalizing_a_non_read_result_leaves_read_coverage_intact() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-ext-other-tool-");
    let file = worktree.path().join("modest.rs");
    seed_modest_file(&file, 300);
    let session_id = worktree.path().display().to_string();

    call_read(&state, &read_args("modest.rs"), worktree.path())
        .await
        .expect("read should succeed");

    let shell_value = serde_json::json!({
        "ok": true,
        "exit_code": 0,
        "stdout": "x".repeat(40_000),
        "stderr": "",
    });
    let (rendered, _stub) = render_then_externalize(&shell_value, "shell");
    downgrade_externalized_read_coverage(&state, "shell", &rendered, worktree.path()).await;

    let rec = state
        .file_time
        .latest_record(&session_id, &file)
        .await
        .expect("read record");
    assert!(
        rec.is_full(),
        "a non-read externalization must not touch read coverage: {:?}",
        rec.coverage
    );
    assert!(!rec.truncated);
}

/// `apply_patch` still lands on an ordinary file: read, investigate, patch.
/// This is the deadlock #2821 removed — the downgrade must not re-arm it for
/// results the turn budget never touched.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn apply_patch_still_succeeds_on_a_normal_file() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup_worktree("gg-ext-patch-ok-");
    let file = worktree.path().join("modest.rs");
    seed_modest_file(&file, 300);

    call_read(&state, &read_args("modest.rs"), worktree.path())
        .await
        .expect("read should succeed");

    let patch = "*** Begin Patch\n\
                 *** Update File: modest.rs\n\
                 @@     let some_reasonable_line_of_rust_code_7 = compute(value_7);\n\
                 -    let some_reasonable_line_of_rust_code_7 = compute(value_7);\n\
                 +    let some_reasonable_line_of_rust_code_7 = compute(value_777);\n\
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
    .expect_err("the first covered edit hits the investigation FORCE");
    assert!(
        !err.contains("FORCE-UNCOVERED-READ") && !err.contains("FORCE-TRUNCATED-READ"),
        "apply_patch on a freshly read file must not be denied for coverage: {err}"
    );

    // The patch invalidated nothing (it was denied), but the investigation
    // gate is now satisfied; re-read as the worker would and patch for real.
    call_read(&state, &read_args("modest.rs"), worktree.path())
        .await
        .expect("re-read");
    call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("apply_patch must succeed once the investigation gate is satisfied");

    let content = std::fs::read_to_string(&file).expect("read back");
    assert!(
        content.contains("let some_reasonable_line_of_rust_code_7 = compute(value_777);"),
        "the patch must have been applied"
    );
}
