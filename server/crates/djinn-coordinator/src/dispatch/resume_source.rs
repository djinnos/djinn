//! Pure resume-source selection for re-dispatch.
//!
//! This module intentionally performs no checkout, GitHub, Kubernetes, or
//! database work. Callers translate durable lifecycle/review/checkpoint rows
//! into [`ResumeSourceCandidate`] values, then use [`select_resume_source`] to
//! choose the safest source in a deterministic precedence order.
//!
use serde::{Deserialize, Serialize};

use crate::{
    AutoSubmitLifecycleMetadata, AutoSubmitSkipReason, CheckpointLifecycleMetadata,
    CheckpointSafetyScanMetadata, ResumeLifecycleConfig, ResumeLifecycleMetadata,
    ResumeSelectionReason, WorkerLifecycleMetadata,
};

/// Resume source classes, ordered by [`source_precedence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeSourceKind {
    /// Accepted auto-submit/review state from the normal submission path.
    AutoSubmit,
    /// A safety-scanned checkpoint commit on the task branch.
    TaskBranchCheckpoint,
    /// A safety-scanned checkpoint commit on an alternate checkpoint ref.
    AlternateCheckpointRef,
    /// Clean task branch fallback when no prior output can be resumed safely.
    CleanTaskBranch,
}

/// Build pure selector candidates from passive lifecycle metadata.
///
/// The coordinator uses this seam before spawning a replacement worker. It is
/// deliberately side-effect free: all git/ref/safety facts must already be in
/// `lifecycle` and its `extra` maps. A clean task-branch candidate is always
/// appended so enabled resume selection degrades to an explicit, machine-readable
/// fallback instead of panicking or silently resuming unsafe output.
pub fn build_resume_source_candidates(
    task_id: &str,
    target_task_ref: &str,
    prior_session_lineage: Option<&str>,
    lifecycle: &WorkerLifecycleMetadata,
) -> Vec<ResumeSourceCandidate> {
    let mut candidates = Vec::new();

    if let Some(auto_submit) = lifecycle.auto_submit.as_ref() {
        candidates.push(auto_submit_candidate(
            task_id,
            target_task_ref,
            prior_session_lineage,
            auto_submit,
        ));
    }

    if let Some(checkpoint) = lifecycle.checkpoint.as_ref() {
        candidates.push(checkpoint_candidate(
            ResumeSourceKind::TaskBranchCheckpoint,
            task_id,
            target_task_ref,
            prior_session_lineage,
            checkpoint,
            None,
            None,
        ));

        if let Some(alternate_ref) = string_extra(&checkpoint.extra, "alternate_checkpoint_ref")
            .or_else(|| string_extra(&checkpoint.extra, "alternate_ref_name"))
        {
            let alternate_sha = string_extra(&checkpoint.extra, "alternate_checkpoint_sha")
                .or_else(|| string_extra(&checkpoint.extra, "alternate_commit_sha"));
            candidates.push(checkpoint_candidate(
                ResumeSourceKind::AlternateCheckpointRef,
                task_id,
                &alternate_ref,
                prior_session_lineage,
                checkpoint,
                Some(alternate_ref.clone()),
                alternate_sha,
            ));
        }
    }

    candidates.push(ResumeSourceCandidate::clean_task_branch(
        task_id,
        target_task_ref,
    ));
    candidates
}

fn auto_submit_candidate(
    task_id: &str,
    target_task_ref: &str,
    prior_session_lineage: Option<&str>,
    metadata: &AutoSubmitLifecycleMetadata,
) -> ResumeSourceCandidate {
    let submit_or_review_id = metadata
        .submission_id
        .clone()
        .or_else(|| string_extra(&metadata.extra, "review_id"))
        .or_else(|| string_extra(&metadata.extra, "submit_id"));
    let state = if submit_or_review_id.is_some() && metadata.skipped_reason.is_none() {
        AutoSubmitResumeState::Accepted
    } else if matches!(
        metadata.skipped_reason,
        Some(AutoSubmitSkipReason::ReviewRequired)
    ) {
        AutoSubmitResumeState::NotAccepted
    } else if metadata.skipped_reason.is_some() {
        AutoSubmitResumeState::Blocked
    } else {
        AutoSubmitResumeState::NotAccepted
    };

    ResumeSourceCandidate {
        kind: ResumeSourceKind::AutoSubmit,
        task_id: string_extra(&metadata.extra, "task_id").unwrap_or_else(|| task_id.to_owned()),
        session_lineage: lineage_from_extra(&metadata.extra, prior_session_lineage),
        checkpoint_sha: None,
        submit_or_review_id,
        target_ref: string_extra(&metadata.extra, "target_ref")
            .unwrap_or_else(|| target_task_ref.to_owned()),
        auto_submit_state: Some(state),
        checkpoint_safety: None,
        age_secs: u64_extra(&metadata.extra, "age_secs"),
    }
}

