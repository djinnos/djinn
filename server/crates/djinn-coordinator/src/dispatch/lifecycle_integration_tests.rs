//! Integrated lifecycle tests for the y8pv epic.
//!
//! These tests bind the pure no-progress gate, preservation decision,
//! resume-source selection, and model-rotation metadata into focused scenarios
//! without touching live Kubernetes, Postgres, GitHub, or git worktrees. They
//! assert through the public/internal helper APIs introduced by j6u1, 3ln4,
//! 48ru, and sy0g.

use crate::dispatch::resume_source::{
    CheckpointSafetyVerdict, ResumeSourceCandidate, ResumeSourceKind,
    build_resume_source_candidates, select_resume_source, selection_to_metadata,
};
use crate::worker_lifecycle::{
    ControlledExitPreservationAction, decide_controlled_exit_preservation_action,
    evaluate_no_progress_controlled_exit,
};
use crate::{
    AutoSubmitLifecycleConfig, AutoSubmitLifecycleMetadata, AutoSubmitSkipReason,
    CheckpointLifecycleConfig, CheckpointLifecycleMetadata, CheckpointRequestReason,
    CheckpointSafetyScanMetadata, NoProgressCommandState, NoProgressControlledExitDecision,
    NoProgressEnforcementMode, NoProgressThresholdConfig, PreservationFailurePolicy,
    PreservationOutcome, ResumeLifecycleConfig, ResumeSelectionReason, WorkerLifecycleConfig,
    WorkerLifecycleMetadata,
};

const TASK: &str = "task-y8pv";
const LINEAGE: &str = "session-prior";
const TASK_REF: &str = "refs/heads/task/task-y8pv";

fn enforcing_config() -> WorkerLifecycleConfig {
    WorkerLifecycleConfig {
        rollout: crate::DurableProgressRolloutConfig {
            detection_mode: crate::DurableProgressDetectionMode::Enforce,
            no_progress_enforcement: NoProgressEnforcementMode::Enforce,
            checkpoint_before_no_progress_exit: true,
            auto_submit_if_green: true,
            resume_from_checkpoint: true,
            rotate_model_on_no_progress: true,
        },
        no_progress_thresholds: NoProgressThresholdConfig {
            min_evaluated_turns: 2,
            warning_turns: 2,
            model_rotation_turns: Some(6),
            forced_exit_turns: Some(8),
            long_command_suspension_secs: 600,
            flaky_command_grace_turns: 0,
        },
        checkpoint: CheckpointLifecycleConfig {
            enabled: true,
            require_before_no_progress_exit: true,
            ref_namespace: None,
            failure_policy: PreservationFailurePolicy::RecordAndProceed,
        },
        auto_submit: AutoSubmitLifecycleConfig {
            enabled: true,
            require_fresh_verification: true,
            canonical_verification_gate: Some("default".to_string()),
        },
        resume: ResumeLifecycleConfig {
            enabled: true,
            prefer_checkpoint: true,
            max_checkpoint_age_secs: Some(300),
        },
        model_rotation: crate::ModelRotationLifecycleConfig::default(),
        slow_extension: crate::SlowExtensionConfig::default(),
    }
}

fn safe_checkpoint_metadata() -> CheckpointLifecycleMetadata {
    let mut extra = serde_json::Map::new();
    extra.insert("session_id".to_string(), serde_json::json!(LINEAGE));
    CheckpointLifecycleMetadata {
        checkpoint_id: Some("ckpt-y8pv".to_string()),
        commit_sha: Some("deadbeef".to_string()),
        ref_name: Some(TASK_REF.to_string()),
        requested_for: Some(CheckpointRequestReason::NoProgressWindDown),
        safety_scan: Some(CheckpointSafetyScanMetadata {
            passed: true,
            scanner: Some("safety-v1".to_string()),
            findings: vec![],
        }),
        preservation_outcome: Some(PreservationOutcome::Succeeded),
        extra,
    }
}

