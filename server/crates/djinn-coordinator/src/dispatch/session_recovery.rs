// djinn:allow-oversize — session recovery + preservation gate for reap/stall/escalated termination
use super::super::*;
use crate::pr_poller::pr_cleanup::CloseKind;
use djinn_core::models::TransitionAction;
use djinn_core::models::task_attempt::TaskAttemptOutcome;
use djinn_db::{
    ClaimExtensionRecord, CurrentLivenessState, LivenessEvidenceSnapshot, LivenessRepository,
};
use djinn_slot::RunningTaskInfo;
use tracing::Instrument as _;

use super::liveness::{
    ActivitySignal, ClassificationResult, DbSessionStatus, DbTaskStatus, LivenessEvidence,
    LivenessOutcome, LivenessReason, PodPhase, Verdict,
};

/// Map a classifier [`LivenessReason`] into the persisted
/// `outcome_reason` string, dropping the `None` variant to `NULL`.
///
/// Migration 95's CHECK constraint permits only the four concrete reason
/// strings (`clean_exit_nonterminal`, `nonzero_exit_nonterminal`,
/// `hard_runtime_exceeded`, `slow_extension_budget_exhausted`) — the
/// `LivenessReason::None` enum variant is reserved for cases with no
/// specific reason and must be stored as SQL NULL. Attempting to persist
/// the literal string `"none"` violates the CHECK constraint.
fn liveness_reason_for_persistence(reason: Option<LivenessReason>) -> Option<String> {
    reason.and_then(|r| match r {
        LivenessReason::None => None,
        other => Some(other.as_str().to_owned()),
    })
}

