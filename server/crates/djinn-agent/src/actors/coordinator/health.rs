use super::*;

use std::collections::HashSet;
use std::path::Path;

/// How long a `task_run` may stay in `running` without an `ended_at` before
/// the periodic sweep flips it to `interrupted`. Must comfortably exceed the
/// K8s Job `activeDeadlineSeconds` (10800s) + `terminationGracePeriodSeconds`
/// (60s) + `ttlSecondsAfterFinished` (300s) so we never reap a still-live run
/// whose pod is mid-termination. Bumped to 4h alongside the deadline raise
/// (3600→10800): a 3h-budget run that legitimately uses most of its window
/// must not be reaped out from under itself.
const STALE_TASK_RUN_THRESHOLD_SECS: i64 = 4 * 60 * 60;

/// Startup-only threshold. Any `task_run` whose `started_at` is older than
/// this at process boot is from a previous host instance — the worker Pod
/// has lost its RPC channel and can't flush a terminal status. Tighter than
/// the periodic threshold because there's no live-run ambiguity at cold
/// start (we know our previous self is gone).
const STARTUP_TASK_RUN_THRESHOLD_SECS: i64 = 10;

const CARGO_TARGET_RUNS_ROOT: &str = djinn_supervisor::CARGO_TARGET_RUNS_ROOT;

// ─── Stale-resource sweep ────────────────────────────────────────────────────

pub(super) async fn sweep_stale_resources(
    db: &djinn_db::Database,
    app_state: &crate::context::AgentContext,
) {
    reap_stale_task_runs(db).await;
    reap_orphaned_taskrun_jobs(db, app_state, "periodic").await;
    sweep_orphaned_cargo_target_run_dirs(db, app_state.cargo_target_runs_root.as_deref()).await;

    let project_repo = ProjectRepository::new(db.clone(), app_state.event_bus.clone());
    let task_repo = TaskRepository::new(db.clone(), app_state.event_bus.clone());
    let session_repo = djinn_db::SessionRepository::new(db.clone(), app_state.event_bus.clone());

    let projects = match project_repo.list().await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error=%e, "CoordinatorActor: stale sweep failed to list projects");
            return;
        }
    };

    // Task #8: worktree GC removed — the supervisor-driven dispatch path no
    // longer creates `.djinn/worktrees/<short_id>` directories, and the
    // session record's `worktree_path` column will be dropped in task #13.
    // We still walk per-project local branches to prune `task/<short_id>`
    // refs for closed, Djinn-authored tasks.
    let _ = session_repo;
    for project in projects {
        let project_dir =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);

        if let Ok(git) = app_state.git_actor(&project_dir).await
            && let Ok(out) = git
                .run_command(vec!["branch".into(), "--format=%(refname:short)".into()])
                .await
        {
            for line in out.stdout.lines() {
                let Some(short_id) = line.strip_prefix("task/") else {
                    continue;
                };
                let should_delete = match task_repo.get_by_short_id(short_id).await {
                    // Only delete branches for closed tasks that Djinn created a PR for.
                    // Branches for tasks without a pr_url were not managed by Djinn
                    // and must not be touched.
                    Ok(Some(task)) => task.status == "closed" && task.pr_url.is_some(),
                    // Unknown task — do NOT delete; the branch may belong to
                    // another project or have been created outside Djinn.
                    Ok(None) => false,
                    Err(_) => false,
                };
                if should_delete {
                    tracing::info!(project_id=%project.id, branch=%line, "CoordinatorActor: deleting stale task branch");
                    let _ = git.delete_branch(line).await;
                }
            }
        }

        // Mirror-side task-ref prune. `fetch_mirror` excludes
        // `refs/heads/task/*` from its `--prune` (those are djinn-owned
        // durability refs the worker pods push — see the negative refspec in
        // `MirrorManager::fetch_mirror`), so without this sweep the bare
        // mirror's ref set grows one branch per task forever. Same deletion
        // policy as the project-clone prune above: closed + Djinn-opened PR.
        if let Some(mirror) = app_state.mirror.as_ref() {
            let mirror_dir = mirror.mirror_path(&project.id);
            if let Ok(git) = app_state.git_actor(&mirror_dir).await
                && let Ok(out) = git
                    .run_command(vec!["branch".into(), "--format=%(refname:short)".into()])
                    .await
            {
                for line in out.stdout.lines() {
                    let Some(short_id) = line.strip_prefix("task/") else {
                        continue;
                    };
                    let should_delete = matches!(
                        task_repo.get_by_short_id(short_id).await,
                        Ok(Some(task)) if task.status == "closed" && task.pr_url.is_some()
                    );
                    if should_delete {
                        tracing::info!(
                            project_id=%project.id,
                            branch=%line,
                            "CoordinatorActor: deleting closed task branch from mirror"
                        );
                        let _ = mirror.delete_branch(&project.id, line).await;
                    }
                }
            }
        }
    }
}

