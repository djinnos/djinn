use super::*;

/// Regression for the apply_patch "context mismatch" loop: after a
/// successful apply_patch, a SECOND patch to the same file WITHOUT an
/// intervening read must be rejected with the "read again" guard — NOT
/// silently allowed to match against the now-stale post-patch content
/// (which produced "context mismatch" and an infinite retry loop). The
/// read record is invalidated on every successful modify so the model is
/// forced to re-read and rebuild its patch from current content.
#[tokio::test]
async fn apply_patch_twice_without_reread_forces_reread() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-patch-reread-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "fn main() {\n    services();\n}\n")
        .await
        .expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    // Read first to satisfy read-before-modify.
    let read_args = Some(
        serde_json::json!({ "file_path": "svc.rs" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    // First patch succeeds: services() -> collections_query_registration().
    let patch1 = "*** Begin Patch\n*** Update File: svc.rs\n@@ fn main() @@\n fn main() {\n-    services();\n+    collections_query_registration();\n }\n*** End Patch";
    let patch1_args = Some(
        serde_json::json!({ "patch": patch1 })
            .as_object()
            .expect("obj")
            .clone(),
    );
    call_apply_patch(&state, &patch1_args, worktree.path(), None, None, None)
        .await
        .expect("first apply_patch should succeed");

    // Second patch WITHOUT a re-read, whose context still references the
    // OLD `services()` line. Pre-fix this would reach the patch engine and
    // fail with "context mismatch" → retry loop. Post-fix it's rejected up
    // front with the friendly "read again" guard.
    let patch2 = "*** Begin Patch\n*** Update File: svc.rs\n@@ fn main() @@\n fn main() {\n-    services();\n+    other();\n }\n*** End Patch";
    let patch2_args = Some(
        serde_json::json!({ "patch": patch2 })
            .as_object()
            .expect("obj")
            .clone(),
    );
    let err = call_apply_patch(&state, &patch2_args, worktree.path(), None, None, None)
        .await
        .expect_err("second patch without re-read must be rejected");
    assert!(
        err.contains("modified since last read") || err.contains("read"),
        "expected re-read guard, got: {err}"
    );
    assert!(
        !err.contains("context mismatch"),
        "must not surface a stale-context error; got: {err}"
    );
}

/// read -> edit -> edit (no intervening re-read) must force a re-read on
/// the second edit. Same invalidate-on-modify contract as apply_patch.
#[tokio::test]
async fn edit_twice_without_reread_forces_reread() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-reread-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\n")
        .await
        .expect("seed file");

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

    let edit1 = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "services",
            "new_text": "collections_query_registration",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    call_edit(&state, &edit1, worktree.path(), None, None, None)
        .await
        .expect("first edit should succeed");

    // Second edit without a re-read — must be rejected, forcing a re-read.
    let edit2 = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "collections_query_registration",
            "new_text": "other",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let err = call_edit(&state, &edit2, worktree.path(), None, None, None)
        .await
        .expect_err("second edit without re-read must be rejected");
    assert!(
        err.contains("modified since last read") || err.contains("read"),
        "expected re-read guard, got: {err}"
    );

    // After re-reading, the edit against current content succeeds.
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("re-read");
    call_edit(&state, &edit2, worktree.path(), None, None, None)
        .await
        .expect("edit after re-read should succeed");
}

/// `call_edit` without any prior read fails with the read-before-edit
/// guidance and does NOT reach the matcher at all (no file write occurs).
#[tokio::test]
async fn edit_without_prior_read_fails_with_guidance() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-noread-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = 1;\n")
        .await
        .expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    // No read call — jump straight to edit.
    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "let a = 1;",
            "new_text": "let b = 2;",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let err = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect_err("edit without prior read must be rejected");

    // Must surface the read-before-edit guidance.
    assert!(
        err.contains("must read") || err.contains("read") || err.contains("Read"),
        "error must include read-before-edit guidance, got: {err}"
    );

    // File must NOT be modified.
    let after = tokio::fs::read_to_string(&file).await.expect("read back");
    assert_eq!(
        after, "let a = 1;\n",
        "file must not be modified when no read record exists"
    );
}