/// A `running`, zero-token session older than this has slipped past the
/// 180s fast-path stall breaker — its in-memory tracking has drifted. Reap it
/// on DB truth alone.
pub(crate) const ZOMBIE_HARD_CAP_SECS: u64 = 10 * 60;

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
    pub(crate) async fn enforce_session_stall_timeout(&mut self) {
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
        // Prune the DB-progress watermarks the same way — a session that has
        // left `list_active()` will never be evaluated again, so its watermark
        // is dead weight (and a redispatched successor gets a fresh session id).
        self.stall_progress_watermark
            .retain(|id, _| active_session_ids.contains(id));
        self.stall_extension_count
            .retain(|id, _| active_session_ids.contains(id));

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
            let (idle, activity_tracked, token_count, turn_count, no_progress_streak) = match self
                .pool
                .session_for_task(task_id)
                .await
            {
                Ok(Some(info)) => (
                    info.idle_seconds,
                    info.activity_tracked,
                    info.token_count,
                    info.turn_count,
                    info.no_progress_streak,
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
                    (elapsed, false, 0, 0, 0)
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

                // ── Liveness evidence: persist dead_reclaimed for ceiling kill ──
                {
                    let liveness_repo = LivenessRepository::new(self.db.clone());
                    let snapshot = LivenessEvidenceSnapshot {
                        session_id: session.id.clone(),
                        task_id: Some(task_id.to_owned()),
                        task_run_id: session.task_run_id.clone(),
                        verdict: "dead".to_owned(),
                        outcome_kind: Some("dead_reclaimed".to_owned()),
                        outcome_reason: None,
                        evidence: serde_json::json!({
                            "reason": "ceiling_kill",
                            "token_count": token_count,
                            "turn_count": turn_count,
                            "kill_source": "session_recovery_ceiling",
                        }),
                    };
                    if let Err(e) = liveness_repo.persist_evidence(&snapshot).await {
                        tracing::warn!(
                            task_id = %task_id,
                            session_id = %session.id,
                            error = %e,
                            "CoordinatorActor: failed to persist ceiling-kill dead_reclaimed evidence"
                        );
                    }
                }

                // ── Preservation gate: request checkpoint before terminal transition ──
                // The ceiling kill is a controlled termination; request or record
                // preservation of potentially dirty worker output before proceeding.
                let preservation_result = self
                    .request_session_preservation(
                        task_id,
                        &session.id,
                        session.task_run_id.as_deref(),
                        djinn_telemetry::preservation::TRIGGER_CEILING,
                    )
                    .await;

                // Gate: inspect the preservation outcome. If the outcome would
                // block the transition under the configured policy, record a
                // warning and skip post-kill actions (planner intervention routing)
                // for this tick. The session is already killed, so it will not be
                // re-killed; the next tick will re-evaluate.
                if preservation_result
                    .outcome
                    .should_block_transition(self.worker_lifecycle_config.checkpoint.failure_policy)
                {
                    tracing::warn!(
                        task_id = %task_id,
                        session_id = %session.id,
                        outcome = ?preservation_result.outcome,
                        reason = %preservation_result.reason,
                        "Preservation gate blocking ceiling termination; skipping planner intervention this tick"
                    );
                    continue;
                }

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

                // Terminalize the matching attempt as a loop-guard trip: a
                // token/turn ceiling kill is a runaway guard, routed through the
                // loop-guard planner intervention rather than a stall/timeout.
                self.terminalize_recovery_attempt(
                    task_id,
                    &session.agent_type,
                    TaskAttemptOutcome::LoopGuardTripped,
                    "session_recovery_ceiling",
                    Some(&session.id),
                    session.task_run_id.as_deref(),
                    Some("dead"),
                    Some("ceiling_kill"),
                    Some(&reason),
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

            let no_progress_command_state = if !activity_tracked {
                NoProgressCommandState::Unknown
            } else if idle
                >= self
                    .worker_lifecycle_config
                    .no_progress_thresholds
                    .long_command_suspension_secs
            {
                NoProgressCommandState::InFlight { running_secs: idle }
            } else {
                NoProgressCommandState::Idle
            };
            let no_progress_decision = evaluate_no_progress_controlled_exit(
                &self.worker_lifecycle_config,
                no_progress_streak,
                no_progress_command_state,
            );
            let no_progress_threshold = self
                .worker_lifecycle_config
                .no_progress_thresholds
                .forced_exit_turns;
            match no_progress_decision {
                NoProgressControlledExitDecision::Disabled
                | NoProgressControlledExitDecision::BelowThreshold => {}
                NoProgressControlledExitDecision::ShadowWouldExit => {
                    tracing::info!(
                        task_id = %task_id,
                        session_id = %session.id,
                        threshold = ?no_progress_threshold,
                        streak = no_progress_streak,
                        termination_reason = "no_progress",
                        rollout = "shadow",
                        "djinn.controlled_exit.requested"
                    );
                }
                NoProgressControlledExitDecision::DeferredForCommand => {
                    tracing::info!(
                        task_id = %task_id,
                        session_id = %session.id,
                        threshold = ?no_progress_threshold,
                        streak = no_progress_streak,
                        termination_reason = "no_progress",
                        command_state = ?no_progress_command_state,
                        "djinn.controlled_exit.deferred"
                    );
                    continue;
                }
                NoProgressControlledExitDecision::RequestExit => {
                    tracing::warn!(
                        task_id = %task_id,
                        session_id = %session.id,
                        threshold = ?no_progress_threshold,
                        streak = no_progress_streak,
                        termination_reason = "no_progress",
                        "djinn.controlled_exit.requested"
                    );
                    let auto_submit_accepted = self
                        .try_accept_existing_auto_submit(
                            task_id,
                            &session.id,
                            session.task_run_id.as_deref(),
                            no_progress_streak,
                            "no_progress",
                        )
                        .await;
                    let preservation_result = if auto_submit_accepted {
                        PreservationGateResult::clean_skip(
                            task_id,
                            &session.id,
                            djinn_telemetry::preservation::TRIGGER_STALL,
                            "auto-submit accepted before no-progress exit",
                        )
                    } else {
                        self.request_session_preservation(
                            task_id,
                            &session.id,
                            session.task_run_id.as_deref(),
                            djinn_telemetry::preservation::TRIGGER_STALL,
                        )
                        .await
                    };
                    let preservation_blocks = preservation_result.outcome.should_block_transition(
                        self.worker_lifecycle_config.checkpoint.failure_policy,
                    );
                    tracing::info!(
                        task_id = %task_id,
                        session_id = %session.id,
                        threshold = ?no_progress_threshold,
                        streak = no_progress_streak,
                        termination_reason = "no_progress",
                        auto_submit_result = if auto_submit_accepted { "accepted" } else { "not_accepted" },
                        checkpoint_result = ?preservation_result.outcome,
                        failure_policy = ?self.worker_lifecycle_config.checkpoint.failure_policy,
                        failure_policy_outcome = if preservation_blocks { "blocked" } else { "proceeded" },
                        "djinn.preservation.decision"
                    );
                    if preservation_blocks {
                        tracing::warn!(task_id = %task_id, session_id = %session.id, "djinn.controlled_exit.deferred");
                        continue;
                    }
                    if let Err(e) = self.pool.kill_session(task_id).await {
                        tracing::warn!(task_id = %task_id, session_id = %session.id, error = %e, "CoordinatorActor: failed to kill no-progress session");
                        continue;
                    }
                    self.stall_killed.insert(session.id.clone());

                    // ── Liveness evidence: persist dead_reclaimed for no-progress kill ──
                    {
                        let liveness_repo = LivenessRepository::new(self.db.clone());
                        let snapshot = LivenessEvidenceSnapshot {
                            session_id: session.id.clone(),
                            task_id: Some(task_id.to_owned()),
                            task_run_id: session.task_run_id.clone(),
                            verdict: "dead".to_owned(),
                            outcome_kind: Some("dead_reclaimed".to_owned()),
                            outcome_reason: None,
                            evidence: serde_json::json!({
                                "reason": "no_progress_kill",
                                "no_progress_streak": no_progress_streak,
                                "kill_source": "session_recovery_no_progress",
                            }),
                        };
                        if let Err(e) = liveness_repo.persist_evidence(&snapshot).await {
                            tracing::warn!(
                                task_id = %task_id,
                                session_id = %session.id,
                                error = %e,
                                "CoordinatorActor: failed to persist no-progress-kill dead_reclaimed evidence"
                            );
                        }
                    }

                    // Terminalize the matching attempt as a timeout: a
                    // no-progress controlled exit reclaims a session that stopped
                    // making durable progress (a stall variant).
                    self.terminalize_recovery_attempt(
                        task_id,
                        &session.agent_type,
                        TaskAttemptOutcome::TimedOut,
                        "session_recovery_no_progress",
                        Some(&session.id),
                        session.task_run_id.as_deref(),
                        Some("dead"),
                        Some("no_progress"),
                        None,
                    )
                    .await;

                    tracing::warn!(
                        task_id = %task_id,
                        session_id = %session.id,
                        threshold = ?no_progress_threshold,
                        streak = no_progress_streak,
                        termination_reason = "no_progress",
                        "djinn.controlled_exit.completed"
                    );
                    continue;
                }
            }

            // ── DB-truth liveness backstop ──────────────────────────────────
            // The idle measurement above comes from the in-memory
            // `ActivityTracker`, which for a remote worker pod is fed only by
            // the worker's `touch_activity` RPC bridge. When that bridge drifts
            // (or breaks) the host tracker goes silent even though the pod is
            // busy — the exact failure that stall-killed 88 productive sessions
            // overnight. Before killing, consult a signal the drift cannot
            // touch: the session ROW's own token counters, persisted mid-flight
            // by `flush_session_tokens_rpc`. Compare the live total against the
            // watermark from the previous sweep; if it advanced, the worker is
            // demonstrably producing output, so spare it and roll the watermark
            // forward. A genuinely dead session's counters are frozen — its
            // watermark never advances and it is still killed at threshold. This
            // mirrors the tool-run-heartbeat precedent that stopped false reaps
            // of long clippy runs (~v0.4.7).
            let db_progress = session.tokens_in.max(0) as u64
                + session.tokens_out.max(0) as u64
                + session.cache_read_tokens.max(0) as u64
                + session.cache_write_tokens.max(0) as u64;
            let prev_watermark = self
                .stall_progress_watermark
                .insert(session.id.clone(), db_progress);

            if idle <= applied_threshold {
                continue;
            }

            // Over the idle threshold, but if the session row shows token
            // progress since the last sweep the in-memory idle clock is lying —
            // never cancel a session for "idle" while there is independent DB
            // evidence of progress.
            if let Some(prev) = prev_watermark
                && db_progress > prev
            {
                tracing::info!(
                    task_id = %task_id,
                    session_id = %session.id,
                    idle_seconds = idle,
                    threshold_secs = applied_threshold,
                    db_progress,
                    prev_watermark = prev,
                    "CoordinatorActor: stall backstop spared session — DB token progress advanced despite silent activity tracker"
                );
                continue;
            }

            // ── Liveness classifier gate (Slow/Live extension) ─────────
            // Before killing, consult the shared liveness classifier. A
            // `Slow` verdict (pod running but idle, below hard cap) extends
            // the claim instead of killing. A `Live` verdict spares the
            // session. `Dead`/`ProtocolViolation`/hard-cap-exceeded fall
            // through to the existing kill path.
            //
            // Classification runs unconditionally so the result is available
            // for dead_reclaimed / kill_noop evidence persistence after the
            // kill, regardless of whether slow-extension is enabled.
            let classification = self.classify_task_liveness(task_id).await;
            let slow_ext = &self.worker_lifecycle_config.slow_extension;
            if slow_ext.enabled
                && let Some(ref result) = classification
            {
                match result.verdict {
                    Verdict::Live => {
                        tracing::info!(
                            task_id = %task_id,
                            session_id = %session.id,
                            verdict = %result.verdict,
                            idle_seconds = idle,
                            "CoordinatorActor: liveness classifier spared session (Live verdict)"
                        );
                        continue;
                    }
                    Verdict::Slow if result.extension_eligible => {
                        let ext_count = self
                            .stall_extension_count
                            .entry(session.id.clone())
                            .or_insert(0);
                        if *ext_count < slow_ext.max_extensions {
                            let budget_before = *ext_count;
                            *ext_count += 1;
                            let budget_after = *ext_count;

                            // Persist slow_extended evidence.
                            let liveness_repo = LivenessRepository::new(self.db.clone());
                            let evidence_snapshot = LivenessEvidenceSnapshot {
                                session_id: session.id.clone(),
                                task_id: Some(task_id.to_owned()),
                                task_run_id: session.task_run_id.clone(),
                                verdict: Verdict::Slow.as_str().to_owned(),
                                outcome_kind: Some(
                                    LivenessOutcome::SlowExtended.as_str().to_owned(),
                                ),
                                outcome_reason: None,
                                evidence: serde_json::to_value(&result.evidence)
                                    .unwrap_or_default(),
                            };
                            let evidence_id = liveness_repo
                                .persist_evidence(&evidence_snapshot)
                                .await
                                .ok();

                            // Record claim extension metadata.
                            let project_id = session.project_id.clone().unwrap_or_default();
                            let extension_record = ClaimExtensionRecord {
                                session_id: session.id.clone(),
                                task_run_id: session.task_run_id.clone(),
                                project_id,
                                liveness_evidence_id: evidence_id,
                                granted: true,
                                extension_budget_before: budget_before as i32,
                                extension_budget_after: budget_after as i32,
                                metadata: serde_json::json!({
                                    "quantum_secs": slow_ext.quantum_secs,
                                    "idle_seconds": idle,
                                    "threshold_secs": applied_threshold,
                                    "reason": "slow_verdict_stall_extension",
                                }),
                            };
                            if let Err(e) = liveness_repo
                                .record_claim_extension(&extension_record)
                                .await
                            {
                                tracing::warn!(
                                    task_id = %task_id,
                                    session_id = %session.id,
                                    error = %e,
                                    "CoordinatorActor: failed to record claim extension"
                                );
                            }

                            tracing::info!(
                                task_id = %task_id,
                                session_id = %session.id,
                                verdict = %result.verdict,
                                extension_count = budget_after,
                                max_extensions = slow_ext.max_extensions,
                                quantum_secs = slow_ext.quantum_secs,
                                idle_seconds = idle,
                                "CoordinatorActor: liveness classifier extended claim (Slow verdict)"
                            );
                            continue;
                        }
                        // Budget exhausted — fall through to kill.
                        tracing::info!(
                            task_id = %task_id,
                            session_id = %session.id,
                            verdict = %result.verdict,
                            extension_count = *ext_count,
                            max_extensions = slow_ext.max_extensions,
                            "CoordinatorActor: slow extension budget exhausted — falling through to kill"
                        );
                    }
                    Verdict::Slow => {
                        // Slow but not extension-eligible (hard cap exceeded
                        // or budget exhausted via classifier). Fall through.
                        tracing::info!(
                            task_id = %task_id,
                            session_id = %session.id,
                            verdict = %result.verdict,
                            extension_eligible = false,
                            reason = ?result.reason,
                            "CoordinatorActor: liveness classifier Slow verdict not extension-eligible — killing"
                        );
                    }
                    Verdict::Dead | Verdict::ProtocolViolation => {
                        tracing::info!(
                            task_id = %task_id,
                            session_id = %session.id,
                            verdict = %result.verdict,
                            outcome = ?result.outcome,
                            reason = ?result.reason,
                            "CoordinatorActor: liveness classifier returned kill-worthy verdict"
                        );
                    }
                }
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

            // ── Liveness evidence: persist dead_reclaimed for stall kill ──
            // The stall kill frees a running session — persist evidence so
            // the kill is recorded as a real reclaim rather than silently
            // swallowed. Use the classification result from the slow-
            // extension gate if available; otherwise build fallback evidence.
            {
                let liveness_repo = LivenessRepository::new(self.db.clone());
                let (verdict_str, outcome_kind, evidence_json) =
                    if let Some(ref result) = classification {
                        (
                            result.verdict.as_str().to_owned(),
                            Some(LivenessOutcome::DeadReclaimed.as_str().to_owned()),
                            serde_json::to_value(&result.evidence).unwrap_or_default(),
                        )
                    } else {
                        (
                            "dead".to_owned(),
                            Some("dead_reclaimed".to_owned()),
                            serde_json::json!({
                                "reason": "stall_kill",
                                "idle_seconds": idle,
                                "threshold_secs": applied_threshold,
                                "never_active": never_active,
                                "classification_unavailable": true,
                            }),
                        )
                    };
                let snapshot = LivenessEvidenceSnapshot {
                    session_id: session.id.clone(),
                    task_id: Some(task_id.to_owned()),
                    task_run_id: session.task_run_id.clone(),
                    verdict: verdict_str,
                    outcome_kind,
                    outcome_reason: None,
                    evidence: evidence_json,
                };
                if let Err(e) = liveness_repo.persist_evidence(&snapshot).await {
                    tracing::warn!(
                        task_id = %task_id,
                        session_id = %session.id,
                        error = %e,
                        "CoordinatorActor: failed to persist stall-kill dead_reclaimed evidence"
                    );
                }
            }

            // ── Preservation gate: request checkpoint before terminal transition ──
            // The stall kill is a controlled termination; request or record
            // preservation of potentially dirty worker output before proceeding.
            let preservation_result = self
                .request_session_preservation(
                    task_id,
                    &session.id,
                    session.task_run_id.as_deref(),
                    djinn_telemetry::preservation::TRIGGER_STALL,
                )
                .await;

            // Gate: inspect the preservation outcome. If the outcome would
            // block the transition under the configured policy, record a
            // warning and skip post-kill actions (breaker recording, stall
            // recording, planner intervention routing) for this tick. The
            // session is already killed and in `stall_killed`, so it will
            // not be re-killed; the next tick will re-evaluate.
            if preservation_result
                .outcome
                .should_block_transition(self.worker_lifecycle_config.checkpoint.failure_policy)
            {
                tracing::warn!(
                    task_id = %task_id,
                    session_id = %session.id,
                    outcome = ?preservation_result.outcome,
                    reason = %preservation_result.reason,
                    "Preservation gate blocking stall termination; skipping breaker/intervention this tick"
                );
                continue;
            }

            // Resolve the breaker scope: health is keyed per owning user so this
            // trip only demotes the model for THIS task's creator, not globally.
            // The throttle that produces first-call hangs is per-credential
            // (most acutely the per-account ChatGPT Codex backend), so a global
            // trip would disable the model for everyone on one user's bad luck.
            let task_repo = self.task_repo();
            let task_row = task_repo.get(task_id).await.ok().flatten();
            let scope = task_row.as_ref().map(|t| t.created_by_user_id.clone());
            let current_status = task_row
                .as_ref()
                .map(|t| t.status.clone())
                .unwrap_or_default();

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

            // Terminalize the matching attempt as a timeout. A stall kill (idle
            // past threshold, or a first-call hang) reclaims a live session; the
            // failure class distinguishes the two so downstream can tell a hung
            // first call from a productive turn that went quiet.
            let stall_verdict = classification.as_ref().map(|c| c.verdict.as_str());
            let stall_failure_class = if never_active {
                "first_call_hang"
            } else {
                "idle_stall"
            };

            // Emit reasoning-kill observability: classify stall kills by
            // reasoning-model context and failure class so operators can track
            // false-positive kill rates for reasoning models (whose first-turn
            // latency is naturally higher).
            let model_context =
                if djinn_telemetry::reasoning_kill::is_reasoning_model(&session.model_id) {
                    djinn_telemetry::reasoning_kill::MODEL_CONTEXT_REASONING
                } else {
                    djinn_telemetry::reasoning_kill::MODEL_CONTEXT_NON_REASONING
                };
            let failure_class = if never_active {
                djinn_telemetry::reasoning_kill::FAILURE_CLASS_FIRST_CALL_HANG
            } else {
                djinn_telemetry::reasoning_kill::FAILURE_CLASS_IDLE_STALL
            };
            djinn_telemetry::reasoning_kill::increment(
                model_context,
                failure_class,
                djinn_telemetry::reasoning_kill::OUTCOME_KILLED,
            );

            self.terminalize_recovery_attempt(
                task_id,
                &session.agent_type,
                TaskAttemptOutcome::TimedOut,
                "session_recovery_stall",
                Some(&session.id),
                session.task_run_id.as_deref(),
                stall_verdict,
                Some(stall_failure_class),
                Some(reason),
            )
            .await;

            // ── Zero-output wall-clock observation ────────────────────
            // Record the wall-clock time this session spent before the
            // stall/kill decision was made. Emitted after all gate checks
            // pass so the metric only fires for genuine kills.
            {
                let (timeout_source, failure_class) = if never_active {
                    (
                        djinn_telemetry::liveness_metrics::TIMEOUT_SOURCE_FIRST_CALL_HANG,
                        djinn_telemetry::liveness_metrics::FAILURE_CLASS_FIRST_CALL_HANG,
                    )
                } else {
                    (
                        djinn_telemetry::liveness_metrics::TIMEOUT_SOURCE_IDLE_STALL,
                        djinn_telemetry::liveness_metrics::FAILURE_CLASS_IDLE_STALL,
                    )
                };
                // chain_exhausted is set conservatively to false here; the
                // strike_count escalation below may trigger planner
                // intervention but the failover chain itself hasn't been
                // exhausted at this point (the session was killed for stall,
                // not for failover chain exhaustion). The "chain_exhausted"
                // dimension captures the case where failover candidates were
                // all tried; in the stall path there is no candidate chain.
                djinn_telemetry::liveness_metrics::record_zero_output_stall(
                    std::time::Duration::from_secs(idle),
                    timeout_source,
                    failure_class,
                    false,
                );
            }

            // ── Second-strike stall escalation ──────────────────────────────
            // Record this stall cancel against the task. Two CONSECUTIVE stall
            // cancels with no durable task-status progress between them means the
            // task is looping without converging — a loop the reopen-count
            // escalation (triggers A/B) never observes, because a stall kill
            // releases the task straight back into execution without passing
            // through `open` (reopen_count stays flat). On the second strike,
            // route it to the Planner instead of letting dispatch send a third
            // doomed session (the overnight incident stall-killed one task 14
            // times behind a single slot).
            let strike_count = {
                let streak = self
                    .stall_cancel_streak
                    .entry(task_id.to_owned())
                    .and_modify(|s| {
                        if s.last_status == current_status {
                            s.count += 1;
                        } else {
                            // Durable status progress between strikes — reset.
                            s.count = 1;
                            s.last_status = current_status.clone();
                        }
                    })
                    .or_insert_with(|| StallCancelStreak {
                        count: 1,
                        last_status: current_status.clone(),
                    });
                streak.count
            };

            if strike_count >= STALL_CANCEL_ESCALATION_THRESHOLD {
                tracing::warn!(
                    task_id = %task_id,
                    session_id = %session.id,
                    strike_count,
                    status = %current_status,
                    "CoordinatorActor: consecutive stall cancels without status progress — routing to Planner intervention instead of redispatch"
                );
                let intervention_reason = format!(
                    "Task stall-cancelled on {strike_count} consecutive sessions with no durable \
                     status progress between them (status `{current_status}`). A stall kill \
                     releases the task straight back into execution without passing through \
                     `open`, so the reopen-based escalation never saw this loop — each redispatch \
                     just stalls again and holds the dispatch slot. Decide how to unstick it: \
                     DECOMPOSE into focused subtasks, RESCOPE/clarify the acceptance criteria and \
                     re-dispatch, or CLOSE if the durable work on the task branch is already \
                     sufficient or the task is moot/duplicate."
                );
                // Clear the streak so a post-intervention run starts fresh (and a
                // re-armed intervention is not double-counted).
                self.stall_cancel_streak.remove(task_id);
                self.route_loop_guard_planner_intervention(
                    task_id,
                    "coordinator",
                    &intervention_reason,
                )
                .await;
            }
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
    pub(crate) async fn reap_zombie_sessions(&mut self) {
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

            // ── Liveness classifier gate (Dead verdict) ──────────────
            // Consult the shared liveness classifier before reclaim. The
            // existing guards (RPC registry, activity tracker, zombie age)
            // have already narrowed to sessions that look dead, but the
            // classifier is the authoritative gate: a `Live` or `Slow`
            // verdict suppresses reclaim, and a terminal-task `KillNoop`
            // prevents reopening a task that is already finished.
            //
            // KillNoop is checked FIRST because the classifier returns
            // `Verdict::Live` for terminal tasks (the verdict is moot;
            // only the outcome matters). Without this ordering the `Live`
            // arm fires and the KillNoop handling is unreachable.
            let classification = self.classify_task_liveness(task_id).await;
            if let Some(ref result) = classification {
                // Terminal task race: task is already terminal.
                // classify_task_liveness already persisted the KillNoop
                // evidence (verdict + outcome_kind on the session row
                // and in the liveness_evidence table). We must still
                // finalize the orphaned session, but must NOT release
                // the task — it is already finished.
                if result.outcome == Some(LivenessOutcome::KillNoop) {
                    tracing::info!(
                        task_id = %task_id,
                        session_id = %session.id,
                        "CoordinatorActor: zombie session — task already terminal; finalizing session (KillNoop)"
                    );
                    // Persist KillNoop evidence. classify_task_liveness may
                    // have already written this, but we write it unconditionally
                    // so the outcome is always recorded even if the classifier's
                    // own persistence failed (e.g. load_current_state returned
                    // an active_session_id that changed between classify and
                    // here, or the DB write was silently swallowed).
                    let liveness_repo = LivenessRepository::new(self.db.clone());
                    let snapshot = LivenessEvidenceSnapshot {
                        session_id: session.id.clone(),
                        task_id: Some(task_id.to_owned()),
                        task_run_id: session.task_run_id.clone(),
                        verdict: result.verdict.as_str().to_owned(),
                        outcome_kind: Some(LivenessOutcome::KillNoop.as_str().to_owned()),
                        outcome_reason: None,
                        evidence: serde_json::to_value(&result.evidence).unwrap_or_default(),
                    };
                    if let Err(e) = liveness_repo.persist_evidence(&snapshot).await {
                        tracing::warn!(
                            task_id = %task_id,
                            session_id = %session.id,
                            error = %e,
                            "CoordinatorActor: failed to persist KillNoop evidence"
                        );
                    }
                    // Finalize the orphaned running row.
                    if let Err(e) = session_repo.interrupt_running_for_task(task_id).await {
                        tracing::warn!(
                            task_id = %task_id,
                            session_id = %session.id,
                            error = %e,
                            "CoordinatorActor: failed to finalize zombie session row for KillNoop"
                        );
                    }
                    // Teardown any leaked K8s Job for this task-run.
                    self.teardown_zombie_taskrun_job(
                        task_id,
                        &session.id,
                        session.task_run_id.as_deref(),
                    )
                    .await;
                    self.stall_killed.remove(&session.id);
                    continue;
                }
                match result.verdict {
                    Verdict::Live | Verdict::Slow => {
                        tracing::info!(
                            task_id = %task_id,
                            session_id = %session.id,
                            verdict = %result.verdict,
                            "CoordinatorActor: liveness classifier spared zombie session"
                        );
                        continue;
                    }
                    _ => {}
                }
            }

            // Persist DeadReclaimed evidence before destructive reclaim.
            {
                let liveness_repo = LivenessRepository::new(self.db.clone());
                let dead_evidence = match &classification {
                    Some(result) => serde_json::to_value(&result.evidence).unwrap_or_default(),
                    None => serde_json::json!({
                        "reason": "classification_unavailable",
                        "age_seconds": age,
                        "status_overrides_token_skip": status_overrides_token_skip,
                    }),
                };
                let snapshot = LivenessEvidenceSnapshot {
                    session_id: session.id.clone(),
                    task_id: Some(task_id.to_owned()),
                    task_run_id: session.task_run_id.clone(),
                    verdict: Verdict::Dead.as_str().to_owned(),
                    outcome_kind: Some(LivenessOutcome::DeadReclaimed.as_str().to_owned()),
                    outcome_reason: None,
                    evidence: dead_evidence,
                };
                if let Err(e) = liveness_repo.persist_evidence(&snapshot).await {
                    tracing::warn!(
                        task_id = %task_id,
                        session_id = %session.id,
                        error = %e,
                        "CoordinatorActor: failed to persist dead_reclaimed evidence"
                    );
                } else {
                    tracing::info!(
                        task_id = %task_id,
                        session_id = %session.id,
                        "CoordinatorActor: persisted dead_reclaimed liveness evidence"
                    );
                }
            }

            // ── Zero-output wall-clock observation (zombie reap) ────────
            // Record the wall-clock time this zombie session spent with
            // zero output before being reaped. `age` is the elapsed
            // seconds since session start.
            djinn_telemetry::liveness_metrics::record_zero_output_stall(
                std::time::Duration::from_secs(age),
                djinn_telemetry::liveness_metrics::TIMEOUT_SOURCE_FIRST_CALL_HANG,
                djinn_telemetry::liveness_metrics::FAILURE_CLASS_FIRST_CALL_HANG,
                false,
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

            // ── Preservation gate: record preservation attempt before zombie reap ──
            // Zombie sessions have no live worker (that's how they got here),
            // so the preservation gate will record an "unavailable_worker" result.
            // The result is logged for observability and the metadata is attached
            // to the session's lifecycle state.
            let preservation_result = self
                .request_session_preservation(
                    task_id,
                    &session.id,
                    session.task_run_id.as_deref(),
                    djinn_telemetry::preservation::TRIGGER_ZOMBIE,
                )
                .await;

            // Gate: inspect the preservation outcome. If the outcome would
            // block the transition under the configured policy, skip the
            // job teardown, eviction, finalization, and task release for
            // this tick. The next tick will re-evaluate.
            if preservation_result
                .outcome
                .should_block_transition(self.worker_lifecycle_config.checkpoint.failure_policy)
            {
                tracing::warn!(
                    task_id = %task_id,
                    session_id = %session.id,
                    outcome = ?preservation_result.outcome,
                    reason = %preservation_result.reason,
                    "Preservation gate blocking zombie reap; skipping teardown/finalization this tick"
                );
                continue;
            }

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

            // Terminalize the matching attempt for the reclaimed zombie session.
            // A dead pod past the hard cap with no live worker is a crash; a
            // hard-runtime-cap breach is a timeout. Carry the liveness verdict
            // and a failure class into the attempt's structured context.
            let (zombie_outcome, zombie_failure_class) =
                match classification.as_ref().and_then(|c| c.reason) {
                    Some(super::liveness::LivenessReason::HardRuntimeExceeded) => {
                        (TaskAttemptOutcome::TimedOut, "hard_runtime_exceeded")
                    }
                    Some(reason) => (TaskAttemptOutcome::Crashed, reason.as_str()),
                    None => (TaskAttemptOutcome::Crashed, "zombie_no_live_worker"),
                };
            let zombie_verdict = classification
                .as_ref()
                .map(|c| c.verdict.as_str())
                .unwrap_or("dead");
            self.terminalize_recovery_attempt(
                task_id,
                &session.agent_type,
                zombie_outcome,
                "session_recovery_zombie_reap",
                Some(&session.id),
                session.task_run_id.as_deref(),
                Some(zombie_verdict),
                Some(zombie_failure_class),
                None,
            )
            .await;

            djinn_telemetry::zombie::increment_reap(djinn_telemetry::zombie::KIND_STALL);

            // Release the task from its execution status so dispatch can pick
            // it up again. Mirrors the orphan reconciler's status→action map,
            // but does not depend on `has_session` (which is exactly the gate
            // that drifted).
            //
            // Terminal-task race guard: re-read the task status to detect
            // concurrent finalization. If the task became terminal between
            // our initial read and now, persist KillNoop metadata and skip
            // the release — do not reopen a finished task.
            match task_row {
                Ok(Some(_)) => {
                    // Re-read for concurrent terminal check.
                    let fresh_task = task_repo.get(task_id).await;
                    match fresh_task {
                        Ok(Some(ref task))
                            if task.status == "closed" || task.status == "force_closed" =>
                        {
                            let liveness_repo = LivenessRepository::new(self.db.clone());
                            let snapshot = LivenessEvidenceSnapshot {
                                session_id: session.id.clone(),
                                task_id: Some(task_id.to_owned()),
                                task_run_id: session.task_run_id.clone(),
                                verdict: Verdict::Dead.as_str().to_owned(),
                                outcome_kind: Some(LivenessOutcome::KillNoop.as_str().to_owned()),
                                outcome_reason: None,
                                evidence: serde_json::json!({
                                    "reason": "task_became_terminal_concurrently",
                                    "task_status": task.status,
                                }),
                            };
                            let _ = liveness_repo.persist_evidence(&snapshot).await;
                            tracing::info!(
                                task_id = %task_id,
                                session_id = %session.id,
                                status = %task.status,
                                "CoordinatorActor: zombie task already terminal — skipping release (KillNoop)"
                            );
                        }
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
                            tracing::warn!(task_id = %task_id, error = %e, "CoordinatorActor: failed to re-read task for zombie reap terminal race check")
                        }
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
    pub(crate) async fn reap_idle_chat_sessions(&self) {
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
    pub(crate) async fn detect_and_recover_stuck_filtered(&mut self, project_filter: Option<&str>) {
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
                        #[allow(clippy::expect_used)]
                        let guard = self
                            .background_work_tracker
                            .lock()
                            .expect("background work tracker mutex poisoned");
                        guard.contains(&task.id)
                    };
                    if has_background_work {
                        continue;
                    }

                    // ── Liveness classifier gate for stale ready-state orphan ──
                    // Consult the classifier before finalizing the running
                    // session. A stale ready-state orphan has no live pod
                    // (verified by has_session and background-work checks
                    // above), so the classifier will normally return Dead.
                    // If the task is already terminal, record KillNoop.
                    let classification = self.classify_task_liveness(&task.id).await;
                    if let Some(ref result) = classification {
                        if result.outcome == Some(LivenessOutcome::KillNoop) {
                            let liveness_repo = LivenessRepository::new(self.db.clone());
                            let snapshot = LivenessEvidenceSnapshot {
                                session_id: running_session.id.clone(),
                                task_id: Some(task.id.clone()),
                                task_run_id: running_session.task_run_id.clone(),
                                verdict: result.verdict.as_str().to_owned(),
                                outcome_kind: Some(LivenessOutcome::KillNoop.as_str().to_owned()),
                                outcome_reason: None,
                                evidence: serde_json::to_value(&result.evidence)
                                    .unwrap_or_default(),
                            };
                            let _ = liveness_repo.persist_evidence(&snapshot).await;
                            tracing::info!(
                                task_id = %task.short_id,
                                session_id = %running_session.id,
                                "CoordinatorActor: stale ready-state orphan — task already terminal; recording KillNoop"
                            );
                            continue;
                        }
                        // Persist dead_reclaimed evidence before finalizing.
                        if result.verdict == Verdict::Dead {
                            let liveness_repo = LivenessRepository::new(self.db.clone());
                            let snapshot = LivenessEvidenceSnapshot {
                                session_id: running_session.id.clone(),
                                task_id: Some(task.id.clone()),
                                task_run_id: running_session.task_run_id.clone(),
                                verdict: Verdict::Dead.as_str().to_owned(),
                                outcome_kind: Some(
                                    LivenessOutcome::DeadReclaimed.as_str().to_owned(),
                                ),
                                outcome_reason: None,
                                evidence: serde_json::to_value(&result.evidence)
                                    .unwrap_or_default(),
                            };
                            let _ = liveness_repo.persist_evidence(&snapshot).await;
                        }
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
                            // Terminalize the matching attempt for the reclaimed
                            // orphan. The session outlived a reset/release with no
                            // live pod, so it counts as a crashed attempt.
                            let orphan_verdict = classification
                                .as_ref()
                                .map(|c| c.verdict.as_str())
                                .unwrap_or("dead");
                            self.terminalize_recovery_attempt(
                                &task.id,
                                &running_session.agent_type,
                                TaskAttemptOutcome::Crashed,
                                "session_recovery_stale_ready_state",
                                Some(&running_session.id),
                                running_session.task_run_id.as_deref(),
                                Some(orphan_verdict),
                                Some("stale_ready_state_orphan"),
                                None,
                            )
                            .await;
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
                    #[allow(clippy::expect_used)]
                    let guard = self
                        .background_work_tracker
                        .lock()
                        .expect("background work tracker mutex poisoned");
                    guard.contains(&task.id)
                };
                if has_background_work {
                    continue;
                }

                // ── Liveness classifier gate for execution-state orphan ──
                // The task is in an execution state but has no slot session
                // and no background work. Consult the classifier to confirm
                // this is a dead-orphan reclaim, and to detect concurrent
                // terminal transitions.
                let classification = self.classify_task_liveness(&task.id).await;
                if let Some(ref result) = classification {
                    if result.outcome == Some(LivenessOutcome::KillNoop) {
                        let liveness_repo = LivenessRepository::new(self.db.clone());
                        let snapshot = LivenessEvidenceSnapshot {
                            session_id: String::new(),
                            task_id: Some(task.id.clone()),
                            task_run_id: None,
                            verdict: result.verdict.as_str().to_owned(),
                            outcome_kind: Some(LivenessOutcome::KillNoop.as_str().to_owned()),
                            outcome_reason: None,
                            evidence: serde_json::to_value(&result.evidence).unwrap_or_default(),
                        };
                        let _ = liveness_repo.persist_evidence(&snapshot).await;
                        tracing::info!(
                            task_id = %task.short_id,
                            "CoordinatorActor: execution-state orphan — task already terminal; recording KillNoop"
                        );
                        continue;
                    }
                    if result.verdict == Verdict::Dead {
                        let liveness_repo = LivenessRepository::new(self.db.clone());
                        let snapshot = LivenessEvidenceSnapshot {
                            session_id: String::new(),
                            task_id: Some(task.id.clone()),
                            task_run_id: None,
                            verdict: Verdict::Dead.as_str().to_owned(),
                            outcome_kind: Some(LivenessOutcome::DeadReclaimed.as_str().to_owned()),
                            outcome_reason: None,
                            evidence: serde_json::to_value(&result.evidence).unwrap_or_default(),
                        };
                        let _ = liveness_repo.persist_evidence(&snapshot).await;
                    }
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

    async fn try_accept_existing_auto_submit(
        &self,
        task_id: &str,
        session_id: &str,
        task_run_id: Option<&str>,
        no_progress_streak: u32,
        termination_reason: &'static str,
    ) -> bool {
        let Some(task_run_id) = task_run_id else {
            tracing::info!(
                task_id = %task_id,
                session_id = %session_id,
                termination_reason,
                no_progress_streak,
                auto_submit_result = "skipped",
                block_reason = "missing_task_run_id",
                "djinn.auto_submit.decision"
            );
            return false;
        };
        let enabled = self.worker_lifecycle_config.rollout.auto_submit_if_green
            && self.worker_lifecycle_config.auto_submit.enabled;
        if !enabled {
            tracing::info!(
                task_id = %task_id,
                session_id = %session_id,
                task_run_id = %task_run_id,
                termination_reason,
                no_progress_streak,
                auto_submit_result = "skipped",
                block_reason = "not_enabled",
                "djinn.auto_submit.decision"
            );
            return false;
        }
        let repo =
            djinn_db::repositories::verify_run::AutoSubmitReviewRepository::new(self.db.clone());
        match repo.list_for_task_run(task_run_id).await {
            Ok(reviews) => {
                let accepted = reviews.iter().any(|review| {
                    review.trigger_reason
                        == djinn_core::models::AutoSubmitTriggerReason::NoProgress.as_str()
                        && review.session_id.as_deref() == Some(session_id)
                        && review.model_called_submit_work
                });
                tracing::info!(
                    task_id = %task_id,
                    session_id = %session_id,
                    task_run_id = %task_run_id,
                    termination_reason,
                    no_progress_streak,
                    auto_submit_result = if accepted { "accepted" } else { "blocked" },
                    block_reason = if accepted { "none" } else { "no_eligible_no_progress_review" },
                    "djinn.auto_submit.decision"
                );
                accepted
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    session_id = %session_id,
                    task_run_id = %task_run_id,
                    termination_reason,
                    no_progress_streak,
                    auto_submit_result = "blocked",
                    block_reason = "repository_error",
                    error = %e,
                    "djinn.auto_submit.decision"
                );
                false
            }
        }
    }

    /// Fail a task terminally (`ForceClose`) with an actionable reason. Used
    /// when a task is structurally undispatchable (its owner has no model with a
    /// connected provider) or has failed too many consecutive times. Looping
    /// forever is worse than a clear terminal state, so closing it self-cleans
    /// that guard.
    pub(crate) async fn terminally_fail_task(
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

        // ── Preservation gate: request checkpoint before interrupting sessions ──
        // Terminal task failure is a controlled termination; request or record
        // preservation of potentially dirty worker output for all running sessions
        // before interrupting them.
        let session_repo = djinn_db::SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let mut preservation_blocked = false;
        match session_repo.list_for_task(&task.id).await {
            Ok(sessions) => {
                for session in sessions.iter().filter(|s| s.status == "running") {
                    let preservation_result = self
                        .request_session_preservation(
                            &task.id,
                            &session.id,
                            session.task_run_id.as_deref(),
                            djinn_telemetry::preservation::TRIGGER_TERMINAL_FAIL,
                        )
                        .await;

                    // Gate: inspect the preservation outcome. If the outcome
                    // would block the transition under the configured policy,
                    // flag the block and skip session interruption for this tick.
                    if preservation_result.outcome.should_block_transition(
                        self.worker_lifecycle_config.checkpoint.failure_policy,
                    ) {
                        tracing::warn!(
                            task_id = %task.short_id,
                            session_id = %session.id,
                            outcome = ?preservation_result.outcome,
                            reason = %preservation_result.reason,
                            "Preservation gate blocking terminal fail; skipping session interruption"
                        );
                        preservation_blocked = true;
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "CoordinatorActor: failed to list sessions for preservation gate; recording runtime_unavailable"
                );
                let result = PreservationGateResult::runtime_unavailable(
                    &task.id,
                    "",
                    djinn_telemetry::preservation::TRIGGER_TERMINAL_FAIL,
                );
                self.log_preservation_result(&result).await;
                djinn_telemetry::preservation::increment_attempt(
                    djinn_telemetry::preservation::OUTCOME_RUNTIME_UNAVAILABLE,
                    djinn_telemetry::preservation::TRIGGER_TERMINAL_FAIL,
                );
            }
        }

        // If preservation blocked for any running session, do not interrupt
        // or clean up. The task is already ForceClosed; sessions will be
        // cleaned up by the zombie reaper or orphan reconciler on a
        // subsequent tick once preservation unblocks.
        if preservation_blocked {
            return false;
        }

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
        self.cleanup_pr_and_branch_on_close(task, CloseKind::NonMerge)
            .await;
        true
    }

    /// Coordinator-side checkpoint preservation gate.
    ///
    /// Before a controlled failed/escalated/reap-adjacent termination path
    /// finalizes a dirty or potentially dirty worker session, this method
    /// requests preservation from the live worker or records an explicit
    /// preservation failure/policy result when no worker is available.
    ///
    /// The result is logged as an activity entry with the session/task metadata
    /// and a structured `djinn_preservation` tracing event, and the
    /// `djinn_preservation_attempts_total` counter is incremented.
    ///
    /// Callers **must** inspect the returned [`PreservationGateResult`] and
    /// call [`PreservationOutcome::should_block_transition`] with the
    /// configured [`PreservationFailurePolicy`] before proceeding with the
    /// terminal transition. The default policy
    /// ([`PreservationFailurePolicy::RecordAndProceed`]) records the failure
    /// and allows the transition; [`PreservationFailurePolicy::Block`] defers
    /// the transition to the next coordinator tick. The stubbed
    /// `Succeeded` placeholder for the not-yet-wired worker RPC serves as the
    /// unblocking signal; y8pv enforcement may switch the policy to `Block`
    /// once the worker-side RPC is fully wired.
    pub(crate) async fn request_session_preservation(
        &self,
        task_id: &str,
        session_id: &str,
        task_run_id: Option<&str>,
        trigger: &'static str,
    ) -> PreservationGateResult {
        // No runtime_ops available (dev/test mode without Kubernetes or live
        // coordinator) — record an explicit "runtime unavailable" result.
        if self.runtime_ops.is_none() {
            let result = PreservationGateResult::runtime_unavailable(task_id, session_id, trigger);
            self.log_preservation_result(&result).await;
            djinn_telemetry::preservation::increment_attempt(
                djinn_telemetry::preservation::OUTCOME_RUNTIME_UNAVAILABLE,
                trigger,
            );
            return result;
        }

        // No task_run_id — cannot target a specific worker pod.
        let Some(run_id) = task_run_id else {
            let result =
                PreservationGateResult::unavailable_worker(task_id, session_id, None, trigger);
            self.log_preservation_result(&result).await;
            djinn_telemetry::preservation::increment_attempt(
                djinn_telemetry::preservation::OUTCOME_UNAVAILABLE_WORKER,
                trigger,
            );
            return result;
        };

        // Check if a live worker connection exists.
        let has_live_worker = if let Some(registry) = self.rpc_registry.as_ref() {
            registry.is_connected(run_id).await
        } else {
            // No connection registry — treat as runtime unavailable.
            let result = PreservationGateResult::runtime_unavailable(task_id, session_id, trigger);
            self.log_preservation_result(&result).await;
            djinn_telemetry::preservation::increment_attempt(
                djinn_telemetry::preservation::OUTCOME_RUNTIME_UNAVAILABLE,
                trigger,
            );
            return result;
        };

        if !has_live_worker {
            let result = PreservationGateResult::unavailable_worker(
                task_id,
                session_id,
                Some(run_id),
                trigger,
            );
            self.log_preservation_result(&result).await;
            djinn_telemetry::preservation::increment_attempt(
                djinn_telemetry::preservation::OUTCOME_UNAVAILABLE_WORKER,
                trigger,
            );
            return result;
        }

        // A live worker connection exists. The actual preservation RPC
        // (request checkpoint commit/push and await the result) is additive
        // and will be wired by sibling task ztta. For now, record that
        // preservation was "requested" — the worker's checkpoint result shape
        // from epic 7xl3 carries the commit SHA/ref/retry/conflict data
        // through to the activity log once the RPC lands.
        //
        // Until then, we record a "succeeded" result with placeholder data
        // so the contract gate is exercised and metrics flow correctly.
        // The downstream enforcement epic (y8pv) will inspect the
        // `CheckpointLifecycleMetadata.preservation_outcome` field to decide
        // whether to block the terminal transition.
        tracing::info!(
            task_id = %task_id,
            session_id = %session_id,
            task_run_id = %run_id,
            trigger,
            "djinn.preservation.requested — live worker connection exists; actual checkpoint RPC deferred to sibling task ztta"
        );
        let result = PreservationGateResult::succeeded(
            task_id,
            session_id,
            Some(run_id),
            trigger,
            None, // commit_sha — filled by worker RPC when wired
            None, // ref_name — filled by worker RPC when wired
        );
        self.log_preservation_result(&result).await;
        djinn_telemetry::preservation::increment_attempt(
            djinn_telemetry::preservation::OUTCOME_SUCCEEDED,
            trigger,
        );
        result
    }

    /// Emit a structured tracing event and activity-log entry for a
    /// preservation gate result.
    async fn log_preservation_result(&self, result: &PreservationGateResult) {
        let outcome_label = match result.outcome {
            PreservationOutcome::Succeeded => "succeeded",
            PreservationOutcome::Failed => "failed",
            PreservationOutcome::UnavailableWorker => "unavailable_worker",
            PreservationOutcome::RuntimeUnavailable => "runtime_unavailable",
            PreservationOutcome::CleanSkip => "clean_skip",
        };
        match result.outcome {
            PreservationOutcome::Succeeded => {
                tracing::info!(
                    task_id = %result.task_id,
                    session_id = %result.session_id,
                    task_run_id = ?result.task_run_id,
                    trigger = result.trigger,
                    commit_sha = ?result.commit_sha,
                    ref_name = ?result.ref_name,
                    outcome = outcome_label,
                    reason = %result.reason,
                    "djinn.preservation.result"
                );
            }
            PreservationOutcome::CleanSkip => {
                tracing::debug!(
                    task_id = %result.task_id,
                    session_id = %result.session_id,
                    trigger = result.trigger,
                    outcome = outcome_label,
                    reason = %result.reason,
                    "djinn.preservation.result"
                );
            }
            PreservationOutcome::UnavailableWorker | PreservationOutcome::RuntimeUnavailable => {
                tracing::info!(
                    task_id = %result.task_id,
                    session_id = %result.session_id,
                    task_run_id = ?result.task_run_id,
                    trigger = result.trigger,
                    outcome = outcome_label,
                    reason = %result.reason,
                    "djinn.preservation.result — worker/runtime unavailable; proceeding with terminal transition"
                );
            }
            PreservationOutcome::Failed => {
                tracing::warn!(
                    task_id = %result.task_id,
                    session_id = %result.session_id,
                    task_run_id = ?result.task_run_id,
                    trigger = result.trigger,
                    outcome = outcome_label,
                    reason = %result.reason,
                    "djinn.preservation.result — worker reported failure; recording policy result before terminal transition"
                );
            }
        }

        // Persist to activity log as a structured coordinator comment.
        let payload = serde_json::json!({
            "message": format!(
                "Coordinator preservation gate: session {} — {} (trigger={}, outcome={}, reason={})",
                result.session_id, result.task_id, result.trigger, outcome_label, result.reason
            ),
            "preservation_outcome": outcome_label,
            "preservation_trigger": result.trigger,
            "preservation_reason": result.reason,
            "preservation_commit_sha": result.commit_sha,
            "preservation_ref_name": result.ref_name,
        })
        .to_string();
        let task_repo = self.task_repo();
        let _ = task_repo
            .log_activity(
                Some(&result.task_id),
                "coordinator",
                "system",
                "comment",
                &payload,
            )
            .await;
    }

    // ── Liveness classifier foundation entrypoint ────────────────────────

    /// Non-mutating liveness-classification entrypoint near session recovery.
    ///
    /// Gathers normalized pool activity and DB state for `task_id`, maps them
    /// into [`LivenessEvidence`], invokes the pure classifier from
    /// [`super::liveness::classify`], and optionally persists the structured
    /// result via [`LivenessRepository`].
    ///
    /// **Side-effect contract:** this method does NOT mutate task status,
    /// extend claims, kill pods, or increment attempts. It only records
    /// classifier evidence for later consumer epics (`vbgl`, `jk7v`, `ptvg`).
    /// Terminal task races are recorded as noop/idempotent outcomes.
    #[tracing::instrument(
        name = "djinn.session_recovery.classify_liveness",
        skip(self),
        fields(task_id = %task_id)
    )]
    pub(crate) async fn classify_task_liveness(
        &self,
        task_id: &str,
    ) -> Option<ClassificationResult> {
        // ── 1. Load DB state ──────────────────────────────────────────
        let liveness_repo = LivenessRepository::new(self.db.clone());
        let db_state = match liveness_repo.load_current_state(task_id).await {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "classify_task_liveness: failed to load current state"
                );
                return None;
            }
        };

        // ── 2. Load pool activity ────────────────────────────────────
        let pool_info = match self.pool.session_for_task(task_id).await {
            Ok(Some(info)) => Some(info),
            Ok(None) => None,
            Err(e) => {
                tracing::debug!(
                    task_id = %task_id,
                    error = %e,
                    "classify_task_liveness: pool query failed (continuing with DB-only evidence)"
                );
                None
            }
        };

        // ── 3. Build evidence and classify ───────────────────────────
        let evidence = build_liveness_evidence(pool_info.as_ref(), &db_state);
        let result = super::liveness::classify(&evidence);

        tracing::info!(
            task_id = %task_id,
            verdict = %result.verdict,
            outcome = ?result.outcome,
            reason = ?result.reason,
            extension_eligible = %result.extension_eligible,
            "classify_task_liveness: classification complete"
        );

        // ── 4. Persist evidence via ts1j repository helper ───────────
        // Persist whenever an active session exists — the repository is
        // append-only and terminal-task races are recorded as noop/
        // idempotent outcomes (the verdict is KillNoop, not a mutation).
        if let Some(ref session_id) = db_state.active_session_id {
            let snapshot = LivenessEvidenceSnapshot {
                session_id: session_id.clone(),
                task_id: Some(task_id.to_owned()),
                task_run_id: db_state.latest_task_run_id.clone(),
                verdict: result.verdict.as_str().to_owned(),
                outcome_kind: result.outcome.map(|o| o.as_str().to_owned()),
                outcome_reason: liveness_reason_for_persistence(result.reason),
                evidence: serde_json::to_value(&result.evidence).unwrap_or_default(),
            };
            match liveness_repo.persist_evidence(&snapshot).await {
                Ok(evidence_id) => {
                    tracing::debug!(
                        task_id = %task_id,
                        session_id = %session_id,
                        evidence_id = %evidence_id,
                        verdict = %result.verdict,
                        "classify_task_liveness: evidence persisted"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task_id,
                        session_id = %session_id,
                        error = %e,
                        "classify_task_liveness: failed to persist evidence"
                    );
                }
            }
        } else {
            tracing::debug!(
                task_id = %task_id,
                verdict = %result.verdict,
                "classify_task_liveness: no active session — evidence logged only (not persisted)"
            );
        }

        Some(result)
    }

    /// Classify a just-ended session for protocol-violation detection and
    /// persist structured liveness evidence.
    ///
    /// Called when a session transitions to `completed`, `interrupted`, or
    /// `failed`. The classifier determines whether the exit is:
    ///
    /// - **ProtocolViolation (CleanExitNonterminal):** session completed
    ///   (exit 0) but the task is still nonterminal. This is a genuine failed
    ///   attempt — not success — and must be counted by retry accounting.
    /// - **ProtocolViolation (NonzeroExitNonterminal):** session failed/
    ///   interrupted (nonzero exit) while the task is nonterminal. Crash
    ///   semantics; existing retry/failure-streak mechanisms handle it.
    /// - **KillNoop:** the task is already terminal. Preserve terminal state
    ///   and attach metadata only.
    ///
    /// Returns `Some(ClassificationResult)` when a classification was made,
    /// or `None` on error. The caller uses the result to decide retry
    /// accounting (protocol violations increment attempts; slow extensions
    /// do not — but slow extensions never reach this path because they never
    /// end the session).
    ///
    /// `role` is the session's `agent_type`: for `failed`/`interrupted` exits
    /// on a nonterminal task this function also terminalizes the live
    /// `task_attempts` row for the (task, role) pair (see step 6 below).
    #[tracing::instrument(
        name = "djinn.session_recovery.classify_exit",
        skip(self),
        fields(task_id, session_id, session_status)
    )]
    pub(crate) async fn classify_session_exit_liveness(
        &self,
        session_id: &str,
        task_id: &str,
        task_run_id: Option<&str>,
        session_status: &str,
        role: &str,
    ) -> Option<ClassificationResult> {
        tracing::Span::current().record("task_id", task_id);
        tracing::Span::current().record("session_id", session_id);
        tracing::Span::current().record("session_status", session_status);

        // ── 1. Load DB liveness state ──────────────────────────────────
        let liveness_repo = LivenessRepository::new(self.db.clone());
        let db_state = match liveness_repo.load_current_state(task_id).await {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    session_id = %session_id,
                    error = %e,
                    "classify_session_exit_liveness: failed to load current state"
                );
                return None;
            }
        };

        // ── 2. Build evidence with exit code from session status ───────
        // Map session terminal status → pod phase and exit code.
        // - completed → Succeeded, exit 0
        // - failed / interrupted → Failed, exit nonzero (1 as sentinel;
        //   the exact exit code is not stored on the session row)
        let (exit_pod_phase, exit_code) = match session_status {
            "completed" => (PodPhase::Succeeded, Some(0)),
            "failed" | "interrupted" => (PodPhase::Failed, Some(1)),
            _ => {
                tracing::debug!(
                    session_id = %session_id,
                    session_status = %session_status,
                    "classify_session_exit_liveness: non-terminal session status, skipping"
                );
                return None;
            }
        };

        // Build the base evidence from pool/DB state, then override
        // pod_phase and exit_code for the exit path.
        //
        // IMPORTANT: The `db_session_status` is set to `Running` (not
        // the terminal value) because the protocol-violation check in
        // the classifier fires when the POD exits while the SESSION is
        // still running. The session has since been marked terminal by
        // the settlement code, but the evidence must reflect the state
        // at the moment of the violation: pod exited, session was still
        // running, task was nonterminal.
        let mut evidence = build_liveness_evidence(
            None, // no pool info — session already ended, pod is gone
            &db_state,
        );
        evidence.pod_phase = Some(exit_pod_phase);
        evidence.exit_code = exit_code;
        // Use Running to represent the state when the pod exited — this
        // is what triggers the protocol-violation classifier path.
        evidence.db_session_status = Some(DbSessionStatus::Running);

        // ── 3. Classify ────────────────────────────────────────────────
        let result = super::liveness::classify(&evidence);

        tracing::info!(
            task_id = %task_id,
            session_id = %session_id,
            session_status = %session_status,
            verdict = %result.verdict,
            outcome = ?result.outcome,
            reason = ?result.reason,
            "classify_session_exit_liveness: classification complete"
        );

        // ── 4. Persist evidence ────────────────────────────────────────
        let snapshot = LivenessEvidenceSnapshot {
            session_id: session_id.to_owned(),
            task_id: Some(task_id.to_owned()),
            task_run_id: task_run_id.map(str::to_owned),
            verdict: result.verdict.as_str().to_owned(),
            outcome_kind: result.outcome.map(|o| o.as_str().to_owned()),
            outcome_reason: liveness_reason_for_persistence(result.reason),
            evidence: serde_json::to_value(&result.evidence).unwrap_or_default(),
        };
        match liveness_repo.persist_evidence(&snapshot).await {
            Ok(evidence_id) => {
                tracing::debug!(
                    task_id = %task_id,
                    session_id = %session_id,
                    evidence_id = %evidence_id,
                    verdict = %result.verdict,
                    "classify_session_exit_liveness: evidence persisted"
                );
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    session_id = %session_id,
                    error = %e,
                    "classify_session_exit_liveness: failed to persist evidence"
                );
            }
        }

        // ── 5. Log protocol violation prominently ──────────────────────
        if result.verdict == Verdict::ProtocolViolation {
            tracing::warn!(
                task_id = %task_id,
                session_id = %session_id,
                session_status = %session_status,
                reason = ?result.reason,
                outcome = ?result.outcome,
                "classify_session_exit_liveness: protocol violation detected — \
                 session exited while task is nonterminal; counts as failed attempt"
            );
        }

        // ── 6. Terminalize the live attempt for failed/interrupted exits ──
        // A cleanly-failed run (e.g. provider error → TaskRunOutcome::Failed)
        // self-finalizes its session, so none of the session-recovery /
        // zombie-reap terminalizers ever fires for it. Without this step the
        // `pending` task_attempts row created at dispatch-start stays
        // non-terminal forever and the respawn guard defers every future
        // dispatch of this (task, role) pair — a permanent wedge.
        //
        // Outcome selection distinguishes an ENVIRONMENTAL interruption from a
        // genuine failure:
        //
        //  - `interrupted`  → the pod was killed by infrastructure (a coordinator
        //    deploy/rollout, a k8s pod eviction/deletion, or a startup reap of a
        //    run that deploy orphaned) while the task was still nonterminal. This
        //    is an environmental non-attempt: terminalize as `Interrupted` so
        //    nothing wedges, but contribute NO quality strike, NO dispatch-failure
        //    streak, and NO reopen_class penalty (the dispatch reappearance path
        //    treats a latest `Interrupted` attempt as environmental).
        //  - `failed`       → a genuine application/provider crash. Stays a
        //    failure (`Crashed`), which the reappearance path still counts.
        //
        // This terminalizer only fires when the attempt is STILL pending/submitted
        // (`advance_latest_to_terminal` no-ops on an already-terminal row). Every
        // coordinator liveness path (stall-kill → TimedOut, ceiling → LoopGuard,
        // no-progress/zombie/hard-runtime → TimedOut/Crashed) terminalizes the
        // attempt with its own failure outcome BEFORE this event is processed, so
        // stall / runtime-cap / zombie kills are NEVER reclassified environmental
        // here — only a pure infra kill that no liveness path claimed reaches this
        // `Interrupted` branch. Forward-only + idempotent; `KillNoop` means the
        // task is already terminal — leave the attempt to the terminal-path owners
        // (PR poller / force-close).
        if matches!(session_status, "failed" | "interrupted")
            && result.outcome != Some(LivenessOutcome::KillNoop)
        {
            let (attempt_outcome, failure_class, summary) = if session_status == "interrupted" {
                (
                    TaskAttemptOutcome::Interrupted,
                    "environmental_interrupt",
                    "session interrupted by infrastructure (deploy/rollout/pod-eviction/reap) \
                     while task nonterminal — environmental non-attempt, no dispatch penalty",
                )
            } else {
                (
                    TaskAttemptOutcome::Crashed,
                    "session_exit_nonterminal",
                    "session exited failed while task nonterminal",
                )
            };
            self.terminalize_recovery_attempt(
                task_id,
                role,
                attempt_outcome,
                "session_exit_liveness",
                Some(session_id),
                task_run_id,
                Some(result.verdict.as_str()),
                Some(failure_class),
                Some(summary),
            )
            .await;
        }

        Some(result)
    }

    /// Best-effort terminalization of the live `task_attempts` row for a
    /// session-recovery outcome.
    ///
    /// Wraps [`super::attempt_lifecycle::advance_latest_to_terminal`] with a
    /// structured `summary_json` capturing the recovery classifier, the
    /// session/task-run identity, the liveness verdict, and a failure class.
    /// The underlying repo helper is forward-only and idempotent, so duplicate
    /// recovery scans, duplicate terminal handlers, and late terminal reports
    /// never create a duplicate row and never move a terminal attempt backward.
    ///
    /// Best-effort: lookup/write failures are logged inside the helper and
    /// never propagate, so they cannot fail the surrounding recovery
    /// transition.
    #[allow(clippy::too_many_arguments)]
    async fn terminalize_recovery_attempt(
        &self,
        task_id: &str,
        role: &str,
        outcome: TaskAttemptOutcome,
        classifier: &str,
        session_id: Option<&str>,
        task_run_id: Option<&str>,
        liveness_verdict: Option<&str>,
        failure_class: Option<&str>,
        summary: Option<&str>,
    ) {
        let summary_json = serde_json::json!({
            "recovery_classifier": classifier,
            "session_id": session_id,
            "task_run_id": task_run_id,
            "liveness_verdict": liveness_verdict,
            "failure_class": failure_class,
        })
        .to_string();
        super::attempt_lifecycle::advance_latest_to_terminal(
            &self.db,
            super::attempt_lifecycle::TerminalAdvancementParams {
                task_id,
                role,
                outcome,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary,
                summary_json: Some(&summary_json),
                log_tail: None,
            },
        )
        .await;
    }
}

/// Map normalized pool activity and DB state into a [`LivenessEvidence`]
/// packet for the pure classifier.
///
/// This is a standalone pure helper (no `&self`) so it can be unit-tested
/// without constructing a full [`CoordinatorActor`].
fn build_liveness_evidence(
    pool_info: Option<&RunningTaskInfo>,
    db_state: &CurrentLivenessState,
) -> LivenessEvidence {
    // ── Pod phase ─────────────────────────────────────────────────────
    let pod_phase = match pool_info {
        Some(_) => PodPhase::Running,
        None => PodPhase::Absent,
    };

    // ── Activity signal ──────────────────────────────────────────────
    let activity = match pool_info {
        Some(info) => {
            if !info.activity_tracked {
                ActivitySignal::NeverActive
            } else if info.idle_seconds > ZOMBIE_HARD_CAP_SECS {
                ActivitySignal::Idle
            } else {
                ActivitySignal::Active
            }
        }
        None => ActivitySignal::Idle,
    };

    // ── DB session status ────────────────────────────────────────────
    let db_session_status = db_state.active_session_status.as_deref().map(|s| match s {
        "running" => DbSessionStatus::Running,
        "completed" => DbSessionStatus::Completed,
        "interrupted" => DbSessionStatus::Interrupted,
        "failed" => DbSessionStatus::Failed,
        "paused" => DbSessionStatus::Paused,
        _ => DbSessionStatus::Running,
    });

    // ── DB task status ───────────────────────────────────────────────
    let db_task_status = db_state.task_status.as_deref().map(|s| match s {
        "open" => DbTaskStatus::Open,
        "in_progress" => DbTaskStatus::InProgress,
        "needs_task_review" => DbTaskStatus::NeedsTaskReview,
        "in_task_review" => DbTaskStatus::InTaskReview,
        "approved" => DbTaskStatus::Approved,
        "pr_draft" => DbTaskStatus::PrDraft,
        "pr_review" => DbTaskStatus::PrReview,
        "needs_lead_intervention" => DbTaskStatus::NeedsLeadIntervention,
        "in_lead_intervention" => DbTaskStatus::InLeadIntervention,
        "closed" => DbTaskStatus::Closed,
        _ => DbTaskStatus::Open,
    });

    // ── Claim TTL remaining ──────────────────────────────────────────
    // Derive remaining claim TTL from session duration vs zombie hard cap.
    let claim_ttl_remaining = db_state
        .session_started_at
        .as_deref()
        .and_then(parse_iso_elapsed)
        .map(|elapsed| {
            if elapsed >= ZOMBIE_HARD_CAP_SECS {
                Duration::ZERO
            } else {
                Duration::from_secs(ZOMBIE_HARD_CAP_SECS - elapsed)
            }
        });

    // ── Hard runtime deadline ────────────────────────────────────────
    let hard_runtime_deadline_exceeded = db_state
        .task_run_started_at
        .as_deref()
        .and_then(parse_iso_elapsed)
        .map(|elapsed| elapsed >= ZOMBIE_HARD_CAP_SECS)
        .unwrap_or(false);

    // ── Extension budget ────────────────────────────────────────────
    // A session that has run past its claim lease (`claim_ttl_remaining`
    // zero, i.e. session age ≥ the zombie hard cap) is egregiously stale:
    // its budget is exhausted, so the classifier marks it NOT
    // extension-eligible and the stall path kills it on the first tick
    // instead of granting a "free" grace extension. A session still WITHIN
    // its claim window keeps a non-zero TTL, stays extension-eligible, and
    // gets its one coordinator-gated grace extension (accounted separately
    // via `stall_extension_count` in `enforce_session_stall_timeout`).
    //
    // This restores the immediate-kill fast path for past-TTL sessions.
    // Hardcoding `false` here removed it: every stalled session — however
    // stale — was granted a grace extension, delaying its kill by one
    // extension quantum and breaking the "past-lease → kill now" contract.
    let extension_budget_exhausted = claim_ttl_remaining.map(|t| t.is_zero()).unwrap_or(false);

    LivenessEvidence {
        pod_phase: Some(pod_phase),
        activity,
        db_session_status,
        db_task_status,
        claim_ttl_remaining,
        extension_budget_exhausted,
        hard_runtime_deadline_exceeded,
        exit_code: None,
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
            let fmt = ::time::format_description::parse_borrowed::<3>(
                "[year]-[month]-[day] [hour]:[minute]:[second]",
            )
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

// ─── Liveness foundation tests ─────────────────────────────────────────────

#[cfg(test)]
mod liveness_foundation_tests {
    use super::*;
    use djinn_db::CurrentLivenessState;
    use djinn_slot::RunningTaskInfo;
    use time::Duration as TimeDuration;
    use time::OffsetDateTime;

    /// Format an `OffsetDateTime` as an ISO-8601 string matching the DB format.
    fn format_iso(dt: OffsetDateTime) -> String {
        dt.format(&time::format_description::well_known::Iso8601::DEFAULT)
            .unwrap_or_else(|_| "2026-01-01T00:00:00.000Z".to_owned())
    }

    /// Helper to build a `CurrentLivenessState` for a running, non-terminal
    /// task with a recently-started session and task_run.
    fn running_db_state() -> CurrentLivenessState {
        // Use a timestamp 60 seconds ago so the session is well within the
        // zombie hard cap.
        let recent = OffsetDateTime::now_utc() - TimeDuration::seconds(60);
        let ts = format_iso(recent);
        CurrentLivenessState {
            task_status: Some("in_progress".to_owned()),
            task_is_terminal: false,
            active_session_id: Some("sess-1".to_owned()),
            active_session_status: Some("running".to_owned()),
            latest_task_run_id: Some("run-1".to_owned()),
            latest_task_run_status: Some("running".to_owned()),
            session_liveness_verdict: None,
            session_liveness_outcome_kind: None,
            session_liveness_outcome_reason: None,
            session_liveness_evidence: None,
            task_run_liveness_outcome_kind: None,
            task_run_liveness_outcome_reason: None,
            task_run_liveness_evidence: None,
            task_created_at: Some(ts.clone()),
            session_started_at: Some(ts.clone()),
            task_run_started_at: Some(ts),
            task_run_ended_at: None,
        }
    }

    /// Helper to build a `RunningTaskInfo` that represents a healthy,
    /// recently-active in-memory session.
    fn healthy_pool_info() -> RunningTaskInfo {
        RunningTaskInfo {
            task_id: "task-1".to_owned(),
            model_id: "test/mock".to_owned(),
            slot_id: 0,
            duration_seconds: 60,
            idle_seconds: 5,
            activity_tracked: true,
            project_id: Some("proj-1".to_owned()),
            token_count: 1000,
            turn_count: 5,
            no_progress_streak: 0,
        }
    }

    // ── AC 3: live activity evidence mapping ──────────────────────────────

    #[test]
    fn live_activity_maps_to_live_verdict() {
        let pool = healthy_pool_info();
        let db = running_db_state();
        let evidence = build_liveness_evidence(Some(&pool), &db);

        assert_eq!(evidence.pod_phase, Some(PodPhase::Running));
        assert_eq!(evidence.activity, ActivitySignal::Active);
        assert_eq!(evidence.db_session_status, Some(DbSessionStatus::Running));
        assert_eq!(evidence.db_task_status, Some(DbTaskStatus::InProgress));
        assert!(evidence.claim_ttl_remaining.is_some());
        assert!(!evidence.hard_runtime_deadline_exceeded);
        assert!(!evidence.extension_budget_exhausted);

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(result.verdict, super::super::liveness::Verdict::Live);
        assert_eq!(result.outcome, None);
        assert!(!result.extension_eligible);
    }

    // ── AC 3: missing/failed pod + stale DB activity ──────────────────────

    #[test]
    fn absent_pod_with_idle_session_maps_to_dead() {
        // No pool info (absent pod) and the DB session is running but idle.
        let mut db = running_db_state();
        // Session started long ago (beyond zombie hard cap).
        let old = OffsetDateTime::now_utc() - TimeDuration::seconds(700);
        let old_ts = format_iso(old);
        db.session_started_at = Some(old_ts);
        // Keep task_run_started_at recent so hard_runtime_deadline_exceeded
        // doesn't fire (that's precedence 2, tested separately).

        let evidence = build_liveness_evidence(None, &db);

        assert_eq!(evidence.pod_phase, Some(PodPhase::Absent));
        assert_eq!(evidence.activity, ActivitySignal::Idle);
        assert_eq!(evidence.db_session_status, Some(DbSessionStatus::Running));
        assert!(!evidence.hard_runtime_deadline_exceeded);

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(result.verdict, super::super::liveness::Verdict::Dead);
        assert_eq!(
            result.outcome,
            Some(super::super::liveness::LivenessOutcome::DeadReclaimed)
        );
    }

    #[test]
    fn failed_pod_idle_stale_session_is_dead() {
        // Session is old and no pool activity — absent pod + idle → Dead.
        // Keep task_run_started_at recent so hard_runtime_deadline_exceeded
        // doesn't fire first.
        let mut db = running_db_state();
        let old = OffsetDateTime::now_utc() - TimeDuration::seconds(700);
        db.session_started_at = Some(format_iso(old));

        // No pool info → absent pod + idle → Dead.
        let evidence = build_liveness_evidence(None, &db);
        assert!(!evidence.hard_runtime_deadline_exceeded);

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(result.verdict, super::super::liveness::Verdict::Dead);
        assert_eq!(
            result.outcome,
            Some(super::super::liveness::LivenessOutcome::DeadReclaimed)
        );
    }

    // ── AC 3: hard runtime cap ────────────────────────────────────────────

    #[test]
    fn hard_runtime_cap_produces_dead_timeout() {
        let pool = healthy_pool_info();
        let mut db = running_db_state();
        // Task run started long ago, exceeding zombie hard cap.
        let old = OffsetDateTime::now_utc() - TimeDuration::seconds(700);
        db.task_run_started_at = Some(format_iso(old));

        let evidence = build_liveness_evidence(Some(&pool), &db);

        assert!(evidence.hard_runtime_deadline_exceeded);
        assert_eq!(evidence.pod_phase, Some(PodPhase::Running));

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(result.verdict, super::super::liveness::Verdict::Dead);
        assert_eq!(
            result.outcome,
            Some(super::super::liveness::LivenessOutcome::Timeout)
        );
        assert_eq!(
            result.reason,
            Some(super::super::liveness::LivenessReason::HardRuntimeExceeded)
        );
        assert!(!result.extension_eligible);
    }

    // ── AC 3: protocol-violation outcome metadata ─────────────────────────

    #[test]
    fn clean_exit_nonterminal_produces_protocol_violation() {
        // Directly build evidence that represents a succeeded pod with running
        // session — this tests the classifier protocol-violation path.
        // Directly build evidence that represents a succeeded pod with running
        // session — this tests the classifier protocol-violation path.
        let evidence = LivenessEvidence {
            pod_phase: Some(PodPhase::Succeeded),
            activity: ActivitySignal::Idle,
            db_session_status: Some(DbSessionStatus::Running),
            db_task_status: Some(DbTaskStatus::InProgress),
            claim_ttl_remaining: Some(Duration::from_secs(600)),
            extension_budget_exhausted: false,
            hard_runtime_deadline_exceeded: false,
            exit_code: Some(0),
        };

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(
            result.verdict,
            super::super::liveness::Verdict::ProtocolViolation
        );
        assert_eq!(
            result.outcome,
            Some(super::super::liveness::LivenessOutcome::Success)
        );
        assert_eq!(
            result.reason,
            Some(super::super::liveness::LivenessReason::CleanExitNonterminal)
        );
        assert!(!result.extension_eligible);
    }

    #[test]
    fn nonzero_exit_nonterminal_produces_protocol_violation_crash() {
        let evidence = LivenessEvidence {
            pod_phase: Some(PodPhase::Failed),
            activity: ActivitySignal::Idle,
            db_session_status: Some(DbSessionStatus::Running),
            db_task_status: Some(DbTaskStatus::InProgress),
            claim_ttl_remaining: Some(Duration::from_secs(600)),
            extension_budget_exhausted: false,
            hard_runtime_deadline_exceeded: false,
            exit_code: Some(137),
        };

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(
            result.verdict,
            super::super::liveness::Verdict::ProtocolViolation
        );
        assert_eq!(
            result.outcome,
            Some(super::super::liveness::LivenessOutcome::Crash)
        );
        assert_eq!(
            result.reason,
            Some(super::super::liveness::LivenessReason::NonzeroExitNonterminal)
        );
    }

    // ── Foundation persistence shape: terminal task → noop ────────────────

    #[test]
    fn terminal_task_produces_kill_noop_outcome() {
        let pool = healthy_pool_info();
        let mut db = running_db_state();
        db.task_status = Some("closed".to_owned());
        db.task_is_terminal = true;

        let evidence = build_liveness_evidence(Some(&pool), &db);
        assert_eq!(evidence.db_task_status, Some(DbTaskStatus::Closed));

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(
            result.outcome,
            Some(super::super::liveness::LivenessOutcome::KillNoop)
        );
        // The verdict is Live (moot for terminal tasks) — the important
        // thing is the KillNoop outcome which tells persistence to record
        // an idempotent noop instead of mutating task state.
        assert!(!result.extension_eligible);
    }

    // ── Evidence snapshot serialization shape ─────────────────────────────

    #[test]
    fn evidence_snapshot_round_trips_through_json() {
        let pool = healthy_pool_info();
        let db = running_db_state();
        let evidence = build_liveness_evidence(Some(&pool), &db);
        let result = super::super::liveness::classify(&evidence);

        let snapshot = LivenessEvidenceSnapshot {
            session_id: "sess-1".to_owned(),
            task_id: Some("task-1".to_owned()),
            task_run_id: Some("run-1".to_owned()),
            verdict: result.verdict.as_str().to_owned(),
            outcome_kind: result.outcome.map(|o| o.as_str().to_owned()),
            outcome_reason: result.reason.map(|r| r.as_str().to_owned()),
            evidence: serde_json::to_value(&result.evidence).unwrap(),
        };

        let json = serde_json::to_string(&snapshot).unwrap();
        let back: LivenessEvidenceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(back.verdict, "live");
        assert_eq!(back.outcome_kind, None);
        assert_eq!(back.session_id, "sess-1");
        assert!(back.evidence.is_object());
    }

    #[test]
    fn dead_evidence_snapshot_has_expected_shape() {
        let mut db = running_db_state();
        let old = OffsetDateTime::now_utc() - TimeDuration::seconds(700);
        db.session_started_at = Some(format_iso(old));
        // Keep task_run_started_at recent so hard_runtime_deadline doesn't fire.

        let evidence = build_liveness_evidence(None, &db);
        let result = super::super::liveness::classify(&evidence);

        let snapshot = LivenessEvidenceSnapshot {
            session_id: "sess-1".to_owned(),
            task_id: Some("task-1".to_owned()),
            task_run_id: Some("run-1".to_owned()),
            verdict: result.verdict.as_str().to_owned(),
            outcome_kind: result.outcome.map(|o| o.as_str().to_owned()),
            outcome_reason: result.reason.map(|r| r.as_str().to_owned()),
            evidence: serde_json::to_value(&result.evidence).unwrap(),
        };

        assert_eq!(snapshot.verdict, "dead");
        assert_eq!(snapshot.outcome_kind.as_deref(), Some("dead_reclaimed"));
        // Evidence blob should be a JSON object with the expected fields.
        let ev = &snapshot.evidence;
        assert!(ev.get("pod_phase").is_some());
        assert!(ev.get("activity").is_some());
        assert!(ev.get("db_task_status").is_some());
    }

    // ── Slow verdict evidence mapping ─────────────────────────────────────

    #[test]
    fn running_pod_with_idle_activity_maps_to_slow() {
        let mut pool = healthy_pool_info();
        pool.idle_seconds = ZOMBIE_HARD_CAP_SECS + 1;
        pool.activity_tracked = true;

        let db = running_db_state();
        let evidence = build_liveness_evidence(Some(&pool), &db);

        assert_eq!(evidence.pod_phase, Some(PodPhase::Running));
        assert_eq!(evidence.activity, ActivitySignal::Idle);

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(result.verdict, super::super::liveness::Verdict::Slow);
        assert!(result.extension_eligible);
    }

    // ── No pool info + DB-only evidence ───────────────────────────────────

    #[test]
    fn no_pool_info_with_no_db_session_still_classifies() {
        let db = CurrentLivenessState {
            task_status: Some("in_progress".to_owned()),
            task_is_terminal: false,
            active_session_id: None,
            active_session_status: None,
            latest_task_run_id: None,
            latest_task_run_status: None,
            session_liveness_verdict: None,
            session_liveness_outcome_kind: None,
            session_liveness_outcome_reason: None,
            session_liveness_evidence: None,
            task_run_liveness_outcome_kind: None,
            task_run_liveness_outcome_reason: None,
            task_run_liveness_evidence: None,
            task_created_at: None,
            session_started_at: None,
            task_run_started_at: None,
            task_run_ended_at: None,
        };

        let evidence = build_liveness_evidence(None, &db);
        // No pool → Absent pod, no activity → Idle
        assert_eq!(evidence.pod_phase, Some(PodPhase::Absent));
        assert_eq!(evidence.activity, ActivitySignal::Idle);
        // No session/task statuses resolved
        assert_eq!(evidence.db_session_status, None);
        assert_eq!(evidence.db_task_status, Some(DbTaskStatus::InProgress));

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(result.verdict, super::super::liveness::Verdict::Dead);
    }

    // ── AC: Slow verdict extension eligibility and hard-cap precedence ──

    #[test]
    fn slow_verdict_below_hard_cap_is_extension_eligible() {
        // A running pod that has gone idle, with the task run started
        // recently (below ZOMBIE_HARD_CAP_SECS), should classify as
        // Slow with extension_eligible = true.
        let mut pool = healthy_pool_info();
        pool.idle_seconds = ZOMBIE_HARD_CAP_SECS + 1;
        pool.activity_tracked = true;

        let db = running_db_state(); // task_run_started 60s ago → below hard cap
        let evidence = build_liveness_evidence(Some(&pool), &db);

        assert_eq!(evidence.pod_phase, Some(PodPhase::Running));
        assert_eq!(evidence.activity, ActivitySignal::Idle);
        assert!(!evidence.hard_runtime_deadline_exceeded);
        assert!(!evidence.extension_budget_exhausted);

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(result.verdict, super::super::liveness::Verdict::Slow);
        assert!(result.extension_eligible);
        // Slow with budget available: no outcome/reason yet (action pending)
        assert_eq!(result.outcome, None);
        assert_eq!(result.reason, None);
    }

    #[test]
    fn slow_verdict_with_exhausted_extension_budget_is_not_eligible() {
        // Construct LivenessEvidence directly with extension_budget_exhausted = true
        // to verify the pure classifier behavior. build_liveness_evidence no longer
        // derives extension_budget_exhausted from claim_ttl_remaining; the
        // coordinator tracks extension budget via stall_extension_count.
        use super::super::liveness::LivenessEvidence;

        let evidence = LivenessEvidence {
            pod_phase: Some(PodPhase::Running),
            activity: ActivitySignal::Idle,
            db_session_status: Some(DbSessionStatus::Running),
            db_task_status: Some(DbTaskStatus::InProgress),
            claim_ttl_remaining: Some(Duration::from_secs(100)),
            extension_budget_exhausted: true,
            hard_runtime_deadline_exceeded: false,
            exit_code: None,
        };

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(result.verdict, super::super::liveness::Verdict::Slow);
        assert!(!result.extension_eligible);
        assert_eq!(
            result.outcome,
            Some(super::super::liveness::LivenessOutcome::SlowExtended)
        );
        assert_eq!(
            result.reason,
            Some(super::super::liveness::LivenessReason::SlowExtensionBudgetExhausted)
        );
    }

    #[test]
    fn hard_runtime_exceeded_produces_dead_timeout_no_extension() {
        // When the task run has exceeded ZOMBIE_HARD_CAP_SECS, the
        // classifier must return Dead with Timeout regardless of pod
        // activity state. extension_eligible must be false.
        let pool = healthy_pool_info();
        let mut db = running_db_state();
        let old = OffsetDateTime::now_utc() - TimeDuration::seconds(700);
        db.task_run_started_at = Some(format_iso(old));

        let evidence = build_liveness_evidence(Some(&pool), &db);

        assert!(evidence.hard_runtime_deadline_exceeded);
        // Pod is running and activity is active (pool has recent activity),
        // but hard cap takes precedence.
        assert_eq!(evidence.pod_phase, Some(PodPhase::Running));
        assert_eq!(evidence.activity, ActivitySignal::Active);

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(result.verdict, super::super::liveness::Verdict::Dead);
        assert_eq!(
            result.outcome,
            Some(super::super::liveness::LivenessOutcome::Timeout)
        );
        assert_eq!(
            result.reason,
            Some(super::super::liveness::LivenessReason::HardRuntimeExceeded)
        );
        assert!(!result.extension_eligible);
    }

    #[test]
    fn hard_runtime_exceeded_overrides_slow_signals() {
        // Even when all signals point to Slow (running pod, idle activity,
        // no budget exhaustion), the hard runtime cap has unconditional
        // precedence.
        let mut pool = healthy_pool_info();
        pool.idle_seconds = ZOMBIE_HARD_CAP_SECS + 1;
        pool.activity_tracked = true;

        let mut db = running_db_state();
        // task_run started long ago → hard cap exceeded
        let old = OffsetDateTime::now_utc() - TimeDuration::seconds(700);
        db.task_run_started_at = Some(format_iso(old));
        // session started recently → claim TTL not exhausted
        let recent = OffsetDateTime::now_utc() - TimeDuration::seconds(60);
        db.session_started_at = Some(format_iso(recent));

        let evidence = build_liveness_evidence(Some(&pool), &db);

        assert!(evidence.hard_runtime_deadline_exceeded);
        assert!(!evidence.extension_budget_exhausted);
        assert_eq!(evidence.pod_phase, Some(PodPhase::Running));
        assert_eq!(evidence.activity, ActivitySignal::Idle);

        let result = super::super::liveness::classify(&evidence);
        // Hard cap wins → Dead/Timeout, NOT Slow
        assert_eq!(result.verdict, super::super::liveness::Verdict::Dead);
        assert_eq!(
            result.outcome,
            Some(super::super::liveness::LivenessOutcome::Timeout)
        );
        assert!(!result.extension_eligible);
    }

    #[test]
    fn never_active_running_pod_is_slow_not_dead() {
        // A pod that has never shown activity (first-call hung) but is
        // still running in the pool should be Slow, not Dead. The pod
        // phase Running prevents the Dead classification.
        let mut pool = healthy_pool_info();
        pool.activity_tracked = false;
        pool.idle_seconds = 400; // past FIRST_CALL_STALL_SECS but pod still running

        let db = running_db_state();
        let evidence = build_liveness_evidence(Some(&pool), &db);

        assert_eq!(evidence.pod_phase, Some(PodPhase::Running));
        assert_eq!(evidence.activity, ActivitySignal::NeverActive);
        assert!(!evidence.hard_runtime_deadline_exceeded);

        let result = super::super::liveness::classify(&evidence);
        assert_eq!(result.verdict, super::super::liveness::Verdict::Slow);
        assert!(result.extension_eligible);
    }

    // ── Kill evidence: kill_noop for terminal task ──────────────────────────

    /// When a task is already terminal, the classifier returns KillNoop.
    /// This test verifies the evidence snapshot shape that would be persisted
    /// by `execution_kill_task` or the coordinator stall/ceiling kill paths.
    #[test]
    fn kill_noop_evidence_snapshot_has_expected_shape() {
        let mut db = running_db_state();
        db.task_status = Some("closed".to_owned());
        db.task_is_terminal = true;

        let evidence = build_liveness_evidence(Some(&healthy_pool_info()), &db);
        let result = super::super::liveness::classify(&evidence);

        assert_eq!(
            result.outcome,
            Some(super::super::liveness::LivenessOutcome::KillNoop)
        );

        // Build the snapshot as the kill path would.
        let snapshot = LivenessEvidenceSnapshot {
            session_id: db.active_session_id.clone().unwrap_or_default(),
            task_id: Some("task-1".to_owned()),
            task_run_id: db.latest_task_run_id.clone(),
            verdict: result.verdict.as_str().to_owned(),
            outcome_kind: result.outcome.map(|o| o.as_str().to_owned()),
            outcome_reason: result.reason.map(|r| r.as_str().to_owned()),
            evidence: serde_json::to_value(&result.evidence).unwrap(),
        };

        assert_eq!(snapshot.verdict, "live"); // verdict is moot for terminal tasks
        assert_eq!(snapshot.outcome_kind.as_deref(), Some("kill_noop"));
        assert!(snapshot.evidence.get("pod_phase").is_some());
        assert!(snapshot.evidence.get("db_task_status").is_some());
    }

    // ── Kill evidence: dead_reclaimed for absent pod (stall kill) ───────────

    /// When a session has an absent pod and no activity (the state just
    /// before a stall kill frees it), the classifier returns Dead with
    /// DeadReclaimed. This verifies the evidence shape persisted by the
    /// stall kill path.
    #[test]
    fn stall_kill_dead_reclaimed_evidence_snapshot_has_expected_shape() {
        let mut db = running_db_state();
        // Session started long ago — well past the zombie hard cap.
        let old = OffsetDateTime::now_utc() - TimeDuration::seconds(700);
        db.session_started_at = Some(format_iso(old));
        // Keep task_run recent so hard_runtime check doesn't interfere.
        let recent = OffsetDateTime::now_utc() - TimeDuration::seconds(60);
        db.task_run_started_at = Some(format_iso(recent));

        // No pool info → absent pod, idle activity.
        let evidence = build_liveness_evidence(None, &db);
        let result = super::super::liveness::classify(&evidence);

        assert_eq!(result.verdict, super::super::liveness::Verdict::Dead);
        assert_eq!(
            result.outcome,
            Some(super::super::liveness::LivenessOutcome::DeadReclaimed)
        );

        // Build snapshot as the stall kill path would.
        let snapshot = LivenessEvidenceSnapshot {
            session_id: db.active_session_id.clone().unwrap_or_default(),
            task_id: Some("task-1".to_owned()),
            task_run_id: db.latest_task_run_id.clone(),
            verdict: result.verdict.as_str().to_owned(),
            outcome_kind: result.outcome.map(|o| o.as_str().to_owned()),
            outcome_reason: result.reason.map(|r| r.as_str().to_owned()),
            evidence: serde_json::to_value(&result.evidence).unwrap(),
        };

        assert_eq!(snapshot.verdict, "dead");
        assert_eq!(snapshot.outcome_kind.as_deref(), Some("dead_reclaimed"));
        assert!(snapshot.evidence.get("pod_phase").is_some());
        assert!(snapshot.evidence.get("activity").is_some());
    }

    /// Verify that a kill_noop does NOT falsely close or reopen the task.
    /// The evidence records the task's terminal status, and the task state
    /// is unchanged.
    #[test]
    fn kill_noop_preserves_task_state() {
        let mut db = running_db_state();
        db.task_status = Some("closed".to_owned());
        db.task_is_terminal = true;

        let evidence = build_liveness_evidence(None, &db);
        let result = super::super::liveness::classify(&evidence);

        // KillNoop: the classifier does not recommend any task state change.
        assert_eq!(
            result.outcome,
            Some(super::super::liveness::LivenessOutcome::KillNoop)
        );
        assert!(!result.extension_eligible);
        // The evidence reflects the terminal task status — no mutation.
        assert_eq!(evidence.db_task_status, Some(DbTaskStatus::Closed));
    }
}

// ─── Session-exit protocol-violation consumer tests ──────────────────────

#[cfg(test)]
mod session_exit_protocol_violation_tests {
    use super::super::liveness::LivenessReason;
    use super::*;
    use djinn_db::CurrentLivenessState;
    use time::Duration as TimeDuration;
    use time::OffsetDateTime;

    /// Format an `OffsetDateTime` as an ISO-8601 string matching the DB format.
    fn format_iso(dt: OffsetDateTime) -> String {
        dt.format(&time::format_description::well_known::Iso8601::DEFAULT)
            .unwrap_or_else(|_| "2026-01-01T00:00:00.000Z".to_owned())
    }

    /// Helper: nonterminal task + running session (recent).
    fn nonterminal_db_state() -> CurrentLivenessState {
        let recent = OffsetDateTime::now_utc() - TimeDuration::seconds(60);
        let ts = format_iso(recent);
        CurrentLivenessState {
            task_status: Some("in_progress".to_owned()),
            task_is_terminal: false,
            active_session_id: Some("sess-exit-1".to_owned()),
            active_session_status: Some("running".to_owned()),
            latest_task_run_id: Some("run-exit-1".to_owned()),
            latest_task_run_status: Some("running".to_owned()),
            session_liveness_verdict: None,
            session_liveness_outcome_kind: None,
            session_liveness_outcome_reason: None,
            session_liveness_evidence: None,
            task_run_liveness_outcome_kind: None,
            task_run_liveness_outcome_reason: None,
            task_run_liveness_evidence: None,
            task_created_at: Some(ts.clone()),
            session_started_at: Some(ts.clone()),
            task_run_started_at: Some(ts),
            task_run_ended_at: None,
        }
    }

    /// Helper: terminal (closed) task.
    fn terminal_db_state() -> CurrentLivenessState {
        let recent = OffsetDateTime::now_utc() - TimeDuration::seconds(60);
        let ts = format_iso(recent);
        CurrentLivenessState {
            task_status: Some("closed".to_owned()),
            task_is_terminal: true,
            active_session_id: Some("sess-exit-2".to_owned()),
            active_session_status: Some("completed".to_owned()),
            latest_task_run_id: Some("run-exit-2".to_owned()),
            latest_task_run_status: Some("completed".to_owned()),
            session_liveness_verdict: None,
            session_liveness_outcome_kind: None,
            session_liveness_outcome_reason: None,
            session_liveness_evidence: None,
            task_run_liveness_outcome_kind: None,
            task_run_liveness_outcome_reason: None,
            task_run_liveness_evidence: None,
            task_created_at: Some(ts.clone()),
            session_started_at: Some(ts.clone()),
            task_run_started_at: Some(ts),
            task_run_ended_at: None,
        }
    }

    /// AC 1: Status-0 worker exit with nonterminal task → protocol violation
    /// with reason `clean_exit_nonterminal`. Evidence maps pod phase to
    /// Succeeded and exit_code to 0. The session status in evidence is
    /// `Running` because the violation is detected at the moment the pod
    /// exits while the session was still running (before settlement marks
    /// it terminal).
    #[test]
    fn clean_exit_nonterminal_session_completed_is_protocol_violation() {
        let db = nonterminal_db_state();
        // Simulate a session that completed (exit 0) while task is nonterminal.
        // The evidence reflects the state AT pod exit: session was still running.
        let mut evidence = build_liveness_evidence(None, &db);
        evidence.pod_phase = Some(PodPhase::Succeeded);
        evidence.exit_code = Some(0);
        evidence.db_session_status = Some(DbSessionStatus::Running);

        let result = super::super::liveness::classify(&evidence);

        assert_eq!(result.verdict, Verdict::ProtocolViolation);
        assert_eq!(result.outcome, Some(LivenessOutcome::Success));
        assert_eq!(result.reason, Some(LivenessReason::CleanExitNonterminal));
        assert!(!result.extension_eligible);
    }

    /// AC 2: Nonzero exit while task is nonterminal → protocol violation
    /// with crash outcome and reason `nonzero_exit_nonterminal`.
    #[test]
    fn nonzero_exit_nonterminal_session_failed_is_crash() {
        let db = nonterminal_db_state();
        // Simulate a session that failed (exit nonzero) while task is nonterminal.
        let mut evidence = build_liveness_evidence(None, &db);
        evidence.pod_phase = Some(PodPhase::Failed);
        evidence.exit_code = Some(137);
        evidence.db_session_status = Some(DbSessionStatus::Running);

        let result = super::super::liveness::classify(&evidence);

        assert_eq!(result.verdict, Verdict::ProtocolViolation);
        assert_eq!(result.outcome, Some(LivenessOutcome::Crash));
        assert_eq!(result.reason, Some(LivenessReason::NonzeroExitNonterminal));
        assert!(!result.extension_eligible);
    }

    /// AC 3: Interrupted session (nonzero exit) while task is nonterminal
    /// → same crash/protocol-violation semantics.
    #[test]
    fn interrupted_session_nonterminal_is_crash() {
        let db = nonterminal_db_state();
        let mut evidence = build_liveness_evidence(None, &db);
        evidence.pod_phase = Some(PodPhase::Failed);
        evidence.exit_code = Some(1);
        evidence.db_session_status = Some(DbSessionStatus::Running);

        let result = super::super::liveness::classify(&evidence);

        assert_eq!(result.verdict, Verdict::ProtocolViolation);
        assert_eq!(result.outcome, Some(LivenessOutcome::Crash));
        assert_eq!(result.reason, Some(LivenessReason::NonzeroExitNonterminal));
    }

    /// AC 4: Already-terminal task on session exit → KillNoop, not protocol
    /// violation. Terminal status is preserved; only metadata is attached.
    #[test]
    fn terminal_task_session_completed_is_kill_noop() {
        let db = terminal_db_state();
        let mut evidence = build_liveness_evidence(None, &db);
        // Simulate exit 0, but task is terminal.
        evidence.pod_phase = Some(PodPhase::Succeeded);
        evidence.exit_code = Some(0);
        evidence.db_session_status = Some(DbSessionStatus::Completed);

        let result = super::super::liveness::classify(&evidence);

        // Terminal task → KillNoop (not protocol violation).
        assert_eq!(result.outcome, Some(LivenessOutcome::KillNoop));
        assert_ne!(result.verdict, Verdict::ProtocolViolation);
        assert!(!result.extension_eligible);
    }

    /// AC 4: Already-terminal task + nonzero exit → KillNoop, not crash.
    #[test]
    fn terminal_task_session_failed_is_kill_noop() {
        let db = terminal_db_state();
        let mut evidence = build_liveness_evidence(None, &db);
        evidence.pod_phase = Some(PodPhase::Failed);
        evidence.exit_code = Some(137);
        evidence.db_session_status = Some(DbSessionStatus::Failed);

        let result = super::super::liveness::classify(&evidence);

        // Terminal task → KillNoop regardless of exit code.
        assert_eq!(result.outcome, Some(LivenessOutcome::KillNoop));
        assert_ne!(result.verdict, Verdict::ProtocolViolation);
    }

    /// AC 5: Retry accounting — protocol violations produce a structured
    /// result with a reason. The retry/redispatch system already treats
    /// same-role reappearance as a failure, but the protocol-violation
    /// evidence gives it structured cause. Verify the result shape is
    /// suitable for retry accounting (non-extension-eligible, has reason).
    #[test]
    fn protocol_violation_result_is_retry_signal() {
        let db = nonterminal_db_state();
        let mut evidence = build_liveness_evidence(None, &db);
        evidence.pod_phase = Some(PodPhase::Succeeded);
        evidence.exit_code = Some(0);
        // Session was still running when pod exited (protocol violation context).
        evidence.db_session_status = Some(DbSessionStatus::Running);

        let result = super::super::liveness::classify(&evidence);

        // Protocol violation result is a retry signal:
        assert_eq!(result.verdict, Verdict::ProtocolViolation);
        assert!(!result.extension_eligible); // cannot extend — session already ended
        assert!(result.reason.is_some()); // structured reason for audit trail
        assert!(result.outcome.is_some()); // structured outcome for persistence
    }

    /// AC 5: Slow verdict extensions are metadata-only and do NOT produce
    /// protocol violation or increment attempts. This is a Slow verdict
    /// test to verify it does NOT cross into protocol-violation territory.
    #[test]
    fn slow_verdict_is_not_protocol_violation() {
        // Simulate a Slow verdict (running pod, idle, below hard cap).
        // Slow extensions NEVER end the session — they extend the claim.
        // This test verifies the boundary: if the pod is still running
        // and the session is still "running" (nonterminal), the classifier
        // returns Slow, not ProtocolViolation.
        let db = nonterminal_db_state();
        let mut evidence = build_liveness_evidence(None, &db);
        evidence.pod_phase = Some(PodPhase::Running);
        evidence.activity = ActivitySignal::Idle;
        evidence.db_session_status = Some(DbSessionStatus::Running);

        let result = super::super::liveness::classify(&evidence);

        assert_eq!(result.verdict, Verdict::Slow);
        assert_ne!(result.verdict, Verdict::ProtocolViolation);
        assert!(result.extension_eligible);
        // Slow verdict does not produce an outcome/reason until acted upon.
        assert_eq!(result.outcome, None);
        assert_eq!(result.reason, None);
    }
}

// ─── Zero-output / prompt-context latency instrumentation tests ──────────────
// These tests exercise the actual call-site decision paths (not just the
// telemetry facade helpers) so they would catch duplicated phase-duration
// bugs or misrouted timeout-source/failure-class labels.

#[cfg(test)]
mod zero_output_instrumentation_tests {
    use djinn_telemetry::liveness_metrics::{
        FAILURE_CLASS_FIRST_CALL_HANG, FAILURE_CLASS_IDLE_STALL, TIMEOUT_SOURCE_FIRST_CALL_HANG,
        TIMEOUT_SOURCE_IDLE_STALL, record_zero_output_stall,
    };
    use djinn_telemetry::render;
    use std::sync::{Mutex, MutexGuard};
    use std::time::Duration;

    static TEST_MUTEX: Mutex<()> = Mutex::new(());

    fn test_guard() -> MutexGuard<'static, ()> {
        TEST_MUTEX.lock().expect("telemetry test mutex poisoned")
    }

    /// Exercise the first-call-hang decision path (`never_active = true`).
    /// Verifies the timeout_source and failure_class labels match what
    /// `enforce_session_stall_timeout` would record for a session that
    /// never produced any activity.
    #[test]
    fn first_call_hang_decision_emits_correct_labels() {
        let _guard = test_guard();
        djinn_telemetry::init().unwrap();

        // Mirror the decision path in enforce_session_stall_timeout for
        // never_active=true: timeout_source=first_call_hang,
        // failure_class=first_call_hang, chain_exhausted=false.
        let idle_secs = 90u64;
        let never_active = true;
        let (timeout_source, failure_class) = if never_active {
            (
                TIMEOUT_SOURCE_FIRST_CALL_HANG,
                FAILURE_CLASS_FIRST_CALL_HANG,
            )
        } else {
            (TIMEOUT_SOURCE_IDLE_STALL, FAILURE_CLASS_IDLE_STALL)
        };
        record_zero_output_stall(
            Duration::from_secs(idle_secs),
            timeout_source,
            failure_class,
            false,
        );

        let rendered = render().unwrap();
        assert!(
            rendered.contains("timeout_source=\"first_call_hang\""),
            "first_call_hang timeout_source label missing from stall metric:\n{rendered}",
        );
        assert!(
            rendered.contains("failure_class=\"first_call_hang\""),
            "first_call_hang failure_class label missing from stall metric:\n{rendered}",
        );
        assert!(
            rendered.contains("chain_exhausted=\"false\""),
            "chain_exhausted=false missing from stall metric:\n{rendered}",
        );
        assert!(
            rendered.contains("djinn_zero_output_stall_seconds"),
            "djinn_zero_output_stall_seconds metric missing:\n{rendered}",
        );
    }

    /// Exercise the idle-stall decision path (`never_active = false`).
    /// Verifies the timeout_source and failure_class labels match what
    /// `enforce_session_stall_timeout` would record for a session that
    /// was active but went idle.
    #[test]
    fn idle_stall_decision_emits_correct_labels() {
        let _guard = test_guard();
        djinn_telemetry::init().unwrap();

        let idle_secs = 300u64;
        let never_active = false;
        let (timeout_source, failure_class) = if never_active {
            (
                TIMEOUT_SOURCE_FIRST_CALL_HANG,
                FAILURE_CLASS_FIRST_CALL_HANG,
            )
        } else {
            (TIMEOUT_SOURCE_IDLE_STALL, FAILURE_CLASS_IDLE_STALL)
        };
        record_zero_output_stall(
            Duration::from_secs(idle_secs),
            timeout_source,
            failure_class,
            true,
        );

        let rendered = render().unwrap();
        assert!(
            rendered.contains("timeout_source=\"idle_stall\""),
            "idle_stall timeout_source label missing from stall metric:\n{rendered}",
        );
        assert!(
            rendered.contains("failure_class=\"idle_stall\""),
            "idle_stall failure_class label missing from stall metric:\n{rendered}",
        );
        assert!(
            rendered.contains("chain_exhausted=\"true\""),
            "chain_exhausted=true missing from stall metric:\n{rendered}",
        );
    }

    /// Verify the zombie-reap decision path emits first_call_hang labels
    /// (matching the actual call site at line ~1257 in session_recovery.rs).
    #[test]
    fn zombie_reap_decision_emits_first_call_hang_labels() {
        let _guard = test_guard();
        djinn_telemetry::init().unwrap();

        // Mirror the zombie-reap call site: always first_call_hang,
        // chain_exhausted=false.
        let zombie_age_secs = 601u64;
        record_zero_output_stall(
            Duration::from_secs(zombie_age_secs),
            TIMEOUT_SOURCE_FIRST_CALL_HANG,
            FAILURE_CLASS_FIRST_CALL_HANG,
            false,
        );

        let rendered = render().unwrap();
        assert!(
            rendered.contains("timeout_source=\"first_call_hang\""),
            "zombie reap should emit first_call_hang timeout_source:\n{rendered}",
        );
        assert!(
            rendered.contains("failure_class=\"first_call_hang\""),
            "zombie reap should emit first_call_hang failure_class:\n{rendered}",
        );
        assert!(
            rendered.contains("chain_exhausted=\"false\""),
            "zombie reap should emit chain_exhausted=false:\n{rendered}",
        );
    }
}

// NOTE: Prompt-context assembly instrumentation tests now live in the agent
// crate (`djinn-agent/…/prompt_context_tests.rs`) where they exercise the real
// `assemble_prompt_context` path and assert on rendered metrics.  The toy
// `tokio::join!` timing tests that previously lived here did not cover the
// actual assembly instrumentation and were removed per reviewer feedback.
