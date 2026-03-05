use super::*;

impl GitActor {
    /// Create a worktree at `{repo}/.djinn/worktrees/{task_short_id}/` on `branch` (GIT-02).
    ///
    /// Prunes stale worktree metadata first (GIT-06) so leftover entries from
    /// crashed sessions don't block the new checkout.
    pub(super) async fn create_worktree_impl(
        path: PathBuf,
        task_short_id: String,
        branch: String,
        detach: bool,
    ) -> Result<PathBuf, GitError> {
        // GIT-06: prune stale worktree bookkeeping before creating.
        let _ = Self::run_git_command(path.clone(), vec!["worktree".into(), "prune".into()]).await;

        let wt_path = path.join(".djinn").join("worktrees").join(&task_short_id);

        let mut args = vec!["worktree".into(), "add".into()];
        if detach {
            args.push("--detach".into());
        }
        args.push(wt_path.to_str().unwrap_or_default().into());
        args.push(branch);

        Self::run_git_command(path, args).await?;

        Ok(wt_path)
    }

    /// Remove a worktree by path and prune stale entries (GIT-06).
    pub(super) async fn remove_worktree_impl(
        path: PathBuf,
        wt_path: PathBuf,
    ) -> Result<(), GitError> {
        Self::run_git_command(
            path.clone(),
            vec![
                "worktree".into(),
                "remove".into(),
                "--force".into(),
                wt_path.to_str().unwrap_or_default().into(),
            ],
        )
        .await?;

        // GIT-06: prune after removal to clean up any remaining metadata.
        let _ = Self::run_git_command(path, vec!["worktree".into(), "prune".into()]).await;

        Ok(())
    }

    /// List all worktrees with structured metadata (GIT-02).
    ///
    /// Parses `git worktree list --porcelain` which emits blocks separated by
    /// blank lines, each containing `worktree <path>`, `HEAD <sha>`, and
    /// optionally `branch refs/heads/<name>`.
    pub(super) async fn list_worktrees_impl(path: PathBuf) -> Result<Vec<WorktreeInfo>, GitError> {
        let out = Self::run_git_command(
            path,
            vec!["worktree".into(), "list".into(), "--porcelain".into()],
        )
        .await?;

        let mut worktrees = Vec::new();
        let mut wt_path: Option<PathBuf> = None;
        let mut head: Option<String> = None;
        let mut branch: Option<String> = None;

        for line in out.stdout.lines() {
            if line.is_empty() {
                // End of a worktree block — flush.
                if let (Some(p), Some(h)) = (wt_path.take(), head.take()) {
                    worktrees.push(WorktreeInfo {
                        path: p,
                        branch: branch.take(),
                        head: h,
                    });
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("worktree ") {
                wt_path = Some(PathBuf::from(rest));
            } else if let Some(rest) = line.strip_prefix("HEAD ") {
                head = Some(rest.to_string());
            } else if let Some(rest) = line.strip_prefix("branch refs/heads/") {
                branch = Some(rest.to_string());
            }
        }

        // Flush last block (porcelain output may not end with a blank line).
        if let (Some(p), Some(h)) = (wt_path, head) {
            worktrees.push(WorktreeInfo {
                path: p,
                branch: branch.take(),
                head: h,
            });
        }

        Ok(worktrees)
    }
}
