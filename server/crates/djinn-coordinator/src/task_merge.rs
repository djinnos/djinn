use crate::context::CoordinatorContext;
use djinn_core::models::SessionStatus;
use djinn_db::{ProjectRepository, SessionRepository, TaskRepository};
use djinn_git::GitError;
use djinn_provider::github_api::GitHubApiClient;
use djinn_provider::github_app::app_id as github_app_id;
use djinn_provider::github_app::installations::get_installation_token;
use djinn_workspace::{
    GitIdentity, MergeOutcome, MergeSafetyDecision, MirrorManager, evaluate_merge_head,
    is_checkpoint_ref, is_protected_ref,
};

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

#[allow(dead_code)]
pub(crate) async fn interrupt_paused_worker_session(task_id: &str, app_state: &CoordinatorContext) {
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
/// ## Safety guards (sibling task `sy0g`)
/// The cleanup is gated by [`djinn_workspace::is_checkpoint_ref`] and
/// [`djinn_workspace::is_protected_ref`] so it can never delete a ref the
/// merge / resume paths still need:
/// - **Alternate checkpoint refs** (`refs/djinn/checkpoints/...`) created
///   by sibling task `8yjx` (capture-before-exit) are NEVER deleted here
///   — they are preservation / resume sources the resume-via-git selector
///   (sibling `3ln4`) may still need to consult on a subsequent
///   re-dispatch. Deleting them would silently turn a recoverable
///   checkpoint into a clean-task-branch fallback.
/// - **Protected refs** (`main`, `master`, `HEAD`, …) are NEVER deleted.
///   The canonical task branch ref never matches a protected entry, but
///   a future refactor that composes refs from project_id (e.g. an
///   accidental `main` slug in a config) must not silently erase the
///   integration target.
///
/// After the local-mirror delete we additionally enumerate any alternate
/// checkpoint refs sitting in the mirror under
/// `refs/djinn/checkpoints/...` and emit a structured `info!` line per
/// batch. We do NOT delete those refs — the resume selector still needs
/// them — but the inventory gives operators a paper trail and lets the
/// activity-log writer correlate them with the closing task.
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
    let task_ref_full = format!("refs/heads/{task_branch}");

    // Defense-in-depth: refuse to act on anything that isn't the
    // canonical task branch. The local-mirror path below hard-codes
    // `refs/heads/{task_branch}`, but if a future refactor threads a
    // different ref name through, this guard catches it before we
    // accidentally delete a protected or checkpoint ref.
    if is_checkpoint_ref(&task_ref_full) {
        tracing::error!(
            task_id = %task.short_id,
            ref_name = %task_ref_full,
            "post-close cleanup: refusing to delete a checkpoint preservation ref as if it were a task branch"
        );
        return;
    }
    if is_protected_ref(&task_ref_full) || is_protected_ref(&task_branch) {
        tracing::error!(
            task_id = %task.short_id,
            ref_name = %task_ref_full,
            "post-close cleanup: refusing to delete a protected ref"
        );
        return;
    }

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

        // Structured inventory of any alternate checkpoint refs the
        // mirror still carries for this task. We deliberately do NOT
        // delete these — the resume-via-git selector still needs them
        // on a subsequent re-dispatch — but the inventory gives
        // operators a paper trail and lets the activity-log writer
        // correlate them with the closing task row.
        emit_checkpoint_ref_inventory(&mirror_path, &task.short_id).await;
    }

    // ── GitHub remote ──────────────────────────────────────────────────
    let Some(pr_url) = task.pr_url.as_deref() else {
        return;
    };
    let Some((owner, repo, _pull)) = crate::pr_poller::parse_pr_url(pr_url) else {
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

/// Enumerate every alternate checkpoint ref currently present in
/// `mirror_path` and emit a structured `info!` log line for the batch.
///
/// Does NOT delete the refs — the resume-via-git selector
/// (`djinn_coordinator::dispatch::resume_source`) still needs them on
/// a subsequent re-dispatch — but gives operators a paper trail of
/// what checkpoint preservation state is sitting in the mirror, so a
/// manual operator scrub can clear them deliberately when the task is
/// fully closed.
///
/// `task_short_id` is included so the log line can be correlated with
/// the task row without a separate DB lookup.
///
/// Best-effort: any failure (missing mirror, git error) is logged at
/// `warn!` and swallowed. The caller must NEVER block a task close on
/// this enumeration.
async fn emit_checkpoint_ref_inventory(mirror_path: &std::path::Path, task_short_id: &str) {
    if !mirror_path.exists() {
        return;
    }
    // `git for-each-ref refs/djinn/checkpoints/` is the canonical way to
    // enumerate alternate checkpoint refs on a bare mirror. The mirror
    // may carry refs from prior tasks too; we deliberately don't filter
    // by task_id here (no naming convention enforces it) so the
    // inventory gives a complete picture of the preservation namespace.
    let output = djinn_git::run_git_command(
        mirror_path.to_path_buf(),
        vec![
            "for-each-ref".into(),
            "--format=%(refname)".into(),
            djinn_workspace::CHECKPOINT_REF_PREFIX.into(),
        ],
    )
    .await;
    match output {
        Ok(out) => {
            let refs: Vec<String> = out
                .stdout
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect();
            if refs.is_empty() {
                return;
            }
            tracing::info!(
                task_id = %task_short_id,
                checkpoint_ref_count = refs.len(),
                checkpoint_refs = ?refs,
                "post-close cleanup: preserved alternate checkpoint refs in mirror (resume-via-git must not delete)"
            );
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_short_id,
                error = %e,
                "post-close cleanup: failed to enumerate alternate checkpoint refs (non-fatal)"
            );
        }
    }
}

