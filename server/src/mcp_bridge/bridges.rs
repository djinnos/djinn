use async_trait::async_trait;
use djinn_agent::actors::coordinator::CoordinatorHandle;
use djinn_agent::actors::slot::SlotPoolHandle;
use djinn_agent::lsp::LspManager;
use djinn_control_plane::bridge::{
    CoordinatorOps, CoordinatorStatus, LspOps, LspWarning, ModelPoolStatus, PoolStatus,
    ProposalRefinementStartRequest, ReconcileTerminateExecution, ReconcileTerminateKind,
    ReconcileTerminateObservations, ReconcileTerminateSnapshot, RunningTaskInfo, SlotPoolOps,
};

// ── Newtype wrappers ───────────────────────────────────────────────────────────

pub(super) struct CoordinatorBridge {
    pub handle: CoordinatorHandle,
    /// Needed by `record_supervisor_rework_reopen`, which delegates to the
    /// free-function attempt-lifecycle chokepoint that operates on the DB
    /// directly (the coordinator actor is not involved).
    pub db: djinn_db::Database,
}
pub(super) struct SlotPoolBridge(pub SlotPoolHandle);
pub(super) struct LspBridge(pub LspManager);

// ── CoordinatorBridge → CoordinatorOps ───────────────────────────────────────

#[async_trait]
impl CoordinatorOps for CoordinatorBridge {
    fn get_status(&self) -> Result<CoordinatorStatus, String> {
        let s = self.handle.get_status().map_err(|e| e.to_string())?;
        Ok(CoordinatorStatus {
            tasks_dispatched: s.tasks_dispatched,
            sessions_recovered: s.sessions_recovered,
            epic_throughput: s.epic_throughput,
            pr_errors: s.pr_errors,
        })
    }