/// `call_edit` with typed matcher: success response includes `edit_match`
/// metadata with strategy, byte/line ranges, byte counts, reindented flag,
/// and match note.
#[tokio::test]
async fn edit_success_includes_edit_match_metadata() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-match-meta-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\nlet b = 42;\n")
        .await
        .expect("seed file");

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

    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "services",
            "new_text": "collections_query_registration",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let response = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect("edit should succeed");

    assert_eq!(response["ok"], serde_json::json!(true));
    assert_eq!(
        response["path"],
        serde_json::json!(file.display().to_string())
    );
    assert!(
        response.get("diagnostics").is_some(),
        "must include diagnostics"
    );

    // edit_match metadata
    let em = response
        .get("edit_match")
        .expect("response must include edit_match");
    assert!(
        em["strategy"].as_str().is_some(),
        "edit_match.strategy must be a string"
    );
    // The match was exact
    assert_eq!(em["strategy"], serde_json::json!("exact"));
    assert!(
        em["matched_byte_range"].is_array(),
        "must have matched_byte_range"
    );
    assert!(em["old_bytes"].as_u64().is_some(), "must have old_bytes");
    assert!(em["new_bytes"].as_u64().is_some(), "must have new_bytes");
    assert!(
        em["matched_bytes"].as_u64().is_some(),
        "must have matched_bytes"
    );
    assert_eq!(em["reindented"], serde_json::json!(false));
    // Exact match → no match_note on compat layer
    assert!(
        response.get("match_note").is_none(),
        "exact match has no match_note"
    );
}

/// `call_edit` with a line-trimmed match includes a match_note
/// and the strategy in edit_match.
#[tokio::test]
async fn edit_fuzzy_match_includes_strategy_and_note() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-fuzzy-");
    let file = worktree.path().join("svc.rs");
    // Content has trailing whitespace that prevents exact match but allows
    // line_trimmed match.
    let content = "fn hello() {\n    let a = services;   \n}\n";
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

    // old_text without trailing spaces — triggers line_trimmed strategy
    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "fn hello() {\n    let a = services;\n}",
            "new_text": "fn hello() {\n    let b = done;\n}",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let response = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect("fuzzy edit should succeed");

    assert_eq!(response["ok"], serde_json::json!(true));
    // Should have a match_note for non-exact strategy
    let note = response
        .get("match_note")
        .and_then(|v| v.as_str())
        .expect("fuzzy match should have match_note");
    assert!(!note.is_empty(), "match_note must not be empty");
    // edit_match.strategy should be a non-exact strategy
    let strategy = response["edit_match"]["strategy"]
        .as_str()
        .expect("edit_match.strategy");
    assert_ne!(
        strategy, "exact",
        "fuzzy match should use a non-exact strategy"
    );
}

/// `call_edit` ambiguous outcome: file is NOT modified, error is returned.
#[tokio::test]
async fn edit_ambiguous_does_not_modify_file() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-ambig-");
    let file = worktree.path().join("svc.rs");
    let content = "foo bar\nfoo bar\n";
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

    // "foo bar" appears twice → ambiguous
    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "foo bar",
            "new_text": "replaced",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let err = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect_err("ambiguous edit must return error");

    // Leading compatibility text preserved.
    assert!(
        err.contains("appears") && err.contains("times"),
        "error must contain 'appears N times', got: {err}"
    );
    // Structured details present.
    assert!(
        err.contains("\"ambiguous\""),
        "error must include structured 'ambiguous' outcome, got: {err}"
    );

    // File must NOT be modified.
    let after = tokio::fs::read_to_string(&file).await.expect("read back");
    assert_eq!(
        after, content,
        "file must not be modified on ambiguous outcome"
    );
}

/// `call_edit` no-match outcome: file is NOT modified, error is returned
/// with leading compatibility text and structured nearest_miss.
#[tokio::test]
async fn edit_no_match_does_not_modify_file() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-nomatch-");
    let file = worktree.path().join("svc.rs");
    let content = "let a = 1;\n";
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

    // Text that does not exist in the file at all.
    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "this text does not exist in the file anywhere",
            "new_text": "replacement",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let err = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect_err("no-match edit must return error");

    // Leading compatibility text.
    assert!(
        err.contains("old_text not found in file"),
        "error must start with 'old_text not found in file', got: {err}"
    );
    // Structured details present.
    assert!(
        err.contains("\"no_match\""),
        "error must include structured 'no_match' outcome, got: {err}"
    );
    assert!(
        err.contains("nearest_miss"),
        "error must include nearest_miss field, got: {err}"
    );

    // File must NOT be modified.
    let after = tokio::fs::read_to_string(&file).await.expect("read back");
    assert_eq!(
        after, content,
        "file must not be modified on no-match outcome"
    );
}

