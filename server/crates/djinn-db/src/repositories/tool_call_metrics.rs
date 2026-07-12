//! Exact metric derivation over [`NormalizedToolCallRow`] from
//! [`crate::repositories::tool_call_export`].
//!
//! Implements proposal a5ht's definitions for:
//! - `edit_failure_rate` / `apply_patch_failure_rate`
//! - `retry_after_edit_failure_rate`
//! - `apply_patch_adoption_share`
//! - `read_truncation_loop_rate`
//!
//! Plus the prescribed one-pseudo-count ratio handling and a deterministic
//! 95% Wilson interval for the difference used by the decision gate.
//!
//! All metric results are serializable so callers can audit numerator,
//! denominator, rates, and intermediate counts.

use crate::repositories::tool_call_export::NormalizedToolCallRow;
use serde::{Deserialize, Serialize};

// ─── Failure-class definitions (proposal a5ht) ─────────────────────────────

/// Error classes that count as `edit` failures per proposal a5ht.
const EDIT_FAILURE_CLASSES: &[&str] = &[
    "validation",
    "patch-context-miss",
    "file-not-read",
    "ambiguous-match",
    "stale-file",
    "io",
    "timeout",
];

/// Error classes that count as `apply_patch` failures per proposal a5ht.
const APPLY_PATCH_FAILURE_CLASSES: &[&str] = &[
    "validation",
    "patch-context-miss",
    "file-not-read",
    "io",
    "timeout",
];

/// Returns true if the row is a declared failure for the given tool.
///
/// A row counts as a failure when `result_status != "success"` **or** its
/// `error_class` is one of the proposal-declared classes. Task-stop
/// cancellation (`error_class == "cancelled"`) is explicitly **excluded**.
fn is_declared_failure(row: &NormalizedToolCallRow, classes: &[&str]) -> bool {
    // Task-stop cancellation is excluded — never counts as a failure.
    if row.error_class.as_deref() == Some("cancelled") {
        return false;
    }
    row.result_status != "success"
        || row
            .error_class
            .as_deref()
            .is_some_and(|c| classes.contains(&c))
}

// ─── Simple rate metric ────────────────────────────────────────────────────

/// A rate metric with auditable numerator/denominator and the resulting rate.
///
/// Rates are stored as `f64` in `[0, 1]`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RateMetric {
    pub numerator: usize,
    pub denominator: usize,
    pub rate: f64,
}

impl RateMetric {
    /// Compute a plain rate: `numerator / denominator` (0.0 when denominator is 0).
    pub fn new(numerator: usize, denominator: usize) -> Self {
        let rate = if denominator == 0 {
            0.0
        } else {
            numerator as f64 / denominator as f64
        };
        Self {
            numerator,
            denominator,
            rate,
        }
    }

    /// Compute a rate with one pseudo-count added to both numerator and
    /// denominator, per proposal a5ht's zero-rate handling.
    pub fn with_pseudo_count(numerator: usize, denominator: usize) -> Self {
        Self::new(numerator + 1, denominator + 1)
    }
}

