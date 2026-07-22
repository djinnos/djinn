// djinn:allow-oversize — park telemetry + quality-strike guard logic pushed file past the byte threshold; split when touched substantively.
use super::super::*;
use super::DispatchOutcome;
use super::admission::lane_under_user_cap;
use super::model_under_user_cap;
use djinn_core::clock::{Clock, SystemClock};
use djinn_core::models::task_attempt::{TaskAttemptLedgerRow, TaskAttemptOutcome};
use djinn_core::models::{ReopenClass, TransitionAction};
#[cfg(not(test))]
use djinn_db::AgentRepository;
use djinn_db::EffectiveCreatorProvenance;
use djinn_db::repositories::task_arbitration::{
    CreateArbitrationParams, TaskArbitrationRecord, TaskArbitrationRepository, TryCreateResult,
    UpdateDispatchLedgerParams,
};
use djinn_db::repositories::task_attempt::TaskAttemptRepository;

#[derive(Clone, Copy)]
pub(crate) struct DispatchStrikeDecision {
    pub exempted: bool,
    pub decision: &'static str,
    pub source: &'static str,
}

impl CoordinatorActor {
    pub(crate) async fn latest_attempt_strike_decision(
        &self,
        task_id: &str,
        role: &str,
    ) -> Option<DispatchStrikeDecision> {
        let attempts = TaskAttemptRepository::new(self.db.clone())
            .list_for_task(task_id)
            .await
            .ok()?;
        let attempt = attempts
            .iter()
            .filter(|attempt| attempt.role == role)
            .find(|attempt| {
                attempt.outcome != TaskAttemptOutcome::Deferred.as_str()
                    && attempt.outcome != TaskAttemptOutcome::AdoptedPr.as_str()
            })?;
        let source = match attempt.outcome.as_str() {
            "spawn_failed" => djinn_telemetry::dispatch::STRIKE_SOURCE_SPAWN_FAILED,
            "crashed" => djinn_telemetry::dispatch::STRIKE_SOURCE_CRASHED,
            _ => djinn_telemetry::dispatch::STRIKE_SOURCE_OTHER_TERMINAL,
        };
        // Environmental exemption: a restart/deploy interruption never books a
        // dispatch strike. Two evidence tuples qualify, both deterministic:
        //   1. environmental_owner_expired — a periodic/startup reap proved the
        //      row's non-NULL owner lease expired beyond threshold.
        //   2. environmental_restart_orphan — a STARTUP reap proved a pre-boot
        //      orphan had no live owner under single-leader (owner NULL, missing,
        //      or a different incarnation than the boot's). The recorded
        //      `boot_incarnation_id` + startup reason is the proof; the owner may
        //      legitimately be NULL, so no owner-field match is required here.
        // Returns the distinct environmental source label when exempt so restart
        // orphans and owner-expiry are counted on separate series.
        let exempt_source = (attempt.outcome == "interrupted")
            .then_some(attempt.summary_json.as_deref())
            .flatten()
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
            .and_then(|evidence| {
                let owner_expired = attempt
                    .dispatch_owner_incarnation_id
                    .as_deref()
                    .is_some_and(|owner| {
                        evidence["failure_class"] == "environmental_owner_expired"
                            && evidence["owner_incarnation_id"] == owner
                            && evidence["owner_classification"] == "expired"
                            && evidence["owner_lease_last_renewed_at"].is_string()
                    });
                let restart_orphan = evidence["failure_class"]
                    == "environmental_restart_orphan"
                    && evidence["reason"] == "startup"
                    && evidence["boot_incarnation_id"].is_string();
                if owner_expired {
                    Some(djinn_telemetry::dispatch::STRIKE_SOURCE_ENVIRONMENTAL_OWNER_EXPIRED)
                } else if restart_orphan {
                    Some(djinn_telemetry::dispatch::STRIKE_SOURCE_ENVIRONMENTAL_RESTART_ORPHAN)
                } else {
                    None
                }
            });
        let exempted = exempt_source.is_some();
        Some(DispatchStrikeDecision {
            exempted,
            decision: if exempted {
                djinn_telemetry::dispatch::STRIKE_DECISION_EXEMPTED
            } else {
                djinn_telemetry::dispatch::STRIKE_DECISION_COUNTED
            },
            source: exempt_source.unwrap_or(source),
        })
    }
}

/// uv3p Part B: what the fleet actually did after the current intervention (or
/// after a human released a prior hold), derived from `task_attempts` rows.
/// Drives the attempted-remediation park gate, the forced model rotation at
/// dispatch (via [`rotation_excluded_models`](PostInterventionHistory::rotation_excluded_models)),
/// and the truthful park reason.
#[derive(Debug, Default)]
pub(crate) struct PostInterventionHistory {
    /// At least one post-intervention session reached `submit_work` (logged a
    /// `work_submitted` activity after the evidence floor). When true the
    /// remediation was genuinely attempted, so a park is legitimate.
    pub any_submitted: bool,
    /// Distinct model labels of post-intervention worker sessions that terminated
    /// pre-submission, in first-seen (chronological) order. Empty when a submit
    /// occurred. Contains model IDs when session lookup succeeds, or outcome
    /// strings as a fallback. Counted against [`NON_ATTEMPT_PARK_THRESHOLD`].
    /// For model-rotation exclusions, use [`rotation_excluded_models()`](PostInterventionHistory::rotation_excluded_models)
    /// which filters to actual model IDs only.
    ///
    /// NOTE: infra-classified outcomes (`timed_out`, `spawn_failed`, `crashed`)
    /// are excluded from this list — they do not count toward the park
    /// escalation threshold. See [`infra_session_labels`].
    pub non_attempt_models: Vec<String>,
    /// `sess <id8> (model)` labels for the truthful park reason.
    pub non_attempt_session_labels: Vec<String>,
    /// Diagnostic labels for infra-classified pre-submission terminal attempts
    /// (`timed_out`, `spawn_failed`, `crashed`). These are excluded from
    /// [`non_attempt_models`] (and thus the park escalation threshold) but
    /// still appear in truthful park/retry diagnostics so operators can see
    /// that infrastructure failures occurred.
    pub infra_session_labels: Vec<String>,
    /// The newest post-intervention `work_submitted` activity is newer than any
    /// rejection/CI-failure evidence — the submission is pending review
    /// (`needs_task_review` / `in_task_review`) and the round is still in flight.
    /// When `true`, the park rung must NOT park: the attempt has not concluded.
    pub submission_pending_review: bool,
    /// ISO-8601 timestamp of the latest post-intervention `work_submitted`
    /// activity. Used for the CI-staleness check: CI evidence whose
    /// `first_seen_at` predates this timestamp is from a prior head SHA and
    /// must not serve as the park-triggering strike.
    pub latest_submission_at: Option<String>,
    /// Class of the most recent reopen after the evidence floor. Determines the
    /// truthful park-reason attribution (merge_queue_failed vs review_rejected
    /// vs infra).
    pub most_recent_reopen_class: ReopenClass,
}

impl PostInterventionHistory {
    /// Model IDs to exclude from post-intervention forced model rotation.
    ///
    /// Filters [`non_attempt_models`] to return only actual `provider/model`
    /// identifiers (those containing `/`), skipping fallback outcome strings
    /// (e.g. "crashed", "timed_out") that appear when the session model lookup
    /// fails and the model ID cannot be resolved from the linked session.
    ///
    /// Use this method — not `non_attempt_models` directly — for dispatch
    /// rotation filtering and arbiter `excluded_models` so the rotation
    /// decision operates on real model IDs rather than attempt outcome labels.
    /// Pending/in-flight attempts are already excluded from `non_attempt_models`
    /// by [`CoordinatorActor::post_intervention_history`], so their models are
    /// never present here.
    pub(crate) fn rotation_excluded_models(&self) -> Vec<String> {
        self.non_attempt_models
            .iter()
            .filter(|m| m.contains('/'))
            .cloned()
            .collect()
    }
}

/// Which kind of remediation task to create for a stuck source task.
///
/// Both kinds create a `Planner remediation [<short_id>]: <title>` review task
/// and block the source on it. They differ in WHO is expected to resolve it:
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RemediationKind {
    /// A Planner is dispatched to auto-remediate (reshape / dedupe / spike). When
    /// it closes, `emit_unblocked_tasks` revives the source automatically.
    Planner,
    /// Repeated automated remediation already failed — this requires a HUMAN.
    /// No Planner (or any agent) is dispatched, so the remediation never
    /// auto-resolves; the source stays held (open + blocked) until a human
    /// closes the remediation task. Idempotent: skipped when the source is
    /// already held by an unresolved blocker.
    ///
    /// Only reachable through an explicit org-policy escape hatch (a tripwire
    /// rule an operator opted into `adjudication = human`) or a legacy-close
    /// compatibility path — the autonomous default is [`Self::PlannerEscalation`].
    HumanReview,
    /// Autonomous **planner-park escalation** (the no-human default for the
    /// second-strike / CI-loop / tripwire-hold rungs). Like [`Self::HumanReview`]
    /// it dispatches NO agent inline and blocks the source until the escalation
    /// closes — but it is a NORMAL, planner-dispatchable `review` task labelled
    /// [`PLANNER_PARK_ESCALATION_LABEL`](crate::roles::PLANNER_PARK_ESCALATION_LABEL),
    /// **not** `human-review-hold`. The coordinator dispatch pass routes it to
    /// the Planner (which owns terminal resolution: decompose + supersede, close
    /// won't-fix, or re-scope + reopen), and closing it runs the same
    /// source-release semantics as a human hold
    /// ([`releases_source_on_close`](crate::roles::releases_source_on_close)):
    /// blocker resolution + `human_review_resolved_at` stamp + `tripwire.hold.released`.
    /// Idempotent: skipped when the source is already held by an unresolved
    /// blocker.
    PlannerEscalation,
}

impl CoordinatorActor {
    fn session_taxonomy_has_durable_artifacts(taxonomy: &serde_json::Value) -> bool {
        taxonomy
            .get("files_changed")
            .and_then(|v| v.as_u64())
            .is_some_and(|count| count > 0)
            || taxonomy
                .get("notes_written")
                .and_then(|v| v.as_u64())
                .is_some_and(|count| count > 0)
    }

    /// Probe a session worktree on disk for uncommitted changes (modified,
    /// staged, or untracked files).  This is the ground-truth signal: it
    /// catches files written via `call_shell`, file moves, and anything the
    /// session-extraction taxonomy (which only counts `write|edit|apply_patch`
    /// tool calls) cannot see.
    ///
    /// Returns `true` if the path exists, opens as a git repo, and reports any
    /// non-clean entry.  Errors and missing paths conservatively return
    /// `false` so we never *promote* a task to the PR flow on a bogus signal —
    /// the in-DB signals (taxonomy, comments) are still consulted as a
    /// fallback.
    pub(crate) fn worktree_has_uncommitted_changes(worktree_path: &std::path::Path) -> bool {
        djinn_git::worktree_is_dirty(worktree_path)
    }

    pub(crate) async fn simple_lifecycle_task_has_durable_artifacts(&self, task_id: &str) -> bool {
        let session_repo = SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let task_run_repo =
            djinn_db::repositories::task_run::TaskRunRepository::new(self.db.clone());

        // Signal 1: real workspace git status (catches shell-driven changes
        // that the tool-call-based extraction in session_extraction.rs misses).
        // Post-refactor we read the workspace path from task_runs rather than
        // sessions (migration 5); task_run_id is NULL for stubbed supervisor
        // runs today, so this silently degrades to the taxonomy signal below.
        if let Ok(Some(workspace)) = task_run_repo.latest_workspace_path_for_task(task_id).await {
            let path = std::path::PathBuf::from(&workspace);
            if Self::worktree_has_uncommitted_changes(&path) {
                tracing::info!(
                    task_id = %task_id,
                    workspace = %workspace,
                    "simple-lifecycle artifact detected: workspace has uncommitted changes"
                );
                return true;
            }
        }

        // Signal 2: session event taxonomy (files_changed / notes_written).
        if let Ok(Some(taxonomy)) = session_repo.latest_event_taxonomy_for_task(task_id).await
            && Self::session_taxonomy_has_durable_artifacts(&taxonomy)
        {
            tracing::info!(
                task_id = %task_id,
                "simple-lifecycle artifact detected: non-zero files_changed/notes_written in taxonomy"
            );
            return true;
        }

        false
    }

    /// Resolve a user model priority using an optional explicit lane override.
    ///
    /// When `effective_lane` is `Some`, the user's model selection for that lane
    /// is used instead of the lane implied by `base_role`. This lets post-
    /// intervention worker dispatches use the plan lane without altering the
    /// `ModelLane::for_role` mapping.
    pub(crate) async fn resolve_user_model_priority_with_lane(
        &self,
        created_by_user_id: Option<&str>,
        base_role: &str,
        effective_lane: Option<djinn_core::models::ModelLane>,
    ) -> Vec<String> {
        #[cfg(test)]
        {
            let _ = created_by_user_id;
            let _ = base_role;
            let _ = effective_lane;
            #[allow(clippy::needless_return)]
            return Vec::new();
        }

        #[cfg(not(test))]
        {
            let Some(uid) = created_by_user_id else {
                return Vec::new();
            };
            let us_repo = djinn_db::UserSettingsRepository::new(self.db.clone());
            let lane = effective_lane
                .unwrap_or_else(|| djinn_core::models::ModelLane::for_role(base_role));
            let models = match us_repo.get(uid).await {
                Ok(Some(s)) => s.lanes.map(|l| l.lane(lane).to_vec()).unwrap_or_default(),
                _ => return Vec::new(),
            };
            if models.is_empty() {
                return Vec::new();
            }

            // Drop any selected model whose provider the creator no longer has
            // connected (own or org-shared) — never trust a stale selection.
            let cred_repo = djinn_provider::repos::CredentialRepository::new(
                self.db.clone(),
                crate::events::event_bus_for(&self.events_tx),
            );
            let credentials = match cred_repo.list_for_user(Some(uid)).await {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let connected = self.catalog.connected_provider_ids(&credentials);
            if connected.is_empty() {
                return Vec::new();
            }

            models
                .into_iter()
                .filter(|m| {
                    let provider = m.split_once('/').map(|(p, _)| p).unwrap_or(m.as_str());
                    connected.contains(provider)
                })
                .collect()
        }
    }

    /// The creator's per-user model selection for the lane matching `base_role`
    /// (plan / implement / review), filtered to providers they still have
    /// connected. `base_role` selects the lane: planner/architect/chat → plan,
    /// worker → implement, reviewer → review, lead/unknown → plan.
    ///
    /// Only consumed by the `#[cfg(not(test))]` dispatch-model fallback in
    /// `resolve_dispatch_models_for_role`; the `#[cfg(test)]` harness stubs that
    /// fallback out, so this wrapper is (correctly) unused in test builds.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) async fn resolve_user_model_priority(
        &self,
        created_by_user_id: Option<&str>,
        base_role: &str,
    ) -> Vec<String> {
        self.resolve_user_model_priority_with_lane(created_by_user_id, base_role, None)
            .await
    }

