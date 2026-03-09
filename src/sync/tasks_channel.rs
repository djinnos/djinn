//! Tasks sync channel — `djinn/tasks` git branch.
//!
//! Layout in the sync worktree (`{project}/.djinn/sync/tasks/`):
//!   `{user_id}.jsonl`  — one JSON object per line, each a serialised `Task`.
//!
//! Multi-project (SYNC-07): each project syncs independently using its own
//! git worktree. Tasks are filtered by `project_id` for export, and the
//! two-phase pull SHA is tracked per-project.
//!
//! Export: write this user's tasks → commit → fetch → rebase → push (retry ×3).
//! Import: fetch, fast-forward, read all *.jsonl, upsert by updated_at (LWW).

use std::path::{Path, PathBuf};

use tokio::sync::broadcast;

use crate::db::connection::Database;
use crate::db::repositories::task::TaskRepository;
use crate::events::DjinnEvent;
use crate::models::task::Task;

/// The sync branch name.
pub const BRANCH: &str = "djinn/tasks";

/// Worktree path relative to the project root.
const WORKTREE_SUBDIR: &str = ".djinn/sync/tasks";

// ── Error ─────────────────────────────────────────────────────────────────────

#[derive(Debug, thiserror::Error)]
pub enum TaskSyncError {
    #[error("git command failed (exit {code}): {stderr}")]
    Git { code: i32, stderr: String },

    #[error("push rejected after {retries} retries")]
    PushRejected { retries: u32 },

