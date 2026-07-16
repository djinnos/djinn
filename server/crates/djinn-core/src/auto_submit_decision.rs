//! Auto-submit decision gate and structured event payloads.
//!
//! This module provides the pure, testable decision layer for the auto-submit
//! pipeline. It evaluates whether an auto-submit trigger (idle, looping,
//! no-progress exit, soft deadline, controlled termination) should proceed,
//! and produces structured event payloads for `verify.freshness_evaluated`
//! and `review.auto_submit_decision`.
//!
//! This module does **not** perform any submission side effects — it only
//! decides submit vs skip and explains skip reasons. Actual submission wiring
//! is handled by the downstream task (sq5h).
//!
//! Diff safety classification (secret, binary, excluded, generated, WIP) is
//! provided by the caller as pre-classified [`ChangedFile`] entries. If a
//! safety scanner or diff classifier exists in sibling work, it should be
//! used to populate `changed_files` rather than duplicating classification
//! here.

use serde::{Deserialize, Serialize};

use crate::canonical_verify::{
    FileStatus, FreshnessCompatibilityInput, FreshnessVerdict, evaluate_freshness,
};
use crate::models::{AutoSubmitTriggerReason, VerifyRunRecord};

// ─── Changed-file classification ──────────────────────────────────────────

/// Safety category assigned to a changed file in the diff.
///
/// The caller is responsible for classifying each changed file before passing
/// it to the decision gate. This module does not scan file contents — it
/// evaluates the pre-classified categories for blocking decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeFileCategory {
    /// File is safe for auto-submit (source, config, docs, etc.).
    Safe,
    /// File contains secrets or sensitive material (keys, tokens, credentials).
    Secret,
    /// File is a binary artifact (images, compiled objects, archives).
    Binary,
    /// File is excluded from auto-submit by project/task policy.
    Excluded,
    /// File is auto-generated (codegen output, lockfiles, etc.).
    Generated,
    /// File is a work-in-progress artifact (drafts, temp files).
    Wip,
}

impl ChangeFileCategory {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Secret => "secret",
            Self::Binary => "binary",
            Self::Excluded => "excluded",
            Self::Generated => "generated",
            Self::Wip => "wip",
        }
    }
}

impl std::fmt::Display for ChangeFileCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A changed file in the diff being evaluated, with its pre-assigned safety
/// category.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChangedFile {
    /// Relative path within the repository.
    pub path: String,
    /// Pre-classified safety category.
    pub category: ChangeFileCategory,
}

// ─── Block reasons ────────────────────────────────────────────────────────

/// Machine-readable reasons the auto-submit decision gate can block
/// submission.
///
/// Each variant carries enough context for metrics, logging, and structured
/// event payloads so downstream consumers can surface actionable information
/// without re-evaluating the decision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoSubmitBlockReason {
    /// No canonical verify run exists for this task run.
    MissingCanonicalVerify,
    /// The canonical verify is stale (failed, diff mismatch, or
    /// file-changed-after-verify). The inner string is the human-readable
    /// description derived from the [`FreshnessRejectionReason`].
    StaleVerify(String),
    /// Task-specific required checks are missing from verify coverage.
    MissingTaskChecks(Vec<String>),
    /// The diff contains unsafe changes (broad category for any safety
    /// violation not covered by the specific categories below).
    UnsafeDiff,
    /// The diff contains files classified as secret/sensitive.
    SecretChange(Vec<String>),
    /// The diff contains binary file changes.
    BinaryChange(Vec<String>),
    /// The diff contains excluded file changes.
    ExcludedChange(Vec<String>),
    /// Every changed file in the diff is generated — no human-authored
    /// source changes are present.
    GeneratedOnlyChanges,
    /// Every changed file in the diff is WIP — no non-WIP changes
    /// are present.
    WipOnlyChanges,
}

impl std::fmt::Display for AutoSubmitBlockReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCanonicalVerify => write!(f, "missing canonical verify"),
            Self::StaleVerify(detail) => write!(f, "stale verify: {detail}"),
            Self::MissingTaskChecks(checks) => {
                write!(f, "missing task checks: [{}]", checks.join(", "))
            }
            Self::UnsafeDiff => write!(f, "unsafe diff"),
            Self::SecretChange(paths) => {
                write!(f, "secret changes: [{}]", paths.join(", "))
            }
            Self::BinaryChange(paths) => {
                write!(f, "binary changes: [{}]", paths.join(", "))
            }
            Self::ExcludedChange(paths) => {
                write!(f, "excluded changes: [{}]", paths.join(", "))
            }
            Self::GeneratedOnlyChanges => write!(f, "generated-only changes"),
            Self::WipOnlyChanges => write!(f, "wip-only changes"),
        }
    }
}

// ─── Decision input ───────────────────────────────────────────────────────