fn accepted_auto_submit_metadata() -> AutoSubmitLifecycleMetadata {
    let mut extra = serde_json::Map::new();
    extra.insert("session_id".to_string(), serde_json::json!(LINEAGE));
    AutoSubmitLifecycleMetadata {
        considered: true,
        green: Some(true),
        verification_command: Some("cargo test".to_string()),
        submission_id: Some("review-y8pv".to_string()),
        skipped_reason: None,
        extra,
    }
}

#[test]
fn no_progress_idle_requests_exit_and_uses_checkpoint_fallback() {
    let config = enforcing_config();

    // Streak at the configured forced_exit_turns bound while idle.
    let decision = evaluate_no_progress_controlled_exit(
        &config,
        config.no_progress_thresholds.forced_exit_turns.unwrap(),
        NoProgressCommandState::Idle,
    );
    assert_eq!(decision, NoProgressControlledExitDecision::RequestExit);

    // With no accepted auto-submit, the dirty delta must be preserved via checkpoint.
    let action = decide_controlled_exit_preservation_action(
        false,
        Some(PreservationOutcome::Succeeded),
        config.checkpoint.failure_policy,
    );
    assert_eq!(action, ControlledExitPreservationAction::RequestCheckpoint);

    // Resume selection then picks the safe checkpoint on the task branch.
    let lifecycle = WorkerLifecycleMetadata {
        checkpoint: Some(safe_checkpoint_metadata()),
        ..Default::default()
    };
    let candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);
    let selection = select_resume_source(&config.resume, TASK, Some(LINEAGE), &candidates)
        .expect("resume selection should succeed");

    assert_eq!(
        selection.chosen_kind,
        ResumeSourceKind::TaskBranchCheckpoint
    );
    assert_eq!(selection.checkpoint_sha.as_deref(), Some("deadbeef"));
    assert_eq!(
        selection.reason,
        ResumeSelectionReason::LatestSafeCheckpoint
    );
    assert_eq!(selection.target_ref, TASK_REF);
}

#[test]
fn no_progress_long_command_or_unknown_command_defer_exit() {
    let config = enforcing_config();
    let threshold = config.no_progress_thresholds.forced_exit_turns.unwrap();

    // In-flight command suspends the destructive exit regardless of running_secs.
    assert_eq!(
        evaluate_no_progress_controlled_exit(
            &config,
            threshold,
            NoProgressCommandState::InFlight { running_secs: 1200 },
        ),
        NoProgressControlledExitDecision::DeferredForCommand
    );

    // Unknown command state also defers because the coordinator cannot prove safety.
    assert_eq!(
        evaluate_no_progress_controlled_exit(&config, threshold, NoProgressCommandState::Unknown,),
        NoProgressControlledExitDecision::DeferredForCommand
    );
}

#[test]
fn accepted_auto_submit_wins_and_skips_checkpoint_fallback() {
    let config = enforcing_config();
    let lifecycle = WorkerLifecycleMetadata {
        auto_submit: Some(accepted_auto_submit_metadata()),
        checkpoint: Some(safe_checkpoint_metadata()),
        ..Default::default()
    };

    let candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);
    let selection =
        select_resume_source(&config.resume, TASK, Some(LINEAGE), &candidates).expect("selection");

    assert_eq!(selection.chosen_kind, ResumeSourceKind::AutoSubmit);
    assert_eq!(
        selection.submit_or_review_id.as_deref(),
        Some("review-y8pv")
    );
    assert_eq!(selection.reason, ResumeSelectionReason::AutoSubmitAccepted);
    // Auto-submit wins at precedence 0; the checkpoint candidate is never evaluated.
    assert!(selection.skipped.is_empty());

    // Preservation action prefers the accepted auto-submit over checkpointing.
    let action = decide_controlled_exit_preservation_action(
        true,
        Some(PreservationOutcome::RuntimeUnavailable),
        config.checkpoint.failure_policy,
    );
    assert_eq!(action, ControlledExitPreservationAction::UseAutoSubmit);
}

