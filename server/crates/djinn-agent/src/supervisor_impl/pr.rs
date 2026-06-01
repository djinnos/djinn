//! Supervisor-driven PR-open orchestration.
//!
//! Stays in `djinn-agent` (rather than moving to `djinn-supervisor`) because
//! the PR path calls `task_merge::squash_merge_via_mirror` /
//! `build_app_push_url` and reads `AgentContext.mirror`. It's invoked by the
//! supervisor body through `SupervisorServices::open_pr_fn`, wired by
//! `actors::slot::supervisor_runner::run_supervisor_dispatch`.
//!
//! Scope is intentionally narrower than
//! [`crate::task_merge::merge_and_transition`]: no worktree teardown, no
//! knowledge-promotion side effects, no activity-log writes.  The supervisor
//! keeps those concerns inside [`super::stage::execute_stage`]'s post-session
//! path; this module only:
//!
//! 1. Resolves the project's owner/repo/installation.
//! 2. Mints a GitHub-App installation token.
//! 3. Runs [`crate::task_merge::squash_merge_via_mirror`] through the mirror.
//! 4. Creates (or adopts/reopens) a GitHub PR for the squashed commit.

use djinn_core::models::TransitionAction;
use djinn_db::{ProjectRepository, TaskRepository};
use djinn_git::GitError;
use djinn_provider::github_api::{CreatePrParams, GitHubApiClient, PrState};
use djinn_provider::github_app::{app_id as github_app_id, installations::get_installation_token};
use djinn_runtime::spec::{TaskRunOutcome, TaskRunSpec};
use djinn_workspace::MirrorManager;

use super::SupervisorCallbackContext;
use crate::actors::slot::helpers::default_target_branch;
use crate::task_merge::build_app_push_url;

