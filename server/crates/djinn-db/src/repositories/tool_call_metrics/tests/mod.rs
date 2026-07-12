use crate::repositories::tool_call_export::NormalizedToolCallRow;
use crate::repositories::tool_call_metrics::{
    adoption_counts, apply_patch_adoption_share, compute_metrics, failure_rates,
    read_truncation_loop_rate, retry_after_edit_failure,
};

// ─── Helpers ───────────────────────────────────────────────────────────

fn row(
    session_id: &str,
    task_id: Option<&str>,
    turn_index: usize,
    tool_name: &str,
    result_status: &str,
    error_class: Option<&str>,
    path: Option<&str>,
    read_truncated: bool,
    read_offset: Option<i64>,
    read_limit: Option<i64>,
    args_hash: &str,
) -> NormalizedToolCallRow {
    NormalizedToolCallRow {
        provider_id: Some("openai".into()),
        model_id: Some("gpt-5-codex".into()),
        format_family: Some("OpenAIResponses".into()),
        tool_surface_family: Some("codex".into()),
        agent_role: Some("worker".into()),
        session_id: session_id.into(),
        task_id: task_id.map(str::to_owned),
        calendar_day: Some("2026-02-03".into()),
        window_start: Some("2026-02-03T00:00:00Z".into()),
        tool_call_id: Some(format!("call-{session_id}-{turn_index}")),
        turn_index,
        tool_name: tool_name.into(),
        args_hash: args_hash.into(),
        result_status: result_status.into(),
        error_class: error_class.map(str::to_owned),
        error_text: None,
        read_truncated,
        path: path.map(str::to_owned),
        read_offset,
        read_limit,
        diagnostics: vec![],
    }
}

// ─── Failure rate tests ───────────────────────────────────────────────

#[test]
fn edit_failure_rate_counts_all_declared_error_classes() {
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("validation"),
            None,
            false,
            None,
            None,
            "h1",
        ),
        row(
            "s1",
            Some("t1"),
            1,
            "edit",
            "error",
            Some("patch-context-miss"),
            None,
            false,
            None,
            None,
            "h2",
        ),
        row(
            "s1",
            Some("t1"),
            2,
            "edit",
            "error",
            Some("file-not-read"),
            None,
            false,
            None,
            None,
            "h3",
        ),
        row(
            "s1",
            Some("t1"),
            3,
            "edit",
            "error",
            Some("ambiguous-match"),
            None,
            false,
            None,
            None,
            "h4",
        ),
        row(
            "s1",
            Some("t1"),
            4,
            "edit",
            "error",
            Some("stale-file"),
            None,
            false,
            None,
            None,
            "h5",
        ),
        row(
            "s1",
            Some("t1"),
            5,
            "edit",
            "error",
            Some("io"),
            None,
            false,
            None,
            None,
            "h6",
        ),
        row(
            "s1",
            Some("t1"),
            6,
            "edit",
            "error",
            Some("timeout"),
            None,
            false,
            None,
            None,
            "h7",
        ),
        row(
            "s1",
            Some("t1"),
            7,
            "edit",
            "success",
            None,
            None,
            false,
            None,
            None,
            "h8",
        ),
    ];
    let rates = failure_rates(&rows);
    assert_eq!(rates.edit_failure_rate.numerator, 7);
    assert_eq!(rates.edit_failure_rate.denominator, 8);
}

#[test]
fn edit_failure_rate_excludes_task_stop_cancellation() {
    let rows = vec![
        // Cancelled edit — excluded.
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("cancelled"),
            Some("a.rs"),
            false,
            None,
            None,
            "h1",
        ),
        // Validation failure — counted.
        row(
            "s1",
            Some("t1"),
            1,
            "edit",
            "error",
            Some("validation"),
            None,
            false,
            None,
            None,
            "h2",
        ),
        // Success — not counted.
        row(
            "s1",
            Some("t1"),
            2,
            "edit",
            "success",
            None,
            None,
            false,
            None,
            None,
            "h3",
        ),
    ];
    let rates = failure_rates(&rows);
    assert_eq!(rates.edit_failure_rate.numerator, 1);
    assert_eq!(rates.edit_failure_rate.denominator, 3);
}

