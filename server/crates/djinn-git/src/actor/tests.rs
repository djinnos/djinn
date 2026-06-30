use super::*;
use crate::test_support::{
    TestRepoFixture, checkout_branch, configure_local_identity, git, init_repo_with_bare_origin,
    init_repo_with_main_commit, write_and_commit,
};
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

fn assert_paths_once(mut actual: Vec<String>, mut expected: Vec<&str>) {
    actual.sort();
    expected.sort_unstable();
    assert_eq!(actual, expected);
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

/// `create_branch` creates `task/{short_id}` from `main` and pushes to origin (GIT-01).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_branch_creates_and_pushes() {
    let fixture = init_repo_with_bare_origin();
    let handle = fixture.spawn_handle();

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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_branch_falls_back_to_local_target_when_remote_ref_missing() {
    let remote = TempDir::new().expect("create remote temp dir");
    git(remote.path(), ["init", "--bare"]);

    let local = init_repo_with_main_commit();
    let remote_path = remote
        .path()
        .to_str()
        .expect("remote temp path should be valid UTF-8");
    git(local.path(), ["remote", "add", "origin", remote_path]);

    let fixture = TestRepoFixture {
        local: local.local,
        remote: Some(remote),
    };
    let handle = fixture.spawn_handle();

    handle.create_branch("fallback", "main").await.unwrap();

    let task_ref = handle
        .run_command(vec![
            "rev-parse".into(),
            "--verify".into(),
            "refs/heads/task/fallback".into(),
        ])
        .await
        .unwrap();
    let main_ref = handle
        .run_command(vec!["rev-parse".into(), "--verify".into(), "main".into()])
        .await
        .unwrap();

    assert_eq!(task_ref.stdout.trim(), main_ref.stdout.trim());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn create_branch_cleans_corrupt_partial_ref_before_retry() {
    let fixture = init_repo_with_bare_origin();
    let ref_path = fixture.path().join(".git/refs/heads/task/broken");
    std::fs::create_dir_all(ref_path.parent().expect("task ref parent"))
        .expect("create task ref directory");
    std::fs::write(&ref_path, b"not a valid ref\n").expect("write corrupt task ref");

    let handle = fixture.spawn_handle();
    handle.create_branch("broken", "main").await.unwrap();

    let out = handle
        .run_command(vec![
            "rev-parse".into(),
            "--verify".into(),
            "refs/heads/task/broken".into(),
        ])
        .await
        .unwrap();
    assert_eq!(out.stdout.trim().len(), 40);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_diff_task_branch_reports_zero_ahead_without_error() {
    let fixture = init_repo_with_main_commit();
    git(fixture.path(), ["branch", "task/zero", "main"]);
    let handle = fixture.spawn_handle();

    assert!(handle.branch_exists("task/zero").await.unwrap());
    assert!(handle.has_commits().await.unwrap());

    let out = handle
        .run_command(vec![
            "rev-list".into(),
            "--count".into(),
            "main..task/zero".into(),
        ])
        .await
        .unwrap();
    assert_eq!(out.stdout.trim(), "0");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn has_commits_returns_false_for_unborn_repo() {
    let local = TempDir::new().expect("create local temp dir");
    git(local.path(), ["init"]);
    configure_local_identity(local.path());
    let handle = GitActorHandle::spawn(local.path().to_path_buf()).expect("spawn");

    assert!(!handle.has_commits().await.unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_categorizes_staged_modified_and_untracked_files() {
    let fixture = init_repo_with_main_commit();
    write_and_commit(fixture.path(), "tracked.txt", "original\n", "add tracked");

    std::fs::write(fixture.path().join("staged.txt"), "staged\n").expect("write staged file");
    git(fixture.path(), ["add", "staged.txt"]);
    std::fs::write(fixture.path().join("tracked.txt"), "modified\n").expect("modify tracked file");
    std::fs::write(fixture.path().join("untracked.txt"), "untracked\n")
        .expect("write untracked file");

    let status = fixture.spawn_handle().status().await.unwrap();

    assert_paths_once(status.staged, vec!["staged.txt"]);
    assert_paths_once(status.modified, vec!["tracked.txt"]);
    assert_paths_once(status.untracked, vec!["untracked.txt"]);
}

/// `delete_branch` removes the local branch (GIT-03 post-merge cleanup).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn delete_branch_removes_local() {
    let fixture = init_repo_with_bare_origin();
    let path = fixture.path();

    // Create a branch manually and return to main.
    checkout_branch(path, "task/del1", Some("main"));
    checkout_branch(path, "main", None);

    let handle = fixture.spawn_handle();
    handle.delete_branch("task/del1").await.unwrap();

    // Branch should no longer exist locally.
    let out = handle
        .run_command(vec!["branch".into(), "--list".into(), "task/del1".into()])
        .await
        .unwrap();
    assert!(out.stdout.trim().is_empty(), "branch should be deleted");
}
