//! End-to-end smoke test: source repo → bare mirror → ephemeral clone → commit.

use std::path::Path;

use djinn_workspace::{CommitOutcome, GitIdentity, MirrorManager};
use tempfile::TempDir;
use tokio::process::Command;

async fn run(cmd: &[&str], cwd: &Path) {
    let output = Command::new(cmd[0])
        .args(&cmd[1..])
        .current_dir(cwd)
        .output()
        .await
        .expect("git");
    assert!(
        output.status.success(),
        "cmd {cmd:?} failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn make_source_repo(path: &Path) {
    run(&["git", "init", "-b", "main"], path).await;
    run(&["git", "config", "user.email", "test@example.com"], path).await;
    run(&["git", "config", "user.name", "Test"], path).await;
    tokio::fs::write(path.join("README.md"), "hello")
        .await
        .unwrap();
    run(&["git", "add", "."], path).await;
    run(&["git", "commit", "-m", "init"], path).await;
}

#[tokio::test]
async fn mirror_clone_commit_cycle() {
    let source_dir = TempDir::new().unwrap();
    make_source_repo(source_dir.path()).await;
    let source_url = format!("file://{}", source_dir.path().display());

    let mirrors_dir = TempDir::new().unwrap();
    let mgr = MirrorManager::new(mirrors_dir.path().to_path_buf());

    let project_id = "proj-abc";
    mgr.ensure_mirror(project_id, &source_url).await.unwrap();
    assert!(mgr.mirror_path(project_id).exists());

    // Idempotent
    mgr.ensure_mirror(project_id, &source_url).await.unwrap();

    // Fetch is a no-op against an up-to-date mirror; reports no changes.
    let changed = mgr.fetch_mirror(project_id, &source_url).await.unwrap();
    assert!(!changed, "no-op fetch must report no ref advance");

    // New upstream commit → fetch reports changes.
    tokio::fs::write(source_dir.path().join("new.txt"), "added")
        .await
        .unwrap();
    run(&["git", "add", "."], source_dir.path()).await;
    run(&["git", "commit", "-m", "add new"], source_dir.path()).await;
    let changed = mgr.fetch_mirror(project_id, &source_url).await.unwrap();
    assert!(
        changed,
        "fetch after upstream commit must report a ref advance"
    );

    let ws = mgr.clone_ephemeral(project_id, "main").await.unwrap();
    assert!(ws.path().join("README.md").exists());
    assert_eq!(ws.branch(), "main");

    let id = GitIdentity {
        name: "djinn-bot",
        email: "bot@example.com",
    };

    tokio::fs::write(ws.path().join("hello.txt"), "world")
        .await
        .unwrap();
    let made = ws.commit("wip", id).await.unwrap();
    assert!(
        matches!(made, CommitOutcome::Committed { .. }),
        "expected a commit since hello.txt was added; got {made:?}"
    );

    // Clean tree → no commit.
    let made_again = ws.commit("empty", id).await.unwrap();
    assert!(
        matches!(made_again, CommitOutcome::NoChanges),
        "clean tree should not produce a commit; got {made_again:?}"
    );
}

/// Worker-side push path: after committing inside the ephemeral clone,
/// `Workspace::push_to_origin(branch)` must land the commit on the same
/// branch in the bare mirror so the host's `squash_merge_via_mirror`
/// (which fetches from the mirror) can see it.
#[tokio::test]
async fn push_to_origin_lands_worker_commit_in_mirror() {
    // Same boilerplate as `mirror_clone_commit_cycle`: tiny source repo
    // → bare mirror → ephemeral clone.
    let source_dir = TempDir::new().unwrap();
    make_source_repo(source_dir.path()).await;
    let source_url = format!("file://{}", source_dir.path().display());

    let mirrors_dir = TempDir::new().unwrap();
    let mgr = MirrorManager::new(mirrors_dir.path().to_path_buf());

    let project_id = "proj-push";
    mgr.ensure_mirror(project_id, &source_url).await.unwrap();

    let ws = mgr.clone_ephemeral(project_id, "main").await.unwrap();

    // Create a task-style branch off main, commit a file on it, then
    // push back to origin (the bare mirror).
    let task_branch = "djinn/task-push-fixture";
    run(&["git", "checkout", "-b", task_branch], ws.path()).await;
    tokio::fs::write(ws.path().join("from-worker.txt"), "ship it")
        .await
        .unwrap();
    let id = GitIdentity {
        name: "djinn-bot",
        email: "bot@example.com",
    };
    let made = ws.commit("worker stage", id).await.unwrap();
    assert!(
        matches!(made, CommitOutcome::Committed { .. }),
        "expected a commit since from-worker.txt was added; got {made:?}"
    );

    ws.push_to_origin(task_branch)
        .await
        .expect("push to origin");

    // The mirror must now have the task_branch ref pointing at the
    // worker's commit. `git rev-parse refs/heads/{branch}` inside the
    // bare mirror is the smoking-gun assertion.
    let mirror_path = mgr.mirror_path(project_id);
    let out = Command::new("git")
        .arg("-C")
        .arg(&mirror_path)
        .arg("rev-parse")
        .arg(format!("refs/heads/{task_branch}"))
        .output()
        .await
        .expect("git rev-parse");
    assert!(
        out.status.success(),
        "mirror missing task_branch ref after push: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mirror_sha = String::from_utf8_lossy(&out.stdout).trim().to_string();

    // And it must match HEAD of the workspace (the worker's commit).
    let head = Command::new("git")
        .arg("-C")
        .arg(ws.path())
        .arg("rev-parse")
        .arg("HEAD")
        .output()
        .await
        .expect("git rev-parse HEAD");
    let worker_sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
    assert_eq!(
        mirror_sha, worker_sha,
        "mirror task_branch must point at the worker's HEAD"
    );

    // Idempotent: a second push with no new commits is a no-op (exit 0).
    ws.push_to_origin(task_branch)
        .await
        .expect("idempotent re-push");
}

#[tokio::test]
async fn clone_nonexistent_mirror_returns_missing() {
    let mirrors_dir = TempDir::new().unwrap();
    let mgr = MirrorManager::new(mirrors_dir.path().to_path_buf());

    let err = mgr
        .clone_ephemeral("no-such-project", "main")
        .await
        .unwrap_err();
    assert!(
        matches!(err, djinn_workspace::MirrorError::Missing(ref p) if p == "no-such-project"),
        "unexpected error: {err:?}"
    );
}
