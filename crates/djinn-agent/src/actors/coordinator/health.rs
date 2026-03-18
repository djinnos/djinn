use super::*;
use crate::verification::StepEvent;

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

            let project_cfg = match crate::verification::settings::load_commands(
                std::path::Path::new(&project.path),
            ) {
                Ok(cfg) => cfg,
                Err(e) => {
                    tracing::warn!(project_id = %project.id, error = %e, "CoordinatorActor: failed to load commands for health check");
                    continue;
                }
            };
            if project_cfg.0.is_empty() && project_cfg.1.is_empty() {
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
            let path = project.path.clone();
            let db = self.db.clone();

            tracing::info!(
                project_id = %project_id,
                setup_count = project_cfg.0.len(),
                verify_count = project_cfg.1.len(),
                "CoordinatorActor: spawning project health check"
            );

            tokio::spawn(async move {
                let (healthy, error) =
                    match run_project_health_check(project_id.clone(), path, db, events_tx).await {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use djinn_core::models::TransitionAction;
    use djinn_db::{EpicRepository, TaskRepository};
    use tokio::sync::broadcast;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn sweep_removes_worktree_for_closed_task() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
        let (tx, _rx) = broadcast::channel(32);
        let project = test_helpers::create_test_project(&db).await;

        let events = crate::events::event_bus_for(&tx);
        let epic = EpicRepository::new(db.clone(), events.clone())
            .create_for_project(
                &project.id,
                djinn_db::EpicCreateInput {
                    title: "epic",
                    description: "",
                    emoji: "",
                    color: "",
                    owner: "",
                    memory_refs: None,
                },
            )
            .await
            .unwrap();

        let task_repo = TaskRepository::new(db.clone(), events);
        let task = task_repo
            .create_in_project(
                &project.id,
                Some(&epic.id),
                "stale",
                "",
                "",
                "task",
                0,
                "",
                None,
            )
            .await
            .unwrap();
        task_repo
            .transition(
                &task.id,
                TransitionAction::Close,
                "test",
                "system",
                None,
                None,
            )
            .await
            .unwrap();

        let short_id = task.short_id.clone();
        let worktree_path = std::path::PathBuf::from(&project.path)
            .join(".djinn")
            .join("worktrees")
            .join(&short_id);
        std::fs::create_dir_all(&worktree_path).unwrap();
        assert!(worktree_path.exists());

        sweep_stale_resources(&db, &ctx).await;

        assert!(
            !worktree_path.exists(),
            "stale worktree should be removed for closed task"
        );
    }
}

// ─── Project health check (ADR-014) ──────────────────────────────────────────

/// Create a temporary git worktree, run setup + verification commands, clean up,
/// and return `Ok(())` if all commands pass or `Err(reason)` if any fail.
pub(super) async fn run_project_health_check(
    project_id: String,
    path: String,
    db: djinn_db::Database,
    events_tx: broadcast::Sender<DjinnEventEnvelope>,
) -> Result<(), String> {
    let project_path = std::path::PathBuf::from(&path);

    // Resolve target branch (falls back to "main").
    let target_branch =
        GitSettingsRepository::new(db.clone(), crate::events::event_bus_for(&events_tx))
            .get(&project_id)
            .await
            .map(|s| s.target_branch)
            .unwrap_or_else(|_| "main".to_string());

    let git = GitActorHandle::spawn(project_path.clone())
        .map_err(|e| format!("failed to open git repo at {path}: {e}"))?;

    // Remove any stale health-check worktree from a previous crashed run.
    let stale = project_path
        .join(".djinn")
        .join("worktrees")
        .join("_health_check");
    if stale.exists() {
        let _ = tokio::fs::remove_dir_all(&stale).await;
    }
    let _ = git
        .run_command(vec!["worktree".into(), "prune".into()])
        .await;

    let wt_path = git
        .create_worktree("_health_check", &target_branch, true)
        .await
        .map_err(|e| format!("failed to create health-check worktree: {e}"))?;

    let result = async {
        let head = git
            .run_command(vec!["rev-parse".into(), "HEAD".into()])
            .await
            .map_err(|e| format!("failed to resolve target branch HEAD: {e}"))?;
        let commit_sha = head.stdout.trim().to_string();
        if commit_sha.is_empty() {
            return Err("failed to resolve non-empty target branch HEAD".to_string());
        }

        let verification =
            crate::verification::service::verify_commit(&project_id, &commit_sha, &wt_path, &db)
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
    .await;

    // Always remove the temporary worktree.
    if let Err(e) = git.remove_worktree(&wt_path).await {
        tracing::warn!(
            project_id = %project_id,
            error = %e,
            "CoordinatorActor: failed to remove health-check worktree"
        );
    }

    result
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

    for project in projects {
        let project_dir = std::path::PathBuf::from(&project.path);
        let worktrees_dir = project_dir.join(".djinn").join("worktrees");
        let active_sessions = session_repo
            .list_active_in_project(&project.id)
            .await
            .unwrap_or_default();
        let mut protected = std::collections::HashSet::new();
        for s in active_sessions {
            if let Some(w) = s.worktree_path {
                protected.insert(std::path::PathBuf::from(w));
            }
        }

        if worktrees_dir.exists()
            && let Ok(entries) = std::fs::read_dir(&worktrees_dir)
        {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with('.') || name == "_health_check" {
                    continue;
                }
                let wt_path = entry.path();
                if !wt_path.is_dir() || protected.contains(&wt_path) {
                    continue;
                }
                let should_remove = match task_repo.get_by_short_id(&name).await {
                    Ok(Some(task)) => task.status == "closed",
                    Ok(None) => true,
                    Err(_) => false,
                };
                if should_remove {
                    tracing::info!(project_id=%project.id, short_id=%name, worktree=%wt_path.display(), "CoordinatorActor: removing stale worktree");
                    crate::actors::slot::teardown_worktree(
                        &name,
                        &wt_path,
                        &project_dir,
                        app_state,
                        true,
                    )
                    .await;
                }
            }
        }

        if let Ok(git) = app_state.git_actor(&project_dir).await
            && let Ok(out) = git
                .run_command(vec!["branch".into(), "--format=%(refname:short)".into()])
                .await
        {
            for line in out.stdout.lines() {
                let Some(short_id) = line.strip_prefix("task/") else {
                    continue;
                };
                let wt_exists = worktrees_dir.join(short_id).exists();
                let should_delete = match task_repo.get_by_short_id(short_id).await {
                    Ok(Some(task)) => task.status == "closed" && !wt_exists,
                    Ok(None) => true,
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