/// Compute `edit_failure_rate` and `apply_patch_failure_rate` from normalized
/// rows. Task-stop cancellation is excluded; all declared error classes are
/// included.
pub fn failure_rates(rows: &[NormalizedToolCallRow]) -> FailureRates {
    let mut edit_num = 0usize;
    let mut edit_den = 0usize;
    let mut apply_patch_num = 0usize;
    let mut apply_patch_den = 0usize;

    for row in rows {
        match row.tool_name.as_str() {
            "edit" => {
                edit_den += 1;
                if is_declared_failure(row, EDIT_FAILURE_CLASSES) {
                    edit_num += 1;
                }
            }
            "apply_patch" => {
                apply_patch_den += 1;
                if is_declared_failure(row, APPLY_PATCH_FAILURE_CLASSES) {
                    apply_patch_num += 1;
                }
            }
            _ => {}
        }
    }

    FailureRates {
        edit_failure_rate: RateMetric::new(edit_num, edit_den),
        apply_patch_failure_rate: RateMetric::new(apply_patch_num, apply_patch_den),
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct FailureRates {
    pub edit_failure_rate: RateMetric,
    pub apply_patch_failure_rate: RateMetric,
}

// ─── Retry-after-edit-failure ──────────────────────────────────────────────

/// Detect retries after failed `edit` calls.
///
/// Per proposal a5ht: a retry is a failed `edit` call followed within the next
/// three assistant turns by another `edit` or `apply_patch` call touching the
/// same file path **or** the same `args_hash` family. A retry is **blocked** by
/// an intervening successful modification on that path.
///
/// Rows must be sorted by `turn_index` within each session/task. The function
/// groups by `(session_id, task_id)` and processes each group independently.
pub fn retry_after_edit_failure(rows: &[NormalizedToolCallRow]) -> RateMetric {
    // Group rows by (session_id, task_id) preserving original order.
    let mut groups: Vec<(String, Option<String>, Vec<usize>)> = Vec::new();
    'outer: for (i, row) in rows.iter().enumerate() {
        for g in &mut groups {
            if g.0 == row.session_id && g.1 == row.task_id {
                g.2.push(i);
                continue 'outer;
            }
        }
        groups.push((row.session_id.clone(), row.task_id.clone(), vec![i]));
    }

    let mut numerator = 0usize;
    let mut denominator = 0usize;

    for (_, _, indices) in &groups {
        for &failed_idx in indices {
            let failed = &rows[failed_idx];
            if failed.tool_name != "edit" || !is_declared_failure(failed, EDIT_FAILURE_CLASSES) {
                continue;
            }
            denominator += 1;

            let failed_path = failed.path.as_deref();
            let failed_args_hash = failed.args_hash.as_str();
            let failed_turn = failed.turn_index;

            // Scan the next three assistant turns for a retry candidate.
            // A retry is blocked by an intervening successful modification on
            // the same path that occurs BEFORE the retry candidate.
            let mut retry_idx: Option<usize> = None;
            let mut has_intervening_modification = false;
            for &candidate_idx in indices {
                let candidate = &rows[candidate_idx];
                if candidate.turn_index <= failed_turn {
                    continue;
                }
                // Within the next three assistant turns.
                if candidate.turn_index > failed_turn + 3 {
                    break;
                }

                // If we already found a retry candidate, stop scanning.
                if retry_idx.is_some() {
                    break;
                }

                // Only count edit/apply_patch as retry candidates.
                let is_modifying =
                    candidate.tool_name == "edit" || candidate.tool_name == "apply_patch";
                if !is_modifying {
                    continue;
                }

                // Check if this candidate is a retry: same path or same
                // args_hash family. A successful modification on the same
                // path is NOT a retry — it's an intervening modification
                // that blocks retries (the file was already fixed).
                let same_path = failed_path.is_some() && candidate.path.as_deref() == failed_path;
                let same_args = candidate.args_hash == failed_args_hash;

                let is_blocking_modification = candidate.result_status == "success" && same_path;

                if is_blocking_modification {
                    has_intervening_modification = true;
                    continue;
                }

                if same_path || same_args {
                    retry_idx = Some(candidate_idx);
                    continue;
                }
            }

            // A retry counts only if no successful modification on the same
            // path occurred before the retry candidate.
            let found_retry = retry_idx.is_some() && !has_intervening_modification;

            if found_retry {
                numerator += 1;
            }
        }
    }

    RateMetric::new(numerator, denominator)
}

// ─── Apply_patch adoption share ────────────────────────────────────────────

/// `apply_patch_adoption_share`: the share of successful modifying calls made
/// with `apply_patch` out of all successful modifying calls (`edit` +
/// `apply_patch`).
///
/// Per proposal a5ht: report separately for all modifying attempts and for
/// post-failure retries.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AdoptionShare {
    /// Adoption among all successful modifying calls.
    pub all_attempts: RateMetric,
    /// Adoption among successful modifying calls that follow a failed edit
    /// (post-failure retries).
    pub post_failure_retries: RateMetric,
}

/// Intermediate auditable counts for adoption.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AdoptionCounts {
    pub all_apply_patch_success: usize,
    pub all_edit_success: usize,
    pub retry_apply_patch_success: usize,
    pub retry_edit_success: usize,
}

pub fn apply_patch_adoption_share(rows: &[NormalizedToolCallRow]) -> AdoptionShare {
    let counts = adoption_counts(rows);
    AdoptionShare {
        all_attempts: RateMetric::new(
            counts.all_apply_patch_success,
            counts.all_apply_patch_success + counts.all_edit_success,
        ),
        post_failure_retries: RateMetric::new(
            counts.retry_apply_patch_success,
            counts.retry_apply_patch_success + counts.retry_edit_success,
        ),
    }
}