/// Open (or adopt) a GitHub PR for the completed task-run.
///
/// Returns:
/// - `TaskRunOutcome::PrOpened { url, sha }` on success.
/// - `TaskRunOutcome::Failed { stage: "pr_open", reason }` for any failure.
pub(crate) async fn supervisor_pr_open(
    spec: &TaskRunSpec,
    task: &djinn_core::models::Task,
    callbacks: &SupervisorCallbackContext,
) -> TaskRunOutcome {
    if github_app_id().is_err() {
        return TaskRunOutcome::Failed {
            stage: "pr_open".into(),
            provider_failure: None,
            reason: "GitHub App is not configured on this deployment — \
                     supervisor PR-open requires the App"
                .into(),
        };
    }

    let app_state = &callbacks.agent_context;
    let mirror = match app_state.mirror.as_ref() {
        Some(m) => m.clone(),
        None => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: "supervisor PR-open requires MirrorManager but AgentContext has none"
                    .into(),
            };
        }
    };

    let project_repo = ProjectRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    let (owner, repo_name) = match project_repo.get_github_coords(&spec.project_id).await {
        Ok(Some(coords)) => coords,
        Ok(None) => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: format!(
                    "project {} has no github_owner/github_repo persisted",
                    spec.project_id
                ),
            };
        }
        Err(e) => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: format!(
                    "failed to read github coords for project {}: {e}",
                    spec.project_id
                ),
            };
        }
    };

    let installation_id = match project_repo.get_installation_id(&spec.project_id).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: format!(
                    "project {} ({}/{}) has no cached installation_id",
                    spec.project_id, owner, repo_name
                ),
            };
        }
        Err(e) => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: format!(
                    "failed to read installation_id for project {}: {e}",
                    spec.project_id
                ),
            };
        }
    };

    let install_token = match get_installation_token(installation_id).await {
        Ok(t) => t,
        Err(e) => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: format!("could not mint installation token: {e}"),
            };
        }
    };
    let push_url = build_app_push_url(&owner, &repo_name, &install_token.token);

    let merge_target = default_target_branch(&spec.project_id, app_state).await;

    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    // No-commits guard. A task whose run produced no git diff (e.g. its
    // deliverable was a memory/notes import, not code) leaves task_branch with
    // zero commits ahead of the base. GitHub's create_pull_request then 422s
    // ("No commits between <base> and task/<id>"), which the coordinator
    // surfaces as a persistent "PR blocked" health banner and re-attempts every
    // tick. There is nothing to open a PR with, so close the task as completed
    // (Close is valid from any non-closed status — covers both the supervisor
    // body's in_progress/verifying caller and the coordinator's approved
    // re-cycle caller) and report the run as Closed rather than Failed.
    match task_branch_commits_ahead(
        mirror.as_ref(),
        &spec.project_id,
        &spec.task_branch,
        &merge_target,
    )
    .await
    {
        Ok(0) => {
            tracing::info!(
                task_id = %task.id,
                task_branch = %spec.task_branch,
                base = %merge_target,
                "supervisor PR-open: task_branch has no commits ahead of base — \
                 nothing to open a PR with; closing the task as completed"
            );
            let reason = "no code changes were produced, so there is nothing to open a \
                          pull request with (e.g. a memory/notes-only task); closing as completed";
            if let Err(e) = task_repo
                .transition(
                    &task.id,
                    TransitionAction::Close,
                    "supervisor",
                    "system",
                    Some(reason),
                    None,
                )
                .await
            {
                tracing::warn!(
                    task_id = %task.id,
                    error = %e,
                    "supervisor PR-open: no-commits close transition skipped"
                );
            }
            return TaskRunOutcome::Closed {
                reason: reason.to_string(),
            };
        }
        Ok(_) => {}
        Err(e) => {
            // Don't block the PR on a precheck failure — fall through to the
            // normal push/open path (which will surface any real error).
            tracing::warn!(
                task_id = %task.id,
                error = %e,
                "supervisor PR-open: commits-ahead precheck failed; proceeding with normal PR path"
            );
        }
    }

    let commit_type = if task.issue_type == "task" {
        "chore"
    } else {
        "feat"
    };
    let message = format!("{}({}): {}", commit_type, task.short_id, task.title);

    // Push the worker's task_branch from the mirror to the GitHub remote
    // so the PR's head ref exists. We deliberately do NOT call
    // squash_merge_via_mirror here — it pushes the squashed commit directly
    // to refs/heads/main, which branch-protected repos (Quality Gate etc.)
    // reject ("Changes must be made through a pull request"). The PR flow
    // below handles landing the change properly via human/CI review.
    let head_sha = match push_task_branch_to_github(
        mirror.as_ref(),
        &spec.project_id,
        &spec.task_branch,
        &push_url,
    )
    .await
    {
        Ok(sha) => sha,
        Err(e) => {
            return TaskRunOutcome::Failed {
                stage: "pr_open".into(),
                provider_failure: None,
                reason: format!("push task_branch to GitHub failed: {e}"),
            };
        }
    };
    let _ = &message;
    let merge_result_commit_sha = head_sha;

    let pr_title = format!("{}({}): {}", commit_type, task.short_id, task.title);
    let pr_body = format!(
        "## Summary\n{description}\n\n---\nDjinn task: {short_id}",
        description = task.description,
        short_id = task.short_id,
    );

    let github_client = GitHubApiClient::for_installation(installation_id);
    let head_ref = format!("{owner}:{}", spec.task_branch);

    let existing_pr = match github_client
        .list_pulls_by_head_with_state(&owner, &repo_name, &head_ref, "all")
        .await
    {
        Ok(prs) => prs.into_iter().next(),
        Err(e) => {
            tracing::warn!(
                task_id = %task.id,
                error = %e,
                "supervisor PR-open: list_pulls_by_head_with_state failed; creating a new PR"
            );
            None
        }
    };

    let pr = if let Some(existing) = existing_pr {
        if existing.state == PrState::Open {
            existing
        } else {
            match github_client
                .reopen_pull_request(&owner, &repo_name, existing.number)
                .await
            {
                Ok(reopened) => reopened,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.id,
                        pr_number = existing.number,
                        error = %e,
                        "supervisor PR-open: failed to reopen closed PR; creating a new one"
                    );
                    match github_client
                        .create_pull_request(
                            &owner,
                            &repo_name,
                            CreatePrParams {
                                title: pr_title.clone(),
                                body: pr_body.clone(),
                                head: spec.task_branch.clone(),
                                base: merge_target.clone(),
                                maintainer_can_modify: Some(true),
                                draft: Some(true),
                            },
                        )
                        .await
                    {
                        Ok(pr) => pr,
                        Err(e) => {
                            return TaskRunOutcome::Failed {
                                stage: "pr_open".into(),
                                provider_failure: None,
                                reason: format!("GitHub PR creation failed: {e}"),
                            };
                        }
                    }
                }
            }
        }
    } else {
        match github_client
            .create_pull_request(
                &owner,
                &repo_name,
                CreatePrParams {
                    title: pr_title,
                    body: pr_body,
                    head: spec.task_branch.clone(),
                    base: merge_target,
                    maintainer_can_modify: Some(true),
                    draft: Some(true),
                },
            )
            .await
        {
            Ok(pr) => pr,
            Err(e) => {
                return TaskRunOutcome::Failed {
                    stage: "pr_open".into(),
                    provider_failure: None,
                    reason: format!("GitHub PR creation failed: {e}"),
                };
            }
        }
    };

    if let Err(e) = task_repo.set_pr_url(&task.id, &pr.html_url).await {
        tracing::warn!(
            task_id = %task.id,
            error = %e,
            "supervisor PR-open: failed to store pr_url on task (non-fatal)"
        );
    }

    // Walk the task into the PR-aware status (approved → pr_draft) so the
    // host's dispatcher stops treating it as "still open work" and
    // re-spawning a worker the moment supervisor_pr_open returns. The
    // post-PR side (PrUndraft / PrMerge / PrCiFailed / PrChangesRequested)
    // is driven by `pr_poller` on every coordinator tick.
    if let Err(e) = task_repo
        .transition(
            &task.id,
            djinn_core::models::TransitionAction::PrCreated,
            "supervisor",
            "system",
            None,
            None,
        )
        .await
    {
        tracing::warn!(
            task_id = %task.id,
            error = %e,
            "supervisor PR-open: pr_created transition skipped (task may not be in approved state — check earlier stage-loop transitions)"
        );
    }

    tracing::info!(
        task_id = %task.short_id,
        pr_url = %pr.html_url,
        pr_number = pr.number,
        commit_sha = %merge_result_commit_sha,
        "Supervisor: PR opened"
    );

    TaskRunOutcome::PrOpened {
        url: pr.html_url,
        sha: merge_result_commit_sha,
    }
}

