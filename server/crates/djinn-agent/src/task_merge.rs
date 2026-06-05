use crate::context::AgentContext;
use djinn_core::models::SessionStatus;
use djinn_db::{ProjectRepository, SessionRepository, TaskRepository};
use djinn_git::GitError;
use djinn_provider::github_api::GitHubApiClient;
use djinn_provider::github_app::app_id as github_app_id;
use djinn_workspace::MirrorManager;

/// Build the HTTPS push URL for a GitHub repo authenticated by a GitHub App
/// **installation** access token.
///
/// The resulting URL uses the `x-access-token` basic-auth username that
/// GitHub documents for installation tokens, and encodes `owner`/`repo`
/// unchanged (we assume callers have already normalised them). Commits
/// pushed through this URL are attributed to the App's bot identity
/// (`djinn-bot[bot]`).
pub(crate) fn build_app_push_url(owner: &str, repo: &str, installation_token: &str) -> String {
    let repo = repo.trim_end_matches(".git");
    format!("https://x-access-token:{installation_token}@github.com/{owner}/{repo}.git")
}

/// Bot identity used when committing/pushing through the GitHub App. The
/// canonical no-reply email form is `<app-id>+djinn-bot[bot]@users.noreply.github.com`.
fn bot_identity() -> (String, String) {
    let app_id = djinn_provider::github_app::app_id()
        .map(|id| id.to_string())
        .unwrap_or_else(|_| "0".to_string());
    (
        "djinn-bot[bot]".to_string(),
        format!("{app_id}+djinn-bot[bot]@users.noreply.github.com"),
    )
}

/// Parse `owner` and `repo` from a GitHub remote URL.
///
/// Supports both HTTPS (`https://github.com/owner/repo.git`) and SSH
/// (`git@github.com:owner/repo.git`) formats.
///
/// Kept for tests and potential future use; the production push path now
/// reads coordinates from the `projects` DB row instead.
#[allow(dead_code)]
fn parse_github_owner_repo(remote_url: &str) -> Option<(String, String)> {
    // Normalize: strip user@ from HTTPS URLs (e.g. https://user@github.com/...)
    let url = if let Some(rest) = remote_url.strip_prefix("https://") {
        if let Some(at_pos) = rest.find('@') {
            format!("https://{}", &rest[at_pos + 1..])
        } else {
            remote_url.to_string()
        }
    } else if let Some(rest) = remote_url.strip_prefix("http://") {
        if let Some(at_pos) = rest.find('@') {
            format!("http://{}", &rest[at_pos + 1..])
        } else {
            remote_url.to_string()
        }
    } else {
        remote_url.to_string()
    };

    // SSH: git@github.com:owner/repo.git
    if let Some(path) = url.strip_prefix("git@github.com:") {
        return split_owner_repo(path);
    }
    // HTTPS: https://github.com/owner/repo.git
    for prefix in &["https://github.com/", "http://github.com/"] {
        if let Some(path) = url.strip_prefix(prefix) {
            return split_owner_repo(path);
        }
    }
    None
}

fn split_owner_repo(path: &str) -> Option<(String, String)> {
    let path = path.trim_end_matches(".git");
    let mut parts = path.splitn(2, '/');
    let owner = parts.next()?.to_string();
    let repo = parts.next()?.to_string();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some((owner, repo))
}

pub(crate) async fn interrupt_paused_worker_session(task_id: &str, app_state: &AgentContext) {
    let repo = SessionRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let Ok(Some(paused)) = repo.paused_for_task(task_id).await else {
        return;
    };

    if let Err(e) = repo
        .update(
            &paused.id,
            SessionStatus::Interrupted,
            paused.tokens_in,
            paused.tokens_out,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            record_id = %paused.id,
            error = %e,
            "failed to interrupt paused worker session after reviewer rejection"
        );
    }
}