    async fn trigger_dispatch_for_project(&self, project_id: &str) -> Result<(), String> {
        self.handle
            .trigger_dispatch_for_project(project_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn wake_refinement_run(&self, run_id: String) -> Result<(), String> {
        self.handle
            .wake_refinement_run(run_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn run_retrieval_health_checks(
        &self,
        check_names: Vec<String>,
        run_id: String,
    ) -> Result<Vec<djinn_core::doctor::DoctorCheckRun>, String> {
        self.handle
            .run_retrieval_health_checks(check_names, run_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn trigger_board_health_mismatch_scan(&self) -> Result<(), String> {
        self.handle
            .trigger_board_health_mismatch_scan()
            .await
            .map_err(|e| e.to_string())
    }

    async fn start_proposal_refinement(
        &self,
        request: ProposalRefinementStartRequest,
    ) -> Result<(), String> {
        self.handle
            .start_proposal_refinement(
                request.proposal_id,
                request.current_revision_seq,
                request.owner_user_id,
            )
            .await
            .map_err(|e| e.to_string())
    }

    async fn demand_proposal_refinement_round(
        &self,
        request: ProposalRefinementStartRequest,
    ) -> Result<(), String> {
        self.handle
            .demand_proposal_refinement_round(request.proposal_id, request.current_revision_seq)
            .await
            .map_err(|e| e.to_string())
    }

    async fn resolve_refinement_review(
        &self,
        proposal_id: String,
        accept: bool,
        feedback: Option<String>,
    ) -> Result<(), String> {
        self.handle
            .resolve_refinement_review(proposal_id, accept, feedback)
            .await
            .map_err(|e| e.to_string())
    }

    async fn record_supervisor_rework_reopen(
        &self,
        task_id: &str,
        action: &djinn_core::models::TransitionAction,
        reason: Option<&str>,
    ) {
        // Delegate to the same attempt-lifecycle chokepoint the in-process/RPC
        // transition path uses (`DirectServices::transition_task`). It no-ops
        // for non-rework actions and swallows its own errors (best-effort).
        djinn_agent::actors::coordinator::record_supervisor_rework_reopen(
            &self.db, task_id, action, reason,
        )
        .await;
    }
}

// ── SlotPoolBridge → SlotPoolOps ──────────────────────────────────────────────

#[async_trait]
impl SlotPoolOps for SlotPoolBridge {
    async fn get_status(&self) -> Result<PoolStatus, String> {
        let s = self.0.get_status().await.map_err(|e| e.to_string())?;
        Ok(PoolStatus {
            active_slots: s.active_slots,
            total_slots: s.total_slots,
            per_model: s
                .per_model
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        ModelPoolStatus {
                            active: v.active,
                            free: v.free,
                            total: v.total,
                        },
                    )
                })
                .collect(),
            running_tasks: s
                .running_tasks
                .into_iter()
                .map(|t| RunningTaskInfo {
                    task_id: t.task_id,
                    model_id: t.model_id,
                    slot_id: t.slot_id,
                    duration_seconds: t.duration_seconds,
                    idle_seconds: t.idle_seconds,
                    project_id: t.project_id,
                    no_progress_streak: t.no_progress_streak,
                })
                .collect(),
        })
    }

    async fn kill_session(&self, task_id: &str) -> Result<(), String> {
        self.0
            .kill_session(task_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn terminate_session(&self, task_id: &str) -> Result<(), String> {
        self.0
            .terminate_session(task_id)
            .await
            .map_err(|e| e.to_string())
    }

    async fn reconcile_terminate(
        &self,
        task_id: &str,
    ) -> Result<djinn_slot::pool::ReconcileTerminateSnapshot, String> {
        let snapshot = self
            .0
            .reconcile_terminate(task_id)
            .await
            .map_err(|e| e.to_string())?;
        let kind = |kind| match kind {
            djinn_slot::pool::ReconcileTerminateKind::GenuinelyAbsent => {
                ReconcileTerminateKind::GenuinelyAbsent
            }
            djinn_slot::pool::ReconcileTerminateKind::Terminated => {
                ReconcileTerminateKind::Terminated
            }
            djinn_slot::pool::ReconcileTerminateKind::DesyncReconciled => {
                ReconcileTerminateKind::DesyncReconciled
            }
            djinn_slot::pool::ReconcileTerminateKind::TeardownFailed => {
                ReconcileTerminateKind::TeardownFailed
            }
            djinn_slot::pool::ReconcileTerminateKind::SettlementFailed => {
                ReconcileTerminateKind::SettlementFailed
            }
            djinn_slot::pool::ReconcileTerminateKind::ReconciliationIncomplete => {
                ReconcileTerminateKind::ReconciliationIncomplete
            }
        };
        Ok(ReconcileTerminateSnapshot {
            ok: snapshot.ok,
            kind: kind(snapshot.kind),
            task_id: snapshot.task_id,
            executions: snapshot
                .executions
                .into_iter()
                .map(|e| ReconcileTerminateExecution {
                    session_id: e.session_id,
                    task_run_id: e.task_run_id,
                    teardown_owner: e.teardown_owner,
                    teardown_attempted: e.teardown_attempted,
                    teardown_error: e.teardown_error,
                    settlement_attempted: e.settlement_attempted,
                    settlement_error: e.settlement_error,
                })
                .collect(),
            observations: ReconcileTerminateObservations {
                initial_non_terminal_ids: snapshot.observations.initial_non_terminal_ids,
                initial_mapping_slot_id: snapshot.observations.initial_mapping_slot_id,
                initial_pending_teardown: snapshot.observations.initial_pending_teardown,
                initial_compacting: snapshot.observations.initial_compacting,
                fenced_generation: snapshot.observations.fenced_generation,
                initial_capture_error: snapshot.observations.initial_capture_error,
                final_non_terminal_ids: snapshot.observations.final_non_terminal_ids,
                final_mapping_slot_id: snapshot.observations.final_mapping_slot_id,
                final_pending_teardown: snapshot.observations.final_pending_teardown,
                final_reread_error: snapshot.observations.final_reread_error,
                pool_cleanup_error: snapshot.observations.pool_cleanup_error,
                completion_source: snapshot.observations.completion_source,
                underlying_kind: snapshot.observations.underlying_kind.map(kind),
            },
        })
    }

    async fn session_for_task(&self, task_id: &str) -> Result<Option<RunningTaskInfo>, String> {
        let result = self
            .0
            .session_for_task(task_id)
            .await
            .map_err(|e| e.to_string())?;
        Ok(result.map(|t| RunningTaskInfo {
            task_id: t.task_id,
            model_id: t.model_id,
            slot_id: t.slot_id,
            duration_seconds: t.duration_seconds,
            idle_seconds: t.idle_seconds,
            project_id: t.project_id,
            no_progress_streak: t.no_progress_streak,
        }))
    }

    async fn has_session(&self, task_id: &str) -> Result<bool, String> {
        self.0.has_session(task_id).await.map_err(|e| e.to_string())
    }
}

// ── LspBridge → LspOps ───────────────────────────────────────────────────────

#[async_trait]
impl LspOps for LspBridge {
    async fn warnings(&self) -> Vec<LspWarning> {
        self.0
            .warnings()
            .await
            .into_iter()
            .map(|w| LspWarning {
                server: w.server,
                message: w.message,
            })
            .collect()
    }
}