fn checkpoint_candidate(
    kind: ResumeSourceKind,
    task_id: &str,
    target_ref: &str,
    prior_session_lineage: Option<&str>,
    metadata: &CheckpointLifecycleMetadata,
    target_override: Option<String>,
    sha_override: Option<String>,
) -> ResumeSourceCandidate {
    ResumeSourceCandidate {
        kind,
        task_id: string_extra(&metadata.extra, "task_id").unwrap_or_else(|| task_id.to_owned()),
        session_lineage: lineage_from_extra(&metadata.extra, prior_session_lineage),
        checkpoint_sha: sha_override.or_else(|| metadata.commit_sha.clone()),
        submit_or_review_id: None,
        target_ref: target_override
            .or_else(|| metadata.ref_name.clone())
            .unwrap_or_else(|| target_ref.to_owned()),
        auto_submit_state: None,
        checkpoint_safety: Some(safety_verdict(metadata.safety_scan.as_ref())),
        age_secs: u64_extra(&metadata.extra, "age_secs"),
    }
}

fn safety_verdict(scan: Option<&CheckpointSafetyScanMetadata>) -> CheckpointSafetyVerdict {
    match scan {
        Some(scan) if scan.passed => CheckpointSafetyVerdict::Safe,
        Some(scan) => CheckpointSafetyVerdict::Unsafe {
            findings: scan.findings.clone(),
        },
        None => CheckpointSafetyVerdict::Missing,
    }
}

fn lineage_from_extra(
    extra: &serde_json::Map<String, serde_json::Value>,
    fallback: Option<&str>,
) -> Option<String> {
    string_extra(extra, "session_lineage")
        .or_else(|| string_extra(extra, "session_id"))
        .or_else(|| fallback.map(str::to_owned))
}

fn string_extra(extra: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    extra.get(key).and_then(|value| match value {
        serde_json::Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    })
}

fn u64_extra(extra: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<u64> {
    extra.get(key).and_then(serde_json::Value::as_u64)
}

impl ResumeSourceKind {
    fn precedence(self) -> u8 {
        match self {
            Self::AutoSubmit => 0,
            Self::TaskBranchCheckpoint => 1,
            Self::AlternateCheckpointRef => 2,
            Self::CleanTaskBranch => 3,
        }
    }

    fn selection_reason(self) -> ResumeSelectionReason {
        match self {
            Self::AutoSubmit => ResumeSelectionReason::AutoSubmitAccepted,
            Self::TaskBranchCheckpoint => ResumeSelectionReason::LatestSafeCheckpoint,
            Self::AlternateCheckpointRef => ResumeSelectionReason::AlternateCheckpointRef,
            Self::CleanTaskBranch => ResumeSelectionReason::CleanTaskBranchFallback,
        }
    }
}

fn source_precedence(candidate: &ResumeSourceCandidate) -> u8 {
    candidate.kind.precedence()
}

/// Accepted/blocked state for auto-submit candidates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoSubmitResumeState {
    /// A review/submission was accepted and may be used as the resume source.
    Accepted,
    /// Auto-submit ran but did not produce an accepted review/submission.
    NotAccepted,
    /// Auto-submit is blocked by policy, freshness, or review state.
    Blocked,
}

/// Safety verdict for checkpoint candidates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum CheckpointSafetyVerdict {
    /// The safety scanner accepted this checkpoint for later resume.
    Safe,
    /// The scanner rejected this checkpoint.
    Unsafe {
        /// Machine- or human-readable safety findings.
        findings: Vec<String>,
    },
    /// No safety verdict was recorded; checkpoint resume must not use it.
    Missing,
}

