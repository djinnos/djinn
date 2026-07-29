//! Unicode / CRLF / guard-rejection and apply_patch role-plumbing cases for the
//! edit dispatch path. Split out of `edit_dispatch_tests.rs` to stay within the
//! repository file-size guard (MAX_LINES=1500 / MAX_BYTES=51200); shares the same
//! harness via `use super::*`.

use super::*;

/// Unicode dispatch-level test: multi-byte characters in unchanged spans
/// are preserved byte-for-byte after a successful edit. The match uses an
/// exact ASCII word that sits between multi-byte Unicode content.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn edit_unicode_success_preserves_multibyte_unchanged_spans() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-unicode-dsp-");
    let file = worktree.path().join("comment.rs");
    // Content: multi-byte Unicode in prefix and suffix, ASCII target in middle.
    let content = "// \u{201C}smart\u{201D} \u{2014} note\nlet x = target;\nlet y = \u{4E16};\n";
    tokio::fs::write(&file, content).await.expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let read_args = Some(
        serde_json::json!({ "file_path": "comment.rs" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    // Exact match for the ASCII word — no Unicode normalization needed.
    // The point is that multi-byte surrounding chars survive the replacement.
    let edit_args = Some(
        serde_json::json!({
            "path": "comment.rs",
            "old_text": "target",
            "new_text": "done",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let response = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect("edit should succeed");

    assert_eq!(response["ok"], serde_json::json!(true));

    // Verify the file preserves multi-byte chars in unchanged spans.
    let after = tokio::fs::read_to_string(&file).await.expect("read back");
    assert!(
        after.contains('\u{201C}'),
        "left smart quote must be preserved in output: {after:?}"
    );
    assert!(
        after.contains('\u{201D}'),
        "right smart quote must be preserved in output: {after:?}"
    );
    assert!(
        after.contains('\u{2014}'),
        "em dash must be preserved in output: {after:?}"
    );
    assert!(
        after.contains('\u{4E16}'),
        "CJK character must be preserved in output: {after:?}"
    );
    assert!(after.contains("done"), "replacement must be applied");

    // Byte offsets in edit_match must be valid UTF-8 boundaries.
    let em = response.get("edit_match").expect("must have edit_match");
    let range = em["matched_byte_range"]
        .as_array()
        .expect("matched_byte_range is array");
    let start = range[0].as_u64().unwrap() as usize;
    let end = range[1].as_u64().unwrap() as usize;
    assert!(
        content.is_char_boundary(start),
        "matched_byte_range.start ({start}) must be a UTF-8 char boundary"
    );
    assert!(
        content.is_char_boundary(end),
        "matched_byte_range.end ({end}) must be a UTF-8 char boundary"
    );
}

// ── CRLF dispatch-level tests ────────────────────────────────────────────

/// CRLF file with exact CRLF match at dispatch level: success and CRLF
/// preserved in written output (no silent LF conversion).
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn edit_crlf_success_preserves_crlf_in_output() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-crlf-dsp-");
    let file = worktree.path().join("data.txt");
    let content = "line one\r\nline two\r\nline three\r\n";
    tokio::fs::write(&file, content).await.expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let read_args = Some(
        serde_json::json!({ "file_path": "data.txt" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    // old_text uses CRLF (exact match). new_text also uses CRLF.
    let edit_args = Some(
        serde_json::json!({
            "path": "data.txt",
            "old_text": "line one\r\nline two\r\n",
            "new_text": "replaced\r\n",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let response = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect("exact CRLF edit should succeed");

    assert_eq!(response["ok"], serde_json::json!(true));

    // File must retain CRLF in unchanged spans.
    let after = tokio::fs::read_to_string(&file).await.expect("read back");
    assert!(
        after.contains("line three\r\n"),
        "CRLF must be preserved in unchanged suffix: {after:?}"
    );
    assert!(
        after.contains("replaced\r\n"),
        "replacement with CRLF applied: {after:?}"
    );
    // No bare LF in the suffix (LF must always follow CR).
    if let Some(idx) = after.find("line three") {
        let suffix = &after[idx..];
        assert!(
            !suffix.contains("\n\r"),
            "no reversed CRLF sequences: {suffix:?}"
        );
    }
}

/// CRLF guard rejection at dispatch level: file is NOT modified.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn edit_crlf_guard_rejected_does_not_modify_file() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-crlf-guard-dsp-");
    let file = worktree.path().join("data.txt");
    let content = "line one\r\nline two\r\n";
    tokio::fs::write(&file, content).await.expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let read_args = Some(
        serde_json::json!({ "file_path": "data.txt" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    // old_text uses LF — guard must reject because CRLF would be silently
    // rewritten to LF in unchanged spans.
    let edit_args = Some(
        serde_json::json!({
            "path": "data.txt",
            "old_text": "line one\nline two",
            "new_text": "replaced",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let err = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect_err("CRLF guard rejection must return error");

    assert!(
        err.contains("rejected by safety guard"),
        "error must mention guard rejection: {err}"
    );
    assert!(
        err.contains("\"guard_rejected\""),
        "structured guard_rejected outcome: {err}"
    );

    // File must NOT be modified.
    let after = tokio::fs::read_to_string(&file).await.expect("read back");
    assert_eq!(
        after, content,
        "file must not be modified on guard rejection"
    );
}

// ── Escape guard rejection at dispatch level ─────────────────────────────

/// Escape guard rejection at dispatch level: quote imbalance causes guard
/// rejection; file is NOT modified.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn edit_escape_guard_rejected_does_not_modify_file() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-esc-guard-");
    let file = worktree.path().join("str.rs");
    // Content has escaped quotes; the old_text crosses a quote boundary.
    let content = "let a = \"x\"; let b = \\\"x\\\";\n";
    tokio::fs::write(&file, content).await.expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let read_args = Some(
        serde_json::json!({ "file_path": "str.rs" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    // old_text crosses an escape boundary → guard rejects.
    let edit_args = Some(
        serde_json::json!({
            "path": "str.rs",
            "old_text": "\"x\"; let b = \"x\"",
            "new_text": "replaced",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let err = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect_err("escape guard rejection must return error");

    assert!(
        err.contains("rejected by safety guard"),
        "error must mention guard rejection: {err}"
    );

    // File must NOT be modified.
    let after = tokio::fs::read_to_string(&file).await.expect("read back");
    assert_eq!(
        after, content,
        "file must not be modified on escape guard rejection"
    );
}

// ── Ambiguous non-exact at dispatch level ─────────────────────────────────

/// Ambiguous at a non-exact strategy (trimmed_boundary): file is NOT modified.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn edit_ambiguous_trimmed_boundary_does_not_modify_file() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-ambig-tb-");
    let file = worktree.path().join("dup.rs");
    // Inner content "let x = 1;" appears twice; old_text has boundary
    // whitespace lines that defeat exact match → trimmed_boundary ambiguity.
    let content = "let x = 1;\n\nlet x = 1;\n";
    tokio::fs::write(&file, content).await.expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let read_args = Some(
        serde_json::json!({ "file_path": "dup.rs" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    let edit_args = Some(
        serde_json::json!({
            "path": "dup.rs",
            "old_text": "   \n\nlet x = 1;\n   \n",
            "new_text": "replaced",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let err = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect_err("ambiguous trimmed-boundary edit must return error");

    assert!(
        err.contains("appears") && err.contains("times"),
        "error must contain ambiguity info: {err}"
    );
    assert!(
        err.contains("\"ambiguous\""),
        "structured ambiguous outcome: {err}"
    );

    // File must NOT be modified.
    let after = tokio::fs::read_to_string(&file).await.expect("read back");
    assert_eq!(after, content, "file must not be modified on ambiguity");
}

// ── No-match nearest-miss metadata at dispatch level ─────────────────────

/// No-match at dispatch level: verify nearest_miss score is a reasonable
/// number (between 0 and 1) and file is not modified.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn edit_no_match_nearest_miss_score_is_reasonable() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-nomatch-score-");
    let file = worktree.path().join("svc.rs");
    let content = "function process_data(input) {\n    return input;\n}\n";
    tokio::fs::write(&file, content).await.expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    // Close but not matching — should have a high nearest_miss score.
    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "function process_data(output) {\n    return output;\n}",
            "new_text": "replaced",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let err = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect_err("no-match edit must return error");

    assert!(err.contains("not found"), "error must say not found: {err}");
    assert!(
        err.contains("nearest_miss"),
        "error must include nearest_miss: {err}"
    );

    // Parse the structured details to verify the score is reasonable.
    // The error format is: "old_text not found in file: <path> {json}"
    let json_start = err.find('{').expect("error must contain JSON details");
    let details: serde_json::Value =
        serde_json::from_str(&err[json_start..]).expect("must parse JSON details");
    let score = details["edit_match"]["nearest_miss"]
        .as_f64()
        .expect("nearest_miss must be a float");
    assert!(
        (0.0..=1.0).contains(&score),
        "nearest_miss score must be in [0, 1]: {score}"
    );
    assert!(
        score > 0.3,
        "partial overlap should have score > 0.3: {score}"
    );

    // File must NOT be modified.
    let after = tokio::fs::read_to_string(&file).await.expect("read back");
    assert_eq!(after, content, "file must not be modified on no-match");
}

/// Regression: `call_apply_patch` accepts `session_task_id` and `session_role`
/// (plumbed consistently with `call_edit`). GateGuard denies the first worker
/// edit; retry after re-read succeeds. Full gate-guard behaviour is covered in
/// `gate_guard::tests`.
#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn apply_patch_accepts_worker_role_plumbing() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-patch-worker-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "fn main() {\n    old();\n}\n")
        .await
        .expect("seed");
    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());
    let read_args = Some(
        serde_json::json!({"file_path":"svc.rs"})
            .as_object()
            .expect("o")
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");
    let patch_args = Some(serde_json::json!({"patch":"*** Begin Patch\n*** Update File: svc.rs\n@@ fn main() @@\n fn main() {\n-    old();\n+    new();\n }\n*** End Patch"}).as_object().expect("o").clone());
    // First call: GateGuard FORCE prompt (plumbed params hit the gate).
    let err = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-abc"),
        Some("worker"),
    )
    .await
    .expect_err("GateGuard must deny");
    assert!(err.contains("GateGuard"), "must mention GateGuard: {err}");
    // Re-read + retry: succeeds (edit_forced set).
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");
    let response = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-abc"),
        Some("worker"),
    )
    .await
    .expect("retry succeeds");
    assert_eq!(response.get("ok").and_then(|v| v.as_bool()), Some(true));
    let after = tokio::fs::read_to_string(&file).await.expect("read back");
    assert!(after.contains("new()"), "patched content missing: {after}");
}
