// djinn:allow-oversize — legacy dispatch module over size-guard threshold; split when touched substantively.
use super::super::*;
use super::DispatchOutcome;
#[cfg(test)]
use super::admission::{DispatchCapObservation, DispatchCapObservationStage};
#[cfg(test)]
use super::admission::{
    clear_dispatch_cap_observations, observe_dispatch_cap_count, take_dispatch_cap_observations,
};
use super::admission::{model_under_user_cap, overlay_inflight_ledger};
use super::post_intervention_lane;
use crate::dispatch_pause::{load_dispatch_pause_state, matching_task_dispatch_pause};
use crate::roles::DispatchContext;
use djinn_core::clock::{Clock, SystemClock};
use djinn_db::{DispatchStateRepository, DispatchStateUpsert};

fn record_dispatch_attempt(outcome: &'static str) {
    djinn_telemetry::dispatch::increment_attempt(outcome);
}

fn record_dispatch_outcome(outcome: &'static str) {
    if outcome == djinn_telemetry::dispatch::OUTCOME_OK {
        djinn_telemetry::dispatch::record_success_at(dispatch_success_timestamp_secs());
    } else {
        record_dispatch_attempt(outcome);
    }
}

fn dispatch_success_timestamp_secs() -> f64 {
    SystemClock::new()
        .now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

fn record_dispatch_live_state(cooldowns_active: usize, inflight_ledger_size: usize) {
    djinn_telemetry::dispatch::set_cooldowns_active(cooldowns_active);
    djinn_telemetry::dispatch::set_inflight_ledger_size(inflight_ledger_size);
}

/// Env flag allowing operators (and the in-process TestRuntime path) to
/// bypass the devcontainer-image + graph-warm readiness gate. Default is
/// "on" (fail-closed). Set to `0`/`false`/`no` to dispatch as soon as a
/// task is ready, regardless of project readiness.
const ENV_REQUIRE_WARMED_GRAPH: &str = "DJINN_REQUIRE_WARMED_GRAPH";

fn readiness_gate_enabled() -> bool {
    match std::env::var(ENV_REQUIRE_WARMED_GRAPH) {
        Ok(val) => !matches!(
            val.trim().to_ascii_lowercase().as_str(),
            "" | "0" | "false" | "no" | "off"
        ),
        Err(_) => true,
    }
}

fn task_ownerless_from_creator_lookup<E: std::fmt::Display>(
    task_short_id: &str,
    created_by_user_id: Option<&str>,
    creator_lookup: Result<Option<()>, E>,
) -> bool {
    let Some(uid) = created_by_user_id else {
        return true;
    };

    match creator_lookup {
        // Creator id resolves to a user row: dispatch may proceed under that owner.
        Ok(Some(())) => false,
        // Creator id present but no matching user row (dangling reference) →
        // ownerless; refuse rather than dispatch under a ghost identity.
        Ok(None) => true,
        // DB error resolving the creator: fail-closed (refuse) so a transient
        // lookup failure can't slip an unverified owner past the guard.
        Err(e) => {
            tracing::warn!(
                task_id = %task_short_id,
                created_by_user_id = uid,
                error = %e,
                "CoordinatorActor: ownership guard — failed to resolve task creator; treating as ownerless"
            );
            true
        }
    }
}

#[derive(Default)]
struct DurableDispatchStateUpdate {
    failure_streak: Option<u32>,
    cooldown_until: Option<Option<String>>,
    last_dispatched: Option<Option<(String, String)>>,
    inflight: Option<Option<(Option<String>, String)>>,
}

fn format_dispatch_wall_clock(ts: ::time::OffsetDateTime) -> Option<String> {
    // Dispatch-state timestamps are round-tripped through Postgres and selected
    // with millisecond precision. Emit the same precision up front so no-op
    // write-throughs (for example, pause skips preserving cooldown_until) do not
    // appear to mutate durable state by dropping sub-millisecond digits.
    let ts = ts
        .replace_nanosecond((ts.nanosecond() / 1_000_000) * 1_000_000)
        .ok()?;
    ts.format(&::time::format_description::well_known::Rfc3339)
        .ok()
}

fn dispatch_wall_clock_after(duration: Duration) -> Option<String> {
    let secs = duration.as_secs().min(i64::MAX as u64) as i64;
    let nanos = duration.subsec_nanos() as i64;
    let deadline = ::time::OffsetDateTime::now_utc()
        + ::time::Duration::seconds(secs)
        + ::time::Duration::nanoseconds(nanos);
    format_dispatch_wall_clock(deadline)
}

fn dispatch_wall_clock_now() -> Option<String> {
    format_dispatch_wall_clock(::time::OffsetDateTime::now_utc())
}

impl CoordinatorActor {
    async fn reconcile_inflight_dispatch_ledger(&mut self) {
        match self.pool.get_status().await {
            Ok(status) => {
                let live: std::collections::HashSet<String> = status
                    .running_tasks
                    .into_iter()
                    .map(|t| t.task_id)
                    .collect();
                let stale_inflight_task_ids: Vec<String> = self
                    .inflight_dispatches
                    .keys()
                    .filter(|task_id| !live.contains(*task_id))
                    .cloned()
                    .collect();
                self.inflight_dispatches
                    .retain(|task_id, _| live.contains(task_id));
                for task_id in stale_inflight_task_ids {
                    self.persist_durable_dispatch_state_update(
                        &task_id,
                        None,
                        "inflight_ledger_reconcile_clear",
                        DurableDispatchStateUpdate {
                            inflight: Some(None),
                            ..Default::default()
                        },
                    )
                    .await;
                }
            }
            // On a pool query error, keep the ledger as-is rather than dropping
            // it — a stale-but-present ledger is conservative (may briefly defer
            // a task), whereas dropping it would re-open the overshoot window.
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: pool get_status failed during cap seed; keeping in-flight ledger as-is");
            }
        }
    }

    /// Load the current effective per-`(creator, model)` running counts — DB seed
    /// overlaid with the in-flight ledger via `max` — as a fresh snapshot.
    ///
    /// This is the shared source of truth for the per-user, per-model concurrency
    /// cap. Both `dispatch_ready_tasks` (worker/reviewer/lead/architect wave) and
    /// `dispatch_planner_escalation` (planner intervention dispatch) must use it so
    /// no dispatch path can admit a session that overshoots the cap.
    ///
    /// Unlike `dispatch_ready_tasks` which seeds once per pass and bumps locally,
    /// this method re-reads the DB + ledger each call. That's acceptable for the
    /// planner escalation path (called at most a handful of times per tick), and
    /// guarantees it never sees a stale snapshot after a just-recorded admission.
    pub(crate) async fn effective_running_by_user_model(
        &mut self,
    ) -> HashMap<(String, String), u32> {
        let mut running: HashMap<(String, String), u32> = match SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
        .count_active_by_user_and_model()
        .await
        {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|(creator, model, cnt)| {
                    creator.map(|c| ((c, model), u32::try_from(cnt).unwrap_or(0)))
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: per-user concurrency counts failed; proceeding without caps");
                HashMap::new()
            }
        };
        self.reconcile_inflight_dispatch_ledger().await;
        overlay_inflight_ledger(&mut running, &self.inflight_dispatches);
        self.overlay_provisional_admissions(&mut running);
        running
    }

    /// Re-overlay the in-flight ledger onto the local per-pass
    /// `running_by_user_model` map after a planner intervention dispatched a
    /// new session inside the `dispatch_ready_tasks` loop.
    ///
    /// `dispatch_planner_escalation` records its admission into
    /// `self.inflight_dispatches` via `record_inflight_dispatch`, but the
    /// local `running_by_user_model` was seeded at the top of the pass and
    /// won't reflect the new entry. Re-overlaying ensures the next task in
    /// the same pass sees reduced capacity — closing the within-tick overshoot
    /// gap between the worker wave and the planner intervention sweep.
    async fn bump_local_cap_for_last_planner_admission(
        &mut self,
        running_by_user_model: &mut HashMap<(String, String), u32>,
    ) {
        overlay_inflight_ledger(running_by_user_model, &self.inflight_dispatches);
    }

    /// Record a successful dispatch admission of ANY role into the in-flight
    /// ledger so the per-user, per-model cap reflects it immediately.
    ///
    /// This is the single shared admission-recording path. Both
    /// `dispatch_ready_tasks` (worker/reviewer/lead/architect) and
    /// `dispatch_planner_escalation` call it, so a just-dispatched session of
    /// any role counts against the cap for a same-tick second admission
    /// (the worker + reviewer overshoot gap).
    pub(crate) async fn record_inflight_dispatch(
        &mut self,
        task_id: &str,
        task_short_id: Option<&str>,
        creator: Option<&str>,
        model: &str,
    ) {
        if let Some(c) = creator {
            self.inflight_dispatches.insert(
                task_id.to_string(),
                (Some(c.to_string()), model.to_string()),
            );
            record_dispatch_live_state(
                self.dispatch_cooldowns.len(),
                self.inflight_dispatches.len(),
            );
            self.persist_durable_dispatch_state_update(
                task_id,
                task_short_id,
                "inflight_ledger_insert",
                DurableDispatchStateUpdate {
                    inflight: Some(Some((Some(c.to_string()), model.to_string()))),
                    ..Default::default()
                },
            )
            .await;
        }
    }

    async fn select_resume_lifecycle_metadata_for_dispatch(
        &self,
        task: &djinn_core::models::Task,
    ) -> Option<crate::ResumeLifecycleMetadata> {
        if !self.worker_lifecycle_config.resume.enabled {
            return None;
        }

        let target_ref = format!("task/{}", task.short_id);
        let lifecycle = self
            .resume_lifecycle_metadata_from_existing_rows(task)
            .await;
        let prior_lineage = lifecycle
            .checkpoint
            .as_ref()
            .and_then(|checkpoint| checkpoint.extra.get("session_id"))
            .and_then(serde_json::Value::as_str)
            .or_else(|| {
                lifecycle
                    .auto_submit
                    .as_ref()
                    .and_then(|auto_submit| auto_submit.extra.get("session_id"))
                    .and_then(serde_json::Value::as_str)
            });
        let candidates = crate::dispatch::resume_source::build_resume_source_candidates(
            &task.id,
            &target_ref,
            prior_lineage,
            &lifecycle,
        );
        let selection = crate::dispatch::resume_source::select_resume_source(
            &self.worker_lifecycle_config.resume,
            &task.id,
            prior_lineage,
            &candidates,
        )?;
        let metadata = crate::dispatch::resume_source::selection_to_metadata(&selection);

        // Thread additional context fields from the lifecycle metadata into
        // the resume metadata's extra map so they deserialize into the
        // runtime ResumeLifecycleMetadata typed fields (previous_model,
        // verification_command, last_durable_progress_summary). These are
        // consumed by the worker resume-prompt note (48ru).
        let mut metadata = metadata;
        if let Some(model_rotation) = &lifecycle.model_rotation
            && let Some(prev) = &model_rotation.previous_model
        {
            metadata
                .extra
                .insert("previous_model".to_string(), serde_json::json!(prev));
        }
        if let Some(auto_submit) = &lifecycle.auto_submit
            && let Some(cmd) = &auto_submit.verification_command
        {
            metadata
                .extra
                .insert("verification_command".to_string(), serde_json::json!(cmd));
        }
        // last_durable_progress_summary: extract from checkpoint extra if present.
        if let Some(checkpoint) = &lifecycle.checkpoint
            && let Some(summary) = checkpoint.extra.get("last_durable_progress_summary")
        {
            metadata
                .extra
                .insert("last_durable_progress_summary".to_string(), summary.clone());
        }

        let payload = serde_json::json!({
            "event": "resume_source_selected",
            "worker_lifecycle": {
                "resume": metadata,
            }
        });
        if let Err(e) = self
            .task_repo()
            .log_activity(
                Some(&task.id),
                "coordinator",
                "system",
                "comment",
                &payload.to_string(),
            )
            .await
        {
            tracing::warn!(
                task_id = %task.short_id,
                error = %e,
                "CoordinatorActor: failed to persist resume-source selection metadata"
            );
        }
        Some(metadata)
    }

    async fn resume_lifecycle_metadata_from_existing_rows(
        &self,
        task: &djinn_core::models::Task,
    ) -> crate::WorkerLifecycleMetadata {
        let mut lifecycle = crate::WorkerLifecycleMetadata::default();
        let mut latest_task_run_id: Option<String> = None;

        match djinn_db::TaskRunRepository::new(self.db.clone())
            .list_for_task(&task.id)
            .await
        {
            Ok(task_runs) => {
                latest_task_run_id = task_runs.first().map(|run| run.id.clone());
            }
            Err(e) => {
                tracing::warn!(task_id = %task.short_id, error = %e, "CoordinatorActor: failed to load task-runs for resume selection");
            }
        }

        if let Some(task_run_id) = latest_task_run_id.as_deref() {
            lifecycle.auto_submit = self.auto_submit_lifecycle_for_task_run(task_run_id).await;
        }
        lifecycle.checkpoint = self.checkpoint_lifecycle_from_activity(task).await;
        lifecycle
    }

    async fn auto_submit_lifecycle_for_task_run(
        &self,
        task_run_id: &str,
    ) -> Option<crate::AutoSubmitLifecycleMetadata> {
        let repo =
            djinn_db::repositories::verify_run::AutoSubmitReviewRepository::new(self.db.clone());
        let reviews = match repo.list_for_task_run(task_run_id).await {
            Ok(reviews) => reviews,
            Err(e) => {
                tracing::warn!(task_run_id = %task_run_id, error = %e, "CoordinatorActor: failed to load auto-submit reviews for resume selection");
                return None;
            }
        };
        let review = reviews.first()?;
        let mut extra = serde_json::Map::new();
        extra.insert(
            "task_run_id".to_string(),
            serde_json::json!(review.task_run_id),
        );
        if let Some(session_id) = &review.session_id {
            extra.insert("session_id".to_string(), serde_json::json!(session_id));
        }
        if let Some(model_id) = &review.model_id {
            extra.insert("model_id".to_string(), serde_json::json!(model_id));
        }
        Some(crate::AutoSubmitLifecycleMetadata {
            considered: true,
            green: Some(review.model_called_submit_work),
            verification_command: review.verify_source.clone(),
            submission_id: review.model_called_submit_work.then(|| review.id.clone()),
            skipped_reason: (!review.model_called_submit_work)
                .then_some(crate::AutoSubmitSkipReason::ReviewRequired),
            extra,
        })
    }

    async fn checkpoint_lifecycle_from_activity(
        &self,
        task: &djinn_core::models::Task,
    ) -> Option<crate::CheckpointLifecycleMetadata> {
        let entries = match self.task_repo().list_activity(&task.id).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(task_id = %task.short_id, error = %e, "CoordinatorActor: failed to load activity for resume checkpoint selection");
                return None;
            }
        };
        entries.iter().rev().find_map(|entry| {
            let value: serde_json::Value = serde_json::from_str(&entry.payload).ok()?;
            let commit_sha = value.get("preservation_commit_sha")?.as_str()?.to_owned();
            let ref_name = value
                .get("preservation_ref_name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let mut extra = serde_json::Map::new();
            if let Some(session_id) = value.get("session_id").and_then(serde_json::Value::as_str) {
                extra.insert("session_id".to_string(), serde_json::json!(session_id));
            }
            Some(crate::CheckpointLifecycleMetadata {
                checkpoint_id: None,
                commit_sha: Some(commit_sha),
                ref_name,
                requested_for: None,
                safety_scan: Some(crate::CheckpointSafetyScanMetadata {
                    passed: true,
                    scanner: Some("preservation_activity".to_string()),
                    findings: vec![],
                }),
                preservation_outcome: Some(crate::PreservationOutcome::Succeeded),
                extra,
            })
        })
    }

    // ─── Shared per-(user, model) admission surface ────────────────────────
    //
    // The methods below compose the pure primitives in [`super::admission`]
    // with the actor's own DB, in-flight ledger, and durable dispatch-state.
    // They exist so that every dispatch path — normal task dispatch,
    // planner escalation, AND refinement tribunal dispatch — checks admission
    // and reserves/clears in-flight slots through one shared API rather than
    // each caller re-implementing the cap-check + ledger logic.
    //
    // These methods are used by both `dispatch_ready_tasks` (and
    // `dispatch_planner_escalation`) and `refinement_dispatch::
    // dispatch_next_refinement_phase`, so refinement tribunal dispatch and
    // normal task dispatch go through the exact same cap/ledger code path.

    /// Resolve the configured `max_sessions` cap map for `creator`.
    ///
    /// Returns `model_id → max concurrent sessions`. When the user has no
    /// settings row or `max_sessions` is unset, an empty map is returned —
    /// callers then apply a per-model default (conventionally 1) via
    /// [`model_under_user_cap`].
    pub(crate) async fn resolve_model_caps_for_user(
        &self,
        creator: &str,
    ) -> std::collections::HashMap<String, u32> {
        djinn_db::UserSettingsRepository::new(self.db.clone())
            .get(creator)
            .await
            .ok()
            .flatten()
            .and_then(|s| s.max_sessions)
            .unwrap_or_default()
    }

    /// Check whether a single `(user, model)` is admissible under the
    /// configured per-user concurrency cap.
    ///
    /// This re-reads the DB active-session counts plus the in-flight ledger
    /// overlay on each call (via [`effective_running_by_user_model`]), so it
    /// always reflects the latest state — including a just-recorded admission
    /// in the same tick. Returns `true` when the user has room for one more
    /// session on `model`.
    ///
    /// This is the single shared admission-check for single-model dispatch
    /// paths (refinement tribunal dispatch). Multi-model dispatch paths
    /// (normal task dispatch) seed once per pass and filter with
    /// [`model_under_user_cap`] directly for efficiency, but they use the same
    /// underlying primitives.
    pub(crate) async fn check_user_model_admission(
        &mut self,
        user: &str,
        model: &str,
        cap: u32,
    ) -> bool {
        let running = self.effective_running_by_user_model().await;
        model_under_user_cap(&running, user, model, cap)
    }

    /// Clear an in-flight dispatch reservation from the ledger.
    ///
    /// Called on failure paths where a dispatch was reserved (via
    /// [`record_inflight_dispatch`]) but never produced a running session —
    /// e.g. pool dispatch failure, task-setup failure, or — for refinement
    /// tribunal dispatch — spawn-cap rejection after task creation. Removes
    /// the entry so the `(user, model)` slot is immediately available again,
    /// and persists the clearance to durable dispatch-state.
    ///
    /// The session-start and reconciliation paths in
    /// `reconcile_inflight_dispatch_ledger` continue to clear started/stale
    /// entries independently.
    pub(crate) async fn clear_inflight_dispatch(&mut self, task_id: &str) {
        if self.inflight_dispatches.remove(task_id).is_some() {
            record_dispatch_live_state(
                self.dispatch_cooldowns.len(),
                self.inflight_dispatches.len(),
            );
            self.persist_durable_dispatch_state_update(
                task_id,
                None,
                "inflight_ledger_clear",
                DurableDispatchStateUpdate {
                    inflight: Some(None),
                    ..Default::default()
                },
            )
            .await;
        }
    }

    /// Overlay provisional refinement admissions onto the per-`(user, model)`
    /// running counts. Called from [`effective_running_by_user_model`] so that
    /// `check_user_model_admission` accounts for reservations that have not yet
    /// been re-keyed to a real task id.
    fn overlay_provisional_admissions(
        &self,
        running_by_user_model: &mut HashMap<(String, String), u32>,
    ) {
        for (creator, model) in self.provisional_admissions.values() {
            if let Some(c) = creator {
                let entry = running_by_user_model
                    .entry((c.clone(), model.clone()))
                    .or_insert(0);
                *entry = (*entry).max(1);
            }
        }
    }

    /// Re-key a provisional refinement admission to the real task id in the
    /// in-flight dispatch ledger.
    ///
    /// Called after a refinement task row has been created so that the
    /// reservation is now tracked by the durable `inflight_dispatches` ledger
    /// (visible to reconciliation and session-start cleanup) rather than the
    /// ephemeral `provisional_admissions` map.
    pub(crate) async fn rekey_provisional_to_inflight(
        &mut self,
        provisional_key: &str,
        real_task_id: &str,
        creator: &str,
        model: &str,
    ) {
        self.provisional_admissions.remove(provisional_key);
        self.inflight_dispatches.remove(provisional_key);
        self.record_inflight_dispatch(real_task_id, None, Some(creator), model)
            .await;
    }

    /// Clear a provisional refinement admission.
    ///
    /// Called when the refinement dispatch fails before the real task id is
    /// known (e.g. task creation failure, at-cap deferral cleanup).
    pub(crate) fn clear_provisional_admission(&mut self, provisional_key: &str) {
        self.provisional_admissions.remove(provisional_key);
        self.inflight_dispatches.remove(provisional_key);
    }

    async fn persist_durable_dispatch_state_update(
        &self,
        task_id: &str,
        task_short_id: Option<&str>,
        reason: &str,
        update: DurableDispatchStateUpdate,
    ) {
        let repo = DispatchStateRepository::new(self.db.clone());
        let existing = match repo.get(task_id).await {
            Ok(existing) => existing,
            Err(e) => {
                tracing::warn!(
                    task_id = task_short_id.unwrap_or(task_id),
                    task_uuid = %task_id,
                    reason,
                    error = %e,
                    "CoordinatorActor: failed to load durable dispatch state before write-through"
                );
                return;
            }
        };

        let failure_streak = update
            .failure_streak
            .map(i64::from)
            .or_else(|| existing.as_ref().map(|r| r.failure_streak))
            .unwrap_or(0);
        let cooldown_until = update
            .cooldown_until
            .unwrap_or_else(|| existing.as_ref().and_then(|r| r.cooldown_until.clone()));
        let escalation_count = existing
            .as_ref()
            .map(|r| r.escalation_count)
            .unwrap_or_else(|| {
                i64::from(self.escalation_counts.get(task_id).copied().unwrap_or(0))
            });
        let (last_dispatched_at, last_dispatched_role) = match update.last_dispatched {
            Some(Some((at, role))) => (Some(at), Some(role)),
            Some(None) => (None, None),
            None => existing
                .as_ref()
                .map(|r| (r.last_dispatched_at.clone(), r.last_dispatched_role.clone()))
                .unwrap_or((None, None)),
        };
        let (inflight_creator_user_id, inflight_model_id) = match update.inflight {
            Some(Some((creator, model))) => (creator, Some(model)),
            Some(None) => (None, None),
            None => existing
                .as_ref()
                .map(|r| {
                    (
                        r.inflight_creator_user_id.clone(),
                        r.inflight_model_id.clone(),
                    )
                })
                .unwrap_or((None, None)),
        };

        if let Err(e) = repo
            .upsert(DispatchStateUpsert {
                task_id,
                failure_streak,
                cooldown_until: cooldown_until.as_deref(),
                escalation_count,
                last_dispatched_at: last_dispatched_at.as_deref(),
                last_dispatched_role: last_dispatched_role.as_deref(),
                inflight_creator_user_id: inflight_creator_user_id.as_deref(),
                inflight_model_id: inflight_model_id.as_deref(),
            })
            .await
        {
            tracing::warn!(
                task_id = task_short_id.unwrap_or(task_id),
                task_uuid = %task_id,
                reason,
                error = %e,
                "CoordinatorActor: failed to persist durable dispatch state mutation"
            );
        }
    }

    pub(crate) async fn clear_durable_dispatch_backoff_state(
        &self,
        task_id: &str,
        task_short_id: Option<&str>,
        reason: &str,
    ) {
        self.persist_durable_dispatch_state_update(
            task_id,
            task_short_id,
            reason,
            DurableDispatchStateUpdate {
                failure_streak: Some(0),
                cooldown_until: Some(None),
                last_dispatched: Some(None),
                inflight: Some(None),
            },
        )
        .await;
    }

    pub(crate) async fn clear_planned_dispatch_completion(&mut self, task_id: &str, reason: &str) {
        // Planned lifecycle completions (including budget parks and ignored
        // wind-down parks) are successful settlements, not same-role dispatch
        // failures. Drop any stale recovery/backoff attribution before the next
        // continuation dispatch so they cannot advance Trigger-B or terminal
        // close accounting during recovery/refactor paths.
        self.dispatch_failure_streak.remove(task_id);
        self.provider_failure_streak.remove(task_id);
        self.dispatch_cooldowns.remove(task_id);
        self.last_dispatched.remove(task_id);
        self.inflight_dispatches.remove(task_id);
        self.clear_durable_dispatch_backoff_state(task_id, None, reason)
            .await;
    }

    pub(crate) async fn increment_durable_escalation_count(
        &self,
        task_id: &str,
    ) -> djinn_db::Result<u32> {
        let repo = DispatchStateRepository::new(self.db.clone());
        let existing = repo.get(task_id).await?;
        let existing_count = existing
            .as_ref()
            .map(|r| r.escalation_count.max(0).min(u32::MAX as i64) as u32)
            .unwrap_or(0)
            .max(self.escalation_counts.get(task_id).copied().unwrap_or(0));
        let next_count = existing_count.saturating_add(1);

        repo.upsert(DispatchStateUpsert {
            task_id,
            failure_streak: existing
                .as_ref()
                .map(|r| r.failure_streak)
                .unwrap_or_else(|| {
                    i64::from(
                        self.dispatch_failure_streak
                            .get(task_id)
                            .copied()
                            .unwrap_or(0),
                    )
                }),
            cooldown_until: existing.as_ref().and_then(|r| r.cooldown_until.as_deref()),
            escalation_count: i64::from(next_count),
            last_dispatched_at: existing
                .as_ref()
                .and_then(|r| r.last_dispatched_at.as_deref()),
            last_dispatched_role: existing
                .as_ref()
                .and_then(|r| r.last_dispatched_role.as_deref()),
            inflight_creator_user_id: existing
                .as_ref()
                .and_then(|r| r.inflight_creator_user_id.as_deref()),
            inflight_model_id: existing
                .as_ref()
                .and_then(|r| r.inflight_model_id.as_deref()),
        })
        .await?;

        Ok(next_count)
    }

    /// Cross-model ("Thorough") review steering for a reviewer dispatch.
    ///
    /// When the task creator has `diverse_review` enabled, reorder `model_ids`
    /// (the resolved review-lane fallback list) so the first viable entry is a
    /// model id DIFFERENT from the one that implemented the task. The
    /// implementer's model id is read from the most recent `worker` session for
    /// the task. Behavior:
    ///
    /// - `diverse_review` off, or no implementer model on file yet, or no
    ///   distinct alternative exists → leave the order untouched (same-model
    ///   review is the graceful fallback; the task is never blocked).
    /// - Otherwise → stable-partition so all ids != implementer come first
    ///   (priority preserved within each group), then ids == implementer.
    ///
    /// Records the outcome (log + `djinn_cross_model_review_total`) so the
    /// substitution/fallback is observable.
    #[cfg(not(test))]
    async fn apply_diverse_review_ordering(
        &self,
        task: &djinn_core::models::Task,
        creator: Option<&str>,
        model_ids: &mut [String],
    ) {
        let Some(uid) = creator else {
            return;
        };
        let us_repo = djinn_db::UserSettingsRepository::new(self.db.clone());
        let diverse = match us_repo.get(uid).await {
            // Default ON when the user has never written settings (matches the
            // DB column default + `defaults_for`).
            Ok(Some(s)) => s.diverse_review,
            Ok(None) => true,
            Err(_) => return,
        };
        if !diverse {
            return;
        }

        // The model that implemented the task = the most recent worker session's
        // model id. Unknown (no worker session yet) → nothing to diverge from.
        let implementer = match djinn_db::SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
        .latest_model_for_task_role(&task.id, "worker")
        .await
        {
            Ok(Some(m)) if !m.trim().is_empty() => m,
            _ => return,
        };

        let has_alternative = model_ids.iter().any(|m| m != &implementer);
        if !has_alternative {
            // The whole viable review list collapses to the implementer's model
            // — proceed same-model rather than blocking the review.
            tracing::info!(
                task_id = %task.short_id,
                implementer_model = %implementer,
                "CoordinatorActor: diverse review wanted but only the implementer's \
                 model is viable — proceeding same-model"
            );
            djinn_telemetry::dispatch::record_cross_model_review("same_fallback");
            return;
        }

        // Stable partition: ids != implementer keep their relative priority and
        // move ahead of any id == implementer.
        model_ids.sort_by_key(|m| m == &implementer);
        tracing::info!(
            task_id = %task.short_id,
            implementer_model = %implementer,
            reviewer_model = %model_ids.first().map(String::as_str).unwrap_or(""),
            "CoordinatorActor: cross-model review — steering reviewer to a model \
             distinct from the implementer"
        );
        djinn_telemetry::dispatch::record_cross_model_review("different");
    }

    /// Test-build no-op: the in-process dispatch fixtures seed no real users or
    /// sessions, and `resolve_user_model_priority` already returns empty under
    /// test, so cross-model steering has nothing to act on. The production path
    /// is exercised via the live MCP/session flow + the repo-level unit tests.
    #[cfg(test)]
    async fn apply_diverse_review_ordering(
        &self,
        _task: &djinn_core::models::Task,
        _creator: Option<&str>,
        _model_ids: &mut [String],
    ) {
    }

    #[tracing::instrument(
        name = "djinn.dispatch",
        skip(self, model_ids, dispatch_fn),
        fields(
            task_id = %label,
            model_id = tracing::field::Empty,
            role = %role,
            attempt = %attempt,
            pass_kind = "pool",
            outcome = tracing::field::Empty
        )
    )]
    pub(crate) async fn try_dispatch_to_pool<F, Fut>(
        &self,
        label: &str,
        role: &str,
        attempt: u32,
        // Owning user the breaker is keyed on (`tasks.created_by_user_id`);
        // `None` = system/unowned work on the org-shared credential. Health is
        // per-`(scope, model)` so one user's throttled account can't disable a
        // model for everyone — see [`djinn_provider::catalog::HealthKey`].
        scope: Option<&str>,
        model_ids: &[String],
        dispatch_fn: F,
    ) -> DispatchOutcome
    where
        F: Fn(&SlotPoolHandle, &str) -> Fut,
        Fut: std::future::Future<Output = Result<(), PoolError>>,
    {
        let mut any_at_capacity = false;

        for model_id in model_ids {
            tracing::Span::current().record("model_id", tracing::field::display(model_id));
            if !self.health.is_available(scope, model_id) {
                tracing::Span::current().record("outcome", "breaker");
                tracing::debug!(outcome = "breaker", model_id = %model_id, label);
                tracing::debug!(
                    model_id = %model_id,
                    scope = ?scope,
                    label,
                    "CoordinatorActor: model unavailable by health tracker"
                );
                continue;
            }

            match dispatch_fn(&self.pool, model_id).await {
                Ok(()) => {
                    tracing::Span::current().record("outcome", "ok");
                    tracing::info!(outcome = "ok", model_id = %model_id, label);
                    return DispatchOutcome::Dispatched;
                }
                Err(PoolError::AtCapacity { .. }) => {
                    any_at_capacity = true;
                    tracing::Span::current().record("outcome", "cap");
                    tracing::debug!(outcome = "cap", model_id = %model_id, label);
                    tracing::debug!(
                        model_id = %model_id,
                        label,
                        "CoordinatorActor: model at capacity, trying next model"
                    );
                }
                Err(PoolError::ActorDead) => {
                    tracing::Span::current().record("outcome", "error");
                    tracing::debug!(outcome = "error", model_id = %model_id, label);
                    tracing::error!("CoordinatorActor: slot pool actor dead, aborting dispatch");
                    return DispatchOutcome::PoolDead;
                }
                Err(e) => {
                    tracing::Span::current().record("outcome", "error");
                    tracing::debug!(outcome = "error", model_id = %model_id, label);
                    tracing::warn!(
                        model_id = %model_id,
                        label,
                        error = %e,
                        "CoordinatorActor: dispatch failed"
                    );
                    return DispatchOutcome::Failed;
                }
            }
        }

        if any_at_capacity {
            tracing::Span::current().record("outcome", "cap");
            DispatchOutcome::AtCapacity
        } else {
            tracing::Span::current().record("outcome", "error");
            DispatchOutcome::Failed
        }
    }

    /// Check whether the GitHub App is configured on this server.
    ///
    /// A configured App is the gate for dispatch because the merge path mints
    /// installation tokens via `djinn_provider::github_app`. Without it every
    /// completed task's PR creation would fail. In test builds we short-circuit
    /// to `true` so dispatch tests don't need to plumb env vars.
    async fn has_github_credentials(&self) -> bool {
        #[cfg(test)]
        {
            true
        }
        #[cfg(not(test))]
        {
            djinn_provider::github_app::app_id().is_ok()
        }
    }

    /// Proposal 1omc hard guard: a task may only dispatch under a real user.
    ///
    /// Returns `true` when the task has no resolved owner: `created_by_user_id`
    /// is NULL, points at no user row, or cannot be verified because the DB
    /// lookup fails.
    /// Such tasks must NOT consume org-shared credentials under no identity; the
    /// caller parks them and emits a loud warning so the ownership regression is
    /// visible instead of silently running ownerless.
    ///
    /// Always `false` under `#[cfg(test)]`: the in-process test suite dispatches
    /// fixtures with no real users seeded, and the production identity invariant
    /// is exercised by the live MCP/session path, not these unit fixtures.
    #[cfg(not(test))]
    async fn task_is_ownerless(&self, task: &djinn_core::models::Task) -> bool {
        let Some(uid) = task.created_by_user_id.as_deref() else {
            return true;
        };
        let creator_lookup = djinn_db::UserRepository::new(self.db.clone())
            .get_by_id(uid)
            .await
            .map(|maybe_user| maybe_user.map(|_| ()));

        task_ownerless_from_creator_lookup(
            &task.short_id,
            task.created_by_user_id.as_deref(),
            creator_lookup,
        )
    }

    #[cfg(test)]
    async fn task_is_ownerless(&self, _task: &djinn_core::models::Task) -> bool {
        false
    }

    /// Find all ready tasks (open, no unresolved blockers, non-epic) and dispatch
    /// those that don't already have an active session.
    #[tracing::instrument(
        name = "djinn.coordinator.ready_pass",
        skip(self),
        fields(pass_kind = "ready", project_filter = ?project_filter)
    )]
    pub(crate) async fn dispatch_ready_tasks(&mut self, project_filter: Option<&str>) {
        // Gate: do not dispatch if the GitHub App isn't configured (ADR-039).
        // PR creation depends on minting installation tokens, which requires
        // GITHUB_APP_ID + private key; without them every dispatch would
        // fail-retry at merge time.
        if !self.has_github_credentials().await {
            tracing::warn!(
                "CoordinatorActor: GitHub App not configured (GITHUB_APP_ID unset or \
                 private key missing), skipping dispatch. Configure the App env vars \
                 before starting execution."
            );
            return;
        }

        // Base per-role eligibility is resolved INSIDE the task loop, scoped to
        // each task's creator's credentials (the same set the worker resolves
        // with) — so we can't precompute one global list. Memoize per
        // (role, creator) for this pass since most tasks share a creator.
        let mut role_models_cache: HashMap<(&'static str, Option<String>), Vec<String>> =
            HashMap::new();

        let repo = self.task_repo();
        let mut ready: Vec<djinn_core::models::Task> = match repo
            .list_ready(ReadyQuery {
                issue_type: None,
                limit: self.dispatch_limit as i64,
                ..Default::default()
            })
            .await
        {
            Ok(tasks) => tasks,
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: list_ready failed");
                return;
            }
        };

        for status in ["needs_task_review", "needs_lead_intervention"] {
            match repo.list_by_status_filtered(status, true).await {
                Ok(mut tasks) => ready.append(&mut tasks),
                Err(e) => {
                    tracing::warn!(error = %e, status, "CoordinatorActor: list_by_status failed");
                }
            }
        }

        let mut seen = HashSet::new();
        ready.retain(|t| seen.insert(t.id.clone()));
        ready.sort_by(|a, b| {
            a.priority
                .cmp(&b.priority)
                .then_with(|| a.created_at.cmp(&b.created_at))
        });

        // Load the durable administrative dispatch-pause snapshot once for this
        // pass. A matching pause is an admission-control deferral, not a task,
        // model, provider, or infrastructure failure: paused tasks below are
        // skipped before any claim/spawn path and before task-specific
        // last-dispatched/failure-streak/cooldown accounting.
        let pause_state = match load_dispatch_pause_state(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
        .await
        {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "CoordinatorActor: failed to load dispatch-pause state; deferring dispatch pass"
                );
                return;
            }
        };

        let paused_ready_task_ids: HashSet<String> = ready
            .iter()
            .filter(|task| matching_task_dispatch_pause(&pause_state, task).is_some())
            .map(|task| task.id.clone())
            .collect();

        // ADR-048 §3A: cancel any in-flight idle consolidation sweep when
        // tasks are ready for dispatch.
        if !ready.is_empty() {
            self.cancel_idle_consolidation();
        }

        let mut exhausted_roles: HashSet<&'static str> = HashSet::new();

        // Expire elapsed cooldowns (value = cooldown EXPIRY instant) and old
        // dispatch timestamps. Keep dispatch timestamps for the full
        // failure-detection window so SLOW failures (a ~30s worker run that
        // fails on empty/throttled provider turns) are still attributed and
        // backed off, instead of slipping past a short window and re-dispatching
        // every tick.
        let prune_now = SystemClock::new().now_instant();
        let expired_cooldown_task_ids: Vec<String> = self
            .dispatch_cooldowns
            .iter()
            .filter_map(|(task_id, expiry)| {
                (*expiry <= prune_now && !paused_ready_task_ids.contains(task_id))
                    .then_some(task_id.clone())
            })
            .collect();
        self.dispatch_cooldowns.retain(|task_id, expiry| {
            *expiry > prune_now || paused_ready_task_ids.contains(task_id)
        });
        for task_id in expired_cooldown_task_ids {
            self.persist_durable_dispatch_state_update(
                &task_id,
                None,
                "cooldown_expired_prune",
                DurableDispatchStateUpdate {
                    cooldown_until: Some(None),
                    ..Default::default()
                },
            )
            .await;
        }
        let expired_last_dispatched_task_ids: Vec<String> = self
            .last_dispatched
            .iter()
            .filter_map(|(task_id, marker)| {
                (marker.instant.elapsed() >= FAILURE_DETECTION_WINDOW
                    && !paused_ready_task_ids.contains(task_id))
                .then_some(task_id.clone())
            })
            .collect();
        self.last_dispatched.retain(|task_id, marker| {
            marker.instant.elapsed() < FAILURE_DETECTION_WINDOW
                || paused_ready_task_ids.contains(task_id)
        });
        for task_id in expired_last_dispatched_task_ids {
            self.persist_durable_dispatch_state_update(
                &task_id,
                None,
                "last_dispatched_expired_prune",
                DurableDispatchStateUpdate {
                    last_dispatched: Some(None),
                    ..Default::default()
                },
            )
            .await;
        }

        // Bug #18 guard: skip any task that already has a running session.
        // Without this the coordinator tick re-dispatches the same task every
        // minute while a worker pod is still doing real work, racking up
        // duplicate K8s Jobs and burning tokens.
        let active_task_ids: HashSet<String> = match SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        )
        .list_active()
        .await
        {
            Ok(sessions) => sessions.into_iter().filter_map(|s| s.task_id).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: failed to load active sessions for dispatch guard; proceeding without it");
                HashSet::new()
            }
        };

        // Per-user, per-model concurrency: current running counts keyed by
        // (creator, model), seeded from the DB and bumped locally on each
        // dispatch this pass. A task only dispatches while its creator is under
        // their own cap for the chosen model — the sole admission control, since
        // the slot pool is elastic (spawns on demand, no global ceiling).
        let mut running_by_user_model: HashMap<(String, String), u32> =
            match SessionRepository::new(
                self.db.clone(),
                crate::events::event_bus_for(&self.events_tx),
            )
            .count_active_by_user_and_model()
            .await
            {
                Ok(rows) => rows
                    .into_iter()
                    .filter_map(|(creator, model, cnt)| {
                        creator.map(|c| ((c, model), u32::try_from(cnt).unwrap_or(0)))
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!(error = %e, "CoordinatorActor: per-user concurrency counts failed; proceeding without caps");
                    HashMap::new()
                }
            };

        // In-flight dispatch ledger overlay. The DB seed above only counts
        // sessions that have reached `running`, but a worker pod takes 20-60s to
        // boot and write that row — so a task dispatched moments ago is invisible
        // to the seed. Dispatch passes that re-fire in that window would re-seed
        // from the stale-low count and overshoot the per-user cap (observed: 8
        // workers dispatched in one ~167ms burst for a cap of 4, because every
        // session row only landed ~20-60s later). Fix: reconcile the ledger
        // against the live slot pool (drop entries whose task the pool no longer
        // runs — completed/freed/evicted), then overlay `max(db, ledger)` so an
        // in-flight dispatch counts against the cap the instant it lands. `max`
        // (not sum) avoids double-counting a task present in both, and keeps the
        // DB as a durable floor that survives a server restart (the in-memory
        // ledger resets, but old `running` rows still gate until reaped).
        self.reconcile_inflight_dispatch_ledger().await;
        overlay_inflight_ledger(&mut running_by_user_model, &self.inflight_dispatches);
        record_dispatch_live_state(
            self.dispatch_cooldowns.len(),
            self.inflight_dispatches.len(),
        );

        // Memoized per-creator cap maps (model_id → max concurrent) for this pass.
        let mut creator_caps: HashMap<String, std::collections::HashMap<String, u32>> =
            HashMap::new();

        // Cache readiness per project across this dispatch pass so we don't
        // hammer the DB on every task.
        let gate_enabled = readiness_gate_enabled();
        let mut readiness_cache: HashMap<String, bool> = HashMap::new();

        for task in ready {
            if let Some(project_id) = project_filter
                && task.project_id != project_id
            {
                continue;
            }
            if let Some((pause_scope, pause_target_id, pause)) =
                matching_task_dispatch_pause(&pause_state, &task)
            {
                tracing::info!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    project_id = %task.project_id,
                    status = %task.status,
                    created_by_user_id = ?task.created_by_user_id,
                    pause_scope,
                    pause_target_id,
                    paused_by = %pause.paused_by,
                    paused_at = %pause.paused_at,
                    reason = %pause.reason,
                    "CoordinatorActor: dispatch deferred by administrative pause"
                );
                continue;
            }
            if active_task_ids.contains(&task.id) {
                tracing::debug!(
                    task_id = %task.short_id,
                    "CoordinatorActor: task already has an active session, skipping dispatch"
                );
                continue;
            }
            // Final stale-snapshot/bypass guard: `ready` is assembled before the
            // dispatch loop and also includes filtered status queues. Re-check
            // blocker edges immediately before any role/model selection so a task
            // that was parked behind an open remediation hold in the meantime —
            // including `review`/`human-review-hold` blockers — cannot spawn a
            // worker from an earlier ready vector or alternate status path.
            match repo.list_blockers(&task.id).await {
                Ok(blockers) if blockers.iter().any(|b| b.status != "closed") => {
                    tracing::debug!(
                        task_id = %task.short_id,
                        task_uuid = %task.id,
                        project_id = %task.project_id,
                        blocker_count = blockers.iter().filter(|b| b.status != "closed").count(),
                        "CoordinatorActor: task has unresolved blockers, skipping dispatch"
                    );
                    continue;
                }
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        task_uuid = %task.id,
                        project_id = %task.project_id,
                        error = %e,
                        "CoordinatorActor: failed to re-check blockers before dispatch; deferring task"
                    );
                    continue;
                }
            }
            // Human-review hold guard: a remediation task tagged with
            // `human-review-hold` is a terminal escalation that requires a
            // human to close it. No agent (planner, worker, reviewer) must
            // ever be dispatched for it. Without this guard the planner
            // review-claims rule (`open` + `issue_type=review`) matches the
            // hold task and dispatches a planner session against it,
            // defeating the park.
            if task.labels.contains("human-review-hold") {
                tracing::debug!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    "CoordinatorActor: skipping dispatch — task carries human-review-hold label (human-only hold)"
                );
                continue;
            }
            // Proposal 1omc: every dispatch must run under a real user. Refuse to
            // dispatch a task with no resolved owner. Park it loudly rather than
            // silently consuming org-shared credentials under no identity — this
            // surfaces an ownership regression instead of running ownerless.
            if self.task_is_ownerless(&task).await {
                tracing::warn!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    project_id = %task.project_id,
                    created_by_user_id = ?task.created_by_user_id,
                    "CoordinatorActor: REFUSING dispatch — task has no real owner \
                     (created_by_user_id is NULL, dangling, or could not be verified). \
                     Every task must run under a real user (proposal 1omc); parking it."
                );
                continue;
            }
            if gate_enabled {
                let ready_for_dispatch = match readiness_cache.get(&task.project_id) {
                    Some(v) => *v,
                    None => {
                        let project_repo = djinn_db::ProjectRepository::new(
                            self.db.clone(),
                            crate::events::event_bus_for(&self.events_tx),
                        );
                        let ok = match project_repo.get_dispatch_readiness(&task.project_id).await {
                            Ok(Some(r)) => r.is_ready_for_dispatch(),
                            // Unknown project or DB error: fail-closed so a
                            // broken setup never silently burns tokens.
                            _ => false,
                        };
                        readiness_cache.insert(task.project_id.clone(), ok);
                        ok
                    }
                };
                if !ready_for_dispatch {
                    tracing::debug!(
                        task_id = %task.short_id,
                        project_id = %task.project_id,
                        "CoordinatorActor: dispatch deferred — project devcontainer image not ready"
                    );
                    continue;
                }
            }
            // Skip tasks still inside an active dispatch cooldown.
            if self.dispatch_cooldowns.contains_key(&task.id) {
                record_dispatch_outcome(djinn_telemetry::dispatch::OUTCOME_COOLDOWN);
                tracing::debug!(outcome = "cooldown", task_id = %task.short_id, task_uuid = %task.id);
                tracing::debug!(
                    task_id = %task.short_id,
                    "CoordinatorActor: task in dispatch cooldown, skipping"
                );
                continue;
            }
            let ctx = DispatchContext;
            let Some(role) = self.role_registry.dispatch_role_for_task(&task, &ctx) else {
                continue;
            };
            // Planner intervention for a stuck worker task (trigger A): if a
            // worker task is about to be re-dispatched for the Nth time
            // (`reopen_count >= REOPEN_INTERVENTION_THRESHOLD` — e.g. the
            // internal reviewer keeps rejecting the SAME acceptance criterion),
            // route it to a Planner intervention pass instead of burning
            // another worker session. The Planner decides how to unstick it
            // (decompose, rescope, close, or apply-as-feedback) via the
            // existing intervention machinery (planner Workflow C). This is a
            // no-op (returns false) for non-worker roles, tasks under the
            // threshold, or tasks already routed at this reopen count.
            if role == "worker" && self.maybe_intervene_on_stuck_task(&task).await {
                // The planner escalation dispatched a new session (under the
                // same creator, potentially the same model). Bump the local
                // per-(creator, model) count so a later task in THIS pass sees
                // reduced capacity — the inflight ledger is already updated
                // inside dispatch_planner_escalation, but the local
                // running_by_user_model was seeded before this admission.
                self.bump_local_cap_for_last_planner_admission(&mut running_by_user_model)
                    .await;
                continue;
            }
            // A task that is dispatch-ready again (no active session — guarded
            // above) after a recent dispatch to the SAME role means the prior
            // run failed. A different role means the previous stage succeeded
            // and handed the task off (e.g. worker → reviewer), so clear any
            // old streak and let it proceed.
            if let Some(marker) = self.last_dispatched.remove(&task.id) {
                let current_streak = self
                    .dispatch_failure_streak
                    .get(&task.id)
                    .copied()
                    .unwrap_or(0);
                // Read-and-clear the last-failure signal the slot runner stashed
                // for THIS task on the shared HealthTracker (A2 side-channel).
                // `None` when the prior run's failure wasn't a typed provider
                // error (a structural/crash failure, a missing credential, etc.),
                // in which case the ordinary streak/ladder logic applies
                // unchanged.
                let provider_failure = self.health.take_task_provider_failure(&task.id);
                match classify_reappearing_dispatch(marker, role, current_streak) {
                    Some(ReappearingDispatch::SameRoleFailure {
                        next_streak,
                        cooldown,
                    }) => {
                        // A3: a throttle/rate-limit reappearance is a transient
                        // provider fault, NOT evidence the task is structurally
                        // undispatchable — so it must not advance the terminal
                        // `dispatch_failure_streak` toward MAX (which would close a
                        // perfectly healthy task). The task still backs off (the
                        // escalating cooldown below still grows with the
                        // reappearance) and the per-(scope,model) breaker still
                        // fails over; only the terminal-close counter is spared.
                        let throttle = provider_failure.is_some_and(|f| f.throttle);

                        // Second-strike Planner escalation for provider-error
                        // FAILED sessions. A genuine (non-throttle) typed
                        // provider failure that recurs for the same task is the
                        // poisoned-transcript-400 / dead-credential / persistent
                        // server-fault class: redispatch reproduces it
                        // identically, so riding the backoff ladder toward the
                        // streak-10 terminal close just burns attempts with
                        // nobody deciding what to do. The cycling gate (trigger
                        // B) below excludes provider faults by design, and the
                        // stall-cancel escalation only covers coordinator stall
                        // kills — so without this the failure has no Planner
                        // path. Count consecutive such failures (reset when the
                        // task's status advances, mirroring the stall streak)
                        // and hand the task to the Planner on the
                        // FAILURE_ESCALATION_THRESHOLD-th strike instead of
                        // another doomed redispatch.
                        if provider_failure.is_some()
                            && !throttle
                            && self
                                .maybe_escalate_provider_failure_streak(&task, role)
                                .await
                        {
                            // Bump the local cap to reflect the planner session
                            // the intervention just dispatched (same as trigger
                            // B and the stuck-task path).
                            self.bump_local_cap_for_last_planner_admission(
                                &mut running_by_user_model,
                            )
                            .await;
                            continue;
                        }

                        // Trigger B: the task keeps reappearing for the SAME
                        // role with no typed provider failure to blame — its
                        // runs complete but the task never converges (the
                        // review-cycle bounce that never passes through `open`,
                        // so trigger A's reopen_count never arms). Route it to
                        // a Planner intervention instead of riding the ladder
                        // to the terminal close at MAX_DISPATCH_FAILURES, which
                        // would force-close a task whose durable work may be
                        // fine. Falls through to the ordinary ladder when the
                        // Planner was already routed for this loop (idempotency
                        // marker) — the terminal close then remains the final
                        // backstop.
                        if should_route_cycling_intervention(
                            role,
                            next_streak,
                            provider_failure.is_some(),
                        ) && self
                            .maybe_intervene_on_cycling_task(&task, role, next_streak)
                            .await
                        {
                            self.dispatch_failure_streak.remove(&task.id);
                            self.provider_failure_streak.remove(&task.id);
                            self.dispatch_cooldowns.remove(&task.id);
                            self.clear_durable_dispatch_backoff_state(
                                &task.id,
                                Some(&task.short_id),
                                "cycling_planner_intervention_handoff_clear",
                            )
                            .await;
                            // Bump local cap to reflect the planner session the
                            // intervention just dispatched (same as Trigger A).
                            self.bump_local_cap_for_last_planner_admission(
                                &mut running_by_user_model,
                            )
                            .await;
                            continue;
                        }

                        // After MAX consecutive same-role failures the task is
                        // structurally doomed (e.g. its run can never complete);
                        // fail it terminally instead of looping forever. Skipped
                        // for throttles (A3): a transient quota window must never
                        // terminally close the task.
                        if !throttle && next_streak >= MAX_DISPATCH_FAILURES {
                            self.terminally_fail_task(
                                &task,
                                role,
                                "repeated dispatch failures: the task could not complete after \
                                 multiple attempts. Resolve the underlying issue and reopen.",
                            )
                            .await;
                            self.dispatch_failure_streak.remove(&task.id);
                            self.provider_failure_streak.remove(&task.id);
                            self.dispatch_cooldowns.remove(&task.id);
                            self.inflight_dispatches.remove(&task.id);
                            self.clear_durable_dispatch_backoff_state(
                                &task.id,
                                Some(&task.short_id),
                                "same_role_terminal_close_clear",
                            )
                            .await;
                            continue;
                        }

                        // A3: leave the terminal streak at its current value on a
                        // throttle (don't persist the advanced `next_streak`).
                        let stored_streak =
                            stored_streak_after_failure(current_streak, next_streak, throttle);
                        if stored_streak > 0 {
                            self.dispatch_failure_streak
                                .insert(task.id.clone(), stored_streak);
                        } else {
                            self.dispatch_failure_streak.remove(&task.id);
                        }

                        // A6: honor a provider-stated reset as a redispatch floor.
                        // `cooldown` is the escalating ladder value for this
                        // reappearance; when the provider stated a Retry-After /
                        // rate-limit-reset that exceeds it, redispatch no earlier
                        // than that reset (otherwise a 5-hour quota window would be
                        // probed every ~30 min, burning failover). The provider
                        // reset is deliberately allowed to EXCEED the ladder's
                        // 30-min ceiling — that's the whole point — but is clamped
                        // to a hard safety max so a malformed value can't wedge the
                        // task forever.
                        let retry_after_ms = provider_failure.and_then(|f| f.retry_after_ms);
                        let effective_cooldown =
                            apply_provider_retry_floor(cooldown, retry_after_ms);
                        if effective_cooldown > cooldown {
                            tracing::info!(
                                task_id = %task.short_id,
                                role,
                                ladder_cooldown_secs = cooldown.as_secs(),
                                provider_floor_secs = effective_cooldown.as_secs(),
                                "CoordinatorActor: applying provider-stated retry-after as redispatch floor"
                            );
                        }

                        tracing::warn!(
                            task_id = %task.short_id,
                            role,
                            streak = stored_streak,
                            throttle,
                            cooldown_secs = effective_cooldown.as_secs(),
                            "CoordinatorActor: repeated task failure — backing off dispatch (escalating cooldown)"
                        );
                        self.dispatch_cooldowns.insert(
                            task.id.clone(),
                            SystemClock::new().now_instant() + effective_cooldown,
                        );
                        self.persist_durable_dispatch_state_update(
                            &task.id,
                            Some(&task.short_id),
                            "same_role_failure_backoff",
                            DurableDispatchStateUpdate {
                                failure_streak: Some(stored_streak),
                                cooldown_until: Some(dispatch_wall_clock_after(effective_cooldown)),
                                last_dispatched: Some(None),
                                ..Default::default()
                            },
                        )
                        .await;
                        continue;
                    }
                    Some(ReappearingDispatch::RoleTransition) | None => {
                        self.dispatch_failure_streak.remove(&task.id);
                        self.provider_failure_streak.remove(&task.id);
                        self.dispatch_cooldowns.remove(&task.id);
                        self.clear_durable_dispatch_backoff_state(
                            &task.id,
                            Some(&task.short_id),
                            "role_transition_dispatch_state_clear",
                        )
                        .await;
                    }
                }
            }
            if exhausted_roles.contains(role) {
                continue;
            }
            let creator = task.created_by_user_id.clone();

            // Base per-role eligibility, scoped to THIS task's creator's
            // credentials (own + org-shared) — the same set the worker resolves
            // with — so the coordinator never offers a model it can't auth.
            // Memoized per (role, creator) for the pass.
            let base_model_ids = match role_models_cache.get(&(role, creator.clone())) {
                Some(v) => v.clone(),
                None => {
                    let v = self
                        .resolve_dispatch_models_for_role(role, creator.as_deref())
                        .await;
                    role_models_cache.insert((role, creator.clone()), v.clone());
                    v
                }
            };

            // Final fallback list, precedence: creator's per-user selection →
            // project default-role preference → role base. All scoped to the
            // creator, so selection and runtime resolution stay consistent.
            //
            // Post-intervention worker retries (intervention_count >= 1) are
            // routed to the plan lane when the default-on feature flag is set,
            // while keeping the `worker` role and `ModelLane::for_role` mapping
            // unchanged.
            let effective_lane = post_intervention_lane::effective_dispatch_lane(
                role,
                task.intervention_count,
                post_intervention_lane::use_plan_lane_for_post_intervention_workers(),
            );
            let user_model_ids = self
                .resolve_user_model_priority_with_lane(creator.as_deref(), role, effective_lane)
                .await;
            let model_preference_ids = self
                .resolve_role_model_preference(&task.project_id, role, creator.as_deref())
                .await;
            let mut seen = std::collections::HashSet::new();
            let mut model_ids: Vec<String> = Vec::with_capacity(
                user_model_ids.len() + model_preference_ids.len() + base_model_ids.len(),
            );
            for id in user_model_ids
                .iter()
                .chain(model_preference_ids.iter())
                .chain(base_model_ids.iter())
            {
                if seen.insert(id.clone()) {
                    model_ids.push(id.clone());
                }
            }

            // uv3p Part B: forced model rotation on the post-intervention retry
            // path. When the human-park rung declined to park because no session
            // has reached submit_work yet, it redispatches — but a redispatch to
            // the SAME model that just terminated pre-submission (loop-guard trip,
            // infra death) would loop identically (kibj went back to k2p7 twice).
            // Drop the models whose post-intervention sessions terminated without
            // submitting, derived from durable session history so no new state is
            // needed. Degrades to the unfiltered list when exclusion would empty
            // it (only one viable model → plan-lane retry, then park at the bound).
            if role == "worker" && task.intervention_count >= 1 {
                let history = self.post_intervention_history(&task).await;
                if !history.non_attempt_models.is_empty() {
                    let filtered: Vec<String> = model_ids
                        .iter()
                        .filter(|m| !history.non_attempt_models.contains(m))
                        .cloned()
                        .collect();
                    if !filtered.is_empty() && filtered.len() < model_ids.len() {
                        tracing::info!(
                            task_id = %task.short_id,
                            excluded = ?history.non_attempt_models,
                            "uv3p: forcing model rotation on post-intervention redispatch — excluding models that terminated pre-submission"
                        );
                        model_ids = filtered;
                    }
                }
            }

            // Cross-model ("Thorough") review: when this is a reviewer dispatch
            // and the creator has `diverse_review` on, steer the fallback list so
            // the first viable model id differs from the one that implemented the
            // task. Reorders in place (entries != implementer first, preserving
            // priority); collapses to same-model when nothing else is viable.
            if role == "reviewer" {
                self.apply_diverse_review_ordering(&task, creator.as_deref(), &mut model_ids)
                    .await;
            }

            // No model whose provider this task's owner has connected → the task
            // is structurally undispatchable (the canary). Don't loop it forever
            // (wedging the task open). Back off with the
            // escalating cooldown, and after MAX consecutive misses fail it
            // terminally with an actionable reason.
            if model_ids.is_empty() {
                let streak = {
                    let s = self
                        .dispatch_failure_streak
                        .entry(task.id.clone())
                        .or_insert(0);
                    *s = s.saturating_add(1);
                    *s
                };
                if streak >= MAX_DISPATCH_FAILURES {
                    self.terminally_fail_task(
                        &task,
                        role,
                        "no model available for this task's owner: none of the role's configured \
                         models have a provider connected for them. Connect a provider/model for \
                         the owner and reopen.",
                    )
                    .await;
                    self.dispatch_failure_streak.remove(&task.id);
                    self.dispatch_cooldowns.remove(&task.id);
                    self.inflight_dispatches.remove(&task.id);
                    self.clear_durable_dispatch_backoff_state(
                        &task.id,
                        Some(&task.short_id),
                        "no_eligible_model_terminal_close_clear",
                    )
                    .await;
                } else {
                    let cooldown = escalating_dispatch_cooldown(streak);
                    tracing::warn!(
                        task_id = %task.short_id,
                        role,
                        streak,
                        cooldown_secs = cooldown.as_secs(),
                        "CoordinatorActor: no eligible model for task owner — backing off"
                    );
                    self.dispatch_cooldowns
                        .insert(task.id.clone(), SystemClock::new().now_instant() + cooldown);
                    self.persist_durable_dispatch_state_update(
                        &task.id,
                        Some(&task.short_id),
                        "no_eligible_model_backoff",
                        DurableDispatchStateUpdate {
                            failure_streak: Some(streak),
                            cooldown_until: Some(dispatch_wall_clock_after(cooldown)),
                            last_dispatched: Some(None),
                            ..Default::default()
                        },
                    )
                    .await;
                }
                continue;
            }

            // Per-user cap gate: keep only models where this task's creator is
            // under their own concurrency cap (default 1 when unset). Eligible
            // but all-at-cap ⇒ the user is simply busy — skip this pass (NOT
            // terminal, no streak/cooldown; retries next tick as their sessions
            // free). Tasks with no creator (legacy NULL) are ungated.
            if let Some(c) = creator.as_deref() {
                if !creator_caps.contains_key(c) {
                    let caps = djinn_db::UserSettingsRepository::new(self.db.clone())
                        .get(c)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|s| s.max_sessions)
                        .unwrap_or_default();
                    creator_caps.insert(c.to_string(), caps);
                }
                let caps = &creator_caps[c];
                model_ids.retain(|m| {
                    model_under_user_cap(
                        &running_by_user_model,
                        c,
                        m,
                        caps.get(m).copied().unwrap_or(1),
                    )
                });
                if model_ids.is_empty() {
                    record_dispatch_outcome(djinn_telemetry::dispatch::OUTCOME_CAP);
                    tracing::debug!(outcome = "cap", task_id = %task.short_id, role);
                    tracing::debug!(
                        task_id = %task.short_id,
                        role,
                        "CoordinatorActor: task owner at per-model concurrency cap — deferring"
                    );
                    continue;
                }
            }
            let model_ids: &[String] = &model_ids;

            match self.pool.has_session(&task.id).await {
                Ok(true) => continue,
                Ok(false) => {}
                Err(PoolError::ActorDead) => {
                    tracing::error!("CoordinatorActor: slot pool actor dead, aborting dispatch");
                    return;
                }
                Err(e) => {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: has_session query failed"
                    );
                    continue;
                }
            }

            let Some(project_path) = self.project_path_for_id(&task.project_id).await else {
                tracing::warn!(task_id = %task.short_id, project_id = %task.project_id, "CoordinatorActor: project path not found, skipping dispatch");
                continue;
            };

            // Phase 3 PR 8: architect-only pre-dispatch `await_fresh` gate.
            // Blocks up to 45s for a warm canonical graph; on timeout the
            // warmer returns Ok and the architect proceeds best-effort.
            // Other roles proceed immediately (per ADR: workers tolerate
            // a stale skeleton).
            if role == "architect"
                && let Some(warmer) = self.graph_warmer.clone()
            {
                let pid = task.project_id.clone();
                let _ = warmer
                    .await_fresh(
                        &pid,
                        std::time::Duration::from_secs(300),
                        std::time::Duration::from_secs(45),
                    )
                    .await;
            }

            // Coordinator-side resume-via-git selection: when resume is
            // enabled and the selector produced a metadata struct, serialize
            // it and route the dispatch through `dispatch_with_resume_metadata`
            // so the selection lands on `TaskRunSpec::resume_lifecycle_metadata`
            // (read by downstream prompt/model/merge work in siblings `48ru`/
            // `twsk`/`sy0g`). When resume is disabled OR the selector
            // returned `None`, fall back to the legacy `dispatch` call so
            // existing default/off dispatch behavior is byte-for-byte
            // preserved.
            let resume_metadata = self
                .select_resume_lifecycle_metadata_for_dispatch(&task)
                .await;
            let resume_metadata_json = resume_metadata
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .map_err(|e| {
                    tracing::warn!(
                        task_id = %task.short_id,
                        error = %e,
                        "CoordinatorActor: failed to serialize ResumeLifecycleMetadata for re-dispatch; \
                         proceeding without resume metadata"
                    );
                    e
                })
                .ok()
                .flatten();

            let task_id = task.id.clone();
            let project_path_owned = project_path.clone();
            let outcome = self
                .try_dispatch_to_pool(
                    &task.short_id,
                    role,
                    task.reopen_count.max(0) as u32,
                    creator.as_deref(),
                    model_ids,
                    |pool, model_id| {
                        let pool = pool.clone();
                        let tid = task_id.clone();
                        let pp = project_path_owned.clone();
                        let mid = model_id.to_owned();
                        let resume = resume_metadata_json.clone();
                        async move {
                            match resume {
                                Some(metadata) => {
                                    pool.dispatch_with_resume_metadata(
                                        &tid,
                                        &pp,
                                        &mid,
                                        Some(metadata),
                                    )
                                    .await
                                }
                                None => pool.dispatch(&tid, &pp, &mid).await,
                            }
                        }
                    },
                )
                .await;

            match outcome {
                DispatchOutcome::Dispatched => {
                    record_dispatch_outcome(djinn_telemetry::dispatch::OUTCOME_OK);
                    tracing::info!(outcome = "ok", task_id = %task.short_id, role);
                    tracing::info!(
                        task_id = %task.short_id,
                        task_uuid = %task.id,
                        project_id = %task.project_id,
                        status = %task.status,
                        priority = task.priority,
                        role,
                        project_path,
                        "CoordinatorActor: task dispatched"
                    );
                    self.last_dispatched.insert(
                        task.id.clone(),
                        DispatchMarker {
                            instant: SystemClock::new().now_instant(),
                            role: role.to_owned(),
                        },
                    );
                    self.persist_durable_dispatch_state_update(
                        &task.id,
                        Some(&task.short_id),
                        "successful_dispatch_marker",
                        DurableDispatchStateUpdate {
                            cooldown_until: Some(None),
                            last_dispatched: Some(
                                dispatch_wall_clock_now().map(|ts| (ts, role.to_owned())),
                            ),
                            ..Default::default()
                        },
                    )
                    .await;
                    self.dispatched += 1;
                    // Bump the per-user running count for the model actually
                    // used (the first health-available one — the elastic pool
                    // accepts it), so further same-creator+model tasks in THIS
                    // pass respect the cap before the session row is visible.
                    if let Some(c) = creator.as_deref()
                        && let Some(used) = model_ids
                            .iter()
                            .find(|m| self.health.is_available(Some(c), m))
                    {
                        *running_by_user_model
                            .entry((c.to_string(), used.clone()))
                            .or_insert(0) += 1;
                        #[cfg(test)]
                        observe_dispatch_cap_count(
                            DispatchCapObservationStage::InflightIncremented,
                            c,
                            used,
                            running_by_user_model
                                .get(&(c.to_string(), used.clone()))
                                .copied()
                                .unwrap_or(0),
                        );
                        // Record in the in-flight ledger via the shared
                        // admission helper so the NEXT pass (and the planner
                        // escalation path) counts this dispatch against the
                        // cap immediately — before its `running` session row
                        // lands (pod boot lags 20-60s). Reconciled against
                        // the live pool at the top of each pass, so it drops
                        // out the moment the task completes.
                        self.record_inflight_dispatch(
                            &task.id,
                            Some(&task.short_id),
                            Some(c),
                            used,
                        )
                        .await;
                    }
                }
                DispatchOutcome::AtCapacity => {
                    record_dispatch_outcome(djinn_telemetry::dispatch::OUTCOME_CAP);
                    tracing::debug!(outcome = "cap", task_id = %task.short_id, role);
                    tracing::debug!(
                        task_id = %task.short_id,
                        task_uuid = %task.id,
                        project_id = %task.project_id,
                        role,
                        status = %task.status,
                        candidate_models = model_ids.len(),
                        "CoordinatorActor: all models at capacity for role"
                    );
                    exhausted_roles.insert(role);
                }
                DispatchOutcome::PoolDead => {
                    record_dispatch_outcome(djinn_telemetry::dispatch::OUTCOME_ERROR);
                    return;
                }
                DispatchOutcome::Failed => {
                    let breaker_open_for_all_candidates = model_ids
                        .iter()
                        .all(|model_id| !self.health.is_available(creator.as_deref(), model_id));
                    if breaker_open_for_all_candidates {
                        record_dispatch_outcome(djinn_telemetry::dispatch::OUTCOME_BREAKER);
                        tracing::debug!(outcome = "breaker", task_id = %task.short_id, role);
                    } else {
                        record_dispatch_outcome(djinn_telemetry::dispatch::OUTCOME_ERROR);
                        tracing::debug!(outcome = "error", task_id = %task.short_id, role);
                    }
                    tracing::debug!(
                        task_id = %task.short_id,
                        task_uuid = %task.id,
                        project_id = %task.project_id,
                        role,
                        status = %task.status,
                        candidate_models = model_ids.len(),
                        "CoordinatorActor: no model could accept dispatch"
                    );
                }
            }
        }
        self.publish_status();
    }
}

