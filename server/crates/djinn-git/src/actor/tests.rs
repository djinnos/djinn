use super::*;
use std::path::PathBuf;
use tempfile::TempDir;

/// Walk up from `start` to find the nearest ancestor directory containing `.git`.
fn find_git_root(start: &std::path::Path) -> PathBuf {
    start
        .ancestors()
        .find(|p| p.join(".git").exists())
        .expect("no git repo found above CARGO_MANIFEST_DIR")
        .to_path_buf()
}

/// Spin up a GitActorHandle on the workspace repo and verify basic reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reads_from_server_repo() {
    let repo_path = find_git_root(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
    let handle = GitActorHandle::spawn(repo_path).expect("failed to spawn actor");

    let branch = handle.current_branch().await.expect("current_branch");
    assert!(!branch.is_empty(), "branch name should be non-empty");

    let commit = handle.head_commit().await.expect("head_commit");
    assert_eq!(commit.sha.len(), 40, "SHA should be 40 hex chars");

    let status = handle.status().await.expect("status");
    drop(status);
}

/// Verify that RunCommand works for a read-only git command.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_command_git_log() {
    let repo_path = find_git_root(std::path::Path::new(env!("CARGO_MANIFEST_DIR")));
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
    // Canonicalize both sides because macOS tempdir /var/folders/ is a symlink
    // to /private/var/folders/, and git resolves symlinks in porcelain output.
    let wt_canonical = wt_path.canonicalize().unwrap_or(wt_path.clone());
    let task_wt = worktrees
        .iter()
        .find(|w| w.path.canonicalize().unwrap_or(w.path.clone()) == wt_canonical);
    assert!(task_wt.is_some(), "task worktree should be listed");
    let task_wt = task_wt.unwrap();
    assert_eq!(task_wt.branch.as_deref(), Some("task/wt3"));
    assert_eq!(task_wt.head.len(), 40, "HEAD should be a 40-char SHA");
}
