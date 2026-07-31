//! Tests for the authorship-time size nudge.
//!
//! The contract has three halves and every test here holds all of them at
//! once, because two of the three are satisfiable by doing nothing:
//!
//!   1. the nudge appears when the file no longer fits in one `read`;
//!   2. it does NOT appear when the file does fit;
//!   3. **the edit succeeds either way.**
//!
//! (3) is the one that matters most. A "nudge" that could deny an edit would
//! be the retired gate wearing a new name, and worse, it would land in the
//! deny path that #2821/#2839 just unwedged — workers were escaping that
//! deadlock onto the ungated `shell` tool, and anything reading as a denial
//! sends them straight back.

use super::*;
use crate::extension::handlers::workspace::{call_apply_patch, call_edit, call_write};
use crate::test_helpers::{agent_context_from_db, create_test_db, test_tempdir};
use tokio_util::sync::CancellationToken;

fn setup(prefix: &str) -> (tempfile::TempDir, crate::context::AgentContext) {
    let dir = test_tempdir(prefix);
    let db = create_test_db();
    let state = agent_context_from_db(db, CancellationToken::new());
    (dir, state)
}

/// A body large enough that no single `read` can return it: comfortably past
/// both the 2000-line ceiling and the listing character budget.
fn oversized_body(marker: &str) -> String {
    let mut body = format!("// {marker}\n");
    for i in 0..3_000 {
        body.push_str(&format!(
            "fn generated_{i:05}() {{ /* padding padding */ }}\n"
        ));
    }
    body
}

fn args(value: serde_json::Value) -> Option<serde_json::Map<String, serde_json::Value>> {
    Some(value.as_object().expect("object args").clone())
}

// ── the arithmetic, against the production clamps ────────────────────────

#[test]
fn a_file_inside_both_clamps_costs_exactly_one_read() {
    // 500 lines of ~40 bytes is well inside 2000 lines and inside the
    // listing budget once per-line overhead is charged.
    assert_eq!(reads_required(20_000, 500), 1);
    // Empty and single-line files are never interesting.
    assert_eq!(reads_required(0, 0), 1);
    assert_eq!(reads_required(12, 1), 1);
}

#[test]
fn each_clamp_forces_a_second_read_on_its_own() {
    // Line clamp alone: 2001 tiny lines are only ~4 kB, far inside the
    // character budget, but `read` still refuses to return them in one call.
    assert_eq!(reads_required(4_002, 2_001), 2);

    // Character clamp alone: one enormous line is a single line, so the line
    // clamp is satisfied, yet the listing cannot be rendered in one result.
    let budget = read_content_budget();
    assert!(reads_required(budget * 2, 1) >= 2);
}

#[test]
fn the_estimate_charges_json_escaping_for_the_listing_gutter() {
    // The clamp applies to the SERIALIZED result, so a tab and a newline cost
    // two characters each. Under-charging here would let a file that really
    // needs two reads report one. Pin the constant against that.
    assert_eq!(LISTING_LINE_OVERHEAD, 10);
    let budget = read_content_budget();
    // A file whose raw bytes fit the budget but whose gutter does not.
    let lines = 1_500;
    let bytes = budget - 1;
    assert_eq!(reads_required(bytes, lines), 2);
}

#[test]
fn the_cheap_bound_never_hides_a_file_that_needs_two_reads() {
    // The bound exists so small edits skip the file read entirely. It is only
    // sound if EVERY line distribution at that size still costs one read —
    // including the pathological all-newlines file.
    let bound = single_read_certain_bytes();
    assert!(bound > 0);
    for lines in [0usize, 1, bound / 2, bound] {
        assert_eq!(
            reads_required(bound, lines),
            1,
            "cheap bound {bound} claimed a single read for {lines} lines, but the estimate disagrees",
        );
    }
}

#[test]
fn generated_and_lock_paths_are_exempt() {
    assert!(is_exempt(Path::new("/w/server/src/generated/api.rs")));
    assert!(is_exempt(Path::new("/w/ui/src/mcp-tools.gen.ts")));
    assert!(is_exempt(Path::new("/w/Cargo.lock")));
    assert!(!is_exempt(Path::new("/w/server/src/lib.rs")));
    assert!(!is_exempt(Path::new("/w/server/src/generated_report.rs")));
}

