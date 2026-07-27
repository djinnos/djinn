//! Pure resume-source selection for re-dispatch.
//!
//! This module intentionally performs no checkout, GitHub, Kubernetes, or
//! database work. Callers translate durable lifecycle/review/checkpoint rows
//! into [`ResumeSourceCandidate`] values, then use [`select_resume_source`] to
//! choose the safest source in a deterministic precedence order.
//!
use serde::{Deserialize, Serialize};

use crate::{
    CheckpointLifecycleMetadata, CheckpointSafetyScanMetadata, ResumeLifecycleConfig,
    ResumeLifecycleMetadata,
    ResumeSelectionReason, WorkerLifecycleMetadata,
};

/// Resume source classes, ordered by [`source_precedence`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResumeSourceKind {
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
            Self::TaskBranchCheckpoint => 0,
            Self::AlternateCheckpointRef => 1,
            Self::CleanTaskBranch => 2,
        }
    }

    fn selection_reason(self) -> ResumeSelectionReason {
        match self {
            Self::TaskBranchCheckpoint => ResumeSelectionReason::LatestSafeCheckpoint,
            Self::AlternateCheckpointRef => ResumeSelectionReason::AlternateCheckpointRef,
            Self::CleanTaskBranch => ResumeSelectionReason::CleanTaskBranchFallback,
        }
    }
}

fn source_precedence(candidate: &ResumeSourceCandidate) -> u8 {
    candidate.kind.precedence()
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
    CheckpointUnsafe {
        findings: Vec<String>,
    },
    CheckpointSafetyMissing,
    SourceStale {
        age_secs: u64,
        max_age_secs: u64,
    },
    MissingCheckpointSha,
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
/// safe task-branch checkpoint, safe alternate checkpoint
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
        ResumeSourceKind::TaskBranchCheckpoint | ResumeSourceKind::AlternateCheckpointRef => {
            validate_checkpoint(candidate)
        }
        ResumeSourceKind::CleanTaskBranch => Ok(()),
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
    }
}