/// E6 regression tests.
///
/// Part A locks the reopen→Planner escalation gate for the merge-queue-rejection
/// path: a merge-queue rejection reopens the task via `PrCiFailed`, which must
/// (1) land the task at `open` while *incrementing* `reopen_count`, (2) route the
/// reopened task to the `worker` role, and (3) cross the
/// `REOPEN_INTERVENTION_THRESHOLD` after enough rejections — exactly the three
/// preconditions for the `role == "worker" && maybe_intervene_on_stuck_task(..)`
/// gate in `dispatch_ready_tasks`. (The intervention body itself is covered in
/// `mod.rs` tests.)
///
/// Part B locks the proactive-rebase non-fatal contract: a genuine rebase
/// conflict surfaces as an `Err` from `djinn_git::rebase_with_retry` AND leaves a
/// clean (not mid-rebase) workspace — so `proactively_rebase_approved_branch`
/// (which swallows that `Err` and proceeds) can never wedge dispatch.
#[cfg(test)]
mod inflight_ledger_tests {
    use super::*;
    use djinn_core::models::{DispatchPause, DispatchPauseState, Task};
    use std::sync::{Arc as StdArc, Mutex as StdMutex};
    use std::time::Duration;

    fn pause() -> DispatchPause {
        DispatchPause {
            paused_by: "admin".to_owned(),
            paused_at: "2026-06-12T00:00:00Z".to_owned(),
            reason: "maintenance".to_owned(),
            expires_at: None,
        }
    }