#[test]
fn the_message_is_advisory_in_its_first_and_last_sentence() {
    let text = compose("server/src/big.rs", 200_000, 5_000, 7);
    // Specific, not generic: the actionable number is the read cost.
    assert!(text.contains("5000 lines / 200000 bytes"), "{text}");
    assert!(text.contains("7 `read` calls"), "{text}");
    assert!(text.contains("2000 lines"), "{text}");

    // It must not read as a denial. These are the words the gate_guard deny
    // path uses, and the ones a worker learned to route around via `shell`.
    assert!(text.starts_with("The edit succeeded"), "{text}");
    for forbidden in ["FORCE-", "denied", "forbidden", "before you", "retry"] {
        assert!(
            !text.contains(forbidden),
            "advisory must not read as a denial, found {forbidden:?} in: {text}",
        );
    }
}

// ── end to end, through the real tool handlers ───────────────────────────

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn write_of_an_oversized_file_succeeds_and_carries_the_nudge() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup("nudge-write-big-");

    let result = call_write(
        &state,
        &args(serde_json::json!({
            "path": "big.rs",
            "content": oversized_body("oversized write fixture"),
        })),
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("an advisory must never fail a write");

    assert_eq!(result["ok"], serde_json::json!(true));
    // The write really happened — the nudge did not stand in for it.
    let on_disk = tokio::fs::read_to_string(worktree.path().join("big.rs"))
        .await
        .expect("file written");
    assert!(on_disk.contains("generated_02999"), "full body written");

    let nudge = result["size_nudge"].as_str().expect("nudge present");
    assert!(nudge.contains("big.rs"), "{nudge}");
    assert!(nudge.contains("`read` calls"), "{nudge}");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn write_of_a_small_file_succeeds_with_no_nudge() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup("nudge-write-small-");

    let result = call_write(
        &state,
        &args(serde_json::json!({
            "path": "small.rs",
            "content": "fn small() {}\n",
        })),
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("small writes are unaffected");

    assert_eq!(result["ok"], serde_json::json!(true));
    assert!(
        result.get("size_nudge").is_none(),
        "a file that fits in one read must produce no advisory: {result}",
    );
    let on_disk = tokio::fs::read_to_string(worktree.path().join("small.rs"))
        .await
        .expect("file written");
    assert_eq!(on_disk, "fn small() {}\n");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn a_mid_size_file_past_the_cheap_bound_still_gets_no_nudge() {
    // The small-file tests above exit through `single_read_certain_bytes`
    // without ever consulting the estimate, so on their own they prove only
    // that the short-circuit works. This one is deliberately sized past that
    // bound — ~10 kB over 300 lines — so the "no nudge" verdict has to come
    // out of `reads_required` itself. Without it, an estimate that claimed
    // every file needs two reads would still pass the whole suite.
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup("nudge-midsize-");

    let mut body = String::new();
    for i in 0..300 {
        body.push_str(&format!(
            "fn mid_{i:03}() {{ /* thirty-odd bytes of padding here */ }}\n"
        ));
    }
    assert!(
        body.len() > single_read_certain_bytes(),
        "fixture must clear the cheap bound to exercise the estimate",
    );

    let result = call_write(
        &state,
        &args(serde_json::json!({ "path": "mid.rs", "content": body })),
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("mid-size writes are unaffected");

    assert_eq!(result["ok"], serde_json::json!(true));
    assert!(
        result.get("size_nudge").is_none(),
        "a 300-line file still fits in one read and must not be advised about: {result}",
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn edit_of_an_oversized_file_succeeds_and_carries_the_nudge() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup("nudge-edit-big-");
    let file = worktree.path().join("big.rs");
    tokio::fs::write(&file, oversized_body("oversized edit fixture"))
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();
    // Non-worker role: this test is about the advisory, not about GateGuard's
    // read-coverage ladder, which has its own suite.
    state
        .file_time
        .read_with_coverage(
            &session_id,
            &file,
            crate::file_time::ReadCoverage::Full,
            false,
        )
        .await
        .expect("record read");

    let result = call_edit(
        &state,
        &args(serde_json::json!({
            "path": "big.rs",
            "old_text": "fn generated_00007",
            "new_text": "fn renamed_00007",
        })),
        worktree.path(),
        None,
        Some("task-1"),
        Some("planner"),
    )
    .await
    .expect("an advisory must never fail an edit");

    assert_eq!(result["ok"], serde_json::json!(true));
    let on_disk = tokio::fs::read_to_string(&file).await.expect("read back");
    assert!(on_disk.contains("fn renamed_00007"), "edit applied");

    let nudge = result["size_nudge"].as_str().expect("nudge present");
    assert!(nudge.contains("big.rs"), "{nudge}");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn edit_of_a_small_file_succeeds_with_no_nudge() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup("nudge-edit-small-");
    let file = worktree.path().join("small.rs");
    tokio::fs::write(&file, "fn small() {}\n")
        .await
        .expect("seed");

    let session_id = worktree.path().display().to_string();
    state
        .file_time
        .read_with_coverage(
            &session_id,
            &file,
            crate::file_time::ReadCoverage::Full,
            false,
        )
        .await
        .expect("record read");

    let result = call_edit(
        &state,
        &args(serde_json::json!({
            "path": "small.rs",
            "old_text": "small",
            "new_text": "tiny",
        })),
        worktree.path(),
        None,
        Some("task-1"),
        Some("planner"),
    )
    .await
    .expect("small edits are unaffected");

    assert_eq!(result["ok"], serde_json::json!(true));
    assert!(
        result.get("size_nudge").is_none(),
        "a file that fits in one read must produce no advisory: {result}",
    );
    let on_disk = tokio::fs::read_to_string(&file).await.expect("read back");
    assert_eq!(on_disk, "fn tiny() {}\n");
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn apply_patch_of_an_oversized_file_succeeds_and_carries_one_nudge() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup("nudge-patch-big-");

    // Two Adds in one patch, one oversized and one tiny. The result must
    // carry exactly one advisory, about the file that needs it.
    let patch = format!(
        "*** Begin Patch\n*** Add File: big.rs\n{}*** Add File: tiny.rs\n+fn tiny() {{}}\n*** End Patch\n",
        oversized_body("oversized patch fixture")
            .lines()
            .map(|l| format!("+{l}\n"))
            .collect::<String>(),
    );

    let result = call_apply_patch(
        &state,
        &args(serde_json::json!({ "patch": patch })),
        worktree.path(),
        None,
        Some("task-1"),
        Some("worker"),
    )
    .await
    .expect("an advisory must never fail a patch");

    assert_eq!(result["ok"], serde_json::json!(true));
    assert!(
        worktree.path().join("big.rs").exists() && worktree.path().join("tiny.rs").exists(),
        "both files applied",
    );

    let nudge = result["size_nudge"].as_str().expect("nudge present");
    assert!(nudge.contains("big.rs"), "{nudge}");
    assert!(
        !nudge.contains("tiny.rs"),
        "the small file must not be advised about: {nudge}",
    );
}

#[allow(clippy::await_holding_lock)]
#[tokio::test]
async fn the_nudge_fires_once_per_file_per_session() {
    let _jit_env = crate::test_helpers::jit_env_read_guard();
    let (worktree, state) = setup("nudge-once-");
    let file = worktree.path().join("big.rs");
    let session_id = worktree.path().display().to_string();

    let mut seen = Vec::new();
    for pass in 0..3 {
        tokio::fs::write(&file, oversized_body(&format!("pass {pass}")))
            .await
            .expect("seed");
        state
            .file_time
            .read_with_coverage(
                &session_id,
                &file,
                crate::file_time::ReadCoverage::Full,
                false,
            )
            .await
            .expect("record read");

        let result = call_edit(
            &state,
            &args(serde_json::json!({
                "path": "big.rs",
                "old_text": format!("// pass {pass}"),
                "new_text": format!("// touched {pass}"),
            })),
            worktree.path(),
            None,
            Some("task-1"),
            Some("planner"),
        )
        .await
        .expect("every edit succeeds");

        assert_eq!(result["ok"], serde_json::json!(true));
        seen.push(result.get("size_nudge").is_some());
    }

    assert_eq!(
        seen,
        vec![true, false, false],
        "a worker editing one big file repeatedly gets one advisory, not one per edit",
    );
}
