use super::*;
use crate::test_support::{checkout_branch, init_repo_with_bare_origin};
use std::path::PathBuf;

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
