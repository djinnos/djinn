use super::super::*;
use djinn_core::models::TransitionAction;
use tracing::Instrument as _;

/// A `running`, zero-token session older than this has slipped past the
/// 180s fast-path stall breaker — its in-memory tracking has drifted. Reap it
/// on DB truth alone.
pub(in crate::actors::coordinator) const ZOMBIE_HARD_CAP_SECS: u64 = 10 * 60;

impl CoordinatorActor {
    async fn teardown_zombie_taskrun_job(
        &self,
        task_id: &str,
        session_id: &str,
        task_run_id: Option<&str>,
    ) {
        let Some(task_run_id) = task_run_id.map(str::trim).filter(|id| !id.is_empty()) else {
            return;
        };
        let Some(runtime_ops) = self.runtime_ops.as_ref() else {
            return;
        };

        if let Err(e) = runtime_ops.teardown_taskrun_job(task_run_id).await {
            tracing::warn!(
                task_id = %task_id,
                session_id = %session_id,
                task_run_id = %task_run_id,
                error = %e,
                "CoordinatorActor: task-run Job teardown failed during zombie reap (continuing DB recovery)"
            );
        }
    }

    #[tracing::instrument(
        name = "djinn.session_recovery.stall_timeout",
        skip(self),
        fields(kind = "stall")
    )]
    pub(in crate::actors::coordinator) async fn enforce_session_stall_timeout(&mut self) {
        let repo = djinn_db::SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let active = match repo.list_active().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: failed to list active sessions for stall timeout");
                return;
            }
        };

        /// Default stall timeout: 30 minutes. Real worker sessions spend long
        /// stretches in LLM reasoning or `cargo build`/`cargo test` with no
        /// activity-log entries; the previous 5-minute cap killed worker pods
        /// mid-edit and destroyed their ephemeral workspaces before they could
        /// commit (no PR ever opens).
        const STALL_TIMEOUT_SECS: u64 = 30 * 60;
        /// Architect sessions kept on the same 30-minute budget —
        /// reviews are similarly read-heavy and don't need a shorter clock.
        const ARCHITECT_STALL_TIMEOUT_SECS: u64 = 30 * 60;

        // Prune `stall_killed` entries for sessions that have finished cleaning
        // up. This set is keyed by SESSION id, not task id: keying by task id
        // let a stale entry from a just-killed session permanently mask the
        // task's NEXT session, because the kill→finalize→redispatch sequence
        // can complete within a single tick (orphan recovery + dispatch run
        // back-to-back) so there is never a tick where the task has zero
        // running sessions for the task-keyed prune to observe. Per-session
        // keying means a brand-new session row is always re-evaluated.
        let active_session_ids: HashSet<String> = active.iter().map(|s| s.id.clone()).collect();
        self.stall_killed
            .retain(|id| active_session_ids.contains(id));

        /// First-call short-circuit: a session that has never shown a sign of
        /// life (the host's `ActivityTracker` has no entry — see
        /// `RunningTaskInfo::activity_tracked`) after this many seconds has its
        /// very first LLM call hung. Applied to every role, ahead of the general
        /// idle-based threshold which protects long worker turns.
        ///
        /// Sized to clear a *reasoning* model's first turn on a large context:
        /// the ChatGPT Codex / OpenAI responses backend can stream nothing for a
        /// minute-plus while it reasons before the first output token, and that
        /// is NOT a hang. 180s was too tight and false-killed those first turns
        /// (then tripped the breaker → failover storm); 300s clears them while
        /// still catching a genuine hang well under the 10-minute zombie cap.
        const FIRST_CALL_STALL_SECS: u64 = 300;

        for session in active {
            let Some(task_id) = session.task_id.as_deref() else {
                continue;
            };

            // Skip this exact session if we've already killed it — its DB
            // record stays `running` until the async lifecycle cleanup
            // finishes. Keyed by session id so a redispatched successor for
            // the same task is never masked.
            if self.stall_killed.contains(&session.id) {
                continue;
            }

            // Use role-specific stall timeout: Architect gets 10 minutes.
            let stall_threshold = if session.agent_type == "architect" {
                ARCHITECT_STALL_TIMEOUT_SECS
            } else {
                STALL_TIMEOUT_SECS
            };

            // Query the activity tracker for idle time.  If the task has no
            // activity entry (e.g. session predates this feature, or reply loop
            // never started) fall back to wall-clock elapsed from started_at.
            let (idle, activity_tracked, token_count, turn_count) = match self
                .pool
                .session_for_task(task_id)
                .await
            {
                Ok(Some(info)) => (
                    info.idle_seconds,
                    info.activity_tracked,
                    info.token_count,
                    info.turn_count,
                ),
                _ => {
                    // Fallback: parse ISO-8601 started_at from the DB and compute
                    // elapsed seconds.  The column stores datetime strings like
                    // "2026-03-27 13:52:47" or "2026-03-27T13:52:47.231Z".
                    let Some(elapsed) = parse_iso_elapsed(&session.started_at) else {
                        tracing::warn!(
                            task_id = %task_id,
                            started_at = %session.started_at,
                            "CoordinatorActor: cannot parse started_at for stall check, skipping"
                        );
                        continue;
                    };
                    // No host activity entry → still on the first LLM call.
                    (elapsed, false, 0, 0)
                }
            };

            // Pick the threshold that fires first. A session that has never
            // shown a sign of life (no host ActivityTracker entry) is
            // wedged-on-first-call and gets the aggressive cap regardless of
            // role; one that has shown activity at least once falls under the
            // role's full idle budget — covering long quiet stretches (a
            // multi-minute build, a long reasoning turn between tool calls)
            // that legitimately don't touch activity until they finish.
            //
            // NOTE: this MUST come from in-memory liveness, not the session row.
            // `sessions.tokens_in/out` are written only at session *end*, so a
            // running row reads `0/0` for its whole life — keying the cap on it
            // (the old `zero_tokens` check) put EVERY in-flight session on the
            // aggressive cap and killed productive workers mid-flow.
            let never_active = !activity_tracked;
            let applied_threshold = if never_active {
                stall_threshold.min(FIRST_CALL_STALL_SECS)
            } else {
                stall_threshold
            };

            // ── Per-session token / turn ceiling check (runaway guard) ──
            // These are session-ownership guards, NOT provider-health evidence.
            // When tripped we kill the session and route the task through the
            // loop-guard planner intervention path — without feeding the model
            // circuit breaker.
            let ceiling_reason = if token_count > SESSION_TOKEN_CEILING {
                Some(format!(
                    "token ceiling exceeded ({} > {})",
                    token_count, SESSION_TOKEN_CEILING
                ))
            } else if turn_count > SESSION_TURN_CEILING {
                Some(format!(
                    "turn ceiling exceeded ({} > {})",
                    turn_count, SESSION_TURN_CEILING
                ))
            } else {
                None
            };

            if let Some(reason) = ceiling_reason {
                let kill_task_id = task_id.to_owned();
                let kill_session_id = session.id.clone();
                let kill_span = tracing::info_span!(
                    "djinn.session_recovery.kill_session",
                    kind = "ceiling",
                    task_id = %kill_task_id,
                    session_id = %kill_session_id
                );
                let kill_result = async {
                    let result = self.pool.kill_session(&kill_task_id).await;
                    let outcome = match &result {
                        Ok(()) => "ok",
                        Err(PoolError::TaskNotFound { .. }) => "not_found",
                        Err(_) => "error",
                    };
                    tracing::info!(
                        kind = "ceiling",
                        task_id = %kill_task_id,
                        session_id = %kill_session_id,
                        outcome,
                        "CoordinatorActor: session recovery kill_session attempt (ceiling)"
                    );
                    result
                }
                .instrument(kill_span)
                .await;
                if let Err(e) = kill_result {
                    tracing::warn!(
                        task_id = %task_id,
                        session_id = %session.id,
                        error = %e,
                        "CoordinatorActor: failed to kill ceiling-tripped session"
                    );
                    continue;
                }

                // Mark this session as killed so we don't re-kill on subsequent ticks.
                self.stall_killed.insert(session.id.clone());

                // Log an actionable coordinator comment.
                let payload = serde_json::json!({
                    "message": format!(
                        "Coordinator session ceiling: {} session {} — {}. Session was cancelled and task routed to Planner intervention.",
                        session.agent_type, session.id, reason
                    )
                })
                .to_string();
                let task_repo = self.task_repo();
                let _ = task_repo
                    .log_activity(Some(task_id), "coordinator", "system", "comment", &payload)
                    .await;

                // Route through loop-guard planner intervention (NOT a stall kill).
                let intervention_reason = format!(
                    "Session exceeded {} ceiling ({}). Routing to Planner intervention to decide how to unstick the task.",
                    if token_count > SESSION_TOKEN_CEILING {
                        "token"
                    } else {
                        "turn"
                    },
                    reason
                );
                self.route_loop_guard_planner_intervention(
                    task_id,
                    "coordinator",
                    &intervention_reason,
                )
                .await;

                tracing::warn!(
                    task_id = %task_id,
                    session_id = %session.id,
                    agent_type = %session.agent_type,
                    token_count,
                    turn_count,
                    "CoordinatorActor: killed ceiling-tripped session"
                );
                continue;
            }

            if idle <= applied_threshold {
                continue;
            }

            let kill_task_id = task_id.to_owned();
            let kill_session_id = session.id.clone();
            let kill_span = tracing::info_span!(
                "djinn.session_recovery.kill_session",
                kind = "stall",
                task_id = %kill_task_id,
                session_id = %kill_session_id
            );
            let kill_result = async {
                let result = self.pool.kill_session(&kill_task_id).await;
                let outcome = match &result {
                    Ok(()) => "ok",
                    Err(PoolError::TaskNotFound { .. }) => "not_found",
                    Err(_) => "error",
                };
                tracing::info!(
                    kind = "stall",
                    task_id = %kill_task_id,
                    session_id = %kill_session_id,
                    outcome,
                    "CoordinatorActor: session recovery kill_session attempt"
                );
                result
            }
            .instrument(kill_span)
            .await;
            if let Err(e) = kill_result {
                tracing::warn!(task_id = %task_id, session_id = %session.id, error = %e, "CoordinatorActor: failed to kill stalled session");
                continue;
            }

            // Mark this session as killed so we don't re-kill and re-log on
            // subsequent ticks while its DB row drains.
            self.stall_killed.insert(session.id.clone());

            // Resolve the breaker scope: health is keyed per owning user so this
            // trip only demotes the model for THIS task's creator, not globally.
            // The throttle that produces first-call hangs is per-credential
            // (most acutely the per-account ChatGPT Codex backend), so a global
            // trip would disable the model for everyone on one user's bad luck.
            let task_repo = self.task_repo();
            let scope = task_repo
                .get(task_id)
                .await
                .ok()
                .flatten()
                .and_then(|t| t.created_by_user_id);

            // Feed the model circuit-breaker so dispatch fails over to the next
            // model in the creator's ordered list on redispatch. A first-call
            // hang (no sign of life at all) is a strong "this model/backend is
            // bad right now" signal → trip immediately with a long cooldown that
            // outlasts the task's escalating redispatch cooldown (`record_stall`).
            // A plain idle stall is a weaker signal (a genuinely long worker turn
            // that went quiet), so we feed the gentler consecutive-failure
            // breaker (`record_failure`) which only trips after repeats — this
            // avoids needlessly demoting the user's preferred model on a single
            // idle blip. Either way the cooldown auto-expires (self-heal) and a
            // recovered model is reset by `record_success` on a real run.
            // `self.health` is the same HealthTracker instance the dispatch
            // `is_available` gate consults (cloned into slots), so this trip is
            // visible to the very next dispatch pass.
            if never_active {
                // A first-call hang is a genuine model/backend-health signal
                // (not a quota throttle), so escalate the cooldown cap.
                self.health
                    .record_stall(scope.as_deref(), &session.model_id, true);
            } else {
                self.health
                    .record_failure(scope.as_deref(), &session.model_id);
            }

            let reason = if never_active {
                "first-call hung (no activity)"
            } else {
                "idle"
            };
            let payload = serde_json::json!({
                "message": format!(
                    "Coordinator stall timeout: {} session {} for {}s (threshold {}s, {}). Session was cancelled for redispatch.",
                    session.agent_type, if never_active { "stuck" } else { "idle" }, idle, applied_threshold, reason
                )
            })
            .to_string();
            let _ = task_repo
                .log_activity(Some(task_id), "coordinator", "system", "comment", &payload)
                .await;

            tracing::warn!(
                task_id = %task_id,
                session_id = %session.id,
                agent_type = %session.agent_type,
                idle_seconds = idle,
                threshold_secs = applied_threshold,
                never_active,
                scope = ?scope,
                "CoordinatorActor: killed stalled session"
            );
        }
    }

    /// DB-truth backstop beneath the in-memory stall breaker and orphan
    /// reconciler. Both of those gate on in-memory coordinator state that can
    /// silently drift from the source of truth (the session DB row): the stall
    /// breaker on `stall_killed`, the orphan reconciler on `pool.has_session`
    /// (the in-memory `task_to_slot` map). When that state drifts — a leaked
    /// slot whose `Killed` event never arrived, a pod evicted/OOM-killed before
    /// it produced a token, a server restart between session create and slot
    /// registration — a session can sit `running` with zero tokens forever and
    /// be reaped by *neither* mechanism, wedging its task in an execution state
    /// indefinitely.
    ///
    /// This sweep ignores all in-memory state and acts on DB truth alone: any
    /// non-chat session that has been `running` with zero token progress for
    /// longer than every fast-path threshold is finalized, its (likely leaked)
    /// slot forcibly reclaimed, and its task released for redispatch. The hard
    /// cap sits well above the 180s zero-token stall threshold so the fast path
    /// always wins in the normal case; this only fires for genuine drift.
    #[tracing::instrument(
        name = "djinn.session_recovery.zombie_reap",
        skip(self),
        fields(kind = "periodic")
    )]
    pub(in crate::actors::coordinator) async fn reap_zombie_sessions(&mut self) {
        let session_repo = djinn_db::SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let active = match session_repo.list_active().await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: failed to list active sessions for zombie reap");
                return;
            }
        };
        let task_repo = self.task_repo();

        for session in active {
            let Some(task_id) = session.task_id.as_deref() else {
                continue;
            };
            // Chat sessions have their own idle reaper and legitimately sit at
            // zero tokens between turns.
            if session.agent_type == "chat" {
                continue;
            }

            // Load the owning task BEFORE the token-nonzero skip so we can
            // bypass that skip when DB truth says the session is an orphan.
            // Terminal/parked/reset tasks have no valid owner — their sessions
            // should be reaped regardless of accumulated tokens.
            let task_row = task_repo.get(task_id).await;
            let cached_task = match &task_row {
                Ok(Some(t)) => Some(t.clone()),
                _ => None,
            };
            let status_overrides_token_skip = match cached_task.as_ref() {
                Some(task) => {
                    match task.status.as_str() {
                        // Terminal or parked — session is orphaned regardless of tokens.
                        "force_closed" | "closed" | "parked_permanently" | "parked_for_review" => {
                            true
                        }
                        // Task was reset to `open` while the session was still
                        // running — the session predates the reset.
                        "open" => {
                            session_predates_task_status(&session.started_at, &task.updated_at)
                                .unwrap_or(false)
                        }
                        // In-progress / in-task-review / in-lead-intervention:
                        // healthy long-running session, preserve the skip.
                        _ => false,
                    }
                }
                // Task not found — treat as orphaned (no valid owner).
                None if matches!(task_row, Ok(None)) => true,
                // Lookup error — skip this session (avoid acting on bad data).
                _ => continue,
            };

            if !status_overrides_token_skip && (session.tokens_in != 0 || session.tokens_out != 0) {
                continue;
            }
            let Some(age) = parse_iso_elapsed(&session.started_at) else {
                continue;
            };
            if age <= ZOMBIE_HARD_CAP_SECS {
                continue;
            }

            // Ground-truth liveness gate: if the worker still holds a live RPC
            // connection for this run, it is alive — just long or quiet — so
            // never reap it. This is authoritative where the heuristics below
            // are not: session-row tokens read `0/0` until session *end*, and
            // for remote K8s workers the host-side slot/activity bookkeeping
            // (`task_to_slot` / `active_tasks`) can drift out of sync with the
            // live pod, making the activity gate below false-negative and
            // reaping a productive worker (it then restarts from scratch and
            // never converges). A genuinely dead worker — crashed, OOM-killed,
            // or never scheduled — has no attached connection slot and falls
            // through to the reap. `None` registry (off-server/tests) skips
            // this and uses the activity heuristic alone.
            if let (Some(registry), Some(run_id)) =
                (self.rpc_registry.as_ref(), session.task_run_id.as_deref())
                && registry.is_connected(run_id).await
            {
                continue;
            }

            // Liveness gate (leak-safe): a worker that has touched activity
            // within the hard cap is alive and productive — its DB row reads
            // `0/0` only because `sessions.tokens_in/out` are flushed at session
            // *end*, not per turn. Without this gate the token check above is
            // inert (every running row is `0/0`) and the reaper kills EVERY
            // non-chat session that simply runs longer than 10 minutes. We read
            // idle from the host `ActivityTracker` (bridged from the worker's
            // `touch_activity` RPC), which keeps climbing once a pod dies even if
            // its slot mapping leaks — so reaping still fires for a genuine
            // zombie, but a long, productive run is left alone.
            if let Ok(Some(info)) = self.pool.session_for_task(task_id).await
                && info.activity_tracked
                && info.idle_seconds <= ZOMBIE_HARD_CAP_SECS
            {
                continue;
            }

            tracing::warn!(
                task_id = %task_id,
                session_id = %session.id,
                agent_type = %session.agent_type,
                age_seconds = age,
                model_id = %session.model_id,
                status_overrides_token_skip,
                "CoordinatorActor: reaping zombie session (no live worker, past hard cap)"
            );

            let token_info = if session.tokens_in != 0 || session.tokens_out != 0 {
                format!(
                    "{} tokens (in={}, out={})",
                    session.tokens_in + session.tokens_out,
                    session.tokens_in,
                    session.tokens_out
                )
            } else {
                "zero tokens".to_string()
            };
            let status_note = if status_overrides_token_skip {
                " Task status indicates orphan (terminal/parked/reset)."
            } else {
                ""
            };
            let payload = serde_json::json!({
                "message": format!(
                    "Coordinator zombie-session backstop: {} session was `running` with {} for {}s (hard cap {}s) with no live worker.{} Session finalized and task released for redispatch.",
                    session.agent_type, token_info, age, ZOMBIE_HARD_CAP_SECS, status_note
                )
            })
            .to_string();
            let _ = task_repo
                .log_activity(Some(task_id), "coordinator", "system", "comment", &payload)
                .await;

            // Deliberately does NOT feed the model circuit-breaker. This
            // DB-truth backstop fires on infra/drift conditions — a Pod that
            // never scheduled (node capacity), an OOM/crash before the first
            // heartbeat, a leaked slot, or a tool hung past the fast path — none
            // of which are evidence the MODEL is bad. Tripping the breaker here
            // misattributed capacity/tool failures to the provider and
            // auto-disabled the (often only) model for the whole scope, turning
            // a transient capacity pinch into a full dispatch outage (every task
            // for that user deferred with "no eligible model"). Genuine provider
            // stalls/errors are still caught where they belong: the fast-path
            // stall-kill (`detect_and_handle_stalls`) and the supervisor's typed
            // ProviderError path (Throttle/Failure/AuthInvalid) both feed the
            // breaker on real model evidence.

            // Forcibly reclaim the (likely leaked) slot so the reopened task is
            // not rejected with `SessionAlreadyActive` on redispatch.
            self.teardown_zombie_taskrun_job(task_id, &session.id, session.task_run_id.as_deref())
                .await;
            let evict_task_id = task_id.to_owned();
            let evict_session_id = session.id.clone();
            let evict_span = tracing::info_span!(
                "djinn.session_recovery.zombie_reap.evict_session",
                kind = "periodic",
                task_id = %evict_task_id,
                session_id = %evict_session_id
            );
            let evict_result = async {
                let result = self.pool.evict_session(&evict_task_id).await;
                let outcome = match &result {
                    Ok(()) => "ok",
                    Err(PoolError::TaskNotFound { .. }) => "not_found",
                    Err(_) => "error",
                };
                tracing::info!(
                    kind = "periodic",
                    task_id = %evict_task_id,
                    session_id = %evict_session_id,
                    outcome,
                    "CoordinatorActor: zombie session evict attempt"
                );
                result
            }
            .instrument(evict_span)
            .await;
            if let Err(e) = evict_result {
                tracing::warn!(task_id = %task_id, error = %e, "CoordinatorActor: failed to evict slot for zombie session");
            }
            // Drop the stale stall guard for this finalized session.
            self.stall_killed.remove(&session.id);

            // Finalize the orphaned `running` row so it stops being listed.
            let finalize_task_id = task_id.to_owned();
            let finalize_session_id = session.id.clone();
            let finalize_span = tracing::info_span!(
                "djinn.session_recovery.zombie_reap.finalize_session",
                kind = "periodic",
                task_id = %finalize_task_id,
                session_id = %finalize_session_id
            );
            let finalize_result = async {
                let result = session_repo
                    .interrupt_running_for_task(&finalize_task_id)
                    .await;
                let outcome = if result.is_ok() { "ok" } else { "error" };
                tracing::info!(
                    kind = "periodic",
                    task_id = %finalize_task_id,
                    session_id = %finalize_session_id,
                    outcome,
                    "CoordinatorActor: zombie session finalize attempt"
                );
                result
            }
            .instrument(finalize_span)
            .await;
            if let Err(e) = finalize_result {
                tracing::warn!(task_id = %task_id, error = %e, "CoordinatorActor: failed to finalize zombie session row");
            }

            djinn_telemetry::zombie::increment_reap(djinn_telemetry::zombie::KIND_STALL);

            // Release the task from its execution status so dispatch can pick
            // it up again. Mirrors the orphan reconciler's status→action map,
            // but does not depend on `has_session` (which is exactly the gate
            // that drifted).
            match task_row {
                Ok(Some(task)) => {
                    let release = match task.status.as_str() {
                        "in_progress" => Some((TransitionAction::Release, "open")),
                        "in_task_review" => {
                            Some((TransitionAction::ReleaseTaskReview, "needs_task_review"))
                        }
                        "in_lead_intervention" => Some((
                            TransitionAction::LeadInterventionRelease,
                            "needs_lead_intervention",
                        )),
                        _ => None,
                    };
                    if let Some((action, release_to)) = release
                        && let Err(e) = task_repo
                            .transition(
                                &task.id,
                                action,
                                "coordinator",
                                "system",
                                Some(
                                    "Recovered by coordinator: zombie session reaped (no live worker, past hard cap)",
                                ),
                                None,
                            )
                            .await
                    {
                        tracing::warn!(task_id = %task_id, to = release_to, error = %e, "CoordinatorActor: failed to release task after zombie reap");
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(task_id = %task_id, error = %e, "CoordinatorActor: failed to load task for zombie reap")
                }
            }
        }
    }

    /// Settle chat sessions left `running` past the idle window.
    ///
    /// Chat rows are created `running` and never transition on their own — the
    /// completions handler doesn't settle them and `interrupt_all_running` only
    /// fires at startup — so without this they linger as zombie `running` rows
    /// for the whole server lifetime (one per conversation, never closed when a
    /// browser tab is abandoned or the SSE drops). They no longer occupy a
    /// dispatch slot (`count_active_by_user_and_model` excludes chat), but
    /// reaping keeps session state honest for observability and the chat list.
    /// A settled session revives to `running` on its next turn via
    /// `upsert_chat_session`. The stall sweep above can't cover these: it keys
    /// on `task_id`, which is always NULL for chat.
    #[tracing::instrument(
        name = "djinn.session_recovery.idle_reap",
        skip(self),
        fields(kind = "idle")
    )]
    pub(in crate::actors::coordinator) async fn reap_idle_chat_sessions(&self) {
        /// Idle window before a chat session is considered settled: 30 minutes,
        /// matching the worker stall timeout.
        const CHAT_IDLE_TIMEOUT_SECS: u64 = 30 * 60;

        let repo = SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let rows = match repo.list_running_chat_with_last_activity().await {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: failed to list running chat sessions for idle reap");
                return;
            }
        };
        for (session_id, last_activity) in rows {
            let Some(idle) = parse_iso_elapsed(&last_activity) else {
                continue;
            };
            if idle <= CHAT_IDLE_TIMEOUT_SECS {
                continue;
            }
            let reap_session_id = session_id.clone();
            let reap_span = tracing::info_span!(
                "djinn.session_recovery.idle_reap.settle_session",
                kind = "idle",
                session_id = %reap_session_id
            );
            let settle_result = async {
                let result = repo.settle_idle_chat(&reap_session_id).await;
                let outcome = if result.is_ok() { "ok" } else { "error" };
                tracing::info!(
                    kind = "idle",
                    session_id = %reap_session_id,
                    outcome,
                    "CoordinatorActor: idle chat session settle attempt"
                );
                result
            }
            .instrument(reap_span)
            .await;
            if let Err(e) = settle_result {
                tracing::warn!(session_id = %session_id, error = %e, "CoordinatorActor: failed to settle idle chat session");
                continue;
            }
            tracing::info!(
                session_id = %session_id,
                idle_seconds = idle,
                "CoordinatorActor: settled idle chat session"
            );
        }
    }

    /// On each tick: find tasks in active execution states with no active session
    /// and release them back to a dispatch-ready state (AGENT-08).
    ///
    /// For slot-based statuses (in_progress, in_task_review, in_lead_intervention),
    /// we check `has_session` in the slot pool, and skip tasks with in-flight
    /// post-session background work registered in the shared
    /// `BackgroundWorkTracker` (e.g. a non-worker merge/transition still running
    /// after the slot freed its session).
    pub(in crate::actors::coordinator) async fn detect_and_recover_stuck_filtered(
        &mut self,
        project_filter: Option<&str>,
    ) {
        let repo = self.task_repo();
        let mut affected = 0u64;

        let session_repo = djinn_db::SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );

        for status in [
            "in_progress",
            "in_task_review",
            "in_lead_intervention",
            "open",
            "needs_task_review",
        ] {
            let tasks = match repo.list_by_status(status).await {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(error = %e, status, "CoordinatorActor: list_by_status failed");
                    continue;
                }
            };

            for task in tasks {
                if let Some(project_id) = project_filter
                    && task.project_id != project_id
                {
                    continue;
                }

                if matches!(task.status.as_str(), "open" | "needs_task_review") {
                    let sessions = match session_repo.list_for_task(&task.id).await {
                        Ok(sessions) => sessions,
                        Err(e) => {
                            tracing::warn!(task_id = %task.short_id, error = %e, "CoordinatorActor: failed to query sessions for ready-state orphan scan");
                            continue;
                        }
                    };
                    let Some(running_session) = sessions
                        .iter()
                        .find(|session| session.status.as_str() == "running")
                    else {
                        continue;
                    };

                    let Some(session_predates_ready_state) =
                        session_predates_task_status(&running_session.started_at, &task.updated_at)
                    else {
                        tracing::warn!(
                            task_id = %task.short_id,
                            session_id = %running_session.id,
                            session_started_at = %running_session.started_at,
                            task_updated_at = %task.updated_at,
                            "CoordinatorActor: failed to compare ready-state task/session timestamps for orphan scan"
                        );
                        continue;
                    };
                    if !session_predates_ready_state {
                        continue;
                    }

                    let has_session = match self.pool.has_session(&task.id).await {
                        Ok(b) => b,
                        Err(PoolError::ActorDead) => {
                            tracing::error!(
                                "CoordinatorActor: slot pool actor dead, aborting stuck scan"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::warn!(task_id = %task.short_id, error = %e, "CoordinatorActor: has_session query failed");
                            continue;
                        }
                    };
                    if has_session {
                        continue;
                    }

                    let has_background_work = {
                        let guard = self
                            .background_work_tracker
                            .lock()
                            .expect("background work tracker mutex poisoned");
                        guard.contains(&task.id)
                    };
                    if has_background_work {
                        continue;
                    }

                    match session_repo.interrupt_running_for_task(&task.id).await {
                        Ok(interrupted) if interrupted > 0 => {
                            tracing::warn!(
                                task_id = %task.short_id,
                                status = %task.status,
                                interrupted,
                                session_id = %running_session.id,
                                "CoordinatorActor: finalized stale ready-state running session"
                            );
                            affected += 1;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                task_id = %task.short_id,
                                error = %e,
                                "CoordinatorActor: failed to finalize stale ready-state sessions"
                            );
                        }
                    }
                    continue;
                }

                let has_session = match self.pool.has_session(&task.id).await {
                    Ok(b) => b,
                    Err(PoolError::ActorDead) => {
                        tracing::error!(
                            "CoordinatorActor: slot pool actor dead, aborting stuck scan"
                        );
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(task_id = %task.short_id, error = %e, "CoordinatorActor: has_session query failed");
                        continue;
                    }
                };

                if has_session {
                    continue;
                }

                // Non-worker roles free the slot immediately and run post-session
                // work (merge, transition) in a background task. The
                // background-work tracker covers that in-flight post-session work.
                let has_background_work = {
                    let guard = self
                        .background_work_tracker
                        .lock()
                        .expect("background work tracker mutex poisoned");
                    guard.contains(&task.id)
                };
                if has_background_work {
                    continue;
                }

                let (release_action, release_to) = match task.status.as_str() {
                    "in_task_review" => (TransitionAction::ReleaseTaskReview, "needs_task_review"),
                    "in_lead_intervention" => (
                        TransitionAction::LeadInterventionRelease,
                        "needs_lead_intervention",
                    ),
                    _ => (TransitionAction::Release, "open"),
                };

                match repo
                    .transition(
                        &task.id,
                        release_action,
                        "coordinator",
                        "system",
                        Some(&format!(
                            "Recovered by coordinator: no active slot session for {}",
                            task.status
                        )),
                        None,
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::warn!(
                            task_id = %task.short_id,
                            from = %task.status,
                            to = release_to,
                            "CoordinatorActor: recovered stuck task"
                        );
                        // Finalize any orphaned "running" session records for this
                        // task so they don't accumulate as ghost rows.
                        if let Err(e) = session_repo.interrupt_running_for_task(&task.id).await {
                            tracing::warn!(
                                task_id = %task.short_id,
                                error = %e,
                                "CoordinatorActor: failed to finalize orphaned sessions"
                            );
                        }
                        affected += 1;
                    }
                    Err(e) => {
                        tracing::warn!(task_id = %task.short_id, error = %e, "CoordinatorActor: failed to recover stuck task")
                    }
                }
            }
        }

        if affected > 0 {
            self.recovered += affected;
            self.publish_status();
            tracing::info!(
                affected,
                total_recovered = self.recovered,
                "CoordinatorActor: stuck-task recovery pass complete"
            );
        }
    }

    /// Fail a task terminally (`ForceClose`) with an actionable reason. Used
    /// when a task is structurally undispatchable (its owner has no model with a
    /// connected provider) or has failed too many consecutive times. Looping
    /// forever is worse than a clear terminal state, so closing it self-cleans
    /// that guard.
    pub(in crate::actors::coordinator::dispatch) async fn terminally_fail_task(
        &self,
        task: &djinn_core::models::Task,
        role: &str,
        reason: &str,
    ) -> bool {
        tracing::warn!(
            task_id = %task.short_id,
            role,
            status = %task.status,
            reason,
            "CoordinatorActor: failing task terminally (undispatchable / max retries)"
        );
        let repo = self.task_repo();
        match repo
            .transition(
                &task.id,
                TransitionAction::ForceClose,
                "coordinator",
                "system",
                Some(reason),
                None,
            )
            .await
        {
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(task_id = %task.short_id, error = %e, "CoordinatorActor: terminal close failed");
                return false;
            }
        }

        let session_repo = djinn_db::SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        match session_repo.interrupt_running_for_task(&task.id).await {
            Ok(interrupted) if interrupted > 0 => {
                tracing::info!(
                    task_id = %task.short_id,
                    interrupted,
                    "CoordinatorActor: interrupted running sessions after terminal task close to release capacity"
                );
            }
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "CoordinatorActor: failed to interrupt running sessions after terminal task close"
                );
            }
        }
        true
    }
}