#[test]
fn apply_patch_failure_rate_counts_declared_classes() {
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "apply_patch",
            "error",
            Some("validation"),
            None,
            false,
            None,
            None,
            "h1",
        ),
        row(
            "s1",
            Some("t1"),
            1,
            "apply_patch",
            "error",
            Some("patch-context-miss"),
            None,
            false,
            None,
            None,
            "h2",
        ),
        row(
            "s1",
            Some("t1"),
            2,
            "apply_patch",
            "error",
            Some("file-not-read"),
            None,
            false,
            None,
            None,
            "h3",
        ),
        row(
            "s1",
            Some("t1"),
            3,
            "apply_patch",
            "error",
            Some("io"),
            None,
            false,
            None,
            None,
            "h4",
        ),
        row(
            "s1",
            Some("t1"),
            4,
            "apply_patch",
            "error",
            Some("timeout"),
            None,
            false,
            None,
            None,
            "h5",
        ),
        row(
            "s1",
            Some("t1"),
            5,
            "apply_patch",
            "success",
            None,
            None,
            false,
            None,
            None,
            "h6",
        ),
    ];
    let rates = failure_rates(&rows);
    assert_eq!(rates.apply_patch_failure_rate.numerator, 5);
    assert_eq!(rates.apply_patch_failure_rate.denominator, 6);
}

#[test]
fn apply_patch_failure_rate_excludes_cancellation() {
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "apply_patch",
            "error",
            Some("cancelled"),
            None,
            false,
            None,
            None,
            "h1",
        ),
        row(
            "s1",
            Some("t1"),
            1,
            "apply_patch",
            "success",
            None,
            None,
            false,
            None,
            None,
            "h2",
        ),
    ];
    let rates = failure_rates(&rows);
    assert_eq!(rates.apply_patch_failure_rate.numerator, 0);
    assert_eq!(rates.apply_patch_failure_rate.denominator, 2);
}

#[test]
fn result_status_not_success_counts_as_failure_even_without_declared_class() {
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("tool"),
            None,
            false,
            None,
            None,
            "h1",
        ),
        row(
            "s1",
            Some("t1"),
            1,
            "edit",
            "missing",
            None,
            None,
            false,
            None,
            None,
            "h2",
        ),
    ];
    let rates = failure_rates(&rows);
    assert_eq!(rates.edit_failure_rate.numerator, 2);
    assert_eq!(rates.edit_failure_rate.denominator, 2);
}

// ─── Retry tests ──────────────────────────────────────────────────────

#[test]
fn retry_same_path_within_three_turns() {
    let rows = vec![
        // Failed edit at turn 0.
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("validation"),
            Some("a.rs"),
            false,
            None,
            None,
            "h1",
        ),
        // Retry (failed edit on same path) at turn 1.
        row(
            "s1",
            Some("t1"),
            1,
            "edit",
            "error",
            Some("validation"),
            Some("a.rs"),
            false,
            None,
            None,
            "h2",
        ),
    ];
    let rate = retry_after_edit_failure(&rows);
    // The first failed edit has a retry (turn 1); the second has none.
    // Both failed edits are in the denominator.
    assert_eq!(rate.numerator, 1);
    assert_eq!(rate.denominator, 2);
}

#[test]
fn retry_same_args_hash_within_three_turns() {
    let rows = vec![
        // Failed edit at turn 0.
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("validation"),
            None,
            false,
            None,
            None,
            "samehash",
        ),
        // Retry with same args_hash at turn 2 (no path).
        row(
            "s1",
            Some("t1"),
            2,
            "apply_patch",
            "success",
            None,
            None,
            false,
            None,
            None,
            "samehash",
        ),
    ];
    let rate = retry_after_edit_failure(&rows);
    assert_eq!(rate.numerator, 1);
    assert_eq!(rate.denominator, 1);
}

#[test]
fn retry_at_exactly_three_turns_boundary() {
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("validation"),
            Some("a.rs"),
            false,
            None,
            None,
            "h1",
        ),
        // Turn 3 — still within "next three assistant turns" (0+3).
        row(
            "s1",
            Some("t1"),
            3,
            "edit",
            "error",
            Some("io"),
            Some("a.rs"),
            false,
            None,
            None,
            "h2",
        ),
    ];
    let rate = retry_after_edit_failure(&rows);
    assert_eq!(rate.numerator, 1);
    assert_eq!(rate.denominator, 2);
}

#[test]
fn retry_beyond_three_turns_not_counted() {
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("validation"),
            Some("a.rs"),
            false,
            None,
            None,
            "h1",
        ),
        // Turn 4 — beyond the three-turn boundary.
        row(
            "s1",
            Some("t1"),
            4,
            "edit",
            "success",
            None,
            Some("a.rs"),
            false,
            None,
            None,
            "h2",
        ),
    ];
    let rate = retry_after_edit_failure(&rows);
    assert_eq!(rate.numerator, 0);
    assert_eq!(rate.denominator, 1);
}

