// djinn:allow-oversize — legacy dispatch module over size-guard threshold; split when touched substantively.
use super::super::*;
use super::DispatchOutcome;
#[cfg(test)]
use super::admission::{DispatchCapObservation, DispatchCapObservationStage};
#[cfg(test)]
use super::admission::{
    clear_dispatch_cap_observations, observe_dispatch_cap_count, take_dispatch_cap_observations,
};
use super::admission::{
    lane_under_user_cap, model_under_user_cap, overlay_inflight_lane_ledger,
    overlay_inflight_ledger,
};
use super::post_intervention_lane;
use crate::dispatch_pause::{load_dispatch_pause_state, matching_task_dispatch_pause};
use crate::roles::DispatchContext;
use crate::build_admission::BuildAdmissionDecision;
use djinn_db::AdmissionDomain;
use djinn_k8s::{WarmAdmission, WarmAdmissionPermit, WarmAdmissionTransition};
use djinn_core::clock::{Clock, SystemClock};
use djinn_db::repositories::task_arbitration::TaskArbitrationRepository;
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

/// Map a free-form failover / rotation reason string from the activity log
/// onto the typed [`crate::ModelRotationReason`] enum. The mapping accepts
/// both the snake_case serde form (`no_durable_progress_streak`) and the
/// Debug form (`NoProgress` / `NoDurableProgressStreak`) emitted by
/// `emit_rotation_event` in the agent. Unknown values return `None` so the
/// resume metadata degrades to "rotation considered but no reason recorded"
/// instead of persisting an invalid enum variant.
fn map_model_rotation_reason(raw: &str) -> Option<crate::ModelRotationReason> {
    use crate::ModelRotationReason as R;
    let normalized = raw.trim().to_ascii_lowercase();
    let normalized = normalized.split_whitespace().next().unwrap_or("");
    // Strip surrounding quotes the Debug formatter may leave on strings.
    let normalized = normalized.trim_matches('"');
    match normalized {
        "no_durable_progress_streak" | "noprogress" => Some(R::NoDurableProgressStreak),
        "repeated_read_only_no_op" | "repeatedverifyloop" => Some(R::RepeatedReadOnlyNoOp),
        "repeated_flaky_verification" | "flaky" => Some(R::RepeatedFlakyVerification),
        "context_budget_pressure" | "deadline" => Some(R::ContextBudgetPressure),
        "provider_health_degraded" => Some(R::ProviderHealthDegraded),
        "operator_requested" => Some(R::OperatorRequested),
        "not_eligible" => Some(R::NotEligible),
        _ => None,
    }
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

/// Decide whether an in-flight reservation still represents booting work.
///
/// `active_task_ids` is deliberately an earlier snapshot than the DB count
/// queries. `live_task_ids = None` means the pool could not be queried, so only
/// a session already present in that earlier snapshot may clear a reservation.
fn retain_inflight_reservation(
    task_id: &str,
    active_task_ids: &HashSet<String>,
    live_task_ids: Option<&HashSet<String>>,
) -> bool {
    !active_task_ids.contains(task_id) && live_task_ids.is_none_or(|live| live.contains(task_id))
}

impl CoordinatorActor {
    async fn reconcile_inflight_dispatch_ledger(&mut self, active_task_ids: &HashSet<String>) {
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
                    .filter(|task_id| {
                        !retain_inflight_reservation(task_id, active_task_ids, Some(&live))
                    })
                    .cloned()
                    .collect();
                self.inflight_dispatches.retain(|task_id, _| {
                    retain_inflight_reservation(task_id, active_task_ids, Some(&live))
                });
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
            // On a pool query error, only clear entries now represented by a
            // running DB session. Keep every other ledger entry: a stale but
            // present reservation is conservative, whereas dropping it would
            // reopen the overshoot window.
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: pool get_status failed during cap seed; keeping in-flight ledger as-is");
                let started_task_ids: Vec<String> = self
                    .inflight_dispatches
                    .keys()
                    .filter(|task_id| !retain_inflight_reservation(task_id, active_task_ids, None))
                    .cloned()
                    .collect();
                self.inflight_dispatches.retain(|task_id, _| {
                    retain_inflight_reservation(task_id, active_task_ids, None)
                });
                for task_id in started_task_ids {
                    self.persist_durable_dispatch_state_update(
                        &task_id,
                        None,
                        "inflight_ledger_session_started_clear",
                        DurableDispatchStateUpdate {
                            inflight: Some(None),
                            ..Default::default()
                        },
                    )
                    .await;
                }
            }
        }
    }

    /// Load effective running counts for both admission dimensions.
    ///
    /// DB rows represent sessions that have started; the reconciled in-flight
    /// ledger represents work that was still booting at the earlier active-id
    /// snapshot. Overlays are additive. A row that lands between snapshots may
    /// be counted twice for one pass, which is deliberately conservative.
    pub(crate) async fn effective_running_counts(
        &mut self,
    ) -> (
        HashMap<(String, String), u32>,
        HashMap<(String, djinn_core::models::ModelLane), u32>,
    ) {
        let repo = SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        // Snapshot active task ids FIRST. If a booting session becomes visible
        // between this read and the count reads below, its ledger reservation
        // is conservatively retained and added to the DB count for one pass.
        // Reading these concurrently used to permit the inverse ordering:
        // counts could miss the row while a later active-id view cleared its
        // reservation, briefly admitting above the configured cap.
        let active_task_ids: HashSet<String> = match repo.list_active().await {
            Ok(sessions) => sessions.into_iter().filter_map(|s| s.task_id).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: active-session lookup failed during admission; retaining all in-flight reservations");
                HashSet::new()
            }
        };
        let (model_rows, lane_rows) = tokio::join!(
            repo.count_active_by_user_and_model(),
            repo.count_active_by_user_and_lane(),
        );

        let mut by_model = match model_rows {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|(creator, model, count)| {
                    creator.map(|c| ((c, model), u32::try_from(count).unwrap_or(0)))
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: per-user model concurrency counts failed; using in-flight reservations only");
                HashMap::new()
            }
        };
        let mut by_lane = match lane_rows {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|(creator, lane, count)| {
                    creator.map(|c| ((c, lane), u32::try_from(count).unwrap_or(0)))
                })
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: per-user lane concurrency counts failed; using in-flight reservations only");
                HashMap::new()
            }
        };
        self.reconcile_inflight_dispatch_ledger(&active_task_ids)
            .await;
        overlay_inflight_ledger(&mut by_model, &self.inflight_dispatches);
        overlay_inflight_lane_ledger(&mut by_lane, &self.inflight_dispatches);
        self.overlay_provisional_admissions(&mut by_model, &mut by_lane);
        (by_model, by_lane)
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
        running_by_user_lane: &mut HashMap<(String, djinn_core::models::ModelLane), u32>,
    ) {
        let (fresh_by_model, fresh_by_lane) = self.effective_running_counts().await;
        *running_by_user_model = fresh_by_model;
        *running_by_user_lane = fresh_by_lane;
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
        role: &str,
    ) {
        if let Some(c) = creator {
            self.inflight_dispatches.insert(
                task_id.to_string(),
                InflightDispatch {
                    creator: Some(c.to_string()),
                    model: model.to_string(),
                    lane: djinn_core::models::ModelLane::for_role(role),
                },
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
        // the resume metadata's TYPED fields so they deserialize into the
        // runtime ResumeLifecycleMetadata typed fields (previous_model,
        // new_model, failover_reason, verification_command,
        // last_durable_progress_summary). These are consumed by the worker
        // resume-prompt note (`48ru`) and by the failover-aware fallback
        // worker (`kv6i`). Putting them on the typed fields (not the `extra`
        // map) is what lets the worker deserialize the wire blob: serde does
        // not promote nested map entries into top-level typed fields, so the
        // legacy `extra[...]` insertion was effectively dead code on the
        // worker side.
        let mut metadata = metadata;
        if let Some(model_rotation) = &lifecycle.model_rotation {
            if let Some(prev) = &model_rotation.previous_model {
                metadata.previous_model = Some(prev.clone());
            }
            if let Some(next) = &model_rotation.next_model {
                metadata.new_model = Some(next.clone());
            }
            // Serialize the typed enum reason into a snake_case string so the
            // worker resume note can read it back as plain text.
            if let Some(reason) = &model_rotation.reason {
                let reason_str = serde_json::to_value(reason)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{:?}", reason));
                metadata.failover_reason = Some(reason_str);
            }
        }
        if let Some(auto_submit) = &lifecycle.auto_submit
            && let Some(cmd) = &auto_submit.verification_command
        {
            metadata.verification_command = Some(cmd.clone());
        }
        // last_durable_progress_summary: extract from checkpoint extra if present.
        if let Some(checkpoint) = &lifecycle.checkpoint
            && let Some(summary) = checkpoint.extra.get("last_durable_progress_summary")
            && let Some(summary_str) = summary.as_str()
        {
            metadata.last_durable_progress_summary = Some(summary_str.to_owned());
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
        lifecycle.model_rotation = self.model_rotation_lifecycle_from_activity(task).await;
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
            if let Some(summary) = value
                .get("last_durable_progress_summary")
                .and_then(serde_json::Value::as_str)
            {
                extra.insert(
                    "last_durable_progress_summary".to_string(),
                    serde_json::json!(summary),
                );
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

    /// Populate `model_rotation` lifecycle metadata from the activity log.
    ///
    /// Looks for the most recent activity entry tagged as a model rotation
    /// step (event_type == "model_rotation" or payload contains a
    /// `model_rotation` marker). Such entries are persisted by failover-aware
    /// workers when the supervisor rotates to a fallback candidate mid-session.
    /// The helper extracts `previous_model`, `selected_model`, and
    /// `termination_cause` / `fallback_reason` fields and maps them onto the
    /// typed [`crate::ModelRotationLifecycleMetadata`] consumed by
    /// [`Self::select_resume_lifecycle_metadata_for_dispatch`].
    ///
    /// When no rotation entry exists (no failover happened, or the rotation
    /// event hasn't been bridged to the activity log yet), the helper returns
    /// `None` — the resume path then degrades to a plain resume-source
    /// selection without failover context, which matches the pre-`kv6i`
    /// behaviour. This is the **only** seam through which the coordinator
    /// learns about the prior session's model rotation; the dispatch-time
    /// failover chain records observations on `HealthTracker` but does not
    /// persist a rotation metadata entry. Sibling epic `97f8` already plumbs
    /// the worker-side rotation events; this helper is the read side.
    async fn model_rotation_lifecycle_from_activity(
        &self,
        task: &djinn_core::models::Task,
    ) -> Option<crate::ModelRotationLifecycleMetadata> {
        let entries = match self.task_repo().list_activity(&task.id).await {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!(task_id = %task.short_id, error = %e, "CoordinatorActor: failed to load activity for resume model-rotation selection");
                return None;
            }
        };
        // Walk in reverse chronological order so the most recent rotation wins.
        entries.iter().rev().find_map(|entry| {
            let value: serde_json::Value = serde_json::from_str(&entry.payload).ok()?;
            // Two formats are accepted:
            //   1. event_type == "model_rotation" with structured payload (97f8)
            //   2. inline `model_rotation` block in a comment payload (legacy)
            let is_event = entry.event_type == "model_rotation";
            let has_inline = value.get("model_rotation").is_some();
            if !is_event && !has_inline {
                return None;
            }
            let block = if is_event {
                value.clone()
            } else {
                value.get("model_rotation")?.clone()
            };
            let previous_model = block
                .get("previous_model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            // Try `selected_model` (97f8 `rotated` action) and `next_model`
            // (legacy naming) so both shapes are consumable.
            let next_model = block
                .get("selected_model")
                .or_else(|| block.get("next_model"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            // Reason can be `termination_cause` (Debug formatted) or
            // `fallback_reason`. Map the well-known cases onto the typed
            // [`crate::ModelRotationReason`] enum; unknown reasons stay `None`
            // rather than being persisted as an invalid value.
            let reason_raw = block
                .get("termination_cause")
                .or_else(|| block.get("fallback_reason"))
                .or_else(|| block.get("reason"))
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let reason = reason_raw.as_deref().and_then(map_model_rotation_reason);
            if previous_model.is_none() && next_model.is_none() && reason.is_none() {
                return None;
            }
            Some(crate::ModelRotationLifecycleMetadata {
                considered: true,
                reason,
                previous_model,
                next_model,
                extra: serde_json::Map::new(),
            })
        })
    }

    // ─── Shared per-user admission surface ────────────────────────────────
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

    #[cfg(not(test))]
    fn admission_controller(&self) -> Option<&crate::build_admission::BuildAdmissionController> {
        self.build_admission.as_deref()
    }

    #[cfg(test)]
    fn admission_controller(&self) -> Option<&crate::build_admission::BuildAdmissionController> {
        None
    }

    /// Reserve and durably mark a task-run create before the pool side effect.
    /// A controller denial is deliberately neutral: callers leave the task queued.
    pub(crate) async fn begin_task_run_build_admission(
        &self, role: &str, task_id: &str, generation: i64, object_name: String,
    ) -> Result<Option<WarmAdmissionPermit>, ()> {
        let Some(controller) = self.admission_controller() else { return Ok(None); };
        match controller.admit_task_run(Some(role), AdmissionDomain::TaskObservation, task_id.to_owned(), generation, object_name).await {
            Ok(BuildAdmissionDecision::Permitted { permit, .. }) => {
                controller.transition(&permit, WarmAdmissionTransition::CreateStarted).await.map_err(|error| {
                    tracing::warn!(task_id, role, %error, "build admission CreateStarted failed; deferring pool create");
                })?;
                Ok(Some(permit))
            }
            Ok(BuildAdmissionDecision::Denied { occupancy, cap }) => {
                tracing::info!(task_id, role, occupancy, cap, "build admission denied; leaving task queued");
                Err(())
            }
            Ok(BuildAdmissionDecision::Unclassified) => {
                tracing::warn!(task_id, role, "unclassified build admission; leaving task queued");
                Err(())
            }
            Err(error) => {
                tracing::warn!(task_id, role, %error, "build admission unavailable; deferring pool create");
                Err(())
            }
        }
    }

    /// Translate the strongest result available from the slot-pool seam. The pool
    /// does not return a Kubernetes UID, so even an accepted request remains
    /// CreateUnknown until a UID-bearing runtime callback is wired.
    pub(crate) async fn finish_task_run_build_admission(
        &self, permit: Option<WarmAdmissionPermit>, dispatched: bool,
    ) {
        let (Some(controller), Some(permit)) = (self.admission_controller(), permit) else { return; };
        let transition = if dispatched {
            WarmAdmissionTransition::CreateUnknown { diagnostic: "slot-pool accepted create without object UID".to_owned() }
        } else {
            WarmAdmissionTransition::DefinitiveFailure { diagnostic: "slot-pool rejected before task-run creation".to_owned() }
        };
        if let Err(error) = controller.transition(&permit, transition).await {
            tracing::warn!(%error, "failed to persist task-run build-admission outcome; retaining capacity conservatively");
        }
    }

    /// Check whether a single `(user, model, lane)` dispatch is admissible
    /// under both configured concurrency ceilings.
    ///
    /// This re-reads the DB active-session counts plus the in-flight ledger
    /// overlay on each call (via [`effective_running_counts`]), so it
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
        model_cap: u32,
        role: &str,
        lane_cap: Option<u32>,
    ) -> bool {
        let (running_by_model, running_by_lane) = self.effective_running_counts().await;
        let lane = djinn_core::models::ModelLane::for_role(role);
        model_under_user_cap(&running_by_model, user, model, model_cap)
            && lane_under_user_cap(&running_by_lane, user, lane, lane_cap)
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
    /// running counts. Called from [`effective_running_counts`] so that
    /// `check_user_model_admission` accounts for reservations that have not yet
    /// been re-keyed to a real task id.
    fn overlay_provisional_admissions(
        &self,
        running_by_user_model: &mut HashMap<(String, String), u32>,
        running_by_user_lane: &mut HashMap<(String, djinn_core::models::ModelLane), u32>,
    ) {
        for dispatch in self.provisional_admissions.values() {
            if let Some(creator) = dispatch.creator.as_ref() {
                *running_by_user_model
                    .entry((creator.clone(), dispatch.model.clone()))
                    .or_insert(0) += 1;
                *running_by_user_lane
                    .entry((creator.clone(), dispatch.lane))
                    .or_insert(0) += 1;
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
        role: &str,
    ) {
        self.provisional_admissions.remove(provisional_key);
        self.inflight_dispatches.remove(provisional_key);
        self.record_inflight_dispatch(real_task_id, None, Some(creator), model, role)
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
        let escalation_count = existing.as_ref().map(|r| r.escalation_count).unwrap_or(0);
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
        let total_candidates = model_ids.len();
        let mut skipped_count: usize = 0;
        // Chain-scoped failure observations: only this dispatch attempt's
        // observed failures can later trigger breaker side effects. The list
        // is collected here (not on `HealthTracker`) precisely so a successful
        // fallback never causes breaker checks for an earlier candidate, and
        // so two unrelated failover chains cannot leak breaker observations
        // into each other. Returned to the caller via
        // `DispatchOutcome::Failed { exhausted_observations }`; discarded on
        // every other branch.
        let mut exhausted_observations: Vec<djinn_provider::catalog::HealthKey> = Vec::new();
        // Failover latency: wall-clock from first candidate attempt to
        // terminal event (acceptance or exhaustion).
        let failover_chain_start = SystemClock::new().now_instant();
        // Track the last model_id for the chain-exhausted structured log.
        let mut last_model_id: &str = "";

        for (candidate_index, model_id) in model_ids.iter().enumerate() {
            last_model_id = model_id.as_str();
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
                super::lane_resolution_log::emit_failover_candidate_attempt(
                    label,
                    role,
                    model_id,
                    candidate_index,
                    total_candidates,
                    &super::lane_resolution_log::CandidateAttemptOutcome::BreakerOpen,
                    None,
                );
                let (provider_id, model_name) =
                    super::lane_resolution_log::parse_provider_model(model_id);
                djinn_telemetry::failover::increment_candidate_attempt(
                    djinn_telemetry::failover::OUTCOME_BREAKER_OPEN,
                    provider_id,
                    model_name,
                );
                skipped_count += 1;
                continue;
            }

            match dispatch_fn(&self.pool, model_id).await {
                Ok(()) => {
                    tracing::Span::current().record("outcome", "ok");
                    tracing::info!(outcome = "ok", model_id = %model_id, label);
                    super::lane_resolution_log::emit_failover_candidate_accepted(
                        label,
                        role,
                        model_id,
                        candidate_index,
                        total_candidates,
                        skipped_count,
                        None,
                    );
                    let (provider_id, model_name) =
                        super::lane_resolution_log::parse_provider_model(model_id);
                    djinn_telemetry::failover::increment_candidate_accepted(
                        provider_id,
                        model_name,
                    );
                    djinn_telemetry::failover::record_latency(failover_chain_start.elapsed());
                    // Emit fallback-rescue observability when the accepted
                    // candidate is not the first in the chain (earlier
                    // candidates failed and this one rescued the dispatch).
                    if candidate_index > 0 {
                        djinn_telemetry::fallback_rescue::increment_rescue();
                        // Emit reasoning-model outcome observability for the
                        // rescued path: the first candidate's model is the one
                        // that would have been killed without the fallback.
                        let first_model = &model_ids[0];
                        if djinn_telemetry::reasoning_kill::is_reasoning_model(first_model) {
                            djinn_telemetry::reasoning_kill::increment(
                                djinn_telemetry::reasoning_kill::MODEL_CONTEXT_REASONING,
                                djinn_telemetry::reasoning_kill::FAILURE_CLASS_IDLE_STALL,
                                djinn_telemetry::reasoning_kill::OUTCOME_RESCUED,
                            );
                        }
                    }
                    // Successful fallback: discard chain-scoped observations.
                    // The earlier candidate's failure counts stay recorded in
                    // `HealthTracker` (for diagnostics) but no breaker trip
                    // is evaluated, so the fallback-rescued session does not
                    // demote or cooldown a model whose chain was not
                    // exhausted.
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
                    super::lane_resolution_log::emit_failover_candidate_attempt(
                        label,
                        role,
                        model_id,
                        candidate_index,
                        total_candidates,
                        &super::lane_resolution_log::CandidateAttemptOutcome::AtCapacity,
                        None,
                    );
                    let (provider_id, model_name) =
                        super::lane_resolution_log::parse_provider_model(model_id);
                    djinn_telemetry::failover::increment_candidate_attempt(
                        djinn_telemetry::failover::OUTCOME_AT_CAPACITY,
                        provider_id,
                        model_name,
                    );
                    skipped_count += 1;
                }
                Err(PoolError::ActorDead) => {
                    tracing::Span::current().record("outcome", "error");
                    tracing::debug!(outcome = "error", model_id = %model_id, label);
                    tracing::error!("CoordinatorActor: slot pool actor dead, aborting dispatch");
                    return DispatchOutcome::PoolDead;
                }
                Err(e) => {
                    // Failover-chain traversal: record the per-candidate
                    // health *observation* immediately (failure counts are
                    // incremented for diagnostics and candidate health state),
                    // but do NOT trip the circuit breaker yet — breaker
                    // demotion/cooldown is deferred until the chain is
                    // exhausted (AC2).  Track the key on the *chain-local*
                    // list so a successful fallback never evaluates a breaker
                    // check for an earlier candidate, and so two unrelated
                    // failover chains cannot leak observations into each
                    // other.  Log the failure and continue to the next
                    // eligible candidate.
                    self.health.record_failure_observation(scope, model_id);
                    exhausted_observations
                        .push(djinn_provider::catalog::HealthKey::new(scope, model_id));
                    tracing::Span::current().record("outcome", "error");
                    tracing::debug!(outcome = "error", model_id = %model_id, label);
                    tracing::warn!(
                        model_id = %model_id,
                        label,
                        candidate_index,
                        total_candidates,
                        error = %e,
                        "CoordinatorActor: candidate dispatch failed — advancing to next failover candidate"
                    );
                    super::lane_resolution_log::emit_failover_candidate_attempt(
                        label,
                        role,
                        model_id,
                        candidate_index,
                        total_candidates,
                        &super::lane_resolution_log::CandidateAttemptOutcome::Error(e.to_string()),
                        None,
                    );
                    let (provider_id, model_name) =
                        super::lane_resolution_log::parse_provider_model(model_id);
                    djinn_telemetry::failover::increment_candidate_attempt(
                        djinn_telemetry::failover::OUTCOME_ERROR,
                        provider_id,
                        model_name,
                    );
                    skipped_count += 1;
                    continue;
                }
            }
        }

        // All candidates exhausted: the failover chain is depleted.
        // Record chain-exhausted telemetry (metrics + structured log).
        {
            let (provider_id, model_name) =
                super::lane_resolution_log::parse_provider_model(last_model_id);
            djinn_telemetry::failover::increment_chain_exhausted(provider_id, model_name);
            djinn_telemetry::failover::record_latency(failover_chain_start.elapsed());
        }
        // Emit reasoning-model typed-failure observability when the first
        // candidate (the primary dispatch target) was a reasoning model.
        // The chain was exhausted without rescue, so the outcome is a typed
        // failure rather than a kill or rescue.
        if !model_ids.is_empty()
            && djinn_telemetry::reasoning_kill::is_reasoning_model(&model_ids[0])
        {
            djinn_telemetry::reasoning_kill::increment(
                djinn_telemetry::reasoning_kill::MODEL_CONTEXT_REASONING,
                djinn_telemetry::reasoning_kill::FAILURE_CLASS_IDLE_STALL,
                djinn_telemetry::reasoning_kill::OUTCOME_TYPED_FAILURE,
            );
        }
        super::lane_resolution_log::emit_failover_chain_exhausted(
            label,
            role,
            last_model_id,
            total_candidates,
            exhausted_observations.len(),
            None,
        );
        if any_at_capacity {
            tracing::Span::current().record("outcome", "cap");
            DispatchOutcome::AtCapacity
        } else {
            tracing::Span::current().record("outcome", "error");
            DispatchOutcome::Failed {
                exhausted_observations,
            }
        }
    }

    /// Apply terminal side effects after failover-chain exhaustion.
    ///
    /// Called when [`try_dispatch_to_pool`] returns
    /// [`DispatchOutcome::Failed { .. }`] — all candidates were tried and none
    /// accepted the dispatch. Advances the per-task failure streak and applies
    /// an escalating cooldown so the task is not silently retried every tick.
    /// After [`MAX_DISPATCH_FAILURES`] consecutive exhaustions the task is
    /// terminally failed.
    ///
    /// Extracted from `dispatch_ready_tasks` so the chain-exhaustion terminal
    /// side-effect contract is unit-testable without the full dispatch loop.
    ///
    /// **AC2 breaker deferral**: This method also applies the circuit-breaker
    /// trip for each candidate that was observed to fail during *this chain's*
    /// traversal. Breaker demotion/cooldown is deferred from per-candidate
    /// failure time to this point — after all candidates are exhausted — so
    /// a successful fallback never triggers a breaker trip for an earlier
    /// candidate, and two unrelated failover chains cannot leak breaker
    /// observations into each other (AC2).
    ///
    /// `exhausted_observations` is the chain-scoped list of `(scope, model)`
    /// keys carried through from [`DispatchOutcome::Failed`]. It MUST come
    /// from the *same* dispatch attempt whose outcome triggered this call;
    /// observations from a fallback-rescued chain were discarded by
    /// `try_dispatch_to_pool` on the success path and cannot leak here, and
    /// observations from an unrelated exhaustion cannot leak in via any
    /// shared buffer (the previous `HealthTracker::pending_breaker_observations`
    /// global buffer was removed specifically to prevent that).
    pub(crate) async fn apply_chain_exhaustion_side_effects(
        &mut self,
        task: &djinn_core::models::Task,
        role: &str,
        exhausted_observations: &[djinn_provider::catalog::HealthKey],
    ) {
        // Apply deferred breaker checks for THIS chain's observed failures
        // only.  Observations from a fallback-rescued chain (which were
        // discarded by `try_dispatch_to_pool` on the success path) cannot
        // leak here, and observations from an unrelated exhaustion cannot
        // leak in via a global buffer. The breaker trip happens at most
        // once per chain-exhausted failover attempt.
        for key in exhausted_observations {
            self.health
                .apply_breaker_check_for(key.scope.as_deref(), &key.model_id);
        }

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
                task,
                role,
                "all failover candidates exhausted after multiple attempts. \
                 The task could not be dispatched to any configured model. \
                 Resolve the underlying issue and reopen.",
            )
            .await;
            self.dispatch_failure_streak.remove(&task.id);
            self.dispatch_cooldowns.remove(&task.id);
            self.inflight_dispatches.remove(&task.id);
            self.clear_durable_dispatch_backoff_state(
                &task.id,
                Some(&task.short_id),
                "chain_exhaustion_terminal_close_clear",
            )
            .await;
        } else {
            let cooldown = escalating_dispatch_cooldown(streak);
            tracing::warn!(
                task_id = %task.short_id,
                role,
                streak,
                cooldown_secs = cooldown.as_secs(),
                observed_candidates = exhausted_observations.len(),
                "CoordinatorActor: all failover candidates exhausted — backing off dispatch (escalating cooldown)"
            );
            self.dispatch_cooldowns
                .insert(task.id.clone(), SystemClock::new().now_instant() + cooldown);
            self.persist_durable_dispatch_state_update(
                &task.id,
                Some(&task.short_id),
                "chain_exhaustion_backoff",
                DurableDispatchStateUpdate {
                    failure_streak: Some(streak),
                    cooldown_until: Some(dispatch_wall_clock_after(cooldown)),
                    last_dispatched: Some(None),
                    ..Default::default()
                },
            )
            .await;
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
        let session_repo = SessionRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        // Keep the same handoff ordering as `effective_running_counts`: active
        // ids are captured before either count query. A session that appears
        // between these reads is double-counted for at most this pass (safe),
        // never missed while its ledger reservation is cleared (unsafe).
        let active_task_ids: HashSet<String> = match session_repo.list_active().await {
            Ok(sessions) => sessions.into_iter().filter_map(|s| s.task_id).collect(),
            Err(e) => {
                tracing::warn!(error = %e, "CoordinatorActor: failed to load active sessions for dispatch guard; proceeding without it");
                HashSet::new()
            }
        };
        let (model_rows, lane_rows) = tokio::join!(
            session_repo.count_active_by_user_and_model(),
            session_repo.count_active_by_user_and_lane(),
        );

        // Per-user, per-model concurrency: current running counts keyed by
        // (creator, model), seeded from the DB and bumped locally on each
        // dispatch this pass. A task only dispatches while its creator is under
        // their own cap for the chosen model and, when configured, under the
        // matching lane cap. The slot pool itself remains elastic (spawns on
        // demand, with no global ceiling).
        let mut running_by_user_model: HashMap<(String, String), u32> = match model_rows {
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

        let mut running_by_user_lane: HashMap<(String, djinn_core::models::ModelLane), u32> =
            match lane_rows {
                Ok(rows) => rows
                    .into_iter()
                    .filter_map(|(creator, lane, count)| {
                        creator.map(|c| ((c, lane), u32::try_from(count).unwrap_or(0)))
                    })
                    .collect(),
                Err(e) => {
                    tracing::warn!(error = %e, "CoordinatorActor: per-user lane concurrency counts failed; using in-flight reservations only");
                    HashMap::new()
                }
            };

        // In-flight dispatch ledger overlay. The DB seed above only counts
        // sessions that have reached `running`, but a worker pod takes 20-60s to
        // boot and write that row — so a task dispatched moments ago is invisible
        // to the seed. Dispatch passes that re-fire in that window would re-seed
        // from the stale-low count and overshoot the per-user cap (observed: 8
        // workers dispatched in one ~167ms burst for a cap of 4, because every
        // session row only landed ~20-60s later). Fix: capture active task ids
        // first, reconcile against that snapshot and pool liveness, then add the
        // retained booting reservations to DB counts. A session row that lands
        // between the active-id and count reads is conservatively double-counted
        // for one pass; it is never missed during the handoff. DB rows remain
        // the durable floor across server restarts.
        self.reconcile_inflight_dispatch_ledger(&active_task_ids)
            .await;
        overlay_inflight_ledger(&mut running_by_user_model, &self.inflight_dispatches);
        overlay_inflight_lane_ledger(&mut running_by_user_lane, &self.inflight_dispatches);
        self.overlay_provisional_admissions(&mut running_by_user_model, &mut running_by_user_lane);
        record_dispatch_live_state(
            self.dispatch_cooldowns.len(),
            self.inflight_dispatches.len(),
        );

        // Memoized per-creator cap maps (model_id → max concurrent) for this pass.
        let mut creator_caps: HashMap<String, std::collections::HashMap<String, u32>> =
            HashMap::new();
        let mut creator_lane_caps: HashMap<String, Option<djinn_core::models::LaneMaxSessions>> =
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
            if self
                .enforce_expired_arbiter_deadline_before_dispatch(&task)
                .await
            {
                tracing::info!(
                    task_id = %task.short_id,
                    "CoordinatorActor: expired active arbitration auto-parked before Lead dispatch"
                );
                continue;
            }
            let ctx = DispatchContext;
            let Some(role) = self.role_registry.dispatch_role_for_task(&task, &ctx) else {
                continue;
            };
            // Pre-dispatch respawn guard: consult attempt-history before any
            // fresh spawn/admission side-effects.  Guard ordering:
            // 1. Open-PR adoption: when the task has an existing open PR
            //    (task.pr_url), adopt it and record an adopted_pr audit row.
            //    Adoption is bypassed when the PR needs rework — the reopen
            //    flow returned the task to open so a worker can fix the PR,
            //    and adopting would starve that rework: failing required CI
            //    (task.ci_status == "failing", PrCiFailed flow), an
            //    unresolved merge conflict (task.merge_conflict_metadata,
            //    PrConflict flow), or a reopened-latest attempt (paths with
            //    no task-row column, e.g. PrChangesRequested / merge-queue
            //    dequeue with green PR-head checks).
            // 2. Non-terminal attempt: if a pending or submitted attempt
            //    already exists for this task+role, defer dispatch and record
            //    a guard-only audit row.  No dispatch / provider / reopen
            //    counters are incremented for the deferral.
            // Same-signature CI remediation dead-end (incident ay3d): an open
            // worker task whose failing required-CI PR already had a
            // remediation run against the CURRENT head
            // (`last_remediation_base_sha` == head) and whose failure signature
            // has persisted past the threshold would be deferred by the respawn
            // guard on EVERY ready pass forever — no new push to re-evaluate, no
            // escalation, no strike accrual, and no manual lever from `open`.
            // Break the wedge by routing it into the autonomous escalation
            // ladder: a planner-park escalation below the ceiling, terminal-fail
            // at it. Fires once — the escalation blocks + parks the source, so
            // it leaves the ready set on the next pass.
            if Self::ci_same_signature_deadlocked(&task, role) {
                tracing::warn!(
                    task_id = %task.short_id,
                    same_signature_count = task.ci_same_signature_count,
                    head = task.ci_github_head_sha.as_deref().or(task.ci_head_sha.as_deref()).unwrap_or("(unknown)"),
                    "CoordinatorActor: same-signature CI remediation dead-end — routing to autonomous escalation instead of deferring worker forever"
                );
                let head = task
                    .ci_github_head_sha
                    .as_deref()
                    .or(task.ci_head_sha.as_deref())
                    .unwrap_or("(unknown)")
                    .to_owned();
                let reason = format!(
                    "Required CI has failed on the same signature {} time(s) at head {} and a \
                     remediation already ran against that exact head with no new push — the worker \
                     cannot make forward progress by re-running. Escalating for terminal resolution.",
                    task.ci_same_signature_count, head,
                );
                self.escalate_to_planner_or_terminally_fail(&task, &reason)
                    .await;
                continue;
            }
            let pr_rework_signal = super::respawn_guard::PrReworkSignal::from_task_row(
                task.ci_status.as_str(),
                task.merge_conflict_metadata.as_deref(),
            );
            match super::respawn_guard::run_respawn_guard(
                &self.db,
                &task.id,
                role,
                task.pr_url.as_deref(),
                pr_rework_signal,
            )
            .await
            {
                super::respawn_guard::RespawnGuardDecision::Allow => {}
                super::respawn_guard::RespawnGuardDecision::Adopted { pr_url } => {
                    tracing::info!(
                        task_id = %task.short_id,
                        role,
                        pr_url = %pr_url,
                        "CoordinatorActor: respawn guard adopting existing open PR — handing off to PR poller"
                    );
                    super::respawn_guard::record_adopted_pr_attempt(
                        &self.db,
                        &task.id,
                        role,
                        &pr_url,
                        Some("respawn_guard: adopted existing open PR"),
                    )
                    .await;
                    // Adoption must imply ownership: move the task out of the
                    // dispatchable `open` column into the poller-owned
                    // `pr_review` column so the PR poller advances it (incident
                    // gton — an adopted `open` task was polled by nobody and
                    // wedged 9h). Idempotent: a no-op if already poller-owned.
                    super::respawn_guard::handoff_adopted_pr_to_poller(
                        &self.task_repo(),
                        &task.id,
                        &task.status,
                        &pr_url,
                    )
                    .await;
                    continue;
                }
                super::respawn_guard::RespawnGuardDecision::Defer(reason) => {
                    tracing::info!(
                        task_id = %task.short_id,
                        role,
                        reason = %reason,
                        "CoordinatorActor: respawn guard deferring dispatch"
                    );
                    super::respawn_guard::record_guard_deferred_attempt(
                        &self.db,
                        &task.id,
                        role,
                        reason,
                        Some("respawn_guard: non-terminal attempt in flight"),
                    )
                    .await;
                    continue;
                }
            }
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
                self.bump_local_cap_for_last_planner_admission(
                    &mut running_by_user_model,
                    &mut running_by_user_lane,
                )
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
                let reappearing = classify_reappearing_dispatch(marker, role, current_streak);
                // Environmental-interrupt exemption. A same-role reappearance is
                // normally a failed attempt (streak++ + escalating cooldown). But
                // when the prior session was killed by INFRASTRUCTURE — a
                // coordinator deploy/rollout, a k8s pod eviction, or a startup
                // reap of a run that deploy orphaned — the task did nothing wrong:
                // the classifier/reaper terminalized its attempt as `Interrupted`
                // (environmental non-attempt). Deploys happen many times a day, so
                // without this every deploy would march innocent in-flight tasks up
                // the cooldown ladder toward strikes/interventions/terminal close.
                // Treat it as if the attempt never ran: clear any backoff state and
                // dispatch immediately, contributing NO streak and NO cooldown.
                // Genuine `crashed`/`timed_out` attempts are NOT environmental and
                // still fall through to the ordinary failure accounting below.
                if matches!(
                    reappearing,
                    Some(ReappearingDispatch::SameRoleFailure { .. })
                ) && self
                    .latest_attempt_was_environmental_interrupt(&task.id, role)
                    .await
                {
                    self.dispatch_failure_streak.remove(&task.id);
                    self.provider_failure_streak.remove(&task.id);
                    self.dispatch_cooldowns.remove(&task.id);
                    self.clear_durable_dispatch_backoff_state(
                        &task.id,
                        Some(&task.short_id),
                        "environmental_interrupt_no_dispatch_penalty",
                    )
                    .await;
                    tracing::info!(
                        task_id = %task.short_id,
                        role,
                        "CoordinatorActor: prior session ended in an environmental interruption \
                         (deploy/rollout/pod-eviction/reap) — reappearance is NOT a dispatch \
                         failure; dispatching without streak or cooldown"
                    );
                    // Fall through to dispatch (deliberately no `continue`).
                } else {
                    match reappearing {
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
                                    &mut running_by_user_lane,
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
                                    &mut running_by_user_lane,
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
                                    cooldown_until: Some(dispatch_wall_clock_after(
                                        effective_cooldown,
                                    )),
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
            // path. When the human-park rung declined to park because no attempt
            // has reached submit_work yet, it redispatches — but a redispatch to
            // the SAME model that just terminated pre-submission (loop-guard trip,
            // infra death) would loop identically. Exclusions are derived from
            // `task_attempts` rows via `PostInterventionHistory::rotation_excluded_models()`;
            // only actual model IDs (provider/model) are excluded — outcome
            // fallback strings from failed session lookups are skipped.
            // Degrades to the unfiltered list when exclusion would empty it
            // (only one viable model → plan-lane retry, then park at the bound).
            if role == "worker" && task.intervention_count >= 1 {
                let history = self.post_intervention_history(&task).await;
                let rotation_excluded = history.rotation_excluded_models();
                if !rotation_excluded.is_empty() {
                    let filtered: Vec<String> = model_ids
                        .iter()
                        .filter(|m| !rotation_excluded.contains(m))
                        .cloned()
                        .collect();
                    if !filtered.is_empty() && filtered.len() < model_ids.len() {
                        tracing::info!(
                            task_id = %task.short_id,
                            excluded = ?rotation_excluded,
                            "uv3p: forcing model rotation on post-intervention redispatch — excluding models that terminated pre-submission"
                        );
                        model_ids = filtered;
                    }
                }
            }

            // zkk9: Enforce arbiter-monitored-reopen `exclude_models` for the
            // one monitored worker dispatch.  When an arbiter issued a `reopen`
            // decision, the directive/excluded models were persisted on the
            // current unconsumed arbitration row.  If this worker dispatch is
            // the monitored attempt, apply those exclusions.  Unlike the
            // rotation exclusions above, these do NOT degrade to the unfiltered
            // list — if no eligible model remains, the task is parked with an
            // updated dossier rather than cycling another worker.
            if role == "worker" {
                let arb_repo = TaskArbitrationRepository::new(self.db.clone());
                if let Ok((_cycle, Some(arb_record))) =
                    arb_repo.resolve_current_hold_cycle(&task.id).await
                    && arb_record.monitored_reopen_count >= 1
                    && !arb_record.directive_injected
                {
                    // This is the monitored reopen worker dispatch.
                    let reopen_excluded: Vec<String> = arb_record
                        .excluded_models
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    if !reopen_excluded.is_empty() {
                        let filtered: Vec<String> = model_ids
                            .iter()
                            .filter(|m| !reopen_excluded.contains(m))
                            .cloned()
                            .collect();
                        tracing::info!(
                            task_id = %task.short_id,
                            excluded = ?reopen_excluded,
                            remaining = filtered.len(),
                            "zkk9: enforcing arbiter reopen exclude_models for monitored worker dispatch"
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

            // Dispatch-time throttle deprioritization: move any model that is
            // currently inside a throttle cooldown window to the BACK of the
            // ordered list so a healthy lane-mate is attempted first. A model
            // whose token plan just 429'd is likely still quota-dead; retrying
            // it head-of-line wastes a session spawn + a <10s crash + the
            // ~30-minute task redispatch cooldown before failover finally
            // happens. This is a pure reorder (not a filter): a throttle-cooling
            // model stays in the list as a last-resort candidate, so if every
            // lane model is cooling the existing all-unavailable path still
            // applies (the dispatch loop's `is_available` gate + escalating
            // backoff), and a half-open (cooldown-expired) throttle model is
            // still tried when it is the only option.
            deprioritize_throttle_cooling(&self.health, creator.as_deref(), &mut model_ids);

            // Structured observability: emit one log record per candidate in
            // the final ordered list so post-apply / post-rollback model order
            // can be inspected without production-only tooling.
            super::lane_resolution_log::emit_lane_resolution_candidates(
                &task.short_id,
                role,
                creator.as_deref().unwrap_or(""),
                &model_ids,
            );

            // zkk9: No-eligible-model parking for monitored reopen.  If the
            // arbiter's `exclude_models` eliminated all worker models for the
            // monitored reopen dispatch, park with an updated dossier instead
            // of cycling another worker or arbiter.  Also mark the monitored
            // attempt complete so re-entry cannot trigger a second cycle.
            if model_ids.is_empty() && role == "worker" {
                let arb_repo = TaskArbitrationRepository::new(self.db.clone());
                let should_park_reopen = match arb_repo.resolve_current_hold_cycle(&task.id).await {
                    Ok((cycle, Some(rec))) => (rec.monitored_reopen_count >= 1
                        && !rec.directive_injected)
                        .then_some((cycle, rec)),
                    _ => None,
                };
                if let Some((hold_cycle, rec)) = should_park_reopen {
                    let park_reason = "arbiter reopen exclude_models eliminated all worker models";
                    let dossier = serde_json::json!({
                        "reason": park_reason,
                        "task_id": task.short_id,
                        "kind": "monitored_reopen_no_eligible_model",
                        "excluded_models": rec.excluded_models,
                        "directive": rec.directive,
                        "verification_command": rec.verification_command,
                    });
                    tracing::warn!(
                        task_id = %task.short_id,
                        "zkk9: monitored reopen exclude_models left no eligible worker model — parking with dossier"
                    );
                    // Persist the dossier on the arbitration row so the
                    // externally visible contract carries the explicit
                    // `monitored_reopen_no_eligible_model` evidence.
                    use djinn_db::repositories::task_arbitration::UpdateDispatchLedgerParams;
                    let _ = arb_repo
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
                        .await;
                    // Complete the monitored attempt so re-entry cannot retry.
                    let _ = arb_repo
                        .complete_monitored_reopen(&task.id, hold_cycle)
                        .await;
                    let quality_strikes = self
                        .task_repo()
                        .quality_reopen_count(&task.id)
                        .await
                        .unwrap_or(0);
                    self.park_source_human_review_with_dossier(
                        &task,
                        park_reason,
                        quality_strikes,
                        Some(dossier),
                        &serde_json::json!({}),
                    )
                    .await;
                    continue;
                }
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
                    let settings = djinn_db::UserSettingsRepository::new(self.db.clone())
                        .get(c)
                        .await
                        .ok()
                        .flatten();
                    creator_caps.insert(
                        c.to_string(),
                        settings
                            .as_ref()
                            .and_then(|s| s.max_sessions.clone())
                            .unwrap_or_default(),
                    );
                    creator_lane_caps
                        .insert(c.to_string(), settings.and_then(|s| s.lane_max_sessions));
                }

                let lane = djinn_core::models::ModelLane::for_role(role);
                let lane_cap = creator_lane_caps[c]
                    .as_ref()
                    .map(|limits| limits.lane(lane));
                if !lane_under_user_cap(&running_by_user_lane, c, lane, lane_cap) {
                    record_dispatch_outcome(djinn_telemetry::dispatch::OUTCOME_CAP);
                    tracing::debug!(
                        outcome = "cap",
                        task_id = %task.short_id,
                        role,
                        lane = ?lane,
                        lane_cap,
                        "CoordinatorActor: task owner at per-lane concurrency cap — deferring"
                    );
                    super::respawn_guard::record_guard_deferred_attempt(
                        &self.db,
                        &task.id,
                        role,
                        djinn_core::models::task_attempt::GuardReason::Capacity,
                        Some("capacity: user at per-lane concurrency cap"),
                    )
                    .await;
                    continue;
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
                    // Audit: record a guard-deferred row for the capacity
                    // deferral.  Best-effort; no counters incremented.
                    super::respawn_guard::record_guard_deferred_attempt(
                        &self.db,
                        &task.id,
                        role,
                        djinn_core::models::task_attempt::GuardReason::Capacity,
                        Some("capacity: user at per-model concurrency cap"),
                    )
                    .await;
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

            let build_admission = match self.begin_task_run_build_admission(
                role,
                &task.id,
                i64::from(task.reopen_count.max(0)),
                format!("task-run-{}-{}", task.id, task.reopen_count.max(0)),
            ).await {
                Ok(permit) => permit,
                Err(()) => continue,
            };

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

            self.finish_task_run_build_admission(build_admission, matches!(outcome, DispatchOutcome::Dispatched)).await;

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
                    // Attempt lifecycle: record the dispatch-start as a
                    // pending task_attempt row. Best-effort — never fails the
                    // dispatch path.
                    let dispatch_key = super::attempt_lifecycle::make_dispatch_key(&task.id, role);
                    super::attempt_lifecycle::record_dispatch_start(
                        &self.db,
                        &task.id,
                        role,
                        None,
                        &dispatch_key,
                    )
                    .await;
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
                        *running_by_user_lane
                            .entry((c.to_string(), djinn_core::models::ModelLane::for_role(role)))
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
                            role,
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
                DispatchOutcome::Failed {
                    exhausted_observations,
                } => {
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

                    // Chain-exhaustion terminal side effects: advance the
                    // failure streak and apply escalating cooldown so the
                    // task is not silently retried every tick without
                    // backoff. Pass this chain's exhausted observations so
                    // breaker side effects apply ONLY to failures from THIS
                    // dispatch attempt — not from a fallback-rescued or
                    // unrelated earlier chain (AC2).
                    self.apply_chain_exhaustion_side_effects(&task, role, &exhausted_observations)
                        .await;
                }
            }
        }
        self.publish_status();
    }
}

/// Reorder `model_ids` in place so that models currently inside a **throttle
/// cooldown window** are moved to the back, preserving relative order within
/// each group (healthy/available first, throttle-cooling last).
///
/// This is a pure reorder, not a filter: a throttle-cooling model stays in the
/// list as a last-resort candidate. Its purpose is to prefer a healthy lane-mate
/// over a model whose token plan just 429'd — avoiding a wasted session spawn +
/// <10s crash + task-redispatch cooldown on a model known to be cooling — while
/// keeping the all-cooling / only-candidate case sane: the dispatch loop's
/// `is_available` gate still skips a model in an *active* cooldown, and a
/// half-open (cooldown-expired) throttle model is still attempted when it is the
/// only option. A list shorter than two candidates is left untouched.
fn deprioritize_throttle_cooling(
    health: &djinn_provider::catalog::HealthTracker,
    scope: Option<&str>,
    model_ids: &mut Vec<String>,
) {
    if model_ids.len() < 2 {
        return;
    }
    let mut cooling: Vec<String> = Vec::new();
    model_ids.retain(|m| {
        if health.is_throttle_cooling(scope, m) {
            cooling.push(m.clone());
            false
        } else {
            true
        }
    });
    model_ids.extend(cooling);
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
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            ci_mq_state: None,
            ci_mq_run_id: None,
            ci_mq_head_sha: None,
            ci_mq_failed_check_names: None,
            ci_mq_failure_fingerprint: None,
            ci_mq_same_signature_count: None,
            ci_mq_first_seen_at: None,
            ci_mq_last_seen_at: None,
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

    fn inflight_entry(
        creator: Option<&str>,
        model: &str,
        lane: djinn_core::models::ModelLane,
    ) -> InflightDispatch {
        InflightDispatch {
            creator: creator.map(str::to_owned),
            model: model.to_owned(),
            lane,
        }
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
            #[cfg(test)]
            test_use_live_credential_resolution: false,
            pr_errors: HashMap::new(),
            last_dispatched: HashMap::new(),
            inflight_dispatches: HashMap::new(),
            provisional_admissions: HashMap::new(),
            dispatch_cooldowns: HashMap::new(),
            dispatch_failure_streak: HashMap::new(),
            background_work_tracker: BackgroundWorkTracker::default(),
            stranded_ready_source: None,
            closed_parent_open_children_source: None,
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
            stall_extension_count: HashMap::new(),
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
            // LedgerOverlay and CapConsidered may conservatively count both a
            // newly visible session row and its reservation for one pass. The
            // admission invariant is the count after a successful increment.
            .filter(|obs| obs.stage == DispatchCapObservationStage::InflightIncremented)
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
        actor
            .reconcile_inflight_dispatch_ledger(&HashSet::new())
            .await;

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
                obs.stage == DispatchCapObservationStage::InflightIncremented
                    && obs.creator_user_id == fixture.created_by_user_id
                    && obs.model == fixture.model_id
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
                    obs.stage == DispatchCapObservationStage::InflightIncremented
                        && obs.creator_user_id == fixture.created_by_user_id
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
        let mut inflight: HashMap<String, InflightDispatch> = HashMap::new();
        for i in 0..4 {
            inflight.insert(
                format!("task-{i}"),
                inflight_entry(
                    Some("user-a"),
                    "openai/gpt-5.5",
                    djinn_core::models::ModelLane::Implement,
                ),
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

    /// Reconciliation makes DB-running and booting ledger reservations
    /// disjoint, so one existing running task plus one booting task must count
    /// as two. This is the case `max(DB, ledger)` previously undercounted.
    #[test]
    fn ledger_overlay_adds_disjoint_booting_reservations() {
        clear_dispatch_cap_observations();
        let mut running = HashMap::from([(key("user-a", "m"), 1)]);
        let inflight = HashMap::from([(
            "booting-task".to_owned(),
            inflight_entry(
                Some("user-a"),
                "m",
                djinn_core::models::ModelLane::Implement,
            ),
        )]);

        overlay_inflight_ledger(&mut running, &inflight);

        assert_eq!(running.get(&key("user-a", "m")).copied(), Some(2));
        assert_eq!(
            take_dispatch_cap_observations(),
            vec![DispatchCapObservation {
                creator_user_id: "user-a".to_owned(),
                model: "m".to_owned(),
                effective_count: 2,
                stage: DispatchCapObservationStage::LedgerOverlay,
            }],
            "observer must see DB-running plus the disjoint booting reservation"
        );
    }

    #[test]
    fn active_first_handoff_snapshot_retains_reservation_before_later_count() {
        use djinn_core::models::ModelLane;

        // T1: active-id snapshot is taken while the worker is still booting.
        let active_task_ids = HashSet::new();
        let live_task_ids = HashSet::from(["handoff-task".to_owned()]);
        assert!(retain_inflight_reservation(
            "handoff-task",
            &active_task_ids,
            Some(&live_task_ids),
        ));

        // T2: the session row lands before the two count queries. The earlier
        // active-id snapshot must still retain the ledger entry, making this a
        // conservative count of 2 for one pass rather than the unsafe 1→0
        // handoff that would admit another dispatch.
        let ledger = HashMap::from([(
            "handoff-task".to_owned(),
            inflight_entry(Some("user-a"), "shared/model", ModelLane::Implement),
        )]);
        let mut by_model = HashMap::from([(key("user-a", "shared/model"), 1)]);
        let mut by_lane = HashMap::from([(("user-a".to_owned(), ModelLane::Implement), 1)]);
        overlay_inflight_ledger(&mut by_model, &ledger);
        overlay_inflight_lane_ledger(&mut by_lane, &ledger);

        assert_eq!(by_model.get(&key("user-a", "shared/model")), Some(&2));
        assert_eq!(
            by_lane.get(&("user-a".to_owned(), ModelLane::Implement)),
            Some(&2)
        );
        assert!(!model_under_user_cap(
            &by_model,
            "user-a",
            "shared/model",
            2,
        ));
        assert!(!lane_under_user_cap(
            &by_lane,
            "user-a",
            ModelLane::Implement,
            Some(2),
        ));
    }

    #[test]
    fn lane_cap_counts_running_plus_booting_across_models() {
        use djinn_core::models::ModelLane;

        let mut running_by_lane = HashMap::from([(("user-a".to_owned(), ModelLane::Plan), 1)]);
        let inflight = HashMap::from([(
            "booting-planner".to_owned(),
            inflight_entry(Some("user-a"), "other/model", ModelLane::Plan),
        )]);
        overlay_inflight_lane_ledger(&mut running_by_lane, &inflight);

        assert_eq!(
            running_by_lane
                .get(&("user-a".to_owned(), ModelLane::Plan))
                .copied(),
            Some(2)
        );
        assert!(!lane_under_user_cap(
            &running_by_lane,
            "user-a",
            ModelLane::Plan,
            Some(2),
        ));
        assert!(lane_under_user_cap(
            &running_by_lane,
            "user-a",
            ModelLane::Implement,
            Some(1),
        ));
    }

    #[test]
    fn missing_lane_cap_is_unbounded() {
        let running_by_lane = HashMap::from([(
            (
                "legacy-user".to_owned(),
                djinn_core::models::ModelLane::Implement,
            ),
            99,
        )]);
        assert!(lane_under_user_cap(
            &running_by_lane,
            "legacy-user",
            djinn_core::models::ModelLane::Implement,
            None,
        ));
    }

    #[test]
    fn lane_caps_are_independent_when_all_roles_share_one_model() {
        use djinn_core::models::ModelLane;

        // All three lanes use the same model. The legacy per-model ceiling has
        // room, so the lane gates alone decide which role can dispatch.
        let by_model = HashMap::from([(key("user-a", "shared/model"), 3)]);
        assert!(model_under_user_cap(&by_model, "user-a", "shared/model", 4,));

        let by_lane = HashMap::from([
            (("user-a".to_owned(), ModelLane::Plan), 1),
            (("user-a".to_owned(), ModelLane::Implement), 1),
            (("user-a".to_owned(), ModelLane::Review), 1),
        ]);
        assert!(!lane_under_user_cap(
            &by_lane,
            "user-a",
            ModelLane::Plan,
            Some(1),
        ));
        assert!(lane_under_user_cap(
            &by_lane,
            "user-a",
            ModelLane::Implement,
            Some(2),
        ));
        assert!(!lane_under_user_cap(
            &by_lane,
            "user-a",
            ModelLane::Review,
            Some(1),
        ));
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
        let mut inflight: HashMap<String, InflightDispatch> = HashMap::new();
        inflight.insert(
            "task-a".into(),
            inflight_entry(Some(user), model, djinn_core::models::ModelLane::Implement),
        );
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
        let mut inflight: HashMap<String, InflightDispatch> = HashMap::new();
        inflight.insert(
            "sys".into(),
            inflight_entry(None, "m", djinn_core::models::ModelLane::Plan),
        );
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

/// Failover-chain traversal regression tests.
///
/// Verifies that `try_dispatch_to_pool` traverses the routed candidate list in
/// order, advances past failed (breaker-open / at-capacity) candidates, and
/// preserves per-candidate observability logging. These tests lock the
/// contracts delivered by blocking epics `5wxi` (lane candidate ordering) and
/// `13w7` (restamp helper) so that the coordinator dispatch path correctly
/// consumes the routed failover chain.
#[cfg(test)]
mod failover_chain_tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    /// Minimal actor factory for failover chain tests.
    ///
    /// Creates a `CoordinatorActor` with a pool configured for the given
    /// models and a fresh `HealthTracker`. The pool uses a controlled slot
    /// factory whose slots stay alive until the `releases` map is drained.
    #[allow(clippy::type_complexity)]
    fn failover_actor(
        db: &djinn_db::Database,
        events_tx: &tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
        models: Vec<(&str, u32)>, // (model_id, max_slots)
    ) -> (
        CoordinatorActor,
        tokio_util::sync::CancellationToken,
        Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>>,
    ) {
        use djinn_slot::{ModelSlotConfig, SlotPoolConfig};

        let cancel = tokio_util::sync::CancellationToken::new();
        let releases: Arc<Mutex<HashMap<String, tokio::sync::oneshot::Sender<()>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let releases_clone = releases.clone();

        let pool_config = SlotPoolConfig {
            models: models
                .iter()
                .map(|(model_id, max_slots)| ModelSlotConfig {
                    model_id: (*model_id).to_owned(),
                    max_slots: *max_slots,
                    roles: ["worker".to_owned()].into_iter().collect(),
                })
                .collect(),
            role_priorities: HashMap::new(),
        };

        let app_state = crate::test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = djinn_slot::SlotPoolHandle::spawn_with_factory(
            app_state,
            cancel.clone(),
            pool_config,
            Arc::new(move |_slot_id, model_id, _event_tx, _app_state, kill| {
                let releases_inner = releases_clone.clone();
                let runner: djinn_slot::TestLifecycleRunner = Arc::new(
                    move |task_id, _project_path, _model_id, _app_state, kill, _pause, _resume| {
                        let releases_inner = releases_inner.clone();
                        Box::pin(async move {
                            let (release_tx, release_rx) = tokio::sync::oneshot::channel();
                            releases_inner
                                .lock()
                                .expect("failover release mutex")
                                .insert(task_id.clone(), release_tx);
                            tokio::select! {
                                _ = release_rx => {}
                                _ = kill.cancelled() => {}
                                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
                            }
                            Ok(())
                        })
                    },
                );
                djinn_slot::SlotHandle::spawn_with_test_runner(
                    0, model_id, _event_tx, _app_state, kill, runner,
                )
            }),
        );

        let (status_tx, _status_rx) = tokio::sync::watch::channel(SharedCoordinatorState {
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        });

        let actor = CoordinatorActor {
            receiver: tokio::sync::mpsc::channel(1).1,
            events: events_tx.subscribe(),
            cancel: cancel.clone(),
            tick: tokio::time::interval(std::time::Duration::from_secs(60)),
            db: db.clone(),
            events_tx: events_tx.clone(),
            pool: pool.clone(),
            catalog: CatalogService::new(),
            health: djinn_provider::catalog::health::HealthTracker::new(),
            role_registry: std::sync::Arc::new(crate::roles::RoleRegistry::new()),
            lsp: djinn_lsp::LspManager::new(),
            self_sender: tokio::sync::mpsc::channel(1).0,
            status_tx,
            dispatch_limit: 50,
            model_priorities: HashMap::new(),
            #[cfg(test)]
            test_use_live_credential_resolution: false,
            pr_errors: HashMap::new(),
            last_dispatched: HashMap::new(),
            inflight_dispatches: HashMap::new(),
            provisional_admissions: HashMap::new(),
            dispatch_cooldowns: HashMap::new(),
            dispatch_failure_streak: HashMap::new(),
            background_work_tracker: BackgroundWorkTracker::default(),
            stranded_ready_source: None,
            closed_parent_open_children_source: None,
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
            pr_status_cache: HashMap::new(),
            pr_draft_first_seen: HashMap::new(),
            review_stuck_sha_first_seen: HashMap::new(),
            merge_fail_count: HashMap::new(),
            auto_approve_attempted: HashMap::new(),
            delegated_to_github: HashMap::new(),
            conversations_resolved: HashMap::new(),
            handled_dequeues: HashMap::new(),
            stall_killed: std::collections::HashSet::new(),
            stall_progress_watermark: HashMap::new(),
            stall_cancel_streak: HashMap::new(),
            stall_extension_count: HashMap::new(),
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
        };

        (actor, cancel, releases)
    }

    /// AC1: Failover-chain traversal preserves candidate order.
    ///
    /// When the first candidate's breaker is open, dispatch should advance to the
    /// second candidate and succeed — proving the chain is traversed in routed
    /// lane order rather than giving up on the first failure.
    #[tokio::test]
    async fn failover_chain_advances_past_breaker_to_next_candidate() {
        let _ = djinn_telemetry::init();
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        // 3 models, each with 1 slot
        let (actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![
                ("provider/model-a", 1),
                ("provider/model-b", 1),
                ("provider/model-c", 1),
            ],
        );

        // Trip breaker for model-a
        for _ in 0..3 {
            actor.health.record_failure(None, "provider/model-a");
        }

        // Dispatch with candidate order [model-a, model-b, model-c].
        // model-a breaker is open; dispatch should advance to model-b.
        let outcome = actor
            .try_dispatch_to_pool(
                "failover-task",
                "worker",
                0,
                None,
                &[
                    "provider/model-a".to_owned(),
                    "provider/model-b".to_owned(),
                    "provider/model-c".to_owned(),
                ],
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = "failover-task".to_owned();
                    let pp = "/tmp/proj".to_owned();
                    let mid = model_id.to_owned();
                    async move { pool.dispatch(&tid, &pp, &mid).await }
                },
            )
            .await;

        assert!(
            matches!(outcome, DispatchOutcome::Dispatched),
            "dispatch should succeed on model-b after model-a breaker is open"
        );

        // Verify the task was dispatched to model-b (not model-a or model-c)
        let status = actor.pool.get_status().await.unwrap();
        let model_b_running = status
            .running_tasks
            .iter()
            .filter(|t| t.model_id == "provider/model-b" && t.task_id == "failover-task")
            .count();
        assert!(
            model_b_running > 0,
            "failover-task should be running on model-b"
        );
        assert!(
            actor
                .pool
                .has_session("failover-task")
                .await
                .unwrap_or(false),
            "failover-task should have an active session"
        );

        cancel.cancel();
    }

    /// AC1 (breaker path): When the first candidate's breaker is open,
    /// dispatch should advance to the next eligible candidate.
    #[tokio::test]
    async fn failover_chain_advances_past_breaker_open() {
        let _ = djinn_telemetry::init();
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let (actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![
                ("provider/model-a", 1),
                ("provider/model-b", 1),
                ("provider/model-c", 1),
            ],
        );

        // Trip the breaker for model-a (stall trips immediately)
        for _ in 0..3 {
            actor.health.record_failure(None, "provider/model-a");
        }

        // Dispatch with candidate order [model-a, model-b, model-c].
        // model-a should be breaker-open; dispatch should advance to model-b.
        let outcome = actor
            .try_dispatch_to_pool(
                "breaker-task",
                "worker",
                0,
                None,
                &[
                    "provider/model-a".to_owned(),
                    "provider/model-b".to_owned(),
                    "provider/model-c".to_owned(),
                ],
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = "breaker-task".to_owned();
                    let pp = "/tmp/proj".to_owned();
                    let mid = model_id.to_owned();
                    async move { pool.dispatch(&tid, &pp, &mid).await }
                },
            )
            .await;

        assert!(
            matches!(outcome, DispatchOutcome::Dispatched),
            "dispatch should succeed on model-b after model-a breaker is open"
        );

        cancel.cancel();
    }

    /// AC3: When all candidates are exhausted (all breaker-open),
    /// the chain traversal returns `Failed` without marking terminal state.
    #[tokio::test]
    async fn failover_chain_all_breakers_exhausted_returns_failed() {
        let _ = djinn_telemetry::init();
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let (actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![("provider/model-a", 1), ("provider/model-b", 1)],
        );

        // Trip breakers for both models
        for _ in 0..3 {
            actor.health.record_failure(None, "provider/model-a");
            actor.health.record_failure(None, "provider/model-b");
        }

        // Dispatch with candidate order [model-a, model-b] — both breaker-open
        let outcome = actor
            .try_dispatch_to_pool(
                "exhausted-task",
                "worker",
                0,
                None,
                &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = "exhausted-task".to_owned();
                    let pp = "/tmp/proj".to_owned();
                    let mid = model_id.to_owned();
                    async move { pool.dispatch(&tid, &pp, &mid).await }
                },
            )
            .await;

        assert!(
            matches!(outcome, DispatchOutcome::Failed { .. }),
            "dispatch should return Failed when all candidates have open breakers"
        );

        cancel.cancel();
    }

    /// AC1: Mixed scenario — breaker-open on first two, success on third.
    /// Verifies the full failover chain traversal skips multiple candidates.
    #[tokio::test]
    async fn failover_chain_mixed_breaker_success() {
        let _ = djinn_telemetry::init();
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let (actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![
                ("provider/model-a", 1),
                ("provider/model-b", 1),
                ("provider/model-c", 1),
            ],
        );

        // model-a: breaker-open
        for _ in 0..3 {
            actor.health.record_failure(None, "provider/model-a");
        }

        // model-b: breaker-open
        for _ in 0..3 {
            actor.health.record_failure(None, "provider/model-b");
        }

        // model-c: free (should succeed)

        // Dispatch with candidate order [model-a, model-b, model-c]
        let outcome = actor
            .try_dispatch_to_pool(
                "mixed-task",
                "worker",
                0,
                None,
                &[
                    "provider/model-a".to_owned(),
                    "provider/model-b".to_owned(),
                    "provider/model-c".to_owned(),
                ],
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = "mixed-task".to_owned();
                    let pp = "/tmp/proj".to_owned();
                    let mid = model_id.to_owned();
                    async move { pool.dispatch(&tid, &pp, &mid).await }
                },
            )
            .await;

        assert!(
            matches!(outcome, DispatchOutcome::Dispatched),
            "dispatch should succeed on model-c after model-a and model-b breaker-open"
        );

        cancel.cancel();
    }

    /// AC1 + AC3: Candidate order is preserved — first eligible candidate wins.
    /// When model-a is breaker-open and model-b is free, dispatch should use
    /// model-b (not skip to model-c).
    #[tokio::test]
    async fn failover_chain_first_eligible_candidate_wins() {
        let _ = djinn_telemetry::init();
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let (actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![
                ("provider/model-a", 1),
                ("provider/model-b", 1),
                ("provider/model-c", 1),
            ],
        );

        // model-a: breaker-open (skip it)
        for _ in 0..3 {
            actor.health.record_failure(None, "provider/model-a");
        }

        // model-b and model-c are both free
        // Dispatch should succeed on model-b (first eligible), not model-c
        let outcome = actor
            .try_dispatch_to_pool(
                "order-task",
                "worker",
                0,
                None,
                &[
                    "provider/model-a".to_owned(),
                    "provider/model-b".to_owned(),
                    "provider/model-c".to_owned(),
                ],
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = "order-task".to_owned();
                    let pp = "/tmp/proj".to_owned();
                    let mid = model_id.to_owned();
                    async move { pool.dispatch(&tid, &pp, &mid).await }
                },
            )
            .await;

        assert!(
            matches!(outcome, DispatchOutcome::Dispatched),
            "dispatch should succeed on model-b (first eligible in order)"
        );

        // Verify task is on model-b by checking the pool status
        let status = actor.pool.get_status().await.unwrap();
        let model_b_running = status
            .running_tasks
            .iter()
            .filter(|t| t.model_id == "provider/model-b")
            .count();
        assert!(
            model_b_running > 0,
            "model-b should have a running task (order-task dispatched to first eligible candidate)"
        );

        // model-c should NOT have any running tasks (dispatch stopped at model-b)
        let model_c_running = status
            .running_tasks
            .iter()
            .filter(|t| t.model_id == "provider/model-c")
            .count();
        assert_eq!(
            model_c_running, 0,
            "model-c should NOT have a running task — dispatch succeeded on model-b first"
        );

        cancel.cancel();
    }

    /// AC2: When candidates have different restamped ProviderConfig,
    /// each candidate dispatch uses the model_id from the routed candidate
    /// list (the downstream restamp helper resolves ProviderConfig per-model).
    /// This test verifies that each model_id in the candidate list is passed
    /// to the dispatch function, which is the prerequisite for restamping.
    #[tokio::test]
    async fn failover_chain_passes_correct_model_ids_to_dispatch() {
        let _ = djinn_telemetry::init();
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let (actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![
                ("provider/model-a", 1),
                ("provider/model-b", 1),
                ("provider/model-c", 1),
            ],
        );

        // Track which models the dispatch_fn was called with
        let attempted_models: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let attempted_models_clone = attempted_models.clone();

        // Breaker-open model-a; model-b and model-c are free
        for _ in 0..3 {
            actor.health.record_failure(None, "provider/model-a");
        }

        // Dispatch with a closure that tracks which models are attempted
        let outcome = actor
            .try_dispatch_to_pool(
                "model-tracking-task",
                "worker",
                0,
                None,
                &[
                    "provider/model-a".to_owned(),
                    "provider/model-b".to_owned(),
                    "provider/model-c".to_owned(),
                ],
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = "model-tracking-task".to_owned();
                    let pp = "/tmp/proj".to_owned();
                    let mid = model_id.to_owned();
                    let tracker = attempted_models_clone.clone();
                    async move {
                        // Note: model-a is skipped by breaker before dispatch_fn
                        tracker.lock().unwrap().push(mid.clone());
                        pool.dispatch(&tid, &pp, &mid).await
                    }
                },
            )
            .await;

        assert!(matches!(outcome, DispatchOutcome::Dispatched));

        // The dispatch_fn should have been called for model-b (first eligible,
        // succeeds), but NOT for model-a (breaker-open) or model-c (chain
        // already succeeded on model-b).
        let attempted = attempted_models.lock().unwrap().clone();
        assert!(
            attempted.contains(&"provider/model-b".to_string()),
            "dispatch_fn should have been called with model-b (first eligible candidate)"
        );
        // model-a is skipped by the breaker check BEFORE dispatch_fn is called,
        // so it should NOT appear in the attempted list
        assert!(
            !attempted.contains(&"provider/model-a".to_string()),
            "dispatch_fn should NOT have been called with model-a (breaker-open)"
        );
        // model-c is NOT called because model-b succeeded — the chain stops
        // at the first successful candidate
        assert!(
            !attempted.contains(&"provider/model-c".to_string()),
            "dispatch_fn should NOT have been called with model-c (model-b already succeeded)"
        );

        cancel.cancel();
    }

    /// AC4: Per-candidate observation/logging — each candidate in the
    /// failover chain is logged, including those that were skipped or failed.
    ///
    /// Uses `#[tracing_test::traced_test]` to capture structured log output and
    /// assert that:
    /// - `failover_candidate_attempt` is emitted for the breaker-open candidate
    ///   (model-a) with the correct model_id.
    /// - `failover_candidate_accepted` is emitted for the winning candidate
    ///   (model-b) with the correct model_id.
    /// - The correct candidate_index and total_candidates appear in the logs.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn failover_chain_logging_captures_candidate_events() {
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let (actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![
                ("provider/model-a", 1),
                ("provider/model-b", 1),
                ("provider/model-c", 1),
            ],
        );

        // Breaker-open model-a; model-b and model-c are free
        for _ in 0..3 {
            actor.health.record_failure(None, "provider/model-a");
        }

        let outcome = actor
            .try_dispatch_to_pool(
                "logging-event-task",
                "worker",
                0,
                None,
                &[
                    "provider/model-a".to_owned(),
                    "provider/model-b".to_owned(),
                    "provider/model-c".to_owned(),
                ],
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = "logging-event-task".to_owned();
                    let pp = "/tmp/proj".to_owned();
                    let mid = model_id.to_owned();
                    async move { pool.dispatch(&tid, &pp, &mid).await }
                },
            )
            .await;

        assert!(
            matches!(outcome, DispatchOutcome::Dispatched),
            "dispatch should succeed on model-b after model-a breaker-open"
        );

        // ── Assert failover_candidate_attempt for the breaker-open model-a ──
        assert!(
            logs_contain("failover_candidate_attempt"),
            "must log failover_candidate_attempt for the breaker-open candidate"
        );
        assert!(
            logs_contain("provider/model-a"),
            "failover_candidate_attempt must reference the skipped model-a"
        );

        // ── Assert failover_candidate_accepted for the winning model-b ──────
        assert!(
            logs_contain("failover_candidate_accepted"),
            "must log failover_candidate_accepted for the winning candidate"
        );
        assert!(
            logs_contain("provider/model-b"),
            "failover_candidate_accepted must reference the accepted model-b"
        );

        cancel.cancel();
    }

    /// AC2 + AC4: Failover restamp produces model-dependent ProviderConfig
    /// defaults for each candidate model.
    ///
    /// Verifies that `restamp_provider_config_for_model` re-resolves
    /// model-dependent fields (`context_window`, `capabilities`, `format_family`)
    /// from the target model's RestampTarget rather than carrying stale values
    /// from the previous candidate. The dispatch closure calls the restamp
    /// helper for the dispatched candidate model, capturing the result for
    /// post-dispatch assertion.
    #[tokio::test]
    async fn failover_chain_restamp_produces_model_dependent_config() {
        let _ = djinn_telemetry::init();
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let (actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![("provider/model-a", 1), ("provider/model-b", 1)],
        );

        // Breaker-open model-a; model-b is free
        for _ in 0..3 {
            actor.health.record_failure(None, "provider/model-a");
        }

        // Capture restamped ProviderConfig for the dispatched candidate model.
        let restamped_config: Arc<Mutex<Option<djinn_provider::provider::ProviderConfig>>> =
            Arc::new(Mutex::new(None));
        let restamped_clone = restamped_config.clone();

        let outcome = actor
            .try_dispatch_to_pool(
                "restamp-task",
                "worker",
                0,
                None,
                &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = "restamp-task".to_owned();
                    let pp = "/tmp/proj".to_owned();
                    let mid = model_id.to_owned();
                    let restamped = restamped_clone.clone();
                    async move {
                        // Simulate the downstream restamp path: build a source
                        // ProviderConfig for the *previous* model (model-a) with
                        // its own model-dependent defaults, then restamp it to the
                        // *candidate* model (model-b).  This mirrors what
                        // `build_provider_from_resolved` does when a failover
                        // candidate is selected.
                        let source_config = djinn_provider::provider::ProviderConfig {
                            base_url: "https://api.example.com".to_owned(),
                            auth: djinn_provider::provider::AuthMethod::BearerToken(
                                "test-key".to_owned(),
                            ),
                            format_family: djinn_provider::provider::FormatFamily::Anthropic,
                            model_id: "provider/model-a".to_owned(),
                            context_window: 100_000,
                            capabilities: djinn_provider::provider::ProviderCapabilities {
                                streaming: true,
                                max_tokens_default: Some(8192),
                            },
                            reasoning_effort: None,
                            tool_schema_compat: None,
                            telemetry: None,
                            session_affinity_key: None,
                            provider_headers: std::collections::HashMap::new(),
                        };

                        // Restamp to model-b with different model-dependent
                        // defaults to prove the helper re-resolves them.
                        let target = djinn_provider::provider::RestampTarget {
                            model_id: mid.clone(),
                            format_family: djinn_provider::provider::FormatFamily::OpenAI,
                            reasoning: false,
                            context_window: 200_000,
                            capabilities: djinn_provider::provider::ProviderCapabilities {
                                streaming: true,
                                max_tokens_default: Some(32_768),
                            },
                            tool_schema_compat: None,
                        };
                        let cfg = djinn_provider::provider::restamp_provider_config_for_model(
                            source_config,
                            &target,
                        );

                        // Capture the restamped config for post-dispatch assertion
                        *restamped.lock().expect("restamp mutex") = Some(cfg.clone());

                        pool.dispatch(&tid, &pp, &mid).await
                    }
                },
            )
            .await;

        assert!(
            matches!(outcome, DispatchOutcome::Dispatched),
            "dispatch should succeed on model-b after model-a breaker-open"
        );

        // ── Assert restamped ProviderConfig has model-b's defaults ──────────
        let captured = restamped_config
            .lock()
            .expect("restamp mutex")
            .take()
            .expect("dispatch closure must capture restamped config");

        assert_eq!(
            captured.model_id, "provider/model-b",
            "restamped config must have the candidate model-b as model_id"
        );
        assert_eq!(
            captured.format_family,
            djinn_provider::provider::FormatFamily::OpenAI,
            "restamped config must resolve model-b's format_family (OpenAI), \
             not carry model-a's stale value (Anthropic)"
        );
        assert_eq!(
            captured.context_window, 200_000,
            "restamped config must resolve model-b's context_window (200_000), \
             not carry model-a's stale value (100_000)"
        );
        assert_eq!(
            captured.capabilities.max_tokens_default,
            Some(32_768),
            "restamped config must resolve model-b's max_tokens_default (32_768), \
             not carry model-a's stale value (8192)"
        );
        // Transport fields must be preserved from the source config
        assert_eq!(
            captured.base_url, "https://api.example.com",
            "restamped config must preserve the source base_url"
        );

        cancel.cancel();
    }

    /// Comprehensive AC2 + AC4 regression: failover-chain traversal with
    /// restamped ProviderConfig and per-candidate observation/logging.
    ///
    /// Scenario: 2 candidates — model-a (breaker-open, skipped) and model-b
    /// (dispatched). The dispatch closure calls `restamp_provider_config_for_model`
    /// for model-b, re-resolving model-dependent fields from model-a's source
    /// config to model-b's target values. The test asserts:
    ///
    /// 1. **Restamped config use (AC2)**: The restamped `ProviderConfig` has
    ///    model-b's `format_family`, `context_window`, and `max_tokens_default`
    ///    — not model-a's stale values — proving the restamp helper re-resolves
    ///    model-dependent defaults.
    /// 2. **Per-candidate observation/logging (AC4)**: `failover_candidate_attempt`
    ///    is emitted for model-a with `breaker_open` outcome, and
    ///    `failover_candidate_accepted` is emitted for model-b with
    ///    `skipped_count=1`. Both records include `total_candidates=2`.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn failover_chain_restamp_and_logging_comprehensive() {
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let (actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![("provider/model-a", 1), ("provider/model-b", 1)],
        );

        // Breaker-open model-a; model-b is free
        for _ in 0..3 {
            actor.health.record_failure(None, "provider/model-a");
        }

        // Capture restamped ProviderConfig for the dispatched candidate.
        let restamped_config: Arc<Mutex<Option<djinn_provider::provider::ProviderConfig>>> =
            Arc::new(Mutex::new(None));
        let restamped_clone = restamped_config.clone();

        let outcome = actor
            .try_dispatch_to_pool(
                "restamp-log-task",
                "worker",
                0,
                None,
                &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = "restamp-log-task".to_owned();
                    let pp = "/tmp/proj".to_owned();
                    let mid = model_id.to_owned();
                    let restamped = restamped_clone.clone();
                    async move {
                        // Simulate the downstream restamp path: build a source
                        // ProviderConfig for model-a with its own model-dependent
                        // defaults, then restamp it to the candidate model-b.
                        // This mirrors what `build_provider_from_resolved` does
                        // when a failover candidate is selected in production.
                        let source_config = djinn_provider::provider::ProviderConfig {
                            base_url: "https://api.example.com".to_owned(),
                            auth: djinn_provider::provider::AuthMethod::BearerToken(
                                "test-key".to_owned(),
                            ),
                            format_family: djinn_provider::provider::FormatFamily::Anthropic,
                            model_id: "provider/model-a".to_owned(),
                            context_window: 100_000,
                            capabilities: djinn_provider::provider::ProviderCapabilities {
                                streaming: true,
                                max_tokens_default: Some(8192),
                            },
                            reasoning_effort: None,
                            tool_schema_compat: None,
                            telemetry: None,
                            session_affinity_key: None,
                            provider_headers: std::collections::HashMap::new(),
                        };

                        // Restamp to model-b with different model-dependent
                        // defaults to prove the helper re-resolves them.
                        let target = djinn_provider::provider::RestampTarget {
                            model_id: mid.clone(),
                            format_family: djinn_provider::provider::FormatFamily::OpenAI,
                            reasoning: false,
                            context_window: 200_000,
                            capabilities: djinn_provider::provider::ProviderCapabilities {
                                streaming: true,
                                max_tokens_default: Some(32_768),
                            },
                            tool_schema_compat: None,
                        };
                        let cfg = djinn_provider::provider::restamp_provider_config_for_model(
                            source_config,
                            &target,
                        );

                        // Capture the restamped config for post-dispatch assertion
                        *restamped.lock().expect("restamp mutex") = Some(cfg.clone());

                        pool.dispatch(&tid, &pp, &mid).await
                    }
                },
            )
            .await;

        assert!(
            matches!(outcome, DispatchOutcome::Dispatched),
            "dispatch should succeed on model-b after model-a breaker-open"
        );

        // ── AC2: Assert restamped ProviderConfig has model-b's model-dependent
        // values, not model-a's stale values ──────────────────────────────────
        let captured = restamped_config
            .lock()
            .expect("restamp mutex")
            .take()
            .expect("dispatch closure must capture restamped config");

        assert_eq!(
            captured.model_id, "provider/model-b",
            "restamped config must have the candidate model-b as model_id"
        );
        assert_eq!(
            captured.format_family,
            djinn_provider::provider::FormatFamily::OpenAI,
            "restamped config must resolve model-b's format_family (OpenAI), \
             not carry model-a's stale value (Anthropic)"
        );
        assert_eq!(
            captured.context_window, 200_000,
            "restamped config must resolve model-b's context_window (200_000), \
             not carry model-a's stale value (100_000)"
        );
        assert_eq!(
            captured.capabilities.max_tokens_default,
            Some(32_768),
            "restamped config must resolve model-b's max_tokens_default (32_768), \
             not carry model-a's stale value (8192)"
        );
        // Transport fields must be preserved from the source config
        assert_eq!(
            captured.base_url, "https://api.example.com",
            "restamped config must preserve transport fields from the source config"
        );

        // ── AC4: Assert per-candidate observation/logging ─────────────────────
        // failover_candidate_attempt for the breaker-open model-a
        assert!(
            logs_contain("failover_candidate_attempt"),
            "must log failover_candidate_attempt for the breaker-open candidate"
        );
        assert!(
            logs_contain("breaker_open"),
            "failover_candidate_attempt must include breaker_open outcome for model-a"
        );
        assert!(
            logs_contain("total_candidates=2"),
            "failover_candidate_attempt must include total_candidates=2"
        );

        // failover_candidate_accepted for the winning model-b
        assert!(
            logs_contain("failover_candidate_accepted"),
            "must log failover_candidate_accepted for the winning candidate"
        );
        assert!(
            logs_contain("skipped_count=1"),
            "failover_candidate_accepted must report skipped_count=1 \
             (model-a was skipped before model-b was accepted)"
        );

        cancel.cancel();
    }

    /// AC3: First-candidate pool error followed by a successful fallback does
    /// NOT advance the failure streak, apply a dispatch cooldown, trip the
    /// circuit breaker for the failed candidate, or increment park/intervention
    /// counters.
    ///
    /// Scenario: 2 candidates — model-a fails with a pool error (SlotBusy),
    /// model-b dispatches successfully. The test asserts:
    /// 1. The outcome is `Dispatched` (fallback succeeded).
    /// 2. No failure streak was recorded for the task.
    /// 3. No dispatch cooldown was applied.
    /// 4. Per-candidate health failure was recorded for model-a (the failing
    ///    candidate) — failure counts are still incremented for diagnostics
    ///    and candidate health state.
    /// 5. The circuit breaker was NOT tripped because the chain was not
    ///    exhausted (AC2 deferral).
    /// 6. The chain-scoped observation list is NOT returned to the caller (the
    ///    fallback path discards it), so no breaker checks are applied for
    ///    earlier candidates. There is no global observation buffer that a
    ///    later exhaustion could later consume to trigger a breaker trip for
    ///    this fallback-rescued chain (AC2 chain-scoping fix).
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn failover_chain_first_candidate_failure_fallback_succeeds_no_terminal_effects() {
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let (actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![("provider/model-a", 1), ("provider/model-b", 1)],
        );

        // Track which models the dispatch_fn was called with
        let attempted_models: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let attempted_models_clone = attempted_models.clone();

        let outcome = actor
            .try_dispatch_to_pool(
                "fallback-task",
                "worker",
                0,
                None,
                &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = "fallback-task".to_owned();
                    let pp = "/tmp/proj".to_owned();
                    let mid = model_id.to_owned();
                    let tracker = attempted_models_clone.clone();
                    async move {
                        tracker.lock().unwrap().push(mid.clone());
                        if mid == "provider/model-a" {
                            // Simulate a pool error for model-a
                            Err(djinn_slot::PoolError::Slot(djinn_slot::SlotError::SlotBusy))
                        } else {
                            pool.dispatch(&tid, &pp, &mid).await
                        }
                    }
                },
            )
            .await;

        // ── AC3a: dispatch should succeed on model-b ─────────────────────
        assert!(
            matches!(outcome, DispatchOutcome::Dispatched),
            "dispatch should succeed on model-b after model-a pool error"
        );

        // ── AC3b: both models were attempted ─────────────────────────────
        let attempted = attempted_models.lock().unwrap().clone();
        assert!(
            attempted.contains(&"provider/model-a".to_string()),
            "dispatch_fn should have been called with model-a"
        );
        assert!(
            attempted.contains(&"provider/model-b".to_string()),
            "dispatch_fn should have been called with model-b (fallback)"
        );

        // ── AC3c: per-candidate health failure was recorded for model-a ──
        // After one pool error, model-a should have 1 consecutive failure.
        // The breaker threshold is 3, so it should still be available.
        assert!(
            actor.health.is_available(None, "provider/model-a"),
            "model-a should still be available after a single pool error (below breaker threshold)"
        );

        // ── AC3c2: circuit breaker was NOT tripped (AC2 deferral) ────────
        // The breaker must NOT be tripped for model-a even though it failed,
        // because the chain was not exhausted — a later candidate succeeded.
        // Breaker demotion is deferred to `apply_chain_exhaustion_side_effects`.
        let model_a_health = actor.health.model_health(None, "provider/model-a");
        assert!(
            !model_a_health.auto_disabled,
            "model-a breaker must NOT be tripped when chain was not exhausted \
             (breaker demotion deferred until chain exhaustion — AC2)"
        );

        // ── AC3d: no failure streak was recorded for the task ────────────
        assert!(
            !actor.dispatch_failure_streak.contains_key("fallback-task"),
            "no failure streak should be recorded when the chain was not exhausted"
        );

        // ── AC3e: no dispatch cooldown was applied ───────────────────────
        assert!(
            !actor.dispatch_cooldowns.contains_key("fallback-task"),
            "no dispatch cooldown should be applied when the chain was not exhausted"
        );

        // ── AC3f: terminal side effects were NOT applied ─────────────────
        // `apply_chain_exhaustion_side_effects` is called ONLY when
        // `try_dispatch_to_pool` returns `DispatchOutcome::Failed`.  Since
        // the outcome was `Dispatched`, the method was never invoked, so:
        //   - Session was NOT suspended (terminally_fail_task not called)
        //   - Task was NOT reopened / quality-struck
        //   - Park/intervention counters were NOT incremented
        //   - Dispatch failure streak was NOT advanced
        // These are verified above (AC3d, AC3e).  The negative log assertions
        // below confirm the terminal path was not entered:
        assert!(
            !logs_contain("all failover candidates exhausted"),
            "must NOT log chain-exhaustion message when fallback succeeded"
        );
        assert!(
            !logs_contain("circuit-breaker tripped after failover-chain exhaustion"),
            "must NOT log breaker trip when chain was not exhausted"
        );

        // Per-candidate observation and acceptance logs ARE emitted:
        assert!(
            logs_contain("failover_candidate_attempt"),
            "must log per-candidate attempt for the failed model-a"
        );
        assert!(
            logs_contain("failover_candidate_accepted"),
            "must log acceptance for the successful fallback model-b"
        );

        cancel.cancel();
    }

    /// AC4: Chain exhaustion applies the appropriate terminal side effects
    /// and diagnostics once no eligible fallback candidate remains.
    ///
    /// Scenario: 2 candidates — both fail with pool errors (SlotBusy).
    /// After `try_dispatch_to_pool` returns `Failed`, the caller invokes
    /// `apply_chain_exhaustion_side_effects` (the same call
    /// `dispatch_ready_tasks` makes on chain exhaustion). The test asserts:
    /// 1. The outcome is `Failed` (chain exhausted).
    /// 2. `apply_chain_exhaustion_side_effects` records a failure streak.
    /// 3. A dispatch cooldown is applied.
    /// 4. Per-candidate health failures were recorded for both candidates.
    /// 5. Repeated exhaustion escalates the streak.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn failover_chain_exhaustion_applies_terminal_side_effects() {
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let (mut actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![("provider/model-a", 1), ("provider/model-b", 1)],
        );

        // Build a minimal Task for the side-effect method (only `id` and
        // `short_id` are read by `apply_chain_exhaustion_side_effects`).
        let task = djinn_core::models::Task {
            id: "exhausted-task-uuid".to_owned(),
            project_id: String::new(),
            short_id: "exhausted-task".to_owned(),
            epic_id: None,
            title: String::new(),
            description: String::new(),
            design: String::new(),
            issue_type: "task".to_owned(),
            status: "open".to_owned(),
            priority: 0,
            owner: String::new(),
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
            created_by_user_id: None,
            ci_status: "unknown".to_owned(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".to_owned(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            ci_mq_state: None,
            ci_mq_run_id: None,
            ci_mq_head_sha: None,
            ci_mq_failed_check_names: None,
            ci_mq_failure_fingerprint: None,
            ci_mq_same_signature_count: None,
            ci_mq_first_seen_at: None,
            ci_mq_last_seen_at: None,
            unresolved_blocker_count: 0,
        };

        let attempted_models: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let attempted_models_clone = attempted_models.clone();

        let outcome = actor
            .try_dispatch_to_pool(
                &task.short_id,
                "worker",
                0,
                None,
                &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                |_pool, model_id| {
                    let mid = model_id.to_owned();
                    let tracker = attempted_models_clone.clone();
                    async move {
                        tracker.lock().unwrap().push(mid.clone());
                        // All candidates fail
                        Err(djinn_slot::PoolError::Slot(djinn_slot::SlotError::SlotBusy))
                    }
                },
            )
            .await;

        // ── AC4a: dispatch should fail after chain exhaustion ────────────
        assert!(
            matches!(outcome, DispatchOutcome::Failed { .. }),
            "dispatch should return Failed when all candidates fail"
        );

        // ── AC4b: both models were attempted ─────────────────────────────
        let attempted = attempted_models.lock().unwrap().clone();
        assert_eq!(
            attempted.len(),
            2,
            "dispatch_fn should have been called for both candidates"
        );
        assert!(attempted.contains(&"provider/model-a".to_string()));
        assert!(attempted.contains(&"provider/model-b".to_string()));

        // ── AC4c: per-candidate health failures were recorded ────────────
        // After one pool error each, both models should have 1 consecutive
        // failure. Breaker threshold is 3, so both should still be available.
        assert!(
            actor.health.is_available(None, "provider/model-a"),
            "model-a should still be available after a single pool error"
        );
        assert!(
            actor.health.is_available(None, "provider/model-b"),
            "model-b should still be available after a single pool error"
        );

        // ── AC4d: apply terminal side effects (as dispatch_ready_tasks does) ─
        let exhausted_observations = match &outcome {
            DispatchOutcome::Failed {
                exhausted_observations,
            } => exhausted_observations.clone(),
            _ => panic!("expected DispatchOutcome::Failed"),
        };
        actor
            .apply_chain_exhaustion_side_effects(&task, "worker", &exhausted_observations)
            .await;

        // failure streak should be 1
        assert_eq!(
            actor.dispatch_failure_streak.get("exhausted-task-uuid"),
            Some(&1),
            "failure streak should be 1 after first chain exhaustion"
        );

        // dispatch cooldown should be applied
        assert!(
            actor.dispatch_cooldowns.contains_key("exhausted-task-uuid"),
            "dispatch cooldown should be applied after chain exhaustion"
        );

        // ── AC4e: chain-exhaustion diagnostic was logged ─────────────────
        assert!(
            logs_contain("all failover candidates exhausted"),
            "must log chain-exhaustion message"
        );
        assert!(
            logs_contain("failover_candidate_attempt"),
            "must log per-candidate attempt for each failed candidate"
        );

        // ── AC4f: repeated exhaustion escalates streak and cooldown ──────
        // Simulate a second exhaustion: re-dispatch (all fail) then apply
        // side effects.  The streak should advance to 2.
        let outcome2 = actor
            .try_dispatch_to_pool(
                &task.short_id,
                "worker",
                0,
                None,
                &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                |_pool, _model_id| async {
                    Err(djinn_slot::PoolError::Slot(djinn_slot::SlotError::SlotBusy))
                },
            )
            .await;

        assert!(matches!(outcome2, DispatchOutcome::Failed { .. }));
        let exhausted_observations2 = match outcome2 {
            DispatchOutcome::Failed {
                exhausted_observations,
            } => exhausted_observations,
            _ => Vec::new(),
        };
        actor
            .apply_chain_exhaustion_side_effects(&task, "worker", &exhausted_observations2)
            .await;

        assert_eq!(
            actor.dispatch_failure_streak.get("exhausted-task-uuid"),
            Some(&2),
            "failure streak should be 2 after second chain exhaustion"
        );

        cancel.cancel();
    }

    /// AC2 regression: circuit breaker IS tripped after chain exhaustion when
    /// consecutive failure threshold is reached, and breaker demotion/cooldown
    /// was deferred until this point (not applied during per-candidate traversal).
    ///
    /// Scenario: 2 candidates fail, 3 chain exhaustions in a row.
    /// After the 3rd exhaustion (consecutive_failures reaches
    /// CIRCUIT_BREAKER_THRESHOLD = 3), `apply_chain_exhaustion_side_effects`
    /// trips the breaker for both candidates.  Before the 3rd exhaustion,
    /// the breaker should NOT be tripped even though failures were observed.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn breaker_tripped_after_chain_exhaustion_reaches_threshold() {
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let (mut actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![("provider/model-a", 1), ("provider/model-b", 1)],
        );

        let task = djinn_core::models::Task {
            id: "breaker-task-uuid".to_owned(),
            project_id: String::new(),
            short_id: "breaker-task".to_owned(),
            epic_id: None,
            title: String::new(),
            description: String::new(),
            design: String::new(),
            issue_type: "task".to_owned(),
            status: "open".to_owned(),
            priority: 0,
            owner: String::new(),
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
            created_by_user_id: None,
            ci_status: "unknown".to_owned(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".to_owned(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            ci_mq_state: None,
            ci_mq_run_id: None,
            ci_mq_head_sha: None,
            ci_mq_failed_check_names: None,
            ci_mq_failure_fingerprint: None,
            ci_mq_same_signature_count: None,
            ci_mq_first_seen_at: None,
            ci_mq_last_seen_at: None,
            unresolved_blocker_count: 0,
        };

        // Run 2 chain exhaustions — breaker threshold is 3, so breaker should
        // NOT be tripped yet even though failures are observed.
        for round in 1..=2 {
            let outcome = actor
                .try_dispatch_to_pool(
                    &task.short_id,
                    "worker",
                    0,
                    None,
                    &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                    |_pool, _model_id| async {
                        Err(djinn_slot::PoolError::Slot(djinn_slot::SlotError::SlotBusy))
                    },
                )
                .await;
            assert!(matches!(outcome, DispatchOutcome::Failed { .. }));

            // Breaker should still be available before side-effects are applied
            assert!(
                actor.health.is_available(None, "provider/model-a"),
                "model-a should be available before side-effects (round {round})"
            );

            let exhausted_observations = match outcome {
                DispatchOutcome::Failed {
                    exhausted_observations,
                } => exhausted_observations,
                _ => Vec::new(),
            };
            actor
                .apply_chain_exhaustion_side_effects(&task, "worker", &exhausted_observations)
                .await;

            // After round 2: consecutive_failures = 2 for each candidate.
            // Breaker threshold is 3, so NOT tripped yet.
            let model_a_health = actor.health.model_health(None, "provider/model-a");
            assert!(
                !model_a_health.auto_disabled,
                "model-a breaker must NOT be tripped after {round} exhaustions (threshold is 3)"
            );
            let model_b_health = actor.health.model_health(None, "provider/model-b");
            assert!(
                !model_b_health.auto_disabled,
                "model-b breaker must NOT be tripped after {round} exhaustions (threshold is 3)"
            );
        }

        // 3rd chain exhaustion: consecutive_failures reaches 3 → breaker trips.
        let outcome = actor
            .try_dispatch_to_pool(
                &task.short_id,
                "worker",
                0,
                None,
                &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                |_pool, _model_id| async {
                    Err(djinn_slot::PoolError::Slot(djinn_slot::SlotError::SlotBusy))
                },
            )
            .await;
        assert!(matches!(outcome, DispatchOutcome::Failed { .. }));

        // Before side-effects: breaker should still be available
        // (observation-only recording does not trip the breaker).
        assert!(
            actor.health.is_available(None, "provider/model-a"),
            "model-a must still be available before side-effects are applied (AC2 deferral)"
        );

        // Apply chain-exhaustion side effects → breaker trips for both.
        let exhausted_observations = match outcome {
            DispatchOutcome::Failed {
                exhausted_observations,
            } => exhausted_observations,
            _ => Vec::new(),
        };
        actor
            .apply_chain_exhaustion_side_effects(&task, "worker", &exhausted_observations)
            .await;

        // Breaker IS tripped for model-a after chain exhaustion.
        let model_a_health = actor.health.model_health(None, "provider/model-a");
        assert!(
            model_a_health.auto_disabled,
            "model-a breaker MUST be tripped after 3 chain exhaustions reach threshold"
        );

        // Breaker IS tripped for model-b after chain exhaustion.
        let model_b_health = actor.health.model_health(None, "provider/model-b");
        assert!(
            model_b_health.auto_disabled,
            "model-b breaker MUST be tripped after 3 chain exhaustions reach threshold"
        );

        cancel.cancel();
    }

    /// AC3 DB-backed regression: a first-candidate pool failure for a real
    /// task followed by a successful fallback must NOT suspend the session,
    /// reopen or quality-strike the task, or increment park / intervention
    /// counters persisted in the database.
    ///
    /// This locks the full end-to-end behavior of the failover-chain contract
    /// at the persisted-row level — not just in-memory dispatch state.  The
    /// earlier in-memory test (`failover_chain_first_candidate_failure_fallback_succeeds_no_terminal_effects`)
    /// verifies the dispatch-surface invariants; this one proves they hold
    /// when there's a real task row, a real running session row, and real
    /// counters to mis-increment.
    ///
    /// Scenario:
    /// - A task is created in the database with status=`open`, reopen_count=0,
    ///   intervention_count=0, total_reopen_count=0.
    /// - A "running" session is created against model-a (the failing candidate)
    ///   so we can prove it is preserved (not interrupted/suspended).
    /// - `try_dispatch_to_pool` traverses the failover chain with `[model-a,
    ///   model-b]`; model-a returns `Slot::SlotBusy`, model-b dispatches
    ///   successfully against the real pool.
    /// - We then verify:
    ///   1. The session row is still `running` (status unchanged — the
    ///      fallback was rescued without suspending the session).
    ///   2. The task's persisted status is still `open` (no terminal close).
    ///   3. `reopen_count`, `total_reopen_count`, `intervention_count` are
    ///      unchanged (no quality strike, no intervention, no reopen).
    ///   4. No `dispatch_state` row was written for the task (no
    ///      chain-exhaustion backoff was persisted).
    ///   5. No new session rows were created for the failing model-a (only
    ///      the test's pre-existing session is present).
    ///   6. `apply_chain_exhaustion_side_effects` was NOT entered (logs only).
    ///   7. The model-a breaker was NOT tripped.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[tracing_test::traced_test]
    async fn ac3_dbbacked_fallback_rescues_session_without_terminal_effects() {
        use djinn_core::events::EventBus;
        use djinn_db::{
            CreateSessionParams, DispatchStateRepository, EpicRepository, SessionRepository,
            TaskRepository,
        };

        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        // Create a real epic + task + running session for the failing candidate.
        let event_bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), event_bus.clone());
        let epic = epic_repo
            .create("ac3-epic", "", "", "", "", None)
            .await
            .expect("create epic");
        let task_repo = TaskRepository::new(db.clone(), event_bus.clone());
        let task = task_repo
            .create(&epic.id, "ac3-fallback-task", "", "", "task", 0, "", None)
            .await
            .expect("create task");
        let db = db; // shadow to keep worktree-readable

        // Verify initial task invariants before any dispatch happens.
        assert_eq!(task.status, "open", "task must start at `open` status");
        assert_eq!(task.reopen_count, 0, "task starts with reopen_count=0");
        assert_eq!(
            task.total_reopen_count, 0,
            "task starts with total_reopen_count=0"
        );
        assert_eq!(
            task.intervention_count, 0,
            "task starts with intervention_count=0"
        );

        // Create a running session against model-a (the failing candidate) so
        // we can verify it is preserved when fallback rescues the dispatch.
        let session_repo = SessionRepository::new(db.clone(), event_bus.clone());
        let session_a = session_repo
            .create(CreateSessionParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                model: "provider/model-a",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("create session for model-a");
        assert_eq!(
            session_a.status, "running",
            "seeded session must be in `running` status"
        );

        // Capture pre-dispatch snapshot of session list to verify no new
        // session for model-a is created when dispatch fails on it.
        let pre_session_ids: std::collections::HashSet<String> = session_repo
            .list_for_task(&task.id)
            .await
            .expect("list sessions for task")
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(
            pre_session_ids.len(),
            1,
            "test starts with exactly one session for the task"
        );

        // Capture pre-dispatch counters for post-dispatch comparison.
        let pre_task = task_repo
            .get(&task.id)
            .await
            .expect("get task")
            .expect("task should still exist");
        let pre_reopen_count = pre_task.reopen_count;
        let pre_total_reopen_count = pre_task.total_reopen_count;
        let pre_intervention_count = pre_task.intervention_count;
        let pre_status = pre_task.status.clone();

        // Track which models `dispatch_fn` was called with.
        let attempted_models: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let attempted_models_clone = attempted_models.clone();

        let (mut actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![("provider/model-a", 1), ("provider/model-b", 1)],
        );

        // Persist a tiny image + workspace fixture (some dispatch paths probe
        // readiness; we don't want to depend on global env vars).
        let _ = crate::test_helpers::create_test_project(&db).await;

        let outcome = actor
            .try_dispatch_to_pool(
                &task.short_id,
                "worker",
                0,
                Some(task.created_by_user_id.as_deref().unwrap_or("user-x")),
                &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = "ac3-fallback-task".to_owned();
                    let pp = "/tmp/proj".to_owned();
                    let mid = model_id.to_owned();
                    let tracker = attempted_models_clone.clone();
                    async move {
                        tracker.lock().unwrap().push(mid.clone());
                        if mid == "provider/model-a" {
                            Err(djinn_slot::PoolError::Slot(djinn_slot::SlotError::SlotBusy))
                        } else {
                            pool.dispatch(&tid, &pp, &mid).await
                        }
                    }
                },
            )
            .await;

        // ── Outcome: dispatch succeeded via fallback. ────────────────────────
        assert!(
            matches!(outcome, DispatchOutcome::Dispatched),
            "dispatch should succeed on model-b after model-a pool error; got {outcome:?}"
        );
        let attempted = attempted_models.lock().unwrap().clone();
        assert!(
            attempted.contains(&"provider/model-a".to_string()),
            "dispatch_fn should have been called with model-a (the failing candidate)"
        );
        assert!(
            attempted.contains(&"provider/model-b".to_string()),
            "dispatch_fn should have been called with model-b (the fallback)"
        );

        // ── AC3.1: session_a is NOT suspended/interrupted ───────────────────
        // The fallback-rescued session must stay `running`. If the terminal
        // side-effect path had leaked (breach of AC2/AC3), the session would
        // have been interrupted via `session_repo.interrupt_running_for_task`.
        let post_sessions = session_repo
            .list_for_task(&task.id)
            .await
            .expect("list sessions after dispatch");
        assert_eq!(
            post_sessions.len(),
            1,
            "no new session should be created on the failing model-a; \
             only the seeded session should remain (got {} sessions)",
            post_sessions.len()
        );
        let session_a_post = &post_sessions[0];
        assert_eq!(
            session_a_post.id, session_a.id,
            "the surviving session must be the one we pre-created"
        );
        assert_eq!(
            session_a_post.status, "running",
            "fallback-rescued session must NOT be suspended; \
             terminal side effects must only flow through \
             apply_chain_exhaustion_side_effects, which is called only on \
             chain exhaustion"
        );

        // ── AC3.2: task status is unchanged (still `open`, not closed) ──────
        let post_task = task_repo
            .get(&task.id)
            .await
            .expect("get task after dispatch")
            .expect("task should still exist after fallback");
        assert_eq!(
            post_task.status, pre_status,
            "task status must be unchanged after fallback-rescued dispatch; \
             was {pre_status:?}, became {:?}",
            post_task.status
        );
        assert_eq!(
            post_task.status, "open",
            "task must remain `open` (no terminal close) — terminal close is a \
             chain-exhaustion-only effect"
        );

        // ── AC3.3: counters unchanged (no quality strike, no reopen/...) ─────
        assert_eq!(
            post_task.reopen_count, pre_reopen_count,
            "task.reopen_count must not be bumped for a fallback-rescued \
             dispatch; was {pre_reopen_count}, became {}",
            post_task.reopen_count
        );
        assert_eq!(
            post_task.total_reopen_count, pre_total_reopen_count,
            "task.total_reopen_count must not be bumped for a fallback-rescued \
             dispatch; was {pre_total_reopen_count}, became {}",
            post_task.total_reopen_count
        );
        assert_eq!(
            post_task.intervention_count, pre_intervention_count,
            "task.intervention_count must NOT be bumped — fallback rescue must \
             not trigger a Planner intervention; was {pre_intervention_count}, \
             became {}",
            post_task.intervention_count
        );

        // ── AC3.4: no persisted dispatch_state for the task ─────────────────
        // `apply_chain_exhaustion_side_effects` writes `failure_streak` and
        // `cooldown_until` to `dispatch_state` on every chain exhaustion; a
        // successful fallback must NOT produce such a row (it stays NULL,
        // which is itself a meaningful state: no backoff was applied).
        let dispatch_repo = DispatchStateRepository::new(db.clone());
        let dispatch_state = dispatch_repo
            .get(&task.id)
            .await
            .expect("get dispatch_state");
        match dispatch_state {
            None => {
                // No row at all is the cleanest signal: chain-exhaustion
                // side effects never ran.
            }
            Some(state) => {
                assert_eq!(
                    state.failure_streak, 0,
                    "no failure streak should be persisted on a fallback-rescued \
                     dispatch; got failure_streak={}",
                    state.failure_streak
                );
                assert!(
                    state.cooldown_until.is_none(),
                    "no cooldown_until should be persisted on a fallback-rescued \
                     dispatch; got cooldown_until={:?}",
                    state.cooldown_until
                );
            }
        }

        // ── AC3.5: breaker was NOT tripped for the failing model-a ──────────
        // Pre-fix (R1) the breaker tripped inside `try_dispatch_to_pool`;
        // post-fix the breaker check is deferred to
        // `apply_chain_exhaustion_side_effects` and that path was never
        // invoked because the chain was rescued.
        let scope = Some(task.created_by_user_id.as_deref().unwrap_or("user-x"));
        let model_a_health = actor.health.model_health(scope, "provider/model-a");
        assert!(
            !model_a_health.auto_disabled,
            "model-a breaker must NOT be tripped when fallback rescued the \
             chain (AC2 deferral: breaker checks only on chain exhaustion)"
        );
        // The failure counter IS recorded for diagnostics — but the breaker
        // is still below the trip threshold.
        assert!(
            model_a_health.consecutive_failures >= 1,
            "model-a's failure counter must be incremented for diagnostics \
             (per-candidate observation), but the breaker must not trip \
             below the threshold (got {} consecutive failures)",
            model_a_health.consecutive_failures
        );

        // ── AC3.6: chain-exhaustion path was NOT entered ────────────────────
        assert!(
            !logs_contain("all failover candidates exhausted"),
            "must NOT log chain-exhaustion message when fallback succeeded"
        );
        assert!(
            !logs_contain("backing off dispatch"),
            "must NOT log dispatch-backoff when fallback succeeded"
        );

        // ── AC3.7: per-candidate observation WAS recorded ────────────────────
        assert!(
            logs_contain("failover_candidate_attempt"),
            "must log per-candidate attempt for the failed model-a"
        );
        assert!(
            logs_contain("failover_candidate_accepted"),
            "must log acceptance for the successful fallback model-b"
        );

        // ── AC3.8 (chain-scoping fix): a later, unrelated chain exhaustion
        // must NOT trigger a breaker trip for model-a.
        //
        // Pre-fix (R2), the pending observations were buffered globally on
        // `HealthTracker` and a later `apply_chain_exhaustion_side_effects`
        // would have consumed them — meaning model-a would be breaker-tripped
        // even though this dispatch's chain was rescued by fallback.  Post-fix,
        // the chain-scoped observations are returned only via
        // `DispatchOutcome::Failed { exhausted_observations }`; the successful
        // fallback path discards them; and there is no global buffer for a
        // later unrelated exhaustion to consume.
        let unrelated_outcome = actor
            .try_dispatch_to_pool(
                "unrelated-task",
                "worker",
                0,
                None,
                &["provider/model-z".to_owned()],
                |_pool, _model_id| async {
                    Err(djinn_slot::PoolError::Slot(djinn_slot::SlotError::SlotBusy))
                },
            )
            .await;
        let exhausted_unrelated = match unrelated_outcome {
            DispatchOutcome::Failed {
                exhausted_observations,
            } => exhausted_observations,
            _ => panic!(
                "expected unrelated chain to exhaust; got {:?}",
                unrelated_outcome
            ),
        };
        actor
            .apply_chain_exhaustion_side_effects(
                &task, // reuse the task fixture (only its id is read for streak)
                "worker",
                &exhausted_unrelated,
            )
            .await;

        // Model-a's state must still reflect ONLY the per-candidate
        // observation from the fallback-rescued chain (no breaker trip,
        // exactly 1 consecutive failure). A cross-chain leakage would
        // have reset model-a's consecutive_failures via this observation's
        // breaker check, or worse, tripped the breaker.
        let model_a_post = actor.health.model_health(scope, "provider/model-a");
        assert!(
            !model_a_post.auto_disabled,
            "model-a breaker must STILL not be tripped after an unrelated \
             exhaustion is processed (cross-chain observations must not \
             leak into this chain's breaker state)"
        );
        assert_eq!(
            model_a_post.consecutive_failures, 1,
            "model-a's consecutive_failures must remain at 1 (the \
             fallback-rescued chain's observation only); a cross-chain \
             leakage from an unrelated exhaustion would have incremented \
             this counter"
        );

        cancel.cancel();
    }

    /// AC2 reviewer-repro regression: a candidate model that is observed
    /// (recorded for diagnostics) during a *fallback-rescued* chain must NOT
    /// later contribute to the circuit-breaker trip. Only candidate failures
    /// from chains that actually exhaust are breaker-eligible.
    ///
    /// Scenario (the reviewer's repro):
    ///   * Model-a fails twice, each time rescued by a successful fallback
    ///     candidate. After these two non-terminal chains, model-a's
    ///     `consecutive_failures` is 2 but its breaker-eligible counter is 0.
    ///   * A third chain containing model-a *exhausts*. That exhaustion
    ///     advances the breaker-eligible counter by 1. Despite model-a's
    ///     overall diagnostic `consecutive_failures` now being 3, the breaker
    ///     MUST NOT trip — only one breaker-eligible exhausted-chain failure
    ///     has occurred (below the threshold of 3).
    ///   * Only repeated exhausted-chain failures reaching the configured
    ///     threshold trip the breaker (verifying the breaker still trips
    ///     when it should).
    ///
    /// This guards against a regression where the breaker eligibility counter
    /// is identical to (or accumulated into) the diagnostic `consecutive_failures`
    /// counter, which would let fallback-rescued observations leak into later
    /// breaker decisions.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn ac4_fallback_rescued_observations_do_not_advance_breaker_eligibility() {
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let (mut actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            // Single model per chain (model-a failing, plus a model-b fallback);
            // every exhaust chain in step 3 contains both candidates.
            vec![("provider/model-a", 1), ("provider/model-b", 1)],
        );

        // Build a minimal Task for `apply_chain_exhaustion_side_effects` (only
        // `id` and `short_id` are read for streak/persist paths).
        let task = djinn_core::models::Task {
            id: "breaker-elig-task-uuid".to_owned(),
            project_id: String::new(),
            short_id: "breaker-elig-task".to_owned(),
            epic_id: None,
            title: String::new(),
            description: String::new(),
            design: String::new(),
            issue_type: "task".to_owned(),
            status: "open".to_owned(),
            priority: 0,
            owner: String::new(),
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
            created_by_user_id: None,
            ci_status: "unknown".to_owned(),
            ci_head_sha: None,
            ci_pr_number: None,
            ci_blocking_required_check_names: "[]".to_owned(),
            ci_failure_fingerprint: None,
            ci_first_seen_at: None,
            ci_last_seen_at: None,
            ci_same_signature_count: 0,
            ci_last_remediation_base_sha: None,
            ci_mirror_head_sha: None,
            ci_github_head_sha: None,
            ci_heads_diverged: None,
            ci_head_observation_error: None,
            ci_mq_state: None,
            ci_mq_run_id: None,
            ci_mq_head_sha: None,
            ci_mq_failed_check_names: None,
            ci_mq_failure_fingerprint: None,
            ci_mq_same_signature_count: None,
            ci_mq_first_seen_at: None,
            ci_mq_last_seen_at: None,
            unresolved_blocker_count: 0,
        };

        // ── Step 1: Two model-a failures, each rescued by model-b ─────────
        // After two of these chains, model-a's `consecutive_failures` will be
        // 2, but breaker-eligible failures are STILL zero — the two
        // diagnostic observations must not contribute to breaker eligibility.
        for chain in 1..=2 {
            let outcome = actor
                .try_dispatch_to_pool(
                    &task.short_id,
                    "worker",
                    0,
                    None,
                    &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                    |pool, model_id| {
                        let pool = pool.clone();
                        let tid = format!("{}-{}", task.short_id, chain);
                        let pp = "/tmp/proj".to_owned();
                        let mid = model_id.to_owned();
                        async move {
                            if mid == "provider/model-a" {
                                // Failure observation: rescued by next candidate.
                                Err(djinn_slot::PoolError::Slot(djinn_slot::SlotError::SlotBusy))
                            } else {
                                pool.dispatch(&tid, &pp, &mid).await
                            }
                        }
                    },
                )
                .await;

            assert!(
                matches!(outcome, DispatchOutcome::Dispatched),
                "step 1 chain {chain}: fallback should rescue the dispatch; got {outcome:?}"
            );

            // Sanity check the diagnostic observation was recorded.
            let after_chain = actor.health.model_health(None, "provider/model-a");
            assert_eq!(
                after_chain.consecutive_failures, chain as u32,
                "step 1 chain {chain}: model-a's diagnostic counter should be {chain} \
                 (got {})",
                after_chain.consecutive_failures
            );
            assert_eq!(
                after_chain.breaker_eligible_consecutive_failures, 0,
                "step 1 chain {chain}: breaker-eligible counter MUST stay 0 — these \
                 observations are fallback-rescued and never advance breaker \
                 eligibility (AC2 reviewer repro). Got {}.",
                after_chain.breaker_eligible_consecutive_failures
            );
            assert!(
                actor.health.is_available(None, "provider/model-a"),
                "step 1 chain {chain}: model-a must remain available — failed was \
                 rescued by successful fallback, breaker should not have tripped"
            );
        }

        // ── Step 2: A third chain CONTAINING model-a exhausts ──────────────
        // This single exhaustion records one diagnostic observation AND
        // applies exactly one breaker-eligible failure check. After this
        // round, model-a's diagnostic `consecutive_failures` is 3 (from the
        // 2 fallback-rescued + 1 exhausted), but only one breaker-eligible
        // failure has occurred. The breaker MUST NOT trip — that requires
        // the configured threshold of breaker-eligible failures.
        let outcome = actor
            .try_dispatch_to_pool(
                &task.short_id,
                "worker",
                0,
                None,
                &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                |_pool, _model_id| async {
                    // Both candidates fail → chain exhausts.
                    Err(djinn_slot::PoolError::Slot(djinn_slot::SlotError::SlotBusy))
                },
            )
            .await;

        assert!(
            matches!(outcome, DispatchOutcome::Failed { .. }),
            "step 2: chain must exhaust when both candidates fail"
        );

        // Importantly: the breaker must NOT yet be tripped before side effects
        // are applied. Observations alone don't trip the breaker — that is
        // exclusively the job of `apply_chain_exhaustion_side_effects`.
        let pre_side_effects = actor.health.model_health(None, "provider/model-a");
        assert_eq!(
            pre_side_effects.consecutive_failures, 3,
            "step 2 pre-side-effects: diagnostic counter should be 3 \
             (2 fallback-rescued + 1 exhausted-chain)"
        );
        assert!(
            actor.health.is_available(None, "provider/model-a"),
            "step 2 pre-side-effects: model-a must still be available — observation alone \
             does NOT trip the breaker (that requires apply_breaker_check_for via chain \
             exhaustion)"
        );

        // Apply the chain-exhaustion side effects for this single exhaustion.
        let exhausted_observations = match outcome {
            DispatchOutcome::Failed {
                exhausted_observations,
            } => exhausted_observations,
            _ => unreachable!("asserted Failed above"),
        };
        actor
            .apply_chain_exhaustion_side_effects(&task, "worker", &exhausted_observations)
            .await;

        // ── Step 3: After ONE exhausted-chain failure, breaker MUST NOT trip
        let after_one_exhaustion = actor.health.model_health(None, "provider/model-a");
        assert_eq!(
            after_one_exhaustion.consecutive_failures, 3,
            "after one exhausted chain: diagnostic counter must equal 3 \
             (two fallback-rescued + one exhausted-chain observation)"
        );
        assert_eq!(
            after_one_exhaustion.breaker_eligible_consecutive_failures, 1,
            "after one exhausted chain: breaker-eligible counter must equal 1 — \
             only the single exhausted-chain failure advanced it, not the two \
             fallback-rescued observations"
        );
        assert!(
            actor.health.is_available(None, "provider/model-a"),
            "after one exhausted chain: model-a must STILL be available — the configured \
             breaker threshold requires multiple exhausted-chain failures, and only one \
             breaker-eligible failure has occurred (reviewer repro: the prior \
             2 fallback-rescued observations must NOT count). If this trips, the AC2 \
             breaker-eligibility separation regressed."
        );

        // ── Step 4: Repeated exhausted-chain failures reaching threshold trip
        // The breaker-eligible counter is now 1 (from step 2's single
        // exhausted chain). Drive the remaining `THRESHOLD - 1` exhausted
        // chains to reach the configured threshold, which trips the breaker
        // at the end of the next exhaustion. We hardcode the threshold to 3
        // (matching `CIRCUIT_BREAKER_THRESHOLD` in
        // `djinn-provider/src/catalog/health.rs`) to avoid leaking the
        // constant via `pub`; if the constant ever changes, this test
        // must be re-tuned, with the inline comment refreshed.
        const THRESHOLD: u32 = 3;
        let additional_needed = THRESHOLD - 1;
        for exhaustion_round in 1..=additional_needed {
            let outcome = actor
                .try_dispatch_to_pool(
                    &task.short_id,
                    "worker",
                    0,
                    None,
                    &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                    |_pool, _model_id| async {
                        Err(djinn_slot::PoolError::Slot(djinn_slot::SlotError::SlotBusy))
                    },
                )
                .await;

            assert!(
                matches!(outcome, DispatchOutcome::Failed { .. }),
                "exhausted-chain round {exhaustion_round}: must report exhaustion"
            );

            let exhausted_observations = match outcome {
                DispatchOutcome::Failed {
                    exhausted_observations,
                } => exhausted_observations,
                _ => unreachable!(),
            };
            actor
                .apply_chain_exhaustion_side_effects(&task, "worker", &exhausted_observations)
                .await;

            let running_total = 1 + exhaustion_round;
            let health = actor.health.model_health(None, "provider/model-a");
            assert_eq!(
                health.breaker_eligible_consecutive_failures, running_total,
                "after {running_total} total exhausted chains, breaker-eligible counter \
                 must equal {running_total} (got {})",
                health.breaker_eligible_consecutive_failures
            );
            // The breaker should NOT trip until we hit the threshold.
            if running_total < THRESHOLD {
                assert!(
                    actor.health.is_available(None, "provider/model-a"),
                    "after {running_total} exhausted chains (below threshold): \
                     model-a must still be available"
                );
            }
        }

        // We have now exhausted exactly THRESHOLD chains. The breaker must be
        // tripped from the LAST exhaustion's side effects (i.e. on the
        // exhaustion that pushes the counter to the threshold).
        let final_health = actor.health.model_health(None, "provider/model-a");
        assert!(
            final_health.auto_disabled,
            "model-a breaker MUST trip after THRESHOLD exhausted chains reach the \
             configured breaker threshold (verifying the breaker still trips when \
             it should — only exhausted-chain failures count toward eligibility)"
        );
        assert_eq!(
            final_health.breaker_eligible_consecutive_failures, THRESHOLD,
            "breaker-eligible counter must equal THRESHOLD ({THRESHOLD}) after exactly \
             THRESHOLD exhausted chains (got {})",
            final_health.breaker_eligible_consecutive_failures
        );

        cancel.cancel();
    }

    /// AC5 (kv6i): A dirty-work session that is rescued by a fallback
    /// candidate carries failover-aware context (previous model, new model,
    /// failover reason) on the resume lifecycle metadata the worker sees,
    /// AND the session/task are NOT suspended or quality-struck solely
    /// because the original provider/candidate failed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[tracing_test::traced_test]
    async fn ac5_dirty_work_fallback_rescue_threads_failover_context_without_terminal_effects() {
        use djinn_core::events::EventBus;
        use djinn_db::{
            DispatchStateRepository, EpicRepository, SessionRepository, TaskRepository,
        };

        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let event_bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), event_bus.clone());
        let epic = epic_repo
            .create("ac5-epic", "", "", "", "", None)
            .await
            .expect("create epic");
        let task_repo = TaskRepository::new(db.clone(), event_bus.clone());
        let task = task_repo
            .create(
                &epic.id,
                "ac5-dirty-fallback-task",
                "",
                "",
                "task",
                0,
                "",
                None,
            )
            .await
            .expect("create task");
        let _ = crate::test_helpers::create_test_project(&db).await;

        let preservation_payload = serde_json::json!({
            "message": "Coordinator preservation gate: dirty work preserved",
            "preservation_outcome": "succeeded",
            "preservation_trigger": "session_terminated_with_dirty_work",
            "preservation_reason": "termination_before_commit",
            "preservation_commit_sha": "abc123",
            "preservation_ref_name": "refs/djinn/checkpoints/ac5-dirty-fallback-task",
            "session_id": "session-prior",
            "last_durable_progress_summary": "Wrote the parser module",
        })
        .to_string();
        task_repo
            .log_activity(
                Some(&task.id),
                "coordinator",
                "system",
                "comment",
                &preservation_payload,
            )
            .await
            .expect("log preservation activity");

        let rotation_payload = serde_json::json!({
            "action": "rotated",
            "previous_model": "provider/model-a",
            "selected_model": "provider/model-b",
            "termination_cause": "NoProgress",
            "session_id": "session-prior",
        })
        .to_string();
        task_repo
            .log_activity(
                Some(&task.id),
                "agent",
                "system",
                "model_rotation",
                &rotation_payload,
            )
            .await
            .expect("log model_rotation activity");

        let (mut actor, cancel, _releases) = failover_actor(
            &db,
            &events_tx,
            vec![("provider/model-a", 1), ("provider/model-b", 1)],
        );
        actor.worker_lifecycle_config.resume.enabled = true;
        actor.worker_lifecycle_config.resume.prefer_checkpoint = true;

        let resume_meta = actor
            .select_resume_lifecycle_metadata_for_dispatch(&task)
            .await
            .expect(
                "resume selector must return metadata when resume is enabled and a \
                 preserved checkpoint exists",
            );

        assert!(
            resume_meta.considered,
            "resume metadata must be marked considered"
        );
        assert_eq!(
            resume_meta.commit_sha.as_deref(),
            Some("abc123"),
            "resume metadata must carry the preserved checkpoint commit SHA (amth contract)"
        );
        assert_eq!(
            resume_meta.previous_model.as_deref(),
            Some("provider/model-a"),
            "resume metadata must thread the previous (failed) model from model_rotation activity"
        );
        assert_eq!(
            resume_meta.new_model.as_deref(),
            Some("provider/model-b"),
            "resume metadata must thread the new (rescue) model from model_rotation activity"
        );
        assert_eq!(
            resume_meta.failover_reason.as_deref(),
            Some("no_durable_progress_streak"),
            "resume metadata must thread the failover reason mapped from the typed \
             ModelRotationReason::NoDurableProgressStreak enum (97f8 contract)"
        );
        assert_eq!(
            resume_meta.last_durable_progress_summary.as_deref(),
            Some("Wrote the parser module"),
            "resume metadata must thread the durable-progress summary from the \
             preservation activity so the fallback worker has context"
        );

        let wire = serde_json::to_value(&resume_meta).expect("serialize resume metadata");
        assert_eq!(
            wire["previous_model"],
            serde_json::json!("provider/model-a"),
            "previous_model must be a top-level typed field on the wire, not nested in extra"
        );
        assert_eq!(
            wire["new_model"],
            serde_json::json!("provider/model-b"),
            "new_model must be a top-level typed field on the wire, not nested in extra"
        );
        assert_eq!(
            wire["failover_reason"],
            serde_json::json!("no_durable_progress_streak"),
            "failover_reason must be a top-level typed field on the wire, not nested in extra"
        );

        let session_repo = SessionRepository::new(db.clone(), event_bus.clone());
        let session_a = session_repo
            .create(djinn_db::CreateSessionParams {
                project_id: &task.project_id,
                task_id: Some(&task.id),
                model: "provider/model-a",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("create session for model-a");
        assert_eq!(
            session_a.status, "running",
            "seeded session must be running"
        );

        let pre_reopen_count = task.reopen_count;
        let pre_total_reopen_count = task.total_reopen_count;
        let pre_intervention_count = task.intervention_count;
        let pre_status = task.status.clone();

        let attempted_models: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let attempted_models_clone = attempted_models.clone();
        let outcome = actor
            .try_dispatch_to_pool(
                &task.short_id,
                "worker",
                0,
                Some(task.created_by_user_id.as_deref().unwrap_or("user-x")),
                &["provider/model-a".to_owned(), "provider/model-b".to_owned()],
                |pool, model_id| {
                    let pool = pool.clone();
                    let tid = "ac5-dirty-fallback-task".to_owned();
                    let pp = "/tmp/proj".to_owned();
                    let mid = model_id.to_owned();
                    let tracker = attempted_models_clone.clone();
                    async move {
                        tracker.lock().unwrap().push(mid.clone());
                        if mid == "provider/model-a" {
                            Err(djinn_slot::PoolError::Slot(djinn_slot::SlotError::SlotBusy))
                        } else {
                            pool.dispatch(&tid, &pp, &mid).await
                        }
                    }
                },
            )
            .await;
        assert!(
            matches!(outcome, DispatchOutcome::Dispatched),
            "fallback should rescue the dispatch on model-b; got {outcome:?}"
        );

        let post_sessions = session_repo
            .list_for_task(&task.id)
            .await
            .expect("list sessions");
        assert_eq!(
            post_sessions.len(),
            1,
            "no new session should be created on the failing model-a"
        );
        let session_a_post = &post_sessions[0];
        assert_eq!(
            session_a_post.id, session_a.id,
            "the surviving session must be the one we pre-created"
        );
        assert_eq!(
            session_a_post.status, "running",
            "dirty-work fallback-rescued session must NOT be suspended"
        );

        let post_task = task_repo
            .get(&task.id)
            .await
            .expect("get task")
            .expect("task should still exist");
        assert_eq!(
            post_task.status, pre_status,
            "task status must be unchanged after dirty-work fallback rescue"
        );
        assert_eq!(post_task.status, "open", "task must remain `open`");
        assert_eq!(
            post_task.reopen_count, pre_reopen_count,
            "dirty-work fallback rescue must NOT bump reopen_count (no quality strike)"
        );
        assert_eq!(
            post_task.total_reopen_count, pre_total_reopen_count,
            "dirty-work fallback rescue must NOT bump total_reopen_count"
        );
        assert_eq!(
            post_task.intervention_count, pre_intervention_count,
            "dirty-work fallback rescue must NOT bump intervention_count"
        );

        let dispatch_repo = DispatchStateRepository::new(db.clone());
        match dispatch_repo
            .get(&task.id)
            .await
            .expect("get dispatch_state")
        {
            None => {}
            Some(state) => {
                assert_eq!(state.failure_streak, 0, "no failure streak persisted");
                assert!(
                    state.cooldown_until.is_none(),
                    "no cooldown_until persisted"
                );
            }
        }

        cancel.cancel();
    }

    /// AC6 (kv6i): When no prior session produced a model-rotation entry,
    /// the resume metadata degrades to the pre-`kv6i` shape (failover fields
    /// remain `None`) rather than carrying a fabricated reason.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[tracing_test::traced_test]
    async fn ac6_resume_metadata_without_rotation_activity_has_no_failover_context() {
        use djinn_core::events::EventBus;
        use djinn_db::{EpicRepository, TaskRepository};

        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let event_bus = EventBus::noop();
        let epic_repo = EpicRepository::new(db.clone(), event_bus.clone());
        let epic = epic_repo
            .create("ac6-epic", "", "", "", "", None)
            .await
            .expect("create epic");
        let task_repo = TaskRepository::new(db.clone(), event_bus.clone());
        let task = task_repo
            .create(&epic.id, "ac6-clean-task", "", "", "task", 0, "", None)
            .await
            .expect("create task");
        let _ = crate::test_helpers::create_test_project(&db).await;

        let preservation_payload = serde_json::json!({
            "preservation_outcome": "succeeded",
            "preservation_commit_sha": "clean-sha",
            "session_id": "session-clean",
        })
        .to_string();
        task_repo
            .log_activity(
                Some(&task.id),
                "coordinator",
                "system",
                "comment",
                &preservation_payload,
            )
            .await
            .expect("log preservation activity");

        let (mut actor, cancel, _releases) =
            failover_actor(&db, &events_tx, vec![("provider/model-a", 1)]);
        actor.worker_lifecycle_config.resume.enabled = true;

        let resume_meta = actor
            .select_resume_lifecycle_metadata_for_dispatch(&task)
            .await
            .expect("resume selector must return a clean-task-branch candidate");

        assert!(
            resume_meta.considered,
            "resume metadata must be considered when a preservation exists"
        );
        assert!(
            resume_meta.previous_model.is_none(),
            "previous_model must stay None when no model_rotation activity is recorded (no fabrication)"
        );
        assert!(
            resume_meta.new_model.is_none(),
            "new_model must stay None when no model_rotation activity is recorded"
        );
        assert!(
            resume_meta.failover_reason.is_none(),
            "failover_reason must stay None when no model_rotation activity is recorded"
        );
        assert_eq!(
            resume_meta.commit_sha.as_deref(),
            Some("clean-sha"),
            "preservation sha must still flow through to resume metadata"
        );

        cancel.cancel();
    }

    /// AC7 (kv6i): `map_model_rotation_reason` translates both snake_case
    /// serde forms and Debug-formatted strings (as emitted by
    /// `emit_rotation_event` in the agent) onto the typed
    /// [`crate::ModelRotationReason`] enum. Unknown strings map to `None`
    /// rather than persisting an invalid variant.
    #[test]
    fn ac7_map_model_rotation_reason_translates_known_and_unknown_forms() {
        use crate::ModelRotationReason as R;
        assert_eq!(
            map_model_rotation_reason("no_durable_progress_streak"),
            Some(R::NoDurableProgressStreak)
        );
        assert_eq!(
            map_model_rotation_reason("provider_health_degraded"),
            Some(R::ProviderHealthDegraded)
        );
        assert_eq!(
            map_model_rotation_reason("operator_requested"),
            Some(R::OperatorRequested)
        );
        assert_eq!(
            map_model_rotation_reason("NoProgress"),
            Some(R::NoDurableProgressStreak)
        );
        assert_eq!(
            map_model_rotation_reason("Flaky"),
            Some(R::RepeatedFlakyVerification)
        );
        assert_eq!(
            map_model_rotation_reason("Deadline"),
            Some(R::ContextBudgetPressure)
        );
        assert_eq!(
            map_model_rotation_reason("RepeatedVerifyLoop"),
            Some(R::RepeatedReadOnlyNoOp)
        );
        assert_eq!(
            map_model_rotation_reason("\"NoProgress\""),
            Some(R::NoDurableProgressStreak)
        );
        assert_eq!(map_model_rotation_reason("completely-unknown-reason"), None);
        assert_eq!(map_model_rotation_reason(""), None);
    }
}

/// mshn (AC2): No-eligible-worker-model parking for monitored reopen.
///
/// When an arbiter's `exclude_models` eliminates all worker models for a
/// monitored-reopen worker dispatch, the coordinator parks the task with a
/// `monitored_reopen_no_eligible_model` dossier and completes the monitored
/// attempt so re-entry cannot trigger a second cycle.  These tests exercise
/// the code path at lines 2104-2148 via the real `dispatch_ready_tasks`
/// dispatch loop.
#[cfg(test)]
mod monitored_reopen_no_eligible_model_tests {
    use super::*;
    use djinn_db::repositories::task_arbitration::{
        CreateArbitrationParams, TaskArbitrationRepository,
    };
    use std::collections::HashMap;

    /// Build a CoordinatorActor with a pool configured for `test/mock`
    /// (the DEFAULT_MODEL_ID in test builds).  Mirrors the `failover_actor`
    /// helper but returns a bare actor without the release map since the
    /// no-eligible-model path parks before reaching the pool.
    fn park_test_actor(
        db: &djinn_db::Database,
        events_tx: &tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
    ) -> (CoordinatorActor, tokio_util::sync::CancellationToken) {
        use djinn_slot::{ModelSlotConfig, SlotPoolConfig};

        let cancel = tokio_util::sync::CancellationToken::new();

        let pool_config = SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: DEFAULT_MODEL_ID.to_owned(),
                max_slots: 1,
                roles: ["worker".to_owned()].into_iter().collect(),
            }],
            role_priorities: HashMap::new(),
        };

        let app_state = crate::test_helpers::agent_context_from_db(db.clone(), cancel.clone());
        let pool = djinn_slot::SlotPoolHandle::spawn_with_factory(
            app_state,
            cancel.clone(),
            pool_config,
            // Controlled slot runner: if the no-eligible-model park does NOT
            // fire (a bug), the slot runner blocks until killed so the pool
            // dispatch is accepted but no real work happens.
            std::sync::Arc::new(|_slot_id, _model_id, _event_tx, _app_state, kill| {
                let runner: djinn_slot::TestLifecycleRunner = std::sync::Arc::new(
                    move |_task_id, _project_path, _model_id, _app_state, kill, _pause, _resume| {
                        Box::pin(async move {
                            let _ = kill.cancelled().await;
                            Ok(())
                        })
                    },
                );
                djinn_slot::SlotHandle::spawn_with_test_runner(
                    0,
                    DEFAULT_MODEL_ID.to_owned(),
                    _event_tx,
                    _app_state,
                    kill,
                    runner,
                )
            }),
        );

        let (status_tx, _status_rx) = tokio::sync::watch::channel(SharedCoordinatorState {
            dispatched: 0,
            recovered: 0,
            epic_throughput: HashMap::new(),
            pr_errors: HashMap::new(),
            rate_limited_until: None,
        });

        let actor = CoordinatorActor {
            receiver: tokio::sync::mpsc::channel(1).1,
            events: events_tx.subscribe(),
            cancel: cancel.clone(),
            tick: tokio::time::interval(std::time::Duration::from_secs(60)),
            db: db.clone(),
            events_tx: events_tx.clone(),
            pool,
            catalog: CatalogService::new(),
            health: djinn_provider::catalog::health::HealthTracker::new(),
            role_registry: std::sync::Arc::new(crate::roles::RoleRegistry::new()),
            lsp: djinn_lsp::LspManager::new(),
            self_sender: tokio::sync::mpsc::channel(1).0,
            status_tx,
            dispatch_limit: 50,
            model_priorities: HashMap::new(),
            #[cfg(test)]
            test_use_live_credential_resolution: false,
            pr_errors: HashMap::new(),
            last_dispatched: HashMap::new(),
            inflight_dispatches: HashMap::new(),
            provisional_admissions: HashMap::new(),
            dispatch_cooldowns: HashMap::new(),
            dispatch_failure_streak: HashMap::new(),
            background_work_tracker: BackgroundWorkTracker::default(),
            stranded_ready_source: None,
            closed_parent_open_children_source: None,
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
            pr_status_cache: HashMap::new(),
            pr_draft_first_seen: HashMap::new(),
            review_stuck_sha_first_seen: HashMap::new(),
            merge_fail_count: HashMap::new(),
            auto_approve_attempted: HashMap::new(),
            delegated_to_github: HashMap::new(),
            conversations_resolved: HashMap::new(),
            handled_dequeues: HashMap::new(),
            stall_killed: std::collections::HashSet::new(),
            stall_progress_watermark: HashMap::new(),
            stall_cancel_streak: HashMap::new(),
            stall_extension_count: HashMap::new(),
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
        };

        (actor, cancel)
    }

    /// Seed an open worker task in the DB and return its id.
    async fn seed_open_worker_task(db: &djinn_db::Database, project_id: &str) -> String {
        let task_repo =
            djinn_db::TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let task = task_repo
            .create_in_project(
                project_id,
                None,
                "No-eligible-model test",
                "",
                "",
                "task",
                0,
                "",
                Some("open"),
                None,
            )
            .await
            .expect("seed task");
        task.id
    }

    /// Seed a monitored-reopen arbitration row with the given `excluded_models`.
    /// Sets `monitored_reopen_count = 1` and `directive_injected = false` so the
    /// monitored-reopen exclude path triggers on the next worker dispatch.
    async fn seed_monitored_reopen_arbitration(
        db: &djinn_db::Database,
        task_id: &str,
        excluded_models: &serde_json::Value,
    ) {
        let repo = TaskArbitrationRepository::new(db.clone());
        repo.try_create(CreateArbitrationParams {
            task_id,
            hold_cycle: 1,
            deadline_at: Some("2026-12-31T23:59:59.000Z"),
            mirror_head_sha: Some("abc123"),
            github_head_sha: Some("def456"),
            pr_url: Some("https://github.com/test/repo/pull/1"),
            failing_ci_job_ids: &serde_json::json!([]),
            dossier: None,
            directive: Some(&serde_json::json!({"decision": "reopen", "directive": "fix the bug"})),
            verification_command: Some("cargo test"),
            excluded_models,
        })
        .await
        .expect("create arbitration row");
        // Record the monitored reopen attempt start (sets monitored_reopen_count = 1).
        repo.record_monitored_reopen(task_id, 1)
            .await
            .expect("record monitored reopen");
    }

    /// AC2: When an arbiter's `exclude_models` eliminates all worker models
    /// for a monitored-reopen dispatch, the coordinator must:
    /// 1. Park the source task with a human-review hold.
    /// 2. Complete the monitored reopen attempt (arbitration row → consumed).
    /// 3. Emit a dossier with `kind = monitored_reopen_no_eligible_model`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn monitored_reopen_parks_when_exclude_models_eliminates_all_workers() {
        let _ = djinn_telemetry::init();
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        // Create a project with readiness satisfied.
        let project = crate::test_helpers::create_test_project(&db).await;

        // Seed an open worker task.
        let task_id = seed_open_worker_task(&db, &project.id).await;

        // Seed a monitored-reopen arbitration row where `excluded_models`
        // contains DEFAULT_MODEL_ID ("test/mock") — the only model the
        // test-build resolve_dispatch_models_for_role returns for "worker".
        let excluded = serde_json::json!([DEFAULT_MODEL_ID]);
        seed_monitored_reopen_arbitration(&db, &task_id, &excluded).await;

        let (mut actor, cancel) = park_test_actor(&db, &events_tx);

        // Run a dispatch pass — the monitored-reopen exclude path should
        // eliminate the only model, triggering the no-eligible-model park.
        actor.dispatch_ready_tasks(Some(&project.id)).await;

        // Assert: the arbitration row was completed (consumed).
        let arb_repo = TaskArbitrationRepository::new(db.clone());
        let (_, record) = arb_repo
            .resolve_current_hold_cycle(&task_id)
            .await
            .expect("resolve hold cycle");
        assert!(
            record.is_none(),
            "arbitration row must be consumed (terminal) after no-eligible-model park, \
             got: {record:?}"
        );

        // Assert: the dossier on the arbitration row was updated with the
        // `monitored_reopen_no_eligible_model` kind.
        let raw_row = arb_repo
            .get_by_task_and_cycle(&task_id, 1)
            .await
            .expect("get arbitration row")
            .expect("arbitration row must exist");
        let dossier = raw_row.dossier.unwrap_or_else(|| serde_json::json!({}));
        assert_eq!(
            dossier["kind"], "monitored_reopen_no_eligible_model",
            "dossier kind must be 'monitored_reopen_no_eligible_model', got: {dossier}"
        );
        assert_eq!(
            dossier["excluded_models"], excluded,
            "dossier must carry the excluded models, got: {dossier}"
        );

        // Assert: the task was parked — it now has an unresolved blocker
        // (the human-review-hold remediation task).
        let task_repo =
            djinn_db::TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let blockers = task_repo
            .list_blockers(&task_id)
            .await
            .expect("list blockers");
        assert!(
            blockers.iter().any(|b| b.status != "closed"),
            "task must have an unresolved human-review-hold blocker after park, got: {blockers:?}"
        );

        cancel.cancel();
    }

    /// AC2 (negative): When the arbiter's `exclude_models` does NOT eliminate
    /// all models (e.g. only excludes a model not in the worker pool), the
    /// task must dispatch normally and NOT park.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn monitored_reopen_dispatches_when_exclude_models_leaves_eligible_model() {
        let _ = djinn_telemetry::init();
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let project = crate::test_helpers::create_test_project(&db).await;
        let task_id = seed_open_worker_task(&db, &project.id).await;

        // Exclude a model NOT in the pool — DEFAULT_MODEL_ID remains eligible.
        let excluded = serde_json::json!(["nonexistent/model"]);
        seed_monitored_reopen_arbitration(&db, &task_id, &excluded).await;

        let (mut actor, cancel) = park_test_actor(&db, &events_tx);

        // The dispatch should proceed normally (not park).
        actor.dispatch_ready_tasks(Some(&project.id)).await;

        // The arbitration row must NOT be consumed — the task dispatched
        // to a worker, so the monitored reopen is still in progress.
        let arb_repo = TaskArbitrationRepository::new(db.clone());
        let (_, record) = arb_repo
            .resolve_current_hold_cycle(&task_id)
            .await
            .expect("resolve hold cycle");
        assert!(
            record.is_some(),
            "arbitration row must remain unconsumed when an eligible model remains — \
             the no-eligible-model park must NOT fire"
        );

        // No human-review-hold blocker should exist.
        let task_repo =
            djinn_db::TaskRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let blockers = task_repo
            .list_blockers(&task_id)
            .await
            .expect("list blockers");
        assert!(
            blockers.iter().all(|b| b.status == "closed"),
            "task must NOT have an unresolved human-review-hold blocker when a model remains eligible, \
             got: {blockers:?}"
        );

        cancel.cancel();
    }

    /// AC2: When there is NO monitored reopen (monitored_reopen_count == 0),
    /// the no-eligible-model park must NOT fire even if models are empty.
    /// The empty-model path must fall through to the general no-model handling.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_eligible_model_does_not_park_without_monitored_reopen() {
        let _ = djinn_telemetry::init();
        let db = crate::test_helpers::create_test_db();
        let (events_tx, _) = tokio::sync::broadcast::channel(64);

        let project = crate::test_helpers::create_test_project(&db).await;
        let task_id = seed_open_worker_task(&db, &project.id).await;

        // Create an arbitration row but do NOT call record_monitored_reopen —
        // monitored_reopen_count stays at 0.
        let repo = TaskArbitrationRepository::new(db.clone());
        repo.try_create(CreateArbitrationParams {
            task_id: &task_id,
            hold_cycle: 1,
            deadline_at: Some("2026-12-31T23:59:59.000Z"),
            mirror_head_sha: Some("abc123"),
            github_head_sha: Some("def456"),
            pr_url: Some("https://github.com/test/repo/pull/1"),
            failing_ci_job_ids: &serde_json::json!([]),
            dossier: None,
            directive: None,
            verification_command: None,
            excluded_models: &serde_json::json!([DEFAULT_MODEL_ID]),
        })
        .await
        .expect("create arbitration row");

        let (mut actor, cancel) = park_test_actor(&db, &events_tx);
        actor.dispatch_ready_tasks(Some(&project.id)).await;

        // The no-eligible-model park must NOT have fired — the arbitration row
        // must remain unconsumed.
        let (_, record) = repo
            .resolve_current_hold_cycle(&task_id)
            .await
            .expect("resolve hold cycle");
        assert!(
            record.is_some(),
            "arbitration row must remain unconsumed — no-eligible-model park \
             must NOT fire without a monitored reopen"
        );

        cancel.cancel();
    }
}