// ─── Cargo target run-dir GC ────────────────────────────────────────────────

#[derive(Default)]
pub(super) struct CargoTargetRunDirSweepStats {
    pub(super) scanned: usize,
    pub(super) deleted: usize,
    pub(super) retained: usize,
    pub(super) errors: usize,
}

async fn sweep_orphaned_cargo_target_run_dirs(db: &djinn_db::Database, root: Option<&Path>) {
    sweep_orphaned_cargo_target_run_dirs_under(
        db,
        root.unwrap_or_else(|| Path::new(CARGO_TARGET_RUNS_ROOT)),
    )
    .await;
}

pub(super) async fn sweep_orphaned_cargo_target_run_dirs_under(
    db: &djinn_db::Database,
    root: &Path,
) -> CargoTargetRunDirSweepStats {
    let protected = match protected_cargo_target_run_ids(db).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(
                error = %e,
                root = %root.display(),
                "CoordinatorActor: cargo target run-dir sweep failed to load protected task_run ids"
            );
            return CargoTargetRunDirSweepStats {
                errors: 1,
                ..Default::default()
            };
        }
    };

    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                root = %root.display(),
                "CoordinatorActor: cargo target run-dir root does not exist; skipping sweep"
            );
            return CargoTargetRunDirSweepStats::default();
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                root = %root.display(),
                "CoordinatorActor: cargo target run-dir sweep failed to enumerate root"
            );
            return CargoTargetRunDirSweepStats {
                errors: 1,
                ..Default::default()
            };
        }
    };

    let mut stats = CargoTargetRunDirSweepStats::default();
    loop {
        let entry = match entries.next_entry().await {
            Ok(Some(entry)) => entry,
            Ok(None) => break,
            Err(e) => {
                stats.errors += 1;
                tracing::warn!(
                    error = %e,
                    root = %root.display(),
                    "CoordinatorActor: cargo target run-dir sweep failed to read directory entry; continuing"
                );
                continue;
            }
        };

        let path = entry.path();
        stats.scanned += 1;

        let Some(task_run_id) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            stats.retained += 1;
            tracing::debug!(
                path = %path.display(),
                "CoordinatorActor: cargo target run-dir sweep ignored non-UTF8 entry name"
            );
            continue;
        };

        if uuid::Uuid::parse_str(&task_run_id).is_err() {
            stats.retained += 1;
            tracing::debug!(
                task_run_id = %task_run_id,
                path = %path.display(),
                "CoordinatorActor: cargo target run-dir sweep ignored malformed task_run_id entry"
            );
            continue;
        }

        let file_type = match entry.file_type().await {
            Ok(file_type) => file_type,
            Err(e) => {
                stats.errors += 1;
                tracing::warn!(
                    error = %e,
                    task_run_id = %task_run_id,
                    path = %path.display(),
                    "CoordinatorActor: cargo target run-dir sweep failed to inspect entry; continuing"
                );
                continue;
            }
        };

        if !file_type.is_dir() {
            stats.retained += 1;
            tracing::debug!(
                task_run_id = %task_run_id,
                path = %path.display(),
                "CoordinatorActor: cargo target run-dir sweep ignored non-directory entry"
            );
            continue;
        }

        if protected.contains(&task_run_id) {
            stats.retained += 1;
            tracing::debug!(
                task_run_id = %task_run_id,
                path = %path.display(),
                "CoordinatorActor: cargo target run-dir sweep retained live task-run directory"
            );
            continue;
        }

        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => {
                stats.deleted += 1;
                tracing::info!(
                    task_run_id = %task_run_id,
                    path = %path.display(),
                    cleanup_outcome = "removed",
                    deleted_count = 1_u64,
                    error_count = 0_u64,
                    "CoordinatorActor: deleted orphaned cargo target run-dir"
                );
            }
            Err(e) => {
                stats.errors += 1;
                tracing::warn!(
                    error = %e,
                    task_run_id = %task_run_id,
                    path = %path.display(),
                    cleanup_outcome = "failed",
                    deleted_count = 0_u64,
                    error_count = 1_u64,
                    "CoordinatorActor: failed to delete orphaned cargo target run-dir; continuing"
                );
            }
        }
    }

    tracing::info!(
        root = %root.display(),
        scanned = stats.scanned,
        deleted = stats.deleted,
        retained = stats.retained,
        errors = stats.errors,
        cleanup_outcome = if stats.errors == 0 { "completed" } else { "completed_with_errors" },
        "CoordinatorActor: cargo target run-dir sweep completed"
    );

    stats
}

