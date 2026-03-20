use std::sync::Arc;

use djinn_core::models::TransitionAction;
use djinn_provider::github_api::{CheckRun, GitHubApiClient, PrState};
use djinn_provider::repos::CredentialRepository;

use super::*;

impl CoordinatorActor {
    /// Poll GitHub for PR status on all tasks in the `pr_ready` state.
    ///
    /// Runs on every 30-second tick.  Returns immediately when no `pr_ready`
    /// tasks exist so no GitHub API calls are made during idle periods.
    ///
    /// On each task:
    /// - **Merged PR** → `PrMerge` transition (pr_ready → closed), unblocking dependents.
    ///   Dependent tasks that were blocked by this task are automatically unblocked via the
    ///   event system (`emit_unblocked_tasks` fires on close, coordinator dispatches them).
    /// - **CI check failure** → `PrChangesRequested` (pr_ready → open) for agent rework.
    ///   CI failure details are logged as a task comment so the worker has context on re-dispatch.
    ///   CI results are cached by head SHA so unchanged commits are not re-checked.
    /// - **PR closed without merge** → `ForceClose` (pr_ready → closed/force_closed).
    ///   The PR was manually closed without merging; task is force-closed.
    /// - **Changes requested review** → `PrChangesRequested` (pr_ready → open).
    pub(super) async fn poll_pr_statuses(&mut self) {
        let task_repo = self.task_repo();
        let pr_ready_tasks = match task_repo.list_by_status("pr_ready").await {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "PR poller: failed to query pr_ready tasks");
                return;
            }
        };

        // Only poll when there are open PRs to check — no idle API calls.
        let tasks_with_pr: Vec<_> = pr_ready_tasks
            .into_iter()
            .filter(|t| t.pr_url.is_some())
            .collect();

        if tasks_with_pr.is_empty() {
            return;
        }

        tracing::debug!(
            count = tasks_with_pr.len(),
            "PR poller: checking {} pr_ready task(s)",
            tasks_with_pr.len()
        );

        let cred_repo = Arc::new(CredentialRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        ));
        let gh_client = GitHubApiClient::new(cred_repo);

        for task in tasks_with_pr {
            let pr_url = task.pr_url.as_deref().unwrap();
            let Some((owner, repo, pull_number)) = parse_pr_url(pr_url) else {
                tracing::warn!(
                    task_id = %task.short_id,
                    pr_url,
                    "PR poller: unrecognised PR URL format, skipping"
                );
                continue;
            };

            // Fetch current PR state + CI check runs.
            let (pr, checks) =
                match gh_client.get_pull_request(&owner, &repo, pull_number).await {
                    Ok(result) => result,
                    Err(e) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            error = %e,
                            "PR poller: failed to fetch PR status"
                        );
                        continue;
                    }
                };

            // ── Merged? ───────────────────────────────────────────────────────
            if pr.merged == Some(true) {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR merged → closing task"
                );
                self.apply_pr_transition(&task.id, TransitionAction::PrMerge, None)
                    .await;
                self.pr_status_cache.remove(&task.id);
                continue;
            }

            // PR is closed but not merged (e.g. manually closed without merge).
            // Force-close the task — it cannot be merged via this PR anymore.
            if pr.state == PrState::Closed {
                tracing::info!(
                    task_id = %task.short_id,
                    pr = pull_number,
                    "PR poller: PR closed without merge → force-closing task"
                );
                self.apply_pr_transition(
                    &task.id,
                    TransitionAction::ForceClose,
                    Some("PR was closed without merging"),
                )
                .await;
                self.pr_status_cache.remove(&task.id);
                continue;
            }

            // ── CI checks (cached per head SHA) ───────────────────────────────
            let current_sha = pr.head.sha.clone();
            let sha_changed = self
                .pr_status_cache
                .get(&task.id)
                .map(|cached| cached != &current_sha)
                .unwrap_or(true);

            if sha_changed {
                // Update cache before any early-continue so subsequent ticks
                // don't re-evaluate the same SHA.
                self.pr_status_cache
                    .insert(task.id.clone(), current_sha.clone());

                if !checks.check_runs.is_empty() {
                    let failed_checks: Vec<&CheckRun> = checks
                        .check_runs
                        .iter()
                        .filter(|cr| {
                            matches!(
                                cr.conclusion.as_deref(),
                                Some("failure") | Some("timed_out") | Some("cancelled")
                            )
                        })
                        .collect();

                    if !failed_checks.is_empty() {
                        tracing::info!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            sha = %current_sha,
                            failed_count = failed_checks.len(),
                            "PR poller: CI check failed → reopening task for rework"
                        );
                        self.apply_pr_transition(
                            &task.id,
                            TransitionAction::PrChangesRequested,
                            Some("CI checks failed on PR"),
                        )
                        .await;
                        // Log CI failure details as a comment so the re-dispatched worker
                        // has full context on which checks failed and where to look.
                        self.log_ci_failure_comment(&task.id, &failed_checks, pr_url, &current_sha)
                            .await;
                        self.pr_status_cache.remove(&task.id);
                        continue;
                    }
                }
            }

            // ── Review state ──────────────────────────────────────────────────
            match gh_client
                .list_pr_review_states(&owner, &repo, pull_number)
                .await
            {
                Ok(reviews) => {
                    // Only the most recent review per reviewer counts.  If the
                    // latest review from any reviewer is CHANGES_REQUESTED, reopen.
                    let changes_requested = reviews
                        .iter()
                        .any(|r| r.state.as_str() == "CHANGES_REQUESTED");

                    if changes_requested {
                        tracing::info!(
                            task_id = %task.short_id,
                            pr = pull_number,
                            "PR poller: reviewer requested changes → reopening task"
                        );
                        self.apply_pr_transition(
                            &task.id,
                            TransitionAction::PrChangesRequested,
                            Some("Reviewer requested changes on PR"),
                        )
                        .await;
                        self.pr_status_cache.remove(&task.id);
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "PR poller: failed to fetch PR reviews, will retry next tick"
                    );
                }
            }
        }
    }

    async fn apply_pr_transition(
        &self,
        task_id: &str,
        action: TransitionAction,
        reason: Option<&str>,
    ) {
        let task_repo = self.task_repo();
        if let Err(e) = task_repo
            .transition(task_id, action, "system", "pr_poller", reason, None)
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to apply task transition"
            );
        }
    }

    /// Log a comment on the task with details about which CI checks failed.
    ///
    /// This comment becomes part of the activity log that the re-dispatched worker
    /// reads in its system prompt, giving it context about what needs to be fixed.
    async fn log_ci_failure_comment(
        &self,
        task_id: &str,
        failed_checks: &[&CheckRun],
        pr_url: &str,
        sha: &str,
    ) {
        let check_lines: Vec<String> = failed_checks
            .iter()
            .map(|cr| {
                let conclusion = cr.conclusion.as_deref().unwrap_or("unknown");
                format!("- **{}** ({}): {}", cr.name, conclusion, cr.html_url)
            })
            .collect();

        let body = format!(
            "**CI checks failed on PR** (commit `{sha}`)\n\n\
             The following CI checks failed. Review the logs and fix the failures before the PR can merge.\n\n\
             {checks}\n\n\
             PR: {pr_url}",
            sha = &sha[..sha.len().min(12)],
            checks = check_lines.join("\n"),
            pr_url = pr_url,
        );

        let payload = serde_json::json!({ "body": body }).to_string();
        let task_repo = self.task_repo();
        if let Err(e) = task_repo
            .log_activity(Some(task_id), "pr_poller", "system", "comment", &payload)
            .await
        {
            tracing::warn!(
                task_id,
                error = %e,
                "PR poller: failed to log CI failure comment"
            );
        }
    }
}