    /// Resolve a `provider/model` list for a DB role's `model_preference`.
    ///
    /// Looks up the default AgentRole for `(project_id, base_role)`.  If the
    /// role has a `model_preference` string, resolves it against connected
    /// providers (same logic as `resolve_dispatch_models_for_role`) and returns
    /// the matched model IDs.  Returns an empty Vec when:
    ///   - No default role is configured.
    ///   - No `model_preference` is set.
    ///   - The preference cannot be resolved to a connected model.
    ///   - In test builds (always returns empty to keep tests simple).
    pub(in crate::dispatch) async fn resolve_role_model_preference(
        &self,
        project_id: &str,
        base_role: &str,
        created_by_user_id: Option<&str>,
    ) -> Vec<String> {
        #[cfg(test)]
        {
            let _ = (project_id, base_role, created_by_user_id);
            #[allow(clippy::needless_return)]
            return Vec::new();
        }

        #[cfg(not(test))]
        {
            let role_repo = AgentRepository::new(
                self.db.clone(),
                crate::events::event_bus_for(&self.events_tx),
            );
            let db_role = match role_repo
                .get_default_for_base_role(project_id, base_role)
                .await
            {
                Ok(Some(r)) => r,
                Ok(None) => return Vec::new(),
                Err(e) => {
                    tracing::warn!(
                        project_id,
                        base_role,
                        error = %e,
                        "CoordinatorActor: failed to load default role for model_preference"
                    );
                    return Vec::new();
                }
            };

            let preference = match db_role.model_preference.as_deref() {
                Some(p) if !p.trim().is_empty() => p.trim().to_string(),
                _ => return Vec::new(),
            };

            // Resolve `preference` (which may be a bare model name like
            // "claude-opus-4-6" or a full "provider/model" ID) against
            // connected credentials — same resolution path as model priorities.
            let cred_repo = djinn_provider::repos::CredentialRepository::new(
                self.db.clone(),
                crate::events::event_bus_for(&self.events_tx),
            );
            // Validate against the task creator's connected providers (own +
            // org-shared fallback) — never another user's private credential.
            let credentials = match cred_repo.list_for_user(created_by_user_id).await {
                Ok(c) => c,
                Err(_) => return Vec::new(),
            };
            let credential_provider_ids = self.catalog.connected_provider_ids(&credentials);
            if credential_provider_ids.is_empty() {
                return Vec::new();
            }

            // Try to match the preference against every connected provider's model list.
            let mut resolved = Vec::new();
            for provider_id in &credential_provider_ids {
                for model in self.catalog.list_models(provider_id) {
                    let bare = model.id.rsplit('/').next().unwrap_or(&model.id);
                    let full_id = format!("{provider_id}/{}", model.id);
                    if model.id == preference
                        || model.name == preference
                        || bare == preference
                        || full_id == preference
                    {
                        resolved.push(full_id);
                        break;
                    }
                }
                if !resolved.is_empty() {
                    break;
                }
            }

            if !resolved.is_empty() {
                tracing::debug!(
                    project_id,
                    base_role,
                    preference,
                    resolved_model = %resolved[0],
                    "CoordinatorActor: resolved role model_preference"
                );
            }

            resolved
        }
    }