/// Machine-readable reason a candidate was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum ResumeSourceSkipReason {
    TaskIdMismatch {
        expected: String,
        actual: String,
    },
    SessionLineageMismatch {
        expected: String,
        actual: Option<String>,
    },
    AutoSubmitNotAccepted,
    AutoSubmitBlocked,
    CheckpointUnsafe {
        findings: Vec<String>,
    },
    CheckpointSafetyMissing,
    SourceStale {
        age_secs: u64,
        max_age_secs: u64,
    },
    MissingCheckpointSha,
    MissingSubmitOrReviewId,
}

/// Rejected candidate plus its machine-readable reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedResumeSource {
    pub kind: ResumeSourceKind,
    pub target_ref: String,
    pub checkpoint_sha: Option<String>,
    pub submit_or_review_id: Option<String>,
    pub reason: ResumeSourceSkipReason,
}

/// A pure candidate for resume-source selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeSourceCandidate {
    pub kind: ResumeSourceKind,
    pub task_id: String,
    /// Prior session or lineage identifier that produced this source.
    pub session_lineage: Option<String>,
    /// Commit SHA for checkpoint candidates.
    pub checkpoint_sha: Option<String>,
    /// Submit/review id for accepted auto-submit candidates.
    pub submit_or_review_id: Option<String>,
    /// Ref the future integration should check out. The selector only records it.
    pub target_ref: String,
    /// Auto-submit acceptance state; required only for auto-submit candidates.
    pub auto_submit_state: Option<AutoSubmitResumeState>,
    /// Checkpoint safety state; required only for checkpoint candidates.
    pub checkpoint_safety: Option<CheckpointSafetyVerdict>,
    /// Caller-computed age for freshness validation. `None` is treated as fresh.
    pub age_secs: Option<u64>,
}

impl ResumeSourceCandidate {
    pub fn clean_task_branch(task_id: impl Into<String>, target_ref: impl Into<String>) -> Self {
        Self {
            kind: ResumeSourceKind::CleanTaskBranch,
            task_id: task_id.into(),
            session_lineage: None,
            checkpoint_sha: None,
            submit_or_review_id: None,
            target_ref: target_ref.into(),
            auto_submit_state: None,
            checkpoint_safety: None,
            age_secs: None,
        }
    }
}

/// Chosen resume source plus every rejected candidate evaluated before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeSourceSelection {
    pub chosen_kind: ResumeSourceKind,
    pub prior_session_lineage: Option<String>,
    pub checkpoint_sha: Option<String>,
    pub submit_or_review_id: Option<String>,
    pub target_ref: String,
    pub reason: ResumeSelectionReason,
    pub skipped: Vec<RejectedResumeSource>,
}

/// Select the best resume source in deterministic precedence order:
/// accepted auto-submit, safe task-branch checkpoint, safe alternate checkpoint
/// ref, then clean task branch fallback.
///
/// Returns `None` when resume selection is disabled. This keeps the helper pure
/// and lets current dispatch behavior remain unchanged until the integration
/// task wires the selector into worktree setup.
pub fn select_resume_source(
    config: &ResumeLifecycleConfig,
    expected_task_id: &str,
    expected_session_lineage: Option<&str>,
    candidates: &[ResumeSourceCandidate],
) -> Option<ResumeSourceSelection> {
    if !config.enabled {
        return None;
    }

    let mut ordered: Vec<&ResumeSourceCandidate> = candidates.iter().collect();
    ordered.sort_by_key(|candidate| {
        (
            source_precedence(candidate),
            candidate.age_secs.unwrap_or(0),
        )
    });

    let mut skipped = Vec::new();
    for candidate in ordered {
        match validate_candidate(
            config,
            expected_task_id,
            expected_session_lineage,
            candidate,
        ) {
            Ok(()) => {
                return Some(ResumeSourceSelection {
                    chosen_kind: candidate.kind,
                    prior_session_lineage: candidate.session_lineage.clone(),
                    checkpoint_sha: candidate.checkpoint_sha.clone(),
                    submit_or_review_id: candidate.submit_or_review_id.clone(),
                    target_ref: candidate.target_ref.clone(),
                    reason: candidate.kind.selection_reason(),
                    skipped,
                });
            }
            Err(reason) => skipped.push(RejectedResumeSource {
                kind: candidate.kind,
                target_ref: candidate.target_ref.clone(),
                checkpoint_sha: candidate.checkpoint_sha.clone(),
                submit_or_review_id: candidate.submit_or_review_id.clone(),
                reason,
            }),
        }
    }

    None
}

