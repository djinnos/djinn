//! Pure durable-progress detector for reply-loop shadow evaluation.
//!
//! This module provides a deterministic, independently unit-testable detector
//! that evaluates whether a reply-loop turn produced meaningful durable progress.
//! It performs **no live IO** — all inputs are pure value types (worktree
//! snapshots, command identity/result transitions, configuration).
//!
//! The detector is designed for shadow-mode operation first: it produces
//! structured [`DurableProgressObservation`]s that downstream wiring (task yttk)
//! can emit as metrics without affecting worker control flow.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};

use djinn_coordinator::{
    DurableProgressNoResetReason, DurableProgressResetReason, NoProgressThresholdConfig,
};

use serde::{Deserialize, Serialize};

// ─── Configuration ──────────────────────────────────────────────────────────

/// Default number of consecutive flaky observations tolerated before they stop
/// being treated as inconclusive.
const DEFAULT_FLAKY_GRACE_TURNS: u32 = 2;

/// Default long-running command duration (seconds) at which evaluation is
/// suspended rather than counted as a non-progress turn.
const DEFAULT_LONG_COMMAND_SUSPENSION_SECS: u64 = 10 * 60;

// ─── Input types (pure values, no IO) ───────────────────────────────────────

/// Classification of a file path in the worktree snapshot.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileClassification {
    /// Regular tracked source file.
    Tracked,
    /// New untracked file.
    Untracked,
    /// File matching gitignore or equivalent ignore rules.
    Ignored,
    /// Auto-generated file (build artifacts, lock files, etc.).
    Generated,
}

/// A single file entry in a worktree snapshot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Repo-relative path.
    pub path: String,
    /// Classification of the file.
    pub classification: FileClassification,
    /// Content hash for change detection; empty string means "no content".
    pub content_hash: String,
}

/// Snapshot of the worktree at a point in time.
///
/// Each entry represents a file that differs from the task's base ref.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorktreeSnapshot {
    pub entries: Vec<FileEntry>,
}

/// Deterministic command identity: what command was run.
#[derive(Clone, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct CommandIdentity {
    /// Tool name (e.g. "shell", "read").
    pub tool_name: String,
    /// Normalized arguments string.
    pub normalized_args: String,
    /// Stable digest of the above.
    pub digest: String,
}

/// Result classification for a command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandResultClass {
    /// Command exited successfully (exit 0).
    Green,
    /// Command exited with failure (non-zero exit).
    Red,
    /// Result could not be determined.
    Unknown,
    /// Command is still running or exceeded the suspension threshold.
    LongRunning,
}

/// Transition from before → after command result.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CommandResultTransition {
    /// Identity of the command that was run.
    pub command: CommandIdentity,
    /// Previous result for this command, if known.
    pub before: Option<CommandResultClass>,
    /// Current result after this turn.
    pub after: CommandResultClass,
}

/// Input snapshot pair for a single turn evaluation.
#[derive(Clone, Debug)]
pub struct TurnInput {
    /// Worktree state before the turn (None for the first turn).
    pub before: Option<WorktreeSnapshot>,
    /// Worktree state after the turn.
    pub after: WorktreeSnapshot,
    /// Command result transition, if a verification/test command was run.
    pub command_result: Option<CommandResultTransition>,
    /// Whether the assistant turn was read-only or no-op (e.g. only read
    /// tool calls, no write/shell).
    pub is_read_only_turn: bool,
    /// How long the primary command ran, in seconds. `None` if no command.
    pub command_duration_secs: Option<u64>,
    /// Turn index within the session (0-based).
    pub turn_index: u32,
}

// ─── Output types ───────────────────────────────────────────────────────────

/// Outcome of filtered path changes (ignored/generated classification).
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct IgnoredGeneratedOutcome {
    /// Total file entries that changed between before and after.
    pub total_changes: usize,
    /// Tracked file changes.
    pub tracked_changes: usize,
    /// Untracked file changes.
    pub untracked_changes: usize,
    /// Ignored file changes.
    pub ignored_changes: usize,
    /// Generated file changes.
    pub generated_changes: usize,
    /// True if the only changes are generated files.
    pub is_generated_only: bool,
    /// True if the only changes are ignored files.
    pub is_ignored_only: bool,
}