#[test]
fn dirty_delta_checkpoint_fallback_when_auto_submit_blocked() {
    let config = enforcing_config();

    let blocked_auto_submit = AutoSubmitLifecycleMetadata {
        considered: true,
        green: Some(false),
        verification_command: None,
        submission_id: None,
        skipped_reason: Some(AutoSubmitSkipReason::NotGreen),
        extra: serde_json::Map::new(),
    };

    let lifecycle = WorkerLifecycleMetadata {
        auto_submit: Some(blocked_auto_submit),
        checkpoint: Some(safe_checkpoint_metadata()),
        ..Default::default()
    };

    let candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);
    let selection =
        select_resume_source(&config.resume, TASK, Some(LINEAGE), &candidates).expect("selection");

    assert_eq!(
        selection.chosen_kind,
        ResumeSourceKind::TaskBranchCheckpoint
    );
    assert!(
        selection
            .skipped
            .iter()
            .any(|s| s.kind == ResumeSourceKind::AutoSubmit)
    );
}

#[test]
fn unsafe_and_mismatched_checkpoints_are_skipped_with_machine_readable_reasons() {
    let config = enforcing_config();
    let mut checkpoint = safe_checkpoint_metadata();
    checkpoint.safety_scan = Some(CheckpointSafetyScanMetadata {
        passed: false,
        scanner: Some("safety-v1".to_string()),
        findings: vec!["credential leak".to_string()],
    });
    checkpoint
        .extra
        .insert("task_id".to_string(), serde_json::json!("other-task"));

    let lifecycle = WorkerLifecycleMetadata {
        checkpoint: Some(checkpoint),
        ..Default::default()
    };

    let mut candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);
    // Add an unsafe candidate with matching task_id/lineage so both skip reasons appear.
    candidates.push(ResumeSourceCandidate {
        kind: ResumeSourceKind::TaskBranchCheckpoint,
        task_id: TASK.to_string(),
        session_lineage: Some(LINEAGE.to_string()),
        checkpoint_sha: Some("unsafe-sha".to_string()),
        submit_or_review_id: None,
        target_ref: TASK_REF.to_string(),
        auto_submit_state: None,
        checkpoint_safety: Some(CheckpointSafetyVerdict::Unsafe {
            findings: vec!["credential leak".to_string()],
        }),
        age_secs: None,
    });

    let selection =
        select_resume_source(&config.resume, TASK, Some(LINEAGE), &candidates).expect("selection");

    assert_eq!(selection.chosen_kind, ResumeSourceKind::CleanTaskBranch);
    assert!(selection.skipped.iter().any(|s| matches!(
        s.reason,
        crate::dispatch::resume_source::ResumeSourceSkipReason::TaskIdMismatch { .. }
    )));
    assert!(selection.skipped.iter().any(|s| matches!(
        s.reason,
        crate::dispatch::resume_source::ResumeSourceSkipReason::CheckpointUnsafe { .. }
    )));
}

#[test]
fn alternate_checkpoint_ref_selected_after_task_branch_rejected() {
    let config = enforcing_config();
    let mut lifecycle = WorkerLifecycleMetadata {
        checkpoint: Some(safe_checkpoint_metadata()),
        ..Default::default()
    };

    let checkpoint = lifecycle.checkpoint.as_mut().unwrap();
    checkpoint.extra.insert(
        "alternate_checkpoint_ref".to_string(),
        serde_json::json!("refs/djinn/checkpoints/task-y8pv/session-prior"),
    );
    checkpoint.extra.insert(
        "alternate_checkpoint_sha".to_string(),
        serde_json::json!("cafebabe"),
    );
    // Remove the primary task-branch SHA so it is skipped; the alternate ref remains safe.
    checkpoint.commit_sha = None;

    let candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);
    let selection =
        select_resume_source(&config.resume, TASK, Some(LINEAGE), &candidates).expect("selection");

    assert_eq!(
        selection.chosen_kind,
        ResumeSourceKind::AlternateCheckpointRef
    );
    assert_eq!(selection.checkpoint_sha.as_deref(), Some("cafebabe"));
    assert_eq!(
        selection.reason,
        ResumeSelectionReason::AlternateCheckpointRef
    );
    assert_eq!(
        selection.target_ref,
        "refs/djinn/checkpoints/task-y8pv/session-prior"
    );
}

