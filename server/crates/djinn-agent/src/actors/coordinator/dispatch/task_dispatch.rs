// djinn:allow-oversize — legacy dispatch module over size-guard threshold; split when touched substantively.
use super::super::*;
use super::DispatchOutcome;
use crate::dispatch_pause::{load_dispatch_pause_state, matching_task_dispatch_pause};
use crate::roles::DispatchContext;
use djinn_db::{DispatchStateRepository, DispatchStateUpsert};

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

/// Overlay the in-flight dispatch ledger onto the DB-seeded per-user running
/// counts, taking `max(db, ledger)` per `(creator, model)`.
///
/// The DB seed counts only sessions that reached `running`, which lags a fresh
/// dispatch by the worker pod's boot time (20-60s). The ledger holds dispatches
/// that have not yet produced a `running` row, so overlaying it makes those
/// count against the per-user cap immediately and prevents re-firing passes from
/// overshooting it. `max` (not sum) is deliberate: a task present in BOTH the
/// running rows and the ledger must count once, not twice.
fn overlay_inflight_ledger(
    running_by_user_model: &mut HashMap<(String, String), u32>,
    inflight_dispatches: &HashMap<String, (Option<String>, String)>,
) {
    let mut ledger_counts: HashMap<(String, String), u32> = HashMap::new();
    for (creator, model) in inflight_dispatches.values() {
        if let Some(c) = creator {
            *ledger_counts.entry((c.clone(), model.clone())).or_insert(0) += 1;
        }
    }
    for (key, lcount) in ledger_counts {
        let entry = running_by_user_model.entry(key).or_insert(0);
        *entry = (*entry).max(lcount);
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

    pub(in crate::actors::coordinator) async fn clear_durable_dispatch_backoff_state(
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
                ..Default::default()
            },
        )
        .await;
    }

    pub(in crate::actors::coordinator) async fn clear_planned_dispatch_completion(
        &mut self,
        task_id: &str,
        reason: &str,
    ) {
        // Planned lifecycle completions (including budget parks and ignored
        // wind-down parks) are successful settlements, not same-role dispatch
        // failures. Drop any stale recovery/backoff attribution before the next
        // continuation dispatch so they cannot advance Trigger-B or terminal
        // close accounting during recovery/refactor paths.
        self.dispatch_failure_streak.remove(task_id);
        self.dispatch_cooldowns.remove(task_id);
        self.last_dispatched.remove(task_id);
        self.clear_durable_dispatch_backoff_state(task_id, None, reason)
            .await;
    }

    pub(in crate::actors::coordinator) async fn increment_durable_escalation_count(
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

    pub(in crate::actors::coordinator) async fn try_dispatch_to_pool<F, Fut>(
        &self,
        label: &str,
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
            if !self.health.is_available(scope, model_id) {
                tracing::debug!(
                    model_id = %model_id,
                    scope = ?scope,
                    label,
                    "CoordinatorActor: model unavailable by health tracker"
                );
                continue;
            }

            match dispatch_fn(&self.pool, model_id).await {
                Ok(()) => return DispatchOutcome::Dispatched,
                Err(PoolError::AtCapacity { .. }) => {
                    any_at_capacity = true;
                    tracing::debug!(
                        model_id = %model_id,
                        label,
                        "CoordinatorActor: model at capacity, trying next model"
                    );
                }
                Err(PoolError::ActorDead) => {
                    tracing::error!("CoordinatorActor: slot pool actor dead, aborting dispatch");
                    return DispatchOutcome::PoolDead;
                }
                Err(e) => {
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
            DispatchOutcome::AtCapacity
        } else {
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
    /// Returns `true` when the task has no resolved owner (`created_by_user_id`
    /// is NULL) or is still attributed to the retired automation sentinel
    /// (a user row with `github_id == AUTOMATION_SENTINEL_GITHUB_ID`, i.e. `0`).
    /// Such tasks must NOT consume org-shared credentials under no identity; the
    /// caller parks them and emits a loud warning so the ownership regression is
    /// visible instead of silently running ownerless.
    ///
    /// Always `false` under `#[cfg(test)]`: the in-process test suite dispatches
    /// fixtures with no real users seeded, and the production identity invariant
    /// is exercised by the live MCP/session path, not these unit fixtures.
    #[cfg(not(test))]
    async fn task_is_ownerless(&self, task: &djinn_core::models::Task) -> bool {
        /// Legacy automation service-user marker. The automation user is retired
        /// (proposal 1omc); any task still pointing at a `github_id == 0` row is
        /// treated as ownerless.
        const AUTOMATION_SENTINEL_GITHUB_ID: i64 = 0;
        let Some(uid) = task.created_by_user_id.as_deref() else {
            return true;
        };
        match djinn_db::UserRepository::new(self.db.clone())
            .get_by_id(uid)
            .await
        {
            // Creator resolves to the retired automation sentinel → ownerless.
            Ok(Some(user)) => user.github_id == AUTOMATION_SENTINEL_GITHUB_ID,
            // Creator id present but no matching user row (dangling reference) →
            // ownerless; refuse rather than dispatch under a ghost identity.
            Ok(None) => true,
            // DB error resolving the creator: fail-closed (refuse) so a transient
            // lookup failure can't slip an unverified owner past the guard.
            Err(e) => {
                tracing::warn!(
                    task_id = %task.short_id,
                    created_by_user_id = uid,
                    error = %e,
                    "CoordinatorActor: ownership guard — failed to resolve task creator; treating as ownerless"
                );
                true
            }
        }
    }

    #[cfg(test)]
    async fn task_is_ownerless(&self, _task: &djinn_core::models::Task) -> bool {
        false
    }

    /// Find all ready tasks (open, no unresolved blockers, non-epic) and dispatch
    /// those that don't already have an active session.
    pub(in crate::actors::coordinator) async fn dispatch_ready_tasks(
        &mut self,
        project_filter: Option<&str>,
    ) {
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
        let prune_now = StdInstant::now();
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
        overlay_inflight_ledger(&mut running_by_user_model, &self.inflight_dispatches);

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
            // Defensive guard: NEVER dispatch an agent for a task in a host-owned
            // transient status. `verifying` in particular is driven by the
            // slot-free verification pipeline on the host (spawned after the
            // worker stage submits) — NOT by an agent run. It is already excluded
            // from the ready set (`list_ready` returns `open` only, and the review
            // sweep lists only needs_task_review / needs_lead_intervention), but
            // `flow_for_task_dispatch` / `role_for_task_dispatch` would route a
            // `verifying` task to the worker (NewTask) if one ever leaked in — so
            // skip it explicitly. A stuck `verifying` task (no live pipeline) is
            // recovered by `detect_and_recover_stuck_filtered`, not re-dispatched.
            if task.status == "verifying" {
                tracing::debug!(
                    task_id = %task.short_id,
                    "CoordinatorActor: skipping dispatch for verifying task (host-owned verification pipeline)"
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
            // Proposal 1omc: every dispatch must run under a real user. Refuse to
            // dispatch a task with no resolved owner (or one still attributed to
            // the retired automation sentinel, github_id 0). Park it loudly rather
            // than silently consuming org-shared credentials under no identity —
            // this surfaces an ownership regression instead of running ownerless.
            if self.task_is_ownerless(&task).await {
                tracing::warn!(
                    task_id = %task.short_id,
                    task_uuid = %task.id,
                    project_id = %task.project_id,
                    created_by_user_id = ?task.created_by_user_id,
                    "CoordinatorActor: REFUSING dispatch — task has no real owner \
                     (created_by_user_id is NULL or the retired automation sentinel). \
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
                            self.dispatch_cooldowns.remove(&task.id);
                            self.clear_durable_dispatch_backoff_state(
                                &task.id,
                                Some(&task.short_id),
                                "cycling_planner_intervention_handoff_clear",
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
                            self.dispatch_cooldowns.remove(&task.id);
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
                        self.dispatch_cooldowns
                            .insert(task.id.clone(), StdInstant::now() + effective_cooldown);
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
            let user_model_ids = self.resolve_user_model_priority(creator.as_deref()).await;
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
                         the owner (or the automation user) and reopen.",
                    )
                    .await;
                    self.dispatch_failure_streak.remove(&task.id);
                    self.dispatch_cooldowns.remove(&task.id);
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
                        .insert(task.id.clone(), StdInstant::now() + cooldown);
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
                    let used = running_by_user_model
                        .get(&(c.to_string(), m.clone()))
                        .copied()
                        .unwrap_or(0);
                    used < caps.get(m).copied().unwrap_or(1)
                });
                if model_ids.is_empty() {
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

            let task_id = task.id.clone();
            let project_path_owned = project_path.clone();
            let outcome = self
                .try_dispatch_to_pool(
                    &task.short_id,
                    creator.as_deref(),
                    model_ids,
                    |pool, model_id| {
                        let pool = pool.clone();
                        let tid = task_id.clone();
                        let pp = project_path_owned.clone();
                        let mid = model_id.to_owned();
                        async move { pool.dispatch(&tid, &pp, &mid).await }
                    },
                )
                .await;

            match outcome {
                DispatchOutcome::Dispatched => {
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
                            instant: StdInstant::now(),
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
                        // Record in the in-flight ledger so the NEXT pass counts
                        // this dispatch against the cap immediately — before its
                        // `running` session row lands (pod boot lags 20-60s).
                        // Reconciled against the live pool at the top of each
                        // pass, so it drops out the moment the task completes.
                        self.inflight_dispatches
                            .insert(task.id.clone(), (Some(c.to_string()), used.clone()));
                        self.persist_durable_dispatch_state_update(
                            &task.id,
                            Some(&task.short_id),
                            "inflight_ledger_insert",
                            DurableDispatchStateUpdate {
                                inflight: Some(Some((Some(c.to_string()), used.clone()))),
                                ..Default::default()
                            },
                        )
                        .await;
                    }
                }
                DispatchOutcome::AtCapacity => {
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
                DispatchOutcome::PoolDead => return,
                DispatchOutcome::Failed => {
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
            verification_failure_count: 0,
            total_reopen_count: 0,
            total_verification_failure_count: 0,
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
            unresolved_blocker_count: 0,
        }
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
    }

    /// `max`, not sum: a task counted in BOTH the running rows and the ledger
    /// (its session row landed but the ledger entry hasn't been reconciled away
    /// yet) must count once. Also: a larger DB count wins over a smaller ledger.
    #[test]
    fn ledger_overlay_takes_max_never_double_counts() {
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
    }

    /// Creator-less (legacy system) dispatches are ungated by the per-user cap,
    /// so they must not contribute to any count.
    #[test]
    fn ledger_overlay_ignores_creatorless_entries() {
        let mut running: HashMap<(String, String), u32> = HashMap::new();
        let mut inflight: HashMap<String, (Option<String>, String)> = HashMap::new();
        inflight.insert("sys".into(), (None, "m".into()));
        overlay_inflight_ledger(&mut running, &inflight);
        assert!(running.is_empty());
    }
}