/// Push the worker's task_branch from the mirror clone up to GitHub via
/// the App-installation push URL. Returns the HEAD SHA on success.
///
/// **Concurrent-push race**: the coordinator's tick-driven dispatch path and
/// the supervisor body's own `open_pr` can fire simultaneously for the same
/// task — they both clone the mirror and push the same SHA. GitHub's
/// `git-receive-pack` serializes refs and rejects the loser with:
///
/// ```text
/// ! [remote rejected] task/X -> task/X (cannot lock ref '...': reference already exists)
/// ```
///
/// When that happens we re-check the remote ref: if it already points at our
/// local SHA the race winner pushed identical content and the operation is
/// effectively a no-op — return success. Only when the remote SHA differs do
/// we propagate the error.
/// Count commits on `task_branch` that are not already reachable from `base`.
///
/// Clones the mirror ephemerally on `task_branch` (the same `--local --shared`
/// clone the push path uses, so `origin/<base>` is present as a remote-tracking
/// ref) and runs `git rev-list --count origin/<base>..HEAD`. A result of `0`
/// means the branch has no new commits — there is nothing to open a PR with.
async fn task_branch_commits_ahead(
    mirror: &MirrorManager,
    project_id: &str,
    task_branch: &str,
    base: &str,
) -> Result<u64, GitError> {
    use djinn_git::run_git_command;
    let workspace = mirror
        .clone_ephemeral(project_id, task_branch)
        .await
        .map_err(|e| GitError::Other(anyhow::anyhow!("clone_ephemeral {task_branch}: {e}")))?;
    let out = run_git_command(
        workspace.path_buf(),
        vec![
            "rev-list".into(),
            "--count".into(),
            format!("origin/{base}..HEAD"),
        ],
    )
    .await?;
    let count = out.stdout.trim().parse::<u64>().map_err(|e| {
        GitError::Other(anyhow::anyhow!(
            "rev-list --count returned unparseable output {:?}: {e}",
            out.stdout
        ))
    })?;
    Ok(count)
}

