//! Tasks sync channel — `djinn/tasks` git branch.
//!
//! Layout in the sync worktree (`{project}/.djinn/sync/tasks/`):
//!   `{user_id}.jsonl`  — one JSON object per line, each a serialised `Task`.
//!
//! Export: write this user's tasks → commit → fetch → rebase → push (retry ×3).
//! Import: fetch, fast-forward, read all *.jsonl, upsert by updated_at (LWW).

use std::path::{Path, PathBuf};

use tokio::sync::broadcast;

use crate::db::connection::Database;
use crate::db::repositories::task::{ListQuery, TaskRepository};
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
        git(project, &["worktree", "add", "--orphan", &wt_str, BRANCH]).await?;
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

/// Export all tasks from the local DB to `{user_id}.jsonl`, commit, and push.
///
/// Retries fetch → rebase → push up to 3 times on non-fast-forward conflicts.
/// Since each user writes only their own file, rebase conflicts are extremely
/// rare. The retry loop handles them correctly regardless.
///
/// Returns the number of tasks written.
pub async fn export(
    project: &Path,
    user_id: &str,
    db: &Database,
    events: &broadcast::Sender<DjinnEvent>,
) -> Result<usize> {
    let wt = ensure_worktree(project).await?;

    // Fetch all tasks from the local DB.
    let repo = TaskRepository::new(db.clone(), events.clone());
    let result = repo
        .list_filtered(ListQuery {
            limit: 100_000,
            ..Default::default()
        })
        .await
        .map_err(TaskSyncError::from)?;
    let tasks = result.tasks;
    let count = tasks.len();

    let filename = format!("{user_id}.jsonl");
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

/// Fetch the `djinn/tasks` branch and merge peer task records (LWW by updated_at).
///
/// Reads all `*.jsonl` files in the sync worktree, deduplicates by keeping the
/// newest record for each task ID, then upserts into the local DB.
///
/// Returns the number of tasks upserted.
pub async fn import(
    project: &Path,
    db: &Database,
    events: &broadcast::Sender<DjinnEvent>,
) -> Result<usize> {
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

    // Upsert into local DB (LWW — skips tasks whose local copy is newer).
    let repo = TaskRepository::new(db.clone(), events.clone());
    let mut upserted = 0usize;

    for task in peer_tasks.into_values() {
        match repo.upsert_peer(&task).await {
            Ok(true) => upserted += 1,
            Ok(false) => {}
            Err(e) => tracing::warn!(task_id = task.id, error = %e, "peer upsert failed"),
        }
    }

    Ok(upserted)
}

/// Delete the remote `djinn/tasks` branch (team-wide disable).
pub async fn delete_remote_branch(project: &Path) -> Result<()> {
    git(project, &["push", "origin", "--delete", BRANCH]).await?;
    Ok(())
}