/// Inputs for updating the no-progress streak in the loop guard.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct NoProgressStreakUpdate {
    /// Whether to increment the streak (true) or reset it (false).
    pub should_increment: bool,
    /// The streak length after applying this update (caller's current + 1, or 0).
    pub streak_after: u32,
}

/// Structured observation from the durable-progress detector.
///
/// Contains everything a downstream event emitter needs to produce metrics,
/// shadow observations, and streak-update instructions — without enforcing
/// any session termination or checkpoint behavior.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DurableProgressObservation {
    /// Whether this turn produced durable progress.
    pub is_durable_progress: bool,

    /// Why the no-progress streak was reset, if it was.
    pub reset_reason: Option<DurableProgressResetReason>,

    /// Why this turn did NOT reset the streak, if applicable.
    pub no_reset_reason: Option<DurableProgressNoResetReason>,

    /// Worktree file change classification.
    pub worktree_outcome: IgnoredGeneratedOutcome,

    /// Command identity and result transition, if a command was run.
    pub command_transition: Option<CommandResultTransition>,

    /// Inputs for updating the no-progress streak in the loop guard.
    pub no_progress_streak: NoProgressStreakUpdate,

    /// Index of the evaluated turn.
    pub evaluated_turn_index: u32,

    /// Whether the detector is running in shadow mode (always true for now).
    pub shadow_mode: bool,

    /// Whether the command result was classified as flaky/inconclusive.
    pub flaky_observation: bool,

    /// Whether a long-running command suspended evaluation.
    pub long_command_suspended: bool,

    /// Current flaky streak for the observed command, if any.
    pub command_flaky_streak: Option<u32>,
}

// ─── Detector state ─────────────────────────────────────────────────────────

/// Pure stateful durable-progress detector.
///
/// Maintains command-result history for flaky detection and previously-green
/// command tracking. The [`evaluate`] method takes a [`TurnInput`] and produces
/// a [`DurableProgressObservation`] without performing any IO.
#[derive(Clone, Debug)]
pub struct DurableProgressDetector {
    /// Previous command results keyed by command digest for flaky detection.
    command_result_history: HashMap<String, Vec<CommandResultClass>>,
    /// Set of command digests that have been observed as green at least once.
    previously_green_commands: HashSet<String>,
    /// Number of turns evaluated so far.
    evaluated_turn_count: u32,
    /// Consecutive flaky command observations per command digest.
    flaky_streaks: HashMap<String, u32>,
    /// Flaky command grace turns threshold.
    flaky_grace_turns: u32,
    /// Long command suspension threshold in seconds.
    long_command_suspension_secs: u64,
}

impl DurableProgressDetector {
    /// Create a new detector with default thresholds.
    pub fn new() -> Self {
        Self {
            command_result_history: HashMap::new(),
            previously_green_commands: HashSet::new(),
            evaluated_turn_count: 0,
            flaky_streaks: HashMap::new(),
            flaky_grace_turns: DEFAULT_FLAKY_GRACE_TURNS,
            long_command_suspension_secs: DEFAULT_LONG_COMMAND_SUSPENSION_SECS,
        }
    }

    /// Create a detector with explicit thresholds (for testing or config-driven
    /// construction).
    pub fn with_thresholds(flaky_grace_turns: u32, long_command_suspension_secs: u64) -> Self {
        Self {
            command_result_history: HashMap::new(),
            previously_green_commands: HashSet::new(),
            evaluated_turn_count: 0,
            flaky_streaks: HashMap::new(),
            flaky_grace_turns,
            long_command_suspension_secs,
        }
    }

    /// Create a detector from a [`NoProgressThresholdConfig`].
    pub fn from_threshold_config(config: &NoProgressThresholdConfig) -> Self {
        Self::with_thresholds(
            config.flaky_command_grace_turns,
            config.long_command_suspension_secs,
        )
    }

