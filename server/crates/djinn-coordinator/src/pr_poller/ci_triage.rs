//! Causal ranking of blocking CI checks, and capture of the evidence that
//! actually diagnoses a failure.
//!
//! # Why this module exists
//!
//! GitHub's `gh run cancel` — used by this repo's in-shard fail-fast watcher,
//! and by GitHub's own `fail-fast:` matrices — is **run-level**. When one lane
//! trips it, every in-flight sibling is stamped `cancelled` and every
//! `needs:`-dependent aggregator is sealed as a check run that never executed.
//! A single root cause therefore renders as a fan of red required checks, only
//! one of which knows anything.
//!
//! The historical derivation of "the check to look at first" was *the
//! alphabetically first blocking check name*. On real incident run
//! `30087861197` that selected `Publish Nextest Timing` — a `needs:`-dependent
//! aggregator whose `completed_at` preceded its own `started_at`, with zero
//! annotations, which never executed at all. Agents dispatched off that signal
//! searched a lane that by construction cannot be a root cause.
//!
//! # The structural rule
//!
//! Rank by **evidence produced**, never by identity. A deny list of job names
//! ("Publish Nextest Timing", "Quality Gate") would fix exactly one incident
//! and rot the first time a job is renamed. The signals used here are
//! properties of how GitHub Actions executes any workflow:
//!
//! 1. **Conclusion class.** `failure` / `timed_out` mean the lane reached a
//!    verdict about its own work. `cancelled` means something else killed it;
//!    it says nothing about its own health.
//! 2. **Execution interval.** `completed_at - started_at` must be strictly
//!    positive for a lane to have run. GitHub reports a zero or *negative*
//!    interval for a job that was sealed before it ever dispatched work.
//! 3. **Annotation count.** A lane that produced annotations produced a
//!    diagnosis, which is the thing triage actually needs.
//! 4. **Earliest start.** This is the anti-aggregator rule, and it is
//!    topological rather than heuristic: a `needs:`-dependent job *cannot*
//!    start before the jobs it depends on have completed. Ordering by
//!    `started_at` therefore orders upstream-before-downstream within a run,
//!    for any workflow graph, without knowing the graph.
//!
//! Rule 4 is what makes this durable. Rules 1-3 identify lanes that carry
//! information; rule 4 picks the one furthest upstream among them.

use djinn_provider::github_api::{CheckAnnotation, CheckRun};

/// Maximum number of annotations rendered into the durable snapshot.
///
/// Annotations flow into an agent prompt, so the bound is deliberately tight.
/// Runner-host failures and compiler diagnostics both put the decisive line
/// first; the tail is repetition.
pub(crate) const MAX_CAPTURED_ANNOTATIONS: usize = 5;

/// Maximum characters of any single annotation message retained.
pub(crate) const MAX_ANNOTATION_MESSAGE_CHARS: usize = 400;

/// Hard ceiling on the whole rendered annotation block, applied after
/// per-message truncation. Bounds the worst case regardless of input.
pub(crate) const MAX_ANNOTATION_BLOCK_CHARS: usize = 2_000;

/// How much causal information a failing check run carries about its own health.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CheckEvidence {
    /// The lane executed and reached a failing verdict about its own work.
    /// This is the only class that can be a root cause.
    Causal,
    /// The lane executed but was `cancelled` before finishing. It may well have
    /// been the trigger, but its conclusion is not its own — weak evidence.
    RanThenCancelled,
    /// The lane never executed: a non-positive execution interval and no
    /// annotations. A `needs:`-dependent aggregator under a run-level cancel
    /// looks exactly like this. It is a symptom by construction.
    NeverExecuted,
}

/// Annotation count reported by GitHub for this check run.
pub(crate) fn annotations_count(cr: &CheckRun) -> u64 {
    cr.output
        .as_ref()
        .and_then(|o| o.annotations_count)
        .unwrap_or(0)
}

/// Whether the check run finished strictly after it started.
///
/// We need an *ordering*, not a duration, so this compares the timestamps
/// lexicographically rather than pulling in a date-time dependency: GitHub
/// emits check-run timestamps as fixed-width RFC-3339 UTC (`...Z`), a format in
/// which lexicographic order is chronological order.
///
/// `None` means the comparison is not meaningful — either endpoint missing, or
/// the two strings are not in the same fixed-width `Z` form. That is *not*
/// evidence of non-execution, and [`executed`] treats it accordingly.
pub(crate) fn completed_after_start(cr: &CheckRun) -> Option<bool> {
    let started = cr.started_at.as_deref()?;
    let completed = cr.completed_at.as_deref()?;
    let comparable = |s: &str| s.ends_with('Z') && !s.contains('+');
    if !comparable(started) || !comparable(completed) || started.len() != completed.len() {
        return None;
    }
    Some(completed > started)
}

/// Whether this check run actually executed.
///
/// Conservative by design: only a *proven* non-execution demotes a lane. A lane
/// that produced annotations executed regardless of its timestamps, and a lane
/// with unparseable or absent timestamps is assumed to have executed, so a real
/// failure is never demoted because GitHub omitted a field.
pub(crate) fn executed(cr: &CheckRun) -> bool {
    if annotations_count(cr) > 0 {
        return true;
    }
    completed_after_start(cr).unwrap_or(true)
}