async fn protected_cargo_target_run_ids(
    db: &djinn_db::Database,
) -> djinn_db::Result<HashSet<String>> {
    db.ensure_initialized().await?;

    let task_run_ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM task_runs WHERE status = 'running' AND ended_at IS NULL",
    )
    .fetch_all(db.pool())
    .await?;

    let session_task_run_ids: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT task_run_id FROM sessions
         WHERE status = 'running' AND ended_at IS NULL AND task_run_id IS NOT NULL",
    )
    .fetch_all(db.pool())
    .await?;

    Ok(task_run_ids
        .into_iter()
        .chain(session_task_run_ids)
        .collect())
}

fn session_status_classification(sessions: &[djinn_core::models::SessionRecord]) -> &'static str {
    if sessions
        .iter()
        .any(|session| session.status == "interrupted")
    {
        "session_interrupted"
    } else if sessions.iter().any(|session| session.status == "completed") {
        "session_completed"
    } else if sessions.iter().any(|session| session.status == "failed") {
        "session_failed"
    } else if sessions.is_empty() {
        "task_run_running_without_session"
    } else {
        "task_run_running_without_live_session"
    }
}

// ─── K8s task-run Job backstop ───────────────────────────────────────────────

/// Reconcile runtime task-run Jobs against DB truth and foreground-delete Jobs
/// for task-runs that are absent or already finalized.
///
/// This is intentionally a runtime-bridge policy, not a Kubernetes policy: the
/// coordinator sees only [`djinn_control_plane::bridge::TaskrunJobRef`] values
/// and calls [`djinn_control_plane::bridge::RuntimeOps::teardown_taskrun_job`].
/// Inline teardown, stall reaping, and zombie recovery own currently-running
/// rows; this backstop only cleans Jobs with no live DB owner.
pub(super) async fn reap_orphaned_taskrun_jobs(
    db: &djinn_db::Database,
    app_state: &crate::context::AgentContext,
    reason: &'static str,
) {
    let Some(runtime_ops) = app_state.runtime_ops.as_ref() else {
        return;
    };

    let jobs = match runtime_ops.list_taskrun_jobs().await {
        Ok(jobs) => jobs,
        Err(e) => {
            tracing::warn!(error = %e, reason, "CoordinatorActor: failed to list task-run Jobs for backstop reap");
            return;
        }
    };

    let task_run_repo = djinn_db::repositories::task_run::TaskRunRepository::new(db.clone());

    for job in jobs {
        let task_run_id = job.task_run_id.trim();
        if task_run_id.is_empty() {
            tracing::warn!(
                job_name = %job.job_name,
                task_run_id = %job.task_run_id,
                db_classification = "malformed_inventory",
                reason,
                "CoordinatorActor: task-run Job backstop inventory entry is malformed; skipping"
            );
            continue;
        }

        let task_run = match task_run_repo.get(task_run_id).await {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!(
                    job_name = %job.job_name,
                    task_run_id = %task_run_id,
                    error = %e,
                    reason,
                    "CoordinatorActor: failed to load task_run for task-run Job backstop reap"
                );
                continue;
            }
        };

        let sessions = match list_sessions_for_task_run(db, task_run_id).await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(
                    job_name = %job.job_name,
                    task_run_id = %task_run_id,
                    error = %e,
                    reason,
                    "CoordinatorActor: failed to load sessions for task-run Job backstop reap"
                );
                continue;
            }
        };

        let classification = classify_taskrun_job_owner(task_run.as_ref(), &sessions);
        if classification.keep_job {
            tracing::debug!(
                job_name = %job.job_name,
                task_run_id = %task_run_id,
                db_classification = classification.db_classification,
                reason,
                "CoordinatorActor: task-run Job backstop preserved live task-run Job"
            );
            continue;
        }

        if let Err(e) = runtime_ops.teardown_taskrun_job(task_run_id).await {
            tracing::warn!(
                job_name = %job.job_name,
                task_run_id = %task_run_id,
                db_classification = classification.db_classification,
                error = %e,
                reason,
                "CoordinatorActor: task-run Job backstop teardown failed; continuing sweep"
            );
            continue;
        }

        tracing::info!(
            job_name = %job.job_name,
            task_run_id = %task_run_id,
            db_classification = classification.db_classification,
            reason,
            "CoordinatorActor: backstop reaped orphaned task-run Job"
        );
    }
}