#[test]
fn selection_metadata_records_rotation_reason_for_model_resolution() {
    // Simulate the full re-dispatch chain: resume selection -> metadata -> reason.
    let config = enforcing_config();
    let lifecycle = WorkerLifecycleMetadata {
        checkpoint: Some(safe_checkpoint_metadata()),
        ..Default::default()
    };

    let candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);
    let selection =
        select_resume_source(&config.resume, TASK, Some(LINEAGE), &candidates).expect("selection");
    let metadata = selection_to_metadata(&selection);

    assert!(metadata.considered);
    assert_eq!(
        metadata.selection_reason,
        Some(ResumeSelectionReason::LatestSafeCheckpoint)
    );
    assert_eq!(metadata.commit_sha.as_deref(), Some("deadbeef"));
    assert_eq!(metadata.extra["target_ref"], serde_json::json!(TASK_REF));
    assert!(metadata.extra["skipped"].is_array());
}

#[test]
fn preservation_failure_policy_blocks_or_records_as_configured() {
    let success = decide_controlled_exit_preservation_action(
        false,
        Some(PreservationOutcome::Succeeded),
        PreservationFailurePolicy::RecordAndProceed,
    );
    assert_eq!(success, ControlledExitPreservationAction::RequestCheckpoint);

    let record = decide_controlled_exit_preservation_action(
        false,
        Some(PreservationOutcome::Failed),
        PreservationFailurePolicy::RecordAndProceed,
    );
    assert_eq!(
        record,
        ControlledExitPreservationAction::RecordFailureAndProceed
    );

    let block = decide_controlled_exit_preservation_action(
        false,
        Some(PreservationOutcome::Failed),
        PreservationFailurePolicy::Block,
    );
    assert_eq!(
        block,
        ControlledExitPreservationAction::BlockForPreservationFailure
    );
}

#[test]
fn shadow_mode_observes_no_progress_without_requesting_exit() {
    let mut config = enforcing_config();
    config.rollout.no_progress_enforcement = NoProgressEnforcementMode::Shadow;

    let decision = evaluate_no_progress_controlled_exit(
        &config,
        config.no_progress_thresholds.forced_exit_turns.unwrap(),
        NoProgressCommandState::Idle,
    );
    assert_eq!(decision, NoProgressControlledExitDecision::ShadowWouldExit);
}

#[test]
fn below_threshold_does_not_request_exit() {
    let config = enforcing_config();

    let decision = evaluate_no_progress_controlled_exit(
        &config,
        config.no_progress_thresholds.forced_exit_turns.unwrap() - 1,
        NoProgressCommandState::Idle,
    );
    assert_eq!(decision, NoProgressControlledExitDecision::BelowThreshold);
}

#[test]
fn no_progress_disabled_without_forced_exit_turns() {
    let mut config = enforcing_config();
    config.no_progress_thresholds.forced_exit_turns = None;

    let decision = evaluate_no_progress_controlled_exit(&config, 99, NoProgressCommandState::Idle);
    assert_eq!(decision, NoProgressControlledExitDecision::Disabled);
}

// ── Resume lifecycle integration tests (proposal phif AC 1-5, 9) ─────
// These tests cover the complete worker-side typed resume lifecycle:
// config resolution, typed discontinuity metadata, and prompt rendering.