/// Compute the raw auditable counts behind [`apply_patch_adoption_share`].
pub fn adoption_counts(rows: &[NormalizedToolCallRow]) -> AdoptionCounts {
    let mut all_apply_patch_success = 0usize;
    let mut all_edit_success = 0usize;

    // Group by (session_id, task_id) for post-failure retry detection.
    let mut groups: Vec<(String, Option<String>, Vec<usize>)> = Vec::new();
    'outer: for (i, row) in rows.iter().enumerate() {
        for g in &mut groups {
            if g.0 == row.session_id && g.1 == row.task_id {
                g.2.push(i);
                continue 'outer;
            }
        }
        groups.push((row.session_id.clone(), row.task_id.clone(), vec![i]));
    }

    // Identify which rows are post-failure retries.
    // A successful modifying call is a post-failure retry if it follows a
    // failed edit within the next three turns (same path or args_hash).
    let mut retry_apply_patch_success = 0usize;
    let mut retry_edit_success = 0usize;

    for (_, _, indices) in &groups {
        for &succ_idx in indices {
            let succ = &rows[succ_idx];
            if succ.result_status != "success" {
                continue;
            }
            let is_modifying = succ.tool_name == "edit" || succ.tool_name == "apply_patch";
            if !is_modifying {
                continue;
            }

            // Count all successes.
            match succ.tool_name.as_str() {
                "edit" => all_edit_success += 1,
                "apply_patch" => all_apply_patch_success += 1,
                _ => {}
            }

            // Check if this success follows a failed edit within three turns.
            let succ_path = succ.path.as_deref();
            let succ_turn = succ.turn_index;
            let mut is_post_failure = false;

            for &failed_idx in indices {
                let failed = &rows[failed_idx];
                if failed.turn_index >= succ_turn {
                    break;
                }
                if failed.tool_name != "edit" || !is_declared_failure(failed, EDIT_FAILURE_CLASSES)
                {
                    continue;
                }
                // Within the previous three assistant turns (i.e. the success
                // is within the next three turns of the failure).
                if failed.turn_index + 3 < succ_turn {
                    continue;
                }
                let same_path = succ_path.is_some() && failed.path.as_deref() == succ_path;
                let same_args = failed.args_hash == succ.args_hash;
                if same_path || same_args {
                    is_post_failure = true;
                    break;
                }
            }

            if is_post_failure {
                match succ.tool_name.as_str() {
                    "edit" => retry_edit_success += 1,
                    "apply_patch" => retry_apply_patch_success += 1,
                    _ => {}
                }
            }
        }
    }

    AdoptionCounts {
        all_apply_patch_success,
        all_edit_success,
        retry_apply_patch_success,
        retry_edit_success,
    }
}

// ─── Read truncation loop ──────────────────────────────────────────────────

