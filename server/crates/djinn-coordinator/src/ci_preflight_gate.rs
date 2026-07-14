//! Shared worker-submit / reviewer-approve CI reproduction preflight gate.
//!
//! The gate consumes durable required-check CI snapshot/remediation state,
//! asks the provider for repo-derived GitHub Actions failure context bundles,
//! runs the coordinator reproduction executor, stores structured activity, and
//! reports whether the caller must block the submit/approval control flow.

use std::path::{Path, PathBuf};

use djinn_core::models::{CiStatus, Task, TaskPrCiSnapshot};
use djinn_db::TaskRepository;
use djinn_provider::github_api::{CiFailureContextRequest, GitHubApiClient};

use crate::ci_reproduction::{
    CiPreflightOutcome, CiPreflightResult, CiReproductionRunner, ShellRunner, preflight_reproduce,
};

/// Activity event emitted for each required-check reproduction attempt.
pub(crate) const CI_PREFLIGHT_REPRODUCTION_EVENT: &str = "ci_preflight_reproduction";

/// Caller seam where the preflight ran.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CiPreflightGateKind {
    WorkerSubmit,
    ReviewerApprove,
}

impl CiPreflightGateKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::WorkerSubmit => "worker_submit",
            Self::ReviewerApprove => "reviewer_approve",
        }
    }
}

/// Gate verdict for submit/approval control flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CiPreflightGateVerdict {
    /// No implicated required checks were found in durable state.
    NotApplicable,
    /// Local reproductions passed or only non-blocking fetch failures occurred;
    /// the caller should continue its existing flow. This is not proof that
    /// GitHub CI is green.
    Allow,
    /// A required check reproduced a local failure and the caller must block.
    Block { reason: String },
    /// A required check is unreproducible locally — submit/approval must block
    /// and the caller must route to lead/human intervention. This is distinct
    /// from [`Block`] because the caller must escalate rather than simply
    /// deferring the tick.
    RouteToLeadIntervention { reason: String },
}

/// Select implicated required checks from durable current/prior CI state.
///
/// Current failing snapshots contribute all blocking required check names.
/// Previously failing snapshots are also implicated when a durable remediation
/// baseline remains present (`last_remediation_base_sha`), even if the current
/// status has moved on: this catches submit/approval attempts for red-CI
/// remediation before GitHub has produced a fresh green signal.
pub(crate) fn implicated_required_checks(snapshot: Option<&TaskPrCiSnapshot>) -> Vec<String> {
    let Some(snapshot) = snapshot else {
        return Vec::new();
    };

    let status_implicates = matches!(snapshot.ci_status, CiStatus::Failing);
    let remediation_implicates = snapshot.last_remediation_base_sha.is_some();
    if !status_implicates && !remediation_implicates {
        return Vec::new();
    }

    let mut names = Vec::new();
    for name in &snapshot.blocking_required_check_names {
        let trimmed = name.trim();
        if !trimmed.is_empty() && !names.iter().any(|existing| existing == trimmed) {
            names.push(trimmed.to_string());
        }
    }
    names
}

