//! Evidence-based task-run progress classification.
//!
//! A finished task-run can report success and still have done *nothing*: the
//! worker exited cleanly, but produced no git diff, touched no files, and left
//! the task's acceptance criteria exactly as it found them. djinn already makes
//! "no-op" decisions in a few places, but each does so ad-hoc:
//!
//! - The supervisor PR-open path closes a task as completed when its branch has
//!   **zero commits ahead** of the merge target (`task_branch_commits_ahead` in
//!   `djinn-agent/src/supervisor_impl/pr.rs`) — the canonical "No commits
//!   between <base> and task/<id>" guard.
//! - The coordinator's PR poller short-circuits a re-dispatch when the PR head
//!   is `ahead_by == 0` (`compare_commits_ahead_by` in
//!   `djinn-agent/src/actors/coordinator/pr_poller.rs`).
//! - Session extraction computes a deduplicated **files-changed** count from the
//!   transcript (`SessionSignals::taxonomy.files_changed` in
//!   `djinn-agent/src/actors/slot/session_extraction.rs`).
//! - Finalize applies per-criterion **AC verdicts** (`AcVerdict.met` /
//!   `apply_ac_verdicts` in `djinn-agent/src/actors/slot/finalize_handlers.rs`),
//!   which is how a run records acceptance-criteria progress.
//!
//! This module unifies those *hard-evidence* signals — none of them wall-clock
//! based — into one pure, deterministic classifier so callers (D3b/D3c, later)
//! share a single definition of "did this run actually make progress?".
//!
//! The classifier is intentionally **not** time-aware: liveness/stall detection
//! (see [`crate::liveness`]) already covers "is the run hung?". This answers the
//! orthogonal question "the run *finished* — but did it accomplish anything?".

/// The hard-evidence signals a finished task-run leaves behind.
///
/// Every field mirrors a value djinn already computes elsewhere; nothing here
/// is newly derived. Keep this minimal — only genuinely cheap, already-present
/// evidence belongs here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunProgressSignals {
    /// Commits on the task branch ahead of the merge target.
    ///
    /// Source of truth: `task_branch_commits_ahead` (supervisor PR-open guard)
    /// / `compare_commits_ahead_by` (PR-poller). `0` is the canonical "No
    /// commits between <base> and task/<id>" no-op condition.
    pub commits_ahead: u32,

    /// Distinct files the run modified, as counted from the session transcript.
    ///
    /// Source of truth: `SessionSignals::taxonomy.files_changed` (deduplicated
    /// per path in `session_extraction.rs`). Captures work that exists in the
    /// workspace even if it was never committed (uncommitted edits, notes).
    pub files_changed: u32,

    /// Number of acceptance criteria this run flipped from unmet → met.
    ///
    /// Source of truth: the delta in `AcVerdict.met` flags applied by
    /// `apply_ac_verdicts` during finalize. `0` means the run advanced no
    /// acceptance criterion (it may have started with some already met — this
    /// counts only *newly* satisfied ones, so it is a true progress delta).
    pub ac_newly_satisfied: u32,
}

impl RunProgressSignals {
    /// True when no commit was added and no file was touched.
    ///
    /// This is the "physical" no-diff condition shared by the supervisor and
    /// PR-poller guards, independent of acceptance-criteria bookkeeping.
    fn no_physical_change(&self) -> bool {
        self.commits_ahead == 0 && self.files_changed == 0
    }
}

/// The verdict for a finished run, by descending strength of evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunProgress {
    /// Real evidence of work: at least one commit ahead **or** at least one
    /// file changed. This is the mirror of "has commits ahead of base", which
    /// is exactly the condition the supervisor/PR-poller treat as "there is
    /// something to open a PR with".
    Productive,

    /// No physical change *and* no acceptance-criteria advanced. The run
    /// reported done but left no trace — the canonical "succeeded but did
    /// nothing" case the supervisor force-closes as completed.
    NoOp,

    /// Ambiguous middle: no commits and no files changed, yet the run *did*
    /// flip one or more acceptance criteria to met. Evidence and bookkeeping
    /// disagree (e.g. an AC marked satisfied with no corresponding diff), so a
    /// caller should look closer rather than auto-close or auto-accept.
    Inconclusive,
}

