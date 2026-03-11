use super::*;

impl CoordinatorActor {
    /// Spawn background health-check tasks for all projects (or one) that have
    /// setup/verification commands configured (ADR-014, task bit0).
    pub(super) async fn validate_all_project_health(&mut self, project_id_filter: Option<String>) {
        let repo = ProjectRepository::new(self.db.clone(), self.events_tx.clone());
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

            let setup_cmds: Vec<CommandSpec> =
                serde_json::from_str(&project.setup_commands).unwrap_or_default();
            let verify_cmds: Vec<CommandSpec> =
                serde_json::from_str(&project.verification_commands).unwrap_or_default();

            // ── Pre-flight: verify git remote 'origin' exists ────────────
            // The squash-merge flow assumes `origin` is available. Without it,
            // execution loops infinitely (merge fails → task released → repeat).
            // This is a fast local check — no network calls.
            let project_path = std::path::PathBuf::from(&project.path);
            if project_path.exists() {
                let origin_check = tokio::process::Command::new("git")
                    .args(["remote", "get-url", "origin"])
                    .current_dir(&project_path)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status()
                    .await;
                match origin_check {
                    Ok(status) if !status.success() => {
                        let err = "No git remote 'origin' configured. Execution requires a remote to merge completed work. \
                             Add one with: git remote add origin <url> && git push -u origin main".to_string();
                        tracing::warn!(
                            project_id = %project.id,
                            "CoordinatorActor: project missing git remote 'origin' — blocking dispatch"
                        );
                        let _ = self.self_sender
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
                        let _ = self.self_sender
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

            if setup_cmds.is_empty() && verify_cmds.is_empty() {
                // No commands configured — always healthy; clear any stale failure.
                if self.unhealthy_projects.remove(&project.id).is_some() {
                    let _ = self.events_tx.send(DjinnEvent::ProjectHealthChanged {
                        project_id: project.id.clone(),
                        healthy: true,
                        error: None,
                    });
                }
                continue;
            }

            let sender = self.self_sender.clone();
            let db = self.db.clone();
            let events_tx = self.events_tx.clone();
            let project_id = project.id.clone();
            let path = project.path.clone();

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