/// Parse a GitHub PR URL into `(owner, repo, pull_number)`.
///
/// Handles URLs of the form `https://github.com/{owner}/{repo}/pull/{number}`.
fn parse_pr_url(url: &str) -> Option<(String, String, u64)> {
    let path = url.strip_prefix("https://github.com/")?;
    let mut parts = path.splitn(5, '/');
    let owner = parts.next()?;
    let repo = parts.next()?;
    let segment = parts.next()?;
    if segment != "pull" {
        return None;
    }
    let number_str = parts.next()?;
    // Strip any trailing fragment/query.
    let number_str = number_str.split(&['?', '#'][..]).next()?;
    let number: u64 = number_str.parse().ok()?;
    Some((owner.to_string(), repo.to_string(), number))
}

#[cfg(test)]
mod tests {
    use super::parse_pr_url;

    #[test]
    fn parses_standard_pr_url() {
        let result = parse_pr_url("https://github.com/djinnos/server/pull/42");
        assert_eq!(
            result,
            Some(("djinnos".to_string(), "server".to_string(), 42))
        );
    }

    #[test]
    fn parses_pr_url_with_trailing_fragment() {
        let result = parse_pr_url("https://github.com/owner/repo/pull/7#discussion");
        assert_eq!(
            result,
            Some(("owner".to_string(), "repo".to_string(), 7))
        );
    }

    #[test]
    fn rejects_non_pr_url() {
        assert_eq!(
            parse_pr_url("https://github.com/owner/repo/issues/1"),
            None
        );
    }

    #[test]
    fn rejects_non_github_url() {
        assert_eq!(parse_pr_url("https://gitlab.com/owner/repo/pull/1"), None);
    }
}