/// AC 2: A safe checkpoint re-attempt on a resumed task emits typed
/// resume metadata with attempt number, prior session id, source kind,
/// and selection reason fields that the worker prompt renders.
#[test]
fn safe_checkpoint_re_attempt_emits_typed_metadata_with_attempt_fields() {
    let config = enforcing_config();
    let lifecycle = WorkerLifecycleMetadata {
        checkpoint: Some(safe_checkpoint_metadata()),
        ..Default::default()
    };

    let candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);
    let selection = select_resume_source(&config.resume, TASK, Some(LINEAGE), &candidates)
        .expect("resume selection should succeed when checkpoint is safe");
    let metadata = selection_to_metadata(&selection);

    // Typed metadata must carry selection reason, commit SHA, and source kind.
    assert!(metadata.considered);
    assert_eq!(
        metadata.selection_reason,
        Some(ResumeSelectionReason::LatestSafeCheckpoint)
    );
    assert_eq!(metadata.commit_sha.as_deref(), Some("deadbeef"));

    // The extra map must record the source kind and prior session lineage
    // so the worker prompt can render them.
    assert_eq!(
        metadata.extra["source_kind"],
        serde_json::json!("task_branch_checkpoint")
    );
    assert_eq!(
        metadata.extra["prior_session_lineage"],
        serde_json::json!(LINEAGE)
    );
    assert_eq!(metadata.extra["target_ref"], serde_json::json!(TASK_REF));
}

/// AC 4: When all checkpoint/auto-submit candidates are rejected (stall
/// or zombie/ceiling-like kill path), the selector degrades to a clean
/// task-branch fallback. The metadata records this as a
/// `CleanTaskBranchFallback` selection reason with truthful evidence
/// (the skipped candidates and their rejection reasons).
#[test]
fn clean_fallback_for_stall_kill_path_emits_discontinuity_metadata() {
    let config = enforcing_config();

    // Simulate a stall kill: unsafe checkpoint + blocked auto-submit.
    let unsafe_checkpoint = ResumeSourceCandidate {
        kind: ResumeSourceKind::TaskBranchCheckpoint,
        task_id: TASK.to_string(),
        session_lineage: Some(LINEAGE.to_string()),
        checkpoint_sha: Some("stale-sha".to_string()),
        submit_or_review_id: None,
        target_ref: TASK_REF.to_string(),
        auto_submit_state: None,
        checkpoint_safety: Some(CheckpointSafetyVerdict::Unsafe {
            findings: vec!["stalled session leaked credentials".to_string()],
        }),
        age_secs: None,
    };
    let blocked_auto_submit = ResumeSourceCandidate {
        kind: ResumeSourceKind::AutoSubmit,
        task_id: TASK.to_string(),
        session_lineage: Some(LINEAGE.to_string()),
        checkpoint_sha: None,
        submit_or_review_id: None,
        target_ref: TASK_REF.to_string(),
        auto_submit_state: Some(crate::dispatch::resume_source::AutoSubmitResumeState::Blocked),
        checkpoint_safety: None,
        age_secs: None,
    };

    let candidates = vec![
        blocked_auto_submit,
        unsafe_checkpoint,
        ResumeSourceCandidate::clean_task_branch(TASK, TASK_REF),
    ];
    let selection = select_resume_source(&config.resume, TASK, Some(LINEAGE), &candidates)
        .expect("selection must degrade to clean fallback");
    let metadata = selection_to_metadata(&selection);

    assert_eq!(
        selection.chosen_kind,
        ResumeSourceKind::CleanTaskBranch,
        "stall path must fall back to clean task branch"
    );
    assert_eq!(
        selection.reason,
        ResumeSelectionReason::CleanTaskBranchFallback
    );

    // Evidence must be truthful: both skipped candidates recorded.
    assert_eq!(selection.skipped.len(), 2);
    assert!(selection.skipped.iter().any(|s| matches!(
        s.reason,
        crate::dispatch::resume_source::ResumeSourceSkipReason::AutoSubmitBlocked
    )));
    assert!(selection.skipped.iter().any(|s| matches!(
        s.reason,
        crate::dispatch::resume_source::ResumeSourceSkipReason::CheckpointUnsafe { .. }
    )));

    // Metadata must carry the fallback reason and skipped list.
    assert!(metadata.considered);
    assert_eq!(
        metadata.selection_reason,
        Some(ResumeSelectionReason::CleanTaskBranchFallback)
    );
    assert_eq!(
        metadata.extra["source_kind"],
        serde_json::json!("clean_task_branch")
    );
    assert!(metadata.extra["skipped"].is_array());
}