    /// Number of turns evaluated so far.
    pub fn evaluated_turn_count(&self) -> u32 {
        self.evaluated_turn_count
    }

    /// Evaluate a turn and produce a structured observation.
    ///
    /// This is the core evaluation path. It is pure in the sense that it
    /// reads only from `input` and `self` state, and produces only the
    /// observation output (plus internal state updates for history tracking).
    pub fn evaluate(&mut self, input: TurnInput) -> DurableProgressObservation {
        self.evaluated_turn_count += 1;
        let turn_index = input.turn_index;

        // ── Step 1: Long-command suspension ──────────────────────────────
        if let Some(duration) = input.command_duration_secs
            && duration >= self.long_command_suspension_secs
        {
            // Suspend evaluation: don't count as no-progress, don't reset.
            return DurableProgressObservation {
                is_durable_progress: false,
                reset_reason: None,
                no_reset_reason: Some(DurableProgressNoResetReason::LongCommandSuspended),
                worktree_outcome: compute_worktree_outcome(input.before.as_ref(), &input.after),
                command_transition: input.command_result.clone(),
                no_progress_streak: NoProgressStreakUpdate {
                    should_increment: false,
                    streak_after: 0, // caller should preserve current streak
                },
                evaluated_turn_index: turn_index,
                shadow_mode: true,
                flaky_observation: false,
                long_command_suspended: true,
                command_flaky_streak: None,
            };
        }

        // ── Step 2: Compute worktree outcome ────────────────────────────
        let worktree_outcome = compute_worktree_outcome(input.before.as_ref(), &input.after);

        // ── Step 3: Read-only / no-op turn check ────────────────────────
        if input.is_read_only_turn && input.command_result.is_none() {
            return self.no_progress_observation(
                turn_index,
                DurableProgressNoResetReason::ReadOnlyOrNoOpToolSuccess,
                worktree_outcome,
                None,
                false,
            );
        }

        // ── Step 4: Generated / ignored-only change check ───────────────
        if worktree_outcome.is_generated_only || worktree_outcome.is_ignored_only {
            let reason = DurableProgressNoResetReason::GeneratedOnlyChange;
            return self.no_progress_observation(
                turn_index,
                reason,
                worktree_outcome,
                input.command_result.clone(),
                false,
            );
        }

        // ── Step 5: Command result evaluation ───────────────────────────
        if let Some(ref transition) = input.command_result {
            let digest = transition.command.digest.clone();

            // Record result history for flaky detection.
            self.command_result_history
                .entry(digest.clone())
                .or_default()
                .push(transition.after);

            let was_previously_green = self.previously_green_commands.contains(&digest);

            // Track green commands.
            if transition.after == CommandResultClass::Green {
                self.previously_green_commands.insert(digest.clone());
            }

            // ── Step 5a: Long-running command in result ─────────────────
            if transition.after == CommandResultClass::LongRunning {
                return self.no_progress_observation(
                    turn_index,
                    DurableProgressNoResetReason::LongCommandSuspended,
                    worktree_outcome,
                    input.command_result.clone(),
                    false,
                );
            }

            // ── Step 5b: Flaky detection ────────────────────────────────
            if self.is_flaky_command(&digest) {
                self.flaky_streaks
                    .entry(digest.clone())
                    .and_modify(|s| *s = s.saturating_add(1))
                    .or_insert(1);

                return self.no_progress_observation(
                    turn_index,
                    DurableProgressNoResetReason::FlakyCommandResult,
                    worktree_outcome,
                    input.command_result.clone(),
                    true,
                );
            }
            // Reset flaky streak if result is stable.
            self.flaky_streaks.remove(&digest);

            // ── Step 5c: Already-green rerun ────────────────────────────
            if transition.after == CommandResultClass::Green && was_previously_green {
                // Only mark as already-green if this isn't the first time
                // we see this command as green (i.e., we've seen it green before
                // AND the transition came from green→green).
                if transition.before == Some(CommandResultClass::Green) {
                    return self.no_progress_observation(
                        turn_index,
                        DurableProgressNoResetReason::AlreadyGreenVerificationRerun,
                        worktree_outcome,
                        input.command_result.clone(),
                        false,
                    );
                }
            }

            // ── Step 5d: Newly-green command ────────────────────────────
            if transition.after == CommandResultClass::Green {
                let before_was_not_green = matches!(
                    transition.before,
                    Some(CommandResultClass::Red) | Some(CommandResultClass::Unknown) | None
                );
                if before_was_not_green {
                    return self.durable_progress_observation(
                        turn_index,
                        DurableProgressResetReason::NewlyGreenVerification,
                        worktree_outcome,
                        input.command_result.clone(),
                    );
                }
            }

            // ── Step 5e: Command went red or unknown ────────────────────
            if transition.after == CommandResultClass::Red
                || transition.after == CommandResultClass::Unknown
            {
                return self.no_progress_observation(
                    turn_index,
                    DurableProgressNoResetReason::ReadOnlyOrNoOpToolSuccess,
                    worktree_outcome,
                    input.command_result.clone(),
                    false,
                );
            }
        }

        // ── Step 6: Worktree diff-based progress ────────────────────────
        // At this point we have tracked/untracked changes (not generated-only,
        // not ignored-only) and either no command result or a green→green that
        // wasn't caught by the already-green rerun check.
        if worktree_outcome.tracked_changes > 0 || worktree_outcome.untracked_changes > 0 {
            return self.durable_progress_observation(
                turn_index,
                DurableProgressResetReason::WorktreeChanged,
                worktree_outcome,
                input.command_result.clone(),
            );
        }

        // ── Step 7: Fallback — no actionable signal ─────────────────────
        self.no_progress_observation(
            turn_index,
            DurableProgressNoResetReason::ReadOnlyOrNoOpToolSuccess,
            worktree_outcome,
            input.command_result.clone(),
            false,
        )
    }