fn validate_candidate(
    config: &ResumeLifecycleConfig,
    expected_task_id: &str,
    expected_session_lineage: Option<&str>,
    candidate: &ResumeSourceCandidate,
) -> Result<(), ResumeSourceSkipReason> {
    if candidate.task_id != expected_task_id {
        return Err(ResumeSourceSkipReason::TaskIdMismatch {
            expected: expected_task_id.to_owned(),
            actual: candidate.task_id.clone(),
        });
    }

    if candidate.kind != ResumeSourceKind::CleanTaskBranch
        && let Some(expected) = expected_session_lineage
        && candidate.session_lineage.as_deref() != Some(expected)
    {
        return Err(ResumeSourceSkipReason::SessionLineageMismatch {
            expected: expected.to_owned(),
            actual: candidate.session_lineage.clone(),
        });
    }

    if let Some(max_age_secs) = config.max_checkpoint_age_secs
        && let Some(age_secs) = candidate.age_secs
        && age_secs > max_age_secs
    {
        return Err(ResumeSourceSkipReason::SourceStale {
            age_secs,
            max_age_secs,
        });
    }

    match candidate.kind {
        ResumeSourceKind::AutoSubmit => validate_auto_submit(candidate),
        ResumeSourceKind::TaskBranchCheckpoint | ResumeSourceKind::AlternateCheckpointRef => {
            validate_checkpoint(candidate)
        }
        ResumeSourceKind::CleanTaskBranch => Ok(()),
    }
}

fn validate_auto_submit(candidate: &ResumeSourceCandidate) -> Result<(), ResumeSourceSkipReason> {
    match candidate.auto_submit_state {
        Some(AutoSubmitResumeState::Accepted) => {
            if candidate.submit_or_review_id.is_some() {
                Ok(())
            } else {
                Err(ResumeSourceSkipReason::MissingSubmitOrReviewId)
            }
        }
        Some(AutoSubmitResumeState::Blocked) => Err(ResumeSourceSkipReason::AutoSubmitBlocked),
        Some(AutoSubmitResumeState::NotAccepted) | None => {
            Err(ResumeSourceSkipReason::AutoSubmitNotAccepted)
        }
    }
}

fn validate_checkpoint(candidate: &ResumeSourceCandidate) -> Result<(), ResumeSourceSkipReason> {
    if candidate.checkpoint_sha.is_none() {
        return Err(ResumeSourceSkipReason::MissingCheckpointSha);
    }

    match &candidate.checkpoint_safety {
        Some(CheckpointSafetyVerdict::Safe) => Ok(()),
        Some(CheckpointSafetyVerdict::Unsafe { findings }) => {
            Err(ResumeSourceSkipReason::CheckpointUnsafe {
                findings: findings.clone(),
            })
        }
        Some(CheckpointSafetyVerdict::Missing) | None => {
            Err(ResumeSourceSkipReason::CheckpointSafetyMissing)
        }
    }
}