    /// Trigger A: route a stuck worker task to a Planner intervention pass.
    ///
    /// Gates on DB-backed `quality_reopen_count` (excludes `merge_conflict` /
    /// `superseded`) reaching `REOPEN_INTERVENTION_THRESHOLD`. The Planner
    /// decomposes, rescopes, or closes via `dispatch_planner_escalation`.
    ///
    /// Returns `true` when routed (caller skips the worker dispatch).
    ///
    /// Idempotency: marker keyed by raw `reopen_count` (re-arms after
    /// `reset_intervention_counters`). `quality_strikes` stored for audit.
    #[tracing::instrument(
        name = "djinn.dispatch.intervention.trigger",
        skip(self, task),
        fields(task_id = %task.short_id, role = "worker", pass_kind = "trigger_a")
    )]
    pub(crate) async fn maybe_intervene_on_stuck_task(
        &mut self,
        task: &djinn_core::models::Task,
    ) -> bool {
        // Gate on DB-backed quality_strikes (excludes merge_conflict/superseded).
        let quality_strikes: i64 = match self.task_repo().quality_reopen_count(&task.id).await {
            Ok(count) => count,
            Err(e) => {
                // Fail safe: skip intervention on DB errors.
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "CoordinatorActor: quality_reopen_count lookup failed; skipping trigger A this pass"
                );
                return false;
            }
        };

        if quality_strikes < REOPEN_INTERVENTION_THRESHOLD {
            return false;
        }

        let reason = format!(
            "Internal review loop exceeded {REOPEN_INTERVENTION_THRESHOLD} quality strikes \
             without convergence (quality_strikes={quality_strikes}, raw_reopen_count={}). \
             The worker keeps re-attempting but the same acceptance criteria remain unmet. \
             Decide how to unstick this: DECOMPOSE into focused subtasks (carve out the \
             specific unmet criterion), RESCOPE/clarify the acceptance criteria and \
             re-dispatch, or CLOSE if the work is moot/duplicate/already-done.",
            task.reopen_count
        );
        tracing::Span::current().record("attempt", quality_strikes);
        self.route_planner_intervention(task, "worker", &reason, None, quality_strikes)
            .await
    }

    /// Trigger B: route a task cycling on same-role redispatches to a Planner
    /// intervention pass.
    ///
    /// Trigger A only sees loops that pass through `open` (a reviewer rejection
    /// bumps `reopen_count`). A review-cycle livelock never reopens: the task
    /// bounces `needs_task_review → in_progress → needs_task_review`, each
    /// cycle a same-role reappearance counted on
    /// `dispatch_failure_streak` while `reopen_count` stays 0 — so the only
    /// bound was the terminal close at [`MAX_DISPATCH_FAILURES`], which
    /// force-closes a task whose durable worker output may be perfectly fine
    /// (the t9wi/32bk wedge, 2026-06-11). When the streak crosses
    /// [`STREAK_INTERVENTION_THRESHOLD`] with no typed provider failure to
    /// blame (gated by `should_route_cycling_intervention` at the call site),
    /// hand the loop to the Planner for the same decompose / rescope / close
    /// decision instead.
    ///
    /// Shares all of trigger A's machinery — second-strike terminal park,
    /// reopen-count-keyed idempotency marker (stable across a review-cycle
    /// loop, so one intervention per loop), backoff-state clearing — via
    /// [`Self::route_planner_intervention`].
    #[tracing::instrument(
        name = "djinn.dispatch.intervention.trigger",
        skip(self, task),
        fields(task_id = %task.short_id, role = %role, attempt = streak, pass_kind = "trigger_b")
    )]
    pub(crate) async fn maybe_intervene_on_cycling_task(
        &mut self,
        task: &djinn_core::models::Task,
        role: &'static str,
        streak: u32,
    ) -> bool {
        let reason = format!(
            "Task is cycling without converging: {streak} consecutive `{role}` redispatches \
             completed without the task changing status (status `{}`, reopen_count={} — the \
             loop never passes through `open`, so the reopen-based escalation never saw it). \
             Each run finishes and the task lands right back where it was. Decide how to \
             unstick this: DECOMPOSE into focused subtasks, RESCOPE/clarify the acceptance \
             criteria and re-dispatch, or CLOSE if the durable work on the task branch is \
             already sufficient or the task is moot/duplicate.",
            task.status, task.reopen_count
        );
        self.route_planner_intervention(task, role, &reason, None, task.reopen_count)
            .await
    }

    /// Trigger D: consecutive provider-error FAILED sessions without progress.
    ///
    /// A session that dies on a terminal provider/session error — the
    /// poisoned-transcript 400 (an assistant `tool_calls` message replayed
    /// without its tool results), a dead credential, a persistent server fault —
    /// is redispatched and fails identically, riding the escalating cooldown
    /// ladder toward the terminal close at [`MAX_DISPATCH_FAILURES`] with nobody
    /// deciding what to do. The cycling gate (trigger B) excludes provider
    /// faults by design and the stall-cancel escalation only covers
    /// coordinator-initiated stall kills, so these failures had no Planner path.
    ///
    /// Advances the per-task `provider_failure_streak` — sibling of the
    /// `stall_cancel_streak`, reset when the task's status advances (durable
    /// progress) between strikes — and on the
    /// [`FAILURE_ESCALATION_THRESHOLD`]-th consecutive strike routes the task to
    /// a Planner intervention (decompose / rescope / close), clearing its
    /// backoff state so a post-intervention run starts fresh. Returns `true`
    /// when an intervention was routed (the caller skips the ordinary backoff
    /// ladder for this reappearance); `false` while still below threshold.
    ///
    /// Callers gate this on a genuine, non-throttle typed provider failure — a
    /// transient throttle must decay on the cooldown ladder, not escalate.
    pub(crate) async fn maybe_escalate_provider_failure_streak(
        &mut self,
        task: &djinn_core::models::Task,
        role: &'static str,
    ) -> bool {
        let strike_count = {
            let streak = self
                .provider_failure_streak
                .entry(task.id.clone())
                .and_modify(|s| {
                    if s.last_status == task.status {
                        s.count += 1;
                    } else {
                        // Durable status progress between strikes — reset.
                        s.count = 1;
                        s.last_status = task.status.clone();
                    }
                })
                .or_insert_with(|| StallCancelStreak {
                    count: 1,
                    last_status: task.status.clone(),
                });
            streak.count
        };

        if strike_count < FAILURE_ESCALATION_THRESHOLD {
            return false;
        }

        tracing::warn!(
            task_id = %task.short_id,
            role,
            strike_count,
            status = %task.status,
            "CoordinatorActor: consecutive provider-error session failures without status progress — routing to Planner intervention instead of redispatch"
        );
        let intervention_reason = format!(
            "Task failed on {strike_count} consecutive sessions with a terminal \
             provider/session error and no durable status progress between them (status \
             `{}`). The redispatched worker reproduces the same failure each time (e.g. a \
             poisoned resume transcript that the provider rejects, or an unusable \
             credential), so it is being handed to the Planner to decompose, rescope, or \
             close rather than redispatched again.",
            task.status
        );

        // Clear the streak and backoff state so a post-intervention run starts
        // fresh and a re-armed intervention is not double-counted.
        self.provider_failure_streak.remove(&task.id);
        self.dispatch_failure_streak.remove(&task.id);
        self.dispatch_cooldowns.remove(&task.id);
        self.clear_durable_dispatch_backoff_state(
            &task.id,
            Some(&task.short_id),
            "failure_streak_planner_intervention_handoff_clear",
        )
        .await;

        self.route_loop_guard_planner_intervention(&task.id, role, &intervention_reason)
            .await
    }

    /// Trigger C: a worker/reviewer run completed degenerate because the
    /// reply-loop guard saw repeated identical behavior. This is not a provider
    /// fault and not a dispatch failure; route it directly to the same Planner
    /// intervention / second-strike park machinery used by triggers A and B.
    #[tracing::instrument(
        name = "djinn.dispatch.intervention",
        skip(self, reason),
        fields(task_id = %task_id, role = %role, pass_kind = "loop_guard")
    )]
    pub(crate) async fn route_loop_guard_planner_intervention(
        &mut self,
        task_id: &str,
        role: &'static str,
        reason: &str,
    ) -> bool {
        let task = match self.task_repo().get(task_id).await {
            Ok(Some(task)) => task,
            Ok(None) => {
                tracing::warn!(
                    task_id,
                    "CoordinatorActor: loop-guard planner intervention skipped; task not found"
                );
                return false;
            }
            Err(e) => {
                tracing::warn!(
                    task_id,
                    error = %e,
                    "CoordinatorActor: loop-guard planner intervention skipped; task lookup failed"
                );
                return false;
            }
        };

        self.route_planner_intervention(&task, role, reason, None, task.reopen_count)
            .await
    }

    /// uv3p Part B: what the fleet actually did since the current intervention
    /// (or since a human released a prior hold), derived from `task_attempts`
    /// rows — the source of both the park-vs-redispatch decision and the
    /// truthful park reason. Never templated.
    ///
    /// Primary source of truth is `task_attempts` rows newer than the evidence
    /// floor (last intervention or hold release). Activity-log `work_submitted`
    /// and raw session lists are no longer consulted.
    pub(crate) async fn post_intervention_history(
        &self,
        task: &djinn_core::models::Task,
    ) -> PostInterventionHistory {
        // Evidence floor: the later of the last intervention and the last
        // human-review hold resolution. Sessions/strikes before this floor are
        // pre-intervention (or already-adjudicated pre-release) noise, not
        // evidence the CURRENT remediation was attempted.
        //
        // `task_attempts` rows created_at is the dispatch time; `submitted_at`
        // is the worker submit signal time. Both are compared against the floor
        // to classify post-intervention evidence.
        let resolved_at = self
            .task_repo()
            .human_review_resolved_at(&task.id)
            .await
            .ok()
            .flatten();
        let floor = [task.last_intervention_at.clone(), resolved_at]
            .into_iter()
            .flatten()
            .max();
        let Some(floor) = floor else {
            // No intervention has landed yet — nothing counts as post-intervention.
            return PostInterventionHistory::default();
        };

        // Determine the class of the most recent post-intervention reopen so
        // the reason can truthfully attribute the park to the correct trigger.
        // Derives from attempt outcomes rather than the reopen ledger so both
        // signals stay in sync with the attempt substrate.
        let mut most_recent_reopen_class = ReopenClass::Other;

        // Primary source of truth: `task_attempts` rows for this task.
        let attempt_repo = TaskAttemptRepository::new(self.db.clone());
        let all_attempts = match attempt_repo.list_for_task(&task.id).await {
            Ok(attempts) => attempts,
            Err(e) => {
                // Fail safe: without evidence, treat as "attempted" so the
                // caller parks rather than looping — the conservative direction.
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "uv3p: post-intervention task_attempts lookup failed; treating as attempted"
                );
                return PostInterventionHistory {
                    any_submitted: true,
                    most_recent_reopen_class,
                    ..Default::default()
                };
            }
        };

        // Filter to post-floor worker attempts. Guard-only rows (deferred,
        // adopted_pr) are excluded from remediation evidence.
        let post_floor: Vec<_> = all_attempts
            .iter()
            .filter(|a| a.role == "worker" && a.created_at.as_str() > floor.as_str())
            .collect();

        // Track submitted attempts (rows that reached the `submitted` outcome
        // or have a non-null submitted_at). The newest post-floor `submitted`
        // row is in-flight and must not count as failed/non-attempt evidence.
        let any_submitted = post_floor
            .iter()
            .any(|a| a.outcome == "submitted" || a.submitted_at.is_some());
        let latest_submission_at = post_floor
            .iter()
            .filter_map(|a| a.submitted_at.as_deref())
            .max()
            .map(|s| s.to_string());

        if any_submitted {
            // A post-intervention attempt submitted work. Determine whether
            // the submission is still pending review (no newer terminal
            // rejection/CI-failure exists) or has been rejected.
            //
            // `pending` and `submitted` rows are in-flight and do NOT count as
            // terminal evidence. Terminal submitted attempts (`reopened`,
            // `completed`, etc.) count as actual attempted remediation.
            let submission_pending_review = {
                // A newer terminal rejection after the latest submission means
                // the submission concluded (was rejected). If no such terminal
                // exists, the submission is still pending review.
                let has_terminal_rejection_after_submission = post_floor
                    .iter()
                    .filter(|a| a.is_terminal() && a.submitted_at.is_some())
                    .any(|a| {
                        let ts = a.terminal_at.as_deref().unwrap_or(a.created_at.as_str());
                        ts > latest_submission_at.as_deref().unwrap_or("")
                    });
                !has_terminal_rejection_after_submission
            };

            // Derive reopen class from the newest post-floor terminal
            // rejection that was a submitted attempt (i.e. it concluded after
            // submission).
            if let Some(newest_terminal_rejection) = post_floor
                .iter()
                .filter(|a| a.is_terminal() && a.submitted_at.is_some())
                .max_by_key(|a| a.terminal_at.as_deref().unwrap_or(a.created_at.as_str()))
                && let Ok(outcome_enum) = newest_terminal_rejection.outcome_enum()
            {
                most_recent_reopen_class =
                    outcome_to_reopen_class(&outcome_enum).unwrap_or(ReopenClass::Other);
            }

            return PostInterventionHistory {
                any_submitted: true,
                submission_pending_review,
                latest_submission_at,
                most_recent_reopen_class,
                ..Default::default()
            };
        }

        // No submit: enumerate the post-floor worker attempts that terminated
        // pre-submission (crashed, timed_out, cancelled, spawn_failed,
        // loop_guard_tripped, deferred — no `submitted_at`). These contribute
        // to non-attempt model/label tracking for the park rung.
        // `pending` rows are in flight and must not count as failed evidence.
        //
        // Look up sessions for the task so we can resolve the model_id for each
        // pre-submission terminal attempt (the attempt's `session_id` links to
        // the session that carried the model).
        let session_repo = djinn_db::SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let session_model_map: std::collections::HashMap<String, String> =
            match session_repo.list_for_task(&task.id).await {
                Ok(sessions) => {
                    tracing::debug!(
                        task_id = %task.short_id,
                        session_count = sessions.len(),
                        "uv3p: session model map built from task sessions"
                    );
                    sessions
                        .into_iter()
                        .map(|s| (s.id.clone(), s.model_id.clone()))
                        .collect()
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "uv3p: session lookup for model resolution failed; using outcome labels"
                    );
                    std::collections::HashMap::new()
                }
            };

        let mut non_attempt_models: Vec<String> = Vec::new();
        let mut non_attempt_session_labels: Vec<String> = Vec::new();
        let mut infra_session_labels: Vec<String> = Vec::new();
        for attempt in post_floor.iter() {
            if !attempt.is_terminal() {
                // Pending/in-flight: skip.
                continue;
            }
            if attempt.submitted_at.is_some() {
                // Submitted then terminal: this is an attempted remediation,
                // not a non-attempt. (We'd have entered the any_submitted
                // branch above if this existed.)
                continue;
            }
            // Pre-submission terminal: classify and track.
            //
            // Infra outcomes (timed_out, spawn_failed, crashed) are excluded
            // from the park escalation threshold (non_attempt_models) and
            // tracked separately in infra_session_labels for truthful
            // diagnostics. Non-infra pre-submission terminals (cancelled,
            // loop_guard_tripped, etc.) continue to count toward the
            // non-attempt model rotation / park threshold.
            //
            // Resolve the model_id from the linked session when available,
            // falling back to the attempt outcome for park-reason display.
            // `rotation_excluded_models()` filters this list to actual model
            // IDs so the dispatch rotation block only excludes real models.
            let model_label = attempt
                .session_id
                .as_deref()
                .and_then(|sid| session_model_map.get(sid))
                .map(|m| m.as_str())
                .unwrap_or(attempt.outcome.as_str());
            let id8: String = attempt.id.chars().take(8).collect();
            let is_infra = attempt.outcome_enum().is_ok_and(|o| o.is_infra());
            // Emit infra-delta observability: total classified attempts,
            // distinguished by infra-exempt vs quality-strike class.
            djinn_telemetry::infra_delta::increment(
                djinn_telemetry::infra_delta::OUTCOME_TOTAL,
                is_infra,
            );
            if is_infra {
                // Infra: diagnostic-only; excluded from park escalation.
                infra_session_labels.push(format!("attempt {id8} ({model_label})"));
                continue;
            }
            // Non-infra pre-submission terminal: counts toward park threshold
            // and quality-strike classification.
            djinn_telemetry::infra_delta::increment(
                djinn_telemetry::infra_delta::OUTCOME_QUALITY_STRIKE,
                false,
            );
            non_attempt_session_labels.push(format!("attempt {id8} ({model_label})"));
            if !non_attempt_models.contains(&model_label.to_string()) {
                non_attempt_models.push(model_label.to_string());
            }
        }

        PostInterventionHistory {
            any_submitted: false,
            non_attempt_models,
            non_attempt_session_labels,
            infra_session_labels,
            submission_pending_review: false,
            latest_submission_at: None,
            most_recent_reopen_class,
        }
    }

    /// Build a truthful detail sentence for the park reason, branching on the
    /// post-intervention reopen class. For a merge_queue_failed after an
    /// approved submission, explain that the post-intervention work was approved
    /// but failed the merge-queue full suite. For review_rejected, keep the
    /// existing AC-focused phrasing. Optionally note PR-head CI that shows
    /// passing-with-skips so the green badge is not misleading.
    pub(crate) fn park_reason_detail(
        task: &djinn_core::models::Task,
        history: &PostInterventionHistory,
    ) -> String {
        let has_pr_head_passing_with_skips = task.ci_status.as_str() == "passing"
            && !task.ci_blocking_required_check_names.is_empty();
        let skip_note = if has_pr_head_passing_with_skips {
            " Note: the PR-head CI status currently shows passing, but some required checks are \
             skipped on PR heads (they run only in the merge queue); that green badge does not \
             reflect the merge-queue failure."
        } else {
            ""
        };

        if history.any_submitted {
            match history.most_recent_reopen_class {
                djinn_core::models::ReopenClass::MergeQueueFailed => {
                    let fingerprint = task
                        .ci_failure_fingerprint
                        .as_deref()
                        .filter(|f| !f.is_empty())
                        .map(|f| format!(" (fingerprint: {f})"))
                        .unwrap_or_default();
                    let checks = task
                        .ci_blocking_required_check_names
                        .trim()
                        .strip_prefix('[')
                        .and_then(|s| s.strip_suffix(']'))
                        .map(|s| s.to_string())
                        .filter(|s| !s.is_empty())
                        .unwrap_or_else(|| "unknown check(s)".to_string());
                    format!(
                        "The post-intervention remediation WAS attempted and approved by review, \
                         but the merge-queue full suite failed on {checks}{fingerprint}, so \
                         re-dispatching would only loop again.{skip_note}"
                    )
                }
                djinn_core::models::ReopenClass::Infra => {
                    // Infra-classified submitted terminal: the attempt reached
                    // submission but ended in an infrastructure/provider failure
                    // (timed_out/crashed/spawn_failed). This is NOT a worker
                    // quality strike, so it must not use the AC phrasing.
                    format!(
                        "The post-intervention attempt submitted work but the session ended in an \
                         infrastructure/provider failure (timed_out, crashed, or spawn_failed), \
                         which is excluded from quality-strike counts. Re-dispatching targets \
                         healthy infrastructure rather than penalizing the task.{skip_note}"
                    )
                }
                _ => {
                    // review_rejected / other quality strikes keep the existing AC phrasing.
                    format!(
                        "The post-intervention remediation WAS attempted — at least one session \
                         submitted work after the planner reshaped the scope — but the acceptance \
                         criteria still did not pass, so re-dispatching would only loop again.{skip_note}"
                    )
                }
            }
        } else {
            // Non-submitted branch. Infra-only histories (no non-infra
            // pre-submission terminals) should not use the generic "never
            // converged" phrasing because the failures were infra, not worker
            // quality. Instead, attribute them truthfully to infrastructure.
            if history.non_attempt_session_labels.is_empty()
                && !history.infra_session_labels.is_empty()
            {
                return format!(
                    "The post-intervention attempt(s) ended in infrastructure/provider failures \
                     ({}) — timed_out, crashed, or spawn_failed — which are excluded from \
                     quality-strike counts. Re-dispatching targets healthy infrastructure rather \
                     than penalizing the task.",
                    history.infra_session_labels.join(", "),
                );
            }
            // Mixed or non-infra pre-submission terminals: existing phrasing
            // with an infra diagnostic suffix appended so operators can see
            // infrastructure failures, even though infra outcomes are excluded
            // from the park escalation threshold count above.
            let infra_note = if history.infra_session_labels.is_empty() {
                String::new()
            } else {
                format!(
                    " Additionally, {} infrastructure/provider attempt(s) ({}) terminated \
                     pre-submission (excluded from quality-strike counts as infra failures).",
                    history.infra_session_labels.len(),
                    history.infra_session_labels.join(", "),
                )
            };
            format!(
                "{} post-intervention session(s) terminated pre-submission across models {} — \
                 the remediation never converged despite forced model rotation, so re-dispatching \
                 would only loop again.{infra_note}",
                history.non_attempt_session_labels.len(),
                history.non_attempt_models.join(", "),
            )
        }
    }

    /// uv3p Part B: truthful park reason computed from `history`. Never contains
    /// the templated "same acceptance criteria kept failing" phrasing that five
    /// of five 2026-07-04 parks asserted falsely.
    pub(crate) fn compute_park_reason(
        task: &djinn_core::models::Task,
        history: &PostInterventionHistory,
    ) -> String {
        let detail = Self::park_reason_detail(task, history);
        format!(
            "Auto-parked for human review after {} planner intervention(s) \
             (intervention_count={}, total_reopen_count={}). {detail} The task is held (open + \
             blocked on a human-review remediation task) so it frees the dispatch slot for other \
             ready tasks while its branch and prior work are preserved. A human must resolve the \
             remediation task to release it, or close this task if the work is no longer wanted.",
            MAX_PLANNER_INTERVENTIONS, task.intervention_count, task.total_reopen_count,
        )
    }

    /// uv3p Part B: has a park-redispatch marker already recorded this CI
    /// `fingerprint` as seen for this task? First-occurrence = no such marker.
    async fn park_fingerprint_seen(
        &self,
        task: &djinn_core::models::Task,
        fingerprint: &str,
    ) -> djinn_db::Result<bool> {
        let entries = self
            .task_repo()
            .query_activity(ActivityQuery {
                task_id: Some(task.id.clone()),
                event_type: Some(PARK_REDISPATCH_MARKER.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 200,
                offset: 0,
            })
            .await?;
        Ok(entries.iter().any(|entry| {
            serde_json::from_str::<serde_json::Value>(&entry.payload)
                .ok()
                .and_then(|payload| {
                    payload
                        .get("fingerprint")
                        .and_then(serde_json::Value::as_str)
                        .map(|seen| seen == fingerprint)
                })
                .unwrap_or(false)
        }))
    }

    /// uv3p Part B: record that the park rung declined to park and redispatched.
    /// Non-fatal audit trail (and the durable first-occurrence-fingerprint record).
    async fn record_park_redispatch_marker(
        &self,
        task: &djinn_core::models::Task,
        kind: &str,
        fingerprint: Option<&str>,
        non_attempt_count: usize,
    ) {
        let payload = serde_json::json!({
            "kind": kind,
            "fingerprint": fingerprint,
            "non_attempt_count": non_attempt_count,
            "intervention_count": task.intervention_count,
        })
        .to_string();
        if let Err(e) = self
            .task_repo()
            .log_activity(
                Some(&task.id),
                "coordinator",
                "system",
                PARK_REDISPATCH_MARKER,
                &payload,
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "uv3p: failed to record park-redispatch marker"
            );
        }
    }

    fn arbitration_deadline_has_expired(record: &TaskArbitrationRecord) -> bool {
        record
            .deadline_at
            .as_deref()
            .and_then(|deadline| {
                time::OffsetDateTime::parse(
                    deadline,
                    &time::format_description::well_known::Rfc3339,
                )
                .ok()
            })
            .is_some_and(|deadline| deadline < time::OffsetDateTime::now_utc())
    }

    fn arbiter_deadline_expired_dossier(
        task: &djinn_core::models::Task,
        hold_cycle: i32,
        record: &TaskArbitrationRecord,
    ) -> serde_json::Value {
        serde_json::json!({
            "kind": "arbiter_deadline_expired",
            "cause": "arbiter_deadline_expired",
            "summary": format!(
                "Arbitration deadline expired for hold cycle {}; auto-parking behind HumanReview.",
                hold_cycle,
            ),
            "task_id": task.short_id,
            "task_uuid": task.id,
            "hold_cycle": hold_cycle,
            "deadline_at": record.deadline_at,
            "decision_failure_count": record.decision_failure_count,
            "infra_retry_count": record.infra_retry_count,
            "mirror_head_sha": record.mirror_head_sha,
            "github_head_sha": record.github_head_sha,
            "pr_url": record.pr_url,
            "failing_ci_job_ids": record.failing_ci_job_ids,
        })
    }

    /// Enforce an expired active arbiter deadline before dispatching/re-entering
    /// another Lead arbiter session. This wall-clock lifecycle guard runs from
    /// the normal dispatch pass for tasks already held at `needs_lead_intervention`,
    /// not only from the second-strike route that originally created the row.
    pub(crate) async fn enforce_expired_arbiter_deadline_before_dispatch(
        &mut self,
        task: &djinn_core::models::Task,
    ) -> bool {
        if task.status != "needs_lead_intervention" && task.status != "in_lead_intervention" {
            return false;
        }

        let arbiter_repo = TaskArbitrationRepository::new(self.db.clone());
        let (hold_cycle, record) = match arbiter_repo.resolve_current_hold_cycle(&task.id).await {
            Ok((cycle, Some(record))) => (cycle, record),
            Ok((_cycle, None)) => return false,
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "CoordinatorActor: failed to inspect active arbitration deadline before Lead dispatch"
                );
                return false;
            }
        };

        if !Self::arbitration_deadline_has_expired(&record) {
            return false;
        }

        let dossier = Self::arbiter_deadline_expired_dossier(task, hold_cycle, &record);
        tracing::warn!(
            task_id = %task.short_id,
            hold_cycle,
            deadline_at = ?record.deadline_at,
            "CoordinatorActor: active arbitration deadline expired before Lead dispatch; auto-parking with failure dossier"
        );

        if let Err(e) = arbiter_repo.mark_failed(&task.id, hold_cycle).await {
            tracing::warn!(
                task_id = %task.short_id,
                hold_cycle,
                error = %e,
                "CoordinatorActor: deadline auto-park — failed to mark arbitration failed"
            );
        }
        if let Err(e) = arbiter_repo
            .update_dispatch_ledger(UpdateDispatchLedgerParams {
                task_id: &task.id,
                hold_cycle,
                mirror_head_sha: None,
                github_head_sha: None,
                pr_url: None,
                failing_ci_job_ids: None,
                dossier: Some(&dossier),
                directive: None,
                verification_command: None,
                excluded_models: None,
            })
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                hold_cycle,
                error = %e,
                "CoordinatorActor: deadline auto-park — failed to store deadline dossier"
            );
        }

        // Emit arbiter rollout telemetry: deadline auto-park.
        djinn_telemetry::arbiter::record_park(
            djinn_telemetry::arbiter::PARK_REASON_DEADLINE_EXPIRED,
            djinn_telemetry::arbiter::PARK_OUTCOME_SUCCESS,
        );

        let quality_strikes = self
            .task_repo()
            .quality_reopen_count(&task.id)
            .await
            .unwrap_or(task.intervention_count);
        self.park_source_human_review_with_dossier(
            task,
            &format!("Arbitration deadline expired for hold cycle {hold_cycle}"),
            quality_strikes,
            Some(dossier.clone()),
            &dossier,
        )
        .await
    }
    /// Shared intervention router behind triggers A and B: second-strike
    /// terminal park, idempotency marker keyed by the task's CURRENT
    /// quality strike count, backoff-state clearing, and the Planner escalation
    /// dispatch. Returns `true` when the task was routed (or terminally
    /// parked) — the caller skips its dispatch this pass.
    #[tracing::instrument(
        name = "djinn.dispatch.intervention",
        skip(self, task, reason),
        fields(task_id = %task.short_id, role = %role, pass_kind = "planner_intervention")
    )]
    pub(crate) async fn route_planner_intervention(
        &mut self,
        task: &djinn_core::models::Task,
        role: &'static str,
        reason: &str,
        ci_failure_sections: Option<&str>,
        quality_strikes: i64,
    ) -> bool {
        tracing::Span::current().record("attempt", quality_strikes);
        // Second strike: hold on human-review remediation instead of
        // re-escalating (which just resets and loops). Parked to `open`
        // so the dispatch slot is freed; human resolves the remediation.
        if task.intervention_count >= MAX_PLANNER_INTERVENTIONS {
            // uv3p Part B: attempted-remediation requirement on the park rung.
            // A park declares the current intervention's remediation a failure —
            // but the audits (cgcl/7fj3/nlus) show parks landing 272ms–36s after
            // a rescope, before ANY post-intervention session was ever dispatched.
            // Compute what actually happened since the intervention/hold release
            // and refuse to park on evidence the fleet never tried.
            let history = self.post_intervention_history(task).await;

            // Build the attempt ledger for inclusion in arbiter dossiers.
            // This captures all post-intervention attempt rows (all roles)
            // so operator-facing arbitration payloads consume the durable
            // attempt rows rather than reassembling bespoke history objects.
            let attempt_ledger = self.attempt_ledger_for_task(task).await;

            // CI staleness check (2vxr): CI evidence whose head SHA predates
            // the latest submitted work must not serve as the park-triggering
            // strike. If the CI was first observed before the latest submission,
            // the fingerprint is from a prior head and is stale.
            let ci_stale = match (
                task.ci_first_seen_at.as_deref(),
                history.latest_submission_at.as_deref(),
            ) {
                (Some(ci_ts), Some(sub_ts)) => ci_ts < sub_ts,
                _ => false,
            };

            // First-occurrence CI fingerprint (8y3q): a park-triggering strike
            // whose failure_fingerprint is brand new on this task deserves exactly
            // one remediation before any park — "re-dispatching would only loop"
            // is unfounded against a novel failure (8y3q's fix was one token).
            // Skip the fingerprint check when CI evidence is stale (from a prior
            // head SHA) — it cannot serve as a park-triggering strike.
            if !ci_stale
                && let Some(fingerprint) = task
                    .ci_failure_fingerprint
                    .as_deref()
                    .filter(|f| !f.is_empty())
            {
                match self.park_fingerprint_seen(task, fingerprint).await {
                    Ok(false) => {
                        self.record_park_redispatch_marker(
                            task,
                            "first_occurrence_fingerprint",
                            Some(fingerprint),
                            history.non_attempt_models.len(),
                        )
                        .await;
                        tracing::warn!(
                            task_id = %task.short_id,
                            fingerprint,
                            "uv3p: human-park rung declined to park — first-occurrence CI fingerprint; dispatching one remediation before any park"
                        );
                        return false;
                    }
                    Ok(true) => {}
                    Err(e) => {
                        // Fail safe toward the (unchanged) attempted-remediation
                        // gate below rather than silently parking.
                        tracing::warn!(
                            task_id = %task.short_id,
                            error = %e,
                            "uv3p: park fingerprint-seen check failed; proceeding to attempted-remediation gate"
                        );
                    }
                }
            }

            // Attempted-remediation gate: park only when the remediation was
            // actually attempted (a post-intervention submit) OR enough distinct
            // models have terminated pre-submission to prove rotation won't help.
            // Below the bound, redispatch with forced model rotation instead of
            // consuming the final strike (dispatch-time exclusion in
            // task_dispatch.rs drops the models that just failed).
            if !history.any_submitted
                && history.non_attempt_models.len() < NON_ATTEMPT_PARK_THRESHOLD
            {
                self.record_park_redispatch_marker(
                    task,
                    "no_attempted_remediation",
                    None,
                    history.non_attempt_models.len(),
                )
                .await;
                tracing::warn!(
                    task_id = %task.short_id,
                    intervention_count = task.intervention_count,
                    non_attempt_models = ?history.non_attempt_models,
                    "uv3p: human-park rung declined to park — no post-intervention session reached submit_work yet; redispatching with forced model rotation instead of parking"
                );
                return false;
            }

            // Submission-pending-review guard (2vxr): a post-intervention session
            // submitted work that hasn't been reviewed/rejected yet — the round is
            // still in flight. CI evidence from a head SHA older than the submission
            // (mirror-vs-GitHub staleness) must not serve as the park-triggering
            // strike. If the task is in needs_task_review/in_task_review with no
            // rejection newer than the submission, do not park.
            if history.any_submitted && history.submission_pending_review {
                self.record_park_redispatch_marker(task, "submission_pending_review", None, 0)
                    .await;
                tracing::warn!(
                    task_id = %task.short_id,
                    task_status = %task.status,
                    "uv3p: human-park rung declined to park — newest post-intervention \
                     submission is pending review ({}); the round is still in flight",
                    task.status,
                );
                return false;
            }

            // Truthful park reason computed from actual post-intervention
            // history — never the templated "same acceptance criteria kept
            // failing" text when zero post-intervention rounds occurred.
            let reason = Self::compute_park_reason(task, &history);

            // Arbiter-first routing for the current hold cycle. If the cycle is
            // already unconsumed, the Lead arbiter is already in flight; if a
            // new unconsumed row can be created, dispatch the source to the
            // Lead arbiter.  On any arbitration/hold-cycle DB uncertainty or
            // a consumed/failed cycle, fail closed to the human-review hold
            // path with a structured re-entry dossier.
            let arbiter_repo = TaskArbitrationRepository::new(self.db.clone());
            let hold_cycle = match arbiter_repo.resolve_current_hold_cycle(&task.id).await {
                Ok((cycle, Some(existing))) => 'unconsumed: {
                    // Self-recovery for a stale unconsumed row: when the row's
                    // directive already records a terminal decision, the
                    // arbiter DID run and decide — the row survived unconsumed
                    // only because the decision path failed to mark it (the
                    // approve/approve_conflict path historically never called
                    // `mark_consumed`). Treating it as "arbiter in flight"
                    // wedges the task until the 24h arbitration deadline
                    // (incident lre2, 2026-07-16: approve → pr_draft →
                    // merge-conflict reopen → every tick logged "arbiter
                    // already in flight" with no live arbiter session).
                    // Consume the stale row and open a fresh hold cycle.
                    // "reopen" (monitored reopen) is excluded: that decision
                    // keeps the row unconsumed by design until
                    // `complete_monitored_reopen`.
                    let stale_decision = existing
                        .directive
                        .as_ref()
                        .and_then(|directive| directive.get("decision"))
                        .and_then(serde_json::Value::as_str)
                        .filter(|decision| {
                            matches!(
                                *decision,
                                "approve" | "approve_conflict" | "park" | "supersede"
                            )
                        });
                    if let Some(decision) = stale_decision {
                        match arbiter_repo.mark_consumed(&task.id, cycle).await {
                            Ok(_) => {
                                tracing::warn!(
                                    task_id = %task.short_id,
                                    hold_cycle = cycle,
                                    decision,
                                    "CoordinatorActor: second-strike — unconsumed arbitration row \
                                     already carries a terminal decision; self-consumed the stale \
                                     row and opening a fresh hold cycle"
                                );
                                break 'unconsumed cycle.saturating_add(1);
                            }
                            Err(e) => {
                                tracing::warn!(
                                    task_id = %task.short_id,
                                    hold_cycle = cycle,
                                    error = %e,
                                    "CoordinatorActor: second-strike — failed to self-consume stale \
                                     arbitration row; falling back to in-flight handling this tick"
                                );
                            }
                        }
                    }
                    // Deadline auto-park: if the arbitration deadline has
                    // expired and no valid decision has consumed the row,
                    // auto-park with a generated failure dossier instead of
                    // dispatching another arbiter.
                    if let Some(ref deadline_str) = existing.deadline_at {
                        let deadline_expired = time::OffsetDateTime::parse(
                            deadline_str,
                            &time::format_description::well_known::Rfc3339,
                        )
                        .map(|d| d < time::OffsetDateTime::now_utc())
                        .unwrap_or(false);
                        if deadline_expired {
                            tracing::warn!(
                                task_id = %task.short_id,
                                hold_cycle = cycle,
                                deadline_at = %deadline_str,
                                "CoordinatorActor: arbitration deadline expired; auto-parking with failure dossier"
                            );

                            // Mark the arbitration as failed.
                            let _ = arbiter_repo
                                .mark_failed(&task.id, cycle)
                                .await
                                .map_err(|e| {
                                    tracing::warn!(
                                        task_id = %task.short_id,
                                        error = %e,
                                        "CoordinatorActor: deadline auto-park — failed to mark arbitration failed"
                                    );
                                    e
                                });

                            // Generate a deadline-failure dossier.
                            let deadline_dossier = serde_json::json!({
                                "kind": "arbiter_deadline_expired",
                                "summary": format!(
                                    "Arbitration deadline expired for hold cycle {}; \
                                     auto-parking behind HumanReview.",
                                    cycle,
                                ),
                                "task_id": task.short_id,
                                "hold_cycle": cycle,
                                "deadline_at": deadline_str,
                                "decision_failure_count": existing.decision_failure_count,
                                "infra_retry_count": existing.infra_retry_count,
                                "mirror_head_sha": existing.mirror_head_sha,
                                "github_head_sha": existing.github_head_sha,
                                "pr_url": existing.pr_url,
                                "failing_ci_job_ids": existing.failing_ci_job_ids,
                            });

                            // Emit arbiter rollout telemetry: deadline park.
                            djinn_telemetry::arbiter::record_park(
                                djinn_telemetry::arbiter::PARK_REASON_DEADLINE_EXPIRED,
                                djinn_telemetry::arbiter::PARK_OUTCOME_SUCCESS,
                            );

                            // Update the arbitration row with the dossier.
                            use djinn_db::repositories::task_arbitration::UpdateDispatchLedgerParams;
                            let _ = arbiter_repo
                                .update_dispatch_ledger(UpdateDispatchLedgerParams {
                                    task_id: &task.id,
                                    hold_cycle: cycle,
                                    mirror_head_sha: None,
                                    github_head_sha: None,
                                    pr_url: None,
                                    failing_ci_job_ids: None,
                                    dossier: Some(&deadline_dossier),
                                    directive: None,
                                    verification_command: None,
                                    excluded_models: None,
                                })
                                .await
                                .map_err(|e| {
                                    tracing::warn!(
                                        task_id = %task.short_id,
                                        error = %e,
                                        "CoordinatorActor: deadline auto-park — failed to update dossier"
                                    );
                                    e
                                });

                            return self
                                .park_source_human_review_with_dossier(
                                    task,
                                    &format!(
                                        "Arbitration deadline expired for hold cycle {}",
                                        cycle
                                    ),
                                    quality_strikes,
                                    Some(deadline_dossier.clone()),
                                    &deadline_dossier,
                                )
                                .await;
                        }
                    }

                    // Decision-failure cap check: if the existing
                    // unconsumed arbitration has already hit the cap,
                    // park instead of dispatching another arbiter.
                    const DECISION_FAILURE_CAP: i32 = 2;
                    if existing.decision_failure_count >= DECISION_FAILURE_CAP {
                        tracing::warn!(
                            task_id = %task.short_id,
                            hold_cycle = cycle,
                            decision_failure_count = existing.decision_failure_count,
                            "CoordinatorActor: decision-failure cap reached at dispatch time; parking"
                        );

                        let cap_dossier = serde_json::json!({
                            "kind": "arbiter_decision_failure_cap",
                            "summary": format!(
                                "Decision-failure cap ({}) reached for hold cycle {}; \
                                 parking behind HumanReview.",
                                existing.decision_failure_count, cycle,
                            ),
                            "task_id": task.short_id,
                            "hold_cycle": cycle,
                            "decision_failure_count": existing.decision_failure_count,
                            "infra_retry_count": existing.infra_retry_count,
                            "deadline_at": existing.deadline_at,
                            "mirror_head_sha": existing.mirror_head_sha,
                            "github_head_sha": existing.github_head_sha,
                            "pr_url": existing.pr_url,
                            "failing_ci_job_ids": existing.failing_ci_job_ids,
                        });

                        // Emit arbiter rollout telemetry: decision-failure cap park.
                        djinn_telemetry::arbiter::record_park(
                            djinn_telemetry::arbiter::PARK_REASON_DECISION_FAILURE_CAP,
                            djinn_telemetry::arbiter::PARK_OUTCOME_SUCCESS,
                        );

                        return self
                            .park_source_human_review_with_dossier(
                                task,
                                &format!("Decision-failure cap reached for hold cycle {}", cycle),
                                quality_strikes,
                                Some(cap_dossier.clone()),
                                &cap_dossier,
                            )
                            .await;
                    }

                    // Monitored-reopen starvation fix (incident v1ej,
                    // 2026-07-17): an unconsumed row whose directive is a
                    // `reopen` decision IS the monitored-reopen contract in
                    // flight — the arbiter already decided, and the one
                    // monitored worker attempt is what must run next.  Routing
                    // "Lead arbiter" here (and then short-circuiting on
                    // `AlreadyExistsUnconsumed`) starves that worker dispatch
                    // on every tick once `intervention_count` reaches this
                    // rung, so the monitored attempt never starts and the task
                    // wedges until the 24h arbitration deadline.  Yield to the
                    // normal dispatch pass instead: it applies the arbiter's
                    // `exclude_models`, injects the directive one-shot
                    // (`mark_directive_injected`), the respawn guard dedupes
                    // while the monitored worker is live, and the supervisor's
                    // terminal hook (`complete_monitored_reopen`) consumes the
                    // row.  The deadline-expiry and decision-failure-cap parks
                    // above still fire first, so an abandoned reopen cannot
                    // yield forever.
                    let is_monitored_reopen = existing
                        .directive
                        .as_ref()
                        .and_then(|directive| directive.get("decision"))
                        .and_then(serde_json::Value::as_str)
                        == Some("reopen");
                    if is_monitored_reopen {
                        tracing::info!(
                            task_id = %task.short_id,
                            hold_cycle = cycle,
                            directive_injected = existing.directive_injected,
                            monitored_reopen_count = existing.monitored_reopen_count,
                            "CoordinatorActor: second-strike — unconsumed arbitration row is a \
                             monitored reopen; yielding to normal dispatch so the monitored \
                             worker attempt can run"
                        );
                        return false;
                    }

                    tracing::info!(
                        task_id = %task.short_id,
                        hold_cycle = cycle,
                        "CoordinatorActor: second-strike — current hold cycle already has an unconsumed arbiter; ensuring Lead arbiter routing"
                    );
                    cycle
                }
                Ok((cycle, None)) => cycle,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: second-strike — failed to resolve current hold cycle; failing closed to human review"
                    );
                    return self
                        .park_source_human_review_with_dossier(
                            task,
                            &reason,
                            quality_strikes,
                            None,
                            &Self::arbiter_failure_dossier(
                                &reason,
                                role,
                                task,
                                &history,
                                &attempt_ledger,
                                None,
                                &serde_json::json!([]),
                            ),
                        )
                        .await;
                }
            };

            let failing_ci_job_ids = self.parse_failing_ci_job_ids(ci_failure_sections);
            let excluded_models = serde_json::json!(history.rotation_excluded_models());
            let dossier = serde_json::json!({
                "reason": reason,
                "role": role,
                "intervention_count": task.intervention_count,
                "total_reopen_count": task.total_reopen_count,
                "reopen_count": task.reopen_count,
                "quality_strikes": quality_strikes,
                "post_intervention_history": {
                    "any_submitted": history.any_submitted,
                    "non_attempt_models": history.non_attempt_models,
                    "non_attempt_session_labels": history.non_attempt_session_labels,
                    "submission_pending_review": history.submission_pending_review,
                    "latest_submission_at": history.latest_submission_at,
                },
                "attempt_ledger": attempt_ledger,
            });
            let directive = serde_json::json!({
                "kind": "lead_arbiter",
                "goal": "Forensic review of the current hold cycle after repeated planner interventions",
                "verification_command": None::<String>,
            });
            let deadline = {
                let now = time::OffsetDateTime::now_utc();
                let future = now + time::Duration::hours(24);
                future
                    .format(&time::format_description::well_known::Rfc3339)
                    .ok()
            };

            // Resolve mirror_head_sha from the latest task attempt carrying
            // one. The Task model does not carry this field; the CI snapshot /
            // task attempt ledger does, and by the park rung the relevant
            // attempt may already be terminal.
            let attempt_repo = TaskAttemptRepository::new(self.db.clone());
            let mirror_head_sha = match attempt_repo.list_for_task(&task.id).await {
                Ok(attempts) => attempts.into_iter().find_map(|a| a.mirror_head_sha),
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: failed to read task attempts for mirror_head_sha; proceeding with None"
                    );
                    None
                }
            };

            let create_result = match arbiter_repo
                .try_create(CreateArbitrationParams {
                    task_id: &task.id,
                    hold_cycle,
                    deadline_at: deadline.as_deref(),
                    mirror_head_sha: mirror_head_sha.as_deref(),
                    github_head_sha: task.ci_head_sha.as_deref(),
                    pr_url: task.pr_url.as_deref(),
                    failing_ci_job_ids: &failing_ci_job_ids,
                    dossier: Some(&dossier),
                    directive: Some(&directive),
                    verification_command: None,
                    excluded_models: &excluded_models,
                })
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: second-strike — failed to create arbitration row; failing closed to human review"
                    );
                    return self
                        .park_source_human_review_with_dossier(
                            task,
                            &reason,
                            quality_strikes,
                            None,
                            &Self::arbiter_failure_dossier(
                                &reason,
                                role,
                                task,
                                &history,
                                &attempt_ledger,
                                mirror_head_sha.as_deref(),
                                &failing_ci_job_ids,
                            ),
                        )
                        .await;
                }
            };

            match create_result {
                TryCreateResult::Created(_) => {
                    // Arbiter dispatch path — fresh arbitration row created.
                    self.dispatch_arbiter_second_strike(
                        task,
                        hold_cycle,
                        quality_strikes,
                        &reason,
                        role,
                        &history,
                        &attempt_ledger,
                        mirror_head_sha.as_deref(),
                        &failing_ci_job_ids,
                    )
                    .await;
                    // Log the arbiter_dispatched outbox payload — only on
                    // initial creation so outbox replay (AlreadyExistsUnconsumed)
                    // does not emit a duplicate activity event.
                    let payload = serde_json::json!({
                        "hold_cycle": hold_cycle,
                        "mirror_head_sha": mirror_head_sha,
                        "github_head_sha": task.ci_head_sha,
                        "pr_url": task.pr_url,
                        "failing_ci_job_ids": failing_ci_job_ids,
                        "reason": reason,
                        "role": role,
                    });
                    let task_repo = self.task_repo();
                    if let Err(e) = task_repo
                        .log_activity(
                            Some(&task.id),
                            "system",
                            "coordinator",
                            "arbiter_dispatched",
                            &payload.to_string(),
                        )
                        .await
                    {
                        tracing::warn!(
                            task_id = %task.short_id,
                            error = %e,
                            "CoordinatorActor: failed to log arbiter_dispatched activity"
                        );
                    }
                    return true;
                }
                TryCreateResult::AlreadyExistsUnconsumed(_) => {
                    // Outbox replay: arbitration row already exists and is
                    // unconsumed — the arbiter is already in flight.  Do NOT
                    // re-run dispatch cleanup or status transition (those were
                    // done on the initial `Created` path).
                    //
                    // Crash-recovery idempotency: if the initial dispatch
                    // succeeded but the `arbiter_dispatched` activity write
                    // was lost (best-effort outbox), re-log it.  If the
                    // activity already exists (normal replay), skip to avoid
                    // duplicate rows.
                    tracing::info!(
                        task_id = %task.short_id,
                        hold_cycle,
                        "CoordinatorActor: second-strike — outbox replay; arbiter already in flight"
                    );
                    let existing_events = self
                        .task_repo()
                        .query_activity(ActivityQuery {
                            task_id: Some(task.id.clone()),
                            event_type: Some("arbiter_dispatched".to_string()),
                            ..ActivityQuery::default()
                        })
                        .await
                        .unwrap_or_default();
                    if existing_events.is_empty() {
                        // Crash recovery: the activity was lost. Re-log.
                        let payload = serde_json::json!({
                            "hold_cycle": hold_cycle,
                            "mirror_head_sha": mirror_head_sha,
                            "github_head_sha": task.ci_head_sha,
                            "pr_url": task.pr_url,
                            "failing_ci_job_ids": failing_ci_job_ids,
                            "reason": reason,
                            "role": role,
                        });
                        let task_repo = self.task_repo();
                        if let Err(e) = task_repo
                            .log_activity(
                                Some(&task.id),
                                "system",
                                "coordinator",
                                "arbiter_dispatched",
                                &payload.to_string(),
                            )
                            .await
                        {
                            tracing::warn!(
                                task_id = %task.short_id,
                                error = %e,
                                "CoordinatorActor: failed to replay arbiter_dispatched activity"
                            );
                        }
                    }
                    return true;
                }
                TryCreateResult::AlreadyExistsConsumed(record)
                | TryCreateResult::AlreadyExistsFailed(record) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        hold_cycle,
                        "CoordinatorActor: second-strike — current hold cycle arbitration already consumed/failed; failing closed to human review"
                    );
                    // Emit arbiter rollout telemetry: consumed reentry park.
                    djinn_telemetry::arbiter::record_park(
                        djinn_telemetry::arbiter::PARK_REASON_CONSUMED_REENTRY,
                        djinn_telemetry::arbiter::PARK_OUTCOME_SUCCESS,
                    );
                    let stored_dossier =
                        record.dossier.clone().or_else(|| record.directive.clone());
                    return self
                        .park_source_human_review_with_dossier(
                            task,
                            &reason,
                            quality_strikes,
                            stored_dossier,
                            &Self::reentry_consumed_dossier(
                                &reason,
                                role,
                                task,
                                &history,
                                &record,
                                &attempt_ledger,
                            ),
                        )
                        .await;
                }
            }
        }

        // Idempotency: keyed by raw reopen_count for re-arm after reset.
        match self
            .planner_intervention_marker_exists(task, task.reopen_count)
            .await
        {
            Ok(true) => return false,
            Ok(false) => {}
            Err(e) => {
                // Fail safe: skip intervention on DB errors.
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "CoordinatorActor: planner-intervention marker check failed; skipping intervention this pass"
                );
                return false;
            }
        }

        // Record the marker BEFORE dispatching to prevent double-fire.
        if let Err(e) = self
            .record_planner_intervention_marker(task, task.reopen_count, quality_strikes)
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "CoordinatorActor: failed to record planner-intervention marker; skipping intervention this pass"
            );
            return false;
        }

        tracing::warn!(
            task_id = %task.short_id,
            role,
            quality_strikes,
            reopen_count = task.reopen_count,
            "CoordinatorActor: stuck task — routing to Planner intervention"
        );

        // Clear the escalating-cooldown backoff state so the Planner-created
        // review task (and any follow-up the Planner makes) isn't shadowed by a
        // stale failure streak attributed to the original task.
        self.dispatch_failure_streak.remove(&task.id);
        self.dispatch_cooldowns.remove(&task.id);
        self.last_dispatched.remove(&task.id);
        self.inflight_dispatches.remove(&task.id);
        self.clear_durable_dispatch_backoff_state(
            &task.id,
            Some(&task.short_id),
            "planner_intervention_handoff_clear",
        )
        .await;

        // Append the merge-queue lane facts (run id, failing checks,
        // same-signature count) when the task's CI snapshot carries a
        // merge-queue rejection, so the Planner dossier explains why a PR with a
        // green head keeps getting dequeued.
        let mq_section = crate::pr_poller::merge_queue_lane_escalation_section(task);
        let combined_sections = match (ci_failure_sections, mq_section.as_deref()) {
            (Some(sections), Some(mq)) if !sections.is_empty() => Some(format!("{sections}\n{mq}")),
            (Some(sections), _) if !sections.is_empty() => Some(sections.to_string()),
            (_, Some(mq)) => Some(mq.to_string()),
            _ => None,
        };
        let enriched_reason = match combined_sections {
            Some(sections) => format!("{reason}\n\n**CI Failure Details:**\n{sections}"),
            None => reason.to_string(),
        };
        self.dispatch_planner_escalation(&task.id, &enriched_reason, &task.project_id)
            .await;
        true
    }

    /// Returns `true` if a `planner_intervention` marker already exists for
    /// `task` at the given raw `reopen_count`.
    async fn planner_intervention_marker_exists(
        &self,
        task: &djinn_core::models::Task,
        reopen_count: i64,
    ) -> djinn_db::Result<bool> {
        let task_repo = self.task_repo();
        let entries = task_repo
            .query_activity(ActivityQuery {
                task_id: Some(task.id.clone()),
                event_type: Some(PLANNER_INTERVENTION_MARKER.to_string()),
                actor_role: Some("system".to_string()),
                project_id: None,
                from_time: None,
                to_time: None,
                limit: 100,
                offset: 0,
            })
            .await?;

        Ok(entries.iter().any(|entry| {
            serde_json::from_str::<serde_json::Value>(&entry.payload)
                .ok()
                .and_then(|payload| {
                    payload
                        .get("reopen_count")
                        .and_then(serde_json::Value::as_i64)
                        .filter(|value| *value == reopen_count)
                        .map(|_| ())
                })
                .is_some()
        }))
    }

    /// Record a `planner_intervention` marker keyed by raw `reopen_count`.
    /// `quality_strikes` stored in audit.
    async fn record_planner_intervention_marker(
        &self,
        task: &djinn_core::models::Task,
        reopen_count: i64,
        quality_strikes: i64,
    ) -> djinn_db::Result<()> {
        let payload = serde_json::json!({
            "reopen_count": reopen_count,
            "quality_strikes": quality_strikes,
        })
        .to_string();

        self.task_repo()
            .log_activity(
                Some(&task.id),
                "coordinator",
                "system",
                PLANNER_INTERVENTION_MARKER,
                &payload,
            )
            .await?;

        Ok(())
    }

    /// Emit the parked-task telemetry metric with strike-class breakdown labels.
    ///
    /// Fetches the task's reopen ledger from the DB to derive
    /// `quality_strikes`, `merge_conflict_reopens`, and `superseded_reopens`.
    /// Falls back to the passed `quality_strikes` hint on DB errors so the
    /// metric is never silently swallowed.
    async fn record_task_parked_metric(
        &self,
        task: &djinn_core::models::Task,
        quality_strikes_hint: i64,
    ) {
        let (quality, merge_conflict, superseded, has_infra) =
            match self.task_repo().recent_reopen_ledger(&task.id, 200).await {
                Ok(ledger) => {
                    let mut quality: i64 = 0;
                    let mut merge_conflict: i64 = 0;
                    let mut superseded: i64 = 0;
                    let mut has_infra = false;
                    for entry in &ledger {
                        match entry.reopen_class {
                            ReopenClass::MergeConflict => merge_conflict += 1,
                            ReopenClass::Superseded => superseded += 1,
                            ReopenClass::Infra => {
                                // Infra (provider/infrastructure-attempt
                                // failures) is excluded from quality-strike
                                // counts, intervention counters, and park
                                // escalation thresholds. It still appears in
                                // diagnostic park/retry reasons via
                                // most_recent_reopen_class.
                                has_infra = true;
                            }
                            _ => {
                                // review_rejected, merge_queue_failed, other
                                // are all quality strikes.
                                quality += 1;
                            }
                        }
                    }
                    (quality, merge_conflict, superseded, has_infra)
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: recent_reopen_ledger failed for park telemetry; \
                         using passed quality_strikes hint"
                    );
                    (quality_strikes_hint, 0i64, 0i64, false)
                }
            };

        djinn_telemetry::task::increment_parked_labeled(
            quality,
            merge_conflict,
            superseded,
            task.reopen_count,
        );

        // Emit infra-delta park observability: was this park decision
        // influenced by infra-classified failures? When `has_infra` is true
        // and quality == 0, the park was infra-only (all failures were infra);
        // otherwise it was driven by quality strikes.
        djinn_telemetry::infra_delta::increment(
            djinn_telemetry::infra_delta::OUTCOME_PARK,
            has_infra && quality == 0,
        );
    }

    /// Build the attempt ledger for a task, suitable for inclusion in arbiter
    /// dossiers.  Returns post-intervention attempt rows (all roles) via
    /// [`TaskAttemptRepository::ledger_for_task_since`], limited to 100 rows.
    /// On error, returns an empty vec so the dossier path remains functional.
    async fn attempt_ledger_for_task(
        &self,
        task: &djinn_core::models::Task,
    ) -> Vec<TaskAttemptLedgerRow> {
        let attempt_repo = TaskAttemptRepository::new(self.db.clone());
        match attempt_repo
            .ledger_for_task_since(
                &task.id,
                None, // all roles — worker, guard, etc.
                task.last_intervention_at.as_deref(),
                100,
            )
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "CoordinatorActor: failed to build attempt ledger for dossier; using empty ledger"
                );
                Vec::new()
            }
        }
    }

    /// Shared pre-dispatch logic for the arbiter second-strike path.
    ///
    /// Clears in-memory/durable backoff state, interrupts running sessions,
    /// and transitions the source to `needs_lead_intervention`.  Returns
    /// `true` on success; fails closed to the human-review park path on any
    /// status-transition error.
    ///
    /// The caller is responsible for the `arbiter_dispatched` outbox event —
    /// this helper deliberately does NOT emit it so `AlreadyExistsUnconsumed`
    /// replay callers can skip the duplicate.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_arbiter_second_strike(
        &mut self,
        task: &djinn_core::models::Task,
        hold_cycle: i32,
        quality_strikes: i64,
        reason: &str,
        role: &str,
        history: &PostInterventionHistory,
        attempt_ledger: &[TaskAttemptLedgerRow],
        mirror_head_sha: Option<&str>,
        failing_ci_job_ids: &serde_json::Value,
    ) -> bool {
        tracing::warn!(
            task_id = %task.short_id,
            hold_cycle,
            intervention_count = task.intervention_count,
            total_reopen_count = task.total_reopen_count,
            reopen_count = task.reopen_count,
            quality_strikes,
            "CoordinatorActor: second-strike — dispatching Lead arbiter for current hold cycle"
        );
        // Clear streak/cooldown so the hold isn't shadowed by stale
        // backoff state.
        self.dispatch_failure_streak.remove(&task.id);
        self.dispatch_cooldowns.remove(&task.id);
        self.last_dispatched.remove(&task.id);
        self.inflight_dispatches.remove(&task.id);
        self.clear_durable_dispatch_backoff_state(
            &task.id,
            Some(&task.short_id),
            "planner_second_strike_arbiter_dispatch",
        )
        .await;
        // Interrupt any running session for this task so parking it
        // actually frees the dispatch slot.
        let session_repo = djinn_db::SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        if let Err(e) = session_repo.interrupt_running_for_task(&task.id).await {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "CoordinatorActor: failed to interrupt running sessions while dispatching Lead arbiter"
            );
        }
        // The arbiter entry contract is explicit: the source task
        // must be in `needs_lead_intervention` after this path. If
        // it is already actively running a Lead intervention, move
        // it back to the queued Lead status; otherwise use the
        // widened Escalate transition. Fail closed if either
        // transition cannot be applied.
        if task.status != "needs_lead_intervention" {
            let task_repo = self.task_repo();
            let transition_action = if task.status == "in_lead_intervention" {
                TransitionAction::LeadInterventionRelease
            } else {
                TransitionAction::Escalate
            };
            if let Err(e) = task_repo
                .transition(
                    &task.id,
                    transition_action,
                    "system",
                    "coordinator",
                    Some(reason),
                    None,
                )
                .await
            {
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    status = %task.status,
                    "CoordinatorActor: failed to transition source to \
                     needs_lead_intervention; failing closed to human review"
                );
                return self
                    .park_source_human_review_with_dossier(
                        task,
                        reason,
                        quality_strikes,
                        None,
                        &Self::arbiter_failure_dossier(
                            reason,
                            role,
                            task,
                            history,
                            attempt_ledger,
                            mirror_head_sha,
                            failing_ci_job_ids,
                        ),
                    )
                    .await;
            }
        }
        true
    }

    fn arbiter_failure_dossier(
        base_reason: &str,
        role: &str,
        task: &djinn_core::models::Task,
        history: &PostInterventionHistory,
        attempt_ledger: &[TaskAttemptLedgerRow],
        mirror_head_sha: Option<&str>,
        failing_ci_job_ids: &serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "kind": "arbiter_failure_dossier",
            "summary": "Arbitration state could not be read or written for this hold cycle; the task is held on human review as a fail-closed fallback.",
            "base_reason": base_reason,
            "role": role,
            "intervention_count": task.intervention_count,
            "total_reopen_count": task.total_reopen_count,
            "reopen_count": task.reopen_count,
            "post_intervention_history": {
                "any_submitted": history.any_submitted,
                "non_attempt_models": history.non_attempt_models,
                "non_attempt_session_labels": history.non_attempt_session_labels,
                "submission_pending_review": history.submission_pending_review,
                "latest_submission_at": history.latest_submission_at,
            },
            "attempt_ledger": attempt_ledger,
            "mirror_head_sha": mirror_head_sha,
            "github_head_sha": task.ci_head_sha,
            "pr_url": task.pr_url,
            "failing_ci_job_ids": failing_ci_job_ids,
        })
    }

    fn reentry_consumed_dossier(
        base_reason: &str,
        role: &str,
        task: &djinn_core::models::Task,
        history: &PostInterventionHistory,
        record: &TaskArbitrationRecord,
        attempt_ledger: &[TaskAttemptLedgerRow],
    ) -> serde_json::Value {
        serde_json::json!({
            "kind": "arbiter_consumed_dossier",
            "summary": format!(
                "Arbitration for hold cycle {} was already {} when re-entry was attempted; the task is held on human review.",
                record.hold_cycle,
                record.state,
            ),
            "base_reason": base_reason,
            "role": role,
            "hold_cycle": record.hold_cycle,
            "arbitration_state": record.state,
            "decision_failure_count": record.decision_failure_count,
            "infra_retry_count": record.infra_retry_count,
            "mirror_head_sha": record.mirror_head_sha,
            "github_head_sha": record.github_head_sha,
            "pr_url": record.pr_url,
            "failing_ci_job_ids": record.failing_ci_job_ids,
            "consumed_at": record.consumed_at,
            "intervention_count": task.intervention_count,
            "total_reopen_count": task.total_reopen_count,
            "reopen_count": task.reopen_count,
            "post_intervention_history": {
                "any_submitted": history.any_submitted,
                "non_attempt_models": history.non_attempt_models,
                "non_attempt_session_labels": history.non_attempt_session_labels,
                "submission_pending_review": history.submission_pending_review,
                "latest_submission_at": history.latest_submission_at,
            },
            "attempt_ledger": attempt_ledger,
        })
    }

    /// Structured human-review hold path that prefers a stored arbitration
    /// dossier when available, otherwise uses a generated dossier from actual
    /// post-intervention history. Never falls back to the old static repeated-AC
    /// template.
    pub(crate) async fn park_source_human_review_with_dossier(
        &mut self,
        task: &djinn_core::models::Task,
        reason: &str,
        quality_strikes: i64,
        stored_dossier: Option<serde_json::Value>,
        generated_dossier: &serde_json::Value,
    ) -> bool {
        let dossier = stored_dossier.unwrap_or_else(|| generated_dossier.clone());
        let mut enriched_reason = reason.to_string();
        if let Ok(dossier_text) = serde_json::to_string_pretty(&dossier) {
            enriched_reason.push_str("\n\nArbiter re-entry / failure dossier:\n");
            enriched_reason.push_str(&dossier_text);
        }
        self.park_source_human_review(task, &enriched_reason, quality_strikes)
            .await
    }

    /// Count of prior held-remediation blockers (human-review hold OR
    /// planner-park escalation — see
    /// [`releases_source_on_close`](crate::roles::releases_source_on_close)) on
    /// `source_task_id`, including CLOSED ones.
    ///
    /// Each autonomous escalation adds exactly one blocker that is never
    /// removed (blockers persist across close), so this is the number of
    /// escalation rounds already spent on the source — the strike counter for
    /// the autonomous-escalation ceiling. Fail-open: returns 0 on any query
    /// error (the ceiling only ever GATES a fresh escalation, so under-counting
    /// keeps the ladder running rather than failing a task early).
    async fn planner_escalation_count(&self, source_task_id: &str) -> i64 {
        let repo = self.task_repo();
        let blockers = match repo.list_blockers(source_task_id).await {
            Ok(b) => b,
            Err(_) => return 0,
        };
        let mut count = 0i64;
        for b in &blockers {
            if let Ok(Some(t)) = repo.get(&b.task_id).await
                && crate::roles::releases_source_on_close(&t)
            {
                count += 1;
            }
        }
        count
    }

    /// Pure predicate: is `task` wedged in the same-signature CI remediation
    /// dead-end (incident ay3d)?
    ///
    /// True when a WORKER-role dispatch is being considered for a task whose
    /// required-CI gate is failing, a CI-loop remediation already ran against
    /// the CURRENT PR head (`ci_last_remediation_base_sha` equals the current
    /// head — no new push has landed since), and the same failure signature has
    /// persisted at least [`CI_SAME_SIGNATURE_ESCALATION_THRESHOLD`] times. In
    /// that state re-dispatching the worker only reproduces the identical red
    /// build, so the respawn guard defers it on every ready pass forever; the
    /// caller escalates instead.
    ///
    /// The "current head" is the attempt-derived GitHub head
    /// (`ci_github_head_sha`), falling back to the snapshot head
    /// (`ci_head_sha`) — the same head the CI-loop remediation records as its
    /// baseline.
    pub(crate) fn ci_same_signature_deadlocked(
        task: &djinn_core::models::Task,
        role: &str,
    ) -> bool {
        if role != "worker" {
            return false;
        }
        if task.ci_status.as_str() != djinn_core::models::CiStatus::Failing.as_str() {
            return false;
        }
        if task.ci_same_signature_count < CI_SAME_SIGNATURE_ESCALATION_THRESHOLD {
            return false;
        }
        let Some(baseline) = task
            .ci_last_remediation_base_sha
            .as_deref()
            .filter(|s| !s.is_empty())
        else {
            return false;
        };
        let head = task
            .ci_github_head_sha
            .as_deref()
            .filter(|s| !s.is_empty())
            .or_else(|| task.ci_head_sha.as_deref().filter(|s| !s.is_empty()));
        head == Some(baseline)
    }

    /// The no-human bottom of the remediation ladder.
    ///
    /// Creates an autonomous [`PlannerEscalation`](RemediationKind::PlannerEscalation)
    /// that blocks the stuck source and parks it `open` — UNLESS the
    /// autonomous-escalation ceiling ([`MAX_AUTONOMOUS_ESCALATIONS`]) has
    /// already been spent on this source, in which case the board gives up
    /// LOUDLY: it terminally fails (ForceClose) the source with a reason
    /// documenting the exhausted ladder rather than parking it for a person. A
    /// terminal close releases the source's blockers (verified in #1804), and a
    /// planner can always resurrect the work from the epic level.
    ///
    /// Returns `true` when the source was parked behind a fresh escalation,
    /// `false` when it was terminally failed (ceiling reached).
    pub(crate) async fn escalate_to_planner_or_terminally_fail(
        &mut self,
        task: &djinn_core::models::Task,
        reason: &str,
    ) -> bool {
        let prior = self.planner_escalation_count(&task.id).await;
        if prior >= MAX_AUTONOMOUS_ESCALATIONS {
            let terminal_reason = format!(
                "Autonomous remediation ladder exhausted: {prior} planner-park escalations already \
                 spent (ceiling {MAX_AUTONOMOUS_ESCALATIONS}) without convergence. Terminally \
                 failing this task rather than parking it for a human — a planner may resurrect it \
                 from the epic. Last reason: {reason}"
            );
            tracing::warn!(
                task_id = %task.short_id,
                prior_escalations = prior,
                ceiling = MAX_AUTONOMOUS_ESCALATIONS,
                "CoordinatorActor: autonomous-escalation ceiling reached — terminally failing task (no human park)"
            );
            self.terminally_fail_task(task, "coordinator", &terminal_reason)
                .await;
            return false;
        }
        // Ensure a planner-park escalation task blocks the source (creating one
        // only if it isn't already held), THEN park the source to `open`. The
        // blocker is added before the park, so the open task is never
        // dispatchable without its blocker in place.
        self.create_remediation_task(
            &task.id,
            reason,
            &task.project_id,
            RemediationKind::PlannerEscalation,
        )
        .await;
        self.park_source_open(&task.id, reason).await;
        true
    }

    /// Fail-closed loop-breaker park path for the second-strike rung.
    ///
    /// Clears in-memory and durable backoff, interrupts running sessions, then
    /// hands the source to the autonomous escalation ladder
    /// ([`escalate_to_planner_or_terminally_fail`](Self::escalate_to_planner_or_terminally_fail)):
    /// a planner-park escalation blocks + parks the source, or — once the
    /// escalation ceiling is spent — the source is terminally failed. Records
    /// the park metric. NO human-review hold is ever produced on the autonomous
    /// path.
    async fn park_source_human_review(
        &mut self,
        task: &djinn_core::models::Task,
        reason: &str,
        quality_strikes: i64,
    ) -> bool {
        tracing::warn!(
            task_id = %task.short_id,
            intervention_count = task.intervention_count,
            total_reopen_count = task.total_reopen_count,
            reopen_count = task.reopen_count,
            quality_strikes,
            "CoordinatorActor: second-strike — routing unconvergeable task to autonomous planner escalation after repeated planner interventions"
        );
        // Clear streak/cooldown so the hold isn't shadowed by stale backoff
        // state.
        self.dispatch_failure_streak.remove(&task.id);
        self.dispatch_cooldowns.remove(&task.id);
        self.last_dispatched.remove(&task.id);
        self.inflight_dispatches.remove(&task.id);
        self.clear_durable_dispatch_backoff_state(
            &task.id,
            Some(&task.short_id),
            "planner_second_strike_hold_clear",
        )
        .await;
        // Interrupt any running session for this task so parking it actually
        // frees the dispatch slot (a parked task must not keep burning one).
        let session_repo = djinn_db::SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        if let Err(e) = session_repo.interrupt_running_for_task(&task.id).await {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "CoordinatorActor: failed to interrupt running sessions while parking second-strike task"
            );
        }
        self.escalate_to_planner_or_terminally_fail(task, reason)
            .await;
        self.record_task_parked_metric(task, quality_strikes).await;
        true
    }

    /// Parse failing CI job ids from the `ci_failure_sections` text when the
    /// coordinator embedded `ci_job_log(job_id=...)` hints.
    fn parse_failing_ci_job_ids(&self, ci_failure_sections: Option<&str>) -> serde_json::Value {
        let mut ids = Vec::new();
        if let Some(text) = ci_failure_sections {
            for chunk in text.split("job_id=") {
                if let Some(num_part) = chunk.split_once(')')
                    && let Ok(job_id) = num_part.0.parse::<i64>()
                {
                    ids.push(job_id);
                }
            }
        }
        serde_json::json!(ids)
    }

    /// Dispatch a Planner escalation: create a review task, add a comment linking it
    /// to the source task, then dispatch the Planner to it.
    ///
    /// Called when Lead calls `request_planner` or when auto-escalation fires on the
    /// 2nd planner escalation for the same task.  Per ADR-051 §8 the Planner is now the
    /// escalation ceiling above Lead — it owns the board and decides whether to
    /// reshape, dedupe, or (if the issue requires deeper code-structural reasoning)
    /// dispatch an Architect spike.
    pub(crate) async fn dispatch_planner_escalation(
        &mut self,
        source_task_id: &str,
        reason: &str,
        project_id: &str,
    ) {
        self.create_remediation_task(source_task_id, reason, project_id, RemediationKind::Planner)
            .await;
    }

    /// Create a remediation review task that blocks the stuck `source_task_id`,
    /// add a linking comment, and (for [`RemediationKind::Planner`]) dispatch the
    /// Planner to it.
    ///
    /// Called when Lead calls `request_planner`, when auto-escalation fires on the
    /// 2nd planner escalation for the same task, and on the CI-loop / second-strike
    /// park paths. Per ADR-051 §8 the Planner is the escalation ceiling above Lead
    /// — it owns the board and decides whether to reshape, dedupe, or (if the issue
    /// requires deeper code-structural reasoning) dispatch an Architect spike.
    ///
    /// For [`RemediationKind::HumanReview`] no agent is dispatched (a human must
    /// resolve it) and creation is skipped when the source is already held by an
    /// unresolved blocker.
    pub(crate) async fn create_remediation_task(
        &mut self,
        source_task_id: &str,
        reason: &str,
        project_id: &str,
        kind: RemediationKind,
    ) {
        let task_repo = TaskRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        // The escalation runs under the SOURCE task's creator (the human it acts
        // on behalf of); resolves to None → automation via create_in_project
        // when the source is itself unowned. Used for both model eligibility and
        // creation attribution so they stay consistent.
        // Fetch the full source task once: its creator drives model eligibility
        // and attribution, and its short_id/title give the review task a name
        // with real line-of-sight into WHAT is being escalated.
        let source_task = task_repo.get(source_task_id).await.ok().flatten();
        let source_creator = source_task
            .as_ref()
            .and_then(|t| t.created_by_user_id.clone());

        // A held remediation (human-review OR planner-park escalation) is
        // idempotent: if the source is already held by an unresolved blocker, a
        // remediation task already exists — don't stack a fresh one on every
        // park tick. (Planner dispatch is NOT idempotent here — it is its own
        // dispatch path.)
        if matches!(
            kind,
            RemediationKind::HumanReview | RemediationKind::PlannerEscalation
        ) && let Some(src) = source_task.as_ref()
        {
            match task_repo.list_blockers(&src.id).await {
                Ok(blockers) if blockers.iter().any(|b| b.status != "closed") => {
                    tracing::info!(
                        source_task_id = %src.short_id,
                        "CoordinatorActor: held remediation skipped — source already held by an unresolved blocker"
                    );
                    return;
                }
                _ => {}
            }
        }

        // Models + project path are only needed to DISPATCH the Planner. A
        // human-review remediation is never dispatched, so it needs neither —
        // and must not bail out when no planner model is configured.
        let (model_ids, project_path): (Vec<String>, Option<String>) = match kind {
            RemediationKind::Planner => {
                let model_ids = self
                    .resolve_dispatch_models_for_role("planner", source_creator.as_deref())
                    .await;
                if model_ids.is_empty() {
                    tracing::warn!(
                        source_task_id = %source_task_id,
                        "CoordinatorActor: planner escalation — no model configured for planner role"
                    );
                    return;
                }

                // Per-user, per-model concurrency cap: the planner escalation must
                // consume the SAME shared per-(creator, model) budget as every other
                // dispatch path (worker, reviewer, lead, architect). Without this a
                // planner dispatch admitted in the same tick as worker dispatches can
                // overshoot max_sessions (observed: 2 worker + 1 reviewer = 3 > cap 2).
                // Filter to only models where the creator is under their cap, using a
                // fresh DB + inflight-ledger snapshot so a just-recorded admission in
                // this same tick is visible.
                let model_ids: Vec<String> = if let Some(creator) = source_creator.as_deref() {
                    let (running_by_model, running_by_lane) = self.effective_running_counts().await;
                    let settings = djinn_db::UserSettingsRepository::new(self.db.clone())
                        .get(creator)
                        .await
                        .ok()
                        .flatten();
                    let caps = settings
                        .as_ref()
                        .and_then(|s| s.max_sessions.clone())
                        .unwrap_or_default();
                    let lane = djinn_core::models::ModelLane::Plan;
                    let lane_cap = settings
                        .and_then(|s| s.lane_max_sessions)
                        .map(|limits| limits.lane(lane));
                    if !lane_under_user_cap(&running_by_lane, creator, lane, lane_cap) {
                        tracing::debug!(
                            source_task_id = %source_task_id,
                            creator,
                            lane_cap,
                            "CoordinatorActor: planner escalation deferred — creator at plan-lane concurrency cap"
                        );
                        return;
                    }
                    let mut filtered: Vec<String> = Vec::new();
                    for m in &model_ids {
                        let cap = caps.get(m).copied().unwrap_or(1);
                        if model_under_user_cap(&running_by_model, creator, m, cap) {
                            filtered.push(m.clone());
                        }
                    }
                    if filtered.is_empty() {
                        tracing::debug!(
                            source_task_id = %source_task_id,
                            creator,
                            "CoordinatorActor: planner escalation deferred — creator at per-model concurrency cap"
                        );
                        return;
                    }
                    filtered
                } else {
                    model_ids
                };

                let Some(project_path) = self.project_path_for_id(project_id).await else {
                    tracing::warn!(
                        project_id = %project_id,
                        source_task_id = %source_task_id,
                        "CoordinatorActor: planner escalation — project path not found"
                    );
                    return;
                };
                (model_ids, Some(project_path))
            }
            // Neither held-remediation kind is dispatched inline: HumanReview
            // waits for a human, and a PlannerEscalation is a normal review task
            // the coordinator dispatch pass routes to the Planner.
            RemediationKind::HumanReview | RemediationKind::PlannerEscalation => (Vec::new(), None),
        };

        // Name the review task after the work it is solving, not just a
        // truncated reason. "[<short_id>] <title>" gives the board immediate
        // line-of-sight into which task the Planner is remediating; the reason
        // still lives in the description below. (char-safe truncation — the old
        // byte slice could panic on a multi-byte boundary.)
        let reason_snippet: String = reason.chars().take(80).collect();
        let title = match source_task.as_ref() {
            Some(t) => {
                let name: String = t.title.chars().take(70).collect();
                format!("Planner remediation [{}]: {}", t.short_id, name)
            }
            None => format!("Planner escalation: {reason_snippet}"),
        };
        let source_label = source_task
            .as_ref()
            .map(|t| format!("{} ({})", t.title, t.short_id))
            .unwrap_or_else(|| source_task_id.to_string());
        let (description, instructions) = match kind {
            RemediationKind::Planner => (
                format!(
                    "Escalated from task {source_label}. Lead could not resolve — Planner review required.\n\nReason: {reason}"
                ),
                "Review the escalated task and either resolve it, reshape the work, or leave a 'Requires human review' comment.",
            ),
            RemediationKind::HumanReview => (
                format!(
                    "Escalated from task {source_label}. Repeated automated remediation FAILED — this requires HUMAN review.\n\nDo NOT auto-resolve: a human must close THIS task to release the blocked source task.\n\nReason: {reason}"
                ),
                "Repeated automated remediation failed. Requires human review — do not auto-resolve; a human must close this task to release the blocked source task.",
            ),
            RemediationKind::PlannerEscalation => (
                format!(
                    "Escalated from task {source_label}. Repeated automated remediation could not \
                     converge — the board handed YOU (the Planner) terminal ownership.\n\nYou OWN \
                     the resolution: decompose the source into replacement subtasks and supersede \
                     it, close it as won't-fix with a reason, or re-scope and reopen it. Do NOT \
                     create another escalation and do NOT wait for a human — closing THIS task \
                     releases the blocked source.\n\nReason: {reason}"
                ),
                "Automated remediation could not converge and the board handed you terminal \
                 ownership. Resolve it autonomously (decompose + supersede, close as won't-fix, or \
                 re-scope + reopen the source); closing this task releases the blocked source. Do \
                 NOT escalate again and do NOT wait for a human.",
            ),
        };
        let review_task = match djinn_core::auth_context::SESSION_USER_ID
            .scope(
                source_creator.clone(),
                task_repo.create_in_project_with_provenance(
                    project_id,
                    None,
                    EffectiveCreatorProvenance {
                        explicit_user_id: source_creator.as_deref(),
                        source_task_id: source_task.as_ref().map(|task| task.id.as_str()),
                        proposal_id: None,
                    },
                    &title,
                    &description,
                    instructions,
                    "review",
                    0,
                    "system",
                    Some("open"),
                    None,
                ),
            )
            .await
        {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    project_id = %project_id,
                    source_task_id = %source_task_id,
                    "CoordinatorActor: planner escalation — failed to create review task"
                );
                return;
            }
        };

        // Block the source task on the review task so it stops being
        // re-dispatched while the Planner investigates, and auto-resurfaces
        // (emit_unblocked_tasks) once the review task closes. Use the resolved
        // source uuid (source_task.id) — add_blocker keys on tasks.id, and the
        // caller-supplied source_task_id may be a short_id. Non-fatal: a failed
        // link still leaves the escalation dispatched.
        if let Some(src) = source_task.as_ref()
            && let Err(e) = task_repo.add_blocker(&src.id, &review_task.id).await
        {
            tracing::warn!(
                error = %e,
                source_task_id = %src.short_id,
                review_task_id = %review_task.short_id,
                "CoordinatorActor: planner escalation — failed to block source task on review task"
            );
        }

        // Label the held remediation task. Write only the labels column:
        // reusing the broad `update` path here is more fragile because it
        // reserializes unrelated JSON columns and can silently leave the hold
        // unlabeled if any copied field fails validation. Non-fatal: a failed
        // label write still leaves the hold in place via the blocker + comment,
        // but tests assert this path stays healthy.
        //
        // - `HumanReview`  → `human-review-hold`: excludes the task from dispatch
        //   (a human must act) and marks the "needs your review" UI indicator.
        // - `PlannerEscalation` → `planner-park-escalation`: a NORMAL,
        //   planner-dispatchable review task. The label is NOT applied to the
        //   SOURCE (that would block reviewer/worker dispatch on the source and
        //   defend it from auto-close); the source's own hold state (its parked
        //   status + any active tripwire `gate.held`) is the real gate. The
        //   label only drives the close-time source-release semantics
        //   (`releases_source_on_close`).
        let hold_label: Option<&str> = match kind {
            RemediationKind::HumanReview => Some(r#"["human-review-hold"]"#),
            RemediationKind::PlannerEscalation => Some(r#"["planner-park-escalation"]"#),
            RemediationKind::Planner => None,
        };
        if let Some(labels_json) = hold_label
            && let Err(e) = task_repo.update_labels(&review_task.id, labels_json).await
        {
            tracing::warn!(
                error = %e,
                review_task_id = %review_task.short_id,
                "CoordinatorActor: held remediation — failed to set hold label"
            );
        }

        // Log a comment on the source task linking to the remediation review task.
        let comment_body = match kind {
            RemediationKind::Planner => format!(
                "[PLANNER_ESCALATION] Escalated to Planner review task {}. Reason: {}",
                review_task.short_id, reason
            ),
            RemediationKind::HumanReview => format!(
                "[HUMAN_REVIEW_HOLD] Held on human-review remediation task {} after repeated \
                 automated remediation failed. This task stays open + blocked until a human \
                 resolves it. Reason: {}",
                review_task.short_id, reason
            ),
            RemediationKind::PlannerEscalation => format!(
                "[PLANNER_PARK_ESCALATION] Held on autonomous planner escalation task {} after \
                 automated remediation could not converge. The Planner owns terminal resolution; \
                 closing it releases this source. Reason: {}",
                review_task.short_id, reason
            ),
        };
        let comment_payload = serde_json::json!({ "body": comment_body }).to_string();
        let _ = task_repo
            .log_activity(
                Some(source_task_id),
                "coordinator",
                "system",
                "comment",
                &comment_payload,
            )
            .await;

        // Neither held-remediation kind is dispatched inline here. HumanReview
        // waits for a human. A PlannerEscalation is a normal open review task —
        // the coordinator's dispatch pass claims it for the Planner role on a
        // later tick (see `crate::roles::planner_review_claims`), so we must not
        // dispatch it here (and must not fall through to the Planner-dispatch
        // path below, which would double-dispatch).
        if matches!(
            kind,
            RemediationKind::HumanReview | RemediationKind::PlannerEscalation
        ) {
            tracing::info!(
                review_task_id = %review_task.short_id,
                source_task_id = %source_task_id,
                project_id = %project_id,
                kind = ?kind,
                "CoordinatorActor: held remediation created; source held until the remediation task closes"
            );
            return;
        }

        let Some(project_path) = project_path else {
            tracing::error!(
                source_task_id = %source_task_id,
                "planner remediation: project_path unexpectedly None after early-return guard"
            );
            return;
        };
        let build_admission = match self
            .begin_task_run_build_admission(
                "planner",
                &review_task.id,
                review_task.reopen_count.max(0),
                format!(
                    "task-run-{}-{}",
                    review_task.id,
                    review_task.reopen_count.max(0)
                ),
            )
            .await
        {
            Ok(permit) => permit,
            Err(()) => return,
        };
        let task_id = review_task.id.clone();
        let project_path_owned = project_path.clone();
        let outcome = self
            .try_dispatch_to_pool(
                &review_task.short_id,
                "planner",
                review_task.reopen_count.max(0) as u32,
                review_task.created_by_user_id.as_deref(),
                &model_ids,
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = task_id.clone();
                    let pp = project_path_owned.clone();
                    let mid = model_id.to_owned();
                    async move { pool.dispatch(&tid, &pp, &mid).await }
                },
            )
            .await;
        self.finish_task_run_build_admission(
            build_admission,
            matches!(outcome, DispatchOutcome::Dispatched),
        )
        .await;

        match outcome {
            DispatchOutcome::Dispatched => {
                tracing::info!(outcome = "ok", task_id = %review_task.short_id, role = "planner");
                tracing::info!(
                    review_task_id = %review_task.short_id,
                    review_task_uuid = %review_task.id,
                    source_task_id = %source_task_id,
                    project_id = %project_id,
                    "CoordinatorActor: Planner escalation dispatched"
                );
                self.last_dispatched.insert(
                    review_task.id.clone(),
                    DispatchMarker {
                        instant: SystemClock::new().now_instant(),
                        role: "planner".to_owned(),
                    },
                );
                self.dispatched += 1;
                // Record the planner admission in the shared in-flight ledger
                // so a same-tick dispatch of ANY role sees reduced capacity.
                // The dispatched model is the first health-available candidate
                // (the one try_dispatch_to_pool accepted).
                let dispatched_model = model_ids
                    .iter()
                    .find(|m| {
                        self.health
                            .is_available(review_task.created_by_user_id.as_deref(), m)
                    })
                    .cloned();
                if let Some(model) = dispatched_model {
                    self.record_inflight_dispatch(
                        &review_task.id,
                        Some(&review_task.short_id),
                        review_task.created_by_user_id.as_deref(),
                        &model,
                        "planner",
                    )
                    .await;
                }
                self.publish_status();
            }
            DispatchOutcome::AtCapacity => {
                tracing::debug!(outcome = "cap", task_id = %review_task.short_id, role = "planner");
                tracing::debug!(
                    "CoordinatorActor: planner escalation — Planner model at capacity, will retry next cycle"
                );
            }
            DispatchOutcome::PoolDead => {
                tracing::error!("CoordinatorActor: planner escalation — slot pool actor dead");
            }
            DispatchOutcome::Failed { .. } => {
                tracing::debug!(outcome = "error", task_id = %review_task.short_id, role = "planner");
                tracing::debug!(
                    "CoordinatorActor: planner escalation — no model could accept Planner dispatch"
                );
            }
        }
    }
}