pub(crate) async fn run_ci_reproduction_preflight_gate(
    task: &Task,
    task_repo: &TaskRepository,
    gh_client: &GitHubApiClient,
    owner: &str,
    repo: &str,
    workdir: &Path,
    kind: CiPreflightGateKind,
) -> CiPreflightGateVerdict {
    run_ci_reproduction_preflight_gate_with_runner(
        task,
        task_repo,
        gh_client,
        owner,
        repo,
        workdir,
        kind,
        &ShellRunner,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_ci_reproduction_preflight_gate_with_runner(
    task: &Task,
    task_repo: &TaskRepository,
    gh_client: &GitHubApiClient,
    owner: &str,
    repo: &str,
    workdir: &Path,
    kind: CiPreflightGateKind,
    runner: &dyn CiReproductionRunner,
) -> CiPreflightGateVerdict {
    let Some(pr_number) = task.ci_pr_number else {
        return CiPreflightGateVerdict::NotApplicable;
    };

    let snapshot = match task_repo
        .get_ci_snapshot_for_task_pr(&task.id, pr_number)
        .await
    {
        Ok(snapshot) => snapshot,
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                pr_number,
                error = %e,
                "CI preflight: failed to load durable CI snapshot; skipping reproduction preflight"
            );
            return CiPreflightGateVerdict::NotApplicable;
        }
    };

    let checks = implicated_required_checks(snapshot.as_ref());
    if checks.is_empty() {
        return CiPreflightGateVerdict::NotApplicable;
    }

    let head_sha = snapshot
        .as_ref()
        .map(|snap| snap.head_sha.clone())
        .or_else(|| task.ci_head_sha.clone())
        .unwrap_or_default();
    if head_sha.trim().is_empty() {
        return CiPreflightGateVerdict::NotApplicable;
    }

    for check_name in checks {
        let request = CiFailureContextRequest {
            owner: owner.to_string(),
            repo: repo.to_string(),
            head_sha: head_sha.clone(),
            required_check_name: check_name.clone(),
            workflow_run_id: None,
            workflow_id: None,
            job_id: None,
            workflow_path: None,
        };

        let bundle = match gh_client.build_ci_failure_context_bundle(request).await {
            Ok(bundle) => bundle,
            Err(e) => {
                let payload = serde_json::json!({
                    "gate": kind.as_str(),
                    "check_name": check_name,
                    "observed_head_sha": head_sha,
                    "outcome": "context_unavailable",
                    "unreproducible_reason": "context_bundle_unavailable",
                    "details": e.to_string(),
                });
                log_preflight_activity(task_repo, &task.id, payload).await;
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "CI preflight: could not build context bundle for required check"
                );
                continue;
            }
        };

        let result = preflight_reproduce(&bundle, workdir, runner).await;
        log_preflight_result(
            task_repo,
            &task.id,
            kind,
            &result,
            &bundle.job_name,
            &bundle.failing_step_name,
        )
        .await;

        if let Some(verdict) = classify_preflight_verdict(kind, &result) {
            return verdict;
        }
    }

    CiPreflightGateVerdict::Allow
}

/// Classify a preflight result into the appropriate gate verdict.
///
/// - `ReproducedFailure` → [`CiPreflightGateVerdict::Block`]: the caller must
///   block submit/approval.
/// - `Unreproducible` → [`CiPreflightGateVerdict::RouteToLeadIntervention`]:
///   the caller must block AND route to lead/human intervention.
/// - `Passed` → `None`: the check is not blocking.
fn classify_preflight_verdict(
    kind: CiPreflightGateKind,
    result: &CiPreflightResult,
) -> Option<CiPreflightGateVerdict> {
    match &result.outcome {
        CiPreflightOutcome::ReproducedFailure {
            command,
            exit_code,
            output,
        } => Some(CiPreflightGateVerdict::Block {
            reason: format!(
                "CI reproduction preflight blocked {}: required check '{}' reproduced locally \
                 at head {} with command `{}` exiting {}. First relevant failure output: {}",
                kind.as_str(),
                result.check_name,
                result.observed_head_sha,
                command,
                exit_code,
                first_line_or_empty(output),
            ),
        }),
        CiPreflightOutcome::Unreproducible { reason, details } => {
            let detail_text = details.as_deref().unwrap_or("no additional detail");
            Some(CiPreflightGateVerdict::RouteToLeadIntervention {
                reason: format!(
                    "CI preflight unreproducible {}: required check '{}' at head {} cannot be \
                     locally reproduced. Reason: {:?}. Detail: {}.",
                    kind.as_str(),
                    result.check_name,
                    result.observed_head_sha,
                    reason,
                    detail_text,
                ),
            })
        }
        CiPreflightOutcome::Passed => None,
    }
}

async fn log_preflight_result(
    task_repo: &TaskRepository,
    task_id: &str,
    kind: CiPreflightGateKind,
    result: &CiPreflightResult,
    job_name: &str,
    step_name: &str,
) {
    let payload = preflight_result_activity_payload(kind, result, job_name, step_name);
    log_preflight_activity(task_repo, task_id, payload).await;
}

