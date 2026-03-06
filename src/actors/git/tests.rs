use super::*;
use tempfile::TempDir;

/// Spin up a GitActorHandle on the server's own repo and verify basic reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_from_server_repo() {
    let repo_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let handle = GitActorHandle::spawn(repo_path).expect("failed to spawn actor");

    let branch = handle.current_branch().await.expect("current_branch");
    assert!(!branch.is_empty(), "branch name should be non-empty");

    let commit = handle.head_commit().await.expect("head_commit");
    assert_eq!(commit.sha.len(), 40, "SHA should be 40 hex chars");

    let status = handle.status().await.expect("status");
    drop(status);
}

/// Verify that RunCommand works for a read-only git command.
#[tokio::test]
async fn run_command_git_log() {
    let repo_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let handle = GitActorHandle::spawn(repo_path).expect("spawn");

    let out = handle
        .run_command(vec!["log".into(), "--oneline".into(), "-1".into()])
        .await
        .expect("git log");
    assert!(!out.stdout.is_empty(), "git log should produce output");
}

// ── Branch management tests ───────────────────────────────────────────────

/// Create a local repo with an initial commit on `main` and a local bare remote.
/// Both TempDirs must be kept alive for the test duration.
fn setup_git_repo() -> (TempDir, TempDir) {
    let remote_dir = tempfile::tempdir().unwrap();
    let local_dir = tempfile::tempdir().unwrap();

    // Init bare remote.
    std::process::Command::new("git")
        .args(["init", "--bare"])
        .current_dir(remote_dir.path())
        .output()
        .unwrap();

    // Init local repo.
    std::process::Command::new("git")
        .args(["init"])
        .current_dir(local_dir.path())
        .output()
        .unwrap();

    // Set identity and disable GPG signing (required for `git commit` in CI).
    for (k, v) in [
        ("user.email", "test@test.com"),
        ("user.name", "Test User"),
        ("commit.gpgsign", "false"),
    ] {
        std::process::Command::new("git")
            .args(["config", k, v])
            .current_dir(local_dir.path())
            .output()
            .unwrap();
    }

    // Initial commit.
    std::fs::write(local_dir.path().join("README.md"), "hello").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(local_dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "init"])
        .current_dir(local_dir.path())
        .output()
        .unwrap();

    // Rename default branch to `main`.
    std::process::Command::new("git")
        .args(["branch", "-m", "main"])
        .current_dir(local_dir.path())
        .output()
        .unwrap();

    // Wire up local bare remote and push.
    std::process::Command::new("git")
        .args([
            "remote",
            "add",
            "origin",
            remote_dir.path().to_str().unwrap(),
        ])
        .current_dir(local_dir.path())
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["push", "-u", "origin", "main"])
        .current_dir(local_dir.path())
        .output()
        .unwrap();

    (local_dir, remote_dir)
}

/// `create_branch` creates `task/{short_id}` from `main` and pushes to origin (GIT-01).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_branch_creates_and_pushes() {
    let (local, _remote) = setup_git_repo();
    let handle = GitActorHandle::spawn(local.path().to_path_buf()).unwrap();

    handle.create_branch("abc1", "main").await.unwrap();

    // Branch ref exists locally.
    let out = handle
        .run_command(vec!["branch".into(), "--list".into(), "task/abc1".into()])
        .await
        .unwrap();
    assert!(out.stdout.contains("task/abc1"));

    // create_branch only creates the ref — HEAD stays on main.
    let branch = handle.current_branch().await.unwrap();
    assert_eq!(branch, "main");
}

