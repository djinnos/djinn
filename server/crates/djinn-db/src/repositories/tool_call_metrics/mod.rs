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

mod primitives;
mod shared;

pub use primitives::{ConfidenceInterval, Z_95, wilson_difference_interval, wilson_interval};

use shared::group_by_session_task;

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
    let groups = group_by_session_task(rows);
    let mut numerator = 0usize;
    let mut denominator = 0usize;

    for indices in &groups {
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

    let groups = group_by_session_task(rows);

    // Identify which rows are post-failure retries.
    // A successful modifying call is a post-failure retry if it follows a
    // failed edit within the next three turns (same path or args_hash).
    let mut retry_apply_patch_success = 0usize;
    let mut retry_edit_success = 0usize;

    for indices in &groups {
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
        let mut path_groups: std::collections::BTreeMap<Option<&str>, Vec<&NormalizedToolCallRow>> =
            std::collections::BTreeMap::new();
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
            // truncated or overlapping, before a modifying call.
            for start in 0..reads.len() {
                if start + 3 > reads.len() {
                    break;
                }
                let window = &reads[start..];
                // Find the maximal contiguous run starting at `start`.
                let first_turn = window[0].turn_index;
                let max_turn = first_turn + 6;

                // Collect reads within the 6-turn window.
                let mut in_window: Vec<&NormalizedToolCallRow> = Vec::new();
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
                let last_turn = in_window.last().unwrap().turn_index;
                let has_modification_before = session_rows.iter().any(|r| {
                    (r.tool_name == "edit" || r.tool_name == "apply_patch")
                        && r.result_status == "success"
                        && r.path.as_deref() == path
                        && r.turn_index >= first_turn
                        && r.turn_index <= last_turn
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
fn window_overlaps_any(read: &NormalizedToolCallRow, group: &[&NormalizedToolCallRow]) -> bool {
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
mod tests;
