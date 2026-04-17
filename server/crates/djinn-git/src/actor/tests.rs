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

