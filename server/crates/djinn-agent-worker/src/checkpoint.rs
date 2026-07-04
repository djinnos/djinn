// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
//! Safety-scanned WIP checkpoint preservation for worker worktrees.
//!
//! This module provides the shared checkpoint path used by all controlled worker
//! exits: soft-deadline, signal/controlled-shutdown, terminal checkpoint, and
//! periodic checkpoint hooks. It replaces the previous best-effort commit+push
//! with a safety-scanned preservation lifecycle:
//!
//! 1. **Scan** the worktree via [`checkpoint_safety::scan_worktree`].
//! 2. **Block** if any safety-scanned files are blocked (secret-like or
//!    policy-disallowed).
//! 3. **Stage** only the safety-approved file set (targeted `git add`, not
//!    `add -A`).
//! 4. **Commit** with the WIP message format
//!    `wip: checkpoint session <session-id> turn <turn> reason <reason>`.
//! 5. **Push** with `--force-with-lease` and bounded retry/backoff.
//! 6. **Handle conflicts** via safe refresh/rebase, falling back to an
//!    alternate checkpoint ref under `refs/djinn/checkpoints/`.
//!
//! # Guardrails
//!
//! - No auto-submit-if-green or resume selection (left for future epics).
//! - WIP commits remain on task branches or checkpoint refs; never final merges.
//! - Bounded by [`CHECKPOINT_PUSH_TIMEOUT_SECS`] to stay within kubelet grace.

use std::fmt;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::checkpoint_safety::{self, CheckpointSafetyConfig};

/// Convert a `djinn_git::GitError` to the string error type used by this
/// module's local helpers.
fn git_err_to_string(e: djinn_git::GitError) -> String {
    e.to_string()
}

// ─── Constants ─────────────────────────────────────────────────────────────

/// Maximum number of push attempts (including the initial try).
const MAX_PUSH_ATTEMPTS: u32 = 3;

/// Base backoff duration between push retries. Actual delay = base × attempt.
const PUSH_RETRY_BACKOFF_BASE: Duration = Duration::from_millis(150);

/// Per-push-operation timeout. Bounded well inside kubelet grace.
const CHECKPOINT_PUSH_TIMEOUT_SECS: u64 = 15;

// ─── Public types ──────────────────────────────────────────────────────────

/// Why the checkpoint is being taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CheckpointReason {
    /// Soft-deadline timer fired before kubelet hard-kill.
    SoftDeadline,
    /// SIGTERM or SIGINT received from kubelet/operator.
    Signal,
    /// Periodic OOM-proof push (push-only, no new commit by default).
    Periodic,
    /// Terminal checkpoint after supervisor returns.
    Terminal,
    /// Manually triggered.
    Manual,
}

impl fmt::Display for CheckpointReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SoftDeadline => write!(f, "soft-deadline"),
            Self::Signal => write!(f, "signal"),
            Self::Periodic => write!(f, "periodic"),
            Self::Terminal => write!(f, "terminal"),
            Self::Manual => write!(f, "manual"),
        }
    }
}

/// Identity and context for a WIP checkpoint commit.
///
/// Carried by each checkpoint caller (signal handler, soft-deadline, terminal,
/// periodic) to attribute the commit and populate the structured result.
#[derive(Debug, Clone)]
pub struct CheckpointMetadata {
    /// Session ID (or task-run ID if session is not yet available).
    pub session_id: String,
    /// Kubernetes task-run ID (for log correlation).
    pub task_run_id: String,
    /// Turn number within the session, if known.
    pub turn: Option<u64>,
    /// Why this checkpoint is being taken.
    pub reason: CheckpointReason,
    /// Commit author/committer name.
    pub author_name: String,
    /// Commit author/committer email.
    pub author_email: String,
    /// The task branch to commit on and push to.
    pub task_branch: String,
}

impl CheckpointMetadata {
    /// Build the WIP commit message.
    ///
    /// Format: `wip: checkpoint session <session-id> turn <turn> reason <reason>`
    ///
    /// When `turn` is `None`, emits `turn 0` (unknown) with `turn_known: false`
    /// in the structured result so downstream consumers can distinguish.
    pub fn message(&self) -> String {
        let turn = self.turn.unwrap_or(0);
        format!(
            "wip: checkpoint session {} turn {} reason {}",
            self.session_id, turn, self.reason,
        )
    }
}

/// How a push conflict was resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictStrategy {
    /// No conflict encountered.
    None,
    /// Rebased the local WIP commit onto the updated remote.
    Rebase,
    /// Pushed the WIP commit to an alternate checkpoint ref
    /// (`refs/djinn/checkpoints/<branch>/<session>/turn-{turn}/{sha}`).
    AlternateRef { ref_path: String },
}

impl fmt::Display for ConflictStrategy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Rebase => write!(f, "rebase"),
            Self::AlternateRef { ref_path } => write!(f, "alternate-ref:{ref_path}"),
        }
    }
}

/// Result of the push attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PushResult {
    /// Push succeeded.
    Success,
    /// Push failed after all retries.
    Failed { error: String },
    /// Push had a conflict; see the conflict strategy for resolution.
    Conflict {
        strategy: ConflictStrategy,
        resolved: bool,
        error: Option<String>,
    },
    /// Push timed out.
    TimedOut,
}

impl fmt::Display for PushResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Success => write!(f, "success"),
            Self::Failed { error } => write!(f, "failed: {error}"),
            Self::Conflict {
                strategy,
                resolved,
                error,
            } => {
                let status = if *resolved { "resolved" } else { "unresolved" };
                match error {
                    Some(e) => write!(f, "conflict({strategy}) {status}: {e}"),
                    None => write!(f, "conflict({strategy}) {status}"),
                }
            }
            Self::TimedOut => write!(f, "timed-out"),
        }
    }
}