/// Final-merge head evaluator: convenience wrapper of
/// [`djinn_workspace::evaluate_merge_head`] that pre-fills the task id from
/// a [`djinn_core::models::Task`]. Used by the merge-side guards (PR
/// poller, supervisor's PR-open step) so callers don't have to thread
/// `task.short_id` through every call site.
pub fn evaluate_final_merge_head(
    task: &djinn_core::models::Task,
    ref_name: &str,
    sha: Option<&str>,
) -> MergeSafetyDecision {
    evaluate_merge_head(&task.short_id, ref_name, sha)
}

#[allow(dead_code)]
pub(crate) async fn resolve_project_path_for_id(
    project_id: &str,
    app_state: &CoordinatorContext,
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

/// Outcome of [`try_auto_merge_target_into_task_branch`].
#[derive(Debug)]
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

    // ────────────────────────────────────────────────────────────────────────
    // Sibling task `sy0g`: merge / branch safety guards
    // ────────────────────────────────────────────────────────────────────────
    //
    // The post-close branch cleanup must NEVER delete a checkpoint
    // preservation ref or a protected integration ref. We exercise the
    // pure guard path (no live DB / GitHub) by re-using the
    // `djinn_workspace::is_*_ref` helpers the coordinator's
    // `cleanup_task_branches_post_close` consults, and asserting the
    // classification the cleanup path observes for the ref shapes it
    // actually constructs (`refs/heads/task/<short_id>`,
    // `refs/heads/main`, `refs/djinn/checkpoints/...`).
    //
    // A positive end-to-end test of `cleanup_task_branches_post_close`
    // would require a full DB + MirrorManager harness; that lives in
    // `djinn-workspace/tests/merge_safety_e2e.rs` (the cross-cutting
    // tests below cover the pure seam).

    use djinn_workspace::{
        CHECKPOINT_REF_PREFIX, PROTECTED_REFS, RefRole, classify_ref, is_checkpoint_ref,
        is_protected_ref,
    };

    #[test]
    fn cleanup_target_task_branch_ref_is_classified_as_task_branch() {
        // The cleanup constructs `refs/heads/task/<short_id>` — the
        // canonical task branch — and the guard must classify it as the
        // only role that is safe to delete.
        let task_branch = "refs/heads/task/abc12";
        assert_eq!(classify_ref(task_branch), RefRole::TaskBranch);
        assert!(!is_checkpoint_ref(task_branch));
        assert!(!is_protected_ref(task_branch));
    }

    #[test]
    fn cleanup_guard_skips_alternate_checkpoint_refs() {
        // If a future refactor mistakenly threaded a checkpoint ref
        // through the cleanup target (e.g. by selecting the wrong
        // field from the lifecycle metadata), the guard must catch it
        // and refuse to delete.
        let checkpoint_ref = format!("{CHECKPOINT_REF_PREFIX}task-abc/session-1");
        assert!(is_checkpoint_ref(&checkpoint_ref));
        assert_eq!(classify_ref(&checkpoint_ref), RefRole::CheckpointRef);
        assert!(
            !RefRole::CheckpointRef.is_safe_to_cleanup(),
            "checkpoint refs must be unsafe for the automated cleanup path to delete"
        );
    }

    #[test]
    fn cleanup_guard_skips_protected_refs() {
        // Belt-and-braces: even if a project_id accidentally slugifies
        // to `main`, the guard refuses to delete it.
        for protected_short in PROTECTED_REFS {
            let full_ref = format!("refs/heads/{protected_short}");
            assert!(
                is_protected_ref(&full_ref),
                "guard must classify {full_ref:?} as protected so cleanup refuses it"
            );
            assert!(
                !RefRole::Protected.is_safe_to_cleanup(),
                "protected refs must be unsafe for the automated cleanup path"
            );
        }
    }

    #[test]
    fn cleanup_guard_does_not_misclassify_checkpoint_namespace_as_protected() {
        // Adding `djinn` (or any similar) to PROTECTED_REFS in the
        // future must NOT silently re-classify the checkpoint namespace
        // as protected — that would silently break resume-via-git
        // because the resume selector's checkpoint refs would suddenly
        // be untouchable. Pin the invariant.
        let checkpoint_ref = format!("{CHECKPOINT_REF_PREFIX}task-1/session-1");
        assert_eq!(
            classify_ref(&checkpoint_ref),
            RefRole::CheckpointRef,
            "checkpoint ref must classify as CheckpointRef even when PROTECTED_REFS changes"
        );
        assert!(!is_protected_ref(&checkpoint_ref));
    }

    #[test]
    fn final_merge_head_eligibility_only_for_task_branch_and_other() {
        // The merge path must accept only TaskBranch / Other refs as
        // the source of the final squash merge. Checkpoint and
        // Protected must be rejected.
        assert!(RefRole::TaskBranch.is_eligible_final_merge_source());
        assert!(RefRole::Other.is_eligible_final_merge_source());
        assert!(!RefRole::CheckpointRef.is_eligible_final_merge_source());
        assert!(!RefRole::Protected.is_eligible_final_merge_source());
    }

    #[test]
    fn evaluate_final_merge_head_wrapper_passes_task_short_id() {
        // The wrapper must thread the task's short_id into the
        // rejection payload so callers can emit structured events
        // tagged with the task identifier. We can't easily intercept
        // the `tracing::warn!` call from a unit test, but we CAN assert
        // the decision shape (which carries the ref name and SHA the
        // structured event would log).
        let task = minimal_task("abc12");

        // Task branch with SHA is eligible.
        let decision = evaluate_final_merge_head(&task, "refs/heads/task/abc12", Some("deadbeef"));
        assert_eq!(decision, MergeSafetyDecision::Eligible);

        // Checkpoint ref with SHA is rejected.
        let decision = evaluate_final_merge_head(
            &task,
            "refs/djinn/checkpoints/task-abc/session-1",
            Some("deadbeef"),
        );
        assert!(matches!(
            decision,
            MergeSafetyDecision::CheckpointRef { ref ref_name, sha: Some(s) }
                if ref_name == "refs/djinn/checkpoints/task-abc/session-1" && s == "deadbeef"
        ));

        // Protected ref is rejected regardless of SHA presence.
        let decision = evaluate_final_merge_head(&task, "main", Some("deadbeef"));
        assert!(matches!(decision, MergeSafetyDecision::ProtectedRef { .. }));

        // Missing SHA on a task branch yields MissingSha so the merge
        // path can degrade to a no-op rather than guess.
        let decision = evaluate_final_merge_head(&task, "refs/heads/task/abc12", None);
        assert!(matches!(decision, MergeSafetyDecision::MissingSha { .. }));
    }

    /// Minimal task stub for `evaluate_final_merge_head` tests. The
    /// function only reads `task.short_id`, so the rest can be left
    /// at `String::new()` / default values. `Task` has no `Default`
    /// impl, so we construct it field-by-field.
    fn minimal_task(short_id: &str) -> djinn_core::models::Task {
        djinn_core::models::Task {
            id: "task-uuid".to_string(),
            project_id: String::new(),
            short_id: short_id.to_string(),
            epic_id: None,
            title: String::new(),
            description: String::new(),
            design: String::new(),
            issue_type: "task".to_string(),
            status: "open".to_string(),
            priority: 0,
            owner: String::new(),
            labels: "[]".to_string(),
            acceptance_criteria: "[]".to_string(),
            reopen_count: 0,
            continuation_count: 0,
            total_reopen_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: String::new(),
            updated_at: String::new(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".to_string(),
            agent_type: None,
            created_by_user_id: "test-user".to_owned(),
            ci_status: "unknown".to_string(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".to_string(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            ci_mq_state: None,
            ci_mq_run_id: None,
            ci_mq_head_sha: None,
            ci_mq_failed_check_names: None,
            ci_mq_failure_fingerprint: None,
            ci_mq_same_signature_count: None,
            ci_mq_first_seen_at: None,
            ci_mq_last_seen_at: None,
            unresolved_blocker_count: 0,
        }
    }
}
