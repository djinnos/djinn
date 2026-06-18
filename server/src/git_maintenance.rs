//! Periodic git housekeeping — reclaims disk from the on-disk git stores.
//!
//! Two on-disk git stores grow per project: the bare **mirror**
//! (`$DJINN_HOME/mirrors/{id}.git`) and the per-project working **clone**
//! (`$DJINN_HOME/projects/{owner}/{repo}`). Both already fetch with `--prune`,
//! so refs to branches deleted upstream are dropped — but their *objects* are
//! never reclaimed, because nothing ever ran `git gc`. With djinn's
//! branch-per-task churn (create → PR → merge → delete) that unreferenced
//! pile is the dominant source of disk growth (the projects volume ballooned
//! to tens of GB of pure garbage).
//!
//! This leader-only loop runs `git gc` over every project's mirror + clone on
//! a slow cadence (default daily, `DJINN_GIT_MAINTENANCE_INTERVAL_SECS`). gc
//! is non-destructive to correctness — it only drops objects unreachable from
//! any ref, and a `2.weeks.ago` prune expiry leaves a safety window for any
//! in-flight `--shared` ephemeral clone borrowing objects via alternates. The
//! mirror gc is taken under the same per-project `MirrorManager` lock as the
//! 60s fetch, so it never races a concurrent fetch or clone.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::time::MissedTickBehavior;

use djinn_db::ProjectRepository;
use djinn_workspace::{GcGuardError, MirrorError};

use crate::server::AppState;

const DEFAULT_INTERVAL_SECS: u64 = 24 * 60 * 60;
const INTERVAL_ENV: &str = "DJINN_GIT_MAINTENANCE_INTERVAL_SECS";

/// Spawn the periodic git-maintenance task. Leader-only (started from
/// `become_leader`), runs until `state.cancel()` fires.
pub fn spawn(state: AppState) {
    let interval = parse_interval(std::env::var(INTERVAL_ENV).ok().as_deref());
    let cancel = state.cancel().clone();

    tokio::spawn(async move {
        tracing::info!(?interval, "git_maintenance loop starting");
        let mut ticker = tokio::time::interval(interval);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        // The first tick fires immediately; consume it so we don't gc right at
        // boot during the leadership transition. gc waits one full interval.
        ticker.tick().await;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    tracing::debug!("git_maintenance loop cancelled");
                    break;
                }
                _ = ticker.tick() => run_tick(&state).await,
            }
        }
    });
}

