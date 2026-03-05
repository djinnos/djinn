use super::*;

impl CoordinatorActor {
    /// Spawn background health-check tasks for all projects (or one) that have
    /// setup/verification commands configured (ADR-014, task bit0).
    pub(super) async fn validate_all_project_health(&mut self, project_id_filter: Option<String>) {
        struct ProjectRow {
            id: String,
            path: String,
            setup_commands: String,
            verification_commands: String,
        }

        let rows: Vec<ProjectRow> =
            sqlx::query("SELECT id, path, setup_commands, verification_commands FROM projects")
                .fetch_all(self.db.pool())
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|row| {
                    use sqlx::Row;
                    ProjectRow {
                        id: row.get("id"),
                        path: row.get("path"),
                        setup_commands: row.get("setup_commands"),
                        verification_commands: row.get("verification_commands"),
                    }
                })
                .collect();

        for row in rows {
            if let Some(ref filter) = project_id_filter {
                if row.id != *filter {
                    continue;
                }
            }

            let setup_cmds: Vec<CommandSpec> =
                serde_json::from_str(&row.setup_commands).unwrap_or_default();
            let verify_cmds: Vec<CommandSpec> =
                serde_json::from_str(&row.verification_commands).unwrap_or_default();

            if setup_cmds.is_empty() && verify_cmds.is_empty() {
                // No commands configured — always healthy; clear any stale failure.
                if self.unhealthy_projects.remove(&row.id).is_some() {
                    let _ = self.events_tx.send(DjinnEvent::ProjectHealthChanged {
                        project_id: row.id.clone(),
                        healthy: true,
                        error: None,
                    });
                }
                continue;
            }

            let sender = self.self_sender.clone();
            let db = self.db.clone();
            let events_tx = self.events_tx.clone();
            let project_id = row.id.clone();
            let path = row.path.clone();

            tracing::info!(
                project_id = %project_id,
                setup_count = setup_cmds.len(),
                verify_count = verify_cmds.len(),
                "CoordinatorActor: spawning project health check"
            );

            tokio::spawn(async move {
                let (healthy, error) = match run_project_health_check(
                    project_id.clone(),
                    path,
                    setup_cmds,
                    verify_cmds,
                    db,
                    events_tx,
                )
                .await
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

/// Create a temporary git worktree, run setup + verification commands, clean up,
/// and return `Ok(())` if all commands pass or `Err(reason)` if any fail.
pub(super) async fn run_project_health_check(
    project_id: String,
    path: String,
    setup_cmds: Vec<CommandSpec>,
    verify_cmds: Vec<CommandSpec>,
    db: Database,
    events_tx: broadcast::Sender<DjinnEvent>,
) -> Result<(), String> {
    let project_path = std::path::PathBuf::from(&path);

    // Resolve target branch (falls back to "main").
    let target_branch = GitSettingsRepository::new(db, events_tx)
        .get(&project_id)
        .await
        .map(|s| s.target_branch)
        .unwrap_or_else(|_| "main".to_string());

    let git = GitActorHandle::spawn(project_path.clone())
        .map_err(|e| format!("failed to open git repo at {path}: {e}"))?;

    // Remove any stale health-check worktree from a previous crashed run.
    // Prune first to clear orphaned git metadata (directory may already be
    // gone but git worktree list still shows it), then force-remove the
    // directory if it still exists on disk.
    let _ = git
        .run_command(vec!["worktree".into(), "prune".into()])
        .await;
    let stale = project_path
        .join(".djinn")
        .join("worktrees")
        .join("_health_check");
    if stale.exists() {
        let _ = git.remove_worktree(&stale).await;
    }

    let wt_path = git
        .create_worktree("_health_check", &target_branch, true)
        .await
        .map_err(|e| format!("failed to create health-check worktree: {e}"))?;

    let result = async {
        if !setup_cmds.is_empty() {
            let results = run_commands(&setup_cmds, &wt_path)
                .await
                .map_err(|e| format!("setup error: {e}"))?;
            if let Some(f) = results.last().filter(|r| r.exit_code != 0) {
                return Err(format!(
                    "setup command '{}' failed (exit {}): {}",
                    f.name,
                    f.exit_code,
                    f.stderr.trim()
                ));
            }
        }
        if !verify_cmds.is_empty() {
            let results = run_commands(&verify_cmds, &wt_path)
                .await
                .map_err(|e| format!("verification error: {e}"))?;
            if let Some(f) = results.last().filter(|r| r.exit_code != 0) {
                return Err(format!(
                    "verification command '{}' failed (exit {}): {}",
                    f.name,
                    f.exit_code,
                    f.stderr.trim()
                ));
            }
        }
        Ok(())
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