/// Classify whether a finished run made progress, from hard evidence only.
///
/// Pure, deterministic, side-effect-free — a function of its input alone.
///
/// Decision order (strongest evidence first):
/// 1. **Productive** — `commits_ahead > 0` *or* `files_changed > 0`. Any
///    physical change in the workspace or on the branch is real work.
/// 2. **NoOp** — no physical change *and* `ac_newly_satisfied == 0`. Nothing
///    happened on any axis djinn can measure.
/// 3. **Inconclusive** — no physical change but `ac_newly_satisfied > 0`. The
///    run claims acceptance progress with nothing to show for it physically;
///    the signals contradict each other, so don't decide automatically.
pub fn classify_run_progress(signals: &RunProgressSignals) -> RunProgress {
    if !signals.no_physical_change() {
        // commits_ahead > 0 || files_changed > 0
        RunProgress::Productive
    } else if signals.ac_newly_satisfied == 0 {
        RunProgress::NoOp
    } else {
        RunProgress::Inconclusive
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn signals(
        commits_ahead: u32,
        files_changed: u32,
        ac_newly_satisfied: u32,
    ) -> RunProgressSignals {
        RunProgressSignals {
            commits_ahead,
            files_changed,
            ac_newly_satisfied,
        }
    }

    // ── Productive: any physical evidence ───────────────────────────────────

    #[test]
    fn commits_only_is_productive() {
        assert_eq!(
            classify_run_progress(&signals(1, 0, 0)),
            RunProgress::Productive
        );
    }

    #[test]
    fn files_only_is_productive() {
        assert_eq!(
            classify_run_progress(&signals(0, 1, 0)),
            RunProgress::Productive
        );
    }

    #[test]
    fn commits_and_files_is_productive() {
        assert_eq!(
            classify_run_progress(&signals(3, 7, 0)),
            RunProgress::Productive
        );
    }

    #[test]
    fn physical_change_wins_over_zero_ac() {
        // Commits + files present but no AC flipped: still Productive — physical
        // evidence outranks acceptance-criteria bookkeeping.
        assert_eq!(
            classify_run_progress(&signals(2, 2, 0)),
            RunProgress::Productive
        );
    }

    #[test]
    fn physical_change_with_ac_is_productive() {
        assert_eq!(
            classify_run_progress(&signals(1, 1, 4)),
            RunProgress::Productive
        );
    }

    #[test]
    fn large_counts_are_productive() {
        assert_eq!(
            classify_run_progress(&signals(u32::MAX, u32::MAX, u32::MAX)),
            RunProgress::Productive
        );
    }

    // ── NoOp: nothing on any axis ───────────────────────────────────────────

    #[test]
    fn all_zero_is_noop() {
        assert_eq!(classify_run_progress(&signals(0, 0, 0)), RunProgress::NoOp);
    }

    #[test]
    fn default_signals_are_noop() {
        // The Default impl is all-zero, which must classify as the canonical
        // "did nothing" no-op.
        assert_eq!(
            classify_run_progress(&RunProgressSignals::default()),
            RunProgress::NoOp
        );
    }

    // ── Inconclusive: AC progress with no physical change ───────────────────

    #[test]
    fn ac_only_is_inconclusive() {
        // One AC flipped to met but zero commits and zero files: evidence and
        // bookkeeping disagree.
        assert_eq!(
            classify_run_progress(&signals(0, 0, 1)),
            RunProgress::Inconclusive
        );
    }

    #[test]
    fn many_ac_no_physical_is_inconclusive() {
        assert_eq!(
            classify_run_progress(&signals(0, 0, 5)),
            RunProgress::Inconclusive
        );
    }

    // ── Exhaustive small-domain sweep ───────────────────────────────────────

    #[test]
    fn exhaustive_over_small_domain() {
        // Walk every combination of {0,1,2} across all three axes and assert
        // the verdict matches the documented decision order, leaving no
        // unintended branch.
        for c in 0u32..=2 {
            for f in 0u32..=2 {
                for a in 0u32..=2 {
                    let got = classify_run_progress(&signals(c, f, a));
                    let want = if c > 0 || f > 0 {
                        RunProgress::Productive
                    } else if a == 0 {
                        RunProgress::NoOp
                    } else {
                        RunProgress::Inconclusive
                    };
                    assert_eq!(got, want, "c={c} f={f} a={a}");
                }
            }
        }
    }

    #[test]
    fn classifier_is_deterministic() {
        let s = signals(0, 0, 2);
        assert_eq!(classify_run_progress(&s), classify_run_progress(&s));
    }
}
