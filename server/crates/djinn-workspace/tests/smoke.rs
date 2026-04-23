//! End-to-end smoke test: source repo → bare mirror → ephemeral clone → commit.

use std::path::Path;

use djinn_workspace::{GitIdentity, MirrorManager};
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
    assert!(changed, "fetch after upstream commit must report a ref advance");

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
    assert!(made, "expected a commit since hello.txt was added");

    // Clean tree → no commit.
    let made_again = ws.commit("empty", id).await.unwrap();
    assert!(!made_again, "clean tree should not produce a commit");
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