/// `squash_merge` produces a single commit on the target branch (GIT-03).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn squash_merge_produces_single_commit() {
    let (local, _remote) = setup_git_repo();
    let path = local.path();

    // Set up a feature branch with two commits (setup outside the actor).
    std::process::Command::new("git")
        .args(["checkout", "-b", "task/xyz1", "main"])
        .current_dir(path)
        .output()
        .unwrap();

    for (file, content, msg) in [
        ("feat.txt", "feature content", "feat: add file"),
        ("feat.txt", "updated content", "feat: update file"),
    ] {
        std::fs::write(path.join(file), content).unwrap();
        std::process::Command::new("git")
            .args(["add", "."])
            .current_dir(path)
            .output()
            .unwrap();
        std::process::Command::new("git")
            .args(["commit", "-m", msg])
            .current_dir(path)
            .output()
            .unwrap();
    }

    // Squash-merge via actor.
    let handle = GitActorHandle::spawn(path.to_path_buf()).unwrap();
    let result = handle
        .squash_merge("task/xyz1", "main", "feat: squashed feature")
        .await
        .unwrap();

    assert_eq!(result.commit_sha.len(), 40, "commit SHA should be 40 chars");

    // Squash merge pushes to origin/main. Fetch and check the remote ref.
    handle
        .run_command(vec!["fetch".into(), "origin".into()])
        .await
        .unwrap();
    let log = handle
        .run_command(vec!["log".into(), "--oneline".into(), "origin/main".into()])
        .await
        .unwrap();
    let lines: Vec<&str> = log.stdout.lines().collect();
    assert_eq!(
        lines.len(),
        2,
        "origin/main should have init + squash commit"
    );
    assert!(lines[0].contains("squashed feature"));
}

/// `delete_branch` removes the local branch (GIT-03 post-merge cleanup).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_branch_removes_local() {
    let (local, _remote) = setup_git_repo();
    let path = local.path();

    // Create a branch manually and return to main.
    std::process::Command::new("git")
        .args(["checkout", "-b", "task/del1", "main"])
        .current_dir(path)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["checkout", "main"])
        .current_dir(path)
        .output()
        .unwrap();

    let handle = GitActorHandle::spawn(path.to_path_buf()).unwrap();
    handle.delete_branch("task/del1").await.unwrap();

    // Branch should no longer exist locally.
    let out = handle
        .run_command(vec!["branch".into(), "--list".into(), "task/del1".into()])
        .await
        .unwrap();
    assert!(out.stdout.trim().is_empty(), "branch should be deleted");
}

/// `squash_merge` returns `CommitRejected` when `git commit` is rejected (GIT-07).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn squash_merge_surfaces_commit_rejection() {
    let (local, _remote) = setup_git_repo();
    let path = local.path();

    // Install a pre-commit hook that always fails.
    let hooks_dir = path.join(".git").join("hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let hook_path = hooks_dir.join("pre-commit");
    std::fs::write(&hook_path, "#!/bin/sh\necho 'lint failed'\nexit 1\n").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // Create a feature branch with a commit (committed before hook was installed).
    std::process::Command::new("git")
        .args(["checkout", "-b", "task/hook1", "main"])
        .current_dir(path)
        .output()
        .unwrap();
    std::fs::write(path.join("x.txt"), "x").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "add x", "--no-verify"])
        .current_dir(path)
        .output()
        .unwrap();

    let handle = GitActorHandle::spawn(path.to_path_buf()).unwrap();
    let err = handle
        .squash_merge("task/hook1", "main", "feat: trigger hook")
        .await
        .unwrap_err();

    assert!(
        matches!(err, GitError::CommitRejected { .. }),
        "expected CommitRejected, got: {err}"
    );
    if let GitError::CommitRejected { stdout, stderr, .. } = err {
        let output = format!("{stdout}\n{stderr}");
        assert!(
            output.contains("lint failed"),
            "hook output should be surfaced"
        );
    }
}

/// `squash_merge` is idempotent when task branch has no delta vs target.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn squash_merge_returns_head_when_no_changes_to_commit() {
    let (local, _remote) = setup_git_repo();
    let path = local.path();

    std::process::Command::new("git")
        .args(["branch", "task/noop1", "main"])
        .current_dir(path)
        .output()
        .unwrap();

    let handle = GitActorHandle::spawn(path.to_path_buf()).unwrap();
    let before = handle
        .run_command(vec!["rev-parse".into(), "main".into()])
        .await
        .unwrap()
        .stdout
        .trim()
        .to_string();

    let merged = handle
        .squash_merge("task/noop1", "main", "chore: noop merge")
        .await
        .unwrap();

    assert_eq!(merged.commit_sha, before);
}