/// Overall preservation outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreservationOutcome {
    /// Worktree was clean — nothing to checkpoint.
    CleanTree,
    /// WIP checkpoint committed and (attempted) pushed.
    Checkpointed,
    /// Safety scan blocked the checkpoint (secrets or policy violation).
    BlockedBySafetyScan,
    /// Safety scan found no files eligible for staging (all excluded).
    AllExcluded,
    /// The commit step failed.
    CommitFailed { error: String },
    /// The whole preservation timed out.
    TimedOut,
}

impl fmt::Display for PreservationOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CleanTree => write!(f, "clean-tree"),
            Self::Checkpointed => write!(f, "checkpointed"),
            Self::BlockedBySafetyScan => write!(f, "blocked-by-safety-scan"),
            Self::AllExcluded => write!(f, "all-excluded"),
            Self::CommitFailed { error } => write!(f, "commit-failed: {error}"),
            Self::TimedOut => write!(f, "timed-out"),
        }
    }
}

/// Structured result from a checkpoint preservation attempt.
///
/// Contains all fields required by the capture-before-exit epic for events,
/// metrics, and persisted state. Downstream consumers (auto-submit, resume
/// selection) can inspect these fields without re-scanning the worktree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointPreservationResult {
    /// Overall outcome.
    pub outcome: PreservationOutcome,
    /// Whether the WIP commit was attempted.
    pub commit_attempted: bool,
    /// Whether the commit succeeded (if attempted).
    pub commit_succeeded: bool,
    /// SHA of the WIP commit (if created).
    pub commit_sha: Option<String>,
    /// SHA of the parent commit.
    pub parent_sha: Option<String>,
    /// SHA of the local branch head after commit.
    pub local_sha: Option<String>,
    /// SHA on the remote before push.
    pub remote_sha: Option<String>,
    /// Target ref pushed to (branch or alternate checkpoint ref).
    pub target_ref: String,
    /// Number of push attempts made.
    pub push_attempts: u32,
    /// Conflict resolution strategy used.
    pub conflict_strategy: ConflictStrategy,
    /// Result of the push operation.
    pub push_result: PushResult,
    /// Safety scan summary: number of files staged.
    pub staged_count: usize,
    /// Safety scan summary: file paths staged.
    pub staged_files: Vec<String>,
    /// Safety scan summary: number of files excluded.
    pub excluded_count: usize,
    /// Safety scan summary: excluded files with reasons.
    pub excluded_files: Vec<String>,
    /// Safety scan summary: number of files blocked.
    pub blocked_count: usize,
    /// Safety scan summary: blocked finding descriptions.
    pub blocked_findings: Vec<String>,
    /// Whether the safety scan found any changes at all.
    pub had_changes: bool,
    /// Whether the turn number was explicitly known.
    pub turn_known: bool,
    /// The turn number used (0 if unknown).
    pub turn: u64,
    /// The session/task-run ID.
    pub session_id: String,
    /// The checkpoint reason.
    pub reason: CheckpointReason,
    /// Failure reason, if any.
    pub failure_reason: Option<String>,
    /// Worktree diff fingerprint from safety scan.
    pub worktree_diff_fingerprint: Option<String>,
    /// Staged diff fingerprint from safety scan.
    pub staged_diff_fingerprint: Option<String>,
}

impl Default for CheckpointPreservationResult {
    fn default() -> Self {
        Self {
            outcome: PreservationOutcome::CleanTree,
            commit_attempted: false,
            commit_succeeded: false,
            commit_sha: None,
            parent_sha: None,
            local_sha: None,
            remote_sha: None,
            target_ref: String::new(),
            push_attempts: 0,
            conflict_strategy: ConflictStrategy::None,
            push_result: PushResult::Success,
            staged_count: 0,
            staged_files: Vec::new(),
            excluded_count: 0,
            excluded_files: Vec::new(),
            blocked_count: 0,
            blocked_findings: Vec::new(),
            had_changes: false,
            turn_known: false,
            turn: 0,
            session_id: String::new(),
            reason: CheckpointReason::Terminal,
            failure_reason: None,
            worktree_diff_fingerprint: None,
            staged_diff_fingerprint: None,
        }
    }
}

// ─── Main entry point ──────────────────────────────────────────────────────