/// Build the durable activity payload for a required-check reproduction result.
///
/// Kept as a small pure helper so regression tests can prove the persisted
/// shape contains the repo-derived command, exit code, and failure/error
/// context without needing a live database.
fn preflight_result_activity_payload(
    kind: CiPreflightGateKind,
    result: &CiPreflightResult,
    job_name: &str,
    step_name: &str,
) -> serde_json::Value {
    match &result.outcome {
        CiPreflightOutcome::Passed => serde_json::json!({
            "gate": kind.as_str(),
            "check_name": result.check_name,
            "job": job_name,
            "step": step_name,
            "observed_head_sha": result.observed_head_sha,
            "outcome": "passed",
        }),
        CiPreflightOutcome::ReproducedFailure {
            command,
            exit_code,
            output,
        } => serde_json::json!({
            "gate": kind.as_str(),
            "check_name": result.check_name,
            "job": job_name,
            "step": step_name,
            "command": command,
            "observed_head_sha": result.observed_head_sha,
            "outcome": "reproduced_failure",
            "exit_code": exit_code,
            "first_relevant_failure_output": output,
        }),
        CiPreflightOutcome::Unreproducible { reason, details } => serde_json::json!({
            "gate": kind.as_str(),
            "check_name": result.check_name,
            "job": job_name,
            "step": step_name,
            "observed_head_sha": result.observed_head_sha,
            "outcome": "unreproducible",
            "unreproducible_reason": reason,
            "details": details,
        }),
    }
}

async fn log_preflight_activity(
    task_repo: &TaskRepository,
    task_id: &str,
    payload: serde_json::Value,
) {
    if let Err(e) = task_repo
        .log_activity(
            Some(task_id),
            "coordinator",
            "system",
            CI_PREFLIGHT_REPRODUCTION_EVENT,
            &payload.to_string(),
        )
        .await
    {
        tracing::warn!(
            task_id,
            error = %e,
            "CI preflight: failed to persist structured reproduction activity"
        );
    }
}

fn first_line_or_empty(output: &str) -> &str {
    output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
}