/// True when the conclusion is a *hard* failure — the lane's own work broke, as
/// opposed to `cancelled`, which under run-level fail-fast means something else
/// killed it.
fn hard_failed(cr: &CheckRun) -> bool {
    matches!(
        cr.conclusion.as_deref(),
        Some("failure") | Some("timed_out")
    )
}

/// Classify a failing check run by the evidence it carries.
pub(crate) fn check_evidence(cr: &CheckRun) -> CheckEvidence {
    if !executed(cr) {
        return CheckEvidence::NeverExecuted;
    }
    if hard_failed(cr) {
        CheckEvidence::Causal
    } else {
        CheckEvidence::RanThenCancelled
    }
}

/// Sort key: evidence class, then earliest start, then annotations-first, then
/// name for total determinism.
///
/// `started_at` sorts lexicographically because RFC-3339 UTC timestamps are
/// lexicographically ordered; a missing `started_at` sorts last (`~` exceeds
/// every digit) so lanes of unknown position never displace a known-upstream
/// lane.
fn sort_key(cr: &CheckRun) -> (CheckEvidence, String, std::cmp::Reverse<u64>, String) {
    (
        check_evidence(cr),
        cr.started_at.clone().unwrap_or_else(|| "~".to_string()),
        std::cmp::Reverse(annotations_count(cr)),
        cr.name.clone(),
    )
}

/// Order blocking checks so the most causally informative lane comes first.
///
/// The full ordered list is persisted (not just the winner) so an agent reading
/// the board sees the cascade in the order it should be investigated, with the
/// never-executed aggregators last where they belong.
pub(crate) fn rank_blocking_checks<'a>(blocking: &[&'a CheckRun]) -> Vec<&'a CheckRun> {
    let mut ranked: Vec<&'a CheckRun> = blocking.to_vec();
    ranked.sort_by_key(|cr| sort_key(cr));
    ranked
}

/// The subset of blocking checks that carry causal information.
///
/// This is the correct input for failure fingerprinting: cancelled siblings and
/// never-executed aggregators carry generic, incident-independent messages, so
/// including them collapses distinct failures into one signature and makes
/// `same_signature_count` count noise rather than recurrence.
pub(crate) fn causal_checks<'a>(blocking: &[&'a CheckRun]) -> Vec<&'a CheckRun> {
    rank_blocking_checks(blocking)
        .into_iter()
        .filter(|cr| check_evidence(cr) == CheckEvidence::Causal)
        .collect()
}

/// The single check to triage first, or `None` when nothing blocking carries
/// causal information.
///
/// `None` is the *inconclusive* case: every blocking check was cancelled or
/// never executed, so the run reached no verdict about the code. Callers must
/// treat that as a reason to retrigger, never as a reason to remediate.
pub(crate) fn primary_blocking_check<'a>(blocking: &[&'a CheckRun]) -> Option<&'a CheckRun> {
    causal_checks(blocking).into_iter().next()
}

/// True when a completed run reached no verdict: it has blocking failures, but
/// none of them carry causal information.
pub(crate) fn is_inconclusive(blocking: &[&CheckRun]) -> bool {
    !blocking.is_empty() && primary_blocking_check(blocking).is_none()
}

/// Render annotations into a bounded block suitable for an agent prompt.
///
/// Returns `None` when there is nothing to say. Truncation is explicit in the
/// output so a reader can tell evidence was elided rather than absent.
pub(crate) fn render_annotations(
    check_name: &str,
    annotations: &[CheckAnnotation],
) -> Option<String> {
    if annotations.is_empty() {
        return None;
    }

    // Failures first: GitHub returns annotations in file order, but a `failure`
    // annotation buried under warnings is the one that matters.
    let mut ordered: Vec<&CheckAnnotation> = annotations.iter().collect();
    ordered.sort_by_key(|a| match a.annotation_level.as_str() {
        "failure" => 0,
        "warning" => 1,
        _ => 2,
    });

    let mut out = format!("Annotations on `{check_name}`:");
    for a in ordered.iter().take(MAX_CAPTURED_ANNOTATIONS) {
        let message = truncate_chars(a.message.trim(), MAX_ANNOTATION_MESSAGE_CHARS);
        let location = if a.path.is_empty() {
            String::new()
        } else if a.start_line > 0 {
            format!(" {}:{}", a.path, a.start_line)
        } else {
            format!(" {}", a.path)
        };
        let title = match a.title.as_deref().map(str::trim) {
            Some(t) if !t.is_empty() => format!(" {t} —"),
            _ => String::new(),
        };
        out.push_str(&format!(
            "\n- [{}]{}{} {}",
            a.annotation_level, location, title, message
        ));
    }
    if annotations.len() > MAX_CAPTURED_ANNOTATIONS {
        out.push_str(&format!(
            "\n- (+{} more annotation(s) not shown)",
            annotations.len() - MAX_CAPTURED_ANNOTATIONS
        ));
    }

    Some(truncate_chars(&out, MAX_ANNOTATION_BLOCK_CHARS))
}

/// Truncate on a character boundary, appending an explicit elision marker.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let kept: String = s.chars().take(max).collect();
    format!("{kept}… [truncated]")
}

#[cfg(test)]
mod tests;