/// Dispatch-time throttle-deprioritization tests for
/// [`deprioritize_throttle_cooling`].
#[cfg(test)]
mod throttle_deprioritization_tests {
    use super::deprioritize_throttle_cooling;
    use djinn_provider::catalog::health::HealthTracker;

    const SCOPE: Option<&str> = Some("user-a");

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cooling_model_is_moved_behind_healthy_lanemate() {
        let health = HealthTracker::new();
        // model-a is throttle-cooling; model-b is healthy.
        health.record_stall(SCOPE, "model-a", false);
        assert!(health.is_throttle_cooling(SCOPE, "model-a"));

        let mut model_ids = ids(&["model-a", "model-b"]);
        deprioritize_throttle_cooling(&health, SCOPE, &mut model_ids);
        assert_eq!(
            model_ids,
            ids(&["model-b", "model-a"]),
            "a throttle-cooling head-of-line model is moved behind a healthy lane-mate"
        );
    }

    #[test]
    fn healthy_order_is_preserved_when_nothing_is_cooling() {
        let health = HealthTracker::new();
        let mut model_ids = ids(&["model-a", "model-b", "model-c"]);
        deprioritize_throttle_cooling(&health, SCOPE, &mut model_ids);
        assert_eq!(
            model_ids,
            ids(&["model-a", "model-b", "model-c"]),
            "with no cooling model the priority order is untouched"
        );
    }