async fn list_sessions_for_task_run(
    db: &djinn_db::Database,
    task_run_id: &str,
) -> djinn_db::Result<Vec<djinn_core::models::SessionRecord>> {
    db.ensure_initialized().await?;
    Ok(sqlx::query_as::<_, djinn_core::models::SessionRecord>(
        r#"SELECT id, project_id, task_id, model_id, agent_type, started_at, ended_at,
            status, tokens_in, tokens_out,
            cache_read_tokens, cache_write_tokens, task_run_id, title,
            parked_reason
         FROM sessions WHERE task_run_id = $1 ORDER BY started_at DESC"#,
    )
    .bind(task_run_id)
    .fetch_all(db.pool())
    .await?)
}

/// Startup/rollout pass for the task-run Job backstop. Server boot first marks
/// previously-running sessions interrupted via `interrupt_stale_sessions_on_startup`;
/// the coordinator then runs this immediate reconcile before waiting for the
/// normal stale-resource interval. The helper is idempotent and safe if startup
/// ordering changes: if the session row is still running, the Job is preserved;
/// if the row was interrupted/finalized or is absent, the Job is deleted.
pub(super) async fn reap_orphaned_taskrun_jobs_for_startup(
    db: &djinn_db::Database,
    app_state: &crate::context::AgentContext,
) {
    tracing::info!("CoordinatorActor: running startup task-run Job backstop reconcile");
    reap_orphaned_taskrun_jobs(db, app_state, "startup").await;
}

struct TaskrunJobClassification {
    keep_job: bool,
    db_classification: &'static str,
}

fn classify_taskrun_job_owner(
    task_run: Option<&djinn_core::models::TaskRunRecord>,
    sessions: &[djinn_core::models::SessionRecord],
) -> TaskrunJobClassification {
    let has_live_session = sessions
        .iter()
        .any(|session| session.status == "running" && session.ended_at.is_none());

    if let Some(task_run) = task_run {
        if task_run.status == "running" && task_run.ended_at.is_none() && has_live_session {
            return TaskrunJobClassification {
                keep_job: true,
                db_classification: "live_running",
            };
        }

        if task_run.status == "running" {
            return TaskrunJobClassification {
                keep_job: false,
                db_classification: session_status_classification(sessions),
            };
        }

        return TaskrunJobClassification {
            keep_job: false,
            db_classification: taskrun_status_classification(&task_run.status),
        };
    }

    TaskrunJobClassification {
        keep_job: has_live_session,
        db_classification: if has_live_session {
            "live_session_without_task_run"
        } else {
            "absent"
        },
    }
}

