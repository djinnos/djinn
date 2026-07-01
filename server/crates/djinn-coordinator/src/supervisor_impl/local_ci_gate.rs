use djinn_core::models::TransitionAction;
use djinn_db::TaskRepository;
use djinn_provider::github_api::{
    GitHubApiClient, RequiredCheckReproduction, RequiredCheckUnreproducible,
    RequiredCheckUnreproducibleReason,
};
use djinn_runtime::spec::TaskRunOutcome;
use djinn_workspace::MirrorManager;

use crate::local_gates::{LocalGateResult, LocalGateUnreproducibleReason, reproduce_ci_checks};

pub(crate) const LOCAL_CI_GATE_EVENT: &str = "local_ci_gate_result";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LocalGateBlockKind {
    ReproducedFailure,
    Unreproducible,
}

pub(crate) fn local_gate_block_kind(results: &[LocalGateResult]) -> Option<LocalGateBlockKind> {
    if results
        .iter()
        .any(|result| matches!(result, LocalGateResult::Unreproducible(_)))
    {
        return Some(LocalGateBlockKind::Unreproducible);
    }
    if results
        .iter()
        .any(|result| matches!(result, LocalGateResult::ReproducedFailure(_)))
    {
        return Some(LocalGateBlockKind::ReproducedFailure);
    }
    None
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_required_ci_local_gate_before_pr_open(
    task: &djinn_core::models::Task,
    task_repo: &TaskRepository,
    mirror: &MirrorManager,
    github_client: &GitHubApiClient,
    owner: &str,
    repo_name: &str,
    project_id: &str,
    task_branch: &str,
) -> Option<TaskRunOutcome> {
    let observed_head_sha = task
        .ci_head_sha
        .as_deref()
        .or(task.ci_last_remediation_base_sha.as_deref())?;
    let required_check_names = implicated_required_check_names(task);
    if required_check_names.is_empty() {
        return None;
    }

    let bundles = fetch_required_check_bundles(
        github_client,
        owner,
        repo_name,
        observed_head_sha,
        &required_check_names,
    )
    .await;

    let workspace = match mirror.clone_ephemeral(project_id, task_branch).await {
        Ok(workspace) => workspace,
        Err(e) => {
            let results = crate::local_gates::unreproducible_results_for_checks(
                &required_check_names,
                observed_head_sha,
                LocalGateUnreproducibleReason::CommandSpawnFailed,
                Some(format!("could not materialize task worktree: {e}")),
            );
            persist_local_gate_results(task, task_repo, &results).await;
            return Some(route_local_gate_block(task, task_repo, &results).await);
        }
    };

    let results = reproduce_ci_checks(&bundles, &workspace.path_buf()).await;
    persist_local_gate_results(task, task_repo, &results).await;

    if local_gate_block_kind(&results).is_some() {
        Some(route_local_gate_block(task, task_repo, &results).await)
    } else {
        None
    }
}

pub(crate) async fn fetch_required_check_bundles(
    github_client: &GitHubApiClient,
    owner: &str,
    repo_name: &str,
    observed_head_sha: &str,
    required_check_names: &[String],
) -> Vec<RequiredCheckReproduction> {
    let mut bundles = Vec::with_capacity(required_check_names.len());
    for check_name in required_check_names {
        match github_client
            .required_check_reproduction_context(owner, repo_name, observed_head_sha, check_name)
            .await
        {
            Ok(bundle) => bundles.push(bundle),
            Err(e) => bundles.push(RequiredCheckReproduction::Unreproducible(
                RequiredCheckUnreproducible {
                    required_check_name: check_name.clone(),
                    observed_head_sha: observed_head_sha.to_owned(),
                    reason: RequiredCheckUnreproducibleReason::WorkflowRunNotFound,
                    details: Some(format!(
                        "provider error while fetching reproduction bundle: {e}"
                    )),
                },
            )),
        }
    }
    bundles
}

pub(crate) fn implicated_required_check_names(task: &djinn_core::models::Task) -> Vec<String> {
    serde_json::from_str::<Vec<String>>(&task.ci_blocking_required_check_names)
        .unwrap_or_default()
        .into_iter()
        .filter(|name| !name.trim().is_empty())
        .collect()
}

pub(crate) async fn persist_local_gate_results(
    task: &djinn_core::models::Task,
    task_repo: &TaskRepository,
    results: &[LocalGateResult],
) {
    for result in results {
        let payload = serde_json::json!({
            "task_id": task.id,
            "short_id": task.short_id,
            "pr_number": task.ci_pr_number,
            "result": result,
        });
        if let Err(e) = task_repo
            .log_activity(
                Some(&task.id),
                "coordinator",
                "system",
                LOCAL_CI_GATE_EVENT,
                &payload.to_string(),
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "local CI gate: failed to persist result"
            );
        }
    }
}

