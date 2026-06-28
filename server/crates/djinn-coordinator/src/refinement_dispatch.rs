// Proposal-refinement tribunal dispatch orchestration.
//
// Drives the Advocate → Adversary → Judge refinement loop by dispatching
// sessions through the slot pool and reading outcomes from the DB.
//
// Architecture:
//   - `drive_active_refinements()` is called from `run_tick()` on every
//     coordinator tick (~30s).
//   - For each active refinement, it checks for in-flight sessions. If a
//     session completed, it reads the outcome from the DB and advances the
//     `RefinementLoopState`.
//   - If no session is in-flight and the loop isn't complete, dispatch the
//     next phase.
//   - Each refinement task is created with `issue_type = "refinement"` and
//     `agent_type = "advocate"/"adversary"/"judge"`, so the supervisor's
//     role-overrides layer resolves the correct `AgentType`.
//
// Persistence:
//   - Adversary objections: persisted through
//     `ProposalRepository::add_debate_trail_entry()` with `kind = "objection"`.
//   - Judge verdicts: persisted through
//     `ProposalRepository::add_debate_trail_entry()` with `kind = "verdict"`.
//   - Stop metadata: persisted via `record_refinement_lifecycle`.

use std::time::{Duration, Instant as StdInstant};

use super::refinement::{RefinementPhase, StopReason};

use super::actor::CoordinatorActor;

/// How long to wait for a refinement session to start producing output
/// before treating it as stalled (conservative — sessions can take 5+ min).
const REFINEMENT_SESSION_TIMEOUT: Duration = Duration::from_secs(900);

/// How many consecutive times a refinement role session may fail to start
/// before the loop terminates instead of re-dispatching.
const REFINEMENT_DISPATCH_RETRY_CAP: i32 = 3;

/// The in-flight session tracking for one active refinement loop.
#[derive(Debug, Clone)]
pub(super) struct RefinementSession {
    /// The task id of the refinement task currently dispatched.
    pub task_id: String,
    /// Which phase this session is executing.
    pub phase: RefinementPhase,
    /// When the session was dispatched.
    pub dispatched_at: StdInstant,
    /// The model used for this session.
    #[allow(dead_code)]
    pub model_id: String,
}

// ─── Main dispatch loop ─────────────────────────────────────────────────────

impl CoordinatorActor {
    /// Drive all active refinement loops. Called from `run_tick()`.
    pub(super) async fn drive_active_refinements(&mut self) {
        let proposal_ids: Vec<String> = self.active_refinements.keys().cloned().collect();

        for proposal_id in proposal_ids {
            self.drive_one_refinement(&proposal_id).await;
        }

        // Clean up completed refinements.
        self.active_refinements
            .retain(|_, state| !state.is_complete());
    }

    /// Drive a single refinement loop.
    async fn drive_one_refinement(&mut self, proposal_id: &str) {
        let Some(state) = self.active_refinements.get(proposal_id).cloned() else {
            return;
        };
        if state.is_complete() {
            return;
        }

        // Check if there's an in-flight session for this refinement.
        if let Some(session) = self.refinement_sessions.get(proposal_id).cloned() {
            let still_running = self
                .pool
                .has_session(&session.task_id)
                .await
                .unwrap_or(false);

            if still_running {
                if session.dispatched_at.elapsed() > REFINEMENT_SESSION_TIMEOUT {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        task_id = %session.task_id,
                        phase = ?session.phase,
                        "Refinement session timed out"
                    );
                    self.close_refinement_task(&session.task_id, "refinement session timed out")
                        .await;
                    self.terminate_refinement(
                        proposal_id,
                        StopReason::AgentFailure {
                            role: format!("{:?}", session.phase),
                            error: "session timeout".into(),
                        },
                    )
                    .await;
                }
                return;
            }

            // The slot is no longer running this task. That can mean two very
            // different things:
            //   (a) the agent session actually ran and finished — process its
            //       outcome from the DB (debate trail / revisions); or
            //   (b) the session never started (runtime/devcontainer setup
            //       failure freed the slot before any session row was created).
            // Treating (b) as a completed-but-"dry" round silently burns rounds
            // on a dispatch outage and can hollow-converge the tribunal. Tell
            // them apart by whether any session row exists for the task.
            let session_ran = {
                let event_bus = crate::events::event_bus_for(&self.events_tx);
                let session_repo = djinn_db::SessionRepository::new(self.db.clone(), event_bus);
                match session_repo.list_for_task(&session.task_id).await {
                    Ok(sessions) => !sessions.is_empty(),
                    // On a DB read error, fail safe toward "it ran" so we don't
                    // spin forever re-dispatching.
                    Err(e) => {
                        tracing::warn!(
                            proposal_id = %proposal_id,
                            task_id = %session.task_id,
                            error = %e,
                            "Failed to read sessions for refinement task; assuming it ran"
                        );
                        true
                    }
                }
            };

            if !session_ran {
                // Dispatch/setup failure: the role never executed. Re-dispatch
                // the same phase on the next tick, bounded by a retry cap so a
                // persistently broken runtime escalates instead of looping.
                self.close_refinement_task(
                    &session.task_id,
                    "refinement role session never started (dispatch/setup failure)",
                )
                .await;
                self.refinement_sessions.remove(proposal_id);
                let over_cap = if let Some(state) = self.active_refinements.get_mut(proposal_id) {
                    state.dispatch_failures += 1;
                    state.dispatch_failures >= REFINEMENT_DISPATCH_RETRY_CAP
                } else {
                    true
                };
                if over_cap {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        phase = ?session.phase,
                        "Refinement role session repeatedly failed to start — terminating"
                    );
                    self.terminate_refinement(
                        proposal_id,
                        StopReason::AgentFailure {
                            role: format!("{:?}", session.phase),
                            error: format!(
                                "role session failed to start {REFINEMENT_DISPATCH_RETRY_CAP} times \
                                 (runtime/devcontainer setup failure)"
                            ),
                        },
                    )
                    .await;
                } else {
                    tracing::warn!(
                        proposal_id = %proposal_id,
                        phase = ?session.phase,
                        "Refinement role session never started; will re-dispatch (not counted as dry)"
                    );
                }
                return;
            }