    fn task(project_id: &str, creator: Option<&str>) -> Task {
        Task {
            id: "task-uuid".to_owned(),
            project_id: project_id.to_owned(),
            short_id: "task".to_owned(),
            epic_id: None,
            title: "title".to_owned(),
            description: String::new(),
            design: String::new(),
            issue_type: "task".to_owned(),
            status: "open".to_owned(),
            priority: 0,
            owner: "owner".to_owned(),
            labels: "[]".to_owned(),
            acceptance_criteria: "[]".to_owned(),
            reopen_count: 0,
            continuation_count: 0,
            total_reopen_count: 0,
            intervention_count: 0,
            last_intervention_at: None,
            created_at: "2026-06-12T00:00:00Z".to_owned(),
            updated_at: "2026-06-12T00:00:00Z".to_owned(),
            closed_at: None,
            close_reason: None,
            merge_commit_sha: None,
            pr_url: None,
            merge_conflict_metadata: None,
            memory_refs: "[]".to_owned(),
            agent_type: None,
            created_by_user_id: creator.map(str::to_owned),
            ci_status: "unknown".to_owned(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".to_owned(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            unresolved_blocker_count: 0,
        }
    }

    #[test]
    fn task_ownerless_guard_refuses_null_and_unresolved_creators() {
        assert!(task_ownerless_from_creator_lookup(
            "task",
            None,
            Ok::<Option<()>, &str>(Some(())),
        ));
        assert!(task_ownerless_from_creator_lookup(
            "task",
            Some("missing-user-id"),
            Ok::<Option<()>, &str>(None),
        ));
        assert!(task_ownerless_from_creator_lookup(
            "task",
            Some("lookup-error-user-id"),
            Err::<Option<()>, &str>("database unavailable"),
        ));
        assert!(!task_ownerless_from_creator_lookup(
            "task",
            Some("real-user-id"),
            Ok::<Option<()>, &str>(Some(())),
        ));
    }

    #[test]
    fn matching_dispatch_pause_honors_global_project_and_user_scopes() {
        let mut state = state_with_global(pause());
        assert_eq!(
            matching_task_dispatch_pause(&state, &task("project-a", Some("user-a")))
                .map(|(scope, target, _)| (scope, target)),
            Some(("global", None))
        );

        state = state_with_project("project-a", pause());
        assert_eq!(
            matching_task_dispatch_pause(&state, &task("project-a", Some("user-b")))
                .map(|(scope, target, _)| (scope, target)),
            Some(("project", Some("project-a".to_owned())))
        );
        assert!(matching_task_dispatch_pause(&state, &task("project-b", Some("user-b"))).is_none());

        state = state_with_user("user-a", pause());
        assert_eq!(
            matching_task_dispatch_pause(&state, &task("project-b", Some("user-a")))
                .map(|(scope, target, _)| (scope, target)),
            Some(("user", Some("user-a".to_owned())))
        );
        assert!(matching_task_dispatch_pause(&state, &task("project-b", Some("user-b"))).is_none());
        assert!(matching_task_dispatch_pause(&state, &task("project-b", None)).is_none());
    }

    fn state_with_global(global: DispatchPause) -> DispatchPauseState {
        DispatchPauseState {
            global: Some(global),
            ..Default::default()
        }
    }

    fn state_with_project(project_id: &str, project: DispatchPause) -> DispatchPauseState {
        let mut projects = std::collections::HashMap::new();
        projects.insert(project_id.to_owned(), project);
        DispatchPauseState {
            projects,
            ..Default::default()
        }
    }

    fn state_with_user(user_id: &str, user: DispatchPause) -> DispatchPauseState {
        let mut users = std::collections::HashMap::new();
        users.insert(user_id.to_owned(), user);
        DispatchPauseState {
            users,
            ..Default::default()
        }
    }

    fn key(creator: &str, model: &str) -> (String, String) {
        (creator.to_string(), model.to_string())
    }

    const WND1_READY_TASK_COUNT: usize = 10;
    const WND1_STABLE_MODEL_ID: &str = "test/mock";
    const WND1_DISPATCH_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);
    const WND1_CONTROLLED_RUNTIME_GUARD: Duration = Duration::from_secs(60);