/// `call_edit` guard-rejected outcome: file is NOT modified, error is
/// returned with leading compatibility text and structured guard details.
/// Uses CRLF content with LF old_text to trigger the CRLF preservation guard.
#[tokio::test]
async fn edit_guard_rejected_does_not_modify_file() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-guard-");
    let file = worktree.path().join("svc.rs");
    // Content uses CRLF line endings.
    let content = "line one\r\nline two\r\n";
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

    // old_text uses \n — whitespace normalization finds the candidate but the
    // CRLF preservation guard rejects the match.
    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "line one\nline two",
            "new_text": "replaced",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let err = call_edit(&state, &edit_args, worktree.path(), None, None, None)
        .await
        .expect_err("guard-rejected edit must return error");

    // Leading compatibility text preserved.
    assert!(
        err.contains("rejected by safety guard"),
        "error must contain 'rejected by safety guard', got: {err}"
    );
    // Structured details present.
    assert!(
        err.contains("\"guard_rejected\""),
        "error must include structured 'guard_rejected' outcome, got: {err}"
    );
    assert!(
        err.contains("guard_reason"),
        "error must include guard_reason field, got: {err}"
    );

    // File must NOT be modified.
    let after = tokio::fs::read_to_string(&file).await.expect("read back");
    assert_eq!(
        after, content,
        "file must not be modified on guard-rejected outcome"
    );
}

/// `call_edit` success preserves read-before-edit freshness guard.
#[tokio::test]
async fn edit_success_preserves_read_before_edit_freshness() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-fresh-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "fn main() {}\n")
        .await
        .expect("seed file");

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

    // First edit succeeds.
    let edit1 = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "fn main() {}",
            "new_text": "fn main() { println!(\"hello\"); }",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let resp = call_edit(&state, &edit1, worktree.path(), None, None, None)
        .await
        .expect("first edit");
    assert_eq!(resp["ok"], serde_json::json!(true));
    assert!(resp.get("edit_match").is_some(), "must have edit_match");

    // Second edit without re-read must be rejected (read-before-edit guard).
    let edit2 = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "fn main() { println!(\"hello\"); }",
            "new_text": "fn main() { println!(\"world\"); }",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );
    let err = call_edit(&state, &edit2, worktree.path(), None, None, None)
        .await
        .expect_err("second edit without re-read must be rejected");
    assert!(
        err.contains("modified since last read") || err.contains("read"),
        "expected re-read guard, got: {err}"
    );
}

// ── Telemetry event capture helpers ───────────────────────────────────────

use std::collections::HashMap;
use std::sync::{Arc, Mutex as StdMutex};
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{Layer, registry::LookupSpan};

#[derive(Clone, Debug, Default)]
struct CapturedEvent {
    fields: HashMap<String, String>,
}

#[derive(Default, Clone)]
struct EventCaptureLayer {
    events: Arc<StdMutex<Vec<CapturedEvent>>>,
}

impl EventCaptureLayer {
    fn events(&self) -> Vec<CapturedEvent> {
        self.events.lock().expect("event capture mutex").clone()
    }
}

#[derive(Default)]
struct FieldVisitor {
    fields: HashMap<String, String>,
}

impl Visit for FieldVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields.insert(
            field.name().to_owned(),
            format!("{value:?}").trim_matches('"').to_owned(),
        );
    }
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_owned(), value.to_string());
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_owned(), value.to_owned());
    }
}

impl<S> Layer<S> for EventCaptureLayer
where
    S: tracing::Subscriber,
    S: for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = FieldVisitor::default();
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("event capture mutex")
            .push(CapturedEvent {
                fields: visitor.fields,
            });
    }
}

/// Install a temporary global tracing subscriber that captures events,
/// run the async closure, then return captured events.
///
/// Uses a `OnceLock`-guarded mutex to serialize access across concurrent
/// tests (only one global subscriber can be active at a time).
async fn with_captured_events<F, Fut>(f: F) -> Vec<CapturedEvent>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    use std::sync::OnceLock;
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    let _guard = LOCK
        .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await;

    let layer = EventCaptureLayer::default();
    let subscriber = tracing_subscriber::registry().with(layer.clone());

    // Use `set_global` only if no global subscriber is set; otherwise
    // just run without capturing (best-effort).
    let _guard = tracing::subscriber::set_default(subscriber);
    f().await;
    layer.events()
}