            // Session actually ran — clear the dispatch-failure counter and
            // process the outcome, then close the task so finished phase/round
            // tasks don't linger `open` on the board.
            if let Some(state) = self.active_refinements.get_mut(proposal_id) {
                state.dispatch_failures = 0;
            }
            self.process_refinement_outcome(proposal_id, &session).await;
            self.close_refinement_task(&session.task_id, "refinement phase complete")
                .await;
            self.refinement_sessions.remove(proposal_id);
            return;
        }

        // No in-flight session — dispatch the next phase.
        self.dispatch_next_refinement_phase(proposal_id).await;
    }

    /// Dispatch the next refinement phase for a proposal.
    ///
    /// At each round boundary the deterministic P1 DoR evaluator is consulted
    /// so that readiness findings are available to the dispatched agent and
    /// included in stop metadata.
    ///
    /// Admission gate (proposal j479 / epic xofs): attribution is resolved and
    /// validated, the per-user/model cap is checked, and an in-flight
    /// reservation is recorded — all **before** task creation, spawn-budget
    /// consumption, or `pool.dispatch()`. At-cap phases defer non-terminally
    /// so the state machine retries on the next tick. Failed paths clear the
    /// reservation so no slot leaks.
    async fn dispatch_next_refinement_phase(&mut self, proposal_id: &str) {
        let Some(state) = self.active_refinements.get(proposal_id).cloned() else {
            return;
        };

        let phase = state.phase;
        let round = state.current_round;
        let revision_seq = state.current_revision_seq;

        // Human-review pause gate.
        if phase == RefinementPhase::AwaitingHumanReview {
            tracing::debug!(
                proposal_id = %proposal_id,
                "Refinement parked: awaiting human accept/reject of the refined spec"
            );
            return;
        }

        // Administrative dispatch-pause gate.
        if self.refinement_dispatch_paused(proposal_id).await {
            tracing::info!(
                proposal_id = %proposal_id,
                phase = ?phase,
                "Refinement dispatch deferred by administrative dispatch pause"
            );
            return;
        }

        // ── Step 1: Resolve and validate attribution before any side effect ──
        //
        // The attributed user determines the per-user cap scope and model
        // resolution. Missing, empty, dangling, or otherwise unresolvable
        // attribution fails closed with an operator-visible trace/status
        // reason, and NO refinement task row, spawn-budget consumption, or
        // pool dispatch occurs.
        let attributed_user_id = self
            .resolve_refinement_attributed_user(proposal_id, state.attributed_user_id.clone())
            .await;

        let Some(ref user_id) = attributed_user_id else {
            tracing::warn!(
                proposal_id = %proposal_id,
                phase = ?phase,
                "Refinement dispatch FAIL-CLOSED: no attributed user could be resolved \
                 (no explicit attributed_user_id and no proposal author). \
                 Refusing to dispatch without a real user identity."
            );
            self.terminate_refinement(
                proposal_id,
                StopReason::AgentFailure {
                    role: format!("{:?}", phase),
                    error: "attribution unresolvable: no user identity for refinement dispatch"
                        .into(),
                },
            )
            .await;
            return;
        };

        // Empty-attribution gate.
        if user_id.trim().is_empty() {
            tracing::warn!(
                proposal_id = %proposal_id,
                phase = ?phase,
                explicit = ?state.attributed_user_id,
                "Refinement dispatch: attributed user is empty — failing closed"
            );
            self.terminate_refinement(
                proposal_id,
                StopReason::AgentFailure {
                    role: format!("{:?}", phase),
                    error: "attributed user is empty".into(),
                },
            )
            .await;
            return;
        }

        // Dangling-attribution gate: user id must exist in DB.
        match djinn_db::UserRepository::new(self.db.clone())
            .get_by_id(user_id)
            .await
        {
            Ok(Some(_user)) => {}
            Ok(None) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    phase = ?phase,
                    user_id = %user_id,
                    "Refinement dispatch: attributed user does not resolve to a row — failing closed"
                );
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: format!("attributed user {user_id} not found in users table"),
                    },
                )
                .await;
                return;
            }
            Err(e) => {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    phase = ?phase,
                    user_id = %user_id,
                    error = %e,
                    "Refinement dispatch: failed to resolve attributed user row — failing closed"
                );
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: format!("failed to resolve attributed user: {e}"),
                    },
                )
                .await;
                return;
            }
        }

        // Read diverse_refinement setting at the round boundary.
        let diverse_refinement = self.read_diverse_refinement_setting(proposal_id).await;

        let readiness = self.evaluate_proposal_readiness(proposal_id).await;

        if let Some(ref readiness) = readiness
            && !readiness.ready
        {
            tracing::info!(
                proposal_id = %proposal_id,
                round,
                failure_count = readiness.failures.len(),
                "DoR evaluator found readiness failures at round boundary"
            );
        }

        let (agent_type, model_id) = self
            .resolve_refinement_dispatch_params(phase, diverse_refinement, Some(user_id))
            .await;

        // ── Step 2: Per-user/model cap admission (check + reserve atomically) ──
        //
        // Use the shared admission surface to check whether the attributed
        // user has room for one more session on the selected model. If at-cap,
        // defer non-terminally — no task row, no spawn-budget consumption, no
        // pool dispatch. The state machine retries on the next tick.
        let user_caps = self.resolve_model_caps_for_user(user_id).await;
        let cap = user_caps.get(&model_id).copied().unwrap_or(1);

        if !self
            .check_user_model_admission(user_id, &model_id, cap)
            .await
        {
            tracing::info!(
                proposal_id = %proposal_id,
                phase = ?phase,
                user_id = %user_id,
                model_id = %model_id,
                cap,
                "Refinement dispatch deferred: user at per-model concurrency cap \
                 (retryable — no task row, no spawn-budget, no pool dispatch)"
            );
            return;
        }

        // Record a provisional in-flight reservation so the per-user cap
        // reflects this admission immediately — before the task row exists.
        // This closes the check-reserve race window where another candidate
        // could pass admission for the same (user, model).
        let provisional_key = format!("refinement:{proposal_id}");
        self.provisional_admissions.insert(
            provisional_key.clone(),
            (Some(user_id.clone()), model_id.clone()),
        );

        // Build a readiness-enriched task description so the agent sees
        // current DoR findings.
        let readiness_context = readiness
            .as_ref()
            .and_then(|r| r.to_error_string())
            .unwrap_or_else(|| "Proposal currently meets all DoR checks.".to_string());

        // ── Step 3: Create the refinement task (first DB side effect) ────────
        //
        // The task row is created only AFTER the cap reservation exists. On
        // failure, the provisional reservation is cleared immediately.
        let task_id = match self
            .create_refinement_task_with_context(
                proposal_id,
                &agent_type,
                round,
                revision_seq,
                &readiness_context,
                Some(user_id),
            )
            .await
        {
            Some(id) => id,
            None => {
                self.clear_provisional_admission(&provisional_key);
                tracing::warn!(
                    proposal_id = %proposal_id,
                    phase = ?phase,
                    "Failed to create refinement task"
                );
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: "task creation failed".into(),
                    },
                )
                .await;
                return;
            }
        };

        // Re-key the provisional reservation to the real task id so that
        // existing reconciliation (pool liveness check) and session-start
        // cleanup can clear it, and so subsequent candidates for the same
        // (user, model) see the reservation under the durable key.
        self.rekey_provisional_to_inflight(&provisional_key, &task_id, user_id, &model_id)
            .await;

        // ── Step 4: Consume spawn budget ────────────────────────────────────
        //
        // On spawn-cap overflow, clear the in-flight reservation (the real
        // task id) before terminating so the slot is immediately available.
        {
            let state = self.active_refinements.get_mut(proposal_id).unwrap();
            if let Err(reason) = state.record_spawn() {
                tracing::warn!(
                    proposal_id = %proposal_id,
                    ?reason,
                    "Refinement spawn cap reached"
                );
                self.clear_inflight_dispatch(&task_id).await;
                self.close_refinement_task(
                    &task_id,
                    "refinement spawn cap reached — task will not be dispatched",
                )
                .await;
                self.persist_refinement_stop(proposal_id, &reason).await;
                self.refinement_sessions.remove(proposal_id);
                return;
            }
        }

        let project_path = self.resolve_refinement_project_path(proposal_id).await;

        // ── Step 5: Dispatch through the slot pool (last side effect) ───────
        //
        // On pool dispatch failure, clear the in-flight reservation so the
        // slot is immediately available and terminate the refinement.
        match self.pool.dispatch(&task_id, &project_path, &model_id).await {
            Ok(()) => {
                tracing::info!(
                    proposal_id = %proposal_id,
                    task_id = %task_id,
                    phase = ?phase,
                    round,
                    model_id = %model_id,
                    "Dispatched refinement session"
                );
                self.refinement_sessions.insert(
                    proposal_id.to_string(),
                    RefinementSession {
                        task_id,
                        phase,
                        dispatched_at: StdInstant::now(),
                        model_id,
                    },
                );
            }
            Err(e) => {
                self.clear_inflight_dispatch(&task_id).await;
                self.close_refinement_task(&task_id, "refinement dispatch failed (pool error)")
                    .await;
                tracing::warn!(
                    proposal_id = %proposal_id,
                    task_id = %task_id,
                    phase = ?phase,
                    error = %e,
                    "Failed to dispatch refinement session"
                );
                self.terminate_refinement(
                    proposal_id,
                    StopReason::AgentFailure {
                        role: format!("{:?}", phase),
                        error: format!("dispatch failed: {e}"),
                    },
                )
                .await;
            }
        }
    }
}