    struct Wnd1DispatchFixture {
        project_id: String,
        project_path: String,
        created_by_user_id: String,
        model_id: String,
        task_ids: Vec<String>,
    }

    #[derive(Clone)]
    struct Wnd1ControlledRuntime {
        started_tx: tokio::sync::mpsc::UnboundedSender<String>,
        releases:
            std::sync::Arc<std::sync::Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>,
    }

    impl Wnd1ControlledRuntime {
        fn new() -> (Self, tokio::sync::mpsc::UnboundedReceiver<String>) {
            let (started_tx, started_rx) = tokio::sync::mpsc::unbounded_channel();
            (
                Self {
                    started_tx,
                    releases: std::sync::Arc::new(std::sync::Mutex::new(HashMap::new())),
                },
                started_rx,
            )
        }

        fn spawn_pool(
            &self,
            db: &djinn_db::Database,
            cancel: tokio_util::sync::CancellationToken,
            max_slots: u32,
        ) -> djinn_slot::SlotPoolHandle {
            let started_tx = self.started_tx.clone();
            let releases = self.releases.clone();
            djinn_slot::SlotPoolHandle::spawn_with_factory(
                crate::test_helpers::agent_context_from_db(db.clone(), cancel.clone()),
                cancel,
                djinn_slot::SlotPoolConfig {
                    models: vec![djinn_slot::ModelSlotConfig {
                        model_id: WND1_STABLE_MODEL_ID.to_owned(),
                        max_slots,
                        roles: ["worker".to_owned()].into_iter().collect(),
                    }],
                    role_priorities: HashMap::new(),
                },
                std::sync::Arc::new(move |slot_id, model_id, event_tx, app_state, cancel| {
                    let started_tx = started_tx.clone();
                    let releases = releases.clone();
                    let runner: djinn_slot::TestLifecycleRunner = std::sync::Arc::new(
                        move |task_id,
                              _project_path,
                              _model_id,
                              _app_state,
                              kill,
                              _pause,
                              _resume_lifecycle_metadata| {
                            let started_tx = started_tx.clone();
                            let releases = releases.clone();
                            Box::pin(async move {
                                let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                                releases
                                    .lock()
                                    .expect("wnd1 release map mutex")
                                    .insert(task_id.clone(), release_tx);
                                let _ = started_tx.send(task_id.clone());
                                tokio::select! {
                                    _ = release_rx => {}
                                    _ = kill.cancelled() => {}
                                    _ = tokio::time::sleep(WND1_CONTROLLED_RUNTIME_GUARD) => {}
                                }
                                Ok(())
                            })
                        },
                    );
                    djinn_slot::SlotHandle::spawn_with_test_runner(
                        slot_id, model_id, event_tx, app_state, cancel, runner,
                    )
                }),
            )
        }