async fn push_task_branch_to_github(
    mirror: &MirrorManager,
    project_id: &str,
    task_branch: &str,
    push_url: &str,
) -> Result<String, GitError> {
    use djinn_git::run_git_command;
    let workspace = mirror
        .clone_ephemeral(project_id, task_branch)
        .await
        .map_err(|e| GitError::Other(anyhow::anyhow!("clone_ephemeral {task_branch}: {e}")))?;
    let wt = workspace.path_buf();

    // Capture local HEAD up-front so the race-recovery path can verify the
    // remote against it without a second `rev-parse`.
    let head_out = run_git_command(
        wt.clone(),
        vec!["rev-parse".into(), "HEAD".into()],
    )
    .await?;
    let local_sha = head_out.stdout.trim().to_string();

    // Plain `--force` (not `--force-with-lease`). The push target is a
    // direct GitHub URL, not a configured remote — so there's no
    // remote-tracking ref for `--force-with-lease` to compare against,
    // and git rejects with `[rejected] task/X -> task/X (stale info)`
    // every time. We unconditionally own the task_branch on origin
    // (only djinn pushes there); `--force` is the right semantic.
    let push_result = run_git_command(
        wt.clone(),
        vec![
            "push".into(),
            "--force".into(),
            push_url.to_string(),
            format!("{task_branch}:refs/heads/{task_branch}"),
        ],
    )
    .await;

    match push_result {
        Ok(_) => Ok(local_sha),
        Err(e) if is_concurrent_push_race(&e) => {
            // Race-loser path. Verify the remote ref already matches our
            // SHA — if so, the parallel push landed our exact content and
            // we treat this as success.
            let ls = run_git_command(
                wt.clone(),
                vec![
                    "ls-remote".into(),
                    push_url.to_string(),
                    format!("refs/heads/{task_branch}"),
                ],
            )
            .await?;
            let remote_sha = ls
                .stdout
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_string();
            if remote_sha == local_sha {
                tracing::info!(
                    task_branch,
                    sha = %local_sha,
                    "push_task_branch_to_github: concurrent push race detected; remote already at our SHA — treating as success"
                );
                Ok(local_sha)
            } else {
                tracing::warn!(
                    task_branch,
                    local_sha = %local_sha,
                    remote_sha = %remote_sha,
                    "push_task_branch_to_github: concurrent push race with different remote SHA — propagating error"
                );
                Err(e)
            }
        }
        Err(e) => Err(e),
    }
}

/// Recognise GitHub's "cannot lock ref" rejection, which surfaces when two
/// processes push to the same task branch within the same ref-transaction
/// window. See `push_task_branch_to_github` for the recovery path.
fn is_concurrent_push_race(err: &GitError) -> bool {
    let msg = err.to_string();
    msg.contains("cannot lock ref") && msg.contains("reference already exists")
}

