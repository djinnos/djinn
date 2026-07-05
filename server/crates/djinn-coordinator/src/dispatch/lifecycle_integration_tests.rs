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