    #[error("i/o: {0}")]
    Io(#[from] std::io::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("database: {0}")]
    Database(String),
}

impl From<crate::error::Error> for TaskSyncError {
    fn from(e: crate::error::Error) -> Self {
        Self::Database(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, TaskSyncError>;

// ── Git helpers ───────────────────────────────────────────────────────────────

async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map_err(TaskSyncError::Io)?;

    let code = output.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        return Err(TaskSyncError::Git { code, stderr });
    }
    Ok(stdout)
}

async fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    tokio::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .await
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ── Worktree bootstrap ────────────────────────────────────────────────────────

/// Return the sync worktree path, creating it if needed.
///
/// The worktree lives at `{project}/.djinn/sync/tasks/` on the `djinn/tasks`
/// branch. If the branch doesn't exist locally or remotely, an orphan branch
/// is created with an initial empty commit.
pub async fn ensure_worktree(project: &Path) -> Result<PathBuf> {
    let wt = project.join(WORKTREE_SUBDIR);

    // Already a valid worktree if the .git file exists.
    if wt.join(".git").exists() {
        return Ok(wt);
    }

    // Prune stale worktree metadata.
    let _ = git(project, &["worktree", "prune"]).await;

    // Fetch the remote branch (best-effort — ok if no remote or not yet pushed).
    let _ = git(project, &["fetch", "origin", &format!("{BRANCH}:{BRANCH}")]).await;

    let wt_str = wt.to_str().unwrap_or_default().to_string();

    if git_ok(project, &["rev-parse", "--verify", BRANCH]).await {
        // Branch exists locally (possibly just fetched from remote).
        git(project, &["worktree", "add", &wt_str, BRANCH]).await?;
    } else {
        // Create an orphan branch with an initial empty commit.
        git(project, &["worktree", "add", "--orphan", "-b", BRANCH, &wt_str]).await?;
        // The initial commit makes the branch a proper ref so push works.
        let _ = git(
            &wt,
            &["commit", "--allow-empty", "-m", "djinn: init tasks sync"],
        )
        .await;
    }

    Ok(wt)
}

// ── Export ────────────────────────────────────────────────────────────────────

/// Export tasks for a project to `{user_id}.jsonl`, commit, and push.
///
/// `project_id` scopes the export to a single project (SYNC-07).
///
/// Retries fetch → rebase → push up to 3 times on non-fast-forward conflicts.
/// Since each user writes only their own file, rebase conflicts are extremely
/// rare. The retry loop handles them correctly regardless.
///
/// Returns the number of tasks written.
pub async fn export(
    project: &Path,
    project_id: &str,
    user_id: &str,
    db: &Database,
    events: &broadcast::Sender<DjinnEvent>,
) -> Result<usize> {
    let wt = ensure_worktree(project).await?;

    // Fetch exportable tasks scoped to this project (SYNC-07 + SYNC-12).
    let repo = TaskRepository::new(db.clone(), events.clone());
    let tasks = repo
        .list_for_export(Some(project_id))
        .await
        .map_err(TaskSyncError::from)?;
    let count = tasks.len();

    let filename = format!("{user_id}.jsonl");

    // Migrate legacy `local.jsonl` → `{user_id}.jsonl` if identity resolved
    // to something other than "local" (SYNC-13 single-source identity).
    if user_id != "local" {
        let legacy = wt.join("local.jsonl");
        let target = wt.join(&filename);
        if legacy.exists() && !target.exists() {
            if let Err(e) = tokio::fs::rename(&legacy, &target).await {
                tracing::warn!(error = %e, "failed to migrate local.jsonl to {}", filename);
            } else {
                tracing::info!("migrated local.jsonl → {filename}");
                // Stage the rename so the commit picks it up.
                let _ = git(&wt, &["add", "-A"]).await;
                let _ = git(
                    &wt,
                    &["commit", "-m", &format!("djinn: migrate local.jsonl → {filename}")],
                )
                .await;
            }
        }
    }

    let jsonl: String = tasks
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<Vec<_>, _>>()?
        .join("\n");

    // Retry loop: write → stage → commit → fetch → rebase → push.
    let max_retries = 3u32;
    for attempt in 0..max_retries {
        // Write the JSONL file.
        tokio::fs::write(wt.join(&filename), &jsonl).await?;

        // Stage and commit.
        git(&wt, &["add", &filename]).await?;
        let _ = git(&wt, &["commit", "--allow-empty", "-m", "djinn: sync tasks"]).await;

        // Fetch latest remote state.
        let _ = git(&wt, &["fetch", "origin", BRANCH]).await;

        // Rebase on top of remote if it exists.
        let has_remote = git_ok(&wt, &["rev-parse", "--verify", &format!("origin/{BRANCH}")]).await;
        if has_remote && let Err(e) = git(&wt, &["rebase", &format!("origin/{BRANCH}")]).await {
            tracing::warn!(attempt, error = %e, "rebase failed during export; aborting");
            let _ = git(&wt, &["rebase", "--abort"]).await;
            // Re-stage from scratch on next iteration.
            continue;
        }

        // Try to push.
        match git(&wt, &["push", "origin", BRANCH]).await {
            Ok(_) => {
                tracing::debug!(
                    channel = "tasks",
                    attempt,
                    "sync export pushed successfully"
                );
                return Ok(count);
            }
            Err(e) if attempt + 1 < max_retries => {
                tracing::warn!(channel = "tasks", attempt, error = %e, "push failed; retrying");
                continue;
            }
            Err(_) => break,
        }
    }

    Err(TaskSyncError::PushRejected {
        retries: max_retries,
    })
}

// ── Import ────────────────────────────────────────────────────────────────────

/// Get the remote SHA for the sync branch via `git ls-remote` (~50ms).
///
/// Returns `None` if the remote branch doesn't exist or there's no remote.
async fn ls_remote_sha(project: &Path) -> Option<String> {
    let output = git(project, &["ls-remote", "origin", BRANCH]).await.ok()?;
    // Format: "<sha>\trefs/heads/djinn/tasks\n"
    output.split_whitespace().next().map(|s| s.to_string())
}

/// Per-project settings key for the last-imported SHA (SYNC-07 + SYNC-08).
pub(crate) fn sha_settings_key(project_id: &str) -> String {
    format!("sync.tasks.{project_id}.last_imported_sha")
}

/// Fetch the `djinn/tasks` branch and merge peer task records (LWW by updated_at).
///
/// Uses a two-phase pull (SYNC-08): first checks the remote SHA via `ls-remote`.
/// If the SHA hasn't changed since the last import, skips the expensive fetch+import.
///
/// `project_id` is used for per-project SHA tracking (SYNC-07).
///
/// Reads all `*.jsonl` files in the sync worktree, deduplicates by keeping the
/// newest record for each task ID, then upserts into the local DB.
///
/// Returns the number of tasks upserted.
pub async fn import(
    project: &Path,
    project_id: &str,
    db: &Database,
    events: &broadcast::Sender<DjinnEvent>,
) -> Result<usize> {
    // Two-phase pull (SYNC-08): cheap SHA check before expensive fetch.
    let settings = crate::db::repositories::settings::SettingsRepository::new(
        db.clone(),
        events.clone(),
    );
    let sha_key = sha_settings_key(project_id);
    let remote_sha = ls_remote_sha(project).await;
    if let Some(ref sha) = remote_sha {
        let stored = settings.get(&sha_key).await.ok().flatten();
        if stored.as_ref().map(|s| &s.value) == Some(sha) {
            tracing::trace!(sha, project_id, "two-phase pull: SHA unchanged, skipping import");
            return Ok(0);
        }
    }

    let wt = ensure_worktree(project).await?;

    // Fetch remote.
    let _ = git(&wt, &["fetch", "origin", BRANCH]).await;

    // Fast-forward if remote is ahead.
    if git_ok(&wt, &["rev-parse", "--verify", &format!("origin/{BRANCH}")]).await {
        let _ = git(&wt, &["merge", "--ff-only", &format!("origin/{BRANCH}")]).await;
    }

    // Read all *.jsonl files — collect the best (newest) version of each task.
    let mut peer_tasks: std::collections::HashMap<String, Task> = std::collections::HashMap::new();

    let mut dir = tokio::fs::read_dir(&wt).await?;
    while let Some(entry) = dir.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let content = tokio::fs::read_to_string(&path).await?;
        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<Task>(line) {
                Ok(task) => {
                    let is_newer = peer_tasks
                        .get(&task.id)
                        .map(|existing: &Task| task.updated_at > existing.updated_at)
                        .unwrap_or(true);
                    if is_newer {
                        peer_tasks.insert(task.id.clone(), task);
                    }
                }
                Err(e) => tracing::warn!(error = %e, "skipping malformed line in sync JSONL"),
            }
        }
    }