/// Success outcome emits `edit_match_outcome` and `edit_match_strategy`
/// events with all expected fields populated.
#[tokio::test]
async fn edit_success_emits_telemetry_events() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-telem-success-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = services;\n")
        .await
        .expect("seed file");

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

    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "services",
            "new_text": "collections_query_registration",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );

    let events = with_captured_events(|| async {
        let _ = call_edit(
            &state,
            &edit_args,
            worktree.path(),
            None,
            Some("task-abc"),
            Some("worker"),
        )
        .await;
    })
    .await;

    // Filter to our edit_match events.
    let outcome_events: Vec<_> = events
        .iter()
        .filter(|e| e.fields.get("event_name").map(|s| s.as_str()) == Some("edit_match_outcome"))
        .collect();
    assert_eq!(
        outcome_events.len(),
        1,
        "expected exactly one edit_match_outcome event, got {}",
        outcome_events.len()
    );

    let strategy_events: Vec<_> = events
        .iter()
        .filter(|e| e.fields.get("event_name").map(|s| s.as_str()) == Some("edit_match_strategy"))
        .collect();
    assert_eq!(
        strategy_events.len(),
        1,
        "expected exactly one edit_match_strategy event for success, got {}",
        strategy_events.len()
    );

    // Verify fields on the outcome event.
    let evt = &outcome_events[0];
    assert_eq!(evt.fields["task_id"], "task-abc");
    assert_eq!(evt.fields["agent_role"], "worker");
    assert_eq!(evt.fields["tool_name"], "edit");
    assert_eq!(evt.fields["path_ext"], "rs");
    assert_eq!(evt.fields["outcome"], "success");
    assert_eq!(evt.fields["strategy"], "exact");
    assert_eq!(evt.fields["candidate_count"], "1");
    assert_eq!(evt.fields["reindented"], "false");
    // old_bytes / new_bytes should be numeric
    assert!(
        evt.fields.contains_key("old_bytes"),
        "must have old_bytes field"
    );
    assert!(
        evt.fields.contains_key("new_bytes"),
        "must have new_bytes field"
    );
    assert!(
        evt.fields.contains_key("matched_bytes"),
        "must have matched_bytes field"
    );

    // The strategy event should have the same fields.
    let sevt = &strategy_events[0];
    assert_eq!(sevt.fields["event_name"], "edit_match_strategy");
    assert_eq!(sevt.fields["strategy"], "exact");
    assert_eq!(sevt.fields["outcome"], "success");
}

/// No-match outcome emits `edit_match_outcome` but NOT `edit_match_strategy`.
#[tokio::test]
async fn edit_no_match_emits_telemetry_outcome_only() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-telem-nomatch-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "let a = 1;\n")
        .await
        .expect("seed file");

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

    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "this text does not exist anywhere",
            "new_text": "replacement",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );

    let events = with_captured_events(|| async {
        let _ = call_edit(
            &state,
            &edit_args,
            worktree.path(),
            None,
            Some("task-xyz"),
            Some("reviewer"),
        )
        .await;
    })
    .await;

    let outcome_events: Vec<_> = events
        .iter()
        .filter(|e| e.fields.get("event_name").map(|s| s.as_str()) == Some("edit_match_outcome"))
        .collect();
    assert_eq!(
        outcome_events.len(),
        1,
        "expected exactly one edit_match_outcome event for no-match, got {}",
        outcome_events.len()
    );

    // No success → no edit_match_strategy event.
    let strategy_events: Vec<_> = events
        .iter()
        .filter(|e| e.fields.get("event_name").map(|s| s.as_str()) == Some("edit_match_strategy"))
        .collect();
    assert!(
        strategy_events.is_empty(),
        "non-success must NOT emit edit_match_strategy"
    );

    let evt = &outcome_events[0];
    assert_eq!(evt.fields["task_id"], "task-xyz");
    assert_eq!(evt.fields["agent_role"], "reviewer");
    assert_eq!(evt.fields["tool_name"], "edit");
    assert_eq!(evt.fields["path_ext"], "rs");
    assert_eq!(evt.fields["outcome"], "no_match");
    assert!(
        evt.fields.contains_key("score"),
        "no_match must have score (nearest_miss)"
    );
}

