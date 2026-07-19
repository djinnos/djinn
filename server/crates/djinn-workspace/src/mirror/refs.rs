//! Read-only ref inspection against a project's bare mirror.
//!
//! Both helpers here answer a question about `refs/heads/*` in the mirror
//! without cloning and without mutating anything. They are split out of
//! `mirror.rs` because that file sits near the repository's per-file size
//! guard (`scripts/check-file-size.sh`), and because "probe a ref, classify
//! the answer" is a cohesive concern separate from mirror lifecycle
//! (create/fetch/gc/clone).
//!
//! These are inherent methods on [`MirrorManager`], so the public API is
//! unchanged by living here — callers still write `mgr.branch_exists(..)`.

use super::*;

impl MirrorManager {
    /// Cheap host-side check that `branch`'s commits are durably present in the
    /// mirror AND carry work not already on `base` — i.e. `branch` has at least
    /// one commit beyond its merge-base with `base`.
    ///
    /// Used by the stage-aware-resume decision (`supervisor_runner`): a
    /// reviewer-stage run that died after the worker already pushed its commits
    /// to `task_branch` can be resumed at the reviewer instead of redoing the
    /// worker — but ONLY if that output is actually durable. A missing branch
    /// (first cycle, or the worker never pushed) or a branch with nothing ahead
    /// of base means there is no worker diff to review, so the caller must fall
    /// back to the full worker redo.
    ///
    /// `base` having moved on does NOT invalidate the worker's output. This
    /// deliberately does NOT require `base` to be an ancestor of `branch`
    /// (fast-forwardability): on a busy board some other task's PR merges into
    /// base during nearly every review-cycle run, so an is-ancestor probe reads
    /// "diverged" on each redispatch and the worker redo loops forever — a
    /// livelock where fleet throughput itself wedges every review-stage task
    /// (the t9wi/32bk wedge, 2026-06-11). The reviewer reviews the diff against
    /// the merge-base; integrating a moved base is the merge/PR stage's job.
    ///
    /// Reads the bare mirror directly (no clone): resolves both refs and runs
    /// `git rev-list --count base..branch`. Any resolution failure (mirror
    /// missing, branch absent) yields `false` — the safe answer is "not
    /// durable, redo the worker".
    pub async fn branch_ahead_of_base(&self, project_id: &str, branch: &str, base: &str) -> bool {
        let mirror = self.mirror_path(project_id);
        if !mirror.exists() {
            return false;
        }
        let rev_parse = |refname: String| {
            let mirror = mirror.clone();
            async move {
                run_git_command(
                    mirror,
                    vec![
                        "rev-parse".into(),
                        "--verify".into(),
                        "--quiet".into(),
                        refname,
                    ],
                )
                .await
                .ok()
                .map(|o| o.stdout.trim().to_string())
                .filter(|s| !s.is_empty())
            }
        };
        let (Some(branch_sha), Some(base_sha)) = (
            rev_parse(format!("refs/heads/{branch}")).await,
            rev_parse(format!("refs/heads/{base}")).await,
        ) else {
            return false;
        };
        // Equal heads = no worker diff to review.
        if branch_sha == base_sha {
            return false;
        }
        // `rev-list --count base..branch` counts commits reachable from
        // `branch` but not from `base` — the worker's durable output beyond
        // the merge-base. Any positive count means there is a diff for the
        // reviewer, regardless of whether `base` has since moved on (the
        // branch may be "behind" base and still carry reviewable work).
        run_git_command(
            mirror,
            vec![
                "rev-list".into(),
                "--count".into(),
                format!("{base_sha}..{branch_sha}"),
            ],
        )
        .await
        .ok()
        .and_then(|o| o.stdout.trim().parse::<u64>().ok())
        .is_some_and(|ahead| ahead > 0)
    }

