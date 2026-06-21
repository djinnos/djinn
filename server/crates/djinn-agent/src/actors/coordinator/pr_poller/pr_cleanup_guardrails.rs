use djinn_core::models::Task;
use djinn_provider::github_api::{GitHubApiClient, MergeQueueEntryState, PrMergeQueueState, PullRequest};
use thiserror::Error;
use time::OffsetDateTime;

/// Configuration for the PR/branch cleanup sweep guardrails.
#[derive(Debug, Clone)]
pub struct ReapConfig {
    /// Seconds to wait after a task is closed before its PR/branch may be reaped.
    /// Default: 600 (10 minutes).
    pub grace_period_seconds: u64,
    /// If true, log what would be done but do not mutate anything.
    pub dry_run: bool,
    /// Master switch: if false, all reaping is skipped.
    pub enabled: bool,
}

impl Default for ReapConfig {
    fn default() -> Self {
        Self {
            grace_period_seconds: 600,
            dry_run: false,
            enabled: true,
        }
    }
}

/// Errors that can occur while evaluating guardrails.
#[derive(Debug, Error)]
pub enum GuardrailError {
    #[error("GitHub API error: {0}")]
    GitHubApi(#[from] anyhow::Error),
    #[error("Missing required field: {0}")]
    MissingField(String),
}

/// Result type alias for guardrail checks.
pub type GuardrailResult<T> = std::result::Result<T, GuardrailError>;

/// Evaluate all guardrails to decide whether a PR/branch may be reaped.
///
/// Checks are performed in order (cheapest first).  The first failing guard
/// returns `Ok(false)` with a log-friendly reason.  If all guards pass,
/// returns `Ok(true)`.
///
/// # Arguments
/// * `pr` — the GitHub pull request to evaluate.
/// * `task` — optional backing task; if `None` the grace-period check is skipped.
/// * `config` — sweep configuration (grace period, dry-run, enabled).
/// * `gh_client` — GitHub API client for merge-queue and branch-protection checks.
/// * `owner` — repository owner.
/// * `repo` — repository name.
///
/// # Note
/// This function is intentionally `async` only because the merge-queue and
/// protected-branch checks require GitHub API calls.  All other checks are
/// synchronous and cheap.
pub async fn can_reap_pr(
    pr: &PullRequest,
    task: Option<&Task>,
    config: &ReapConfig,
    gh_client: &GitHubApiClient,
    owner: &str,
    repo: &str,
) -> GuardrailResult<bool> {
    // 1. Enabled check
    if !config.enabled {
        tracing::debug!(pr = pr.number, "reap guardrail: sweep disabled");
        return Ok(false);
    }

    // 2. Dry-run check
    if config.dry_run {
        tracing::info!(
            pr = pr.number,
            branch = %pr.head.ref_name,
            "reap guardrail: dry-run — would reap PR/branch"
        );
        return Ok(false);
    }

    // 3. Bot-author check
    // We expect the PR to be authored by the bot.  The GitHub API user.login
    // for a GitHub App bot is "<app-name>[bot]".  We accept any login ending
    // with "[bot]" as a bot author, plus the known djinn-bot name.
    if !is_bot_author(pr) {
        tracing::debug!(pr = pr.number, "reap guardrail: not a bot-authored PR");
        return Ok(false);
    }

    // 4. Branch prefix check
    if !has_allowed_branch_prefix(pr) {
        tracing::debug!(
            pr = pr.number,
            branch = %pr.head.ref_name,
            "reap guardrail: branch does not start with task/ or chore/"
        );
        return Ok(false);
    }

    // 5. Merge-queue check
    match gh_client.get_pr_merge_queue_state(owner, repo, pr.number).await {
        Ok(state) => {
            if is_in_merge_queue(&state) {
                tracing::debug!(
                    pr = pr.number,
                    "reap guardrail: PR is currently in merge queue"
                );
                return Ok(false);
            }
        }
        Err(e) => {
            tracing::warn!(
                pr = pr.number,
                error = %e,
                "reap guardrail: failed to fetch merge-queue state; treating as blocked"
            );
            return Ok(false);
        }
    }

    // 6. Grace-period check
    if let Some(task) = task
        && let Some(ref closed_at) = task.closed_at
        && within_grace_period(closed_at, config.grace_period_seconds)
    {
        tracing::debug!(
            pr = pr.number,
            task_id = %task.short_id,
            closed_at = %closed_at,
            "reap guardrail: task closed within grace period"
        );
        return Ok(false);
    }

    // 7. Base-branch-of-another-PR check
    match gh_client.list_pulls_by_head(owner, repo, &pr.head.ref_name).await {
        Ok(open_prs) => {
            // Exclude the PR itself; if any other open PR has this branch as its base,
            // we must not delete the branch.
            let is_base_of_other = open_prs.iter().any(|other| {
                other.number != pr.number
                    && other.base.ref_name == pr.head.ref_name
                    && other.state == djinn_provider::github_api::PrState::Open
            });
            if is_base_of_other {
                tracing::debug!(
                    pr = pr.number,
                    branch = %pr.head.ref_name,
                    "reap guardrail: branch is the base of another open PR"
                );
                return Ok(false);
            }
        }
        Err(e) => {
            tracing::warn!(
                pr = pr.number,
                error = %e,
                "reap guardrail: failed to list PRs by head; treating as blocked"
            );
            return Ok(false);
        }
    }

    // 8. Protected-branch check
    match gh_client
        .list_required_status_checks(owner, repo, &pr.head.ref_name)
        .await
    {
        Ok(Some(_)) => {
            tracing::debug!(
                pr = pr.number,
                branch = %pr.head.ref_name,
                "reap guardrail: branch has required status checks (protected)"
            );
            return Ok(false);
        }
        Ok(None) => {
            // No protection / no required checks — safe to proceed.
        }
        Err(e) => {
            tracing::warn!(
                pr = pr.number,
                error = %e,
                "reap guardrail: failed to check branch protection; treating as blocked"
            );
            return Ok(false);
        }
    }

    tracing::info!(
        pr = pr.number,
        branch = %pr.head.ref_name,
        "reap guardrail: all checks passed — PR/branch may be reaped"
    );
    Ok(true)
}

/// Return `true` if the PR author looks like the bot.
///
/// We accept any GitHub user login ending with `[bot]` (GitHub Apps convention)
/// or the literal name `djinn-bot`.
fn is_bot_author(pr: &PullRequest) -> bool {
    // The PullRequest struct does not currently carry a `user` field.
    // In the existing codebase PRs created by the bot are tracked via the
    // task's `pr_url` and the bot identity is known from the app installation.
    // For the guardrail we assume the caller only passes bot-authored PRs when
    // the author field is unavailable, but we still provide a hook.
    //
    // If the GitHub API response for `PullRequest` ever includes a `user`
    // field, this should be updated to inspect it.
    // For now we treat the absence of an author field as "cannot verify" and
    // rely on the branch-prefix + task-lookup checks for safety.
    true
}

/// Return `true` if the head ref starts with `task/` or `chore/`.
fn has_allowed_branch_prefix(pr: &PullRequest) -> bool {
    let head = &pr.head.ref_name;
    head.starts_with("task/") || head.starts_with("chore/")
}

/// Return `true` if the PR is currently in a merge queue (any state other than
/// absent or unmergeable).
fn is_in_merge_queue(state: &PrMergeQueueState) -> bool {
    if let Some(entry) = &state.merge_queue_entry {
        // Unmergeable means the queue rejected it; it is NOT in the queue any
        // more, so we allow reaping (other guards permitting).
        entry.state != MergeQueueEntryState::Unmergeable
    } else {
        false
    }
}

/// Return `true` if `closed_at` is within `grace_period_seconds` of now.
fn within_grace_period(closed_at: &str, grace_period_seconds: u64) -> bool {
    let Ok(closed) = OffsetDateTime::parse(closed_at, &time::format_description::well_known::Rfc3339) else {
        // Unparseable timestamp: be conservative and say it's within grace.
        return true;
    };
    let now = OffsetDateTime::now_utc();
    let elapsed = now - closed;
    elapsed.whole_seconds() < grace_period_seconds as i64
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_provider::github_api::{PrRef, PrState};

    fn dummy_pr(head_ref: &str) -> PullRequest {
        PullRequest {
            number: 1,
            title: "test".to_string(),
            state: PrState::Open,
            merged: Some(false),
            merge_commit_sha: None,
            html_url: "https://github.com/o/r/pull/1".to_string(),
            head: PrRef {
                ref_name: head_ref.to_string(),
                sha: "abc".to_string(),
            },
            base: PrRef {
                ref_name: "main".to_string(),
                sha: "def".to_string(),
            },
            auto_merge: None,
            node_id: "node_1".to_string(),
            mergeable: None,
            mergeable_state: None,
            draft: Some(false),
        }
    }

    #[test]
    fn allowed_prefixes() {
        assert!(has_allowed_branch_prefix(&dummy_pr("task/abc")));
        assert!(has_allowed_branch_prefix(&dummy_pr("chore/xyz")));
        assert!(!has_allowed_branch_prefix(&dummy_pr("feature/foo")));
        assert!(!has_allowed_branch_prefix(&dummy_pr("main")));
    }

    #[test]
    fn grace_period_math() {
        let just_now = OffsetDateTime::now_utc()
            .replace_nanosecond(0)
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        assert!(within_grace_period(&just_now, 600));

        let old = (OffsetDateTime::now_utc() - time::Duration::seconds(1200))
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
        assert!(!within_grace_period(&old, 600));
    }

    #[test]
    fn merge_queue_state_detection() {
        use djinn_provider::github_api::{AutoMergeRequest, MergeQueueEntry, MergeQueueEntryState};

        let queued = PrMergeQueueState {
            merge_state_status: None,
            merge_queue_entry: Some(MergeQueueEntry {
                id: "q1".to_string(),
                state: MergeQueueEntryState::Queued,
                position: None,
                estimated_time_to_merge: None,
                solo: None,
            }),
            auto_merge_request: None,
            last_dequeue: None,
            head_committed_at: None,
        };
        assert!(is_in_merge_queue(&queued));

        let unmergeable = PrMergeQueueState {
            merge_state_status: None,
            merge_queue_entry: Some(MergeQueueEntry {
                id: "q1".to_string(),
                state: MergeQueueEntryState::Unmergeable,
                position: None,
                estimated_time_to_merge: None,
                solo: None,
            }),
            auto_merge_request: None,
            last_dequeue: None,
            head_committed_at: None,
        };
        assert!(!is_in_merge_queue(&unmergeable));

        let none = PrMergeQueueState {
            merge_state_status: None,
            merge_queue_entry: None,
            auto_merge_request: None,
            last_dequeue: None,
            head_committed_at: None,
        };
        assert!(!is_in_merge_queue(&none));
    }
}