/// Guard-rejected outcome emits `edit_match_outcome` but NOT
/// `edit_match_strategy`. Verifies guard and candidate_count fields.
#[tokio::test]
async fn edit_guard_rejected_emits_telemetry_outcome_only() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-telem-guard-");
    let file = worktree.path().join("svc.rs");
    // CRLF content triggers guard rejection.
    tokio::fs::write(&file, "line one\r\nline two\r\n")
        .await
        .expect("seed file");

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

    let edit_args = Some(
        serde_json::json!({
            "path": "svc.rs",
            "old_text": "line one\nline two",
            "new_text": "replaced",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );

    let events = with_captured_events(|| async {
        let _ = call_edit(
            &state,
            &edit_args,
            worktree.path(),
            None,
            Some("task-guard"),
            Some("worker"),
        )
        .await;
    })
    .await;

    let outcome_events: Vec<_> = events
        .iter()
        .filter(|e| e.fields.get("event_name").map(|s| s.as_str()) == Some("edit_match_outcome"))
        .collect();
    assert_eq!(
        outcome_events.len(),
        1,
        "expected exactly one edit_match_outcome event for guard-rejected, got {}",
        outcome_events.len()
    );

    // No success → no edit_match_strategy event.
    let strategy_events: Vec<_> = events
        .iter()
        .filter(|e| e.fields.get("event_name").map(|s| s.as_str()) == Some("edit_match_strategy"))
        .collect();
    assert!(
        strategy_events.is_empty(),
        "guard-rejected must NOT emit edit_match_strategy"
    );

    let evt = &outcome_events[0];
    assert_eq!(evt.fields["task_id"], "task-guard");
    assert_eq!(evt.fields["agent_role"], "worker");
    assert_eq!(evt.fields["tool_name"], "edit");
    assert_eq!(evt.fields["path_ext"], "rs");
    assert_eq!(evt.fields["outcome"], "guard_rejected");
    assert!(
        evt.fields.contains_key("guard") && !evt.fields["guard"].is_empty(),
        "guard-rejected must have non-empty guard field"
    );
    assert!(
        evt.fields.contains_key("candidate_count"),
        "must have candidate_count"
    );
}

/// Telemetry events for a successful edit include the `session_id` field
/// and do NOT leak the full file path or file content.
#[tokio::test]
async fn edit_telemetry_success_includes_session_id_and_no_leaks() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-teem-leak-");
    let file = worktree.path().join("sensitive_data.rs");
    let content = "const SECRET: &str = \"hunter2\";\n";
    tokio::fs::write(&file, content).await.expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let read_args = Some(
        serde_json::json!({ "file_path": "sensitive_data.rs" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    let edit_args = Some(
        serde_json::json!({
            "path": "sensitive_data.rs",
            "old_text": "SECRET",
            "new_text": "TOKEN",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );

    let events = with_captured_events(|| async {
        let _ = call_edit(
            &state,
            &edit_args,
            worktree.path(),
            None,
            Some("task-leak-check"),
            Some("worker"),
        )
        .await;
    })
    .await;

    let outcome_events: Vec<_> = events
        .iter()
        .filter(|e| e.fields.get("event_name").map(|s| s.as_str()) == Some("edit_match_outcome"))
        .collect();
    assert_eq!(outcome_events.len(), 1);

    let evt = &outcome_events[0];

    // session_id field must be present and match the task_id.
    assert!(
        evt.fields.contains_key("session_id"),
        "telemetry must include session_id"
    );
    assert_eq!(evt.fields["session_id"], "task-leak-check");

    // No field value should contain the full file path or filename.
    for (key, value) in &evt.fields {
        assert!(
            !value.contains("sensitive_data.rs"),
            "field '{key}' must not leak file name, got: {value}"
        );
        assert!(
            !value.contains(&file.display().to_string()),
            "field '{key}' must not leak full path, got: {value}"
        );
    }

    // No field value should contain file content.
    for (key, value) in &evt.fields {
        assert!(
            !value.contains("hunter2"),
            "field '{key}' must not leak file content, got: {value}"
        );
    }
}

/// Telemetry events for a non-success outcome also do NOT leak the full
/// file path or file content.
#[tokio::test]
async fn edit_telemetry_no_match_no_leaks() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-edit-teem-noleak-");
    let file = worktree.path().join("credentials.rs");
    let content = "const API_KEY: &str = \"supersecret\";\n";
    tokio::fs::write(&file, content).await.expect("seed file");

    let state =
        crate::test_helpers::agent_context_from_db(create_test_db(), CancellationToken::new());

    let read_args = Some(
        serde_json::json!({ "file_path": "credentials.rs" })
            .as_object()
            .expect("obj")
            .clone(),
    );
    call_read(&state, &read_args, worktree.path())
        .await
        .expect("read");

    let edit_args = Some(
        serde_json::json!({
            "path": "credentials.rs",
            "old_text": "this text does not exist anywhere",
            "new_text": "replacement",
        })
        .as_object()
        .expect("obj")
        .clone(),
    );

    let events = with_captured_events(|| async {
        let _ = call_edit(
            &state,
            &edit_args,
            worktree.path(),
            None,
            Some("task-noleak"),
            Some("reviewer"),
        )
        .await;
    })
    .await;

    let outcome_events: Vec<_> = events
        .iter()
        .filter(|e| e.fields.get("event_name").map(|s| s.as_str()) == Some("edit_match_outcome"))
        .collect();
    assert_eq!(outcome_events.len(), 1);

    let evt = &outcome_events[0];

    // session_id must be present.
    assert!(
        evt.fields.contains_key("session_id"),
        "telemetry must include session_id"
    );
    assert_eq!(evt.fields["session_id"], "task-noleak");

    // No field value should contain the full file path or filename.
    for (key, value) in &evt.fields {
        assert!(
            !value.contains("credentials.rs"),
            "field '{key}' must not leak file name, got: {value}"
        );
        assert!(
            !value.contains(&file.display().to_string()),
            "field '{key}' must not leak full path, got: {value}"
        );
    }

    // No field value should contain file content.
    for (key, value) in &evt.fields {
        assert!(
            !value.contains("supersecret"),
            "field '{key}' must not leak file content, got: {value}"
        );
    }
}

// ── Unicode dispatch-level tests ─────────────────────────────────────────

/// Unicode dispatch-level test: multi-byte characters in unchanged spans
/// are preserved byte-for-byte after a successful edit. The match uses an
/// exact ASCII word that sits between multi-byte Unicode content.
#[tokio::test]
async fn edit_unicode_success_preserves_multibyte_unchanged_spans() {
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
#[tokio::test]
async fn edit_crlf_success_preserves_crlf_in_output() {
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
#[tokio::test]
async fn edit_crlf_guard_rejected_does_not_modify_file() {
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
#[tokio::test]
async fn edit_escape_guard_rejected_does_not_modify_file() {
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
#[tokio::test]
async fn edit_ambiguous_trimmed_boundary_does_not_modify_file() {
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
#[tokio::test]
async fn edit_no_match_nearest_miss_score_is_reasonable() {
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
/// (plumbed consistently with `call_edit`) and still succeeds with a worker role.
#[tokio::test]
async fn apply_patch_accepts_worker_role_plumbing() {
    let worktree = crate::test_helpers::test_tempdir("djinn-ext-patch-worker-");
    let file = worktree.path().join("svc.rs");
    tokio::fs::write(&file, "fn main() {\n    old();\n}\n")
        .await
        .expect("seed file");

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

    let patch = "*** Begin Patch\n*** Update File: svc.rs\n@@ fn main() @@\n fn main() {\n-    old();\n+    new();\n }\n*** End Patch";
    let patch_args = Some(
        serde_json::json!({ "patch": patch })
            .as_object()
            .expect("obj")
            .clone(),
    );

    // Invoke with worker role — must succeed (no GateGuard enforcement yet).
    let response = call_apply_patch(
        &state,
        &patch_args,
        worktree.path(),
        None,
        Some("task-abc-123"),
        Some("worker"),
    )
    .await
    .expect("worker role must succeed");

    assert_eq!(response.get("ok").and_then(|v| v.as_bool()), Some(true));
    let files = response
        .get("files")
        .and_then(|v| v.as_array())
        .expect("files array");
    assert_eq!(files.len(), 1);
    assert_eq!(
        files[0].get("action").and_then(|v| v.as_str()),
        Some("updated"),
    );
    let after = tokio::fs::read_to_string(&file).await.expect("read back");
    assert!(after.contains("new()"), "patched content missing: {after}");
}