/// Detect read truncation loops.
///
/// Per proposal a5ht: a read loop requires at least three `read` calls against
/// the same file where each returned `read_truncated = true` or had overlapping
/// read windows, within six assistant turns and before a modifying call.
///
/// Advancing non-overlapping pagination is NOT a loop. The loop is
/// file-scoped and session-scoped.
///
/// Returns a session-level rate: sessions with at least one loop / sessions
/// with at least one read call.
pub fn read_truncation_loop_rate(rows: &[NormalizedToolCallRow]) -> RateMetric {
    // Collect distinct session_ids.
    let session_ids: Vec<String> = {
        let mut seen = Vec::new();
        for row in rows {
            if !seen.contains(&row.session_id) {
                seen.push(row.session_id.clone());
            }
        }
        seen
    };

    let mut sessions_with_read = 0usize;
    let mut sessions_with_loop = 0usize;

    for sid in &session_ids {
        let session_rows: Vec<&NormalizedToolCallRow> =
            rows.iter().filter(|r| &r.session_id == sid).collect();

        let has_read = session_rows.iter().any(|r| r.tool_name == "read");
        if !has_read {
            continue;
        }
        sessions_with_read += 1;

        // Group read calls by path within the session.
        let mut path_groups: std::collections::BTreeMap<
            Option<&str>,
            Vec<&&NormalizedToolCallRow>,
        > = std::collections::BTreeMap::new();
        for r in &session_rows {
            if r.tool_name == "read" {
                path_groups.entry(r.path.as_deref()).or_default().push(r);
            }
        }

        let mut found_loop = false;
        for reads in path_groups.values() {
            if reads.len() < 3 {
                continue;
            }
            // Check for a window of 3+ reads within 6 assistant turns, each
            // truncated or overlapping, before a modifying call on that path.
            for start in 0..reads.len() {
                if start + 3 > reads.len() {
                    break;
                }
                let window = &reads[start..];
                // Find the maximal contiguous run starting at `start`.
                let first_turn = window[0].turn_index;
                let max_turn = first_turn + 6;

                // Collect reads within the 6-turn window.
                let mut in_window: Vec<&&NormalizedToolCallRow> = Vec::new();
                for r in window {
                    if r.turn_index <= max_turn {
                        in_window.push(r);
                    }
                }
                if in_window.len() < 3 {
                    continue;
                }

                // Check that no modifying call on this path occurs before or
                // between these reads (loop must be before modification).
                let path = in_window[0].path.as_deref();
                let has_modification_before = session_rows.iter().any(|r| {
                    (r.tool_name == "edit" || r.tool_name == "apply_patch")
                        && r.result_status == "success"
                        && r.path.as_deref() == path
                        && r.turn_index >= first_turn
                        && r.turn_index <= in_window.last().unwrap().turn_index
                });
                if has_modification_before {
                    continue;
                }

                // Check each read is truncated or its window overlaps with at
                // least one other read in the window.
                let all_qualify = in_window
                    .iter()
                    .all(|r| r.read_truncated || window_overlaps_any(r, &in_window));
                if all_qualify {
                    found_loop = true;
                    break;
                }
            }
            if found_loop {
                break;
            }
        }

        if found_loop {
            sessions_with_loop += 1;
        }
    }

    RateMetric::new(sessions_with_loop, sessions_with_read)
}

/// Check if a read's window overlaps with any other read in the same group.
/// If the read is not truncated, overlapping means its `[offset, offset+limit)`
/// window intersects another read's window. Reads without offset/limit are
/// treated as overlapping (conservative).
fn window_overlaps_any(read: &&NormalizedToolCallRow, group: &[&&NormalizedToolCallRow]) -> bool {
    let my_offset = read.read_offset.unwrap_or(0);
    let my_limit = read.read_limit.unwrap_or(0);
    let my_end = my_offset + my_limit;

    for other in group {
        if other.turn_index == read.turn_index {
            continue;
        }
        let other_offset = other.read_offset.unwrap_or(0);
        let other_limit = other.read_limit.unwrap_or(0);
        let other_end = other_offset + other_limit;

        // If either read lacks offset/limit info, treat as overlapping
        // (conservative).
        if read.read_offset.is_none()
            || read.read_limit.is_none()
            || other.read_offset.is_none()
            || other.read_limit.is_none()
        {
            return true;
        }

        // Overlapping if the windows intersect.
        if my_offset < other_end && other_offset < my_end {
            return true;
        }
    }
    false
}

// ─── Statistical primitives ────────────────────────────────────────────────

/// A 95% Wilson score interval for a single proportion.
///
/// Uses `z = 1.959964` (the two-sided 95% normal quantile).
pub const Z_95: f64 = 1.959963984540054;

/// Compute the Wilson score interval for a single proportion.
///
/// Returns `(lower, upper)` bounds. Uses the standard Wilson formula.
pub fn wilson_interval(successes: usize, total: usize) -> (f64, f64) {
    if total == 0 {
        return (0.0, 0.0);
    }
    let n = total as f64;
    let p = successes as f64 / n;
    let z2 = Z_95 * Z_95;
    let denom = 1.0 + z2 / n;
    let center = (p + z2 / (2.0 * n)) / denom;
    let margin = Z_95 * ((p * (1.0 - p) / n + z2 / (4.0 * n * n)).sqrt()) / denom;
    (center - margin, center + margin)
}

/// A confidence interval with auditable bounds.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ConfidenceInterval {
    pub lower: f64,
    pub upper: f64,
    /// True when the interval excludes zero (does not contain 0.0).
    pub excludes_zero: bool,
}

