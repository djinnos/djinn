//! Tests for worktree pre-dispatch sync behavior.

use super::worktree::try_rebase_existing_task_branch;
use crate::test_helpers::{agent_context_from_db, create_test_db};
use std::path::PathBuf;
use tempfile::TempDir;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Create a temporary git repository with a main branch and initial commit.
async fn create_test_repo() -> TempDir {
    let temp_dir = TempDir::new().unwrap();
    let path = temp_dir.path();

    // Initialize repo
    let output = Command::new("git")
        .args(["init"])
        .current_dir(&path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    // Configure git user
    let _ = Command::new("git")
        .args(["config", "user.email", "test@test.com"])
        .current_dir(&path)
        .output()
        .await;
    let _ = Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(&path)
        .output()
        .await;

    // Create initial file and commit on main
    tokio::fs::write(path.join("file.txt"), "main content v1")
        .await
        .unwrap();
    let output = Command::new("git")
        .args(["add", "file.txt"])
        .current_dir(&path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    let output = Command::new("git")
        .args(["commit", "-m", "initial commit"])
        .current_dir(&path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    // Create main branch explicitly
    let _ = Command::new("git")
        .args(["branch", "-M", "main"])
        .current_dir(&path)
        .output()
        .await;

    temp_dir
}

/// Create a task branch with a commit based on the original main.
async fn create_task_branch(repo_path: &std::path::Path, task_short_id: &str) -> PathBuf {
    // Create task branch
    let output = Command::new("git")
        .args(["checkout", "-b", &format!("task/{task_short_id}")])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    // Add a commit on the task branch
    tokio::fs::write(repo_path.join("file.txt"), "task branch content")
        .await
        .unwrap();
    let output = Command::new("git")
        .args(["add", "file.txt"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    let output = Command::new("git")
        .args(["commit", "-m", "task branch commit"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    // Go back to main
    let output = Command::new("git")
        .args(["checkout", "main"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    // Create worktree for the task
    let worktrees_dir = repo_path.join(".djinn").join("worktrees");
    tokio::fs::create_dir_all(&worktrees_dir).await.unwrap();

    let worktree_path = worktrees_dir.join(task_short_id);
    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            worktree_path.to_str().unwrap(),
            &format!("task/{task_short_id}"),
        ])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "worktree add failed: {:?}", output);

    worktree_path
}

/// Advance main branch with a conflicting change.
async fn advance_main_with_conflict(repo_path: &std::path::Path) {
    // Checkout main
    let output = Command::new("git")
        .args(["checkout", "main"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    // Make conflicting change to the same line
    tokio::fs::write(repo_path.join("file.txt"), "main content v2 - CONFLICT")
        .await
        .unwrap();
    let output = Command::new("git")
        .args(["add", "file.txt"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    let output = Command::new("git")
        .args(["commit", "-m", "main conflicting commit"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
}

/// Advance main branch with a non-conflicting change.
async fn advance_main_clean(repo_path: &std::path::Path) {
    // Checkout main
    let output = Command::new("git")
        .args(["checkout", "main"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    // Add a new file (non-conflicting)
    tokio::fs::write(repo_path.join("new_file.txt"), "new content")
        .await
        .unwrap();
    let output = Command::new("git")
        .args(["add", "new_file.txt"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    let output = Command::new("git")
        .args(["commit", "-m", "main new file commit"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());
}

#[tokio::test]
async fn resumed_worktree_sync_clean_rebase_succeeds() {
    // Setup: create repo with task branch and worktree
    let repo = create_test_repo().await;
    let repo_path = repo.path();
    let task_short_id = "abc123";
    let worktree_path = create_task_branch(repo_path, task_short_id).await;

    // Advance main with non-conflicting changes
    advance_main_clean(repo_path).await;

    // Setup app state
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    // Sync the resumed worktree - should succeed cleanly
    try_rebase_existing_task_branch(
        repo_path,
        &format!("task/{task_short_id}"),
        "main",
        Some(&worktree_path),
        &state,
    )
    .await;

    // Verify: worktree should have the synced content (rebased on latest main)
    let content = tokio::fs::read_to_string(worktree_path.join("file.txt"))
        .await
        .unwrap();
    assert_eq!(content, "task branch content");

    // New file from main should also exist
    let new_content = tokio::fs::read_to_string(worktree_path.join("new_file.txt"))
        .await
        .unwrap();
    assert_eq!(new_content, "new content");

    // Verify no conflict markers in the file
    assert!(!content.contains("<<<<<<<"));
    assert!(!content.contains(">>>>>>>"));
}

#[tokio::test]
async fn resumed_worktree_sync_conflict_leaves_markers() {
    // Setup: create repo with task branch and worktree
    let repo = create_test_repo().await;
    let repo_path = repo.path();
    let task_short_id = "def456";
    let worktree_path = create_task_branch(repo_path, task_short_id).await;

    // Advance main with CONFLICTING changes to the same file
    advance_main_with_conflict(repo_path).await;

    // Setup app state
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    // Sync the resumed worktree - should leave conflict markers
    try_rebase_existing_task_branch(
        repo_path,
        &format!("task/{task_short_id}"),
        "main",
        Some(&worktree_path),
        &state,
    )
    .await;

    // Verify: conflict markers should be present in the worktree
    let content = tokio::fs::read_to_string(worktree_path.join("file.txt"))
        .await
        .unwrap();

    // Should have conflict markers for worker to resolve
    assert!(
        content.contains("<<<<<<<"),
        "Expected conflict markers in file, got: {}",
        content
    );
    assert!(
        content.contains(">>>>>>>"),
        "Expected conflict end markers in file, got: {}",
        content
    );
    assert!(
        content.contains("task branch content"),
        "Expected task branch content in conflict, got: {}",
        content
    );
    assert!(
        content.contains("main content v2"),
        "Expected main content in conflict, got: {}",
        content
    );
}

#[tokio::test]
async fn fresh_branch_sync_uses_temp_worktree() {
    // Setup: create repo with task branch (no existing worktree)
    let repo = create_test_repo().await;
    let repo_path = repo.path();
    let task_short_id = "ghi789";

    // Create task branch but NO worktree yet
    let output = Command::new("git")
        .args(["checkout", "-b", &format!("task/{task_short_id}")])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    tokio::fs::write(repo_path.join("file.txt"), "task branch content")
        .await
        .unwrap();
    let output = Command::new("git")
        .args(["add", "file.txt"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    let output = Command::new("git")
        .args(["commit", "-m", "task branch commit"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    // Go back to main
    let output = Command::new("git")
        .args(["checkout", "main"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    // Advance main
    advance_main_clean(repo_path).await;

    // Setup app state
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    // Sync the fresh branch (no resumed worktree path)
    try_rebase_existing_task_branch(
        repo_path,
        &format!("task/{task_short_id}"),
        "main",
        None, // Fresh branch - no resumed worktree
        &state,
    )
    .await;

    // The fresh branch sync uses a temp worktree to attempt rebase.
    // It tests whether the rebase would succeed but doesn't actually
    // modify the original branch (the worker will work on a new worktree).
    // We just verify the sync completed without panicking.
}

#[tokio::test]
async fn fresh_branch_sync_no_conflict_when_no_changes() {
    // Setup: create repo with task branch (no existing worktree)
    let repo = create_test_repo().await;
    let repo_path = repo.path();
    let task_short_id = "jkl012";

    // Create task branch with NO changes yet (just branched from main)
    let output = Command::new("git")
        .args(["checkout", "-b", &format!("task/{task_short_id}")])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    // Go back to main
    let output = Command::new("git")
        .args(["checkout", "main"])
        .current_dir(&repo_path)
        .output()
        .await
        .unwrap();
    assert!(output.status.success());

    // Advance main
    advance_main_clean(repo_path).await;

    // Setup app state
    let db = create_test_db();
    let state = agent_context_from_db(db.clone(), CancellationToken::new());

    // Sync the fresh branch (no resumed worktree path)
    try_rebase_existing_task_branch(
        repo_path,
        &format!("task/{task_short_id}"),
        "main",
        None, // Fresh branch
        &state,
    )
    .await;

    // Should complete without errors - no real sync needed since
    // task branch has no commits diverging from main
}