// ─── Refinement cap-deferral / shared-ledger regression tests ─────────────
//
// These integration-style tests exercise `dispatch_next_refinement_phase`
// through the `drive_active_refinements` / `drive_one_refinement` entry
// points, using a real in-memory DB and (where needed) a real slot pool.
//
// Coverage (epic xofs, proposal j479):
//   1. At-cap refinement defers before task creation.
//   2. Repeated at-cap ticks produce no extra tasks / orphan sessions /
//      spawn-budget consumption / pool.dispatch() calls.
//   3. Capacity-free retry dispatches exactly one refinement task.
//   4. Unresolved/dangling attribution fails closed.
//   5. Same-tick session-row lag with shared ledger.
//   6. Dispatch failure clears the refinement-created in-flight entry.

#[cfg(test)]
mod refinement_cap_tests {
    use super::*;
    use crate::SharedCoordinatorState;
    use crate::consolidation::DbConsolidationRunner;
    use crate::roles::RoleRegistry;
    use crate::types::{
        AutoMergeTracker, BackgroundWorkTracker, DEFAULT_MODEL_ID, PrCleanupConfig, STUCK_INTERVAL,
    };
    use djinn_core::events::{DjinnEventEnvelope, EventBus};
    use djinn_db::{
        ProposalCreateInput, ProposalRepository, SessionRepository, TaskRepository, UserRepository,
        UserSettingsRepository,
    };
    use djinn_provider::catalog::CatalogService;
    use djinn_provider::catalog::health::HealthTracker;
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;
    use std::time::{Duration, Instant as StdInstant};
    use tokio_util::sync::CancellationToken;