async fn run_tick(state: &AppState) {
    let repo = ProjectRepository::new(state.db().clone(), state.event_bus());
    let projects = match repo.list().await {
        Ok(p) => p,
        Err(err) => {
            tracing::warn!(error = %err, "git_maintenance: project list failed; skipping tick");
            return;
        }
    };

    let mirror = state.mirror();
    let mut gc_mirrors = 0u64;
    let mut gc_clones = 0u64;

    for project in &projects {
        // 1. Bare mirror — taken under the per-project MirrorManager lock so
        //    it never races the 60s fetch. `Missing` just means no mirror has
        //    been cloned yet (code-less / freshly added project).
        match mirror.gc(&project.id).await {
            Ok(()) => gc_mirrors += 1,
            Err(MirrorError::Missing(_)) => {}
            Err(err) => tracing::warn!(
                project_id = %project.id,
                error = %err,
                "git_maintenance: mirror gc failed"
            ),
        }

        // 2. Working clone (`$DJINN_HOME/projects/{owner}/{repo}`). Skip
        //    GitHub-less projects and any whose clone isn't on disk yet.
        let projects_root = djinn_core::paths::projects_root();
        let Some(clone_gc) =
            clone_gc_request(&projects_root, &project.github_owner, &project.github_repo)
        else {
            continue;
        };
        // Route through the root-scoped workspace API so periodic maintenance
        // cannot gc an arbitrary path derived from project metadata.
        match djinn_workspace::gc_project_clone_under(
            &clone_gc.projects_root,
            &clone_gc.owner,
            &clone_gc.repo,
        )
        .await
        {
            Ok(()) => gc_clones += 1,
            Err(err) => warn_clone_gc_error(&project.id, &err),
        }
    }

    tracing::info!(
        projects = projects.len(),
        gc_mirrors,
        gc_clones,
        "git_maintenance: tick complete"
    );
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProjectCloneGcRequest {
    projects_root: PathBuf,
    owner: String,
    repo: String,
}

fn clone_gc_request(
    projects_root: &Path,
    owner: &str,
    repo: &str,
) -> Option<ProjectCloneGcRequest> {
    if owner.is_empty() || repo.is_empty() {
        return None;
    }

    // Deliberately derive the presence check from the same root and path
    // segments handed to the guarded gc API below. Do not canonicalize or
    // sanitize here: traversal/symlink escapes must still reach the guard so
    // they are refused as unsafe paths rather than silently bypassing root
    // validation in the maintenance loop.
    let clone = projects_root.join(owner).join(repo);
    if !clone.join(".git").exists() {
        return None;
    }

    Some(ProjectCloneGcRequest {
        projects_root: projects_root.to_path_buf(),
        owner: owner.to_string(),
        repo: repo.to_string(),
    })
}

fn warn_clone_gc_error(project_id: &str, err: &GcGuardError) {
    match err {
        GcGuardError::Git(_) => tracing::warn!(
            project_id = %project_id,
            error = %err,
            "git_maintenance: clone gc command failed"
        ),
        GcGuardError::InvalidSegment(_)
        | GcGuardError::RootIo { .. }
        | GcGuardError::RepoIo { .. }
        | GcGuardError::OutsideRoot { .. } => tracing::warn!(
            project_id = %project_id,
            error = %err,
            "git_maintenance: refused unsafe clone gc path"
        ),
    }
}

fn parse_interval(raw: Option<&str>) -> Duration {
    let secs = raw
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_INTERVAL_SECS);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn interval_uses_positive_override_else_default() {
        assert_eq!(parse_interval(Some("3600")), Duration::from_secs(3600));
        assert_eq!(
            parse_interval(None),
            Duration::from_secs(DEFAULT_INTERVAL_SECS)
        );
        assert_eq!(
            parse_interval(Some("0")),
            Duration::from_secs(DEFAULT_INTERVAL_SECS)
        );
        assert_eq!(
            parse_interval(Some("not-a-number")),
            Duration::from_secs(DEFAULT_INTERVAL_SECS)
        );
    }

    #[test]
    fn clone_gc_request_skips_githubless_and_absent_clones() {
        let root = TempDir::new().unwrap();

        assert_eq!(clone_gc_request(root.path(), "", "repo"), None);
        assert_eq!(clone_gc_request(root.path(), "owner", ""), None);
        assert_eq!(clone_gc_request(root.path(), "owner", "repo"), None);
    }

    #[test]
    fn clone_gc_request_uses_projects_root_owner_and_repo_for_present_clone() {
        let root = TempDir::new().unwrap();
        std::fs::create_dir_all(root.path().join("owner").join("repo").join(".git")).unwrap();

        assert_eq!(
            clone_gc_request(root.path(), "owner", "repo"),
            Some(ProjectCloneGcRequest {
                projects_root: root.path().to_path_buf(),
                owner: "owner".to_string(),
                repo: "repo".to_string(),
            })
        );
    }

    #[tokio::test]
    async fn clone_gc_request_does_not_hide_traversal_from_guard() {
        let root = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        std::fs::create_dir_all(outside.path().join("repo").join(".git")).unwrap();

        let traversal_owner = format!(
            "../{}",
            outside.path().file_name().unwrap().to_string_lossy()
        );
        let request = clone_gc_request(root.path(), &traversal_owner, "repo")
            .expect("existing traversal-shaped clone should reach guarded gc");

        let err = djinn_workspace::gc_project_clone_under(
            &request.projects_root,
            &request.owner,
            &request.repo,
        )
        .await
        .unwrap_err();
        assert!(matches!(err, GcGuardError::InvalidSegment(_)));
    }
}