        async fn release(&self, task_id: &str) {
            let sender = self
                .releases
                .lock()
                .expect("wnd1 release map mutex")
                .remove(task_id);
            if let Some(sender) = sender {
                let _ = sender.send(());
            }
        }
    }

    async fn seed_wnd1_ready_worker_tasks(
        db: &djinn_db::Database,
        count: usize,
    ) -> Wnd1DispatchFixture {
        assert!(
            count >= WND1_READY_TASK_COUNT,
            "wnd1 dispatch fixtures must seed at least {WND1_READY_TASK_COUNT} ready tasks"
        );

        let event_bus = djinn_core::events::EventBus::noop();
        let project = crate::test_helpers::create_test_project(db).await;
        let project_path =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        std::fs::create_dir_all(&project_path).expect("create wnd1 fixture project dir");
        let user = djinn_db::UserRepository::new(db.clone())
            .upsert_from_github(
                985_100,
                "wnd1-cap-fixture-user",
                Some("wnd1 cap fixture user"),
                None,
            )
            .await
            .expect("create wnd1 fixture user");
        let user_id = user.id.clone();

        let settings = djinn_db::UserSettingsRepository::new(db.clone());
        settings
            .upsert_lanes(
                &user_id,
                &djinn_core::models::ModelLanes::from_flat(vec![WND1_STABLE_MODEL_ID.to_owned()]),
            )
            .await
            .expect("configure wnd1 fixture selected model");

        let task_repo = djinn_db::TaskRepository::new(db.clone(), event_bus);
        let task_ids = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                let mut ids = Vec::with_capacity(count);
                for i in 0..count {
                    let task = task_repo
                        .create_in_project(
                            &project.id,
                            None,
                            &format!("wnd1 dispatch race fixture task {i}"),
                            "Ready worker task for wnd1 per-user cap race tests.",
                            "",
                            "task",
                            i64::try_from(i).expect("fixture task index fits i64"),
                            "worker",
                            Some("open"),
                            Some("[]"),
                        )
                        .await
                        .expect("seed wnd1 ready worker task");
                    ids.push(task.id);
                }
                ids
            })
            .await;

        Wnd1DispatchFixture {
            project_id: project.id,
            project_path: project_path.to_string_lossy().into_owned(),
            created_by_user_id: user_id,
            model_id: WND1_STABLE_MODEL_ID.to_owned(),
            task_ids,
        }
    }

    fn wnd1_actor_for_tests(
        db: &djinn_db::Database,
        events_tx: &tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
        controlled_runtime: &Wnd1ControlledRuntime,
        max_slots: u32,
    ) -> CoordinatorActor {
        let cancel = tokio_util::sync::CancellationToken::new();
        CoordinatorActor {
            receiver: tokio::sync::mpsc::channel(1).1,
            events: events_tx.subscribe(),
            cancel: cancel.clone(),
            tick: tokio::time::interval(STUCK_INTERVAL),
            db: db.clone(),
            events_tx: events_tx.clone(),
            pool: controlled_runtime.spawn_pool(db, cancel, max_slots),
            catalog: CatalogService::new(),
            health: djinn_provider::catalog::health::HealthTracker::new(),
            role_registry: std::sync::Arc::new(crate::roles::RoleRegistry::new()),
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
            consolidation_runner: std::sync::Arc::new(
                crate::consolidation::DbConsolidationRunner::new(db.clone()),
            ),
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
            stall_progress_watermark: HashMap::new(),
            stall_cancel_streak: HashMap::new(),
            provider_failure_streak: HashMap::new(),
            last_idle_consolidation: None,
            idle_consolidation_cancel: None,
            idle_consolidation_handle: None,
            pr_cleanup_config: PrCleanupConfig::default(),
            worker_lifecycle_config: crate::WorkerLifecycleConfig::default(),
            active_refinements: HashMap::new(),
            refinement_sessions: HashMap::new(),
            dispatched: 0,
            recovered: 0,
        }
    }

    async fn configure_wnd1_user_max_sessions(
        db: &djinn_db::Database,
        user_id: &str,
        model_id: &str,
        cap: u32,
    ) -> djinn_core::models::UserSettings {
        assert!(
            (1..=5).contains(&cap),
            "wnd1 fixture caps intentionally cover the 1..=5 stress range"
        );
        djinn_db::UserSettingsRepository::new(db.clone())
            .upsert_max_sessions(user_id, &HashMap::from([(model_id.to_owned(), cap)]))
            .await
            .expect("configure wnd1 user max_sessions cap")
    }

    async fn materialize_wnd1_running_session(
        db: &djinn_db::Database,
        fixture: &Wnd1DispatchFixture,
        task_id: &str,
    ) -> String {
        djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .create(djinn_db::CreateSessionParams {
                project_id: &fixture.project_id,
                task_id: Some(task_id),
                model: &fixture.model_id,
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("materialize delayed wnd1 running session row")
            .id
    }

    async fn complete_wnd1_session(db: &djinn_db::Database, session_id: &str) {
        djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .update(
                session_id,
                djinn_core::models::SessionStatus::Completed,
                0,
                0,
                0,
                0,
                None,
            )
            .await
            .expect("complete wnd1 running session row");
    }

    async fn wnd1_active_count(
        session_repo: &djinn_db::SessionRepository,
        creator: &str,
        model: &str,
    ) -> i64 {
        session_repo
            .count_active_by_user_and_model()
            .await
            .expect("count active sessions for wnd1 cap")
            .into_iter()
            .filter(|(c, m, _)| c.as_deref() == Some(creator) && m == model)
            .map(|(_, _, count)| count)
            .sum()
    }

    async fn wait_for_pool_to_forget_task(pool: &djinn_slot::SlotPoolHandle, task_id: &str) {
        let deadline = tokio::time::Instant::now() + WND1_DISPATCH_SETTLE_TIMEOUT;
        loop {
            if !pool
                .has_session(task_id)
                .await
                .expect("query wnd1 pool task mapping")
            {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for wnd1 pool to settle task {task_id}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn assert_wnd1_observed_cap(
        cap: u32,
        observations: &mut Vec<DispatchCapObservation>,
        phase: &str,
    ) {
        observations.extend(take_dispatch_cap_observations());
        let max_observed = observations
            .iter()
            .map(|obs| obs.effective_count)
            .max()
            .unwrap_or(0);
        assert!(
            max_observed <= cap,
            "wnd1 cap {cap} exceeded during {phase}: max_observed={max_observed}, observations={observations:?}"
        );
    }

    async fn wnd1_running_count_for_fixture(
        db: &djinn_db::Database,
        fixture: &Wnd1DispatchFixture,
    ) -> u32 {
        djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .count_active_by_user_and_model()
            .await
            .expect("count wnd1 active sessions by creator/model")
            .into_iter()
            .find_map(|(creator, model, count)| {
                (creator.as_deref() == Some(fixture.created_by_user_id.as_str())
                    && model == fixture.model_id)
                    .then(|| u32::try_from(count).expect("wnd1 running count fits u32"))
            })
            .unwrap_or(0)
    }

    async fn assert_wnd1_post_settlement_convergence(
        cap: u32,
        db: &djinn_db::Database,
        actor: &mut CoordinatorActor,
        fixture: &Wnd1DispatchFixture,
    ) {
        actor.reconcile_inflight_dispatch_ledger().await;

        assert!(
            actor.inflight_dispatches.is_empty(),
            "cap {cap}: in-flight dispatch ledger retained stale entries after all controlled tasks settled: {:?}",
            actor.inflight_dispatches
        );

        let dispatch_rows = djinn_db::DispatchStateRepository::new(db.clone())
            .list_all()
            .await
            .expect("list wnd1 durable dispatch state rows");
        let stale_durable_inflight: Vec<_> = dispatch_rows
            .iter()
            .filter(|row| {
                fixture.task_ids.contains(&row.task_id)
                    && (row.inflight_creator_user_id.is_some() || row.inflight_model_id.is_some())
            })
            .collect();
        assert!(
            stale_durable_inflight.is_empty(),
            "cap {cap}: durable dispatch_state retained stale in-flight entries after settlement: {stale_durable_inflight:?}"
        );

        let persisted_running = wnd1_running_count_for_fixture(db, fixture).await;
        let mut effective_counts = HashMap::from([(
            (fixture.created_by_user_id.clone(), fixture.model_id.clone()),
            persisted_running,
        )]);
        overlay_inflight_ledger(&mut effective_counts, &actor.inflight_dispatches);
        let effective_after_overlay = effective_counts
            .get(&(fixture.created_by_user_id.clone(), fixture.model_id.clone()))
            .copied()
            .unwrap_or(0);

        assert_eq!(
            persisted_running, 0,
            "cap {cap}: completed wnd1 sessions left phantom running rows that would continue consuming the per-user cap"
        );
        assert_eq!(
            effective_after_overlay, persisted_running,
            "cap {cap}: coordinator effective cap accounting drifted from persisted session state after settlement"
        );
    }

    // Normal Rust/nextest-discoverable test: it uses only TestRuntime/template
    // Postgres (no kind/k8s) and covers the historical v0.4.15 per-user cap
    // overshoot when session rows lagged behind newly-dispatched worker tasks.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn wnd1_dispatch_race_harness_never_exceeds_caps_1_through_5() {
        for cap in 1..=5 {
            clear_dispatch_cap_observations();
            let db = crate::test_helpers::create_test_db();
            let (events_tx, _events_rx) = tokio::sync::broadcast::channel(256);
            let fixture = seed_wnd1_ready_worker_tasks(&db, WND1_READY_TASK_COUNT + 2).await;
            assert!(
                std::path::Path::new(&fixture.project_path).is_dir(),
                "wnd1 fixture project path must exist for in-process dispatch"
            );
            configure_wnd1_user_max_sessions(
                &db,
                &fixture.created_by_user_id,
                &fixture.model_id,
                cap,
            )
            .await;

            let (runtime, mut started_rx) = Wnd1ControlledRuntime::new();
            let actor = wnd1_actor_for_tests(
                &db,
                &events_tx,
                &runtime,
                u32::try_from(WND1_READY_TASK_COUNT).expect("wnd1 slot count fits u32"),
            );
            let pool = actor.pool.clone();
            let actor = StdArc::new(tokio::sync::Mutex::new(actor));
            let observations = StdArc::new(StdMutex::new(Vec::<DispatchCapObservation>::new()));
            let dispatch_done = StdArc::new(std::sync::atomic::AtomicBool::new(false));
            let ledger_overlay_observed = StdArc::new(std::sync::atomic::AtomicBool::new(false));

            let settler_db = db.clone();
            let settler_fixture = Wnd1DispatchFixture {
                project_id: fixture.project_id.clone(),
                project_path: fixture.project_path.clone(),
                created_by_user_id: fixture.created_by_user_id.clone(),
                model_id: fixture.model_id.clone(),
                task_ids: fixture.task_ids.clone(),
            };
            let settler_runtime = runtime.clone();
            let settler_pool = pool.clone();
            let settler_done = dispatch_done.clone();
            let settler_ledger_overlay_observed = ledger_overlay_observed.clone();
            let settler = tokio::spawn(async move {
                let mut active: Vec<(String, String)> = Vec::new();
                let mut observed_starts = 0usize;
                loop {
                    tokio::select! {
                        maybe_task_id = started_rx.recv() => {
                            if let Some(task_id) = maybe_task_id {
                                // Simulate the pod/session-row lag window: the local
                                // in-flight ledger is already populated by dispatch,
                                // while the DB row becomes visible only after a
                                // subsequent dispatch pass has had a deterministic
                                // chance to overlay that ledger entry. This keeps the
                                // stress proof from depending on scheduler timing on
                                // fast CI runners where a 1ms synthetic lag can elapse
                                // before the next dispatch loop reacquires the actor.
                                let deadline = tokio::time::Instant::now()
                                    + WND1_DISPATCH_SETTLE_TIMEOUT;
                                while !settler_ledger_overlay_observed
                                    .load(std::sync::atomic::Ordering::SeqCst)
                                    && !settler_done.load(std::sync::atomic::Ordering::SeqCst)
                                {
                                    assert!(
                                        tokio::time::Instant::now() < deadline,
                                        "cap {cap}: timed out waiting for dispatch to observe the lag-window ledger overlay before materializing the session row"
                                    );
                                    tokio::task::yield_now().await;
                                }
                                let session_id = materialize_wnd1_running_session(
                                    &settler_db,
                                    &settler_fixture,
                                    &task_id,
                                ).await;
                                observed_starts += 1;
                                active.push((task_id, session_id));
                            }
                        }
                        _ = tokio::time::sleep(Duration::from_millis(2)) => {}
                    }

                    if active.len() >= cap as usize
                        || (settler_done.load(std::sync::atomic::Ordering::SeqCst)
                            && !active.is_empty())
                    {
                        let (task_id, session_id) = active.remove(0);
                        complete_wnd1_session(&settler_db, &session_id).await;
                        settler_runtime.release(&task_id).await;
                        wait_for_pool_to_forget_task(&settler_pool, &task_id).await;
                    }

                    if settler_done.load(std::sync::atomic::Ordering::SeqCst)
                        && active.is_empty()
                        && observed_starts >= WND1_READY_TASK_COUNT
                    {
                        break;
                    }
                }
            });

            let mut dispatchers = Vec::new();
            for _ in 0..4 {
                let actor = actor.clone();
                let observations = observations.clone();
                let ledger_overlay_observed = ledger_overlay_observed.clone();
                let project_id = fixture.project_id.clone();
                let creator_user_id = fixture.created_by_user_id.clone();
                let model_id = fixture.model_id.clone();
                dispatchers.push(tokio::spawn(async move {
                    let deadline = tokio::time::Instant::now() + WND1_DISPATCH_SETTLE_TIMEOUT;
                    loop {
                        let dispatched = {
                            let mut actor = actor.lock().await;
                            clear_dispatch_cap_observations();
                            actor.dispatch_ready_tasks(Some(&project_id)).await;
                            let dispatched = actor.dispatched;
                            let mut observations = observations
                                .lock()
                                .expect("wnd1 observations mutex poisoned");
                            assert_wnd1_observed_cap(
                                cap,
                                &mut observations,
                                "concurrent repeated dispatch passes",
                            );
                            if observations.iter().any(|obs| {
                                obs.stage == DispatchCapObservationStage::LedgerOverlay
                                    && obs.creator_user_id == creator_user_id
                                    && obs.model == model_id
                            }) {
                                ledger_overlay_observed
                                    .store(true, std::sync::atomic::Ordering::SeqCst);
                            }
                            dispatched
                        };
                        if dispatched >= WND1_READY_TASK_COUNT as u64 {
                            break;
                        }
                        assert!(
                            tokio::time::Instant::now() < deadline,
                            "cap {cap}: concurrent dispatch stress did not make bounded progress"
                        );
                        tokio::task::yield_now().await;
                    }
                }));
            }
            for dispatcher in dispatchers {
                dispatcher
                    .await
                    .expect("wnd1 dispatcher task should not panic");
            }
            dispatch_done.store(true, std::sync::atomic::Ordering::SeqCst);
            settler.await.expect("wnd1 settler task should not panic");

            let task_repo =
                djinn_db::TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
            for task_id in &fixture.task_ids {
                task_repo
                    .set_status(task_id, "closed")
                    .await
                    .expect("close wnd1 fixture task for quiescence");
                let _ = pool.kill_session(task_id).await;
                djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop())
                    .interrupt_running_for_task(task_id)
                    .await
                    .expect("settle wnd1 fixture task for quiescence");
            }
            {
                let mut actor = actor.lock().await;
                clear_dispatch_cap_observations();
                actor.dispatch_ready_tasks(Some(&fixture.project_id)).await;
                let mut observations = observations
                    .lock()
                    .expect("wnd1 observations mutex poisoned");
                assert_wnd1_observed_cap(cap, &mut observations, "quiescence reconciliation");
            }

            {
                let mut actor = actor.lock().await;
                assert_wnd1_post_settlement_convergence(cap, &db, &mut actor, &fixture).await;
            }
            let observations = observations
                .lock()
                .expect("wnd1 observations mutex poisoned")
                .clone();
            assert!(
                observations.iter().any(|obs| {
                    obs.stage == DispatchCapObservationStage::InflightIncremented
                        && obs.creator_user_id == fixture.created_by_user_id
                        && obs.model == fixture.model_id
                }),
                "cap {cap}: stress run must exercise real dispatch admissions, not only the pure ledger helper"
            );
            assert!(
                observations.iter().any(|obs| {
                    obs.stage == DispatchCapObservationStage::LedgerOverlay
                        && obs.creator_user_id == fixture.created_by_user_id
                        && obs.model == fixture.model_id
                }),
                "cap {cap}: stress run must observe the lag-window ledger overlay"
            );
            for obs in observations.iter().filter(|obs| {
                obs.creator_user_id == fixture.created_by_user_id && obs.model == fixture.model_id
            }) {
                assert!(
                    obs.effective_count <= cap,
                    "cap {cap}: observed instantaneous {:?} count {} above cap",
                    obs.stage,
                    obs.effective_count
                );
            }
            let max_instantaneous_count = observations
                .iter()
                .filter(|obs| {
                    obs.creator_user_id == fixture.created_by_user_id
                        && obs.model == fixture.model_id
                })
                .map(|obs| obs.effective_count)
                .max()
                .unwrap_or(0);
            assert_eq!(
                max_instantaneous_count, cap,
                "cap {cap}: stress run should make the per-user cap, not the test pool, the limiting factor"
            );

            let actor = actor.lock().await;
            assert_eq!(
                actor.inflight_dispatches.len(),
                0,
                "cap {cap}: in-flight dispatch ledger must drain after quiescence"
            );
            let session_repo =
                djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop());
            assert_eq!(
                wnd1_active_count(
                    &session_repo,
                    &fixture.created_by_user_id,
                    &fixture.model_id,
                )
                .await,
                0,
                "cap {cap}: DB active-session count must converge to zero after quiescence"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wnd1_ready_queue_fixture_is_visible_to_dispatch_selection_and_reads_caps() {
        let db = crate::test_helpers::create_test_db();
        let fixture = seed_wnd1_ready_worker_tasks(&db, WND1_READY_TASK_COUNT).await;
        assert_eq!(fixture.task_ids.len(), WND1_READY_TASK_COUNT);

        let ready = djinn_db::TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop())
            .list_ready(djinn_db::ReadyQuery {
                project_id: Some(fixture.project_id.clone()),
                limit: 25,
                ..Default::default()
            })
            .await
            .expect("dispatch ready selection query should see fixture tasks");
        let fixture_ready: Vec<_> = ready
            .iter()
            .filter(|task| fixture.task_ids.contains(&task.id))
            .collect();

        assert_eq!(
            fixture_ready.len(),
            WND1_READY_TASK_COUNT,
            "all wnd1 fixture tasks must be visible to the same list_ready path dispatch uses"
        );
        assert!(fixture_ready.iter().all(|task| {
            task.status == "open"
                && task.issue_type == "task"
                && task.created_by_user_id.as_deref() == Some(fixture.created_by_user_id.as_str())
                && crate::roles::RoleRegistry::new()
                    .role_for_task(task, &crate::roles::DispatchContext)
                    == Some("worker")
        }));

        for cap in 1..=5 {
            configure_wnd1_user_max_sessions(
                &db,
                &fixture.created_by_user_id,
                &fixture.model_id,
                cap,
            )
            .await;
            let settings = djinn_db::UserSettingsRepository::new(db.clone())
                .get(&fixture.created_by_user_id)
                .await
                .expect("read wnd1 user settings")
                .expect("wnd1 user settings row exists");
            assert_eq!(
                settings
                    .max_sessions
                    .as_ref()
                    .and_then(|caps| caps.get(&fixture.model_id))
                    .copied(),
                Some(cap),
                "configured cap {cap} should round-trip for the stable wnd1 model"
            );
        }

        let running_counts =
            djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop())
                .count_active_by_user_and_model()
                .await
                .expect("dispatch cap count query should work against fixture database");
        assert!(
            running_counts.is_empty(),
            "ready-task fixture should not require pre-existing running sessions"
        );
    }

    #[test]
    fn dispatch_wall_clock_timestamps_are_millisecond_precision() {
        let ts = ::time::OffsetDateTime::parse(
            "2026-06-12T14:48:37.048295203Z",
            &::time::format_description::well_known::Rfc3339,
        )
        .unwrap();

        assert_eq!(
            format_dispatch_wall_clock(ts).as_deref(),
            Some("2026-06-12T14:48:37.048Z"),
            "persisted dispatch-state timestamps must match Postgres millisecond precision"
        );
    }

    /// The overshoot fix: a dispatch whose `running` session row hasn't landed
    /// yet still counts against the per-user cap. The DB seed shows 0 running,
    /// but four in-flight dispatches must raise the count to 4 so the next pass
    /// defers instead of dispatching four more.
    #[test]
    fn ledger_overlay_counts_inflight_dispatches_when_db_seed_is_cold() {
        clear_dispatch_cap_observations();
        let mut running: HashMap<(String, String), u32> = HashMap::new(); // cold DB seed
        let mut inflight: HashMap<String, (Option<String>, String)> = HashMap::new();
        for i in 0..4 {
            inflight.insert(
                format!("task-{i}"),
                (Some("user-a".into()), "openai/gpt-5.5".into()),
            );
        }

        overlay_inflight_ledger(&mut running, &inflight);

        assert_eq!(
            running.get(&key("user-a", "openai/gpt-5.5")).copied(),
            Some(4),
            "four in-flight dispatches must count against the cap even with a cold DB seed"
        );
        assert_eq!(
            take_dispatch_cap_observations(),
            vec![DispatchCapObservation {
                creator_user_id: "user-a".to_owned(),
                model: "openai/gpt-5.5".to_owned(),
                effective_count: 4,
                stage: DispatchCapObservationStage::LedgerOverlay,
            }],
            "observer must see the cold-DB in-flight count used by dispatch"
        );
    }

    /// `max`, not sum: a task counted in BOTH the running rows and the ledger
    /// (its session row landed but the ledger entry hasn't been reconciled away
    /// yet) must count once. Also: a larger DB count wins over a smaller ledger.
    #[test]
    fn ledger_overlay_takes_max_never_double_counts() {
        clear_dispatch_cap_observations();
        let mut running: HashMap<(String, String), u32> = HashMap::new();
        running.insert(key("user-a", "m"), 3); // 3 already running in DB
        running.insert(key("user-b", "m"), 5); // 5 running, ledger will be lower
        let mut inflight: HashMap<String, (Option<String>, String)> = HashMap::new();
        // user-a: 3 in-flight that overlap the 3 running rows → must stay 3, not 6.
        for i in 0..3 {
            inflight.insert(format!("a{i}"), (Some("user-a".into()), "m".into()));
        }
        // user-b: 2 in-flight, fewer than the 5 running → DB count wins.
        for i in 0..2 {
            inflight.insert(format!("b{i}"), (Some("user-b".into()), "m".into()));
        }

        overlay_inflight_ledger(&mut running, &inflight);

        assert_eq!(running.get(&key("user-a", "m")).copied(), Some(3));
        assert_eq!(running.get(&key("user-b", "m")).copied(), Some(5));
        assert_eq!(
            take_dispatch_cap_observations(),
            vec![
                DispatchCapObservation {
                    creator_user_id: "user-a".to_owned(),
                    model: "m".to_owned(),
                    effective_count: 3,
                    stage: DispatchCapObservationStage::LedgerOverlay,
                },
                DispatchCapObservation {
                    creator_user_id: "user-b".to_owned(),
                    model: "m".to_owned(),
                    effective_count: 5,
                    stage: DispatchCapObservationStage::LedgerOverlay,
                },
            ],
            "observer must see max(db, ledger), never db + ledger"
        );
    }

    #[test]
    fn dispatch_cap_observer_records_new_inflight_increments() {
        clear_dispatch_cap_observations();

        observe_dispatch_cap_count(
            DispatchCapObservationStage::InflightIncremented,
            "user-a",
            "m",
            2,
        );

        assert_eq!(
            take_dispatch_cap_observations(),
            vec![DispatchCapObservation {
                creator_user_id: "user-a".to_owned(),
                model: "m".to_owned(),
                effective_count: 2,
                stage: DispatchCapObservationStage::InflightIncremented,
            }],
            "observer must capture the count after dispatch increments local in-flight state"
        );
    }

    #[test]
    fn cap_utilization_metric_uses_db_seed_overlaid_with_inflight_ledger() {
        djinn_telemetry::init().unwrap();

        let user = "cap-path-user";
        let model = "cap-path-model";
        let mut running = HashMap::from([((user.to_owned(), model.to_owned()), 1)]);
        let mut inflight: HashMap<String, (Option<String>, String)> = HashMap::new();
        inflight.insert("task-a".into(), (Some(user.into()), model.into()));
        inflight.insert("task-b".into(), (Some(user.into()), model.into()));
        overlay_inflight_ledger(&mut running, &inflight);

        assert!(model_under_user_cap(&running, user, model, 4));

        let rendered = djinn_telemetry::render().unwrap();
        let sample = rendered_metric_sample(
            &rendered,
            "djinn_user_cap_utilization",
            &[("user", user), ("model", model)],
        );
        assert!(
            sample.ends_with(" 0.5"),
            "cap utilization must be overlaid used/cap = 2/4 in:\n{rendered}"
        );
    }

    #[test]
    fn dispatch_outcome_metrics_record_exact_once_and_ok_updates_timestamp() {
        djinn_telemetry::init().unwrap();

        let before_rendered = djinn_telemetry::render().unwrap();
        let before_timestamp = metric_value(
            &before_rendered,
            "djinn_dispatch_last_success_timestamp",
            &[],
        )
        .unwrap_or(0.0);

        for outcome in [
            djinn_telemetry::dispatch::OUTCOME_OK,
            djinn_telemetry::dispatch::OUTCOME_COOLDOWN,
            djinn_telemetry::dispatch::OUTCOME_CAP,
            djinn_telemetry::dispatch::OUTCOME_BREAKER,
            djinn_telemetry::dispatch::OUTCOME_ERROR,
        ] {
            let before = metric_value(
                &djinn_telemetry::render().unwrap(),
                "djinn_dispatch_attempts_total",
                &[("outcome", outcome)],
            )
            .unwrap_or(0.0);
            record_dispatch_outcome(outcome);
            let after_rendered = djinn_telemetry::render().unwrap();
            let after = metric_value(
                &after_rendered,
                "djinn_dispatch_attempts_total",
                &[("outcome", outcome)],
            )
            .unwrap_or(0.0);
            assert_eq!(
                after - before,
                1.0,
                "outcome={outcome} must increment exactly once in:\n{after_rendered}"
            );
        }

        let rendered = djinn_telemetry::render().unwrap();
        let after_timestamp = metric_value(&rendered, "djinn_dispatch_last_success_timestamp", &[])
            .expect("ok outcome must set dispatch last-success timestamp");
        assert!(
            after_timestamp >= before_timestamp && after_timestamp > 0.0,
            "ok outcome must update last-success timestamp from {before_timestamp} in:\n{rendered}"
        );
    }

    fn rendered_metric_sample<'a>(
        rendered: &'a str,
        metric: &str,
        labels: &[(&str, &str)],
    ) -> &'a str {
        rendered
            .lines()
            .find(|line| {
                line.starts_with(metric)
                    && labels
                        .iter()
                        .all(|(key, value)| line.contains(&format!("{key}=\"{value}\"")))
            })
            .unwrap_or_else(|| panic!("missing metric {metric}{labels:?} in:\n{rendered}"))
    }

    fn metric_value(rendered: &str, metric: &str, labels: &[(&str, &str)]) -> Option<f64> {
        rendered
            .lines()
            .find(|line| {
                line.starts_with(metric)
                    && labels
                        .iter()
                        .all(|(key, value)| line.contains(&format!("{key}=\"{value}\"")))
            })
            .and_then(|line| line.split_whitespace().last())
            .and_then(|value| value.parse::<f64>().ok())
    }

    /// Creator-less (legacy system) dispatches are ungated by the per-user cap,
    /// so they must not contribute to any count.
    #[test]
    fn ledger_overlay_ignores_creatorless_entries() {
        clear_dispatch_cap_observations();
        let mut running: HashMap<(String, String), u32> = HashMap::new();
        let mut inflight: HashMap<String, (Option<String>, String)> = HashMap::new();
        inflight.insert("sys".into(), (None, "m".into()));
        overlay_inflight_ledger(&mut running, &inflight);
        assert!(running.is_empty());
        assert!(take_dispatch_cap_observations().is_empty());
    }

    /// With cap N for a model and N running mixed-role sessions for the same
    /// (creator, model), a further worker OR reviewer dispatch for that
    /// (creator, model) is denied — verifying the per-(user, model) cap counts
    /// ALL non-chat roles, not just workers.
    ///
    /// This is the acceptance-criteria test for the "2 worker + 1 reviewer = 3
    /// > cap 2" overshoot: it seeds exactly cap sessions across mixed roles
    /// (e.g. 1 worker + 1 reviewer for cap 2) and asserts neither a worker nor
    /// a reviewer task can be admitted.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn mixed_role_cap_denies_n_plus_1_dispatch_worker_and_reviewer_variants() {
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel(256);

        // ── fixture: project, user, model selection, cap=2 ────────────────
        let project = crate::test_helpers::create_test_project(&db).await;
        let project_path =
            djinn_core::paths::project_dir(&project.github_owner, &project.github_repo);
        std::fs::create_dir_all(&project_path).expect("create mixed-role fixture project dir");
        let user = djinn_db::UserRepository::new(db.clone())
            .upsert_from_github(
                985_200,
                "mixed-role-cap-user",
                Some("mixed role cap user"),
                None,
            )
            .await
            .expect("create mixed-role fixture user");
        let user_id = user.id.clone();

        let settings = djinn_db::UserSettingsRepository::new(db.clone());
        settings
            .upsert_lanes(
                &user_id,
                &djinn_core::models::ModelLanes::from_flat(vec![WND1_STABLE_MODEL_ID.to_owned()]),
            )
            .await
            .expect("configure mixed-role fixture selected model");
        // cap = 2
        configure_wnd1_user_max_sessions(&db, &user_id, WND1_STABLE_MODEL_ID, 2).await;

        // ── seed cap (2) running sessions with MIXED roles ────────────────
        // One worker + one reviewer, both under the same creator and model.
        // The DB seed count will show 2 running for (creator, model).
        let task_repo =
            djinn_db::TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let session_repo =
            djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        // Create two tasks (owned by the user) to attach sessions to.
        let seeded_task_ids = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                let mut ids = Vec::new();
                for i in 0..2 {
                    let t = task_repo
                        .create_in_project(
                            &project.id,
                            None,
                            &format!("mixed-role seeded task {i}"),
                            "",
                            "",
                            "task",
                            i as i64,
                            "worker",
                            Some("closed"),
                            Some("[]"),
                        )
                        .await
                        .expect("seed mixed-role task");
                    ids.push(t.id);
                }
                ids
            })
            .await;

        // Create running sessions for these tasks with mixed agent types.
        // The first is a "worker" session; the second is a "reviewer" session.
        for (i, task_id) in seeded_task_ids.iter().enumerate() {
            let agent_type = if i == 0 { "worker" } else { "reviewer" };
            session_repo
                .create(djinn_db::CreateSessionParams {
                    project_id: &project.id,
                    task_id: Some(task_id),
                    model: WND1_STABLE_MODEL_ID,
                    agent_type,
                    metadata_json: None,
                    task_run_id: None,
                    pricing: None,
                    cost_basis: None,
                })
                .await
                .expect("create mixed-role running session");
        }

        // Verify the DB seed counts both roles.
        let active_count = wnd1_active_count(
            &djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop()),
            &user_id,
            WND1_STABLE_MODEL_ID,
        )
        .await;
        assert_eq!(
            active_count, 2,
            "DB seed must count both worker and reviewer sessions for the cap"
        );

        // ── create ready tasks: one worker (open) + one reviewer (needs_task_review)
        let ready_task_ids = djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(user_id.clone()), async {
                let mut ids = Vec::new();
                // Worker-variant: an open task → dispatches as "worker".
                let worker_task = task_repo
                    .create_in_project(
                        &project.id,
                        None,
                        "mixed-role ready worker task",
                        "",
                        "",
                        "task",
                        10,
                        "worker",
                        Some("open"),
                        Some("[]"),
                    )
                    .await
                    .expect("create mixed-role ready worker task");
                ids.push(("worker", worker_task.id));

                // Reviewer-variant: a needs_task_review task → dispatches as "reviewer".
                let reviewer_task = task_repo
                    .create_in_project(
                        &project.id,
                        None,
                        "mixed-role ready reviewer task",
                        "",
                        "",
                        "task",
                        5,
                        "worker",
                        Some("needs_task_review"),
                        Some("[]"),
                    )
                    .await
                    .expect("create mixed-role ready reviewer task");
                ids.push(("reviewer", reviewer_task.id));
                ids
            })
            .await;

        // ── dispatch pass ─────────────────────────────────────────────────
        let (runtime, _started_rx) = Wnd1ControlledRuntime::new();
        let mut actor = wnd1_actor_for_tests(
            &db,
            &events_tx,
            &runtime,
            u32::try_from(WND1_READY_TASK_COUNT).unwrap(),
        );
        let dispatched_before = actor.dispatched;
        actor.dispatch_ready_tasks(Some(&project.id)).await;

        // ── assertions ────────────────────────────────────────────────────
        // Neither the worker nor the reviewer variant should have dispatched:
        // both creators are at cap 2 (1 worker + 1 reviewer already running).
        assert_eq!(
            actor.dispatched, dispatched_before,
            "no dispatch should succeed when cap=2 is already saturated by mixed-role running sessions"
        );

        // The inflight ledger should not contain any new entries for the ready tasks.
        for (_variant, task_id) in &ready_task_ids {
            assert!(
                !actor.inflight_dispatches.contains_key(task_id),
                "ready task {task_id} must NOT be in the inflight ledger — it was denied by the cap"
            );
        }

        // Running count must still be 2 (no new sessions).
        let post_count = wnd1_active_count(
            &djinn_db::SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop()),
            &user_id,
            WND1_STABLE_MODEL_ID,
        )
        .await;
        assert_eq!(
            post_count, 2,
            "running session count must remain at cap=2 after a denied dispatch pass"
        );
    }
}
