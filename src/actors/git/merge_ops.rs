use super::*;

impl GitActor {
    /// Squash-merge `branch` into `target_branch` with `message` (GIT-03).
    ///
    /// Commit-failure awareness (GIT-07): any non-zero exit from `git commit`
    /// is wrapped in `GitError::CommitRejected` with exact stdout/stderr.
    pub(super) async fn squash_merge_impl(
        path: PathBuf,
        branch: String,
        target_branch: String,
        message: String,
    ) -> Result<MergeResult, GitError> {
        // Retry the entire merge+push cycle on non-fast-forward push failures
        // (main moved between our fetch and push). Max 3 attempts.
        const MERGE_PUSH_MAX_ATTEMPTS: u32 = 3;
        let mut last_error: Option<GitError> = None;

        for attempt in 1..=MERGE_PUSH_MAX_ATTEMPTS {
            // Fetch latest from remote.
            let _ = Self::run_git_command(
                path.clone(),
                vec!["fetch".into(), "origin".into(), target_branch.clone()],
            )
            .await;

            // Rebase the task branch onto origin/<target> before merging.
            // This auto-resolves divergence that git can handle, reducing
            // false merge conflicts when main has moved forward.
            let origin_ref = format!("origin/{target_branch}");
            let rebase_wt_name = format!(
                ".rebase-{}-{}",
                branch.replace('/', "-"),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            );
            let rebase_wt_path = path.join(".djinn").join("worktrees").join(&rebase_wt_name);
            let rebase_wt = rebase_wt_path.to_string_lossy().to_string();
            if Self::run_git_command(
                path.clone(),
                vec![
                    "worktree".into(),
                    "add".into(),
                    rebase_wt.clone(),
                    branch.clone(),
                ],
            )
            .await
            .is_ok()
            {
                let rebase_ok = Self::run_git_command(
                    rebase_wt_path.clone(),
                    vec!["rebase".into(), origin_ref.clone()],
                )
                .await
                .is_ok();
                if !rebase_ok {
                    // Abort failed rebase; the squash merge will report the real conflict.
                    let _ = Self::run_git_command(
                        rebase_wt_path.clone(),
                        vec!["rebase".into(), "--abort".into()],
                    )
                    .await;
                }
                let _ = Self::run_git_command(
                    path.clone(),
                    vec![
                        "worktree".into(),
                        "remove".into(),
                        "--force".into(),
                        rebase_wt,
                    ],
                )
                .await;
            }

            let temp_name = format!(
                ".merge-{}-{}",
                target_branch,
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis())
                    .unwrap_or(0)
            );
            let merge_wt_path = path.join(".djinn").join("worktrees").join(temp_name);
            let merge_wt = merge_wt_path.to_string_lossy().to_string();
            let origin_target = format!("origin/{target_branch}");

            let add_result = Self::run_git_command(
                path.clone(),
                vec![
                    "worktree".into(),
                    "add".into(),
                    "--detach".into(),
                    merge_wt.clone(),
                    origin_target,
                ],
            )
            .await;

            add_result?;

            let merge_result = Self::squash_merge_detached_worktree_impl(
                path.clone(),
                merge_wt_path.clone(),
                branch.clone(),
                target_branch.clone(),
                message.clone(),
            )
            .await;

            let _ = Self::run_git_command(
                path.clone(),
                vec![
                    "worktree".into(),
                    "remove".into(),
                    "--force".into(),
                    merge_wt,
                ],
            )
            .await;
            let _ = Self::run_git_command(
                path.clone(),
                vec!["worktree".into(), "prune".into()],
            )
            .await;

            match merge_result {
                Ok(result) => return Ok(result),
                Err(ref e) if attempt < MERGE_PUSH_MAX_ATTEMPTS && is_non_fast_forward_error(e) => {
                    tracing::warn!(
                        attempt,
                        max_attempts = MERGE_PUSH_MAX_ATTEMPTS,
                        error = %e,
                        target_branch = %target_branch,
                        "push rejected (non-fast-forward); re-fetching and retrying merge"
                    );
                    last_error = Some(merge_result.unwrap_err());
                    let delay = retry_delay(attempt);
                    tokio::time::sleep(delay).await;
                    continue;
                }
                Err(e) => return Err(e),
            }
        }

