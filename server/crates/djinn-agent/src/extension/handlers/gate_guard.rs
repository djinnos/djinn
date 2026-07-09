use std::path::Path;

use crate::context::AgentContext;

/// Investigation prompt shown to the worker on the FIRST covered edit per
/// (path, live-session). Demands the four facts the arbiter needs before
/// allowing mutation: importers/callers, affected public API, data shapes,
/// and the verbatim task instruction.
pub(crate) const GATEGUARD_FORCE_INVESTIGATION_PROMPT: &str = "\
GateGuard: Before you modify this file, provide:
1. Importers/callers of the code you are about to change.
2. Affected public functions, types, and traits.
3. Any data schema or shape that is touched.
4. The verbatim task instruction.
After providing this information, re-read the file and retry the edit.";

/// GateGuard edit-check helper for worker sessions.
///
/// Returns `Ok(())` to allow the mutation, or `Err(String)` to deny it.
/// Non-worker roles and missing roles pass through unconditionally.
///
/// Call this AFTER `file_time.assert(...)` succeeds AND after the successful
/// match byte range is known, but BEFORE writing the new content.
pub(crate) async fn gate_guard_edit_check(
    state: &AgentContext,
    session_role: Option<&str>,
    session_id: &str,
    path: &Path,
    mutation_byte_range: std::ops::Range<usize>,
) -> Result<(), String> {
    // Only gate worker sessions; all other roles keep current behavior.
    if session_role != Some("worker") {
        return Ok(());
    }

    // Look up the latest read record for coverage/truncation checks.
    if let Some(record) = state.file_time.latest_record(session_id, path).await {
        // Convert usize byte range to u64 for covers_span.
        let span_start = mutation_byte_range.start as u64;
        let span_end = mutation_byte_range.end as u64;

        if record.truncated {
            // Truncated read: deny, do NOT mark edit_forced, and record the
            // truncated diagnostic so repeated denials are consistent.
            return Err(format!(
                "FORCE-TRUNCATED-READ: The latest read of {} was truncated \
                 and did not observe the full file. You MUST re-read the \
                 entire file before editing.",
                path.display(),
            ));
        }

        if !record.covers_span(span_start, span_end) {
            // Uncovered span: same denial as truncated — the worker hasn't
            // seen the area it's trying to mutate.
            return Err(format!(
                "FORCE-UNCOVERED-READ: The latest read of {} does not cover \
                 the byte range [{}, {}) you are trying to edit. You MUST \
                 re-read the file with full coverage before editing.",
                path.display(),
                span_start,
                span_end,
            ));
        }

        // Covered, non-truncated read: check first-edit investigation gate.
        if !state.file_time.has_edit_forced(session_id, path).await {
            state.file_time.mark_edit_forced(session_id, path).await;
            return Err(GATEGUARD_FORCE_INVESTIGATION_PROMPT.to_string());
        }

        // edit_forced already set — allow this and all subsequent edits.
        Ok(())
    } else {
        // No read record at all should have been caught by the earlier
        // `file_time.assert(...)` call, but guard defensively.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extension::handlers::workspace::{
        call_apply_patch, call_edit, call_read, call_write,
    };
    use crate::file_time::ReadCoverage;
    use crate::test_helpers::{agent_context_from_db, create_test_db};
    use tokio_util::sync::CancellationToken;

    fn setup_worktree(prefix: &str) -> (tempfile::TempDir, AgentContext) {
        let dir = crate::test_helpers::test_tempdir(prefix);
        let db = create_test_db();
        let state = agent_context_from_db(db, CancellationToken::new());
        (dir, state)
    }

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
}