    /// The model used for refinement dispatch in `#[cfg(test)]` builds
    /// (hardcoded by `resolve_dispatch_models_for_role`).
    const TEST_MODEL: &str = DEFAULT_MODEL_ID; // "test/mock"

    // ── Fixture ──────────────────────────────────────────────────────────

    struct RefinementFixture {
        #[allow(dead_code)]
        db: djinn_db::Database,
        project_id: String,
        user_id: String,
        proposal_id: String,
    }

    /// Create a project, user, and proposal (with a project target) ready
    /// for refinement dispatch.
    async fn seed_refinement_fixture(db: &djinn_db::Database) -> RefinementFixture {
        let event_bus = EventBus::noop();
        let project = crate::test_helpers::create_test_project(db).await;
        let user = UserRepository::new(db.clone())
            .upsert_from_github(
                777_100,
                "refinement-cap-user",
                Some("Refinement cap test user"),
                None,
            )
            .await
            .expect("create refinement cap test user");
        let user_id = user.id.clone();

        // Create the proposal attributed to the user via the auth-context scope.
        let proposal = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                ProposalRepository::new(db.clone(), EventBus::noop())
                    .create(ProposalCreateInput {
                        title: "Refinement cap test proposal",
                        body: "A proposal for testing per-user model cap enforcement in refinement dispatch.",
                        acceptance_criteria: Some("[]"),
                        status: Some("building"),
                        body_format: None,
                    })
                    .await
                    .expect("create refinement cap test proposal")
            })
            .await;

        // Link the proposal to the project so `create_refinement_task_with_context`
        // and `resolve_refinement_project_path` can resolve a project.
        ProposalRepository::new(db.clone(), event_bus)
            .add_target(&proposal.id, &project.id, "primary")
            .await
            .expect("add proposal target");

        RefinementFixture {
            db: db.clone(),
            project_id: project.id,
            user_id,
            proposal_id: proposal.id,
        }
    }

    // ── Actor construction ───────────────────────────────────────────────

    /// Build a `CoordinatorActor` wired to the given pool and DB.
    fn build_refinement_actor(
        db: &djinn_db::Database,
        events_tx: &tokio::sync::broadcast::Sender<DjinnEventEnvelope>,
        pool: djinn_slot::SlotPoolHandle,
    ) -> CoordinatorActor {
        let cancel = CancellationToken::new();
        CoordinatorActor {
            receiver: tokio::sync::mpsc::channel(1).1,
            events: events_tx.subscribe(),
            cancel: cancel.clone(),
            tick: tokio::time::interval(STUCK_INTERVAL),
            db: db.clone(),
            events_tx: events_tx.clone(),
            pool,
            catalog: CatalogService::new(),
            health: HealthTracker::default(),
            role_registry: Arc::new(RoleRegistry::new()),
            lsp: djinn_lsp::LspManager::new(),
            self_sender: tokio::sync::mpsc::channel(1).0,
            status_tx: tokio::sync::watch::channel(SharedCoordinatorState {
                dispatched: 0,
                recovered: 0,
                epic_throughput: HashMap::new(),
                pr_errors: HashMap::new(),
                rate_limited_until: None,
            })
            .0,
            dispatch_limit: 50,
            model_priorities: HashMap::new(),
            pr_errors: HashMap::new(),
            last_dispatched: HashMap::new(),
            inflight_dispatches: HashMap::new(),
            provisional_admissions: HashMap::new(),
            dispatch_cooldowns: HashMap::new(),
            dispatch_failure_streak: HashMap::new(),
            background_work_tracker: BackgroundWorkTracker::default(),
            auto_merge_tracker: AutoMergeTracker::default(),
            consolidation_runner: Arc::new(DbConsolidationRunner::new(db.clone())),
            last_stale_sweep: StdInstant::now(),
            last_auto_dispatch_sweep: StdInstant::now(),
            last_proposal_review_sweep: StdInstant::now(),
            last_graph_refresh: StdInstant::now(),
            graph_warmer: None,
            mirror: None,
            runtime_ops: None,
            rpc_registry: None,
            prune_tick_counter: 0,
            throughput_events: HashMap::new(),
            escalation_counts: HashMap::new(),
            pr_status_cache: HashMap::new(),
            pr_draft_first_seen: HashMap::new(),
            review_stuck_sha_first_seen: HashMap::new(),
            merge_fail_count: HashMap::new(),
            auto_approve_attempted: HashMap::new(),
            delegated_to_github: HashMap::new(),
            conversations_resolved: HashMap::new(),
            handled_dequeues: HashMap::new(),
            stall_killed: HashSet::new(),
            last_idle_consolidation: None,
            idle_consolidation_cancel: None,
            idle_consolidation_handle: None,
            pr_cleanup_config: PrCleanupConfig::default(),
            active_refinements: HashMap::new(),
            refinement_sessions: HashMap::new(),
            dispatched: 0,
            recovered: 0,
        }
    }

    /// Create a slot pool with the test model configured.
    fn spawn_test_pool(db: &djinn_db::Database, max_slots: u32) -> djinn_slot::SlotPoolHandle {
        let cancel = CancellationToken::new();
        djinn_slot::SlotPoolHandle::spawn(
            crate::test_helpers::agent_context_from_db(db.clone(), cancel.clone()),
            cancel,
            djinn_slot::SlotPoolConfig {
                models: vec![djinn_slot::ModelSlotConfig {
                    model_id: TEST_MODEL.to_owned(),
                    max_slots,
                    roles: ["advocate", "adversary", "judge", "worker"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                }],
                role_priorities: HashMap::new(),
            },
        )
    }

    /// Seed a `RefinementLoopState` in the actor's `active_refinements` map.
    fn seed_refinement_state(
        actor: &mut CoordinatorActor,
        proposal_id: &str,
        attributed_user_id: Option<String>,
    ) {
        let state = super::super::refinement::RefinementLoopState::new(proposal_id, 1)
            .with_attributed_user(attributed_user_id);
        actor
            .active_refinements
            .insert(proposal_id.to_string(), state);
    }

    /// Materialize a running session row for `(user, model)` so the DB
    /// active-session count reflects it.
    async fn materialize_running_session(
        db: &djinn_db::Database,
        project_id: &str,
        task_id: &str,
        model_id: &str,
    ) -> String {
        SessionRepository::new(db.clone(), EventBus::noop())
            .create(djinn_db::CreateSessionParams {
                project_id,
                task_id: Some(task_id),
                model: model_id,
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("materialize running session")
            .id
    }

    /// Seed a task in the DB to host a session row.
    async fn seed_task(
        db: &djinn_db::Database,
        project_id: &str,
        user_id: &str,
        label: &str,
    ) -> String {
        djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.to_owned()), async {
                TaskRepository::new(db.clone(), EventBus::noop())
                    .create_in_project(
                        project_id,
                        None,
                        label,
                        "test task for cap fixture",
                        "",
                        "task",
                        0,
                        "worker",
                        Some("open"),
                        Some("[]"),
                    )
                    .await
                    .expect("seed fixture task")
                    .id
            })
            .await
    }

    /// Configure `max_sessions` for `(user, model)`.
    async fn set_user_cap(db: &djinn_db::Database, user_id: &str, model_id: &str, cap: u32) {
        UserSettingsRepository::new(db.clone())
            .upsert_max_sessions(user_id, &HashMap::from([(model_id.to_owned(), cap)]))
            .await
            .expect("set user max_sessions cap");
    }

    // ── AC#1 & AC#2: At-cap deferral + repeated ticks ────────────────────

    /// Regression: at-cap refinement phase defers before task creation, and
    /// repeated at-cap ticks create no extra refinement tasks, leave no
    /// orphan sessions, consume no spawn budget, and call no pool.dispatch().
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn at_cap_refinement_defers_and_repeated_ticks_are_noops() {
        let db = crate::test_helpers::create_test_db();
        let fixture = seed_refinement_fixture(&db).await;
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
        let pool = spawn_test_pool(&db, 4);

        // Seed one running session so the user is at cap (cap = 1).
        let blocking_task_id =
            seed_task(&db, &fixture.project_id, &fixture.user_id, "blocking").await;
        let _session_id =
            materialize_running_session(&db, &fixture.project_id, &blocking_task_id, TEST_MODEL)
                .await;
        set_user_cap(&db, &fixture.user_id, TEST_MODEL, 1).await;

        let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
        seed_refinement_state(
            &mut actor,
            &fixture.proposal_id,
            Some(fixture.user_id.clone()),
        );

        // ── Tick 1: should defer (at cap) ──
        actor.drive_active_refinements().await;

        assert!(
            actor.active_refinements.contains_key(&fixture.proposal_id),
            "refinement must still be active after at-cap deferral"
        );
        let state = &actor.active_refinements[&fixture.proposal_id];
        assert_eq!(
            state.phase,
            super::super::refinement::RefinementPhase::AdversaryAttack,
            "phase must not advance on deferral"
        );
        assert_eq!(state.total_spawns, 0, "no spawn budget consumed");
        assert!(
            actor.refinement_sessions.is_empty(),
            "no refinement session should be in-flight after deferral"
        );
        assert!(
            actor.inflight_dispatches.is_empty(),
            "no inflight dispatch ledger entry should exist after deferral"
        );
        assert!(
            actor.provisional_admissions.is_empty(),
            "no provisional admission should remain after deferral"
        );

        // ── Ticks 2–4: repeated at-cap ticks must be pure no-ops ──
        for tick in 2..=4 {
            actor.drive_active_refinements().await;

            assert!(
                actor.active_refinements.contains_key(&fixture.proposal_id),
                "tick {tick}: refinement still active"
            );
            let state = &actor.active_refinements[&fixture.proposal_id];
            assert_eq!(
                state.total_spawns, 0,
                "tick {tick}: no spawn budget consumed"
            );
            assert!(
                actor.refinement_sessions.is_empty(),
                "tick {tick}: no refinement session in-flight"
            );
            assert!(
                actor.inflight_dispatches.is_empty(),
                "tick {tick}: no inflight dispatch ledger entry"
            );
            assert!(
                actor.provisional_admissions.is_empty(),
                "tick {tick}: no provisional admission"
            );
        }

        // Verify no refinement tasks were created.
        let tasks = TaskRepository::new(db.clone(), EventBus::noop())
            .list_by_project(&fixture.project_id)
            .await
            .expect("list tasks");
        let refinement_tasks: Vec<_> = tasks
            .iter()
            .filter(|t| t.issue_type == "refinement")
            .collect();
        assert!(
            refinement_tasks.is_empty(),
            "no refinement tasks should have been created during at-cap deferrals"
        );

        // Clean up: release the blocking session.
        SessionRepository::new(db.clone(), EventBus::noop())
            .update(
                &_session_id,
                djinn_core::models::SessionStatus::Completed,
                0,
                0,
                0,
                0,
                None,
            )
            .await
            .expect("complete blocking session");
    }

    // ── AC#3: Capacity-free retry ────────────────────────────────────────

    /// Regression: once capacity becomes available after one or more
    /// deferrals, the phase creates and dispatches exactly one refinement
    /// task and continues through the existing state machine.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn capacity_free_retry_dispatches_exactly_one_refinement() {
        let db = crate::test_helpers::create_test_db();
        let fixture = seed_refinement_fixture(&db).await;
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
        let pool = spawn_test_pool(&db, 4);

        // Seed one running session + cap = 1 so the first tick defers.
        let blocking_task_id =
            seed_task(&db, &fixture.project_id, &fixture.user_id, "blocking").await;
        let session_id =
            materialize_running_session(&db, &fixture.project_id, &blocking_task_id, TEST_MODEL)
                .await;
        set_user_cap(&db, &fixture.user_id, TEST_MODEL, 1).await;

        let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
        seed_refinement_state(
            &mut actor,
            &fixture.proposal_id,
            Some(fixture.user_id.clone()),
        );

        // Tick 1: defer (at cap).
        actor.drive_active_refinements().await;
        assert!(actor.refinement_sessions.is_empty(), "deferred on tick 1");

        // Free the capacity by completing the blocking session.
        SessionRepository::new(db.clone(), EventBus::noop())
            .update(
                &session_id,
                djinn_core::models::SessionStatus::Completed,
                0,
                0,
                0,
                0,
                None,
            )
            .await
            .expect("complete blocking session");

        // Tick 2: should now dispatch exactly one refinement task.
        actor.drive_active_refinements().await;

        assert!(
            actor.refinement_sessions.contains_key(&fixture.proposal_id),
            "refinement session should be in-flight after capacity-freed dispatch"
        );
        let session = &actor.refinement_sessions[&fixture.proposal_id];
        assert_eq!(
            session.phase,
            super::super::refinement::RefinementPhase::AdversaryAttack,
            "first phase is AdversaryAttack"
        );
        assert_eq!(
            session.model_id, TEST_MODEL,
            "dispatched with the test model"
        );

        // Verify exactly one refinement task was created.
        let tasks = TaskRepository::new(db.clone(), EventBus::noop())
            .list_by_project(&fixture.project_id)
            .await
            .expect("list tasks");
        let refinement_tasks: Vec<_> = tasks
            .iter()
            .filter(|t| t.issue_type == "refinement")
            .collect();
        assert_eq!(
            refinement_tasks.len(),
            1,
            "exactly one refinement task should exist"
        );

        // The state machine should have recorded one spawn.
        let state = &actor.active_refinements[&fixture.proposal_id];
        assert_eq!(state.total_spawns, 1, "one spawn recorded");

        // The in-flight ledger should contain the dispatched task.
        assert!(
            actor.inflight_dispatches.contains_key(&session.task_id),
            "in-flight ledger must track the dispatched refinement task"
        );

        // Provisional admission must have been cleared (re-keyed to inflight).
        assert!(
            actor.provisional_admissions.is_empty(),
            "provisional admission re-keyed to inflight"
        );
    }

    // ── AC#4: Unresolved attribution fails closed ────────────────────────

    /// Regression: missing/dangling/unresolvable attribution fails closed
    /// with no refinement task row, no spawn-budget consumption, and no
    /// pool.dispatch().
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn unresolved_attribution_fails_closed() {
        let db = crate::test_helpers::create_test_db();
        let fixture = seed_refinement_fixture(&db).await;
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);
        let pool = spawn_test_pool(&db, 4);

        let mut actor = build_refinement_actor(&db, &events_tx, pool);

        // Case A: no attributed_user_id and proposal has no author.
        // Create a proposal with no author_user_id (outside any user scope).
        let orphan_proposal = ProposalRepository::new(db.clone(), EventBus::noop())
            .create(ProposalCreateInput {
                title: "Orphan proposal",
                body: "No author.",
                acceptance_criteria: Some("[]"),
                status: Some("building"),
                body_format: None,
            })
            .await
            .expect("create orphan proposal");
        ProposalRepository::new(db.clone(), EventBus::noop())
            .add_target(&orphan_proposal.id, &fixture.project_id, "primary")
            .await
            .expect("add target to orphan proposal");

        // Seed a refinement loop with no explicit attributed user.
        let orphan_state =
            super::super::refinement::RefinementLoopState::new(&orphan_proposal.id, 1);
        actor
            .active_refinements
            .insert(orphan_proposal.id.clone(), orphan_state);

        actor.drive_active_refinements().await;

        // `drive_active_refinements` retains only non-complete entries, so a
        // terminated refinement is removed from the map entirely.
        assert!(
            !actor.active_refinements.contains_key(&orphan_proposal.id),
            "orphan proposal refinement must be removed after fail-closed termination"
        );
        assert!(
            actor.refinement_sessions.is_empty(),
            "no refinement session created for unresolved attribution"
        );
        assert!(
            actor.inflight_dispatches.is_empty(),
            "no inflight dispatch for unresolved attribution"
        );
        assert!(
            actor.provisional_admissions.is_empty(),
            "no provisional admission for unresolved attribution"
        );

        // Verify no refinement task was created for the orphan proposal.
        let all_tasks = TaskRepository::new(db.clone(), EventBus::noop())
            .list_by_project(&fixture.project_id)
            .await
            .expect("list tasks");
        let orphan_refinement_tasks: Vec<_> = all_tasks
            .iter()
            .filter(|t| t.issue_type == "refinement" && t.title.contains("Orphan"))
            .collect();
        assert!(
            orphan_refinement_tasks.is_empty(),
            "no refinement task for unresolved attribution"
        );

        // Case B: dangling user id that doesn't exist in the users table.
        let dangling_proposal = ProposalRepository::new(db.clone(), EventBus::noop())
            .create(ProposalCreateInput {
                title: "Dangling attribution proposal",
                body: "Attributed to non-existent user.",
                acceptance_criteria: Some("[]"),
                status: Some("building"),
                body_format: None,
            })
            .await
            .expect("create dangling proposal");
        ProposalRepository::new(db.clone(), EventBus::noop())
            .add_target(&dangling_proposal.id, &fixture.project_id, "primary")
            .await
            .expect("add target to dangling proposal");

        let dangling_state =
            super::super::refinement::RefinementLoopState::new(&dangling_proposal.id, 1)
                .with_attributed_user(Some("non-existent-user-id-000".to_owned()));
        actor
            .active_refinements
            .insert(dangling_proposal.id.clone(), dangling_state);

        actor.drive_active_refinements().await;

        assert!(
            !actor.active_refinements.contains_key(&dangling_proposal.id),
            "dangling attribution refinement must be removed after fail-closed termination"
        );
        assert!(
            actor.refinement_sessions.is_empty(),
            "no refinement session for dangling attribution"
        );
        assert!(
            actor.inflight_dispatches.is_empty(),
            "no inflight dispatch for dangling attribution"
        );
    }

    // ── AC#5: Same-tick session-row lag ──────────────────────────────────

    /// Regression: with DB active-session count cold but one admitted
    /// in-flight refinement for `(user, model)`, the shared ledger overlay
    /// makes the effective count reflect that admission. Both a second
    /// refinement and a normal task for the same `(user, model)` defer
    /// when admitting them would exceed `max_sessions`.
    ///
    /// This test exercises the pure admission primitives
    /// (`overlay_inflight_ledger` + `model_under_user_cap`) directly so
    /// the assertion is not affected by the pool-reconciliation step that
    /// `check_user_model_admission` performs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_tick_session_row_lag_defers_second_admission() {
        use crate::dispatch::{model_under_user_cap, overlay_inflight_ledger};

        // Scenario: DB active-session count is 0 (cold — no session row yet),
        // but one in-flight refinement has been admitted for (user, model).
        // Cap is 1.  The overlay must reflect the in-flight entry so that a
        // second candidate (refinement OR normal task) sees effective count
        // = 1 and defers.
        let user = "test-user";
        let model = TEST_MODEL;
        let cap: u32 = 1;

        let mut running: HashMap<(String, String), u32> = HashMap::new();
        // DB seed: no active sessions (cold).
        assert!(running.is_empty(), "precondition: cold DB");

        // In-flight ledger: one admitted refinement for (user, model).
        let inflight: HashMap<String, (Option<String>, String)> = HashMap::from([(
            "refinement-task-1".to_string(),
            (Some(user.to_string()), model.to_string()),
        )]);

        overlay_inflight_ledger(&mut running, &inflight);

        assert_eq!(
            running.get(&(user.to_string(), model.to_string())).copied(),
            Some(1),
            "overlay must reflect the in-flight refinement"
        );
        assert!(
            !model_under_user_cap(&running, user, model, cap),
            "second admission must be blocked: effective count 1 >= cap 1"
        );

        // With cap = 2, one slot remains.
        assert!(
            model_under_user_cap(&running, user, model, 2),
            "cap=2 allows one more admission with effective count 1"
        );

        // Adding a second inflight entry fills the cap=2 slot too.
        let mut inflight2 = inflight.clone();
        inflight2.insert(
            "normal-task-1".to_string(),
            (Some(user.to_string()), model.to_string()),
        );
        let mut running2: HashMap<(String, String), u32> = HashMap::new();
        overlay_inflight_ledger(&mut running2, &inflight2);

        assert_eq!(
            running2
                .get(&(user.to_string(), model.to_string()))
                .copied(),
            Some(2),
            "overlay with two in-flight entries shows effective count 2"
        );
        assert!(
            !model_under_user_cap(&running2, user, model, 2),
            "cap=2 blocks admission when two in-flight entries exist"
        );
    }

    // ── AC#6: Dispatch/setup failure clears in-flight reservation ────────

    /// Regression: when pool.dispatch() fails after a refinement task has
    /// been created and an in-flight reservation recorded, the reservation
    /// is cleared and the refinement is terminated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_failure_clears_refinement_inflight_entry() {
        let db = crate::test_helpers::create_test_db();
        let fixture = seed_refinement_fixture(&db).await;
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(256);

        // Create a pool, then cancel it and wait for the actor to exit so
        // dispatch() returns PoolError::ActorDead.
        let cancel_token = CancellationToken::new();
        let pool = djinn_slot::SlotPoolHandle::spawn(
            crate::test_helpers::agent_context_from_db(db.clone(), cancel_token.clone()),
            cancel_token.clone(),
            djinn_slot::SlotPoolConfig {
                models: vec![djinn_slot::ModelSlotConfig {
                    model_id: TEST_MODEL.to_owned(),
                    max_slots: 1,
                    roles: ["advocate", "adversary", "judge", "worker"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                }],
                role_priorities: HashMap::new(),
            },
        );
        // Cancel and wait for the pool actor to fully exit.
        cancel_token.cancel();
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut actor = build_refinement_actor(&db, &events_tx, pool.clone());
        seed_refinement_state(
            &mut actor,
            &fixture.proposal_id,
            Some(fixture.user_id.clone()),
        );

        // Dispatch should attempt task creation + pool.dispatch(), fail on
        // pool dispatch, and clear the in-flight reservation.
        actor.drive_active_refinements().await;

        // `drive_active_refinements` removes completed entries from the map.
        assert!(
            !actor.active_refinements.contains_key(&fixture.proposal_id),
            "refinement must be removed after dispatch-failure termination"
        );

        // The in-flight dispatch ledger must be clean.
        assert!(
            actor.inflight_dispatches.is_empty(),
            "inflight dispatch ledger must be cleared after pool dispatch failure: {:?}",
            actor.inflight_dispatches,
        );

        // Provisional admissions must be clean.
        assert!(
            actor.provisional_admissions.is_empty(),
            "provisional admissions must be empty after dispatch failure"
        );

        // The refinement session must have been removed.
        assert!(
            actor.refinement_sessions.is_empty(),
            "refinement session must be removed after dispatch failure"
        );

        // A refinement task WAS created (pool dispatch is after task creation),
        // but no inflight ledger entry should remain for it.
        let tasks = TaskRepository::new(db.clone(), EventBus::noop())
            .list_by_project(&fixture.project_id)
            .await
            .expect("list tasks");
        let refinement_tasks: Vec<_> = tasks
            .iter()
            .filter(|t| t.issue_type == "refinement")
            .collect();
        assert_eq!(
            refinement_tasks.len(),
            1,
            "one refinement task should have been created before the pool dispatch failure"
        );
        let created_task_id = &refinement_tasks[0].id;
        assert!(
            !actor.inflight_dispatches.contains_key(created_task_id),
            "inflight ledger must NOT contain the failed refinement task"
        );
    }

    // ── Shared ledger overlay: normal dispatch and refinement share ledger ─

    /// Regression: the in-flight ledger is shared between normal task
    /// dispatch and refinement dispatch. A just-admitted refinement
    /// reservation reduces capacity visible to the shared admission
    /// primitives for any dispatch path (refinement or normal task).
    ///
    /// This test exercises `overlay_inflight_ledger` and
    /// `model_under_user_cap` directly with mixed entry kinds to prove
    /// the shared ledger contract.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refinement_and_normal_dispatch_share_inflight_ledger() {
        use crate::dispatch::{model_under_user_cap, overlay_inflight_ledger};

        let user = "shared-ledger-user";
        let model = TEST_MODEL;

        // Cap = 2. Two different dispatch sources: one normal task, one
        // refinement. They share the same (user, model) ledger.
        let inflight: HashMap<String, (Option<String>, String)> = HashMap::from([
            (
                "normal-task-1".to_string(),
                (Some(user.to_string()), model.to_string()),
            ),
            (
                "refinement-task-1".to_string(),
                (Some(user.to_string()), model.to_string()),
            ),
        ]);

        let mut running: HashMap<(String, String), u32> = HashMap::new();
        overlay_inflight_ledger(&mut running, &inflight);

        assert_eq!(
            running.get(&(user.to_string(), model.to_string())).copied(),
            Some(2),
            "both entries count toward effective running"
        );
        assert!(
            !model_under_user_cap(&running, user, model, 2),
            "cap=2 with two in-flight entries blocks a third admission"
        );

        // Removing one entry (e.g. normal task completes) frees a slot.
        let inflight_one: HashMap<String, (Option<String>, String)> = HashMap::from([(
            "refinement-task-1".to_string(),
            (Some(user.to_string()), model.to_string()),
        )]);
        let mut running_one: HashMap<(String, String), u32> = HashMap::new();
        overlay_inflight_ledger(&mut running_one, &inflight_one);

        assert_eq!(
            running_one
                .get(&(user.to_string(), model.to_string()))
                .copied(),
            Some(1),
            "one entry shows effective count 1"
        );
        assert!(
            model_under_user_cap(&running_one, user, model, 2),
            "cap=2 with one in-flight entry allows one more admission"
        );

        // Max semantics: if DB already shows count=2 for (user, model),
        // the overlay takes max(db, ledger) — a ledger count of 1 does
        // NOT reduce the effective count.
        let mut running_with_db = HashMap::from([((user.to_string(), model.to_string()), 2u32)]);
        overlay_inflight_ledger(&mut running_with_db, &inflight_one);
        assert_eq!(
            running_with_db
                .get(&(user.to_string(), model.to_string()))
                .copied(),
            Some(2),
            "max(db=2, ledger=1) = 2"
        );
    }
}
