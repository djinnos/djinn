use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use djinn_core::models::Task;
use djinn_provider::github_api::{GitHubApiClient, PrMergeQueueState, PullRequest};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::*;

const DEFAULT_GRACE_PERIOD_SECS: u64 = 600;

/// GitHub operations needed by PR/branch cleanup guardrails.
///
/// This trait keeps [`PrCleanupPolicy`] unit-testable while the production impl
/// delegates to `GitHubApiClient`.
#[async_trait]
pub(crate) trait PrCleanupGitHub: Send + Sync {
    async fn get_pr_merge_queue_state(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PrMergeQueueState>;

    async fn list_pulls_by_base(
        &self,
        owner: &str,
        repo: &str,
        base: &str,
    ) -> Result<Vec<PullRequest>>;

    async fn delete_ref(&self, owner: &str, repo: &str, ref_name: &str) -> Result<()>;
}

#[async_trait]
impl PrCleanupGitHub for GitHubApiClient {
    async fn get_pr_merge_queue_state(
        &self,
        owner: &str,
        repo: &str,
        pull_number: u64,
    ) -> Result<PrMergeQueueState> {
        GitHubApiClient::get_pr_merge_queue_state(self, owner, repo, pull_number).await
    }

    async fn list_pulls_by_base(
        &self,
        owner: &str,
        repo: &str,
        base: &str,
    ) -> Result<Vec<PullRequest>> {
        GitHubApiClient::list_pulls_by_base(self, owner, repo, base).await
    }

    async fn delete_ref(&self, owner: &str, repo: &str, ref_name: &str) -> Result<()> {
        GitHubApiClient::delete_ref(self, owner, repo, ref_name).await
    }
}

/// Configuration for shared PR/branch cleanup guardrails.
#[derive(Debug, Clone)]
pub(crate) struct PrCleanupPolicyConfig {
    pub enabled: bool,
    pub dry_run: bool,
    pub grace_period: Duration,
    pub owner: String,
    pub repo: String,
    pub bot_logins: HashSet<String>,
    pub protected_branches: HashSet<String>,
    pub allowed_branch_prefixes: Vec<String>,
}

impl PrCleanupPolicyConfig {
    pub(crate) fn new(owner: impl Into<String>, repo: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
            repo: repo.into(),
            ..Self::default()
        }
    }
}

