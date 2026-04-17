use super::*;
use crate::verification::StepEvent;
use djinn_workspace::MirrorManager;

impl CoordinatorActor {
    /// Spawn background health-check tasks for all projects (or one) that have
    /// setup/verification commands configured (ADR-014, task bit0).
    pub(super) async fn validate_all_project_health(&mut self, project_id_filter: Option<String>) {
        let repo = ProjectRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let projects = match repo.list().await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: failed to list projects for health check");
                return;
            }
        };

        for project in projects {
            if let Some(ref filter) = project_id_filter
                && project.id != *filter
            {
                continue;
            }

            // ── Pre-flight: verify git remote 'origin' exists ────────────
            // The squash-merge flow assumes `origin` is available. Without it,
            // execution loops infinitely (merge fails → task released → repeat).
            // This is a fast local check — no network calls.
            let project_path = std::path::PathBuf::from(&project.path);
            if project_path.exists() {
                let mut cmd = std::process::Command::new("git");
                cmd.args(["remote", "get-url", "origin"])
                    .current_dir(&project_path)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null());
                let origin_check = crate::process::status(cmd).await;
                match origin_check {
                    Ok(status) if !status.success() => {
                        let err = "No git remote 'origin' configured. Execution requires a remote to merge completed work. \
                             Add one with: git remote add origin <url> && git push -u origin main".to_string();
                        tracing::warn!(
                            project_id = %project.id,
                            "CoordinatorActor: project missing git remote 'origin' — blocking dispatch"
                        );
                        let _ = self
                            .self_sender
                            .send(CoordinatorMessage::SetProjectHealth {
                                project_id: project.id.clone(),
                                healthy: false,
                                error: Some(err),
                            })
                            .await;
                        continue;
                    }
                    Err(e) => {
                        let err = format!(
                            "Failed to check git remote 'origin': {e}. \
                             Ensure git is installed and the project path is a valid repository."
                        );
                        tracing::warn!(
                            project_id = %project.id,
                            error = %e,
                            "CoordinatorActor: failed to run git remote check"
                        );
                        let _ = self
                            .self_sender
                            .send(CoordinatorMessage::SetProjectHealth {
                                project_id: project.id.clone(),
                                healthy: false,
                                error: Some(err),
                            })
                            .await;
                        continue;
                    }
                    _ => {} // origin exists, proceed
                }
            }

            let project_settings = match crate::verification::settings::load_settings(
                std::path::Path::new(&project.path),
            ) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(project_id = %project.id, error = %e, "CoordinatorActor: failed to load settings for health check");
                    continue;
                }
            };
            if project_settings.setup.is_empty() && project_settings.verification_rules.is_empty() {
                // No commands configured — always healthy; clear any stale failure.
                if self.unhealthy_projects.remove(&project.id).is_some() {
                    let _ = self
                        .events_tx
                        .send(DjinnEventEnvelope::project_health_changed(
                            &project.id,
                            true,
                            None,
                        ));
                }
                continue;
            }

            let sender = self.self_sender.clone();
            let events_tx = self.events_tx.clone();
            let project_id = project.id.clone();
            let db = self.db.clone();
            let mirror = self.mirror.clone();

            tracing::info!(
                project_id = %project_id,
                setup_count = project_settings.setup.len(),
                rules_count = project_settings.verification_rules.len(),
                "CoordinatorActor: spawning project health check"
            );

            tokio::spawn(async move {
                let (healthy, error) =
                    match run_project_health_check(project_id.clone(), mirror, db, events_tx).await
                    {
                        Ok(()) => (true, None),
                        Err(e) => (false, Some(e)),
                    };
                let _ = sender
                    .send(CoordinatorMessage::SetProjectHealth {
                        project_id,
                        healthy,
                        error,
                    })
                    .await;
            });
        }
    }
}

// ─── Project health check (ADR-014) ──────────────────────────────────────────

