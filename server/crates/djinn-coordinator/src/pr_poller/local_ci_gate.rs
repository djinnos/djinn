use djinn_core::models::Task;
use djinn_provider::github_api::{
    GitHubApiClient, RequiredCheckReproduction, RequiredCheckUnreproducible,
    RequiredCheckUnreproducibleReason,
};

use super::*;

/// Run the repo-derived local CI gate before reviewer approval can advance.
///
/// Fetches reproduction bundles for each implicated required check from the
/// provider, runs the repo-derived command in an ephemeral worktree, and
/// persists each result as task activity. Returns `Some(())` when the gate
/// blocks (reproduced failure or unreproducible), `None` when the gate passes
/// or no implicated checks exist.
pub(crate) async fn run_local_gate_before_reviewer_approval(
    actor: &CoordinatorActor,
    task: &Task,
    gh_client: &GitHubApiClient,
    owner: &str,
    repo_name: &str,
    current_sha: &str,
) -> Option<()> {
    use crate::local_gates::reproduce_ci_checks;
    use crate::supervisor_impl::local_ci_gate::{
        implicated_required_check_names, local_gate_block_kind, persist_local_gate_results,
        route_local_gate_block,
    };

    let required_check_names = implicated_required_check_names(task);
    if required_check_names.is_empty() {
        return None;
    }

    let mut bundles = Vec::with_capacity(required_check_names.len());
    for check_name in &required_check_names {
        match gh_client
            .required_check_reproduction_context(owner, repo_name, current_sha, check_name)
            .await
        {
            Ok(bundle) => bundles.push(bundle),
            Err(e) => bundles.push(RequiredCheckReproduction::Unreproducible(
                RequiredCheckUnreproducible {
                    required_check_name: check_name.clone(),
                    observed_head_sha: current_sha.to_owned(),
                    reason: RequiredCheckUnreproducibleReason::WorkflowRunNotFound,
                    details: Some(format!(
                        "provider error while fetching reproduction bundle: {e}"
                    )),
                },
            )),
        }
    }

    let task_repo = actor.task_repo();
    let task_branch = format!("task/{}", task.short_id);
    let mirror = match actor.mirror.as_ref() {
        Some(m) => m,
        None => {
            let results = crate::local_gates::unreproducible_results_for_checks(
                &required_check_names,
                current_sha,
                crate::local_gates::LocalGateUnreproducibleReason::CommandSpawnFailed,
                Some("coordinator has no mirror manager".to_owned()),
            );
            persist_local_gate_results(task, &task_repo, &results).await;
            let _ = route_local_gate_block(task, &task_repo, &results).await;
            return Some(());
        }
    };

    let workspace = match mirror.clone_ephemeral(&task.project_id, &task_branch).await {
        Ok(ws) => ws,
        Err(e) => {
            let results = crate::local_gates::unreproducible_results_for_checks(
                &required_check_names,
                current_sha,
                crate::local_gates::LocalGateUnreproducibleReason::CommandSpawnFailed,
                Some(format!("could not materialize task worktree: {e}")),
            );
            persist_local_gate_results(task, &task_repo, &results).await;
            let _ = route_local_gate_block(task, &task_repo, &results).await;
            return Some(());
        }
    };

    let results = reproduce_ci_checks(&bundles, &workspace.path_buf()).await;
    persist_local_gate_results(task, &task_repo, &results).await;

    if local_gate_block_kind(&results).is_some() {
        let _ = route_local_gate_block(task, &task_repo, &results).await;
        Some(())
    } else {
        None
    }
}