impl Default for PrCleanupPolicyConfig {
    fn default() -> Self {
        let mut bot_logins = HashSet::new();
        if let Some(slug) = djinn_provider::github_app::app_slug() {
            bot_logins.insert(format!("{slug}[bot]"));
        }
        bot_logins.insert("djinn-bot[bot]".to_string());

        let mut protected_branches = HashSet::new();
        protected_branches.insert("main".to_string());

        Self {
            enabled: true,
            dry_run: false,
            grace_period: Duration::from_secs(DEFAULT_GRACE_PERIOD_SECS),
            owner: String::new(),
            repo: String::new(),
            bot_logins,
            protected_branches,
            allowed_branch_prefixes: vec!["task/".to_string(), "chore/".to_string()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BranchCleanupOutcome {
    Skipped,
    DryRunWouldDelete,
    Deleted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseKind {
    Merge,
    NonMerge,
}

/// Shared cleanup guardrail policy for inline cleanup and periodic sweeps.
#[derive(Debug, Clone)]
pub(crate) struct PrCleanupPolicy<C> {
    github: C,
    config: PrCleanupPolicyConfig,
    now_override: Option<OffsetDateTime>,
}

impl<C> PrCleanupPolicy<C> {
    pub(crate) fn new(github: C, config: PrCleanupPolicyConfig) -> Self {
        Self {
            github,
            config,
            now_override: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_now(github: C, config: PrCleanupPolicyConfig, now: OffsetDateTime) -> Self {
        Self {
            github,
            config,
            now_override: Some(now),
        }
    }

    pub(crate) fn config(&self) -> &PrCleanupPolicyConfig {
        &self.config
    }
}

impl<C: PrCleanupGitHub> PrCleanupPolicy<C> {
    /// Return true when it is safe and intended to clean up the PR.
    ///
    /// Emits `djinn_inline_cleanup_skipped_total` metrics for each skip reason
    /// and `djinn_inline_pr_closed_total` when the PR would be closed.
    pub(crate) async fn should_cleanup_pr(&self, task: &Task, pr: &PullRequest) -> Result<bool> {
        if !self.config.enabled {
            tracing::info!(task_id = %task.short_id, pr = pr.number, "PR cleanup disabled; skipping PR cleanup");
            djinn_telemetry::inline_cleanup::increment_skipped(
                djinn_telemetry::inline_cleanup::REASON_CONFIG_DISABLED,
            );
            return Ok(false);
        }

        if self.within_grace_period(task)? {
            tracing::info!(task_id = %task.short_id, pr = pr.number, "PR cleanup skipped during grace period");
            djinn_telemetry::inline_cleanup::increment_skipped(
                djinn_telemetry::inline_cleanup::REASON_GRACE_PERIOD,
            );
            return Ok(false);
        }

        let Some(user) = pr.user.as_ref() else {
            tracing::warn!(task_id = %task.short_id, pr = pr.number, "PR cleanup skipped because PR author is unavailable");
            djinn_telemetry::inline_cleanup::increment_skipped(
                djinn_telemetry::inline_cleanup::REASON_BOT_AUTHOR,
            );
            return Ok(false);
        };
        if !self.config.bot_logins.contains(&user.login) {
            tracing::info!(
                task_id = %task.short_id,
                pr = pr.number,
                author = %user.login,
                "PR cleanup skipped for human/non-bot-authored PR"
            );
            djinn_telemetry::inline_cleanup::increment_skipped(
                djinn_telemetry::inline_cleanup::REASON_BOT_AUTHOR,
            );
            return Ok(false);
        }

        let merge_queue_state = self
            .github
            .get_pr_merge_queue_state(&self.config.owner, &self.config.repo, pr.number)
            .await?;
        if let Some(entry) = merge_queue_state.merge_queue_entry {
            tracing::info!(
                task_id = %task.short_id,
                pr = pr.number,
                state = ?entry.state,
                "PR cleanup skipped because PR is in the merge queue"
            );
            djinn_telemetry::inline_cleanup::increment_skipped(
                djinn_telemetry::inline_cleanup::REASON_MERGE_QUEUE,
            );
            return Ok(false);
        }

        if self.config.dry_run {
            tracing::info!(
                task_id = %task.short_id,
                pr = pr.number,
                "dry_run: would close PR #{} for task {}",
                pr.number,
                task.short_id
            );
            djinn_telemetry::inline_cleanup::increment_skipped(
                djinn_telemetry::inline_cleanup::REASON_DRY_RUN,
            );
            return Ok(false);
        }

        Ok(true)
    }

    /// Return true when it is safe and intended to delete `branch`.
    ///
    /// Emits `djinn_inline_cleanup_skipped_total` metrics for each skip reason.
    pub(crate) async fn should_delete_branch(&self, task: &Task, branch: &str) -> Result<bool> {
        if !self.config.enabled {
            tracing::info!(task_id = %task.short_id, branch, "PR cleanup disabled; skipping branch deletion");
            djinn_telemetry::inline_cleanup::increment_skipped(
                djinn_telemetry::inline_cleanup::REASON_CONFIG_DISABLED,
            );
            return Ok(false);
        }

        if self.within_grace_period(task)? {
            tracing::info!(task_id = %task.short_id, branch, "Branch cleanup skipped during grace period");
            djinn_telemetry::inline_cleanup::increment_skipped(
                djinn_telemetry::inline_cleanup::REASON_GRACE_PERIOD,
            );
            return Ok(false);
        }

        let branch = normalize_branch_name(branch);
        if self.config.protected_branches.contains(branch) {
            tracing::info!(task_id = %task.short_id, branch, "Branch cleanup skipped for protected branch");
            djinn_telemetry::inline_cleanup::increment_skipped(
                djinn_telemetry::inline_cleanup::REASON_PROTECTED_BRANCH,
            );
            return Ok(false);
        }

        if !self.config.allowed_branch_prefixes.is_empty()
            && !self
                .config
                .allowed_branch_prefixes
                .iter()
                .any(|prefix| branch.starts_with(prefix))
        {
            tracing::info!(task_id = %task.short_id, branch, "Branch cleanup skipped for non-bot branch prefix");
            djinn_telemetry::inline_cleanup::increment_skipped(
                djinn_telemetry::inline_cleanup::REASON_BOT_AUTHOR,
            );
            return Ok(false);
        }

        let dependent_prs = self
            .github
            .list_pulls_by_base(&self.config.owner, &self.config.repo, branch)
            .await?;
        if dependent_prs.iter().any(|pr| pr.base.ref_name == branch) {
            tracing::info!(
                task_id = %task.short_id,
                branch,
                dependent_pr_count = dependent_prs.len(),
                "Branch cleanup skipped because branch is the base of another open PR"
            );
            djinn_telemetry::inline_cleanup::increment_skipped(
                djinn_telemetry::inline_cleanup::REASON_BASE_OF_PR,
            );
            return Ok(false);
        }

        if self.config.dry_run {
            tracing::info!(
                task_id = %task.short_id,
                branch,
                "dry_run: would delete branch task/{}",
                task.short_id
            );
        }

        Ok(true)
    }

    /// Delete `branch` when guardrails allow it; dry-run mode logs but does not
    /// call GitHub.
    ///
    /// Emits `djinn_inline_branch_deleted_total` when a branch is successfully
    /// deleted and `djinn_inline_cleanup_skipped_total{reason="dry_run"}` when
    /// the deletion is skipped due to dry-run mode.
    pub(crate) async fn delete_branch_if_allowed(
        &self,
        task: &Task,
        branch: &str,
    ) -> Result<BranchCleanupOutcome> {
        if !self.should_delete_branch(task, branch).await? {
            return Ok(BranchCleanupOutcome::Skipped);
        }

        let branch = normalize_branch_name(branch);
        if self.config.dry_run {
            tracing::info!(task_id = %task.short_id, branch, "dry_run: not deleting branch");
            djinn_telemetry::inline_cleanup::increment_skipped(
                djinn_telemetry::inline_cleanup::REASON_DRY_RUN,
            );
            return Ok(BranchCleanupOutcome::DryRunWouldDelete);
        }

        self.github
            .delete_ref(
                &self.config.owner,
                &self.config.repo,
                &format!("heads/{branch}"),
            )
            .await?;
        djinn_telemetry::inline_cleanup::increment_branch_deleted();
        Ok(BranchCleanupOutcome::Deleted)
    }

    fn within_grace_period(&self, task: &Task) -> Result<bool> {
        let timestamp = task.closed_at.as_deref().unwrap_or(&task.updated_at);
        let closed_or_updated_at = OffsetDateTime::parse(timestamp, &Rfc3339).map_err(|e| {
            anyhow!("failed to parse task close/update timestamp {timestamp:?}: {e}")
        })?;
        let grace_period = time::Duration::try_from(self.config.grace_period)
            .map_err(|e| anyhow!("invalid PR cleanup grace period: {e}"))?;
        Ok(self.now() < closed_or_updated_at + grace_period)
    }

    fn now(&self) -> OffsetDateTime {
        match self.now_override {
            Some(now) => now,
            None => OffsetDateTime::now_utc(),
        }
    }
}

fn normalize_branch_name(branch: &str) -> &str {
    branch
        .strip_prefix("refs/heads/")
        .or_else(|| branch.strip_prefix("heads/"))
        .unwrap_or(branch)
}

impl CoordinatorActor {
    pub(crate) async fn cleanup_pr_and_branch_on_close(&self, task: &Task, close_kind: CloseKind) {
        let Some(pr_url) = task
            .pr_url
            .as_deref()
            .map(str::trim)
            .filter(|url| !url.is_empty())
        else {
            return;
        };
        let Some((owner, repo, pull_number)) = parse_pr_url(pr_url) else {
            tracing::warn!(
                task_id = %task.short_id,
                pr_url,
                "inline PR cleanup: skipping cleanup for unparseable PR URL"
            );
            return;
        };

        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let project_repo = djinn_db::ProjectRepository::new(self.db.clone(), event_bus.clone());
        let Some(github) = resolve_installation_client(&project_repo, &task.project_id).await
        else {
            tracing::info!(
                task_id = %task.short_id,
                project_id = %task.project_id,
                "inline PR cleanup: skipping cleanup because no GitHub installation client is available"
            );
            return;
        };

        let config = PrCleanupPolicyConfig::new(owner.clone(), repo.clone());
        let policy = PrCleanupPolicy::new(github, config);
        let task_branch = format!("task/{}", task.short_id);

        if close_kind == CloseKind::NonMerge {
            match policy
                .github
                .get_pull_request(&owner, &repo, pull_number)
                .await
            {
                Ok((pr, _checks)) => {
                    if pr.state == PrState::Open {
                        match policy.should_cleanup_pr(task, &pr).await {
                            Ok(true) if policy.config().dry_run => {
                                self.log_inline_cleanup_activity(
                                    task,
                                    serde_json::json!({
                                        "kind": "non_merge",
                                        "action": "dry_run_would_close_pr",
                                        "pr_url": pr_url,
                                        "pull_number": pull_number,
                                    }),
                                )
                                .await;
                            }
                            Ok(true) => {
                                let comment = format!(
                                    "Djinn is closing this PR because task `{}` has been terminally closed without merging. The task branch will be cleaned up if no dependent PRs use it.",
                                    task.short_id
                                );
                                if let Err(e) = policy
                                    .github
                                    .create_pr_comment(&owner, &repo, pull_number, &comment)
                                    .await
                                {
                                    tracing::warn!(
                                        task_id = %task.short_id,
                                        pull_number,
                                        error = %e,
                                        "inline PR cleanup: failed to add PR close audit comment"
                                    );
                                }
                                match policy
                                    .github
                                    .close_pull_request(&owner, &repo, pull_number)
                                    .await
                                {
                                    Ok(_) => {
                                        tracing::info!(
                                            task_id = %task.short_id,
                                            pull_number,
                                            "inline PR cleanup: closed PR for terminal non-merge task close"
                                        );
                                        self.log_inline_cleanup_activity(
                                            task,
                                            serde_json::json!({
                                                "kind": "non_merge",
                                                "action": "closed_pr",
                                                "pr_url": pr_url,
                                                "pull_number": pull_number,
                                            }),
                                        )
                                        .await;
                                    }
                                    Err(e) => {
                                        tracing::warn!(
                                            task_id = %task.short_id,
                                            pull_number,
                                            error = %e,
                                            "inline PR cleanup: failed to close PR"
                                        );
                                    }
                                }
                            }
                            Ok(false) => {}
                            Err(e) => {
                                tracing::warn!(
                                    task_id = %task.short_id,
                                    pull_number,
                                    error = %e,
                                    "inline PR cleanup: PR cleanup guardrail check failed"
                                );
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        owner = %owner,
                        repo = %repo,
                        pull_number,
                        error = %e,
                        "inline PR cleanup: failed to fetch PR before close cleanup"
                    );
                }
            }
        }

        match policy.delete_branch_if_allowed(task, &task_branch).await {
            Ok(BranchCleanupOutcome::Deleted) => {
                self.log_inline_cleanup_activity(
                    task,
                    serde_json::json!({
                        "kind": match close_kind {
                            CloseKind::Merge => "merge",
                            CloseKind::NonMerge => "non_merge",
                        },
                        "action": "deleted_branch",
                        "branch": task_branch,
                    }),
                )
                .await;
            }
            Ok(BranchCleanupOutcome::DryRunWouldDelete) => {
                self.log_inline_cleanup_activity(
                    task,
                    serde_json::json!({
                        "kind": match close_kind {
                            CloseKind::Merge => "merge",
                            CloseKind::NonMerge => "non_merge",
                        },
                        "action": "dry_run_would_delete_branch",
                        "branch": task_branch,
                    }),
                )
                .await;
            }
            Ok(BranchCleanupOutcome::Skipped) => {}
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    branch = %task_branch,
                    error = %e,
                    "inline PR cleanup: branch cleanup failed"
                );
            }
        }
    }

    async fn log_inline_cleanup_activity(&self, task: &Task, payload: serde_json::Value) {
        let event_bus = crate::events::event_bus_for(&self.events_tx);
        let task_repo = djinn_db::TaskRepository::new(self.db.clone(), event_bus);
        if let Err(e) = task_repo
            .log_activity(
                Some(&task.id),
                "system",
                "coordinator",
                "pr_branch_cleanup",
                &payload.to_string(),
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "inline PR cleanup: failed to log cleanup activity"
            );
        }
    }
}