/// Input for the auto-submit decision gate.
///
/// All fields are provided by the caller (the slot lifecycle or reply-loop
/// settlement code). This module does not fetch data — it evaluates the
/// provided inputs and returns a decision.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AutoSubmitDecisionInput {
    /// The trigger reason that is requesting auto-submit consideration.
    pub trigger_reason: AutoSubmitTriggerReason,
    /// The diff fingerprint of the current worker diff.
    pub diff_fingerprint: String,
    /// The latest canonical verify run for this task run (if any).
    pub verify_run: Option<VerifyRunRecord>,
    /// Tracked files and their modification timestamps.
    pub tracked_files: Vec<FileStatus>,
    /// Allowed untracked files and their modification timestamps.
    pub allowed_untracked_files: Vec<FileStatus>,
    /// Task-specific required checks from the resolved canonical verify
    /// profile.
    pub required_checks: Vec<String>,
    /// Complete current compatibility material. Unavailable derivation or
    /// manifest resolution is represented explicitly and blocks reuse.
    pub compatibility: FreshnessCompatibilityInput,
    /// Changed files in the diff with their pre-classified safety
    /// categories.
    pub changed_files: Vec<ChangedFile>,
    /// Optional submit ID if one is already available.
    pub submit_id: Option<String>,
    /// Optional session ID for event metadata.
    pub session_id: Option<String>,
    /// Optional model ID for event metadata.
    pub model_id: Option<String>,
    /// No-progress streak counter at the time of the decision.
    pub no_progress_streak: i32,
    /// Whether the model explicitly called `submit_work` during the session.
    pub model_called_submit_work: bool,
}

// ─── Decision output ──────────────────────────────────────────────────────

/// Result of the auto-submit decision gate.
///
/// Contains the eligibility verdict, the recorded trigger reason, the block
/// reason (when ineligible), and the freshness evaluation result. Callers
/// use this to decide whether to proceed with submission and to construct
/// the structured event payloads.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AutoSubmitDecision {
    /// `true` when the diff is eligible for auto-submit.
    pub eligible: bool,
    /// The trigger reason that was evaluated (always recorded in metadata).
    pub trigger_reason: AutoSubmitTriggerReason,
    /// When `eligible` is `false`, the machine-readable block reason.
    pub block_reason: Option<AutoSubmitBlockReason>,
    /// The freshness verdict from the canonical verify evaluation.
    pub freshness_verdict: FreshnessVerdict,
}

// ─── Structured event payloads ────────────────────────────────────────────

/// Structured event payload for `verify.freshness_evaluated`.
///
/// Captures the freshness evaluation inputs and result so metrics and logging
/// can observe every freshness check without re-evaluating.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VerifyFreshnessEvaluatedEvent {
    /// The diff fingerprint that was evaluated.
    pub diff_fingerprint: String,
    /// Whether a canonical verify run was found.
    pub has_verify_run: bool,
    /// The freshness verdict (fresh + optional rejection reason).
    pub freshness_verdict: FreshnessVerdict,
    /// The trigger reason that initiated the freshness evaluation.
    pub trigger_reason: AutoSubmitTriggerReason,
    /// Optional submit ID if available.
    pub submit_id: Option<String>,
}

/// Structured event payload for `review.auto_submit_decision`.
///
/// Captures the full decision context so metrics, logging, and audit systems
/// can observe the decision without re-evaluation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReviewAutoSubmitDecisionEvent {
    /// Whether the diff was eligible for auto-submit.
    pub eligible: bool,
    /// The trigger reason that initiated the decision.
    pub trigger_reason: AutoSubmitTriggerReason,
    /// When not eligible, the machine-readable block reason.
    pub block_reason: Option<AutoSubmitBlockReason>,
    /// The diff fingerprint of the evaluated diff.
    pub diff_fingerprint: String,
    /// The freshness verdict from the canonical verify evaluation.
    pub freshness_verdict: FreshnessVerdict,
    /// Optional submit ID when available.
    pub submit_id: Option<String>,
    /// Session ID for audit linkage.
    pub session_id: Option<String>,
    /// Model ID for audit linkage.
    pub model_id: Option<String>,
    /// No-progress streak counter at decision time.
    pub no_progress_streak: i32,
    /// Whether the model called `submit_work` during the session.
    pub model_called_submit_work: bool,
}

// ─── Core decision function ───────────────────────────────────────────────

