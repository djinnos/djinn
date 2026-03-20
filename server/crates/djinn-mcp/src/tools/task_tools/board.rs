use super::*;
use djinn_db::ProjectRepository;
use djinn_provider::oauth::github_app::GITHUB_APP_OAUTH_DB_KEY;

pub(super) async fn board_health_impl(
    server: &DjinnMcpServer,
    p: BoardHealthParams,
) -> Json<ErrorOr<BoardHealthResponse>> {
    if let Err(e) = server.require_project_id(&p.project).await {
        return Json(ErrorOr::Error(e));
    }
    let stale_hours = p.stale_threshold_hours.unwrap_or(24).max(1);
    let repo = TaskRepository::new(server.state.db().clone(), server.state.event_bus());
    match repo.board_health(stale_hours).await {
        Ok(report) => match serde_json::from_value::<BoardHealthResponse>(report) {
            Ok(mut parsed) => {
                // Surface any project health issues from the coordinator.
                if let Some(coordinator) = server.state.coordinator().await
                    && let Ok(status) = coordinator.get_status()
                {
                    if !status.unhealthy_projects.is_empty() {
                        parsed.project_issues = Some(status.unhealthy_projects);
                    }
                    // Surface epic throughput data for the Architect.
                    if !status.epic_throughput.is_empty() {
                        parsed.epic_throughput = Some(status.epic_throughput);
                    }
                }

                // Check for GitHub OAuth credential existence (ADR-039).
                {
                    let cred_repo = djinn_provider::repos::CredentialRepository::new(
                        server.state.db().clone(),
                        server.state.event_bus(),
                    );
                    let github_connected = cred_repo
                        .exists(GITHUB_APP_OAUTH_DB_KEY)
                        .await
                        .unwrap_or(false);
                    if !github_connected {
                        let warnings = parsed.warnings.get_or_insert_with(Vec::new);
                        warnings.push("github_not_connected".to_string());
                    }
                }

                // Surface LSP server warnings (missing binaries).
                let lsp_warnings = server.state.lsp().warnings().await;
                if !lsp_warnings.is_empty() {
                    parsed.lsp_warnings = Some(
                        lsp_warnings
                            .into_iter()
                            .map(|w| BoardHealthLspWarning {
                                server: w.server,
                                message: w.message,
                            })
                            .collect(),
                    );
                }

                Json(ErrorOr::Ok(parsed))
            }
            Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
        },
        Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
    }
}