        Err(last_error.unwrap_or_else(|| GitError::CommandFailed {
            code: 1,
            command: "squash_merge".into(),
            cwd: path.display().to_string(),
            stdout: String::new(),
            stderr: "exhausted merge-push retry attempts".into(),
        }))
    }

    pub(super) async fn squash_merge_detached_worktree_impl(
        repo_path: PathBuf,
        wt_path: PathBuf,
        branch: String,
        target_branch: String,
        message: String,
    ) -> Result<MergeResult, GitError> {
        // Stage all changes from the task branch as a squash (no commit yet).
        if let Err(err) = Self::run_git_command(
            wt_path.clone(),
            vec!["merge".into(), "--squash".into(), branch],
        )
        .await
        {
            if matches!(err, GitError::CommandFailed { .. }) {
                let files = Self::unmerged_files(wt_path.clone())
                    .await
                    .unwrap_or_default();
                let _ =
                    Self::run_git_command(wt_path, vec!["merge".into(), "--abort".into()]).await;
                if !files.is_empty() {
                    return Err(GitError::MergeConflict {
                        target_branch,
                        files,
                    });
                }
            }
            return Err(err);
        }

        let staged = Self::run_git_command(
            wt_path.clone(),
            vec!["diff".into(), "--cached".into(), "--name-only".into()],
        )
        .await?;
        if staged.stdout.trim().is_empty() {
            let out =
                Self::run_git_command(wt_path.clone(), vec!["rev-parse".into(), "HEAD".into()])
                    .await?;
            return Ok(MergeResult {
                commit_sha: out.stdout.trim().to_string(),
            });
        }

        // Commit.
        match Self::run_git_command(wt_path.clone(), vec!["commit".into(), "-m".into(), message])
            .await
        {
            Ok(_) => {}
            Err(GitError::CommandFailed {
                code,
                command,
                cwd,
                stdout,
                stderr,
            }) => {
                return Err(GitError::CommitRejected {
                    code,
                    command,
                    cwd,
                    stdout,
                    stderr,
                });
            }
            Err(e) => return Err(e),
        }

        // Read the resulting commit SHA.
        let out =
            Self::run_git_command(wt_path.clone(), vec!["rev-parse".into(), "HEAD".into()]).await?;
        let commit_sha = out.stdout.trim().to_string();

        // Push merge commit directly to upstream target branch.
        // Retry short-lived transport/ref-lock failures with jitter.
        let push_refspec = format!("{commit_sha}:refs/heads/{target_branch}");
        let mut last_push_error: Option<GitError> = None;
        for attempt in 1..=PUSH_MAX_ATTEMPTS {
            match Self::run_git_command(
                repo_path.clone(),
                vec!["push".into(), "origin".into(), push_refspec.clone()],
            )
            .await
            {
                Ok(_) => {
                    last_push_error = None;
                    break;
                }
                Err(e) if attempt < PUSH_MAX_ATTEMPTS && is_retryable_git_command_error(&e) => {
                    let delay = retry_delay(attempt);
                    tracing::warn!(
                        attempt,
                        max_attempts = PUSH_MAX_ATTEMPTS,
                        delay_ms = delay.as_millis() as u64,
                        error = %e,
                        target_branch = %target_branch,
                        "push failed during squash merge; retrying"
                    );
                    last_push_error = Some(e);
                    tokio::time::sleep(delay).await;
                }
                Err(e) => return Err(e),
            }
        }
        if let Some(e) = last_push_error {
            return Err(e);
        }

        Ok(MergeResult { commit_sha })
    }

    pub(super) async fn unmerged_files(path: PathBuf) -> Result<Vec<String>, GitError> {
        let out = Self::run_git_command(
            path,
            vec![
                "diff".into(),
                "--name-only".into(),
                "--diff-filter=U".into(),
            ],
        )
        .await?;
        Ok(out
            .stdout
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }
}
