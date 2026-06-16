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

use std::time::Duration;

use tokio::time::MissedTickBehavior;

use djinn_db::ProjectRepository;
use djinn_workspace::MirrorError;

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
        if project.github_owner.is_empty() || project.github_repo.is_empty() {
            continue;
        }
        let clone = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        if !clone.join(".git").exists() {
            continue;
        }
        match djinn_workspace::gc_project_clone_under(
            &djinn_core::paths::projects_root(),
            &project.github_owner,
            &project.github_repo,
        )
        .await
        {
            Ok(()) => gc_clones += 1,
            Err(err) => tracing::warn!(
                project_id = %project.id,
                error = %err,
                "git_maintenance: clone gc failed"
            ),
        }
    }

    tracing::info!(
        projects = projects.len(),
        gc_mirrors,
        gc_clones,
        "git_maintenance: tick complete"
    );
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
}