/// Compute the 95% confidence interval for the difference of two proportions
/// (`p1 - p2`) using the Wilson score method per proposal a5ht.
///
/// The difference interval is computed as:
/// `diff ± z * sqrt(p1*(1-p1)/n1 + p2*(1-p2)/n2)`
///
/// This is the Newcombe hybrid Wilson interval for the difference of two
/// proportions, which is the standard method recommended for the Wilson-based
/// difference interval.
pub fn wilson_difference_interval(
    successes1: usize,
    total1: usize,
    successes2: usize,
    total2: usize,
) -> ConfidenceInterval {
    if total1 == 0 || total2 == 0 {
        return ConfidenceInterval {
            lower: 0.0,
            upper: 0.0,
            excludes_zero: false,
        };
    }

    let p1 = successes1 as f64 / total1 as f64;
    let p2 = successes2 as f64 / total2 as f64;

    let (l1, u1) = wilson_interval(successes1, total1);
    let (l2, u2) = wilson_interval(successes2, total2);

    // Newcombe hybrid method: the difference interval bounds are computed from
    // the individual Wilson bounds.
    let diff = p1 - p2;
    let lower = diff - ((p1 - l1).powi(2) + (u2 - p2).powi(2)).sqrt();
    let upper = diff + ((u1 - p1).powi(2) + (p2 - l2).powi(2)).sqrt();

    let excludes_zero = lower > 0.0 || upper < 0.0;

    ConfidenceInterval {
        lower,
        upper,
        excludes_zero,
    }
}

/// Compute the 95% Wilson difference interval for `edit_failure_rate -
/// apply_patch_failure_rate`, applying one pseudo-count to each rate's
/// numerator and denominator as prescribed by proposal a5ht.
pub fn edit_minus_apply_patch_failure_interval(
    rows: &[NormalizedToolCallRow],
) -> (ConfidenceInterval, FailureRates) {
    let rates = failure_rates(rows);
    let interval = wilson_difference_interval(
        rates.edit_failure_rate.numerator + 1,
        rates.edit_failure_rate.denominator + 1,
        rates.apply_patch_failure_rate.numerator + 1,
        rates.apply_patch_failure_rate.denominator + 1,
    );
    (interval, rates)
}

// ─── Aggregate metric report ───────────────────────────────────────────────

/// The complete set of proposal a5ht Phase 1 metrics computed from normalized
/// rows. All fields are serializable for auditability.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolSurfaceMetrics {
    pub failure_rates: FailureRates,
    pub retry_after_edit_failure_rate: RateMetric,
    pub apply_patch_adoption: AdoptionShare,
    pub adoption_counts: AdoptionCounts,
    pub read_truncation_loop_rate: RateMetric,
    /// 95% Wilson difference interval for `edit_failure_rate -
    /// apply_patch_failure_rate` with pseudo-count applied.
    pub failure_difference_interval: ConfidenceInterval,
}