    // ─── Private helpers ─────────────────────────────────────────────────

    fn is_flaky_command(&self, digest: &str) -> bool {
        let Some(history) = self.command_result_history.get(digest) else {
            return false;
        };
        if history.len() < 2 {
            return false;
        }
        // Check for oscillation: the last two results differ (green↔red or
        // similar). This catches intermittent pass/fail patterns.
        let last = history[history.len() - 1];
        let second_last = history[history.len() - 2];
        last != second_last
            && !matches!(
                last,
                CommandResultClass::Unknown | CommandResultClass::LongRunning
            )
            && !matches!(
                second_last,
                CommandResultClass::Unknown | CommandResultClass::LongRunning
            )
    }

    fn durable_progress_observation(
        &mut self,
        turn_index: u32,
        reason: DurableProgressResetReason,
        worktree_outcome: IgnoredGeneratedOutcome,
        command_transition: Option<CommandResultTransition>,
    ) -> DurableProgressObservation {
        DurableProgressObservation {
            is_durable_progress: true,
            reset_reason: Some(reason),
            no_reset_reason: None,
            worktree_outcome,
            command_transition,
            no_progress_streak: NoProgressStreakUpdate {
                should_increment: false,
                streak_after: 0,
            },
            evaluated_turn_index: turn_index,
            shadow_mode: true,
            flaky_observation: false,
            long_command_suspended: false,
            command_flaky_streak: None,
        }
    }

    fn no_progress_observation(
        &self,
        turn_index: u32,
        reason: DurableProgressNoResetReason,
        worktree_outcome: IgnoredGeneratedOutcome,
        command_transition: Option<CommandResultTransition>,
        flaky: bool,
    ) -> DurableProgressObservation {
        let flaky_streak = if flaky {
            command_transition
                .as_ref()
                .and_then(|t| self.flaky_streaks.get(&t.command.digest).copied())
        } else {
            None
        };

        DurableProgressObservation {
            is_durable_progress: false,
            reset_reason: None,
            no_reset_reason: Some(reason),
            worktree_outcome,
            command_transition,
            no_progress_streak: NoProgressStreakUpdate {
                should_increment: true,
                streak_after: 0, // caller computes: current + 1
            },
            evaluated_turn_index: turn_index,
            shadow_mode: true,
            flaky_observation: flaky,
            long_command_suspended: false,
            command_flaky_streak: flaky_streak,
        }
    }
}