/// Evaluate the auto-submit decision gate.
///
/// Runs the following checks in order and returns on the first failure:
///
/// 1. **Freshness evaluation** — delegates to
///    [`evaluate_freshness`](crate::canonical_verify::evaluate_freshness) to
///    check canonical verify existence, pass result, exact diff fingerprint,
///    file-modification recency, and task-specific check coverage.
///
/// 2. **Diff safety classification** — blocks when the diff contains
///    secret, binary, excluded, generated-only, or WIP-only changes.
///
/// 3. **Unsafe diff** — blocks when no safe files are present and none of
///    the specific categories (secret/binary/excluded/generated/wip) apply
///    (a catch-all for unclassified unsafe changes).
///
/// When all checks pass, returns an eligible decision with the trigger reason
/// recorded. The function also produces the structured event payloads that
/// callers can emit on their event bus.
///
/// # Arguments
///
/// * `input` — all data needed for the decision, pre-assembled by the caller
///
/// # Returns
///
/// A tuple of `(AutoSubmitDecision, VerifyFreshnessEvaluatedEvent,
/// ReviewAutoSubmitDecisionEvent)` so callers can use the decision and
/// emit both structured events atomically.
pub fn evaluate_auto_submit_decision(
    input: &AutoSubmitDecisionInput,
) -> (
    AutoSubmitDecision,
    VerifyFreshnessEvaluatedEvent,
    ReviewAutoSubmitDecisionEvent,
) {
    // Step 1: Evaluate freshness using the canonical verify module.
    let freshness_verdict = evaluate_freshness(
        &input.diff_fingerprint,
        input.verify_run.as_ref(),
        &input.tracked_files,
        &input.allowed_untracked_files,
        &input.required_checks,
        &input.compatibility,
    );

    // Build the verify.freshness_evaluated event payload.
    let freshness_event = VerifyFreshnessEvaluatedEvent {
        diff_fingerprint: input.diff_fingerprint.clone(),
        has_verify_run: input.verify_run.is_some(),
        freshness_verdict: freshness_verdict.clone(),
        trigger_reason: input.trigger_reason,
        submit_id: input.submit_id.clone(),
    };

    // If freshness failed, short-circuit — no point checking diff safety.
    if !freshness_verdict.fresh {
        let block_reason = freshness_to_block_reason(&freshness_verdict);
        let decision = AutoSubmitDecision {
            eligible: false,
            trigger_reason: input.trigger_reason,
            block_reason: Some(block_reason.clone()),
            freshness_verdict: freshness_verdict.clone(),
        };
        let review_event = build_review_event(input, &decision);
        return (decision, freshness_event, review_event);
    }

    // Step 2: Evaluate diff safety classification.
    if let Some(block_reason) = evaluate_diff_safety(&input.changed_files) {
        let decision = AutoSubmitDecision {
            eligible: false,
            trigger_reason: input.trigger_reason,
            block_reason: Some(block_reason),
            freshness_verdict: freshness_verdict.clone(),
        };
        let review_event = build_review_event(input, &decision);
        return (decision, freshness_event, review_event);
    }

    // All checks passed — eligible for auto-submit.
    let decision = AutoSubmitDecision {
        eligible: true,
        trigger_reason: input.trigger_reason,
        block_reason: None,
        freshness_verdict: freshness_verdict.clone(),
    };
    let review_event = build_review_event(input, &decision);
    (decision, freshness_event, review_event)
}

// ─── Internal helpers ─────────────────────────────────────────────────────

/// Convert a freshness rejection into an [`AutoSubmitBlockReason`].
fn freshness_to_block_reason(verdict: &FreshnessVerdict) -> AutoSubmitBlockReason {
    use crate::canonical_verify::FreshnessRejectionReason;

    match &verdict.reason {
        Some(FreshnessRejectionReason::NoCanonicalVerify) => {
            AutoSubmitBlockReason::MissingCanonicalVerify
        }
        Some(FreshnessRejectionReason::VerifyNotPass) => {
            AutoSubmitBlockReason::StaleVerify("verify result is not pass".to_owned())
        }
        Some(FreshnessRejectionReason::VersionIncompatible(reason)) => {
            AutoSubmitBlockReason::StaleVerify(format!(
                "verification compatibility miss: {reason:?}"
            ))
        }
        Some(FreshnessRejectionReason::DiffMismatch) => {
            AutoSubmitBlockReason::StaleVerify("diff fingerprint mismatch".to_owned())
        }
        Some(FreshnessRejectionReason::FileChangedAfterVerify(path)) => {
            AutoSubmitBlockReason::StaleVerify(format!("file changed after verify: {path}"))
        }
        Some(FreshnessRejectionReason::MissingTaskChecks(checks)) => {
            AutoSubmitBlockReason::MissingTaskChecks(checks.clone())
        }
        None => {
            // Should not happen when fresh=false, but handle defensively.
            AutoSubmitBlockReason::StaleVerify("unknown freshness failure".to_owned())
        }
    }
}

/// Evaluate diff safety based on pre-classified changed files.
///
/// Returns `Some(block_reason)` when the diff should be blocked, or `None`
/// when the diff is safe for auto-submit.
fn evaluate_diff_safety(changed_files: &[ChangedFile]) -> Option<AutoSubmitBlockReason> {
    if changed_files.is_empty() {
        // Empty diff — nothing to submit, treat as not eligible but not
        // blocked by safety reasons. The caller should handle "no diff"
        // separately. Return None so the decision path considers it
        // eligible (the downstream submit path will see the empty diff).
        return None;
    }

    // Collect paths by non-safe category.
    let secret_paths: Vec<String> = changed_files
        .iter()
        .filter(|f| f.category == ChangeFileCategory::Secret)
        .map(|f| f.path.clone())
        .collect();
    let binary_paths: Vec<String> = changed_files
        .iter()
        .filter(|f| f.category == ChangeFileCategory::Binary)
        .map(|f| f.path.clone())
        .collect();
    let excluded_paths: Vec<String> = changed_files
        .iter()
        .filter(|f| f.category == ChangeFileCategory::Excluded)
        .map(|f| f.path.clone())
        .collect();

    // Block if any secret, binary, or excluded files are present.
    if !secret_paths.is_empty() {
        return Some(AutoSubmitBlockReason::SecretChange(secret_paths));
    }
    if !binary_paths.is_empty() {
        return Some(AutoSubmitBlockReason::BinaryChange(binary_paths));
    }
    if !excluded_paths.is_empty() {
        return Some(AutoSubmitBlockReason::ExcludedChange(excluded_paths));
    }

    // Check if all changes are generated-only.
    let all_generated = changed_files
        .iter()
        .all(|f| f.category == ChangeFileCategory::Generated);
    if all_generated {
        return Some(AutoSubmitBlockReason::GeneratedOnlyChanges);
    }

    // Check if all changes are WIP-only.
    let all_wip = changed_files
        .iter()
        .all(|f| f.category == ChangeFileCategory::Wip);
    if all_wip {
        return Some(AutoSubmitBlockReason::WipOnlyChanges);
    }

    // Mixed categories are OK as long as at least one safe file is present
    // and no blocked categories (secret/binary/excluded) exist. A diff with
    // safe + generated files is acceptable — only generated-only is blocked.
    //
    // If no safe files are present at all (e.g. only generated + wip mixed,
    // or some future unclassified category), block as unsafe.
    let has_safe = changed_files
        .iter()
        .any(|f| f.category == ChangeFileCategory::Safe);
    if !has_safe {
        return Some(AutoSubmitBlockReason::UnsafeDiff);
    }

    None
}