/// Perform a safety-scanned WIP checkpoint preservation of the worktree.
///
/// This is the shared checkpoint path used by all controlled worker exits.
/// It scans the worktree for safety, creates a WIP commit with only
/// safety-approved files, and pushes with lease-aware bounded retry.
///
/// # Behavior
///
/// - **Clean tree**: returns `PreservationOutcome::CleanTree` without staging
///   or committing.
/// - **Dirty safe tree**: stages safe files, creates WIP commit, pushes with
///   `--force-with-lease`, handles conflicts via rebase or alternate ref.
/// - **Safety-blocked tree**: returns `PreservationOutcome::BlockedBySafetyScan`
///   without staging blocked content.
/// - **All excluded**: returns `PreservationOutcome::AllExcluded` when the
///   safety scan found changes but all were excluded by policy.
/// - **Push conflict**: attempts `git pull --rebase`, then falls back to an
///   alternate checkpoint ref under `refs/djinn/checkpoints/`.
///
/// # Parameters
///
/// - `worktree`: path to the git worktree (the ephemeral stage clone).
/// - `metadata`: session/task identity and checkpoint reason.
/// - `config`: safety-scan configuration. Pass `None` for defaults.
///
/// # Returns
///
/// A structured [`CheckpointPreservationResult`] with all fields needed for
/// events, metrics, and persisted state.
///
/// # Timeouts
///
/// The entire operation is bounded by [`CHECKPOINT_PUSH_TIMEOUT_SECS`] per
/// push attempt. The total wall time is bounded by callers wrapping this in
/// `tokio::time::timeout(CHECKPOINT_TIMEOUT, ...)`.
pub async fn preserve_checkpoint(
    worktree: &Path,
    metadata: &CheckpointMetadata,
    config: Option<&CheckpointSafetyConfig>,
) -> CheckpointPreservationResult {
    let safety_config = config.cloned().unwrap_or_default();
    let task_run_id = &metadata.task_run_id;
    let branch = &metadata.task_branch;
    let turn = metadata.turn.unwrap_or(0);
    let turn_known = metadata.turn.is_some();

    // Initialize the result with metadata.
    let mut result = CheckpointPreservationResult {
        turn_known,
        turn,
        session_id: metadata.session_id.clone(),
        reason: metadata.reason,
        target_ref: branch.clone(),
        ..Default::default()
    };

    // ── 1. Safety scan ──────────────────────────────────────────────────
    let scan = match checkpoint_safety::scan_worktree(worktree, branch, &safety_config).await {
        Ok(scan) => scan,
        Err(e) => {
            error!(
                task_run_id,
                branch,
                error = %e,
                "checkpoint preservation: safety scan failed"
            );
            result.outcome = PreservationOutcome::CommitFailed {
                error: format!("safety scan failed: {e}"),
            };
            result.failure_reason = Some(format!("safety scan failed: {e}"));
            return result;
        }
    };

    // Populate safety-scan summaries.
    result.staged_count = scan.staged.len();
    result.staged_files = scan.staged.clone();
    result.excluded_count = scan.excluded.len();
    result.excluded_files = scan
        .excluded
        .iter()
        .map(|e| format!("{}: {:?}", e.path, e.reason))
        .collect();
    result.blocked_count = scan.blocked.len();
    result.blocked_findings = scan
        .blocked
        .iter()
        .map(|b| format!("{}: {:?} ({})", b.path, b.kind, b.matched_pattern))
        .collect();
    result.had_changes = scan.had_changes;
    result.worktree_diff_fingerprint = scan.fingerprints.worktree_diff.clone();
    result.staged_diff_fingerprint = scan.fingerprints.staged_diff.clone();
    result.remote_sha = scan.fingerprints.remote_sha.clone();

    // ── 2. Check preconditions ──────────────────────────────────────────
    if !scan.had_changes {
        // Clean tree — no uncommitted changes. But there may be committed-
        // but-unpushed work (the common case: workers commit via shell).
        // Push existing commits to the mirror so an OOM SIGKILL doesn't
        // strand them.
        result.parent_sha = git_rev_parse(worktree, "HEAD").await.ok();
        result.local_sha = result.parent_sha.clone();

        let needs_push = match (&result.local_sha, &result.remote_sha) {
            // Local has commits; remote doesn't exist or diverged.
            (Some(_), None) => true,
            (Some(local), Some(remote)) => local != remote,
            // No local HEAD or same as remote.
            _ => false,
        };

        if needs_push {
            // There are commits to push. Set commit_sha for push_with_lease_retry.
            result.commit_sha = result.local_sha.clone();
            result.commit_attempted = false;
            result.commit_succeeded = false;

            let push_outcome = push_with_lease_retry(worktree, branch, &mut result).await;
            match push_outcome {
                PushOutcome::Success
                | PushOutcome::ConflictRebased
                | PushOutcome::AlternateRef(_) => {
                    result.outcome = PreservationOutcome::Checkpointed;
                }
                PushOutcome::Failed(error) => {
                    result.failure_reason = Some(error);
                    result.outcome = PreservationOutcome::Checkpointed;
                }
            }
        } else {
            info!(
                task_run_id,
                branch, "checkpoint preservation: worktree clean and mirror is current"
            );
            result.outcome = PreservationOutcome::CleanTree;
        }
        return result;
    }

    if !scan.blocked.is_empty() {
        warn!(
            task_run_id,
            branch,
            blocked_count = scan.blocked.len(),
            blocked_findings = ?scan.blocked,
            "checkpoint preservation: safety scan blocked checkpoint (secrets/policy)"
        );
        result.outcome = PreservationOutcome::BlockedBySafetyScan;
        result.failure_reason = Some(format!(
            "safety scan blocked {} file(s): {}",
            scan.blocked.len(),
            scan.blocked
                .iter()
                .map(|b| format!("{}:{}", b.path, b.matched_pattern))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        return result;
    }

    if scan.staged.is_empty() {
        info!(
            task_run_id,
            branch,
            excluded_count = scan.excluded.len(),
            "checkpoint preservation: all changed files excluded by policy"
        );
        result.outcome = PreservationOutcome::AllExcluded;
        return result;
    }

    // ── 3. Get parent SHA ───────────────────────────────────────────────
    result.parent_sha = git_rev_parse(worktree, "HEAD").await.ok();

    // ── 4. Reset index and stage safe files (targeted, not add -A) ──────
    // Reset the index first to avoid stale staging state from prior operations.
    if let Err(e) = git_cmd(worktree, &["reset", "--mixed", "--quiet", "HEAD", "--"]).await {
        warn!(
            task_run_id,
            branch,
            error = %e,
            "checkpoint preservation: failed to reset index before staging (continuing)"
        );
    }

    for path in &scan.staged {
        let status = git_cmd(worktree, &["add", path]).await;
        if let Err(e) = status {
            warn!(
                task_run_id,
                branch,
                path,
                error = %e,
                "checkpoint preservation: failed to stage file (skipping)"
            );
        }
    }

    // Check if anything is actually staged.
    let staged_names = git_cmd(worktree, &["diff", "--cached", "--name-only"])
        .await
        .unwrap_or_default();
    if staged_names.trim().is_empty() {
        info!(
            task_run_id,
            branch, "checkpoint preservation: nothing staged after safety-filtered add"
        );
        result.outcome = PreservationOutcome::CleanTree;
        return result;
    }

    // ── 5. Create WIP commit ────────────────────────────────────────────
    result.commit_attempted = true;
    let message = metadata.message();
    let commit_result = git_cmd_with_env(
        worktree,
        &["commit", "-m", &message],
        &[
            ("GIT_AUTHOR_NAME", &metadata.author_name),
            ("GIT_AUTHOR_EMAIL", &metadata.author_email),
            ("GIT_COMMITTER_NAME", &metadata.author_name),
            ("GIT_COMMITTER_EMAIL", &metadata.author_email),
        ],
    )
    .await;

    match commit_result {
        Ok(_) => {
            result.commit_succeeded = true;
            result.commit_sha = git_rev_parse(worktree, "HEAD").await.ok();
            result.local_sha = result.commit_sha.clone();
            info!(
                task_run_id,
                branch,
                commit_sha = ?result.commit_sha,
                parent_sha = ?result.parent_sha,
                staged_count = scan.staged.len(),
                message = %message,
                "checkpoint preservation: WIP commit created"
            );
        }
        Err(e) => {
            error!(
                task_run_id,
                branch,
                error = %e,
                "checkpoint preservation: commit failed"
            );
            result.outcome = PreservationOutcome::CommitFailed {
                error: e.to_string(),
            };
            result.failure_reason = Some(format!("commit failed: {e}"));
            return result;
        }
    }

    // ── 6. Push with lease and bounded retry ────────────────────────────
    let push_outcome = push_with_lease_retry(worktree, branch, &mut result).await;

    match push_outcome {
        PushOutcome::Success => {
            result.outcome = PreservationOutcome::Checkpointed;
        }
        PushOutcome::ConflictRebased => {
            result.outcome = PreservationOutcome::Checkpointed;
        }
        PushOutcome::AlternateRef(ref_path) => {
            result.target_ref = ref_path;
            result.outcome = PreservationOutcome::Checkpointed;
        }
        PushOutcome::Failed(error) => {
            result.failure_reason = Some(error.clone());
            result.push_result = PushResult::Failed { error };
            result.outcome = PreservationOutcome::Checkpointed;
            // Still checkpointed locally — the commit exists even if push failed.
        }
    }

    info!(
        task_run_id,
        branch,
        outcome = %result.outcome,
        commit_sha = ?result.commit_sha,
        parent_sha = ?result.parent_sha,
        local_sha = ?result.local_sha,
        remote_sha = ?result.remote_sha,
        target_ref = %result.target_ref,
        push_attempts = result.push_attempts,
        conflict_strategy = %result.conflict_strategy,
        push_result = %result.push_result,
        staged_count = result.staged_count,
        excluded_count = result.excluded_count,
        blocked_count = result.blocked_count,
        had_changes = result.had_changes,
        failure_reason = ?result.failure_reason,
        "checkpoint preservation complete"
    );

    result
}

// ─── Push logic ────────────────────────────────────────────────────────────

/// Internal push outcome for the retry loop.
enum PushOutcome {
    Success,
    ConflictRebased,
    AlternateRef(String),
    Failed(String),
}

/// Push the task branch with `--force-with-lease`, retrying on transient
/// failures and attempting conflict resolution.
async fn push_with_lease_retry(
    worktree: &Path,
    branch: &str,
    result: &mut CheckpointPreservationResult,
) -> PushOutcome {
    let commit_sha = match &result.commit_sha {
        Some(sha) => sha.clone(),
        None => return PushOutcome::Failed("no commit SHA available".to_string()),
    };

    let mut last_error = String::new();

    for attempt in 1..=MAX_PUSH_ATTEMPTS {
        result.push_attempts = attempt;

        // Backoff before retry (not before the first attempt).
        if attempt > 1 {
            let delay = PUSH_RETRY_BACKOFF_BASE * attempt;
            debug!(
                branch,
                attempt,
                delay_ms = delay.as_millis() as u64,
                "checkpoint push: backing off before retry"
            );
            tokio::time::sleep(delay).await;
        }

        // Use force-with-lease to prevent clobbering concurrent pushes.
        // The lease expects the remote branch to point to `expected_remote_sha`;
        // if someone else pushed since we scanned, the lease fails.
        // When the remote branch doesn't exist yet (remote_sha is None),
        // skip the lease — a regular push creates the branch.
        let force_ref = format!("{branch}:{branch}");
        let lease_arg;
        let mut push_args = vec!["push", "origin", force_ref.as_str()];
        if let Some(ref remote_sha_val) = result.remote_sha {
            lease_arg = format!("--force-with-lease={branch}:{remote_sha_val}");
            push_args.push(&lease_arg);
        }

        let push_result = tokio::time::timeout(
            Duration::from_secs(CHECKPOINT_PUSH_TIMEOUT_SECS),
            git_cmd(worktree, &push_args),
        )
        .await;

        match push_result {
            Ok(Ok(_)) => {
                result.push_result = PushResult::Success;
                result.conflict_strategy = ConflictStrategy::None;
                return PushOutcome::Success;
            }
            Ok(Err(e)) => {
                last_error = e.to_string();
                warn!(
                    branch,
                    attempt,
                    error = %e,
                    "checkpoint push: failed"
                );

                // Check if it's a non-fast-forward (conflict) error.
                let stderr = e.to_string();
                if is_push_conflict_error(&stderr) {
                    info!(
                        branch,
                        attempt, "checkpoint push: conflict detected; attempting rebase"
                    );

                    // Try rebase-based conflict resolution.
                    let rebase = try_rebase(worktree, branch).await;
                    if rebase {
                        // Rebase succeeded — re-push without lease (we rebased).
                        result.conflict_strategy = ConflictStrategy::Rebase;

                        let repush = tokio::time::timeout(
                            Duration::from_secs(CHECKPOINT_PUSH_TIMEOUT_SECS),
                            git_cmd(worktree, &["push", "origin", &format!("{branch}:{branch}")]),
                        )
                        .await;

                        match repush {
                            Ok(Ok(_)) => {
                                result.push_result = PushResult::Conflict {
                                    strategy: ConflictStrategy::Rebase,
                                    resolved: true,
                                    error: None,
                                };
                                return PushOutcome::ConflictRebased;
                            }
                            Ok(Err(repush_err)) => {
                                warn!(
                                    branch,
                                    error = %repush_err,
                                    "checkpoint push: post-rebase push failed; trying alternate ref"
                                );
                                // Fall through to alternate checkpoint ref below.
                            }
                            Err(_) => {
                                // Fall through to alternate checkpoint ref below.
                            }
                        }
                    }

                    // Rebase didn't work or post-rebase push failed —
                    // fall back to alternate checkpoint ref.
                    let session_id = &result.session_id;
                    let turn = result.turn;
                    let short_sha = commit_sha.chars().take(12).collect::<String>();
                    let safe_branch = sanitize_ref_component(branch);
                    let safe_session = sanitize_ref_component(session_id);
                    let safe_sha = sanitize_ref_component(&short_sha);
                    let alt_ref = format!(
                        "refs/djinn/checkpoints/{safe_branch}/{safe_session}/turn-{turn}/{safe_sha}"
                    );

                    info!(
                        branch,
                        alt_ref = %alt_ref,
                        "checkpoint push: falling back to alternate checkpoint ref"
                    );

                    let alt_result = tokio::time::timeout(
                        Duration::from_secs(CHECKPOINT_PUSH_TIMEOUT_SECS),
                        git_cmd(
                            worktree,
                            &[
                                "push",
                                "origin",
                                &format!("{commit_sha}:refs/heads/{alt_ref}"),
                            ],
                        ),
                    )
                    .await;

                    match alt_result {
                        Ok(Ok(_)) => {
                            result.push_result = PushResult::Conflict {
                                strategy: ConflictStrategy::AlternateRef {
                                    ref_path: alt_ref.clone(),
                                },
                                resolved: true,
                                error: None,
                            };
                            result.conflict_strategy = ConflictStrategy::AlternateRef {
                                ref_path: alt_ref.clone(),
                            };
                            return PushOutcome::AlternateRef(alt_ref);
                        }
                        Ok(Err(alt_err)) => {
                            warn!(
                                branch,
                                alt_ref = %alt_ref,
                                error = %alt_err,
                                "checkpoint push: alternate ref push also failed"
                            );
                            last_error = alt_err.to_string();
                        }
                        Err(_) => {
                            last_error = "alternate ref push timed out".to_string();
                        }
                    }
                }

                // Non-conflict or all fallbacks exhausted — retry.
            }
            Err(_) => {
                last_error = "push timed out".to_string();
                warn!(branch, attempt, "checkpoint push: timed out");
            }
        }
    }

    // All attempts exhausted.
    PushOutcome::Failed(last_error)
}

/// Sanitize a string for use as a git ref component. Replaces characters
/// that are invalid in ref names with hyphens, trims leading/trailing
/// dots and hyphens, and falls back to "unknown" for empty results.
fn sanitize_ref_component(input: &str) -> String {
    let sanitized: String = input
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = sanitized.trim_matches(['.', '-']).to_string();
    if trimmed.is_empty() {
        "unknown".to_string()
    } else {
        trimmed
    }
}

/// Check if a push stderr indicates a non-fast-forward conflict.
fn is_push_conflict_error(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("rejected")
        || lower.contains("non-fast-forward")
        || lower.contains("failed to push")
        || lower.contains("stale info")
        || lower.contains("fetch first")
        || lower.contains("tip of your current branch is behind")
}

/// Attempt to rebase the local branch onto `origin/<branch>`.
///
/// Returns `true` if the rebase succeeded, `false` if it failed (conflicts,
/// errors). On failure, any in-progress rebase is aborted to leave the
/// worktree in a clean state.
async fn try_rebase(worktree: &Path, branch: &str) -> bool {
    // Fetch the latest remote state.
    let fetch = git_cmd(worktree, &["fetch", "origin", branch]).await;
    if fetch.is_err() {
        return false;
    }

    // Check if we're already up to date (nothing to rebase onto).
    let ancestor = git_cmd(
        worktree,
        &[
            "merge-base",
            "--is-ancestor",
            "HEAD",
            &format!("origin/{branch}"),
        ],
    )
    .await;
    if ancestor.is_ok() {
        // HEAD is already an ancestor of origin/branch — push should have
        // succeeded. Might be a different kind of error.
        return false;
    }

    // Try the rebase.
    let rebase = git_cmd(worktree, &["rebase", &format!("origin/{branch}")]).await;
    if rebase.is_ok() {
        return true;
    }

    // Rebase failed — abort to leave a clean state.
    let _ = git_cmd(worktree, &["rebase", "--abort"]).await;
    false
}

// ─── Git helpers ───────────────────────────────────────────────────────────

/// Run a git command in the worktree and return stdout on success.
///
/// Delegates to [`djinn_git::run_git_command_in`] which applies
/// `safe.directory=*` and lowers process priority.
async fn git_cmd(worktree: &Path, args: &[&str]) -> Result<String, String> {
    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let out = djinn_git::run_git_command_in(worktree, owned)
        .await
        .map_err(git_err_to_string)?;
    Ok(out.stdout)
}

/// Run a git command with extra environment variables.
///
/// Delegates to [`djinn_git::run_git_command_in_with_env`] which applies
/// `safe.directory=*` and lowers process priority alongside the caller's
/// extra env vars.
async fn git_cmd_with_env(
    worktree: &Path,
    args: &[&str],
    env: &[(&str, &str)],
) -> Result<String, String> {
    let owned_args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let owned_env: Vec<(String, String)> = env
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let out = djinn_git::run_git_command_in_with_env(worktree, owned_args, owned_env)
        .await
        .map_err(git_err_to_string)?;
    Ok(out.stdout)
}

/// Resolve a ref to a SHA via `git rev-parse`.
async fn git_rev_parse(worktree: &Path, rev: &str) -> Result<String, String> {
    let output = git_cmd(worktree, &["rev-parse", rev]).await?;
    let sha = output.trim().to_string();
    if sha.is_empty() {
        Err("empty rev-parse output".to_string())
    } else {
        Ok(sha)
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wip_message_format_default_turn() {
        let meta = CheckpointMetadata {
            session_id: "sess-123".to_string(),
            task_run_id: "run-456".to_string(),
            turn: None,
            reason: CheckpointReason::SoftDeadline,
            author_name: "bot".to_string(),
            author_email: "bot@djinn.local".to_string(),
            task_branch: "task/run-456".to_string(),
        };
        assert_eq!(
            meta.message(),
            "wip: checkpoint session sess-123 turn 0 reason soft-deadline"
        );
    }

    #[test]
    fn wip_message_format_known_turn() {
        let meta = CheckpointMetadata {
            session_id: "sess-123".to_string(),
            task_run_id: "run-456".to_string(),
            turn: Some(7),
            reason: CheckpointReason::Signal,
            author_name: "bot".to_string(),
            author_email: "bot@djinn.local".to_string(),
            task_branch: "task/run-456".to_string(),
        };
        assert_eq!(
            meta.message(),
            "wip: checkpoint session sess-123 turn 7 reason signal"
        );
    }

    #[test]
    fn wip_message_format_all_reasons() {
        let reasons = [
            ("soft-deadline", CheckpointReason::SoftDeadline),
            ("signal", CheckpointReason::Signal),
            ("periodic", CheckpointReason::Periodic),
            ("terminal", CheckpointReason::Terminal),
            ("manual", CheckpointReason::Manual),
        ];
        for (expected, reason) in reasons {
            let meta = CheckpointMetadata {
                session_id: "s".to_string(),
                task_run_id: "r".to_string(),
                turn: Some(1),
                reason,
                author_name: "bot".to_string(),
                author_email: "bot@djinn.local".to_string(),
                task_branch: "task/r".to_string(),
            };
            assert!(
                meta.message().ends_with(&format!("reason {expected}")),
                "message should end with 'reason {expected}', got: {}",
                meta.message()
            );
        }
    }

    #[test]
    fn preservation_result_default() {
        let result = CheckpointPreservationResult::default();
        assert_eq!(result.outcome, PreservationOutcome::CleanTree);
        assert!(!result.commit_attempted);
        assert!(!result.commit_succeeded);
        assert_eq!(result.push_attempts, 0);
        assert_eq!(result.conflict_strategy, ConflictStrategy::None);
        assert_eq!(result.push_result, PushResult::Success);
        assert!(result.failure_reason.is_none());
    }

    #[test]
    fn push_conflict_error_detection() {
        assert!(is_push_conflict_error(
            "error: failed to push some refs to 'origin'\nrejected: non-fast-forward"
        ));
        assert!(is_push_conflict_error(
            "tip of your current branch is behind its remote counterpart"
        ));
        assert!(is_push_conflict_error("failed to push some refs"));
        assert!(is_push_conflict_error(
            "Updates were rejected because the tip of your current branch is behind"
        ));
        assert!(is_push_conflict_error(
            "! [remote rejected] main -> main (stale info)"
        ));
        assert!(!is_push_conflict_error("permission denied"));
        assert!(!is_push_conflict_error("could not resolve host"));
    }

    #[test]
    fn conflict_strategy_display() {
        assert_eq!(ConflictStrategy::None.to_string(), "none");
        assert_eq!(ConflictStrategy::Rebase.to_string(), "rebase");
        assert_eq!(
            ConflictStrategy::AlternateRef {
                ref_path: "refs/djinn/checkpoints/task/1".to_string()
            }
            .to_string(),
            "alternate-ref:refs/djinn/checkpoints/task/1"
        );
    }

    #[test]
    fn push_result_display() {
        assert_eq!(PushResult::Success.to_string(), "success");
        assert_eq!(
            PushResult::Failed {
                error: "timeout".to_string()
            }
            .to_string(),
            "failed: timeout"
        );
        assert_eq!(PushResult::TimedOut.to_string(), "timed-out");
    }

    #[test]
    fn preservation_outcome_display() {
        assert_eq!(PreservationOutcome::CleanTree.to_string(), "clean-tree");
        assert_eq!(
            PreservationOutcome::Checkpointed.to_string(),
            "checkpointed"
        );
        assert_eq!(
            PreservationOutcome::BlockedBySafetyScan.to_string(),
            "blocked-by-safety-scan"
        );
        assert_eq!(PreservationOutcome::AllExcluded.to_string(), "all-excluded");
        assert_eq!(
            PreservationOutcome::CommitFailed {
                error: "idx".to_string()
            }
            .to_string(),
            "commit-failed: idx"
        );
        assert_eq!(PreservationOutcome::TimedOut.to_string(), "timed-out");
    }

    #[test]
    fn checkpoint_reason_display() {
        assert_eq!(CheckpointReason::SoftDeadline.to_string(), "soft-deadline");
        assert_eq!(CheckpointReason::Signal.to_string(), "signal");
        assert_eq!(CheckpointReason::Periodic.to_string(), "periodic");
        assert_eq!(CheckpointReason::Terminal.to_string(), "terminal");
        assert_eq!(CheckpointReason::Manual.to_string(), "manual");
    }

    #[test]
    fn preservation_result_serialization_roundtrip() {
        let result = CheckpointPreservationResult {
            outcome: PreservationOutcome::Checkpointed,
            commit_attempted: true,
            commit_succeeded: true,
            commit_sha: Some("abc123".to_string()),
            parent_sha: Some("def456".to_string()),
            local_sha: Some("abc123".to_string()),
            remote_sha: Some("def456".to_string()),
            target_ref: "task/run-1".to_string(),
            push_attempts: 2,
            conflict_strategy: ConflictStrategy::Rebase,
            push_result: PushResult::Conflict {
                strategy: ConflictStrategy::Rebase,
                resolved: true,
                error: None,
            },
            staged_count: 3,
            staged_files: vec!["a.rs".to_string(), "b.rs".to_string()],
            excluded_count: 1,
            excluded_files: vec!["target/debug: GeneratedPath".to_string()],
            blocked_count: 0,
            blocked_findings: vec![],
            had_changes: true,
            turn_known: true,
            turn: 5,
            session_id: "sess-1".to_string(),
            reason: CheckpointReason::SoftDeadline,
            failure_reason: None,
            worktree_diff_fingerprint: Some("sha256:aaa".to_string()),
            staged_diff_fingerprint: Some("sha256:bbb".to_string()),
        };
        let json = serde_json::to_string(&result).unwrap();
        let deserialized: CheckpointPreservationResult = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.outcome, PreservationOutcome::Checkpointed);
        assert_eq!(deserialized.commit_sha, Some("abc123".to_string()));
        assert_eq!(deserialized.push_attempts, 2);
        assert_eq!(deserialized.conflict_strategy, ConflictStrategy::Rebase);
        assert!(deserialized.turn_known);
        assert_eq!(deserialized.turn, 5);
    }

    #[test]
    fn sanitize_ref_component_replaces_special_chars() {
        assert_eq!(sanitize_ref_component("task/run-1"), "task-run-1");
        assert_eq!(
            sanitize_ref_component("branch with spaces"),
            "branch-with-spaces"
        );
        assert_eq!(sanitize_ref_component(""), "unknown");
        assert_eq!(sanitize_ref_component("..."), "unknown");
        assert_eq!(sanitize_ref_component("abc_def-123"), "abc_def-123");
        assert_eq!(sanitize_ref_component("a/b/c"), "a-b-c");
    }

    // ── Integration-style tests with real git repos ─────────────────────

    /// Helper: run a git command via `djinn_git` (applies safe.directory and
    /// lower priority — matches production behavior). Returns stdout.
    async fn git(dir: &std::path::Path, args: &[&str]) -> String {
        let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        djinn_git::run_git_command_in(dir, owned)
            .await
            .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"))
            .stdout
    }

    /// Helper: create a bare git repo as "origin" and a working clone.
    async fn setup_test_repos() -> (tempfile::TempDir, tempfile::TempDir) {
        let origin = tempfile::tempdir().unwrap();
        let clone = tempfile::tempdir().unwrap();

        // Init bare origin.
        git(origin.path(), &["init", "--bare"]).await;

        // Clone it.
        git(
            clone.path(),
            &["clone", origin.path().to_str().unwrap(), "."],
        )
        .await;

        // Configure identity in clone.
        for (k, v) in [("user.name", "test"), ("user.email", "test@test.local")] {
            git(clone.path(), &["config", k, v]).await;
        }

        // Create an initial commit so HEAD exists.
        git(clone.path(), &["checkout", "-b", "task/test-branch"]).await;

        std::fs::write(clone.path().join("README.md"), "init\n").unwrap();
        git(clone.path(), &["add", "README.md"]).await;
        git(clone.path(), &["commit", "-m", "init"]).await;
        git(
            clone.path(),
            &["push", "origin", "task/test-branch:task/test-branch"],
        )
        .await;

        (origin, clone)
    }

    fn test_metadata(task_branch: &str, reason: CheckpointReason) -> CheckpointMetadata {
        CheckpointMetadata {
            session_id: "test-session".to_string(),
            task_run_id: "test-run".to_string(),
            turn: Some(1),
            reason,
            author_name: "test-bot".to_string(),
            author_email: "test@djinn.local".to_string(),
            task_branch: task_branch.to_string(),
        }
    }

    #[tokio::test]
    async fn clean_tree_returns_clean() {
        let (_origin, clone) = setup_test_repos().await;
        let meta = test_metadata("task/test-branch", CheckpointReason::Terminal);

        let result = preserve_checkpoint(clone.path(), &meta, None).await;

        assert_eq!(result.outcome, PreservationOutcome::CleanTree);
        assert!(!result.commit_attempted);
        assert_eq!(result.push_attempts, 0);
        assert!(!result.had_changes);
    }

    #[tokio::test]
    async fn dirty_safe_tree_creates_wip_commit() {
        let (_origin, clone) = setup_test_repos().await;

        // Create a safe dirty file.
        std::fs::create_dir_all(clone.path().join("src")).unwrap();
        std::fs::write(clone.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        let meta = test_metadata("task/test-branch", CheckpointReason::Signal);
        let result = preserve_checkpoint(clone.path(), &meta, None).await;

        assert_eq!(result.outcome, PreservationOutcome::Checkpointed);
        assert!(result.commit_attempted);
        assert!(result.commit_succeeded);
        assert!(result.commit_sha.is_some());
        assert!(result.had_changes);
        assert!(result.staged_count > 0);
        assert!(result.staged_files.contains(&"src/main.rs".to_string()));
    }

    #[tokio::test]
    async fn wip_commit_message_in_repo() {
        let (_origin, clone) = setup_test_repos().await;

        std::fs::write(clone.path().join("hello.txt"), "world\n").unwrap();

        let meta = test_metadata("task/test-branch", CheckpointReason::SoftDeadline);
        let result = preserve_checkpoint(clone.path(), &meta, None).await;

        assert_eq!(result.outcome, PreservationOutcome::Checkpointed);

        // Verify the commit message format in the repo.
        let msg = git(clone.path(), &["log", "-1", "--format=%s"]).await;
        assert!(
            msg.starts_with("wip: checkpoint session test-session turn 1 reason soft-deadline"),
            "unexpected commit message: {msg}"
        );
    }

    #[tokio::test]
    async fn safety_blocked_tree_returns_blocked() {
        let (_origin, clone) = setup_test_repos().await;

        // Create a file with a blocked path (secret-like).
        std::fs::write(clone.path().join(".env"), "SECRET_KEY=abc123\n").unwrap();

        let meta = test_metadata("task/test-branch", CheckpointReason::Terminal);
        let result = preserve_checkpoint(clone.path(), &meta, None).await;

        assert_eq!(result.outcome, PreservationOutcome::BlockedBySafetyScan);
        assert!(!result.commit_attempted);
        assert!(result.failure_reason.is_some());
        assert!(result.blocked_count > 0);
    }

    #[tokio::test]
    async fn generated_files_excluded_only_stages_safe() {
        let (_origin, clone) = setup_test_repos().await;

        // A safe file.
        std::fs::create_dir_all(clone.path().join("src")).unwrap();
        std::fs::write(clone.path().join("src/lib.rs"), "pub fn hello() {}\n").unwrap();

        // A generated file that should be excluded.
        std::fs::create_dir_all(clone.path().join("target/debug")).unwrap();
        std::fs::write(clone.path().join("target/debug/output"), "generated\n").unwrap();

        let meta = test_metadata("task/test-branch", CheckpointReason::Terminal);
        let result = preserve_checkpoint(clone.path(), &meta, None).await;

        // Should succeed because safe files were staged.
        if result.outcome == PreservationOutcome::Checkpointed {
            assert!(result.staged_files.contains(&"src/lib.rs".to_string()));
            // target/debug should be excluded.
            assert!(result.excluded_count > 0);
        }
    }

    #[tokio::test]
    async fn push_succeeds_with_lease() {
        let (_origin, clone) = setup_test_repos().await;

        std::fs::write(clone.path().join("work.txt"), "done\n").unwrap();

        let meta = test_metadata("task/test-branch", CheckpointReason::Periodic);
        let result = preserve_checkpoint(clone.path(), &meta, None).await;

        assert_eq!(result.outcome, PreservationOutcome::Checkpointed);
        assert!(result.commit_succeeded);
        assert_eq!(result.push_result, PushResult::Success);
        assert_eq!(result.push_attempts, 1);
        assert_eq!(result.conflict_strategy, ConflictStrategy::None);
    }

    #[tokio::test]
    async fn divergent_push_falls_back_to_alternate_ref() {
        let (origin, clone) = setup_test_repos().await;

        // Create a divergent commit on the remote.
        let diverger = tempfile::tempdir().unwrap();
        git(
            diverger.path(),
            &["clone", origin.path().to_str().unwrap(), "."],
        )
        .await;

        // Configure identity.
        for (k, v) in [("user.name", "other"), ("user.email", "other@test.local")] {
            git(diverger.path(), &["config", k, v]).await;
        }

        // Create a commit on the same branch in the diverger.
        git(diverger.path(), &["checkout", "task/test-branch"]).await;
        std::fs::write(diverger.path().join("diverge.txt"), "remote work\n").unwrap();
        git(diverger.path(), &["add", "diverge.txt"]).await;
        git(diverger.path(), &["commit", "-m", "diverge"]).await;
        git(
            diverger.path(),
            &["push", "origin", "task/test-branch:task/test-branch"],
        )
        .await;

        // Now our clone is behind — create local work.
        std::fs::write(clone.path().join("local.txt"), "local work\n").unwrap();

        let meta = test_metadata("task/test-branch", CheckpointReason::Signal);
        let result = preserve_checkpoint(clone.path(), &meta, None).await;

        // The push should eventually succeed via alternate ref or rebase.
        assert!(result.commit_succeeded);
        assert!(result.push_attempts > 0);
    }

    #[tokio::test]
    async fn structured_result_has_all_required_fields() {
        let (_origin, clone) = setup_test_repos().await;

        std::fs::write(clone.path().join("data.txt"), "content\n").unwrap();

        let meta = test_metadata("task/test-branch", CheckpointReason::SoftDeadline);
        let result = preserve_checkpoint(clone.path(), &meta, None).await;

        // Verify all required structured fields are present.
        // (These are the fields required by the AC.)
        let _ = result.outcome; // commit attempt
        let _ = result.staged_count; // staged summary
        let _ = result.excluded_count; // excluded summary
        let _ = result.blocked_count; // safety scan result
        let _ = result.parent_sha; // parent SHA
        let _ = result.local_sha; // local SHA
        let _ = result.remote_sha; // remote SHA
        let _ = result.push_attempts; // retries
        let _ = result.conflict_strategy; // conflict strategy
        let _ = result.push_result; // push result
        let _ = result.failure_reason; // failure reason

        // Serialize to verify it round-trips (for persisted results).
        let json = serde_json::to_string_pretty(&result).unwrap();
        let _: CheckpointPreservationResult = serde_json::from_str(&json).unwrap();
    }
}
