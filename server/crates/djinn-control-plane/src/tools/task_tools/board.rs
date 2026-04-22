use super::*;
use djinn_db::NoteRepository;

pub(super) async fn board_health_impl(
    server: &DjinnMcpServer,
    p: BoardHealthParams,
) -> Json<ErrorOr<BoardHealthResponse>> {
    let project_id = match server.require_project_id(&p.project).await {
        Ok(id) => id,
        Err(e) => return Json(ErrorOr::Error(e)),
    };
    let stale_hours = p.stale_threshold_hours.unwrap_or(24).max(1);
    let repo = TaskRepository::new(server.state.db().clone(), server.state.event_bus());
    match repo.board_health(stale_hours).await {
        Ok(report) => match serde_json::from_value::<BoardHealthResponse>(report) {
            Ok(mut parsed) => {
                let note_repo =
                    NoteRepository::new(server.state.db().clone(), server.state.event_bus());
                if let Ok(memory_health) = note_repo.health(&project_id).await {
                    parsed.memory_health = Some(memory_health);
                }

                // Surface aggregate coordinator metrics (throughput + PR errors).
                if let Some(coordinator) = server.state.coordinator().await
                    && let Ok(status) = coordinator.get_status()
                {
                    if !status.epic_throughput.is_empty() {
                        parsed.epic_throughput = Some(status.epic_throughput);
                    }
                    if !status.pr_errors.is_empty() {
                        parsed.pr_errors = Some(status.pr_errors);
                    }
                }

                // Surface whether the GitHub App is configured (ADR-039).
                // Per-org "pending OAuth App approval" warnings belonged to
                // the retired device-code flow and are gone; the modern
                // install model surfaces missing installations at the UI
                // level (see `github_app_installations`).
                if djinn_provider::github_app::app_id().is_err() {
                    let warnings = parsed.warnings.get_or_insert_with(Vec::new);
                    warnings.push("github_app_not_configured".to_string());
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

            // `stale_batch_worktrees*` fields are retained on the response
            // for schema stability but always report empty: the supervisor
            // path never creates `.djinn/worktrees/batch-*` directories, so
            // there is nothing to reconcile.
            let mut parsed = match serde_json::from_value::<BoardReconcileResponse>(
                serde_json::json!({
                    "healed_tasks": result.get("healed_tasks").cloned().unwrap_or(serde_json::json!(0)),
                    "healed_task_ids": result.get("healed_task_ids").cloned().unwrap_or(serde_json::json!([])),
                    "recovered_tasks": result.get("recovered_tasks").cloned().unwrap_or(serde_json::json!(0)),
                    "reviews_triggered": result.get("reviews_triggered").cloned().unwrap_or(serde_json::json!(0)),
                    "stale_sessions_finalized": finalized_stale_session_ids.len(),
                    "stale_session_ids": finalized_stale_session_ids,
                    "recovery_triggered": recovery_triggered,
                    "stale_batch_worktrees_removed": 0,
                    "stale_batch_worktrees": Vec::<String>::new(),
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