fn taskrun_status_classification(status: &str) -> &'static str {
    match status {
        "completed" => "task_run_completed",
        "failed" => "task_run_failed",
        "interrupted" => "task_run_interrupted",
        "running" => "task_run_running_without_live_session",
        _ => "task_run_unknown_status",
    }
}

// ─── Note association pruning ────────────────────────────────────────────────

impl CoordinatorActor {
    /// Prune stale, low-weight note associations for all projects.
    /// Called once per hour from the background tick.
    pub(super) async fn prune_note_associations(&self) {
        let project_repo = ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );

        let projects = match project_repo.list().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: failed to list projects for association pruning");
                return;
            }
        };

        let note_repo = djinn_db::NoteRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );

        for project in projects {
            match note_repo.prune_associations(&project.id).await {
                Ok(count) => {
                    if count > 0 {
                        tracing::info!(
                            project_id = %project.id,
                            deleted = count,
                            "CoordinatorActor: pruned stale note associations"
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        project_id = %project.id,
                        error = %e,
                        "CoordinatorActor: failed to prune note associations"
                    );
                }
            }
        }
    }
}

// Task #8: the `sweep_removes_worktree_for_closed_task` test used to cover
// the worktree-GC branch of `sweep_stale_resources`.  That branch no longer
// exists — the supervisor-driven dispatch path never creates task worktrees,
// so there is nothing to GC.  The per-project task-branch cleanup kept here
// is exercised end-to-end by the task_merge integration tests.

// ─── Stale task_run sweep ────────────────────────────────────────────────────

/// Flip `task_runs` rows still in `running` (with NULL `ended_at`) older than
/// [`STALE_TASK_RUN_THRESHOLD_SECS`] to `interrupted`.
///
/// Catches the residue from worker pods that died without flushing their
/// terminal `update_task_run_status` RPC (host crash mid-run, K8s SIGKILL
/// past the termination grace, network partition). The per-task teardown
/// reap in `supervisor_runner` covers the common case; this is the safety
/// net for paths it can't see (host restart while pods are mid-flight, etc).
async fn reap_stale_task_runs(db: &djinn_db::Database) {
    reap_stale_task_runs_with_threshold(db, STALE_TASK_RUN_THRESHOLD_SECS, "periodic").await;
}

/// Startup variant: aggressive (~10s threshold) because any `running` row
/// older than that at boot is from a prior process whose workers can no
/// longer reach us.
pub(super) async fn reap_stale_task_runs_for_startup(db: &djinn_db::Database) {
    reap_stale_task_runs_with_threshold(db, STARTUP_TASK_RUN_THRESHOLD_SECS, "startup").await;
}

async fn reap_stale_task_runs_with_threshold(
    db: &djinn_db::Database,
    threshold_secs: i64,
    reason: &'static str,
) {
    let cutoff = time::OffsetDateTime::now_utc() - time::Duration::seconds(threshold_secs);
    let format = time::macros::format_description!(
        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z"
    );
    let threshold_iso = match cutoff.format(&format) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "reap_stale_task_runs: failed to format threshold");
            return;
        }
    };

    let repo = djinn_db::repositories::task_run::TaskRunRepository::new(db.clone());
    match repo.reap_stale_running(&threshold_iso).await {
        Ok(ids) if !ids.is_empty() => {
            tracing::warn!(
                count = ids.len(),
                threshold = %threshold_iso,
                reason = reason,
                "CoordinatorActor: reaped stale 'running' task_runs"
            );
        }
        Ok(_) => {}
        Err(e) => {
            tracing::warn!(error = %e, reason = reason, "CoordinatorActor: reap_stale_task_runs failed");
        }
    }
}