/// `squash_merge` returns `MergeConflict` and conflicting files when conflicts occur.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn squash_merge_surfaces_merge_conflict_files() {
    let (local, _remote) = setup_git_repo();
    let path = local.path();

    // Branch A changes the same line.
    std::process::Command::new("git")
        .args(["checkout", "-b", "task/conflict1", "main"])
        .current_dir(path)
        .output()
        .unwrap();
    std::fs::write(path.join("shared.txt"), "from-task\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "task change"])
        .current_dir(path)
        .output()
        .unwrap();

    // Main changes the same line differently.
    std::process::Command::new("git")
        .args(["checkout", "main"])
        .current_dir(path)
        .output()
        .unwrap();
    std::fs::write(path.join("shared.txt"), "from-main\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "."])
        .current_dir(path)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args(["commit", "-m", "main change"])
        .current_dir(path)
        .output()
        .unwrap();
    // Push so that origin/main has the conflicting change (squash_merge
    // uses a detached worktree from origin/<target>).
    std::process::Command::new("git")
        .args(["push", "origin", "main"])
        .current_dir(path)
        .output()
        .unwrap();

    let handle = GitActorHandle::spawn(path.to_path_buf()).unwrap();
    let err = handle
        .squash_merge("task/conflict1", "main", "feat: conflict")
        .await
        .unwrap_err();

    match err {
        GitError::MergeConflict {
            target_branch,
            files,
        } => {
            assert_eq!(target_branch, "main");
            assert!(files.iter().any(|f| f == "shared.txt"));
        }
        other => panic!("expected MergeConflict, got: {other}"),
    }
}

// ── Worktree lifecycle tests (GIT-02, GIT-06) ────────────────────────────

/// `create_worktree` creates the `.djinn/worktrees/{id}/` directory on the given branch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_worktree_creates_directory() {
    let (local, _remote) = setup_git_repo();
    let path = local.path();

    // Create a task branch for the worktree to check out.
    std::process::Command::new("git")
        .args(["branch", "task/wt1", "main"])
        .current_dir(path)
        .output()
        .unwrap();

    let handle = GitActorHandle::spawn(path.to_path_buf()).unwrap();
    let wt_path = handle
        .create_worktree("wt1", "task/wt1", false)
        .await
        .unwrap();

    // Directory must exist.
    assert!(wt_path.exists(), "worktree directory should exist");
    assert!(wt_path.ends_with(".djinn/worktrees/wt1"));

    // The worktree should be on the correct branch.
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(&wt_path)
        .output()
        .unwrap();
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    assert_eq!(branch, "task/wt1");
}

/// `remove_worktree` deletes the worktree directory and prunes metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remove_worktree_cleans_up() {
    let (local, _remote) = setup_git_repo();
    let path = local.path();

    std::process::Command::new("git")
        .args(["branch", "task/wt2", "main"])
        .current_dir(path)
        .output()
        .unwrap();

    let handle = GitActorHandle::spawn(path.to_path_buf()).unwrap();
    let wt_path = handle
        .create_worktree("wt2", "task/wt2", false)
        .await
        .unwrap();
    assert!(wt_path.exists(), "precondition: worktree should exist");

    handle.remove_worktree(&wt_path).await.unwrap();
    assert!(!wt_path.exists(), "worktree directory should be removed");
}

/// `list_worktrees` returns both the main worktree and any task worktrees.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn list_worktrees_includes_main_and_task() {
    let (local, _remote) = setup_git_repo();
    let path = local.path();

    std::process::Command::new("git")
        .args(["branch", "task/wt3", "main"])
        .current_dir(path)
        .output()
        .unwrap();

    let handle = GitActorHandle::spawn(path.to_path_buf()).unwrap();
    let wt_path = handle
        .create_worktree("wt3", "task/wt3", false)
        .await
        .unwrap();

    let worktrees = handle.list_worktrees().await.unwrap();
    assert!(
        worktrees.len() >= 2,
        "should have at least main + task worktree, got {}",
        worktrees.len()
    );

    // Main worktree should be present with branch "main".
    let main_wt = worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some("main"));
    assert!(main_wt.is_some(), "main worktree should be listed");

    // Task worktree should be present at the expected path.
    let task_wt = worktrees.iter().find(|w| w.path == wt_path);
    assert!(task_wt.is_some(), "task worktree should be listed");
    let task_wt = task_wt.unwrap();
    assert_eq!(task_wt.branch.as_deref(), Some("task/wt3"));
    assert_eq!(task_wt.head.len(), 40, "HEAD should be a 40-char SHA");
}
