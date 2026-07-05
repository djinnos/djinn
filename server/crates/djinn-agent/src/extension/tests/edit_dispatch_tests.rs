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
    call_apply_patch(&state, &patch1_args, worktree.path(), None)
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
    let err = call_apply_patch(&state, &patch2_args, worktree.path(), None)
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
