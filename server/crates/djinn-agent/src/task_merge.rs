use crate::context::AgentContext;
use djinn_core::models::SessionStatus;
use djinn_db::{ProjectRepository, SessionRepository, TaskRepository};
use djinn_git::GitError;
use djinn_provider::github_api::GitHubApiClient;
use djinn_provider::github_app::app_id as github_app_id;
use djinn_provider::github_app::installations::get_installation_token;
use djinn_workspace::{GitIdentity, MergeOutcome, MirrorManager};

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

/// Bot identity used when committing/pushing through the active GitHub App.
#[allow(dead_code)]
fn bot_identity() -> (String, String) {
    djinn_provider::github_app::bot_git_identity()
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
            paused.cache_read_tokens,
            paused.cache_write_tokens,
            None,
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
#[allow(dead_code)]
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

/// Outcome of [`try_auto_merge_target_into_task_branch`].
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum AutoMergeOutcome {
    /// The merge target folded cleanly into the task branch (or the task
    /// branch was merely behind / already current). The merge commit was
    /// pushed to BOTH the mirror and GitHub, so the PR's head now contains the
    /// target and GitHub will re-evaluate it as mergeable. NO conflict
    /// metadata should be set and the task should NOT be reopened.
    AutoMerged,
    /// A real content conflict remains — the merge left unmerged files. The
    /// caller proceeds with the normal ConflictRetry flagging (metadata +
    /// reopen); `build_pr_conflict_reason` re-derives the file list for the
    /// metadata, so the list is not threaded back here.
    Conflicts,
    /// The merge could not be decided mechanically (no App configured, missing
    /// coords/installation, mirror fetch or git error). The caller falls back
    /// to its prior behaviour (trial-merge file detection + reopen). Nothing
    /// was pushed.
    Indeterminate,
}

/// Clean-merge fast path at PR-conflict detection.
///
/// When GitHub flags a PR `mergeable == false`, this tries to resolve the
/// situation MECHANICALLY before dispatching any agent: fetch the mirror fresh,
/// ephemeral-clone the task branch, and `git merge --no-ff` the merge target
/// into it.
///
/// - **Clean merge** (or the branch is merely behind / already current): commit
///   the merge under the bot identity, push it to the mirror AND force-push the
///   task branch to GitHub (reusing the same machinery as
///   [`push_task_branch_to_github`]), so the open PR's head advances and GitHub
///   re-evaluates it as mergeable. Returns [`AutoMergeOutcome::AutoMerged`].
///   No worker, no reviewer, no verification — zero agent involvement.
/// - **Conflict**: returns [`AutoMergeOutcome::Conflicts`] with the file list so
///   the caller flags `merge_conflict_metadata` + reopens into ConflictRetry.
/// - **Anything indeterminate** (no App / missing coords / git error): returns
///   [`AutoMergeOutcome::Indeterminate`]; the caller degrades to prior behaviour.
///
/// ## Freshness
/// The mirror is fetched against GitHub (fresh install token) up front, so the
/// `origin/<merge_target>` the merge decision uses is the CURRENT remote tip —
/// a stale mirror would otherwise produce a wrong "clean merge".
///
/// ## Idempotency / race-safety
/// `try_merge` first runs an ancestry no-op guard: if `origin/<merge_target>`
/// is already an ancestor of the task branch HEAD, there is nothing to merge.
/// A second concurrent poll tick (or a concurrent run on the same task) that
/// re-enters here after a prior auto-merge therefore finds the branch already
/// current and pushes the same SHA — a no-op force-push. The GitHub push reuses
/// the `is_concurrent_push_race` recovery in `push_task_branch_to_github`'s
/// sibling logic via a plain `--force` push that's idempotent on identical SHAs.
#[allow(dead_code)]
pub(crate) async fn try_auto_merge_target_into_task_branch(
    mirror: &MirrorManager,
    db: &djinn_db::Database,
    event_bus: &djinn_core::events::EventBus,
    project_id: &str,
    task_short_id: &str,
    task_branch: &str,
    merge_target: &str,
) -> AutoMergeOutcome {
    if github_app_id().is_err() {
        return AutoMergeOutcome::Indeterminate;
    }

    let project_repo = ProjectRepository::new(db.clone(), event_bus.clone());

    let (owner, repo_name) = match project_repo.get_github_coords(project_id).await {
        Ok(Some(coords)) => coords,
        _ => return AutoMergeOutcome::Indeterminate,
    };
    let installation_id = match project_repo.get_installation_id(project_id).await {
        Ok(Some(id)) => id,
        _ => return AutoMergeOutcome::Indeterminate,
    };
    let install_token = match get_installation_token(installation_id).await {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(
                task_short_id,
                error = %e,
                "auto-merge fast path: could not mint installation token; falling back to reopen"
            );
            return AutoMergeOutcome::Indeterminate;
        }
    };

    // Fetch the mirror fresh against GitHub so `origin/<merge_target>` is the
    // current remote tip — otherwise a stale mirror would mis-decide the merge.
    let origin_url = format!(
        "https://x-access-token:{}@github.com/{}/{}.git",
        install_token.token, owner, repo_name
    );
    if let Err(e) = mirror.fetch_mirror(project_id, &origin_url).await {
        tracing::warn!(
            task_short_id,
            error = %e,
            "auto-merge fast path: mirror fetch failed; falling back to reopen"
        );
        return AutoMergeOutcome::Indeterminate;
    }

    // Ephemeral clone on the task branch (so the merge lands ONTO it and we can
    // push the result back as the task branch head).
    let workspace = match mirror.clone_ephemeral(project_id, task_branch).await {
        Ok(ws) => ws,
        Err(e) => {
            tracing::warn!(
                task_short_id,
                branch = task_branch,
                error = %e,
                "auto-merge fast path: clone_ephemeral failed; falling back to reopen"
            );
            return AutoMergeOutcome::Indeterminate;
        }
    };

    // Already current? (origin/<merge_target> is an ancestor of HEAD.) The PR
    // signal is then stale / merely-behind on GitHub's side; re-pushing the
    // current head refreshes its mergeability without a merge commit.
    let already_current = workspace
        .is_up_to_date_with(merge_target)
        .await
        .unwrap_or(false);

    if !already_current {
        match workspace.try_merge(merge_target).await {
            Ok(MergeOutcome::Clean) => {
                let (bot_name, bot_email) = bot_identity();
                let identity = GitIdentity {
                    name: &bot_name,
                    email: &bot_email,
                };
                let message = format!("Merge {merge_target} into {task_branch}");
                match workspace.commit(&message, identity).await {
                    Ok(outcome) if outcome.committed() => {}
                    Ok(_) => {
                        // Staged a non-empty diff but nothing to commit — the
                        // merge produced no tree change. Treat as already-current.
                    }
                    Err(e) => {
                        tracing::warn!(
                            task_short_id,
                            error = %e,
                            "auto-merge fast path: merge commit failed; falling back to reopen"
                        );
                        return AutoMergeOutcome::Indeterminate;
                    }
                }
            }
            Ok(MergeOutcome::Conflicts { files }) => {
                // Real conflict — let the caller flag + reopen into ConflictRetry.
                tracing::info!(
                    task_short_id,
                    conflict_count = files.len(),
                    conflicting_files = ?files,
                    "auto-merge fast path: real conflict; deferring to ConflictRetry flow"
                );
                return AutoMergeOutcome::Conflicts;
            }
            Err(e) => {
                tracing::warn!(
                    task_short_id,
                    error = %e,
                    "auto-merge fast path: try_merge errored; falling back to reopen"
                );
                return AutoMergeOutcome::Indeterminate;
            }
        }
    }

    // Push the (possibly new) task-branch head to the mirror, then force it to
    // GitHub so the PR refreshes. A failure on either leg is indeterminate.
    if let Err(e) = workspace.push_to_origin(task_branch).await {
        tracing::warn!(
            task_short_id,
            branch = task_branch,
            error = %e,
            "auto-merge fast path: push to mirror failed; falling back to reopen"
        );
        return AutoMergeOutcome::Indeterminate;
    }

    let push_url = build_app_push_url(&owner, &repo_name, &install_token.token);
    let wt = workspace.path_buf();
    let push_result = djinn_git::run_git_command(
        wt,
        vec![
            "push".into(),
            "--force".into(),
            push_url,
            format!("{task_branch}:refs/heads/{task_branch}"),
        ],
    )
    .await;
    if let Err(e) = push_result {
        tracing::warn!(
            task_short_id,
            branch = task_branch,
            error = %e,
            "auto-merge fast path: force-push to GitHub failed; falling back to reopen"
        );
        return AutoMergeOutcome::Indeterminate;
    }

    tracing::info!(
        task_short_id,
        branch = task_branch,
        merge_target,
        already_current,
        "auto-merge fast path: merged target into task branch and refreshed PR (no agent dispatch)"
    );
    AutoMergeOutcome::AutoMerged
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