/// AC 4: Non-kill path — when a manual retry or failed-startup produces
/// a re-attempt with only a clean task-branch candidate available (no
/// checkpoint, no auto-submit), the selector emits a
/// `CleanTaskBranchFallback` note that captures the discontinuity.
#[test]
fn non_kill_retry_emits_clean_fallback_discontinuity() {
    let config = enforcing_config();

    // No prior checkpoint or auto-submit — clean task branch only.
    let candidates = vec![ResumeSourceCandidate::clean_task_branch(TASK, TASK_REF)];
    let selection = select_resume_source(&config.resume, TASK, None, &candidates)
        .expect("clean fallback must always succeed");

    assert_eq!(selection.chosen_kind, ResumeSourceKind::CleanTaskBranch);
    assert_eq!(
        selection.reason,
        ResumeSelectionReason::CleanTaskBranchFallback
    );
    assert!(
        selection.skipped.is_empty(),
        "no candidates were skipped on a clean-only path"
    );

    let metadata = selection_to_metadata(&selection);
    assert!(metadata.considered);
    assert_eq!(
        metadata.commit_sha, None,
        "no checkpoint SHA on clean fallback"
    );
}

/// AC 4: Missing evidence path — attempt > 1 with absent prior
/// session/evidence. The selector still dispatches and the metadata
/// marks `considered: true` even when prior_session_lineage is None
/// and no checkpoint/auto-submit candidates exist.
#[test]
fn missing_evidence_dispatches_with_unknown_fields() {
    let config = enforcing_config();

    // Build candidates with no session lineage and no lifecycle metadata.
    let candidates = build_resume_source_candidates(
        TASK,
        TASK_REF,
        None, // no prior session lineage
        &WorkerLifecycleMetadata::default(),
    );
    let selection = select_resume_source(&config.resume, TASK, None, &candidates)
        .expect("must dispatch even with missing evidence");

    assert_eq!(selection.chosen_kind, ResumeSourceKind::CleanTaskBranch);
    assert_eq!(
        selection.reason,
        ResumeSelectionReason::CleanTaskBranchFallback
    );
    assert_eq!(
        selection.prior_session_lineage, None,
        "no prior session when evidence is missing"
    );

    let metadata = selection_to_metadata(&selection);
    assert!(metadata.considered, "metadata must be marked considered");
    assert_eq!(metadata.commit_sha, None, "no checkpoint SHA");
    assert!(
        !metadata.extra.contains_key("prior_session_lineage"),
        "prior_session_lineage key must be absent from extra map when lineage is None"
    );
}