/// Best-effort cleanup of the task branch on the local mirror and on the
/// GitHub remote after the task transitions to `closed` (either via
/// `PrMerge` or any `ForceClose` path — lead intervention, admin tool,
/// PR-closed-without-merge detection).
///
/// Auto-merge / merge-queue cleanup is *implicit*: deleting the PR branch
/// on the remote closes the PR, which automatically cancels GitHub's
/// auto-merge request and removes the PR from the repository merge queue.
/// We deliberately don't call `disable_auto_merge` / `dequeue_pull_request`
/// explicitly here — it would mean 2-3 extra GraphQL round-trips per
/// close for no behavioral change in the happy path.
///
/// Idempotent and non-fatal: any failure is logged and swallowed so the
/// close transition is never blocked on cleanup.  When a branch is already
/// gone from either side, GitHub returns 422 (treated as success by
/// `delete_ref`) and `git update-ref -d` exits non-zero, which we log and
/// move on from.
///
/// Why both sides:
/// - Local mirror: leaves `refs/heads/task/<short_id>` pointing at a dead
///   commit forever otherwise; `clone_ephemeral`s see it on every dispatch.
/// - GitHub remote: GitHub doesn't auto-delete head branches unless the
///   repo has "automatically delete head branches" enabled.  Deleting the
///   ref via API also closes any PR still open on that head — exactly the
///   behavior we want for lead-supersede force-closes where the PR was
///   left open as garbage.
pub async fn cleanup_task_branches_post_close(
    task_id: &str,
    db: &djinn_db::Database,
    event_bus: &djinn_core::events::EventBus,
    mirror: Option<&MirrorManager>,
) {
    let task_repo = TaskRepository::new(db.clone(), event_bus.clone());
    let Ok(Some(task)) = task_repo.get(task_id).await else {
        return;
    };
    let task_branch = format!("task/{}", task.short_id);

    // ── Local mirror ────────────────────────────────────────────────────
    if let Some(mirror) = mirror {
        let mirror_path = mirror.mirror_path(&task.project_id);
        if mirror_path.exists() {
            match djinn_git::run_git_command(
                mirror_path.clone(),
                vec![
                    "update-ref".into(),
                    "-d".into(),
                    format!("refs/heads/{task_branch}"),
                ],
            )
            .await
            {
                Ok(_) => {
                    tracing::info!(
                        task_id = %task.short_id,
                        branch = %task_branch,
                        "post-close cleanup: deleted task branch from mirror"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        branch = %task_branch,
                        error = %e,
                        "post-close cleanup: failed to delete task branch from mirror"
                    );
                }
            }
        }
    }

    // ── GitHub remote ──────────────────────────────────────────────────
    let Some(pr_url) = task.pr_url.as_deref() else {
        return;
    };
    let Some((owner, repo, _pull)) = crate::actors::coordinator::pr_poller::parse_pr_url(pr_url)
    else {
        return;
    };
    if github_app_id().is_err() {
        return;
    }
    let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());
    let installation_id = match project_repo.get_installation_id(&task.project_id).await {
        Ok(Some(id)) => id,
        _ => return,
    };
    let client = GitHubApiClient::for_installation(installation_id);
    let ref_name = format!("heads/{task_branch}");
    match client.delete_ref(&owner, &repo, &ref_name).await {
        Ok(()) => {
            tracing::info!(
                task_id = %task.short_id,
                owner = %owner,
                repo = %repo,
                branch = %task_branch,
                "post-close cleanup: deleted task branch on GitHub"
            );
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task.short_id,
                owner = %owner,
                repo = %repo,
                branch = %task_branch,
                error = %e,
                "post-close cleanup: failed to delete task branch on GitHub"
            );
        }
    }
}

pub(crate) async fn resolve_project_path_for_id(
    project_id: &str,
    app_state: &AgentContext,
) -> Option<String> {
    let repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    repo.get(project_id).await.ok().flatten().map(|p| {
        djinn_core::paths::project_dir(&p.github_owner, &p.github_repo)
            .to_string_lossy()
            .into_owned()
    })
}