pub(crate) async fn latest_task_workdir(task_id: &str, db: djinn_db::Database) -> Option<PathBuf> {
    let repo = djinn_db::repositories::task_run::TaskRunRepository::new(db);
    match repo.latest_workspace_path_for_task(task_id).await {
        Ok(Some(path)) if !path.trim().is_empty() => Some(PathBuf::from(path)),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(task_id, error = %e, "CI preflight: failed to load latest task workdir");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(status: CiStatus, baseline: Option<&str>, names: Vec<&str>) -> TaskPrCiSnapshot {
        TaskPrCiSnapshot {
            task_id: "task-1".to_string(),
            pr_number: 7,
            head_sha: "abc123".to_string(),
            ci_status: status,
            blocking_required_check_names: names.into_iter().map(str::to_string).collect(),
            failure_fingerprint: Some("fp".to_string()),
            first_seen_at: "now".to_string(),
            last_seen_at: "now".to_string(),
            same_signature_count: 1,
            last_remediation_base_sha: baseline.map(str::to_string),
            merge_queue: None,
        }
    }

    #[test]
    fn implicated_required_checks_selects_currently_failing_required_checks() {
        let snap = snapshot(CiStatus::Failing, None, vec!["Quality Gate", ""]);
        assert_eq!(
            implicated_required_checks(Some(&snap)),
            vec!["Quality Gate"]
        );
    }

    #[test]
    fn implicated_required_checks_selects_previously_failing_remediation_baseline() {
        let snap = snapshot(CiStatus::Pending, Some("abc123"), vec!["CI"]);
        assert_eq!(implicated_required_checks(Some(&snap)), vec!["CI"]);
    }

    #[test]
    fn implicated_required_checks_ignores_passing_without_baseline() {
        let snap = snapshot(CiStatus::Passing, None, vec!["CI"]);
        assert!(implicated_required_checks(Some(&snap)).is_empty());
    }

    #[test]
    fn reproduced_failure_blocks_submit_and_records_clear_reason() {
        let result = CiPreflightResult {
            check_name: "Quality Gate".to_string(),
            observed_head_sha: "abc123".to_string(),
            outcome: CiPreflightOutcome::ReproducedFailure {
                command: "cargo test -p crate".to_string(),
                exit_code: 101,
                output: "assertion failed\nstack".to_string(),
            },
        };
        let verdict = classify_preflight_verdict(CiPreflightGateKind::WorkerSubmit, &result);
        match verdict {
            Some(CiPreflightGateVerdict::Block { reason }) => {
                assert!(reason.contains("worker_submit"));
                assert!(reason.contains("Quality Gate"));
                assert!(reason.contains("cargo test -p crate"));
                assert!(reason.contains("101"));
                assert!(reason.contains("assertion failed"));
            }
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[test]
    fn reproduced_failure_blocks_reviewer_approval_reason() {
        let result = CiPreflightResult {
            check_name: "CI".to_string(),
            observed_head_sha: "def456".to_string(),
            outcome: CiPreflightOutcome::ReproducedFailure {
                command: "pnpm test".to_string(),
                exit_code: 1,
                output: "test failed".to_string(),
            },
        };
        let verdict = classify_preflight_verdict(CiPreflightGateKind::ReviewerApprove, &result);
        match verdict {
            Some(CiPreflightGateVerdict::Block { reason }) => {
                assert!(reason.contains("reviewer_approve"));
                assert!(reason.contains("CI"));
                assert!(reason.contains("pnpm test"));
            }
            other => panic!("expected Block, got {:?}", other),
        }
    }

    #[test]
    fn reproduced_failure_activity_payload_persists_command_exit_and_log_context_for_both_gates() {
        for (kind, expected_gate) in [
            (CiPreflightGateKind::WorkerSubmit, "worker_submit"),
            (CiPreflightGateKind::ReviewerApprove, "reviewer_approve"),
        ] {
            let result = CiPreflightResult {
                check_name: "Quality Gate".to_string(),
                observed_head_sha: "abc123".to_string(),
                outcome: CiPreflightOutcome::ReproducedFailure {
                    command: "./repo-defined-check --strict".to_string(),
                    exit_code: 7,
                    output: "first relevant failure line\nsecond line".to_string(),
                },
            };

            let payload = preflight_result_activity_payload(
                kind,
                &result,
                "Repository Defined Check",
                "Run repo-defined check",
            );

            assert_eq!(payload["gate"], expected_gate);
            assert_eq!(payload["outcome"], "reproduced_failure");
            assert_eq!(payload["check_name"], "Quality Gate");
            assert_eq!(payload["job"], "Repository Defined Check");
            assert_eq!(payload["step"], "Run repo-defined check");
            assert_eq!(payload["command"], "./repo-defined-check --strict");
            assert_eq!(payload["exit_code"], 7);
            assert_eq!(
                payload["first_relevant_failure_output"],
                "first relevant failure line\nsecond line"
            );
            assert_eq!(payload["observed_head_sha"], "abc123");
        }
    }

    #[test]
    fn passing_preflight_does_not_block_existing_flow() {
        let result = CiPreflightResult {
            check_name: "CI".to_string(),
            observed_head_sha: "def456".to_string(),
            outcome: CiPreflightOutcome::Passed,
        };
        assert!(classify_preflight_verdict(CiPreflightGateKind::WorkerSubmit, &result).is_none());
    }

    #[test]
    fn unreproducible_required_check_routes_to_lead_intervention() {
        use crate::ci_reproduction::CiPreflightUnreproducibleReason;
        let result = CiPreflightResult {
            check_name: "Quality Gate".to_string(),
            observed_head_sha: "abc123".to_string(),
            outcome: CiPreflightOutcome::Unreproducible {
                reason: CiPreflightUnreproducibleReason::EmptyCommand,
                details: Some("step script is empty".to_string()),
            },
        };
        let verdict = classify_preflight_verdict(CiPreflightGateKind::WorkerSubmit, &result);
        match verdict {
            Some(CiPreflightGateVerdict::RouteToLeadIntervention { reason }) => {
                assert!(reason.contains("worker_submit"));
                assert!(reason.contains("Quality Gate"));
                assert!(reason.contains("abc123"));
                assert!(reason.contains("EmptyCommand"));
                assert!(reason.contains("step script is empty"));
            }
            other => panic!("expected RouteToLeadIntervention, got {:?}", other),
        }
    }

    #[test]
    fn unreproducible_reviewer_gate_also_routes_to_intervention() {
        use crate::ci_reproduction::CiPreflightUnreproducibleReason;
        let result = CiPreflightResult {
            check_name: "CI".to_string(),
            observed_head_sha: "def456".to_string(),
            outcome: CiPreflightOutcome::Unreproducible {
                reason: CiPreflightUnreproducibleReason::MarketplaceActionInSetup,
                details: Some("uses: actions/checkout@v4".to_string()),
            },
        };
        let verdict = classify_preflight_verdict(CiPreflightGateKind::ReviewerApprove, &result);
        match verdict {
            Some(CiPreflightGateVerdict::RouteToLeadIntervention { reason }) => {
                assert!(reason.contains("reviewer_approve"));
                assert!(reason.contains("CI"));
                assert!(reason.contains("MarketplaceActionInSetup"));
            }
            other => panic!("expected RouteToLeadIntervention, got {:?}", other),
        }
    }

    #[test]
    fn unreproducible_activity_payload_persists_typed_reason_and_is_not_passing_for_both_gates() {
        use crate::ci_reproduction::CiPreflightUnreproducibleReason;

        for (kind, expected_gate) in [
            (CiPreflightGateKind::WorkerSubmit, "worker_submit"),
            (CiPreflightGateKind::ReviewerApprove, "reviewer_approve"),
        ] {
            let result = CiPreflightResult {
                check_name: "Quality Gate".to_string(),
                observed_head_sha: "abc123".to_string(),
                outcome: CiPreflightOutcome::Unreproducible {
                    reason: CiPreflightUnreproducibleReason::MarketplaceActionInSetup,
                    details: Some(
                        "setup step 'Checkout' uses marketplace action 'actions/checkout@v4'"
                            .to_string(),
                    ),
                },
            };

            let payload = preflight_result_activity_payload(
                kind,
                &result,
                "Repository Defined Check",
                "Run repo-defined check",
            );

            assert_eq!(payload["gate"], expected_gate);
            assert_eq!(payload["outcome"], "unreproducible");
            assert_ne!(payload["outcome"], "passed");
            assert_eq!(payload["check_name"], "Quality Gate");
            assert_eq!(payload["job"], "Repository Defined Check");
            assert_eq!(payload["step"], "Run repo-defined check");
            assert_eq!(payload["observed_head_sha"], "abc123");
            assert_eq!(
                payload["unreproducible_reason"],
                "marketplace_action_in_setup"
            );
            assert!(
                payload["details"]
                    .as_str()
                    .expect("details string")
                    .contains("actions/checkout")
            );
        }
    }

    #[test]
    fn unreproducible_verdict_is_not_conflated_with_passing_or_reproduced_failure() {
        use crate::ci_reproduction::CiPreflightUnreproducibleReason;
        // Passing → no verdict
        let passing = CiPreflightResult {
            check_name: "CI".to_string(),
            observed_head_sha: "sha1".to_string(),
            outcome: CiPreflightOutcome::Passed,
        };
        assert!(classify_preflight_verdict(CiPreflightGateKind::WorkerSubmit, &passing).is_none());

        // ReproducedFailure → Block (not RouteToLeadIntervention)
        let reproduced = CiPreflightResult {
            check_name: "CI".to_string(),
            observed_head_sha: "sha1".to_string(),
            outcome: CiPreflightOutcome::ReproducedFailure {
                command: "cargo test".to_string(),
                exit_code: 1,
                output: "fail".to_string(),
            },
        };
        assert!(matches!(
            classify_preflight_verdict(CiPreflightGateKind::WorkerSubmit, &reproduced),
            Some(CiPreflightGateVerdict::Block { .. })
        ));

        // Unreproducible → RouteToLeadIntervention (not Block, not None)
        let unreproducible = CiPreflightResult {
            check_name: "CI".to_string(),
            observed_head_sha: "sha1".to_string(),
            outcome: CiPreflightOutcome::Unreproducible {
                reason: CiPreflightUnreproducibleReason::EmptyCommand,
                details: None,
            },
        };
        assert!(matches!(
            classify_preflight_verdict(CiPreflightGateKind::WorkerSubmit, &unreproducible),
            Some(CiPreflightGateVerdict::RouteToLeadIntervention { .. })
        ));
    }
}