/// AC 5: Preservation/no-replay — accepted auto-submit work must be
/// treated as a valid resume source. When an auto-submit was accepted
/// in the prior session, it wins over checkpoint candidates. The
/// metadata records the review/submission id, NOT the checkpoint SHA,
/// so the worker does not replay checkpoint work.
#[test]
fn preservation_no_replay_accepted_auto_submit_wins_over_checkpoint() {
    let config = enforcing_config();

    let accepted = ResumeSourceCandidate {
        kind: ResumeSourceKind::AutoSubmit,
        task_id: TASK.to_string(),
        session_lineage: Some(LINEAGE.to_string()),
        checkpoint_sha: None,
        submit_or_review_id: Some("review-accepted".to_string()),
        target_ref: TASK_REF.to_string(),
        auto_submit_state: Some(crate::dispatch::resume_source::AutoSubmitResumeState::Accepted),
        checkpoint_safety: None,
        age_secs: None,
    };
    let safe_checkpoint = ResumeSourceCandidate {
        kind: ResumeSourceKind::TaskBranchCheckpoint,
        task_id: TASK.to_string(),
        session_lineage: Some(LINEAGE.to_string()),
        checkpoint_sha: Some("ckpt-deadbeef".to_string()),
        submit_or_review_id: None,
        target_ref: TASK_REF.to_string(),
        auto_submit_state: None,
        checkpoint_safety: Some(CheckpointSafetyVerdict::Safe),
        age_secs: None,
    };

    let selection = select_resume_source(
        &config.resume,
        TASK,
        Some(LINEAGE),
        &[safe_checkpoint, accepted],
    )
    .expect("selection");

    assert_eq!(
        selection.chosen_kind,
        ResumeSourceKind::AutoSubmit,
        "accepted auto-submit must win over checkpoint"
    );
    assert_eq!(
        selection.submit_or_review_id.as_deref(),
        Some("review-accepted")
    );
    assert!(
        selection.checkpoint_sha.is_none(),
        "checkpoint SHA must not be set when auto-submit wins"
    );
    assert!(
        selection.skipped.is_empty(),
        "auto-submit at precedence 0 means checkpoint is never evaluated"
    );
}

/// AC 5: Preservation/no-replay — stale pending work (a checkpoint that
/// is older than max_checkpoint_age_secs) must not be treated as current
/// solely because discontinuity metadata exists. The selector skips it
/// and falls back to clean task branch.
#[test]
fn preservation_no_replay_stale_checkpoint_not_treated_as_current() {
    let config = enforcing_config();

    let mut stale_checkpoint = ResumeSourceCandidate {
        kind: ResumeSourceKind::TaskBranchCheckpoint,
        task_id: TASK.to_string(),
        session_lineage: Some(LINEAGE.to_string()),
        checkpoint_sha: Some("stale-sha".to_string()),
        submit_or_review_id: None,
        target_ref: TASK_REF.to_string(),
        auto_submit_state: None,
        checkpoint_safety: Some(CheckpointSafetyVerdict::Safe),
        age_secs: None,
    };
    // Exceed the max_checkpoint_age_secs (300s from enforcing_config).
    stale_checkpoint.age_secs = Some(600);

    let candidates = vec![
        stale_checkpoint,
        ResumeSourceCandidate::clean_task_branch(TASK, TASK_REF),
    ];
    let selection = select_resume_source(&config.resume, TASK, Some(LINEAGE), &candidates)
        .expect("must fall back to clean branch");

    assert_eq!(
        selection.chosen_kind,
        ResumeSourceKind::CleanTaskBranch,
        "stale checkpoint must be skipped, not treated as current"
    );
    assert_eq!(selection.skipped.len(), 1);
    assert!(matches!(
        selection.skipped[0].reason,
        crate::dispatch::resume_source::ResumeSourceSkipReason::SourceStale {
            age_secs: 600,
            max_age_secs: 300,
        }
    ));
}

/// AC 5: Preservation/no-replay — when resume is disabled (config
/// defaults), `select_resume_source` returns None. This proves that
/// the absence of an enabled config gate prevents any resume metadata
/// from being produced, so no stale work can be "replayed" through
/// the resume lifecycle path.
#[test]
fn disabled_resume_config_returns_no_metadata() {
    let disabled_config = ResumeLifecycleConfig::default();
    let lifecycle = WorkerLifecycleMetadata {
        checkpoint: Some(safe_checkpoint_metadata()),
        ..Default::default()
    };
    let candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);

    let selection = select_resume_source(&disabled_config, TASK, Some(LINEAGE), &candidates);
    assert!(
        selection.is_none(),
        "disabled resume config must not produce any selection"
    );
}