#[test]
fn retry_cross_session_excluded() {
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("validation"),
            Some("a.rs"),
            false,
            None,
            None,
            "h1",
        ),
        // Same path but different session.
        row(
            "s2",
            Some("t1"),
            1,
            "edit",
            "success",
            None,
            Some("a.rs"),
            false,
            None,
            None,
            "h2",
        ),
    ];
    let rate = retry_after_edit_failure(&rows);
    assert_eq!(rate.numerator, 0);
    assert_eq!(rate.denominator, 1);
}

#[test]
fn retry_cross_task_excluded() {
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("validation"),
            Some("a.rs"),
            false,
            None,
            None,
            "h1",
        ),
        // Same session, different task.
        row(
            "s1",
            Some("t2"),
            1,
            "edit",
            "success",
            None,
            Some("a.rs"),
            false,
            None,
            None,
            "h2",
        ),
    ];
    let rate = retry_after_edit_failure(&rows);
    assert_eq!(rate.numerator, 0);
    assert_eq!(rate.denominator, 1);
}

#[test]
fn retry_blocked_by_intervening_successful_modification() {
    let rows = vec![
        // Failed edit on a.rs at turn 0.
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("validation"),
            Some("a.rs"),
            false,
            None,
            None,
            "h1",
        ),
        // Intervening successful apply_patch on a.rs at turn 1.
        row(
            "s1",
            Some("t1"),
            1,
            "apply_patch",
            "success",
            None,
            Some("a.rs"),
            false,
            None,
            None,
            "h2",
        ),
        // Would-be retry on a.rs at turn 2, but blocked.
        row(
            "s1",
            Some("t1"),
            2,
            "edit",
            "success",
            None,
            Some("a.rs"),
            false,
            None,
            None,
            "h3",
        ),
    ];
    let rate = retry_after_edit_failure(&rows);
    assert_eq!(rate.numerator, 0);
    assert_eq!(rate.denominator, 1);
}

#[test]
fn retry_apply_patch_counts_as_retry() {
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("validation"),
            Some("a.rs"),
            false,
            None,
            None,
            "h1",
        ),
        // Retry via apply_patch on same path (failed — not a successful
        // modification, so it counts as a retry).
        row(
            "s1",
            Some("t1"),
            1,
            "apply_patch",
            "error",
            Some("validation"),
            Some("a.rs"),
            false,
            None,
            None,
            "h2",
        ),
    ];
    let rate = retry_after_edit_failure(&rows);
    assert_eq!(rate.numerator, 1);
    assert_eq!(rate.denominator, 1);
}

// ─── Adoption tests ───────────────────────────────────────────────────

#[test]
fn adoption_all_attempts_reports_counts() {
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "success",
            None,
            Some("a.rs"),
            false,
            None,
            None,
            "h1",
        ),
        row(
            "s1",
            Some("t1"),
            1,
            "apply_patch",
            "success",
            None,
            Some("b.rs"),
            false,
            None,
            None,
            "h2",
        ),
        row(
            "s1",
            Some("t1"),
            2,
            "edit",
            "success",
            None,
            Some("c.rs"),
            false,
            None,
            None,
            "h3",
        ),
        row(
            "s1",
            Some("t1"),
            3,
            "apply_patch",
            "error",
            Some("validation"),
            Some("d.rs"),
            false,
            None,
            None,
            "h4",
        ),
    ];
    let counts = adoption_counts(&rows);
    assert_eq!(counts.all_apply_patch_success, 1);
    assert_eq!(counts.all_edit_success, 2);
    let share = apply_patch_adoption_share(&rows);
    assert_eq!(share.all_attempts.numerator, 1);
    assert_eq!(share.all_attempts.denominator, 3);
    assert!((share.all_attempts.rate - 1.0 / 3.0).abs() < 1e-9);
}

#[test]
fn adoption_post_failure_retries_reports_counts() {
    let rows = vec![
        // Failed edit on a.rs.
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("validation"),
            Some("a.rs"),
            false,
            None,
            None,
            "h1",
        ),
        // Successful apply_patch on a.rs (post-failure retry).
        row(
            "s1",
            Some("t1"),
            1,
            "apply_patch",
            "success",
            None,
            Some("a.rs"),
            false,
            None,
            None,
            "h2",
        ),
        // Standalone successful edit (not a retry).
        row(
            "s1",
            Some("t1"),
            2,
            "edit",
            "success",
            None,
            Some("b.rs"),
            false,
            None,
            None,
            "h3",
        ),
    ];
    let counts = adoption_counts(&rows);
    assert_eq!(counts.retry_apply_patch_success, 1);
    assert_eq!(counts.retry_edit_success, 0);
    let share = apply_patch_adoption_share(&rows);
    assert_eq!(share.post_failure_retries.numerator, 1);
    assert_eq!(share.post_failure_retries.denominator, 1);
}