/// Compute all proposal a5ht Phase 1 metrics from normalized rows.
pub fn compute_metrics(rows: &[NormalizedToolCallRow]) -> ToolSurfaceMetrics {
    let rates = failure_rates(rows);
    let (failure_difference_interval, _) = edit_minus_apply_patch_failure_interval(rows);
    ToolSurfaceMetrics {
        failure_rates: rates,
        retry_after_edit_failure_rate: retry_after_edit_failure(rows),
        apply_patch_adoption: apply_patch_adoption_share(rows),
        adoption_counts: adoption_counts(rows),
        read_truncation_loop_rate: read_truncation_loop_rate(rows),
        failure_difference_interval,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    // ─── Pseudo-count and Wilson tests ────────────────────────────────────

    #[test]
    fn rate_metric_plain() {
        let m = RateMetric::new(3, 10);
        assert_eq!(m.numerator, 3);
        assert_eq!(m.denominator, 10);
        assert!((m.rate - 0.3).abs() < 1e-9);
    }

    #[test]
    fn rate_metric_zero_denominator() {
        let m = RateMetric::new(0, 0);
        assert_eq!(m.rate, 0.0);
    }

    #[test]
    fn pseudo_count_adds_one_to_both() {
        let m = RateMetric::with_pseudo_count(0, 0);
        assert_eq!(m.numerator, 1);
        assert_eq!(m.denominator, 1);
        assert!((m.rate - 1.0).abs() < 1e-9);

        let m2 = RateMetric::with_pseudo_count(5, 10);
        assert_eq!(m2.numerator, 6);
        assert_eq!(m2.denominator, 11);
    }

    #[test]
    fn wilson_interval_zero_successes() {
        let (lower, upper) = wilson_interval(0, 100);
        assert!(lower >= 0.0);
        assert!(upper > 0.0);
        assert!(upper < 0.1);
    }

    #[test]
    fn wilson_interval_all_successes() {
        let (lower, upper) = wilson_interval(100, 100);
        assert!(lower > 0.9);
        assert!(upper <= 1.0);
    }

    #[test]
    fn wilson_interval_zero_total() {
        let (lower, upper) = wilson_interval(0, 0);
        assert_eq!(lower, 0.0);
        assert_eq!(upper, 0.0);
    }

    #[test]
    fn wilson_difference_interval_nonzero_case_excludes_zero() {
        // p1 = 0.5, p2 = 0.1 — large difference.
        let ci = wilson_difference_interval(50, 100, 10, 100);
        assert!(ci.lower > 0.0, "lower bound {} should be > 0", ci.lower);
        assert!(ci.excludes_zero);
    }

    #[test]
    fn wilson_difference_interval_equal_rates_does_not_exclude_zero() {
        let ci = wilson_difference_interval(50, 100, 50, 100);
        assert!(!ci.excludes_zero);
        assert!(ci.lower <= 0.0);
        assert!(ci.upper >= 0.0);
    }

    #[test]
    fn wilson_difference_interval_zero_rates_does_not_exclude_zero() {
        let ci = wilson_difference_interval(0, 100, 0, 100);
        assert!(!ci.excludes_zero);
    }

    #[test]
    fn wilson_difference_interval_boundary_case() {
        // Very small sample with pseudo-count.
        let ci = wilson_difference_interval(1, 2, 0, 2);
        // With such small samples, the interval should be wide.
        assert!(ci.upper - ci.lower > 0.5);
    }

    #[test]
    fn edit_minus_apply_patch_interval_with_pseudo_count() {
        // Larger sample: 8/10 edit failures vs 1/10 apply_patch failures.
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
                Some("io"),
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
                Some("timeout"),
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
                Some("validation"),
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
                Some("io"),
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
                Some("validation"),
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
                "error",
                Some("validation"),
                None,
                false,
                None,
                None,
                "h8",
            ),
            row(
                "s1",
                Some("t1"),
                8,
                "edit",
                "success",
                None,
                None,
                false,
                None,
                None,
                "h9",
            ),
            row(
                "s1",
                Some("t1"),
                9,
                "edit",
                "success",
                None,
                None,
                false,
                None,
                None,
                "h10",
            ),
            // 1 apply_patch failure, 9 successes.
            row(
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
            ),
            row(
                "s1",
                Some("t1"),
                11,
                "apply_patch",
                "success",
                None,
                None,
                false,
                None,
                None,
                "h12",
            ),
            row(
                "s1",
                Some("t1"),
                12,
                "apply_patch",
                "success",
                None,
                None,
                false,
                None,
                None,
                "h13",
            ),
            row(
                "s1",
                Some("t1"),
                13,
                "apply_patch",
                "success",
                None,
                None,
                false,
                None,
                None,
                "h14",
            ),
            row(
                "s1",
                Some("t1"),
                14,
                "apply_patch",
                "success",
                None,
                None,
                false,
                None,
                None,
                "h15",
            ),
            row(
                "s1",
                Some("t1"),
                15,
                "apply_patch",
                "success",
                None,
                None,
                false,
                None,
                None,
                "h16",
            ),
            row(
                "s1",
                Some("t1"),
                16,
                "apply_patch",
                "success",
                None,
                None,
                false,
                None,
                None,
                "h17",
            ),
            row(
                "s1",
                Some("t1"),
                17,
                "apply_patch",
                "success",
                None,
                None,
                false,
                None,
                None,
                "h18",
            ),
            row(
                "s1",
                Some("t1"),
                18,
                "apply_patch",
                "success",
                None,
                None,
                false,
                None,
                None,
                "h19",
            ),
            row(
                "s1",
                Some("t1"),
                19,
                "apply_patch",
                "success",
                None,
                None,
                false,
                None,
                None,
                "h20",
            ),
        ];
        let (ci, rates) = edit_minus_apply_patch_failure_interval(&rows);
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
}