    #[test]
    fn relative_order_within_each_group_is_stable() {
        let health = HealthTracker::new();
        // a and c cooling; b and d healthy. Expect [b, d, a, c].
        health.record_stall(SCOPE, "model-a", false);
        health.record_stall(SCOPE, "model-c", false);
        let mut model_ids = ids(&["model-a", "model-b", "model-c", "model-d"]);
        deprioritize_throttle_cooling(&health, SCOPE, &mut model_ids);
        assert_eq!(
            model_ids,
            ids(&["model-b", "model-d", "model-a", "model-c"])
        );
    }

    #[test]
    fn only_candidate_that_is_cooling_is_left_in_place() {
        let health = HealthTracker::new();
        health.record_stall(SCOPE, "model-a", false);
        // A lone cooling candidate is NOT dropped — it stays as the last-resort
        // candidate so the dispatch loop can still attempt it (half-open) or
        // defer via the existing all-unavailable path.
        let mut model_ids = ids(&["model-a"]);
        deprioritize_throttle_cooling(&health, SCOPE, &mut model_ids);
        assert_eq!(model_ids, ids(&["model-a"]));
    }

    #[test]
    fn all_cooling_keeps_all_candidates() {
        let health = HealthTracker::new();
        health.record_stall(SCOPE, "model-a", false);
        health.record_stall(SCOPE, "model-b", false);
        let mut model_ids = ids(&["model-a", "model-b"]);
        deprioritize_throttle_cooling(&health, SCOPE, &mut model_ids);
        // No candidate is lost; order among the cooling group is preserved.
        assert_eq!(model_ids, ids(&["model-a", "model-b"]));
    }
}