// ─── Read loop tests ──────────────────────────────────────────────────

#[test]
fn read_loop_truncated_three_reads_same_file() {
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "read",
            "success",
            None,
            Some("a.rs"),
            true,
            None,
            None,
            "h1",
        ),
        row(
            "s1",
            Some("t1"),
            1,
            "read",
            "success",
            None,
            Some("a.rs"),
            true,
            None,
            None,
            "h2",
        ),
        row(
            "s1",
            Some("t1"),
            2,
            "read",
            "success",
            None,
            Some("a.rs"),
            true,
            None,
            None,
            "h3",
        ),
    ];
    let rate = read_truncation_loop_rate(&rows);
    assert_eq!(rate.numerator, 1);
    assert_eq!(rate.denominator, 1);
}

#[test]
fn read_loop_overlapping_windows() {
    // Three reads with overlapping windows (not truncated, but overlapping).
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "read",
            "success",
            None,
            Some("a.rs"),
            false,
            Some(0),
            Some(100),
            "h1",
        ),
        row(
            "s1",
            Some("t1"),
            1,
            "read",
            "success",
            None,
            Some("a.rs"),
            false,
            Some(50),
            Some(100),
            "h2",
        ),
        row(
            "s1",
            Some("t1"),
            2,
            "read",
            "success",
            None,
            Some("a.rs"),
            false,
            Some(0),
            Some(80),
            "h3",
        ),
    ];
    let rate = read_truncation_loop_rate(&rows);
    assert_eq!(rate.numerator, 1);
    assert_eq!(rate.denominator, 1);
}

#[test]
fn read_loop_advancing_non_overlapping_pagination_not_a_loop() {
    // Three reads on same file but advancing non-overlapping pagination.
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "read",
            "success",
            None,
            Some("a.rs"),
            false,
            Some(0),
            Some(100),
            "h1",
        ),
        row(
            "s1",
            Some("t1"),
            1,
            "read",
            "success",
            None,
            Some("a.rs"),
            false,
            Some(100),
            Some(100),
            "h2",
        ),
        row(
            "s1",
            Some("t1"),
            2,
            "read",
            "success",
            None,
            Some("a.rs"),
            false,
            Some(200),
            Some(100),
            "h3",
        ),
    ];
    let rate = read_truncation_loop_rate(&rows);
    assert_eq!(rate.numerator, 0);
    assert_eq!(rate.denominator, 1);
}

#[test]
fn read_loop_requires_three_reads() {
    // Only two reads — not a loop.
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "read",
            "success",
            None,
            Some("a.rs"),
            true,
            None,
            None,
            "h1",
        ),
        row(
            "s1",
            Some("t1"),
            1,
            "read",
            "success",
            None,
            Some("a.rs"),
            true,
            None,
            None,
            "h2",
        ),
    ];
    let rate = read_truncation_loop_rate(&rows);
    assert_eq!(rate.numerator, 0);
    assert_eq!(rate.denominator, 1);
}

#[test]
fn read_loop_within_six_turns() {
    // Three reads but spread beyond 6 turns — not a loop.
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "read",
            "success",
            None,
            Some("a.rs"),
            true,
            None,
            None,
            "h1",
        ),
        row(
            "s1",
            Some("t1"),
            4,
            "read",
            "success",
            None,
            Some("a.rs"),
            true,
            None,
            None,
            "h2",
        ),
        row(
            "s1",
            Some("t1"),
            8,
            "read",
            "success",
            None,
            Some("a.rs"),
            true,
            None,
            None,
            "h3",
        ),
    ];
    let rate = read_truncation_loop_rate(&rows);
    // Turn 8 is beyond turn 0 + 6 = 6, so the window doesn't contain all three.
    assert_eq!(rate.numerator, 0);
    assert_eq!(rate.denominator, 1);
}