pub(super) async fn board_reconcile_impl(
    server: &DjinnMcpServer,
    p: BoardReconcileParams,
) -> Json<ErrorOr<BoardReconcileResponse>> {
    let project_id = match server.require_project_id(&p.project).await {
        Ok(id) => id,
        Err(e) => return Json(ErrorOr::Error(e)),
    };
    let stale_hours = p.stale_threshold_hours.unwrap_or(24).max(1);
    let repo = TaskRepository::new(server.state.db().clone(), server.state.event_bus());
    let Some(pool) = server.state.pool().await else {
        return Json(ErrorOr::Error(ErrorResponse::new(
            "slot pool actor not initialized",
        )));
    };
    let Some(coordinator) = server.state.coordinator().await else {
        return Json(ErrorOr::Error(ErrorResponse::new(
            "coordinator actor not initialized",
        )));
    };
    let session_repo = SessionRepository::new(server.state.db().clone(), server.state.event_bus());

    match repo.reconcile(stale_hours).await {
        Ok(result) => {
            let running_sessions = match session_repo.list_active_in_project(&project_id).await {
                Ok(sessions) => sessions,
                Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
            };

            let mut finalized_stale_session_ids = Vec::new();
            let mut active_worktree_paths = std::collections::HashSet::new();
            for session in &running_sessions {
                let has_runtime_session = if let Some(task_id) = session.task_id.as_deref() {
                    match pool.has_session(task_id).await {
                        Ok(v) => v,
                        Err(e) => {
                            return Json(ErrorOr::Error(ErrorResponse::new(e.to_string())));
                        }
                    }
                } else {
                    true
                };
                if has_runtime_session {
                    if let Some(path) = &session.worktree_path {
                        active_worktree_paths.insert(path.clone());
                    }
                    continue;
                }
                if session_repo
                    .update(
                        &session.id,
                        SessionStatus::Interrupted,
                        session.tokens_in,
                        session.tokens_out,
                    )
                    .await
                    .is_ok()
                {
                    finalized_stale_session_ids.push(session.id.clone());
                }
            }

            let recovery_triggered = if finalized_stale_session_ids.is_empty() {
                false
            } else {
                coordinator
                    .trigger_dispatch_for_project(&project_id)
                    .await
                    .is_ok()
            };

            // ── batch-* worktree cleanup (ADR-016) ───────────────────
            let mut stale_batch_worktrees: Vec<String> = Vec::new();
            let project_repo =
                ProjectRepository::new(server.state.db().clone(), server.state.event_bus());
            if let Ok(Some(project)) = project_repo.get(&project_id).await {
                let project_path = std::path::PathBuf::from(&project.path);
                let worktrees_dir = project_path.join(".djinn").join("worktrees");

                if let Ok(entries) = std::fs::read_dir(&worktrees_dir) {
                    let batch_dirs: Vec<std::path::PathBuf> = entries
                        .filter_map(|e| e.ok())
                        .filter(|e| {
                            e.file_name()
                                .to_str()
                                .map(|n| n.starts_with("batch-"))
                                .unwrap_or(false)
                                && e.path().is_dir()
                        })
                        .map(|e| e.path())
                        .collect();

                    if !batch_dirs.is_empty()
                        && let Ok(git) = server.state.git_actor(&project_path).await
                    {
                        for batch_dir in batch_dirs {
                            let batch_str = batch_dir.display().to_string();
                            if active_worktree_paths.contains(&batch_str) {
                                continue;
                            }
                            tracing::info!(
                                project_id = %project_id,
                                worktree = %batch_dir.display(),
                                "board_reconcile: removing stale batch-* worktree"
                            );
                            if let Err(e) = git.remove_worktree(&batch_dir).await {
                                tracing::warn!(
                                    project_id = %project_id,
                                    worktree = %batch_dir.display(),
                                    error = %e,
                                    "board_reconcile: failed to remove stale batch worktree"
                                );
                            } else {
                                stale_batch_worktrees.push(
                                    batch_dir
                                        .file_name()
                                        .unwrap_or_default()
                                        .to_string_lossy()
                                        .into_owned(),
                                );
                            }
                        }
                        if !stale_batch_worktrees.is_empty() {
                            let _ = git
                                .run_command(vec!["worktree".into(), "prune".into()])
                                .await;
                        }
                    }
                }
            }

            let mut parsed = match serde_json::from_value::<BoardReconcileResponse>(
                serde_json::json!({
                    "healed_tasks": result.get("healed_tasks").cloned().unwrap_or(serde_json::json!(0)),
                    "healed_task_ids": result.get("healed_task_ids").cloned().unwrap_or(serde_json::json!([])),
                    "recovered_tasks": result.get("recovered_tasks").cloned().unwrap_or(serde_json::json!(0)),
                    "reviews_triggered": result.get("reviews_triggered").cloned().unwrap_or(serde_json::json!(0)),
                    "stale_sessions_finalized": finalized_stale_session_ids.len(),
                    "stale_session_ids": finalized_stale_session_ids,
                    "recovery_triggered": recovery_triggered,
                    "stale_batch_worktrees_removed": stale_batch_worktrees.len(),
                    "stale_batch_worktrees": stale_batch_worktrees,
                }),
            ) {
                Ok(v) => v,
                Err(e) => return Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
            };

            parsed.stale_sessions_finalized = parsed.stale_session_ids.len();
            parsed.stale_batch_worktrees_removed = parsed.stale_batch_worktrees.len();

            Json(ErrorOr::Ok(parsed))
        }
        Err(e) => Json(ErrorOr::Error(ErrorResponse::new(e.to_string()))),
    }
}