/// Convert a pure selection into passive lifecycle metadata. This records all
/// integration-relevant fields without performing checkout/worktree mutation.
pub fn selection_to_metadata(selection: &ResumeSourceSelection) -> ResumeLifecycleMetadata {
    let mut extra = serde_json::Map::new();
    extra.insert(
        "source_kind".to_string(),
        serde_json::json!(selection.chosen_kind),
    );
    extra.insert(
        "target_ref".to_string(),
        serde_json::json!(selection.target_ref),
    );
    if let Some(id) = &selection.submit_or_review_id {
        extra.insert("submit_or_review_id".to_string(), serde_json::json!(id));
    }
    if let Some(lineage) = &selection.prior_session_lineage {
        extra.insert(
            "prior_session_lineage".to_string(),
            serde_json::json!(lineage),
        );
    }
    extra.insert("skipped".to_string(), serde_json::json!(selection.skipped));

    ResumeLifecycleMetadata {
        dispatch_owner_incarnation_id: None,
        dispatch_group_id: None,
        considered: true,
        checkpoint_id: None,
        commit_sha: selection.checkpoint_sha.clone(),
        selection_reason: Some(selection.reason),
        extra,
        // Failover-aware fields stay `None` here — they're populated by
        // `select_resume_lifecycle_metadata_for_dispatch` from the model
        // rotation lifecycle row, not from the resume source selection. This
        // keeps `selection_to_metadata` a pure reflection of the
        // [`ResumeSourceSelection`] without leaking dispatch-time knowledge.
        previous_model: None,
        new_model: None,
        failover_reason: None,
        last_durable_progress_summary: None,
        verification_command: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TASK: &str = "task-1";
    const LINEAGE: &str = "session-1";
    const TASK_REF: &str = "refs/heads/task/task-1";

    fn config() -> ResumeLifecycleConfig {
        ResumeLifecycleConfig {
            enabled: true,
            prefer_checkpoint: true,
            max_checkpoint_age_secs: Some(300),
        }
    }

    fn auto_submit(state: AutoSubmitResumeState) -> ResumeSourceCandidate {
        ResumeSourceCandidate {
            kind: ResumeSourceKind::AutoSubmit,
            task_id: TASK.to_owned(),
            session_lineage: Some(LINEAGE.to_owned()),
            checkpoint_sha: None,
            submit_or_review_id: if state == AutoSubmitResumeState::Accepted {
                Some("review-1".to_owned())
            } else {
                None
            },
            target_ref: TASK_REF.to_owned(),
            auto_submit_state: Some(state),
            checkpoint_safety: None,
            age_secs: Some(10),
        }
    }

    fn checkpoint(
        kind: ResumeSourceKind,
        sha: &str,
        safety: CheckpointSafetyVerdict,
    ) -> ResumeSourceCandidate {
        ResumeSourceCandidate {
            kind,
            task_id: TASK.to_owned(),
            session_lineage: Some(LINEAGE.to_owned()),
            checkpoint_sha: Some(sha.to_owned()),
            submit_or_review_id: None,
            target_ref: if kind == ResumeSourceKind::AlternateCheckpointRef {
                "refs/djinn/checkpoints/task-1/session-1".to_owned()
            } else {
                TASK_REF.to_owned()
            },
            auto_submit_state: None,
            checkpoint_safety: Some(safety),
            age_secs: Some(20),
        }
    }

    fn clean() -> ResumeSourceCandidate {
        ResumeSourceCandidate::clean_task_branch(TASK, TASK_REF)
    }

    fn select(candidates: Vec<ResumeSourceCandidate>) -> ResumeSourceSelection {
        select_resume_source(&config(), TASK, Some(LINEAGE), &candidates).expect("selection")
    }

    fn lifecycle_with_checkpoint(
        sha: Option<&str>,
        safety: Option<CheckpointSafetyScanMetadata>,
    ) -> WorkerLifecycleMetadata {
        let mut extra = serde_json::Map::new();
        extra.insert("session_id".to_string(), serde_json::json!(LINEAGE));
        WorkerLifecycleMetadata {
            checkpoint: Some(CheckpointLifecycleMetadata {
                checkpoint_id: Some("ckpt-1".to_string()),
                commit_sha: sha.map(str::to_owned),
                ref_name: Some(TASK_REF.to_string()),
                requested_for: None,
                safety_scan: safety,
                preservation_outcome: None,
                extra,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn builds_candidates_from_accepted_auto_submit_metadata() {
        let mut extra = serde_json::Map::new();
        extra.insert("session_id".to_string(), serde_json::json!(LINEAGE));
        let lifecycle = WorkerLifecycleMetadata {
            auto_submit: Some(AutoSubmitLifecycleMetadata {
                considered: true,
                green: Some(true),
                verification_command: Some("cargo test".to_string()),
                submission_id: Some("submit-1".to_string()),
                skipped_reason: None,
                extra,
            }),
            ..Default::default()
        };

        let candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);
        let selection = select(candidates);
        let metadata = selection_to_metadata(&selection);

        assert_eq!(selection.chosen_kind, ResumeSourceKind::AutoSubmit);
        assert_eq!(selection.submit_or_review_id.as_deref(), Some("submit-1"));
        assert_eq!(
            metadata.extra["prior_session_lineage"],
            serde_json::json!(LINEAGE)
        );
    }

    #[test]
    fn builds_safe_checkpoint_and_alternate_ref_candidates() {
        let mut lifecycle = lifecycle_with_checkpoint(
            Some("ckpt-sha"),
            Some(CheckpointSafetyScanMetadata {
                passed: true,
                scanner: Some("scanner".to_string()),
                findings: vec![],
            }),
        );
        let checkpoint = lifecycle.checkpoint.as_mut().expect("checkpoint");
        checkpoint.extra.insert(
            "alternate_checkpoint_ref".to_string(),
            serde_json::json!("refs/djinn/checkpoints/task-1/session-1"),
        );
        checkpoint.extra.insert(
            "alternate_checkpoint_sha".to_string(),
            serde_json::json!("alt-sha"),
        );

        let candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);

        assert!(candidates.iter().any(|candidate| {
            candidate.kind == ResumeSourceKind::TaskBranchCheckpoint
                && candidate.checkpoint_sha.as_deref() == Some("ckpt-sha")
        }));
        assert!(candidates.iter().any(|candidate| {
            candidate.kind == ResumeSourceKind::AlternateCheckpointRef
                && candidate.checkpoint_sha.as_deref() == Some("alt-sha")
                && candidate.target_ref == "refs/djinn/checkpoints/task-1/session-1"
        }));
    }

    #[test]
    fn missing_safety_and_mismatched_metadata_fall_back_cleanly() {
        let mut lifecycle = lifecycle_with_checkpoint(Some("ckpt-sha"), None);
        lifecycle
            .checkpoint
            .as_mut()
            .expect("checkpoint")
            .extra
            .insert("task_id".to_string(), serde_json::json!("other-task"));

        let candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);
        let selection = select(candidates);

        assert_eq!(selection.chosen_kind, ResumeSourceKind::CleanTaskBranch);
        assert!(selection.skipped.iter().any(|skipped| {
            matches!(
                skipped.reason,
                ResumeSourceSkipReason::TaskIdMismatch { .. }
            )
        }));
    }

    #[test]
    fn empty_lifecycle_builds_clean_fallback_candidate() {
        let candidates = build_resume_source_candidates(
            TASK,
            TASK_REF,
            Some(LINEAGE),
            &WorkerLifecycleMetadata::default(),
        );

        assert_eq!(candidates, vec![clean()]);
    }

    #[test]
    fn accepted_auto_submit_wins_over_safe_checkpoint() {
        let selection = select(vec![
            checkpoint(
                ResumeSourceKind::TaskBranchCheckpoint,
                "abc123",
                CheckpointSafetyVerdict::Safe,
            ),
            auto_submit(AutoSubmitResumeState::Accepted),
            clean(),
        ]);

        assert_eq!(selection.chosen_kind, ResumeSourceKind::AutoSubmit);
        assert_eq!(selection.submit_or_review_id.as_deref(), Some("review-1"));
        assert_eq!(selection.target_ref, TASK_REF);
        assert!(selection.skipped.is_empty());
    }

    #[test]
    fn safe_task_branch_checkpoint_resumes_when_auto_submit_not_accepted() {
        let selection = select(vec![
            auto_submit(AutoSubmitResumeState::NotAccepted),
            checkpoint(
                ResumeSourceKind::TaskBranchCheckpoint,
                "abc123",
                CheckpointSafetyVerdict::Safe,
            ),
            clean(),
        ]);

        assert_eq!(
            selection.chosen_kind,
            ResumeSourceKind::TaskBranchCheckpoint
        );
        assert_eq!(selection.checkpoint_sha.as_deref(), Some("abc123"));
        assert_eq!(
            selection.reason,
            ResumeSelectionReason::LatestSafeCheckpoint
        );
        assert_eq!(selection.skipped.len(), 1);
        assert_eq!(
            selection.skipped[0].reason,
            ResumeSourceSkipReason::AutoSubmitNotAccepted
        );
    }

    #[test]
    fn unsafe_checkpoint_is_skipped_for_clean_fallback() {
        let selection = select(vec![
            checkpoint(
                ResumeSourceKind::TaskBranchCheckpoint,
                "bad123",
                CheckpointSafetyVerdict::Unsafe {
                    findings: vec!["secret detected".to_owned()],
                },
            ),
            clean(),
        ]);

        assert_eq!(selection.chosen_kind, ResumeSourceKind::CleanTaskBranch);
        assert_eq!(
            selection.reason,
            ResumeSelectionReason::CleanTaskBranchFallback
        );
        assert_eq!(selection.skipped.len(), 1);
        assert_eq!(
            selection.skipped[0].reason,
            ResumeSourceSkipReason::CheckpointUnsafe {
                findings: vec!["secret detected".to_owned()]
            }
        );
    }

    #[test]
    fn mismatched_task_id_or_lineage_are_skipped() {
        let mut wrong_task = checkpoint(
            ResumeSourceKind::TaskBranchCheckpoint,
            "abc123",
            CheckpointSafetyVerdict::Safe,
        );
        wrong_task.task_id = "other-task".to_owned();
        let mut wrong_lineage = checkpoint(
            ResumeSourceKind::AlternateCheckpointRef,
            "def456",
            CheckpointSafetyVerdict::Safe,
        );
        wrong_lineage.session_lineage = Some("other-session".to_owned());

        let selection = select(vec![wrong_task, wrong_lineage, clean()]);

        assert_eq!(selection.chosen_kind, ResumeSourceKind::CleanTaskBranch);
        assert_eq!(selection.skipped.len(), 2);
        assert!(matches!(
            selection.skipped[0].reason,
            ResumeSourceSkipReason::TaskIdMismatch { .. }
        ));
        assert!(matches!(
            selection.skipped[1].reason,
            ResumeSourceSkipReason::SessionLineageMismatch { .. }
        ));
    }

    #[test]
    fn alternate_checkpoint_ref_resumes_after_task_branch_checkpoint_rejected() {
        let selection = select(vec![
            checkpoint(
                ResumeSourceKind::TaskBranchCheckpoint,
                "stale",
                CheckpointSafetyVerdict::Missing,
            ),
            checkpoint(
                ResumeSourceKind::AlternateCheckpointRef,
                "alt456",
                CheckpointSafetyVerdict::Safe,
            ),
            clean(),
        ]);

        assert_eq!(
            selection.chosen_kind,
            ResumeSourceKind::AlternateCheckpointRef
        );
        assert_eq!(selection.checkpoint_sha.as_deref(), Some("alt456"));
        assert_eq!(
            selection.reason,
            ResumeSelectionReason::AlternateCheckpointRef
        );
        assert_eq!(selection.skipped.len(), 1);
        assert_eq!(
            selection.skipped[0].reason,
            ResumeSourceSkipReason::CheckpointSafetyMissing
        );
    }

    #[test]
    fn stale_or_missing_safety_checkpoint_candidates_are_skipped() {
        let mut stale = checkpoint(
            ResumeSourceKind::TaskBranchCheckpoint,
            "old",
            CheckpointSafetyVerdict::Safe,
        );
        stale.age_secs = Some(301);
        let missing_safety = checkpoint(
            ResumeSourceKind::AlternateCheckpointRef,
            "no-scan",
            CheckpointSafetyVerdict::Missing,
        );

        let selection = select(vec![stale, missing_safety, clean()]);

        assert_eq!(selection.chosen_kind, ResumeSourceKind::CleanTaskBranch);
        assert_eq!(selection.skipped.len(), 2);
        assert_eq!(
            selection.skipped[0].reason,
            ResumeSourceSkipReason::SourceStale {
                age_secs: 301,
                max_age_secs: 300,
            }
        );
        assert_eq!(
            selection.skipped[1].reason,
            ResumeSourceSkipReason::CheckpointSafetyMissing
        );
    }

    #[test]
    fn clean_fallback_output_records_machine_readable_metadata() {
        let selection = select(vec![auto_submit(AutoSubmitResumeState::Blocked), clean()]);
        let metadata = selection_to_metadata(&selection);

        assert!(metadata.considered);
        assert_eq!(
            metadata.selection_reason,
            Some(ResumeSelectionReason::CleanTaskBranchFallback)
        );
        assert_eq!(
            metadata.extra["source_kind"],
            serde_json::json!("clean_task_branch")
        );
        assert_eq!(metadata.extra["target_ref"], serde_json::json!(TASK_REF));
        assert!(metadata.extra["skipped"].is_array());
    }

    #[test]
    fn disabled_config_returns_none_without_fallback_metadata() {
        let disabled = ResumeLifecycleConfig::default();
        let selection = select_resume_source(&disabled, TASK, Some(LINEAGE), &[clean()]);
        assert!(selection.is_none());
    }
}