#[test]
fn read_loop_session_scoped() {
    // Reads spread across two sessions — each has fewer than 3 on the same file.
    let rows = vec![
        row(
            "s1",
            Some("t1"),
            0,
            "read",
            "success",
            None,
            Some("a.rs"),
            true,
            None,
            None,
            "h1",
        ),
        row(
            "s1",
            Some("t1"),
            1,
            "read",
            "success",
            None,
            Some("a.rs"),
            true,
            None,
            None,
            "h2",
        ),
        row(
            "s2",
            Some("t2"),
            0,
            "read",
            "success",
            None,
            Some("a.rs"),
            true,
            None,
            None,
            "h3",
        ),
    ];
    let rate = read_truncation_loop_rate(&rows);
    assert_eq!(rate.numerator, 0);
    assert_eq!(rate.denominator, 2);
}

// ─── Aggregate report ─────────────────────────────────────────────────

#[test]
fn compute_metrics_aggregates_all() {
    let rows = vec![
        // Edit failures.
        row(
            "s1",
            Some("t1"),
            0,
            "edit",
            "error",
            Some("validation"),
            Some("a.rs"),
            false,
            None,
            None,
            "h1",
        ),
        row(
            "s1",
            Some("t1"),
            1,
            "edit",
            "success",
            None,
            Some("a.rs"),
            false,
            None,
            None,
            "h2",
        ),
        // Apply patch.
        row(
            "s1",
            Some("t1"),
            2,
            "apply_patch",
            "success",
            None,
            Some("b.rs"),
            false,
            None,
            None,
            "h3",
        ),
        // Read loop.
        row(
            "s1",
            Some("t1"),
            3,
            "read",
            "success",
            None,
            Some("c.rs"),
            true,
            None,
            None,
            "h4",
        ),
        row(
            "s1",
            Some("t1"),
            4,
            "read",
            "success",
            None,
            Some("c.rs"),
            true,
            None,
            None,
            "h5",
        ),
        row(
            "s1",
            Some("t1"),
            5,
            "read",
            "success",
            None,
            Some("c.rs"),
            true,
            None,
            None,
            "h6",
        ),
    ];
    let metrics = compute_metrics(&rows);
    assert_eq!(metrics.failure_rates.edit_failure_rate.numerator, 1);
    assert_eq!(metrics.failure_rates.edit_failure_rate.denominator, 2);
    assert_eq!(metrics.failure_rates.apply_patch_failure_rate.numerator, 0);
    assert_eq!(
        metrics.failure_rates.apply_patch_failure_rate.denominator,
        1
    );
    assert_eq!(metrics.read_truncation_loop_rate.numerator, 1);
    assert_eq!(metrics.read_truncation_loop_rate.denominator, 1);
}

// ─── Wilson pseudo-count integration test ───────────────────────────

#[test]
fn edit_minus_apply_patch_interval_with_pseudo_count() {
    // Larger sample: 8/10 edit failures vs 1/10 apply_patch failures.
    let rows = (0..8)
        .map(|i| {
            row(
                "s1",
                Some("t1"),
                i,
                "edit",
                "error",
                Some("validation"),
                None,
                false,
                None,
                None,
                &format!("h{i}"),
            )
        })
        .chain((8..10).map(|i| {
            row(
                "s1",
                Some("t1"),
                i,
                "edit",
                "success",
                None,
                None,
                false,
                None,
                None,
                &format!("h{i}"),
            )
        }))
        .chain(std::iter::once(row(
            "s1",
            Some("t1"),
            10,
            "apply_patch",
            "error",
            Some("validation"),
            None,
            false,
            None,
            None,
            "h11",
        )))
        .chain((11..20).map(|i| {
            row(
                "s1",
                Some("t1"),
                i,
                "apply_patch",
                "success",
                None,
                None,
                false,
                None,
                None,
                &format!("h{i}"),
            )
        }))
        .collect::<Vec<_>>();
    let (ci, rates) =
        crate::repositories::tool_call_metrics::edit_minus_apply_patch_failure_interval(&rows);
    assert_eq!(rates.edit_failure_rate.numerator, 8);
    assert_eq!(rates.edit_failure_rate.denominator, 10);
    assert_eq!(rates.apply_patch_failure_rate.numerator, 1);
    assert_eq!(rates.apply_patch_failure_rate.denominator, 10);
    // With pseudo-count: edit = 9/11 ≈ 0.818, apply_patch = 2/11 ≈ 0.182.
    // Difference ≈ 0.636 — should exclude zero.
    assert!(
        ci.excludes_zero,
        "interval [{}, {}] should exclude zero",
        ci.lower, ci.upper
    );
    assert!(ci.lower > 0.0);
}