/// Best-effort detection of which files would conflict if `task_branch` were
/// merged into `target_branch` right now, by trial-merging against the local
/// bare mirror.
///
/// Used by the PR poller when GitHub reports `mergeable == false` so we can
/// populate `merge_conflict_metadata` with the conflicting file list; without
/// it, the worker re-dispatch picks `SupervisorFlow::NewTask` instead of
/// `ConflictRetry` and the worker keeps fixing CI failures while the actual
/// conflict against the target branch never gets resolved.
///
/// Returns `Ok(files)` (non-empty on conflict, empty if the mirror disagrees
/// with GitHub — typically a stale `origin/{target_branch}` ref) or `Err` if
/// the trial merge couldn't run at all.  The ephemeral workspace is dropped
/// at the end of the call.
pub(crate) async fn detect_pr_conflict_files(
    mirror: &MirrorManager,
    project_id: &str,
    task_branch: &str,
    target_branch: &str,
) -> Result<Vec<String>, GitError> {
    let workspace = mirror
        .clone_ephemeral(project_id, target_branch)
        .await
        .map_err(|e| GitError::Other(anyhow::anyhow!("clone_ephemeral: {e}")))?;
    let wt = workspace.path_buf();

    // `git merge` insists on a user identity even when no commit is recorded.
    let (bot_name, bot_email) = bot_identity();
    let _ = djinn_git::run_git_command(
        wt.clone(),
        vec!["config".into(), "user.name".into(), bot_name],
    )
    .await;
    let _ = djinn_git::run_git_command(
        wt.clone(),
        vec!["config".into(), "user.email".into(), bot_email],
    )
    .await;

    djinn_git::run_git_command(
        wt.clone(),
        vec![
            "fetch".into(),
            "origin".into(),
            format!("{task_branch}:refs/remotes/origin/{task_branch}"),
        ],
    )
    .await?;

    // `merge --squash` matches the landing semantics in `squash_merge_via_mirror`
    // and approximates what GitHub's `mergeable` check evaluates: cleanly fold
    // target + task into a single tree.  Without `--no-commit` git would try to
    // record the merge; `--squash` stops short, leaving the index conflicted on
    // failure.  The TempDir is discarded at function exit so no `merge --abort`
    // is necessary.
    let merge_result = djinn_git::run_git_command(
        wt.clone(),
        vec![
            "merge".into(),
            "--squash".into(),
            format!("origin/{task_branch}"),
        ],
    )
    .await;

    match merge_result {
        Ok(_) => Ok(Vec::new()),
        Err(GitError::CommandFailed { .. }) => {
            Ok(djinn_git::unmerged_files(wt).await.unwrap_or_default())
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_app_push_url_shape() {
        let url = build_app_push_url("acme", "widgets", "ghs_FAKETOKEN123");
        assert_eq!(
            url,
            "https://x-access-token:ghs_FAKETOKEN123@github.com/acme/widgets.git"
        );
    }

    #[test]
    fn build_app_push_url_strips_trailing_dot_git() {
        let url = build_app_push_url("acme", "widgets.git", "ghs_tok");
        assert_eq!(
            url,
            "https://x-access-token:ghs_tok@github.com/acme/widgets.git"
        );
    }

    #[test]
    fn build_app_push_url_uses_x_access_token_user() {
        let url = build_app_push_url("octo", "hello-world", "ghs_abc");
        assert!(url.starts_with("https://x-access-token:"));
        assert!(url.contains("@github.com/octo/hello-world.git"));
        // Must never fall back to `x-oauth-basic` (the legacy PAT form).
        assert!(!url.contains("x-oauth-basic"));
    }

    #[test]
    fn parse_ssh_remote() {
        let (owner, repo) = parse_github_owner_repo("git@github.com:acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_https_remote() {
        let (owner, repo) = parse_github_owner_repo("https://github.com/acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_https_without_dot_git() {
        let (owner, repo) = parse_github_owner_repo("https://github.com/acme/widgets").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_https_with_user_prefix() {
        let (owner, repo) =
            parse_github_owner_repo("https://someuser@github.com/acme/svc-accounts-payable.git")
                .unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "svc-accounts-payable");
    }

    #[test]
    fn parse_https_with_user_prefix_no_dot_git() {
        let (owner, repo) =
            parse_github_owner_repo("https://user@github.com/acme/widgets").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_http_with_user_prefix() {
        let (owner, repo) =
            parse_github_owner_repo("http://user@github.com/acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_non_github_returns_none() {
        assert!(parse_github_owner_repo("git@gitlab.com:acme/widgets.git").is_none());
        assert!(parse_github_owner_repo("https://gitlab.com/acme/widgets.git").is_none());
        assert!(parse_github_owner_repo("https://user@gitlab.com/acme/widgets.git").is_none());
    }

    #[test]
    fn parse_empty_owner_or_repo_returns_none() {
        assert!(parse_github_owner_repo("git@github.com:/widgets.git").is_none());
        assert!(parse_github_owner_repo("git@github.com:acme/").is_none());
    }
}
