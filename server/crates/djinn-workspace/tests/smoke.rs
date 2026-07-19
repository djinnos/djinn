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

// ─── push_to_origin non-fast-forward reconciliation ────────────────────────
//
// Regression cover for the task unmh failure (2026-07-19): the supervisor's
// clone fallback rewound the local task branch to base's HEAD, so every
// `push_to_origin` was rejected `(non-fast-forward) ... tip of your current
// branch is behind its remote counterpart`. Because the caller's retry loop is
// pure sleep-and-repeat, all three attempts failed identically. `push_to_origin`
// now reconciles by rebasing onto the remote tip once before giving up — and
// never by force-pushing over it, since the mirror's copy may be the only one.

/// Read `refs/heads/<branch>` out of the bare mirror.
async fn mirror_sha(mirror: &Path, branch: &str) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(mirror)
        .arg("rev-parse")
        .arg(format!("refs/heads/{branch}"))
        .output()
        .await
        .expect("git rev-parse");
    assert!(
        out.status.success(),
        "mirror has no {branch}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

const BOT: GitIdentity = GitIdentity {
    name: "djinn-bot",
    email: "bot@example.com",
};

/// Build a mirror whose `task` branch already carries a durable commit
/// (`from-earlier-cycle.txt`), then hand back a SECOND ephemeral clone whose
/// local `task` ref has been rewound to `main` — byte-for-byte the shape
/// `clone_ephemeral(base) + ensure_branch(task)` produces.
async fn mirror_with_rewound_second_clone(
    mirrors_dir: &Path,
    project_id: &str,
) -> (MirrorManager, djinn_workspace::Workspace) {
    let source_dir = TempDir::new().unwrap();
    make_source_repo(source_dir.path()).await;
    let mgr = MirrorManager::new(mirrors_dir.to_path_buf());
    mgr.ensure_mirror(
        project_id,
        &format!("file://{}", source_dir.path().display()),
    )
    .await
    .unwrap();

    // Cycle 1: a worker commits on `task` and pushes it to the mirror.
    let first = mgr.clone_ephemeral(project_id, "main").await.unwrap();
    run(&["git", "checkout", "-b", "task"], first.path()).await;
    tokio::fs::write(first.path().join("from-earlier-cycle.txt"), "durable\n")
        .await
        .unwrap();
    first.commit("cycle 1", BOT).await.unwrap();
    first.push_to_origin("task").await.expect("cycle 1 push");

    // Cycle 2: the clone lands on `main` and `checkout -B task` rewinds the
    // local ref to main's HEAD — strictly behind the mirror's `task`.
    let second = mgr.clone_ephemeral(project_id, "main").await.unwrap();
    run(&["git", "checkout", "-B", "task"], second.path()).await;

    (mgr, second)
}

/// The exact production shape: local `task` is strictly BEHIND the mirror and
/// has no commits of its own. The push must succeed by fast-forwarding onto the
/// remote tip, and the earlier cycle's commit must survive untouched.
#[tokio::test]
async fn push_to_origin_reconciles_when_local_branch_is_behind_remote() {
    let mirrors_dir = TempDir::new().unwrap();
    let (mgr, ws) = mirror_with_rewound_second_clone(mirrors_dir.path(), "proj-behind").await;
    let mirror = mgr.mirror_path("proj-behind");
    let before = mirror_sha(&mirror, "task").await;

    ws.push_to_origin("task")
        .await
        .expect("a behind-remote push must reconcile, not fail three times");

    assert_eq!(
        mirror_sha(&mirror, "task").await,
        before,
        "reconciliation must not move the remote tip when there is no local work"
    );
    assert!(
        ws.path().join("from-earlier-cycle.txt").exists(),
        "the earlier cycle's commit must be adopted into the workspace, not discarded"
    );
}

/// Diverged: the rewound local branch has its own commit touching a different
/// file. The rebase replays it on top of the remote tip, so the mirror ends up
/// with BOTH commits — the earlier cycle's work is never clobbered.
#[tokio::test]
async fn push_to_origin_rebases_local_work_onto_remote_tip() {
    let mirrors_dir = TempDir::new().unwrap();
    let (mgr, ws) = mirror_with_rewound_second_clone(mirrors_dir.path(), "proj-diverged").await;
    let mirror = mgr.mirror_path("proj-diverged");

    tokio::fs::write(ws.path().join("from-this-cycle.txt"), "new work\n")
        .await
        .unwrap();
    ws.commit("cycle 2", BOT).await.unwrap();

    ws.push_to_origin("task")
        .await
        .expect("diverged push must reconcile by rebase");

    // Both files must be reachable from the mirror's task tip.
    for file in ["from-earlier-cycle.txt", "from-this-cycle.txt"] {
        let out = Command::new("git")
            .arg("-C")
            .arg(&mirror)
            .arg("cat-file")
            .arg("-e")
            .arg(format!("refs/heads/task:{file}"))
            .output()
            .await
            .expect("git cat-file");
        assert!(
            out.status.success(),
            "{file} missing from the mirror's task tip after reconciliation — \
             remote or local work was discarded"
        );
    }
}

/// A genuine conflict must fail loudly with the reason attached, and MUST NOT
/// force-push: the mirror's tip stays exactly where it was.
#[tokio::test]
async fn push_to_origin_fails_loudly_when_rebase_conflicts() {
    let mirrors_dir = TempDir::new().unwrap();
    let (mgr, ws) = mirror_with_rewound_second_clone(mirrors_dir.path(), "proj-conflict").await;
    let mirror = mgr.mirror_path("proj-conflict");
    let before = mirror_sha(&mirror, "task").await;

    // Same path as the earlier cycle, different content → rebase conflict.
    tokio::fs::write(ws.path().join("from-earlier-cycle.txt"), "conflicting\n")
        .await
        .unwrap();
    ws.commit("cycle 2 conflicting", BOT).await.unwrap();

    let err = ws
        .push_to_origin("task")
        .await
        .expect_err("an unreconcilable push must fail, not force over the remote");
    let msg = err.to_string();
    assert!(
        msg.contains("could not be reconciled"),
        "error must name the reconciliation failure; got: {msg}"
    );

    assert_eq!(
        mirror_sha(&mirror, "task").await,
        before,
        "the mirror may hold the only copy of the earlier cycle's work — it must \
         never be force-overwritten"
    );
}