/// AC 2/4: selection_to_metadata serializes the full typed resume
/// metadata wire shape including source_kind, target_ref, and skipped
/// candidates — the worker prompt rendering depends on these fields.
#[test]
fn selection_metadata_wire_shape_includes_all_discontinuity_fields() {
    let config = enforcing_config();
    let lifecycle = WorkerLifecycleMetadata {
        checkpoint: Some(safe_checkpoint_metadata()),
        ..Default::default()
    };

    let candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);
    let selection =
        select_resume_source(&config.resume, TASK, Some(LINEAGE), &candidates).expect("selection");
    let metadata = selection_to_metadata(&selection);

    let wire = serde_json::to_value(&metadata).expect("serialize");

    // Verify top-level typed fields.
    assert_eq!(wire["considered"], serde_json::json!(true));
    assert_eq!(wire["commit_sha"], serde_json::json!("deadbeef"));
    assert_eq!(
        wire["selection_reason"],
        serde_json::json!("latest_safe_checkpoint")
    );
    // Extra map carries source kind and target ref.
    assert_eq!(
        wire["extra"]["source_kind"],
        serde_json::json!("task_branch_checkpoint")
    );
    assert_eq!(wire["extra"]["target_ref"], serde_json::json!(TASK_REF));
    assert!(wire["extra"]["skipped"].is_array());
}

// ── Preservation/no-replay: auto-submit lifecycle metadata tests ─────

/// AC 5: An accepted auto-submit review should produce lifecycle metadata
/// where `considered: true` and `green: Some(true)`. When this metadata
/// is consumed by resume source selection, the auto-submit candidate is
/// marked Accepted and wins over checkpoint candidates.
#[test]
fn accepted_auto_submit_lifecycle_metadata_is_considered_and_green() {
    let metadata = accepted_auto_submit_metadata();
    assert!(metadata.considered);
    assert_eq!(metadata.green, Some(true));
    assert!(metadata.skipped_reason.is_none());
    assert_eq!(metadata.submission_id.as_deref(), Some("review-y8pv"));
}

/// AC 5: A blocked auto-submit (not green) produces lifecycle metadata
/// where the submission_id is None and skipped_reason is set. When
/// consumed by the resume selector, this candidate is rejected and does
/// not become the resume source.
#[test]
fn blocked_auto_submit_lifecycle_metadata_rejects_candidate() {
    let metadata = AutoSubmitLifecycleMetadata {
        considered: true,
        green: Some(false),
        verification_command: None,
        submission_id: None,
        skipped_reason: Some(AutoSubmitSkipReason::NotGreen),
        extra: serde_json::Map::new(),
    };
    assert!(metadata.considered);
    assert_ne!(metadata.green, Some(true));
    assert!(metadata.skipped_reason.is_some());

    // When this metadata feeds into the resume source selector via
    // build_resume_source_candidates, the auto-submit candidate is blocked.
    let lifecycle = WorkerLifecycleMetadata {
        auto_submit: Some(metadata),
        ..Default::default()
    };
    let candidates = build_resume_source_candidates(TASK, TASK_REF, Some(LINEAGE), &lifecycle);
    let selection =
        select_resume_source(&enforcing_config().resume, TASK, Some(LINEAGE), &candidates)
            .expect("must degrade to clean fallback");

    // The blocked auto-submit is rejected; clean task branch wins.
    assert_eq!(selection.chosen_kind, ResumeSourceKind::CleanTaskBranch);
    assert_eq!(
        selection.reason,
        ResumeSelectionReason::CleanTaskBranchFallback
    );
    assert!(selection.skipped.iter().any(|s| matches!(
        s.reason,
        crate::dispatch::resume_source::ResumeSourceSkipReason::AutoSubmitBlocked
    )));
}