/// Parse an ISO-8601 datetime string from the DB (e.g. "2026-03-27T13:52:47.231Z"
/// or "2026-03-27 13:52:47") and return seconds elapsed since that time.
fn parse_iso_elapsed(started_at: &str) -> Option<u64> {
    use ::time::OffsetDateTime;
    use ::time::format_description::well_known::Iso8601;

    // Try ISO-8601 with offset first, then fall back to space-separated SQLite format.
    let parsed = OffsetDateTime::parse(started_at, &Iso8601::DEFAULT)
        .ok()
        .or_else(|| {
            // SQLite often stores "YYYY-MM-DD HH:MM:SS" without offset — assume UTC.
            let fmt =
                ::time::format_description::parse("[year]-[month]-[day] [hour]:[minute]:[second]")
                    .ok()?;
            let primitive = ::time::PrimitiveDateTime::parse(started_at, &fmt).ok()?;
            Some(primitive.assume_utc())
        })?;

    let now = OffsetDateTime::now_utc();
    let elapsed = (now - parsed).whole_seconds();
    Some(if elapsed < 0 { 0 } else { elapsed as u64 })
}

fn session_predates_task_status(session_started_at: &str, task_updated_at: &str) -> Option<bool> {
    let session_elapsed = parse_iso_elapsed(session_started_at)?;
    let task_updated_elapsed = parse_iso_elapsed(task_updated_at)?;
    Some(session_elapsed > task_updated_elapsed)
}