/// Map a terminal `TaskAttemptOutcome` to a [`ReopenClass`] for truthful park
/// reason attribution. Returns `None` for guard-only or unknown outcomes.
fn outcome_to_reopen_class(outcome: &TaskAttemptOutcome) -> Option<ReopenClass> {
    match outcome {
        TaskAttemptOutcome::Reopened | TaskAttemptOutcome::ForceClosed => {
            Some(ReopenClass::ReviewRejected)
        }
        TaskAttemptOutcome::Completed => Some(ReopenClass::Other),
        // Infrastructure / provider-attempt failures (worker handshake
        // timeouts, provider stalls, spawn failures, timed-out attempts,
        // crashed infra attempts) are classified as `Infra` so they do NOT
        // count as worker/task-quality strikes, intervention counters, or
        // park escalation thresholds — while still surfacing in diagnostic
        // park/retry reasons. Sourced from the `7w2i` `task_attempts.outcome`
        // contract; no parallel outcome store is introduced.
        // `Interrupted` is an environmental infrastructure interruption
        // (deploy/rollout/reap); classified `Infra` so it never counts as a
        // worker/task-quality strike or park escalation.
        TaskAttemptOutcome::TimedOut
        | TaskAttemptOutcome::SpawnFailed
        | TaskAttemptOutcome::Crashed
        | TaskAttemptOutcome::Interrupted => Some(ReopenClass::Infra),
        // Guard-only or not a submission-triggered terminal.
        TaskAttemptOutcome::Deferred
        | TaskAttemptOutcome::AdoptedPr
        | TaskAttemptOutcome::Handoff => None,
        // Pre-submission terminals and unknown: not a reopen.
        _ => None,
    }
}