/// Build the `review.auto_submit_decision` event payload from the input and
/// decision.
fn build_review_event(
    input: &AutoSubmitDecisionInput,
    decision: &AutoSubmitDecision,
) -> ReviewAutoSubmitDecisionEvent {
    ReviewAutoSubmitDecisionEvent {
        eligible: decision.eligible,
        trigger_reason: decision.trigger_reason,
        block_reason: decision.block_reason.clone(),
        diff_fingerprint: input.diff_fingerprint.clone(),
        freshness_verdict: decision.freshness_verdict.clone(),
        submit_id: input.submit_id.clone(),
        session_id: input.session_id.clone(),
        model_id: input.model_id.clone(),
        no_progress_streak: input.no_progress_streak,
        model_called_submit_work: input.model_called_submit_work,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::canonical_verify::{
        CurrentEnvironmentIdentity, SUPPORTED_ENVIRONMENT_IDENTITY_VERSION_V1,
        SUPPORTED_VERIFICATION_INPUT_MANIFEST_VERSION_V1,
    };

    fn compatibility() -> FreshnessCompatibilityInput {
        FreshnessCompatibilityInput {
            verification_input_fingerprint: Some("inputs-v1".to_owned()),
            environment_identity: Some(CurrentEnvironmentIdentity {
                version: SUPPORTED_ENVIRONMENT_IDENTITY_VERSION_V1.to_owned(),
                digest: "identity-digest-v1".to_owned(),
            }),
            manifest_version: Some(SUPPORTED_VERIFICATION_INPUT_MANIFEST_VERSION_V1.to_owned()),
        }
    }

    fn make_run(
        result: &str,
        diff_fingerprint: &str,
        completed_at: &str,
        check_coverage: Option<serde_json::Value>,
    ) -> VerifyRunRecord {
        VerifyRunRecord {
            id: "vr-1".to_owned(),
            task_run_id: "tr-1".to_owned(),
            verify_source: "ci".to_owned(),
            verify_run_id: "external-1".to_owned(),
            command_version: Some("1.0.0".to_owned()),
            profile_version: Some("v1".to_owned()),
            completed_at: completed_at.to_owned(),
            result: result.to_owned(),
            diff_fingerprint: diff_fingerprint.to_owned(),
            check_coverage,
            source_phase: Some("final_verification".to_owned()),
            verification_attempt_id: Some("attempt-1".to_owned()),
            ordered_commands: None,
            covered_checks: None,
            verification_input_fingerprint: Some("inputs-v1".to_owned()),
            manifest_version: Some(SUPPORTED_VERIFICATION_INPUT_MANIFEST_VERSION_V1.to_owned()),
            environment_identity_json: None,
            environment_identity_digest: Some("identity-digest-v1".to_owned()),
            environment_identity_version: Some(
                SUPPORTED_ENVIRONMENT_IDENTITY_VERSION_V1.to_owned(),
            ),
            created_at: "2025-01-15T10:30:00.000Z".to_owned(),
        }
    }

    fn make_input(trigger: AutoSubmitTriggerReason) -> AutoSubmitDecisionInput {
        AutoSubmitDecisionInput {
            trigger_reason: trigger,
            diff_fingerprint: "abc123".to_owned(),
            verify_run: Some(make_run(
                "pass",
                "abc123",
                "2025-01-15T10:30:00.000Z",
                Some(serde_json::json!({"lint": true, "test": true})),
            )),
            tracked_files: vec![],
            allowed_untracked_files: vec![],
            required_checks: vec!["lint".into(), "test".into()],
            compatibility: compatibility(),
            changed_files: vec![
                ChangedFile {
                    path: "src/main.rs".to_owned(),
                    category: ChangeFileCategory::Safe,
                },
                ChangedFile {
                    path: "src/lib.rs".to_owned(),
                    category: ChangeFileCategory::Safe,
                },
            ],
            submit_id: Some("submit-1".to_owned()),
            session_id: Some("session-1".to_owned()),
            model_id: Some("model-1".to_owned()),
            no_progress_streak: 0,
            model_called_submit_work: false,
        }
    }

    // ── Eligible green exact diff ─────────────────────────────────────────

    #[test]
    fn eligible_green_exact_diff_idle_trigger() {
        let input = make_input(AutoSubmitTriggerReason::Idle);
        let (decision, freshness_event, review_event) = evaluate_auto_submit_decision(&input);

        assert!(decision.eligible);
        assert_eq!(decision.trigger_reason, AutoSubmitTriggerReason::Idle);
        assert!(decision.block_reason.is_none());
        assert!(decision.freshness_verdict.fresh);

        // verify.freshness_evaluated event
        assert_eq!(freshness_event.diff_fingerprint, "abc123");
        assert!(freshness_event.has_verify_run);
        assert!(freshness_event.freshness_verdict.fresh);
        assert_eq!(
            freshness_event.trigger_reason,
            AutoSubmitTriggerReason::Idle
        );
        assert_eq!(freshness_event.submit_id.as_deref(), Some("submit-1"));

        // review.auto_submit_decision event
        assert!(review_event.eligible);
        assert_eq!(review_event.trigger_reason, AutoSubmitTriggerReason::Idle);
        assert!(review_event.block_reason.is_none());
        assert_eq!(review_event.diff_fingerprint, "abc123");
        assert_eq!(review_event.session_id.as_deref(), Some("session-1"));
        assert_eq!(review_event.model_id.as_deref(), Some("model-1"));
        assert_eq!(review_event.no_progress_streak, 0);
        assert!(!review_event.model_called_submit_work);
    }

    #[test]
    fn eligible_green_exact_diff_all_trigger_reasons() {
        for trigger in [
            AutoSubmitTriggerReason::Idle,
            AutoSubmitTriggerReason::Looping,
            AutoSubmitTriggerReason::NoProgress,
            AutoSubmitTriggerReason::SoftDeadline,
            AutoSubmitTriggerReason::ControlledTermination,
        ] {
            let input = make_input(trigger);
            let (decision, _, review_event) = evaluate_auto_submit_decision(&input);
            assert!(decision.eligible, "trigger {trigger} must be eligible");
            assert_eq!(decision.trigger_reason, trigger);
            assert_eq!(review_event.trigger_reason, trigger);
            assert!(review_event.eligible);
        }
    }

    // ── Block: missing canonical verify ───────────────────────────────────

    #[test]
    fn block_missing_canonical_verify() {
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        input.verify_run = None;

        let (decision, freshness_event, review_event) = evaluate_auto_submit_decision(&input);

        assert!(!decision.eligible);
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::MissingCanonicalVerify)
        );
        assert!(!freshness_event.freshness_verdict.fresh);
        assert!(!review_event.eligible);
        assert_eq!(
            review_event.block_reason,
            Some(AutoSubmitBlockReason::MissingCanonicalVerify)
        );
    }

    // ── Block: stale verify (not pass) ────────────────────────────────────

    #[test]
    fn block_stale_verify_not_pass() {
        let mut input = make_input(AutoSubmitTriggerReason::Looping);
        input.verify_run = Some(make_run("fail", "abc123", "2025-01-15T10:30:00.000Z", None));

        let (decision, _, _) = evaluate_auto_submit_decision(&input);

        assert!(!decision.eligible);
        match &decision.block_reason {
            Some(AutoSubmitBlockReason::StaleVerify(msg)) => {
                assert!(msg.contains("not pass"));
            }
            other => panic!("expected StaleVerify, got {other:?}"),
        }
    }

    // ── Block: stale verify (diff mismatch) ───────────────────────────────

    #[test]
    fn block_stale_verify_diff_mismatch() {
        let mut input = make_input(AutoSubmitTriggerReason::NoProgress);
        input.verify_run = Some(make_run(
            "pass",
            "different_fingerprint",
            "2025-01-15T10:30:00.000Z",
            None,
        ));

        let (decision, _, _) = evaluate_auto_submit_decision(&input);

        assert!(!decision.eligible);
        match &decision.block_reason {
            Some(AutoSubmitBlockReason::StaleVerify(msg)) => {
                assert!(msg.contains("mismatch"));
            }
            other => panic!("expected StaleVerify, got {other:?}"),
        }
    }

    // ── Block: stale verify (file changed after verify) ───────────────────

    #[test]
    fn block_stale_verify_file_changed_after_verify() {
        let mut input = make_input(AutoSubmitTriggerReason::SoftDeadline);
        input.tracked_files = vec![FileStatus {
            path: "src/main.rs".to_owned(),
            modified_at: "2025-01-15T11:00:00.000Z".to_owned(), // after verify
        }];

        let (decision, freshness_event, _) = evaluate_auto_submit_decision(&input);

        assert!(!decision.eligible);
        match &decision.block_reason {
            Some(AutoSubmitBlockReason::StaleVerify(msg)) => {
                assert!(msg.contains("file changed after verify"));
                assert!(msg.contains("src/main.rs"));
            }
            other => panic!("expected StaleVerify, got {other:?}"),
        }
        assert!(!freshness_event.freshness_verdict.fresh);
    }

    // ── Block: missing task-specific checks ───────────────────────────────

    #[test]
    fn block_missing_task_checks() {
        let mut input = make_input(AutoSubmitTriggerReason::ControlledTermination);
        input.verify_run = Some(make_run(
            "pass",
            "abc123",
            "2025-01-15T10:30:00.000Z",
            Some(serde_json::json!({"lint": true})), // missing "test"
        ));

        let (decision, _, _) = evaluate_auto_submit_decision(&input);

        assert!(!decision.eligible);
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::MissingTaskChecks(vec![
                "test".to_owned()
            ]))
        );
    }

    // ── Block: secret change ──────────────────────────────────────────────

    #[test]
    fn block_secret_change() {
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        input.changed_files = vec![
            ChangedFile {
                path: "src/main.rs".to_owned(),
                category: ChangeFileCategory::Safe,
            },
            ChangedFile {
                path: ".env".to_owned(),
                category: ChangeFileCategory::Secret,
            },
        ];

        let (decision, _, review_event) = evaluate_auto_submit_decision(&input);

        assert!(!decision.eligible);
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::SecretChange(vec![".env".to_owned()]))
        );
        assert_eq!(
            review_event.block_reason,
            Some(AutoSubmitBlockReason::SecretChange(vec![".env".to_owned()]))
        );
    }

    // ── Block: binary change ──────────────────────────────────────────────

    #[test]
    fn block_binary_change() {
        let mut input = make_input(AutoSubmitTriggerReason::Looping);
        input.changed_files = vec![
            ChangedFile {
                path: "assets/logo.png".to_owned(),
                category: ChangeFileCategory::Binary,
            },
            ChangedFile {
                path: "README.md".to_owned(),
                category: ChangeFileCategory::Safe,
            },
        ];

        let (decision, _, _) = evaluate_auto_submit_decision(&input);

        assert!(!decision.eligible);
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::BinaryChange(vec![
                "assets/logo.png".to_owned()
            ]))
        );
    }

    // ── Block: excluded change ────────────────────────────────────────────

    #[test]
    fn block_excluded_change() {
        let mut input = make_input(AutoSubmitTriggerReason::NoProgress);
        input.changed_files = vec![
            ChangedFile {
                path: "vendor/dep.js".to_owned(),
                category: ChangeFileCategory::Excluded,
            },
            ChangedFile {
                path: "src/app.ts".to_owned(),
                category: ChangeFileCategory::Safe,
            },
        ];

        let (decision, _, _) = evaluate_auto_submit_decision(&input);

        assert!(!decision.eligible);
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::ExcludedChange(vec![
                "vendor/dep.js".to_owned()
            ]))
        );
    }

    // ── Block: generated-only changes ─────────────────────────────────────

    #[test]
    fn block_generated_only_changes() {
        let mut input = make_input(AutoSubmitTriggerReason::SoftDeadline);
        input.changed_files = vec![
            ChangedFile {
                path: "generated/api.rs".to_owned(),
                category: ChangeFileCategory::Generated,
            },
            ChangedFile {
                path: "generated/types.rs".to_owned(),
                category: ChangeFileCategory::Generated,
            },
        ];

        let (decision, _, _) = evaluate_auto_submit_decision(&input);

        assert!(!decision.eligible);
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::GeneratedOnlyChanges)
        );
    }

    // ── Block: WIP-only changes ───────────────────────────────────────────

    #[test]
    fn block_wip_only_changes() {
        let mut input = make_input(AutoSubmitTriggerReason::ControlledTermination);
        input.changed_files = vec![
            ChangedFile {
                path: "WIP_notes.md".to_owned(),
                category: ChangeFileCategory::Wip,
            },
            ChangedFile {
                path: "WIP_draft.rs".to_owned(),
                category: ChangeFileCategory::Wip,
            },
        ];

        let (decision, _, _) = evaluate_auto_submit_decision(&input);

        assert!(!decision.eligible);
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::WipOnlyChanges)
        );
    }

    // ── Block: unsafe diff (no safe files) ──────────────────────────────

    #[test]
    fn block_unsafe_diff_no_safe_files() {
        // A diff with mixed generated + wip files (not all one category)
        // has no safe files and should be blocked as UnsafeDiff.
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        input.changed_files = vec![
            ChangedFile {
                path: "generated/api.rs".to_owned(),
                category: ChangeFileCategory::Generated,
            },
            ChangedFile {
                path: "WIP_notes.md".to_owned(),
                category: ChangeFileCategory::Wip,
            },
        ];

        let (decision, _, review_event) = evaluate_auto_submit_decision(&input);

        assert!(!decision.eligible);
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::UnsafeDiff)
        );
        assert!(!review_event.eligible);
        assert_eq!(
            review_event.block_reason,
            Some(AutoSubmitBlockReason::UnsafeDiff)
        );
    }

    // ── Mixed safe + generated is allowed ─────────────────────────────────

    #[test]
    fn eligible_mixed_safe_and_generated() {
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        input.changed_files = vec![
            ChangedFile {
                path: "src/main.rs".to_owned(),
                category: ChangeFileCategory::Safe,
            },
            ChangedFile {
                path: "generated/api.rs".to_owned(),
                category: ChangeFileCategory::Generated,
            },
        ];

        let (decision, _, _) = evaluate_auto_submit_decision(&input);
        assert!(decision.eligible);
    }

    // ── Mixed safe + WIP is allowed ───────────────────────────────────────

    #[test]
    fn eligible_mixed_safe_and_wip() {
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        input.changed_files = vec![
            ChangedFile {
                path: "src/main.rs".to_owned(),
                category: ChangeFileCategory::Safe,
            },
            ChangedFile {
                path: "WIP_notes.md".to_owned(),
                category: ChangeFileCategory::Wip,
            },
        ];

        let (decision, _, _) = evaluate_auto_submit_decision(&input);
        assert!(decision.eligible);
    }

    // ── Empty diff is eligible (no safety block) ──────────────────────────

    #[test]
    fn eligible_empty_diff() {
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        input.changed_files = vec![];

        let (decision, _, _) = evaluate_auto_submit_decision(&input);
        assert!(decision.eligible);
    }

    // ── Multiple secret paths reported ────────────────────────────────────

    #[test]
    fn block_reports_all_secret_paths() {
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        input.changed_files = vec![
            ChangedFile {
                path: ".env".to_owned(),
                category: ChangeFileCategory::Secret,
            },
            ChangedFile {
                path: "secrets.json".to_owned(),
                category: ChangeFileCategory::Secret,
            },
        ];

        let (decision, _, _) = evaluate_auto_submit_decision(&input);
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::SecretChange(vec![
                ".env".to_owned(),
                "secrets.json".to_owned()
            ]))
        );
    }

    // ── Multiple binary paths reported ────────────────────────────────────

    #[test]
    fn block_reports_all_binary_paths() {
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        input.changed_files = vec![
            ChangedFile {
                path: "image.png".to_owned(),
                category: ChangeFileCategory::Binary,
            },
            ChangedFile {
                path: "data.bin".to_owned(),
                category: ChangeFileCategory::Binary,
            },
        ];

        let (decision, _, _) = evaluate_auto_submit_decision(&input);
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::BinaryChange(vec![
                "image.png".to_owned(),
                "data.bin".to_owned()
            ]))
        );
    }

    // ── Multiple excluded paths reported ──────────────────────────────────

    #[test]
    fn block_reports_all_excluded_paths() {
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        input.changed_files = vec![
            ChangedFile {
                path: "vendor/a.js".to_owned(),
                category: ChangeFileCategory::Excluded,
            },
            ChangedFile {
                path: "vendor/b.js".to_owned(),
                category: ChangeFileCategory::Excluded,
            },
        ];

        let (decision, _, _) = evaluate_auto_submit_decision(&input);
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::ExcludedChange(vec![
                "vendor/a.js".to_owned(),
                "vendor/b.js".to_owned()
            ]))
        );
    }

    // ── Secret takes precedence over binary/excluded/generated/wip ────────

    #[test]
    fn secret_takes_precedence_over_binary() {
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        input.changed_files = vec![
            ChangedFile {
                path: ".env".to_owned(),
                category: ChangeFileCategory::Secret,
            },
            ChangedFile {
                path: "image.png".to_owned(),
                category: ChangeFileCategory::Binary,
            },
        ];

        let (decision, _, _) = evaluate_auto_submit_decision(&input);
        // Secret is checked first.
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::SecretChange(vec![".env".to_owned()]))
        );
    }

    // ── Binary takes precedence over excluded ─────────────────────────────

    #[test]
    fn binary_takes_precedence_over_excluded() {
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        input.changed_files = vec![
            ChangedFile {
                path: "image.png".to_owned(),
                category: ChangeFileCategory::Binary,
            },
            ChangedFile {
                path: "vendor/a.js".to_owned(),
                category: ChangeFileCategory::Excluded,
            },
        ];

        let (decision, _, _) = evaluate_auto_submit_decision(&input);
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::BinaryChange(vec![
                "image.png".to_owned()
            ]))
        );
    }

    // ── Event payloads serialize correctly ────────────────────────────────

    #[test]
    fn freshness_event_serializes_and_roundtrips() {
        let input = make_input(AutoSubmitTriggerReason::Idle);
        let (_, freshness_event, _) = evaluate_auto_submit_decision(&input);

        let json = serde_json::to_string(&freshness_event).unwrap();
        let back: VerifyFreshnessEvaluatedEvent = serde_json::from_str(&json).unwrap();

        assert_eq!(back.diff_fingerprint, "abc123");
        assert!(back.has_verify_run);
        assert!(back.freshness_verdict.fresh);
        assert_eq!(back.trigger_reason, AutoSubmitTriggerReason::Idle);
        assert_eq!(back.submit_id.as_deref(), Some("submit-1"));
    }

    #[test]
    fn review_decision_event_serializes_and_roundtrips() {
        let input = make_input(AutoSubmitTriggerReason::SoftDeadline);
        let (_, _, review_event) = evaluate_auto_submit_decision(&input);

        let json = serde_json::to_string(&review_event).unwrap();
        let back: ReviewAutoSubmitDecisionEvent = serde_json::from_str(&json).unwrap();

        assert!(back.eligible);
        assert_eq!(back.trigger_reason, AutoSubmitTriggerReason::SoftDeadline);
        assert!(back.block_reason.is_none());
        assert_eq!(back.diff_fingerprint, "abc123");
        assert_eq!(back.session_id.as_deref(), Some("session-1"));
        assert_eq!(back.model_id.as_deref(), Some("model-1"));
        assert_eq!(back.no_progress_streak, 0);
        assert!(!back.model_called_submit_work);
    }

    #[test]
    fn review_decision_event_with_block_reason_serializes() {
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        input.verify_run = None;

        let (_, _, review_event) = evaluate_auto_submit_decision(&input);

        let json = serde_json::to_string(&review_event).unwrap();
        let back: ReviewAutoSubmitDecisionEvent = serde_json::from_str(&json).unwrap();

        assert!(!back.eligible);
        assert_eq!(
            back.block_reason,
            Some(AutoSubmitBlockReason::MissingCanonicalVerify)
        );
    }

    // ── Block reason Display formatting ───────────────────────────────────

    #[test]
    fn block_reason_display_formatting() {
        assert_eq!(
            AutoSubmitBlockReason::MissingCanonicalVerify.to_string(),
            "missing canonical verify"
        );
        assert_eq!(
            AutoSubmitBlockReason::StaleVerify("test".to_owned()).to_string(),
            "stale verify: test"
        );
        assert_eq!(
            AutoSubmitBlockReason::MissingTaskChecks(vec!["lint".into(), "test".into()])
                .to_string(),
            "missing task checks: [lint, test]"
        );
        assert_eq!(
            AutoSubmitBlockReason::SecretChange(vec![".env".into()]).to_string(),
            "secret changes: [.env]"
        );
        assert_eq!(
            AutoSubmitBlockReason::BinaryChange(vec!["img.png".into()]).to_string(),
            "binary changes: [img.png]"
        );
        assert_eq!(
            AutoSubmitBlockReason::ExcludedChange(vec!["vendor/x".into()]).to_string(),
            "excluded changes: [vendor/x]"
        );
        assert_eq!(
            AutoSubmitBlockReason::GeneratedOnlyChanges.to_string(),
            "generated-only changes"
        );
        assert_eq!(
            AutoSubmitBlockReason::WipOnlyChanges.to_string(),
            "wip-only changes"
        );
    }

    // ── ChangeFileCategory Display ────────────────────────────────────────

    #[test]
    fn change_file_category_display() {
        assert_eq!(ChangeFileCategory::Safe.to_string(), "safe");
        assert_eq!(ChangeFileCategory::Secret.to_string(), "secret");
        assert_eq!(ChangeFileCategory::Binary.to_string(), "binary");
        assert_eq!(ChangeFileCategory::Excluded.to_string(), "excluded");
        assert_eq!(ChangeFileCategory::Generated.to_string(), "generated");
        assert_eq!(ChangeFileCategory::Wip.to_string(), "wip");
    }

    // ── No-progress streak and model_called_submit_work preserved ─────────

    #[test]
    fn metadata_preserved_in_review_event() {
        let mut input = make_input(AutoSubmitTriggerReason::NoProgress);
        input.no_progress_streak = 5;
        input.model_called_submit_work = true;
        input.submit_id = Some("submit-42".to_owned());

        let (decision, _, review_event) = evaluate_auto_submit_decision(&input);

        assert!(decision.eligible);
        assert_eq!(review_event.no_progress_streak, 5);
        assert!(review_event.model_called_submit_work);
        assert_eq!(review_event.submit_id.as_deref(), Some("submit-42"));
    }

    // ── Freshness rejection short-circuits before diff safety ──────────────

    #[test]
    fn freshness_failure_short_circuits_diff_safety() {
        let mut input = make_input(AutoSubmitTriggerReason::Idle);
        // No verify run → freshness fails
        input.verify_run = None;
        // But changed files include a secret — should still report freshness
        // block, not the secret block.
        input.changed_files = vec![ChangedFile {
            path: ".env".to_owned(),
            category: ChangeFileCategory::Secret,
        }];

        let (decision, freshness_event, _) = evaluate_auto_submit_decision(&input);

        assert!(!decision.eligible);
        // The block reason should be from freshness, not diff safety.
        assert_eq!(
            decision.block_reason,
            Some(AutoSubmitBlockReason::MissingCanonicalVerify)
        );
        assert!(!freshness_event.freshness_verdict.fresh);
    }

    // ── Serialization round-trip for AutoSubmitBlockReason ─────────────

    #[test]
    fn block_reason_serialization_uses_externally_tagged_format() {
        let reason = AutoSubmitBlockReason::MissingCanonicalVerify;
        let json = serde_json::to_value(&reason).unwrap();
        assert_eq!(json, serde_json::json!("missing_canonical_verify"));

        let reason = AutoSubmitBlockReason::SecretChange(vec![".env".into()]);
        let json = serde_json::to_value(&reason).unwrap();
        assert_eq!(json, serde_json::json!({"secret_change": [".env"]}));

        let reason = AutoSubmitBlockReason::StaleVerify("diff mismatch".into());
        let json = serde_json::to_value(&reason).unwrap();
        assert_eq!(json, serde_json::json!({"stale_verify": "diff mismatch"}));

        let reason = AutoSubmitBlockReason::MissingTaskChecks(vec!["lint".into()]);
        let json = serde_json::to_value(&reason).unwrap();
        assert_eq!(json, serde_json::json!({"missing_task_checks": ["lint"]}));
    }
}