    if peer_tasks.is_empty() {
        return Ok(0);
    }

    // Upsert into local DB within a single transaction (SYNC-10).
    // Events are collected and emitted only after commit succeeds.
    let mut tx = db.pool().begin().await.map_err(|e| TaskSyncError::Database(e.to_string()))?;
    let mut upserted = 0usize;
    let mut upserted_ids: Vec<String> = Vec::new();

    for task in peer_tasks.into_values() {
        match TaskRepository::upsert_peer_in_tx(&mut tx, &task).await {
            Ok(true) => {
                upserted += 1;
                upserted_ids.push(task.id.clone());
            }
            Ok(false) => {}
            Err(e) => tracing::warn!(task_id = task.id, error = %e, "peer upsert failed"),
        }
    }

    tx.commit().await.map_err(|e| TaskSyncError::Database(e.to_string()))?;

    // Persist SHA only after successful commit (SYNC-07 + SYNC-08 + SYNC-10).
    if let Some(sha) = remote_sha {
        let _ = settings.set(&sha_key, &sha).await;
    }

    // Emit events after successful commit.
    let repo = TaskRepository::new(db.clone(), events.clone());
    for id in &upserted_ids {
        if let Ok(Some(task)) = repo.get(id).await {
            let _ = events.send(DjinnEvent::TaskUpdated { task, from_sync: true });
        }
    }