#[cfg(test)]
mod telemetry_tests {
    /// Verify that the labeled parked-task metric renders with the expected
    /// strike-class breakdown labels.
    #[test]
    fn labeled_park_metric_renders_strike_class_labels() {
        djinn_telemetry::init().unwrap();

        djinn_telemetry::task::increment_parked_labeled(3, 1, 2, 7);

        let rendered = djinn_telemetry::render().unwrap();
        let line = rendered
            .lines()
            .find(|l| {
                l.starts_with("djinn_tasks_parked_total")
                    && l.contains("quality_strikes=\"3\"")
                    && l.contains("merge_conflict_reopens=\"1\"")
                    && l.contains("superseded_reopens=\"2\"")
                    && l.contains("raw_reopen_count=\"7\"")
            })
            .expect("labeled park metric line not found");
        let value: f64 = line
            .rsplit_once(' ')
            .and_then(|(_, v)| v.parse().ok())
            .expect("metric value parses");
        assert!(value >= 1.0, "parked counter should be >= 1.0, got {value}");
    }

    /// Compatibility wrapper emits the counter with zero-valued labels.
    #[test]
    fn unlabeled_park_metric_emits_zero_labels() {
        djinn_telemetry::init().unwrap();

        djinn_telemetry::task::increment_parked();

        let rendered = djinn_telemetry::render().unwrap();
        let line = rendered
            .lines()
            .find(|l| {
                l.starts_with("djinn_tasks_parked_total")
                    && l.contains("quality_strikes=\"0\"")
                    && l.contains("merge_conflict_reopens=\"0\"")
                    && l.contains("superseded_reopens=\"0\"")
                    && l.contains("raw_reopen_count=\"0\"")
            })
            .expect("zero-labeled park metric line not found");
        let value: f64 = line
            .rsplit_once(' ')
            .and_then(|(_, v)| v.parse().ok())
            .expect("metric value parses");
        assert!(value >= 1.0, "parked counter should be >= 1.0, got {value}");
    }
}

