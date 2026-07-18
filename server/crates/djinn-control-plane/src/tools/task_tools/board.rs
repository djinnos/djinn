use super::*;

pub(super) async fn board_health_impl(
    server: &DjinnMcpServer,
    p: BoardHealthParams,
) -> Json<ErrorOr<BoardHealthResponse>> {
    // Validate the project param even though the report itself is board-wide.
    if let Err(e) = server.require_project_id(&p.project).await {
        return Json(ErrorOr::Error(e));
    }
    let stale_hours = p.stale_threshold_hours.unwrap_or(24).max(1);
    if let Some(coordinator) = server.state.coordinator().await {
        if let Err(error) = coordinator.trigger_board_health_mismatch_scan().await {
            tracing::warn!(error = %error, "board_health: mismatch refresh trigger failed");
        }
    }
    let repo = TaskRepository::new(server.state.db().clone(), server.state.event_bus());
    match repo.board_health(stale_hours).await {
        Ok(report) => match serde_json::from_value::<BoardHealthResponse>(report) {
            Ok(mut parsed) => {
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
            let now = time::OffsetDateTime::now_utc();
            let liveness_config = djinn_core::liveness::LivenessConfig::OBSERVATION;
            for session in &running_sessions {
                let Some(task_id) = session.task_id.as_deref() else {
                    // Detached sessions (no task) aren't subject to slot
                    // liveness — leave them alone.
                    continue;
                };

                let slot_alive = match pool.has_session(task_id).await {
                    Ok(v) => v,
                    Err(e) => {
                        return Json(ErrorOr::Error(ErrorResponse::new(e.to_string())));
                    }
                };

                // Stale-slot path: slot already gone, just mark the row.
                // Wedged-slot path: slot alive but no LLM progress past
                // the observation threshold — kill the slot first, then
                // mark interrupted.
                // Runtime-cap path: slot alive, session is a worker on a
                // capped model (glm-5.2) past the wall-clock cap — kill
                // and reroute as runtime policy, NOT a task-quality strike.
                let should_finalize = if slot_alive {
                    let last_msg = session_repo
                        .last_message_at(&session.id)
                        .await
                        .unwrap_or(None);
                    let interruption = djinn_core::liveness::classify_session_interruption(
                        &session.agent_type,
                        &session.model_id,
                        &session.started_at,
                        last_msg.as_deref(),
                        session.tokens_in,
                        session.tokens_out,
                        now,
                        &liveness_config,
                        &djinn_core::liveness::RuntimeCapConfig::default_config(),
                    );
                    match interruption {
                        None => false,
                        Some(reason) => {
                            let label = reason.log_label();
                            if let Err(e) = pool.kill_session(task_id).await {
                                tracing::warn!(
                                    task_id = %task_id,
                                    session_id = %session.id,
                                    reason = label,
                                    error = %e,
                                    "board_reconcile: failed to kill session"
                                );
                                // Don't finalize a row whose slot we
                                // couldn't kill — the next sweep will
                                // see it again with fresher state.
                                false
                            } else {
                                match &reason {
                                    djinn_core::liveness::SessionInterruptReason::Wedged {
                                        idle_secs,
                                        zero_tokens,
                                    } => {
                                        tracing::warn!(
                                            task_id = %task_id,
                                            session_id = %session.id,
                                            idle_seconds = idle_secs,
                                            zero_tokens,
                                            "board_reconcile: killed wedged session"
                                        );
                                    }
                                    djinn_core::liveness::SessionInterruptReason::RuntimeCap {
                                        elapsed_secs,
                                        cap_secs,
                                        provider_id,
                                        model_name,
                                    } => {
                                        tracing::warn!(
                                            task_id = %task_id,
                                            session_id = %session.id,
                                            elapsed_secs,
                                            cap_secs,
                                            provider_id = %provider_id,
                                            model_name = %model_name,
                                            "board_reconcile: killed glm-5.2 runtime-cap session \
                                             (not a task-quality strike)"
                                        );
                                    }
                                }
                                true
                            }
                        }
                    }
                } else {
                    true
                };

                if !should_finalize {
                    continue;
                }
                if session_repo
                    .update(
                        &session.id,
                        SessionStatus::Interrupted,
                        session.tokens_in,
                        session.tokens_out,
                        session.cache_read_tokens,
                        session.cache_write_tokens,
                        None,
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