    Ok(upserted)
}

/// Delete the remote `djinn/tasks` branch (team-wide disable).
pub async fn delete_remote_branch(project: &Path) -> Result<()> {
    git(project, &["push", "origin", "--delete", BRANCH]).await?;
    Ok(())
}

// ── Integration tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::epic::EpicRepository;
    use crate::db::repositories::task::TaskRepository;

    /// Create a temp dir with a git repo + bare "origin" remote.
    /// Returns (project_path, _temp_dir_guard).
    async fn setup_git_repo() -> (PathBuf, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("create tempdir");
        let bare = tmp.path().join("origin.git");
        let repo = tmp.path().join("project");

        // Create bare repo as "origin".
        tokio::fs::create_dir_all(&bare).await.unwrap();
        git(&bare, &["init", "--bare"]).await.unwrap();

        // Create working repo and add origin.
        tokio::fs::create_dir_all(&repo).await.unwrap();
        git(&repo, &["init"]).await.unwrap();
        git(&repo, &["remote", "add", "origin", bare.to_str().unwrap()])
            .await
            .unwrap();
        // Initial commit on main so we have a valid repo.
        git(&repo, &["commit", "--allow-empty", "-m", "init"])
            .await
            .unwrap();

        (repo, tmp)
    }

    fn make_test_task(id: &str, project_id: &str, title: &str) -> Task {
        Task {
            id: id.to_string(),
            project_id: project_id.to_string(),
            short_id: format!("s-{}", &id[..4]),
            epic_id: None,
            title: title.to_string(),
            description: String::new(),
            design: String::new(),
            issue_type: "task".to_string(),
            status: "open".to_string(),
            priority: 0,
            owner: String::new(),
            labels: "[]".to_string(),
            acceptance_criteria: "[]".to_string(),
            reopen_count: 0,
            continuation_count: 0,
            created_at: "2026-01-01T00:00:00.000Z".to_string(),
            updated_at: "2026-03-08T00:00:00.000Z".to_string(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            memory_refs: "[]".to_string(),
            unresolved_blocker_count: 0,
        }
    }

    // ── Worktree tests ───────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ensure_worktree_creates_orphan_branch() {
        let (repo, _tmp) = setup_git_repo().await;
        let wt = ensure_worktree(&repo).await.unwrap();
        assert!(wt.join(".git").exists(), "worktree .git file should exist");

        // The orphan branch should exist.
        assert!(git_ok(&repo, &["rev-parse", "--verify", BRANCH]).await);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ensure_worktree_idempotent() {
        let (repo, _tmp) = setup_git_repo().await;
        let wt1 = ensure_worktree(&repo).await.unwrap();
        let wt2 = ensure_worktree(&repo).await.unwrap();
        assert_eq!(wt1, wt2);
    }

    // ── Export tests ─────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn export_writes_jsonl_and_pushes() {
        let (repo, _tmp) = setup_git_repo().await;

        let db = crate::test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(64);

        // Create a project and task via DB.
        let epic_repo = EpicRepository::new(db.clone(), tx.clone());
        let epic = epic_repo.create("E1", "", "", "", "").await.unwrap();

        let task_repo = TaskRepository::new(db.clone(), tx.clone());
        let task = task_repo
            .create(&epic.id, "My Task", "", "", "task", 0, "")
            .await
            .unwrap();

        let count = export(&repo, &task.project_id, "user1", &db, &tx)
            .await
            .unwrap();
        assert_eq!(count, 1);

        // Verify the JSONL file exists in the worktree.
        let wt = repo.join(WORKTREE_SUBDIR);
        let jsonl = tokio::fs::read_to_string(wt.join("user1.jsonl"))
            .await
            .unwrap();
        assert!(jsonl.contains(&task.id));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn export_scopes_to_project_id() {
        let (repo, _tmp) = setup_git_repo().await;

        let db = crate::test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(64);

        // Create two projects with tasks.
        let project_repo =
            crate::db::repositories::project::ProjectRepository::new(db.clone(), tx.clone());
        let p1 = project_repo.create("proj-a", "/tmp/a").await.unwrap();
        let p2 = project_repo.create("proj-b", "/tmp/b").await.unwrap();

        let epic_repo = EpicRepository::new(db.clone(), tx.clone());
        let epic = epic_repo.create("E1", "", "", "", "").await.unwrap();

        let task_repo = TaskRepository::new(db.clone(), tx.clone());
        task_repo
            .create_in_project(&p1.id, Some(&epic.id), "Task A", "", "", "task", 0, "")
            .await
            .unwrap();
        task_repo
            .create_in_project(&p2.id, Some(&epic.id), "Task B", "", "", "task", 0, "")
            .await
            .unwrap();

        // Export only p1's tasks.
        let count = export(&repo, &p1.id, "user1", &db, &tx).await.unwrap();
        assert_eq!(count, 1);

        let wt = repo.join(WORKTREE_SUBDIR);
        let jsonl = tokio::fs::read_to_string(wt.join("user1.jsonl"))
            .await
            .unwrap();
        assert!(jsonl.contains("Task A"));
        assert!(!jsonl.contains("Task B"));
    }

    // ── Import tests ─────────────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn import_reads_jsonl_and_upserts() {
        let (repo, _tmp) = setup_git_repo().await;
        let db = crate::test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(64);

        // Create an epic so FK checks pass.
        let epic_repo = EpicRepository::new(db.clone(), tx.clone());
        let epic = epic_repo.create("E1", "", "", "", "").await.unwrap();

        // Manually write a JSONL file into the sync worktree (simulating a peer).
        let wt = ensure_worktree(&repo).await.unwrap();
        let task = make_test_task("aaaa-bbbb-cccc-dddd", &epic.project_id, "Peer Task");
        let jsonl = serde_json::to_string(&task).unwrap();
        tokio::fs::write(wt.join("peer1.jsonl"), &jsonl).await.unwrap();
        git(&wt, &["add", "peer1.jsonl"]).await.unwrap();
        git(&wt, &["commit", "-m", "peer sync"]).await.unwrap();

        // Import should upsert the peer task.
        let count = import(&repo, &epic.project_id, &db, &tx).await.unwrap();
        assert_eq!(count, 1);

        // Verify it's in the DB.
        let task_repo = TaskRepository::new(db.clone(), tx.clone());
        let fetched = task_repo.get("aaaa-bbbb-cccc-dddd").await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().title, "Peer Task");
    }

    // ── Full round-trip test ─────────────────────────────────────────────────

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn export_then_import_round_trip() {
        let (repo, _tmp) = setup_git_repo().await;
        let db = crate::test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(64);

        // Create task.
        let epic_repo = EpicRepository::new(db.clone(), tx.clone());
        let epic = epic_repo.create("E1", "", "", "", "").await.unwrap();
        let task_repo = TaskRepository::new(db.clone(), tx.clone());
        let task = task_repo
            .create(&epic.id, "Round Trip", "", "", "task", 0, "")
            .await
            .unwrap();

        // Export.
        let exported = export(&repo, &task.project_id, "user1", &db, &tx)
            .await
            .unwrap();
        assert_eq!(exported, 1);

        // Drain events.
        while rx.try_recv().is_ok() {}

        // Import — should find the same task (LWW: same updated_at = no change).
        let imported = import(&repo, &task.project_id, &db, &tx).await.unwrap();
        // Import reads our own export back, but upsert_peer uses LWW so the
        // task is only updated if the peer's updated_at is newer. Same timestamp
        // means no update.
        assert_eq!(imported, 0, "same timestamp should not re-upsert");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn import_emits_from_sync_true_events() {
        let (repo, _tmp) = setup_git_repo().await;
        let db = crate::test_helpers::create_test_db();
        let (tx, mut rx) = broadcast::channel(64);

        let epic_repo = EpicRepository::new(db.clone(), tx.clone());
        let epic = epic_repo.create("E1", "", "", "", "").await.unwrap();

        // Write a peer task directly into worktree.
        let wt = ensure_worktree(&repo).await.unwrap();
        let task = make_test_task("1111-2222-3333-4444", &epic.project_id, "Sync Event Task");
        let jsonl = serde_json::to_string(&task).unwrap();
        tokio::fs::write(wt.join("peer2.jsonl"), &jsonl).await.unwrap();
        git(&wt, &["add", "peer2.jsonl"]).await.unwrap();
        git(&wt, &["commit", "-m", "peer"]).await.unwrap();

        // Drain setup events.
        while rx.try_recv().is_ok() {}

        let count = import(&repo, &epic.project_id, &db, &tx).await.unwrap();
        assert_eq!(count, 1);

        // Check that the emitted event has from_sync=true.
        let evt = rx.recv().await.unwrap();
        match evt {
            DjinnEvent::TaskUpdated { from_sync, .. } => {
                assert!(from_sync, "import should emit from_sync: true");
            }
            other => panic!("expected TaskUpdated, got: {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn import_skips_malformed_lines() {
        let (repo, _tmp) = setup_git_repo().await;
        let db = crate::test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(64);

        let epic_repo = EpicRepository::new(db.clone(), tx.clone());
        let epic = epic_repo.create("E1", "", "", "", "").await.unwrap();

        let wt = ensure_worktree(&repo).await.unwrap();
        let good_task = make_test_task("5555-6666-7777-8888", &epic.project_id, "Good");
        let content = format!(
            "{}\n{{\"bad json\n{}",
            serde_json::to_string(&good_task).unwrap(),
            "not json at all"
        );
        tokio::fs::write(wt.join("mixed.jsonl"), &content)
            .await
            .unwrap();
        git(&wt, &["add", "mixed.jsonl"]).await.unwrap();
        git(&wt, &["commit", "-m", "mixed"]).await.unwrap();

        let count = import(&repo, &epic.project_id, &db, &tx).await.unwrap();
        assert_eq!(count, 1, "should import only the valid task");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn import_lww_keeps_newer_version() {
        let (repo, _tmp) = setup_git_repo().await;
        let db = crate::test_helpers::create_test_db();
        let (tx, _rx) = broadcast::channel(64);

        let epic_repo = EpicRepository::new(db.clone(), tx.clone());
        let epic = epic_repo.create("E1", "", "", "", "").await.unwrap();

        let wt = ensure_worktree(&repo).await.unwrap();

        // Two versions of the same task from different peers.
        let mut old = make_test_task("same-id-00-00", &epic.project_id, "Old Title");
        old.updated_at = "2026-01-01T00:00:00.000Z".to_string();

        let mut new = make_test_task("same-id-00-00", &epic.project_id, "New Title");
        new.updated_at = "2026-06-01T00:00:00.000Z".to_string();
        new.short_id = "s-same".to_string(); // same short_id

        // Peer A has old version, Peer B has new version.
        tokio::fs::write(
            wt.join("peerA.jsonl"),
            serde_json::to_string(&old).unwrap(),
        )
        .await
        .unwrap();
        tokio::fs::write(
            wt.join("peerB.jsonl"),
            serde_json::to_string(&new).unwrap(),
        )
        .await
        .unwrap();
        git(&wt, &["add", "."]).await.unwrap();
        git(&wt, &["commit", "-m", "peers"]).await.unwrap();

        let count = import(&repo, &epic.project_id, &db, &tx).await.unwrap();
        assert_eq!(count, 1);

        let task_repo = TaskRepository::new(db.clone(), tx.clone());
        let fetched = task_repo.get("same-id-00-00").await.unwrap().unwrap();
        assert_eq!(fetched.title, "New Title", "LWW should keep newer version");
    }
}