#[cfg(test)]
mod infra_reopen_class_tests {
    use super::outcome_to_reopen_class;
    use djinn_core::models::ReopenClass;
    use djinn_core::models::task_attempt::TaskAttemptOutcome;

    /// AC #2: `timed_out`, `spawn_failed`, and `crashed` attempt outcomes map
    /// to `ReopenClass::Infra` without introducing a parallel outcome store.
    #[test]
    fn infra_outcomes_map_to_reopen_class_infra() {
        for outcome in [
            TaskAttemptOutcome::TimedOut,
            TaskAttemptOutcome::SpawnFailed,
            TaskAttemptOutcome::Crashed,
        ] {
            assert_eq!(
                outcome_to_reopen_class(&outcome),
                Some(ReopenClass::Infra),
                "{:?} must map to ReopenClass::Infra",
                outcome
            );
        }
    }

    /// AC #4: A non-infra quality failure (Reopened) keeps mapping to
    /// `ReviewRejected`, proving strike behavior is unchanged for real
    /// worker-quality failures.
    #[test]
    fn non_infra_quality_failure_still_maps_to_review_rejected() {
        assert_eq!(
            outcome_to_reopen_class(&TaskAttemptOutcome::Reopened),
            Some(ReopenClass::ReviewRejected)
        );
        assert_eq!(
            outcome_to_reopen_class(&TaskAttemptOutcome::ForceClosed),
            Some(ReopenClass::ReviewRejected)
        );
    }

    /// `TaskAttemptOutcome::is_infra()` agrees with the mapping set so the
    /// park-escalation exclusion and the reopen classification stay in sync.
    #[test]
    fn is_infra_predicate_matches_outcome_to_reopen_class_set() {
        for outcome in [
            TaskAttemptOutcome::Pending,
            TaskAttemptOutcome::Submitted,
            TaskAttemptOutcome::Completed,
            TaskAttemptOutcome::Reopened,
            TaskAttemptOutcome::Crashed,
            TaskAttemptOutcome::TimedOut,
            TaskAttemptOutcome::Cancelled,
            TaskAttemptOutcome::LoopGuardTripped,
            TaskAttemptOutcome::SpawnFailed,
            TaskAttemptOutcome::Deferred,
            TaskAttemptOutcome::AdoptedPr,
            TaskAttemptOutcome::ForceClosed,
            TaskAttemptOutcome::Handoff,
        ] {
            let mapped_to_infra = outcome_to_reopen_class(&outcome) == Some(ReopenClass::Infra);
            assert_eq!(
                outcome.is_infra(),
                mapped_to_infra,
                "is_infra() must agree with outcome_to_reopen_class for {:?}",
                outcome
            );
        }
    }
}
