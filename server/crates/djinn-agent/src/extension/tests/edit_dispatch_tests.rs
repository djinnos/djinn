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
    call_edit(&state, &edit1, worktree.path(), None)
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
    let err = call_edit(&state, &edit2, worktree.path(), None)
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
    call_edit(&state, &edit2, worktree.path(), None)
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
    let response = call_edit(&state, &edit_args, worktree.path(), None)
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
    let response = call_edit(&state, &edit_args, worktree.path(), None)
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
    let err = call_edit(&state, &edit_args, worktree.path(), None)
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
    let err = call_edit(&state, &edit_args, worktree.path(), None)
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
    let resp = call_edit(&state, &edit1, worktree.path(), None)
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
    let err = call_edit(&state, &edit2, worktree.path(), None)
        .await
        .expect_err("second edit without re-read must be rejected");
    assert!(
        err.contains("modified since last read") || err.contains("read"),
        "expected re-read guard, got: {err}"
    );
}