impl Default for DurableProgressDetector {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Pure helper functions ──────────────────────────────────────────────────

/// Compute the file-change classification from before/after worktree snapshots.
///
/// Files present in `after` but not `before` (or with different content hashes)
/// are counted as changes, bucketed by classification. Files present in `before`
/// but not `after` (deleted) are also counted.
pub fn compute_worktree_outcome(
    before: Option<&WorktreeSnapshot>,
    after: &WorktreeSnapshot,
) -> IgnoredGeneratedOutcome {
    let Some(before) = before else {
        // First turn: everything in `after` is a change.
        return classify_entries(&after.entries);
    };

    // Build lookup maps for before and after.
    let before_map: HashMap<&str, (&FileClassification, &str)> = before
        .entries
        .iter()
        .map(|e| {
            (
                e.path.as_str(),
                (&e.classification, e.content_hash.as_str()),
            )
        })
        .collect();
    let after_map: HashMap<&str, (&FileClassification, &str)> = after
        .entries
        .iter()
        .map(|e| {
            (
                e.path.as_str(),
                (&e.classification, e.content_hash.as_str()),
            )
        })
        .collect();

    let mut total = 0usize;
    let mut tracked = 0usize;
    let mut untracked = 0usize;
    let mut ignored = 0usize;
    let mut generated = 0usize;

    // New or modified files in `after`.
    for (path, (class, hash)) in &after_map {
        match before_map.get(*path) {
            Some((_, old_hash)) if *old_hash == *hash => {
                // Unchanged.
            }
            _ => {
                total += 1;
                match class {
                    FileClassification::Tracked => tracked += 1,
                    FileClassification::Untracked => untracked += 1,
                    FileClassification::Ignored => ignored += 1,
                    FileClassification::Generated => generated += 1,
                }
            }
        }
    }

    // Deleted files (in before but not after).
    for (path, (class, _)) in &before_map {
        if !after_map.contains_key(*path) {
            total += 1;
            match class {
                FileClassification::Tracked => tracked += 1,
                FileClassification::Untracked => untracked += 1,
                FileClassification::Ignored => ignored += 1,
                FileClassification::Generated => generated += 1,
            }
        }
    }

    IgnoredGeneratedOutcome {
        total_changes: total,
        tracked_changes: tracked,
        untracked_changes: untracked,
        ignored_changes: ignored,
        generated_changes: generated,
        is_generated_only: total > 0 && tracked == 0 && untracked == 0 && ignored == 0,
        is_ignored_only: total > 0 && tracked == 0 && untracked == 0 && generated == 0,
    }
}

fn classify_entries(entries: &[FileEntry]) -> IgnoredGeneratedOutcome {
    let total = entries.len();
    let tracked = entries
        .iter()
        .filter(|e| e.classification == FileClassification::Tracked)
        .count();
    let untracked = entries
        .iter()
        .filter(|e| e.classification == FileClassification::Untracked)
        .count();
    let ignored = entries
        .iter()
        .filter(|e| e.classification == FileClassification::Ignored)
        .count();
    let generated = entries
        .iter()
        .filter(|e| e.classification == FileClassification::Generated)
        .count();

    IgnoredGeneratedOutcome {
        total_changes: total,
        tracked_changes: tracked,
        untracked_changes: untracked,
        ignored_changes: ignored,
        generated_changes: generated,
        is_generated_only: total > 0 && tracked == 0 && untracked == 0 && ignored == 0,
        is_ignored_only: total > 0 && tracked == 0 && untracked == 0 && generated == 0,
    }
}

#[cfg(test)]
mod tests;