#[cfg(test)]
mod tests {
    use super::is_concurrent_push_race;
    use djinn_git::GitError;

    #[test]
    fn detects_real_github_lock_rejection() {
        let err = GitError::Other(anyhow::anyhow!(
            "git command failed (exit 1) in /tmp/.tmpDq5yoG: git push --force ... task/uots:refs/heads/task/uots\n \
             ! [remote rejected]   task/uots -> task/uots (cannot lock ref 'refs/heads/task/uots': reference already exists)"
        ));
        assert!(is_concurrent_push_race(&err));
    }

    #[test]
    fn ignores_unrelated_push_failures() {
        let err = GitError::Other(anyhow::anyhow!("auth failed: permission denied"));
        assert!(!is_concurrent_push_race(&err));
    }

    #[test]
    fn requires_both_fragments() {
        // Just "cannot lock ref" (e.g. a local refs-database fsck) without the
        // "reference already exists" qualifier is a different problem.
        let err = GitError::Other(anyhow::anyhow!("cannot lock ref 'foo': corrupted"));
        assert!(!is_concurrent_push_race(&err));
    }
}

#[cfg(test)]
mod commits_ahead_tests {
    //! Regression for the no-commits PR guard: `task_branch_commits_ahead`
    //! must report 0 for a branch identical to the base (the case that made
    //! create_pull_request 422 and spammed the "PR blocked" banner) and the
    //! real count otherwise.
    use super::task_branch_commits_ahead;
    use djinn_git::run_git_command;
    use djinn_workspace::MirrorManager;
    use std::path::Path;
    use tempfile::TempDir;

    async fn git(dir: &Path, args: &[&str]) {
        run_git_command(dir.to_path_buf(), args.iter().map(|s| s.to_string()).collect())
            .await
            .unwrap_or_else(|e| panic!("git {args:?} failed: {e}"));
    }

    /// Seed a mirror at `<root>/<pid>.git` with `main`, a `task/empty` branch
    /// pointing at the same commit as `main`, and a `task/withcommit` branch
    /// carrying one extra commit.
    async fn seed_mirror(root: &Path, pid: &str) {
        let mirror = root.join(format!("{pid}.git"));
        std::fs::create_dir_all(&mirror).unwrap();
        git(&mirror, &["init", "-b", "main"]).await;
        git(&mirror, &["config", "user.email", "t@example.com"]).await;
        git(&mirror, &["config", "user.name", "t"]).await;
        std::fs::write(mirror.join("README.md"), "base").unwrap();
        git(&mirror, &["add", "-A"]).await;
        git(&mirror, &["commit", "-m", "base"]).await;
        // task/empty == main (no new commits ahead).
        git(&mirror, &["branch", "task/empty"]).await;
        // task/withcommit carries one extra commit.
        git(&mirror, &["checkout", "-b", "task/withcommit"]).await;
        std::fs::write(mirror.join("change.txt"), "x").unwrap();
        git(&mirror, &["add", "-A"]).await;
        git(&mirror, &["commit", "-m", "change"]).await;
        git(&mirror, &["checkout", "main"]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn zero_when_branch_has_no_new_commits() {
        let root = TempDir::new().unwrap();
        seed_mirror(root.path(), "proj1").await;
        let mgr = MirrorManager::new(root.path());
        let n = task_branch_commits_ahead(&mgr, "proj1", "task/empty", "main")
            .await
            .unwrap();
        assert_eq!(n, 0, "a branch identical to base must report 0 commits ahead");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn counts_new_commits_ahead_of_base() {
        let root = TempDir::new().unwrap();
        seed_mirror(root.path(), "proj2").await;
        let mgr = MirrorManager::new(root.path());
        let n = task_branch_commits_ahead(&mgr, "proj2", "task/withcommit", "main")
            .await
            .unwrap();
        assert_eq!(n, 1, "a branch with one extra commit must report 1");
    }
}