pub(crate) async fn route_local_gate_block(
    task: &djinn_core::models::Task,
    task_repo: &TaskRepository,
    results: &[LocalGateResult],
) -> TaskRunOutcome {
    let Some(kind) = local_gate_block_kind(results) else {
        return TaskRunOutcome::Escalated {
            reason: "local CI gate had no blocking result".to_owned(),
        };
    };
    let reason = local_gate_block_reason(kind, results);
    if let Err(e) = task_repo
        .transition(
            &task.id,
            TransitionAction::ParkForRemediation,
            "coordinator",
            "system",
            Some(&reason),
            None,
        )
        .await
    {
        tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "local CI gate: remediation park transition skipped"
        );
    }

    if kind == LocalGateBlockKind::Unreproducible
        && let Err(e) = task_repo
            .transition(
                &task.id,
                TransitionAction::Escalate,
                "coordinator",
                "system",
                Some(&reason),
                None,
            )
            .await
    {
        tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "local CI gate: unreproducible lead escalation skipped"
        );
    }
    let comment_payload = serde_json::json!({
        "body": format!(
            "**Local required-CI reproduction gate blocked submit/approval**\n\n{reason}"
        )
    });
    let _ = task_repo
        .log_activity(
            Some(&task.id),
            "coordinator",
            "system",
            "comment",
            &comment_payload.to_string(),
        )
        .await;
    TaskRunOutcome::Escalated { reason }
}

pub(crate) fn local_gate_block_reason(
    kind: LocalGateBlockKind,
    results: &[LocalGateResult],
) -> String {
    match kind {
        LocalGateBlockKind::ReproducedFailure => {
            let failures: Vec<String> = results
                .iter()
                .filter_map(|result| match result {
                    LocalGateResult::ReproducedFailure(outcome) => Some(format!(
                        "{}: `{}` exited {}\n{}",
                        outcome.required_check_name,
                        outcome.command,
                        outcome.exit_code,
                        outcome.log_tail
                    )),
                    _ => None,
                })
                .collect();
            format!(
                "Required CI reproduced locally and failed. Submit/approval is blocked; fix the reproduced failure and resubmit.\n\n{}",
                failures.join("\n\n")
            )
        }
        LocalGateBlockKind::Unreproducible => {
            let failures: Vec<String> = results
                .iter()
                .filter_map(|result| match result {
                    LocalGateResult::Unreproducible(unreproducible) => Some(format!(
                        "{} at {}: {:?}{}",
                        unreproducible.required_check_name,
                        unreproducible.observed_head_sha,
                        unreproducible.reason,
                        unreproducible
                            .details
                            .as_ref()
                            .map(|details| format!(" — {details}"))
                            .unwrap_or_default()
                    )),
                    _ => None,
                })
                .collect();
            format!(
                "A required CI check could not be reproduced locally, so it is not treated as passing. Routing to lead/human intervention.\n\n{}",
                failures.join("\n")
            )
        }
    }
}