/// Clone a hardlinked ephemeral workspace from the project's bare mirror,
/// run setup + verification commands inside it, and return `Ok(())` if all
/// commands pass or `Err(reason)` if any fail.
///
/// The workspace is owned by a `TempDir` and cleans itself up on drop —
/// no explicit teardown is needed, and no `.djinn/worktrees/_health_check`
/// debris is ever left behind (unlike the legacy worktree flow this replaced).
pub(super) async fn run_project_health_check(
    project_id: String,
    mirror: Option<Arc<MirrorManager>>,
    db: djinn_db::Database,
    events_tx: broadcast::Sender<DjinnEventEnvelope>,
) -> Result<(), String> {
    // Resolve target branch (falls back to "main").
    let target_branch =
        GitSettingsRepository::new(db.clone(), crate::events::event_bus_for(&events_tx))
            .get(&project_id)
            .await
            .map(|s| s.target_branch)
            .unwrap_or_else(|_| "main".to_string());

    let mirror = mirror.ok_or_else(|| {
        "health check requires a configured MirrorManager (none attached to coordinator)"
            .to_string()
    })?;

    let workspace = mirror
        .clone_ephemeral(&project_id, &target_branch)
        .await
        .map_err(|e| format!("failed to clone ephemeral workspace for health check: {e}"))?;
    let wt_path = workspace.path().to_path_buf();

    // Resolve HEAD inside the ephemeral clone — `--branch` checked out
    // `target_branch`, so `HEAD` points at its tip commit.
    let head_out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&wt_path)
        .args(["rev-parse", "HEAD"])
        .output()
        .await
        .map_err(|e| format!("failed to resolve target branch HEAD: {e}"))?;
    if !head_out.status.success() {
        let stderr = String::from_utf8_lossy(&head_out.stderr);
        return Err(format!(
            "failed to resolve target branch HEAD: {}",
            stderr.trim()
        ));
    }
    let commit_sha = String::from_utf8_lossy(&head_out.stdout).trim().to_string();
    if commit_sha.is_empty() {
        return Err("failed to resolve non-empty target branch HEAD".to_string());
    }

    // For health checks there is no task/role, so no override; fall back to
    // full-project commands (resolve_scoped_commands will use the full
    // verification list when no rules are configured or nothing matches).
    let scoped_commands =
        crate::verification::scoped::resolve_scoped_commands(&wt_path, &target_branch, None);
    let verification = crate::verification::service::verify_commit(
        &project_id,
        &commit_sha,
        &wt_path,
        &db,
        &scoped_commands,
    )
    .await
    .map_err(|e| format!("health-check verification error: {e}"))?;

    for r in &verification.setup_results {
        let _ = events_tx.send(DjinnEventEnvelope::verification_step(
            &project_id,
            None,
            "setup",
            &StepEvent::Finished {
                index: 0,
                name: r.name.clone(),
                exit_code: r.exit_code,
                duration_ms: r.duration_ms,
                stdout: r.stdout.clone(),
                stderr: r.stderr.clone(),
            },
        ));
    }

    if verification.cached {
        let _ = events_tx.send(DjinnEventEnvelope::verification_step(
            &project_id,
            None,
            "verification",
            &StepEvent::CacheHit {
                commit_sha: commit_sha.clone(),
                cached_at: String::new(),
                original_duration_ms: verification.total_duration_ms,
            },
        ));
    } else {
        for r in &verification.verification_results {
            let _ = events_tx.send(DjinnEventEnvelope::verification_step(
                &project_id,
                None,
                "verification",
                &StepEvent::Finished {
                    index: 0,
                    name: r.name.clone(),
                    exit_code: r.exit_code,
                    duration_ms: r.duration_ms,
                    stdout: r.stdout.clone(),
                    stderr: r.stderr.clone(),
                },
            ));
        }
    }

    // `workspace` drops here — the `TempDir` deletes its contents on drop and
    // the object db was only hardlinked from the mirror, so nothing is left
    // behind on disk.
    if verification.passed {
        Ok(())
    } else {
        let msg = verification
            .verification_results
            .last()
            .map(|f| {
                format!(
                    "verification command '{}' failed (exit {}): {}",
                    f.name,
                    f.exit_code,
                    f.stderr.trim()
                )
            })
            .unwrap_or_else(|| "verification failed".to_string());
        Err(msg)
    }
}

pub(super) async fn sweep_stale_resources(
    db: &djinn_db::Database,
    app_state: &crate::context::AgentContext,
) {
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
        let project_dir = std::path::PathBuf::from(&project.path);

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
