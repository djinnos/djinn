//! Focused tests for the resume-via-git worktree-setup helpers added by
//! `twsk`:
//!
//! - [`MirrorManager::clone_ephemeral_at_ref`] — clones the mirror and checks
//!   out an arbitrary ref/SHA in detached HEAD, used for accepted auto-submit,
//!   safe checkpoint, and alternate checkpoint ref selections.
//! - [`Workspace::checkout_ref`] — moves an existing workspace's HEAD to an
//!   arbitrary ref/SHA in detached mode, used by the supervisor's
//!   post-clone detach step.
//! - Fallback: a selected ref that is missing from the mirror must yield a
//!   typed `MirrorError::Git` so callers can fall back to the legacy
//!   task-branch path without panicking.

use std::path::Path;

use djinn_workspace::MirrorManager;
use tempfile::TempDir;
use tokio::process::Command;

async fn run(cmd: &[&str], cwd: &Path) {
    let output = Command::new(cmd[0])
        .args(&cmd[1..])
        .current_dir(cwd)
        .output()
        .await
        .expect("git spawn");
    assert!(
        output.status.success(),
        "cmd {cmd:?} failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn git_stdout(cmd: &[&str], cwd: &Path) -> String {
    let output = Command::new(cmd[0])
        .args(&cmd[1..])
        .current_dir(cwd)
        .output()
        .await
        .expect("git spawn");
    assert!(
        output.status.success(),
        "cmd {cmd:?} failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

async fn seed_source_repo(path: &Path) {
    run(&["git", "init", "-b", "main", "-q"], path).await;
    run(&["git", "config", "user.email", "test@example.com"], path).await;
    run(&["git", "config", "user.name", "Test"], path).await;
    tokio::fs::write(path.join("README.md"), "v1")
        .await
        .unwrap();
    run(&["git", "add", "."], path).await;
    run(&["git", "commit", "-m", "v1"], path).await;
}

/// Build a tiny mirror from a `source_dir`, push a second commit, and create
/// an alternate checkpoint ref `refs/djinn/checkpoints/task-1/session-1`
/// pointing at the latest commit. Returns the project_id and the SHA of the
/// checkpoint commit.
async fn build_mirror_with_checkpoint(mirrors_dir: &Path, source_dir: &Path) -> (String, String) {
    let mgr = MirrorManager::new(mirrors_dir.to_path_buf());
    let project_id = "proj-resume".to_string();
    let source_url = format!("file://{}", source_dir.display());
    mgr.ensure_mirror(&project_id, &source_url).await.unwrap();

    // Advance the source so there are two distinct commits to checkpoint from.
    tokio::fs::write(source_dir.join("new.txt"), "checkpoint content")
        .await
        .unwrap();
    run(&["git", "add", "."], source_dir).await;
    run(&["git", "commit", "-m", "checkpoint candidate"], source_dir).await;
    mgr.fetch_mirror(&project_id, &source_url).await.unwrap();

    // Lift the latest commit into an alternate checkpoint ref so the resume
    // selector can point at it without using `main`. This mirrors the real
    // push-conflict alternate ref flow from sibling task `8yjx`.
    let tip = git_stdout(&["git", "rev-parse", "HEAD"], source_dir).await;
    let alt_ref = "refs/djinn/checkpoints/task-1/session-1";
    run(
        &[
            "git",
            "push",
            &format!("file://{}", mgr.mirror_path(&project_id).display()),
            &format!("{tip}:{alt_ref}"),
        ],
        source_dir,
    )
    .await;

    (project_id, tip)
}

/// Accepted auto-submit / safe task-branch checkpoint selection:
/// `clone_ephemeral_at_ref("main")` checks out the latest `main` HEAD into a
/// detached HEAD workspace — the exact shape the resume path needs when the
/// selector picks a safe checkpoint on the task branch.
#[tokio::test]
async fn clone_at_branch_ref_checks_out_tip_in_detached_head() {
    let source_dir = TempDir::new().unwrap();
    seed_source_repo(source_dir.path()).await;
    tokio::fs::write(source_dir.path().join("new.txt"), "advance")
        .await
        .unwrap();
    run(&["git", "add", "."], source_dir.path()).await;
    run(&["git", "commit", "-m", "advance"], source_dir.path()).await;

    let mirrors_dir = TempDir::new().unwrap();
    let (project_id, _tip) =
        build_mirror_with_checkpoint(mirrors_dir.path(), source_dir.path()).await;
    let mgr = MirrorManager::new(mirrors_dir.path().to_path_buf());

    let ws = mgr
        .clone_ephemeral_at_ref(&project_id, "main")
        .await
        .expect("clone-at-ref");
    assert!(
        ws.path().join("new.txt").exists(),
        "main tip must be checked out"
    );
    let head_sha = git_stdout(&["git", "rev-parse", "HEAD"], ws.path()).await;
    let main_sha = git_stdout(
        &["git", "rev-parse", "refs/heads/main"],
        &mgr.mirror_path(&project_id),
    )
    .await;
    assert_eq!(
        head_sha, main_sha,
        "detached HEAD must equal origin/main tip"
    );

    // Subsequent ensure_branch(task_branch) must succeed on the detached HEAD.
    ws.ensure_branch("task/resume")
        .await
        .expect("ensure_branch");
    assert_eq!(
        git_stdout(&["git", "rev-parse", "HEAD"], ws.path()).await,
        main_sha
    );
}

/// Alternate checkpoint ref selection: `clone_ephemeral_at_ref` accepts a
/// fully-qualified `refs/djinn/checkpoints/...` ref and checks it out in the
/// resulting clone. This is the resume-selected-source path for the
/// push-conflict alternate ref shape.
#[tokio::test]
async fn clone_at_alternate_checkpoint_ref_lands_on_checkpoint_commit() {
    let source_dir = TempDir::new().unwrap();
    seed_source_repo(source_dir.path()).await;
    let mirrors_dir = TempDir::new().unwrap();
    let (project_id, tip) =
        build_mirror_with_checkpoint(mirrors_dir.path(), source_dir.path()).await;
    let mgr = MirrorManager::new(mirrors_dir.path().to_path_buf());

    let ws = mgr
        .clone_ephemeral_at_ref(&project_id, "refs/djinn/checkpoints/task-1/session-1")
        .await
        .expect("clone-at-alt");
    assert_eq!(
        git_stdout(&["git", "rev-parse", "HEAD"], ws.path()).await,
        tip,
        "alternate checkpoint ref must drive HEAD to its commit"
    );
}

/// Selected ref unavailable fallback: an unknown ref name surfaces a typed
/// `MirrorError::Git` so the supervisor can fall back to the legacy
/// task-branch path. The test asserts that the call returns `Err` rather
/// than panicking.
#[tokio::test]
async fn clone_at_missing_ref_errors_without_panic() {
    let source_dir = TempDir::new().unwrap();
    seed_source_repo(source_dir.path()).await;
    let mirrors_dir = TempDir::new().unwrap();
    let (project_id, _tip) =
        build_mirror_with_checkpoint(mirrors_dir.path(), source_dir.path()).await;
    let mgr = MirrorManager::new(mirrors_dir.path().to_path_buf());

    let err = mgr
        .clone_ephemeral_at_ref(&project_id, "refs/djinn/checkpoints/does/not/exist")
        .await
        .expect_err("missing ref must error so caller can fall back");
    let msg = format!("{err}");
    assert!(
        msg.contains("(at-ref)"),
        "error must identify the resume-setup helper so the supervisor can classify the fallback reason, got: {msg}"
    );
}

/// `Workspace::checkout_ref` moves HEAD to the chosen ref in detached mode
/// without touching any branch ref, exactly as the resume path needs
/// between `clone_ephemeral(task_branch)` (which leaves HEAD on `task_branch`)
/// and the supervisor's `ensure_branch(task_branch)` step.
#[tokio::test]
async fn checkout_ref_detaches_head_onto_selected_sha() {
    let source_dir = TempDir::new().unwrap();
    seed_source_repo(source_dir.path()).await;

    // Materialize an ephemeral clone on `main` via MirrorManager so the
    // hardlinked object db stays consistent with production callers.
    let mirrors_root = TempDir::new().unwrap();
    let mgr = MirrorManager::new(mirrors_root.path().to_path_buf());
    let project_id = "proj-fetch".to_string();
    mgr.ensure_mirror(
        &project_id,
        &format!("file://{}", source_dir.path().display()),
    )
    .await
    .unwrap();
    let ws = mgr.clone_ephemeral(&project_id, "main").await.unwrap();
    let clone_original_head = git_stdout(&["git", "rev-parse", "HEAD"], ws.path()).await;

    // Advance the source so the local clone's HEAD differs from the new tip,
    // then push the new commit through the mirror.
    tokio::fs::write(source_dir.path().join("advance.txt"), "x")
        .await
        .unwrap();
    run(&["git", "add", "."], source_dir.path()).await;
    run(&["git", "commit", "-m", "advance"], source_dir.path()).await;
    mgr.fetch_mirror(
        &project_id,
        &format!("file://{}", source_dir.path().display()),
    )
    .await
    .unwrap();

    let tip = git_stdout(
        &["git", "rev-parse", "refs/heads/main"],
        &mgr.mirror_path(&project_id),
    )
    .await;
    ws.checkout_ref(&tip).await.expect("checkout detached");

    assert_eq!(
        git_stdout(&["git", "rev-parse", "HEAD"], ws.path()).await,
        tip,
        "checkout_ref must move HEAD to the chosen SHA"
    );

    // Local main ref must still point at the original clone HEAD; the
    // supervisor's follow-up `ensure_branch(task_branch)` is the call that
    // promotes HEAD onto the resumed branch.
    assert_ne!(
        clone_original_head, tip,
        "test fixture prerequisite: original clone HEAD must differ from the new tip"
    );
    assert_eq!(
        git_stdout(&["git", "rev-parse", "refs/heads/main"], ws.path(),).await,
        clone_original_head,
        "checkout_ref must not rewrite the existing local main ref"
    );
}