    /// Does `refs/heads/{branch}` exist in the bare mirror for `project_id`?
    ///
    /// Exists so a caller can classify a failed
    /// [`MirrorManager::clone_ephemeral`] into "the branch genuinely does not
    /// exist yet" versus "a transient failure". That classification CANNOT be
    /// done on the [`MirrorError`] variant: `MirrorError::Missing` means the
    /// mirror DIRECTORY is absent, while an absent branch fails inside
    /// `git clone --branch` (`fatal: Remote branch <b> not found in upstream
    /// origin`) and arrives as the same `MirrorError::Git` as a genuinely
    /// transient git failure. Only a direct ref probe separates them.
    ///
    /// Probes with `git show-ref --verify`, which asks purely about ref
    /// existence. A ref that exists but does not resolve (partially fetched /
    /// corrupt mirror) exits 128 and surfaces as `Err` — deliberately NOT
    /// `Ok(false)`, because "I cannot answer" must never be read as "the
    /// branch is absent".
    ///
    /// A missing mirror is likewise an `Err(MirrorError::Missing)`: there is
    /// no repository to answer the question against.
    pub async fn branch_exists(&self, project_id: &str, branch: &str) -> Result<bool, MirrorError> {
        let mirror = self.mirror_path(project_id);
        if !mirror.exists() {
            return Err(MirrorError::Missing(project_id.to_string()));
        }
        let out = run_git_command(
            mirror,
            vec![
                "show-ref".into(),
                "--verify".into(),
                "--quiet".into(),
                format!("refs/heads/{branch}"),
            ],
        )
        .await;
        match out {
            Ok(_) => Ok(true),
            // `show-ref --verify --quiet` exits 1 (silently) when the ref does
            // not exist. That is the genuine-absence answer, not a failure.
            Err(djinn_git::GitError::CommandFailed { code: 1, .. }) => Ok(false),
            Err(e) => Err(git_err_to_mirror("git show-ref --verify", e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn git(repo: &Path, args: &[&str]) {
        run_git_command(
            repo.to_path_buf(),
            args.iter().map(|s| s.to_string()).collect(),
        )
        .await
        .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    }

    /// Build a bare mirror under `{root}/{project_id}.git` seeded with `main`
    /// and (optionally) a `task` branch one commit ahead. Returns the
    /// `MirrorManager`.
    async fn seed_mirror(root: &Path, project_id: &str, with_ahead_task: bool) -> MirrorManager {
        let mirror = root.join(format!("{project_id}.git"));
        git(
            root,
            &["init", "--bare", "--quiet", mirror.to_str().unwrap()],
        )
        .await;

        // Working clone to author commits, then push refs into the bare mirror.
        let work = TempDir::new().unwrap();
        git(
            root,
            &[
                "clone",
                "--quiet",
                mirror.to_str().unwrap(),
                work.path().to_str().unwrap(),
            ],
        )
        .await;
        let wp = work.path();
        git(wp, &["config", "user.email", "t@t"]).await;
        git(wp, &["config", "user.name", "t"]).await;
        git(wp, &["checkout", "-q", "-b", "main"]).await;
        git(wp, &["commit", "--allow-empty", "-qm", "base"]).await;
        git(wp, &["push", "-q", "origin", "main"]).await;

        if with_ahead_task {
            git(wp, &["checkout", "-q", "-b", "task"]).await;
            git(wp, &["commit", "--allow-empty", "-qm", "worker work"]).await;
            git(wp, &["push", "-q", "origin", "task"]).await;
        }

        MirrorManager::new(root.to_path_buf())
    }

    #[tokio::test]
    async fn true_when_task_branch_is_ahead_of_base() {
        let root = TempDir::new().unwrap();
        let mgr = seed_mirror(root.path(), "p1", true).await;
        assert!(
            mgr.branch_ahead_of_base("p1", "task", "main").await,
            "task branch one commit ahead of main must read as durable"
        );
    }

    #[tokio::test]
    async fn true_when_base_advanced_past_task_branch() {
        // The review-cycle livelock case (t9wi/32bk, 2026-06-11): the worker
        // pushed durable commits, then ANOTHER task's PR merged into base, so
        // base is no longer an ancestor of the task branch. The worker output
        // is still durable and reviewable — this must read as durable, or
        // every redispatch on a busy board redoes the worker forever.
        let root = TempDir::new().unwrap();
        let mgr = seed_mirror(root.path(), "p5", true).await;

        // Advance base past the point the task branch forked from.
        let work = TempDir::new().unwrap();
        git(
            root.path(),
            &[
                "clone",
                "--quiet",
                root.path().join("p5.git").to_str().unwrap(),
                work.path().to_str().unwrap(),
            ],
        )
        .await;
        let wp = work.path();
        git(wp, &["config", "user.email", "t@t"]).await;
        git(wp, &["config", "user.name", "t"]).await;
        git(wp, &["checkout", "-q", "main"]).await;
        git(wp, &["commit", "--allow-empty", "-qm", "other task merged"]).await;
        git(wp, &["push", "-q", "origin", "main"]).await;

        assert!(
            mgr.branch_ahead_of_base("p5", "task", "main").await,
            "a task branch with durable commits must read as durable even \
             when base has moved on (diverged ≠ no work to review)"
        );
    }

    #[tokio::test]
    async fn false_when_task_branch_absent() {
        // First-cycle / worker-never-pushed: no task branch → not durable.
        let root = TempDir::new().unwrap();
        let mgr = seed_mirror(root.path(), "p2", false).await;
        assert!(!mgr.branch_ahead_of_base("p2", "task", "main").await);
    }

    #[tokio::test]
    async fn false_when_task_branch_equals_base() {
        // Worker pushed nothing new (branch == base HEAD) → no diff to review.
        let root = TempDir::new().unwrap();
        let mirror = root.path().join("p3.git");
        git(
            root.path(),
            &["init", "--bare", "--quiet", mirror.to_str().unwrap()],
        )
        .await;
        let work = TempDir::new().unwrap();
        git(
            root.path(),
            &[
                "clone",
                "--quiet",
                mirror.to_str().unwrap(),
                work.path().to_str().unwrap(),
            ],
        )
        .await;
        let wp = work.path();
        git(wp, &["config", "user.email", "t@t"]).await;
        git(wp, &["config", "user.name", "t"]).await;
        git(wp, &["checkout", "-q", "-b", "main"]).await;
        git(wp, &["commit", "--allow-empty", "-qm", "base"]).await;
        git(wp, &["branch", "task"]).await;
        git(wp, &["push", "-q", "origin", "main", "task"]).await;

        let mgr = MirrorManager::new(root.path().to_path_buf());
        assert!(!mgr.branch_ahead_of_base("p3", "task", "main").await);
    }

    #[tokio::test]
    async fn false_when_mirror_missing() {
        let root = TempDir::new().unwrap();
        let mgr = MirrorManager::new(root.path().to_path_buf());
        assert!(!mgr.branch_ahead_of_base("absent", "task", "main").await);
    }

    #[tokio::test]
    async fn branch_exists_true_for_present_branch() {
        let root = TempDir::new().unwrap();
        let mgr = seed_mirror(root.path(), "be1", true).await;
        assert!(mgr.branch_exists("be1", "task").await.expect("probe"));
        assert!(mgr.branch_exists("be1", "main").await.expect("probe"));
    }

    #[tokio::test]
    async fn branch_exists_false_for_absent_branch() {
        // The genuine first-cycle shape: mirror is healthy, task branch has
        // never been pushed. This is the ONLY case that may fall back to base.
        let root = TempDir::new().unwrap();
        let mgr = seed_mirror(root.path(), "be2", false).await;
        assert!(!mgr.branch_exists("be2", "task").await.expect("probe"));
    }

    #[tokio::test]
    async fn branch_exists_errs_when_mirror_missing() {
        let root = TempDir::new().unwrap();
        let mgr = MirrorManager::new(root.path().to_path_buf());
        assert!(
            matches!(
                mgr.branch_exists("absent", "task").await,
                Err(MirrorError::Missing(_))
            ),
            "a missing mirror cannot answer the question and must not read as \
             'branch absent'"
        );
    }

    #[tokio::test]
    async fn branch_exists_errs_when_ref_is_unresolvable() {
        // Partially fetched / corrupt mirror: the ref file exists but points
        // at an object the mirror does not have. `git clone --branch` fails
        // here, and this probe must NOT answer `Ok(false)` — reading that as
        // "first cycle" is exactly what rewinds the task branch to base.
        let root = TempDir::new().unwrap();
        let mgr = seed_mirror(root.path(), "be3", false).await;
        let broken = root.path().join("be3.git/refs/heads/task");
        tokio::fs::create_dir_all(broken.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(
            &broken,
            "0000000000000000000000000000000000000001\n".as_bytes(),
        )
        .await
        .unwrap();

        assert!(
            mgr.branch_exists("be3", "task").await.is_err(),
            "an unresolvable ref must surface as an error, never as absence"
        );
    }
}
