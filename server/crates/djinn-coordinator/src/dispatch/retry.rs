use super::super::*;
use super::DispatchOutcome;
use super::model_under_user_cap;
use djinn_core::clock::{Clock, SystemClock};
#[cfg(not(test))]
use djinn_db::AgentRepository;

fn record_task_parked_metric() {
    djinn_telemetry::task::increment_parked();
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
    HumanReview,
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

    fn activity_entry_mentions_djinn_path(entry: &djinn_core::models::ActivityEntry) -> bool {
        if !entry.payload.contains(".djinn/") {
            return false;
        }

        serde_json::from_str::<serde_json::Value>(&entry.payload)
            .ok()
            .and_then(|payload| {
                payload
                    .get("body")
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| entry.payload.clone())
            .contains(".djinn/")
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
        if !worktree_path.exists() {
            return false;
        }
        let repo = match git2::Repository::open(worktree_path) {
            Ok(repo) => repo,
            Err(e) => {
                tracing::debug!(
                    path = %worktree_path.display(),
                    error = %e,
                    "worktree_has_uncommitted_changes: not a git repo"
                );
                return false;
            }
        };
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true)
            .include_ignored(false)
            .recurse_untracked_dirs(true);
        match repo.statuses(Some(&mut opts)) {
            Ok(statuses) => statuses.iter().any(|entry| {
                let s = entry.status();
                s.intersects(
                    git2::Status::INDEX_NEW
                        | git2::Status::INDEX_MODIFIED
                        | git2::Status::INDEX_DELETED
                        | git2::Status::INDEX_RENAMED
                        | git2::Status::INDEX_TYPECHANGE
                        | git2::Status::WT_NEW
                        | git2::Status::WT_MODIFIED
                        | git2::Status::WT_DELETED
                        | git2::Status::WT_TYPECHANGE
                        | git2::Status::WT_RENAMED,
                )
            }),
            Err(e) => {
                tracing::debug!(
                    path = %worktree_path.display(),
                    error = %e,
                    "worktree_has_uncommitted_changes: status() failed"
                );
                false
            }
        }
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

        // Signal 3: task comments referencing .djinn/ paths.
        let repo = self.task_repo();
        if let Ok(entries) = repo.list_activity(task_id).await
            && entries
                .iter()
                .filter(|entry| entry.event_type == "comment")
                .any(Self::activity_entry_mentions_djinn_path)
        {
            tracing::info!(
                task_id = %task_id,
                "simple-lifecycle artifact detected: task comment references .djinn/ path"
            );
            return true;
        }

        false
    }

    /// The creator's per-user model selection for the lane matching `base_role`
    /// (plan / implement / review), filtered to providers they still have
    /// connected. `base_role` selects the lane: planner/architect/chat → plan,
    /// worker → implement, reviewer → review, lead/unknown → plan.
    pub(crate) async fn resolve_user_model_priority(
        &self,
        created_by_user_id: Option<&str>,
        base_role: &str,
    ) -> Vec<String> {
        #[cfg(test)]
        {
            let _ = created_by_user_id;
            let _ = base_role;
            #[allow(clippy::needless_return)]
            return Vec::new();
        }

        #[cfg(not(test))]
        {
            let Some(uid) = created_by_user_id else {
                return Vec::new();
            };
            let us_repo = djinn_db::UserSettingsRepository::new(self.db.clone());
            let models = match us_repo.get(uid).await {
                Ok(Some(s)) => s
                    .lanes
                    .map(|l| l.for_role(base_role).to_vec())
                    .unwrap_or_default(),
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
    /// A task whose `reopen_count >= REOPEN_INTERVENTION_THRESHOLD` and is about
    /// to be re-dispatched to the worker again is, by definition, not converging
    /// on its own — most commonly because the internal reviewer keeps rejecting
    /// the SAME acceptance criterion every round (the p4bb "Shadow gRPC service
    /// never registered" loop). Re-dispatching the worker a fourth time will not
    /// help, so we hand the task to the Planner, which can DECIDE how to unstick
    /// it (decompose into focused subtasks, rescope the AC, close as
    /// moot/duplicate, or apply the feedback) using the existing intervention
    /// machinery — the planner Workflow C path that `dispatch_planner_escalation`
    /// already drives.
    ///
    /// Returns `true` when the task was routed to a Planner (caller skips the
    /// worker dispatch this pass), `false` otherwise.
    ///
    /// Idempotency: a `planner_intervention` activity marker is written per
    /// `reopen_count` value. While the task stays at the same reopen count — or
    /// while the Planner intervention is in flight — the marker suppresses
    /// re-dispatching a Planner on every tick. The marker is keyed by the
    /// CURRENT reopen count, so a later genuine reopen (count bumps again past
    /// the threshold) re-arms one fresh intervention.
    #[tracing::instrument(
        name = "djinn.dispatch.intervention.trigger",
        skip(self, task),
        fields(task_id = %task.short_id, role = "worker", attempt = task.reopen_count, pass_kind = "trigger_a")
    )]
    pub(crate) async fn maybe_intervene_on_stuck_task(
        &mut self,
        task: &djinn_core::models::Task,
    ) -> bool {
        if task.reopen_count < REOPEN_INTERVENTION_THRESHOLD {
            return false;
        }

        let reason = format!(
            "Internal review loop exceeded {REOPEN_INTERVENTION_THRESHOLD} rounds without \
             convergence (reopen_count={}). The worker keeps re-attempting but the same \
             acceptance criteria remain unmet. Decide how to unstick this: DECOMPOSE into \
             focused subtasks (carve out the specific unmet criterion), RESCOPE/clarify the \
             acceptance criteria and re-dispatch, or CLOSE if the work is moot/duplicate/\
             already-done.",
            task.reopen_count
        );
        self.route_planner_intervention(task, "worker", &reason, None)
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
        self.route_planner_intervention(task, role, &reason, None)
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
    /// stall-cancel streak, reset when the task's status advances (durable
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

        self.route_planner_intervention(&task, role, reason, None)
            .await
    }

    /// Shared intervention router behind triggers A and B: second-strike
    /// terminal park, idempotency marker keyed by the task's CURRENT
    /// `reopen_count`, backoff-state clearing, and the Planner escalation
    /// dispatch. Returns `true` when the task was routed (or terminally
    /// parked) — the caller skips its dispatch this pass.
    #[tracing::instrument(
        name = "djinn.dispatch.intervention",
        skip(self, task, reason),
        fields(task_id = %task.short_id, role = %role, attempt = task.reopen_count, pass_kind = "planner_intervention")
    )]
    pub(crate) async fn route_planner_intervention(
        &mut self,
        task: &djinn_core::models::Task,
        role: &'static str,
        reason: &str,
        ci_failure_sections: Option<&str>,
    ) -> bool {
        // Second strike (terminal hold): the Planner has ALREADY intervened on
        // this task at least `MAX_PLANNER_INTERVENTIONS` time(s) and it has STILL
        // churned back up to the reopen threshold. The reshape/rescope did not
        // unstick it. Escalating again just calls `reset_intervention_counters`
        // (reopen_count→0, intervention_count++) and the worker loops anew,
        // monopolizing the (often single) dispatch slot indefinitely — the txr4
        // query_subgraph case burned 37 sessions / ~11h / 10 total reopens behind
        // one gpt-5.5 slot, starving every other ready task. Hold it indefinitely
        // on a HUMAN instead of force-closing: a human-review remediation task
        // blocks the source, which is parked back to `open` so it consumes no
        // dispatch slot (`list_ready` skips blocked-open tasks) yet stays
        // revivable — `emit_unblocked_tasks` resurfaces it the moment a human
        // resolves the remediation. The work and its branch persist; nothing is
        // auto-closed.
        if task.intervention_count >= MAX_PLANNER_INTERVENTIONS {
            let reason = format!(
                "Auto-parked for human review: {} planner intervention(s) failed to break the \
                 rework loop (intervention_count={}, total_reopen_count={}). The same acceptance \
                 criteria kept failing across repeated rounds even after the planner reshaped the \
                 scope, so re-dispatching would only loop again and hold the dispatch slot. The \
                 task is held (open + blocked on a human-review remediation task) so it frees the \
                 dispatch slot for other ready tasks while its branch and prior work are \
                 preserved. A human must resolve the remediation task to release it, or close \
                 this task if the work is no longer wanted.",
                task.intervention_count, task.intervention_count, task.total_reopen_count,
            );
            tracing::warn!(
                task_id = %task.short_id,
                intervention_count = task.intervention_count,
                total_reopen_count = task.total_reopen_count,
                reopen_count = task.reopen_count,
                "CoordinatorActor: second-strike — holding unconvergeable task on human review after repeated planner interventions"
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
                "planner_second_strike_human_hold_clear",
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
            // Ensure a HUMAN-review remediation task blocks the source (creating
            // one only if it isn't already held), THEN park the source to `open`.
            // The blocker is added before the park, so the open task is never
            // dispatchable without its blocker in place.
            self.create_remediation_task(
                &task.id,
                &reason,
                &task.project_id,
                RemediationKind::HumanReview,
            )
            .await;
            self.park_source_open(&task.id, &reason).await;
            record_task_parked_metric();
            return true;
        }

        // Idempotency guard: have we already routed a Planner for THIS reopen
        // count? If so, leave it to the in-flight (or already-dispatched)
        // Planner — do not stack interventions.
        match self
            .planner_intervention_marker_exists(task, task.reopen_count)
            .await
        {
            Ok(true) => return false,
            Ok(false) => {}
            Err(e) => {
                // Fail safe: on a DB read error, do NOT intervene (the normal
                // dispatch + escalating cooldown still bounds the loop). Better
                // to under-trigger than to spam Planners on a transient error.
                tracing::warn!(
                    task_id = %task.short_id,
                    error = %e,
                    "CoordinatorActor: planner-intervention marker check failed; skipping intervention this pass"
                );
                return false;
            }
        }

        // Record the marker BEFORE dispatching so a concurrent tick (or a
        // dispatch failure) cannot double-fire for the same reopen count.
        if let Err(e) = self
            .record_planner_intervention_marker(task, task.reopen_count)
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

        let enriched_reason = match ci_failure_sections {
            Some(sections) if !sections.is_empty() => {
                format!("{reason}\n\n**CI Failure Details:**\n{sections}")
            }
            _ => reason.to_string(),
        };
        self.dispatch_planner_escalation(&task.id, &enriched_reason, &task.project_id)
            .await;
        true
    }

    /// Returns `true` if a `planner_intervention` marker already exists for
    /// `task` at the given `reopen_count`.
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

    /// Record a `planner_intervention` marker for `task` at `reopen_count`.
    async fn record_planner_intervention_marker(
        &self,
        task: &djinn_core::models::Task,
        reopen_count: i64,
    ) -> djinn_db::Result<()> {
        let payload = serde_json::json!({
            "reopen_count": reopen_count,
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

    /// Dispatch a Planner escalation: create a review task, add a comment linking it
    /// to the source task, then dispatch the Planner to it.
    ///
    /// Called when Lead calls `request_planner` or when auto-escalation fires on the
    /// 2nd `request_lead` for the same task.  Per ADR-051 §8 the Planner is now the
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
    /// 2nd `request_lead` for the same task, and on the CI-loop / second-strike
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

        // Human-review remediation is idempotent: if the source is already held
        // by an unresolved blocker, a remediation task already exists — don't
        // stack a fresh one on every park tick.
        if kind == RemediationKind::HumanReview
            && let Some(src) = source_task.as_ref()
        {
            match task_repo.list_blockers(&src.id).await {
                Ok(blockers) if blockers.iter().any(|b| b.status != "closed") => {
                    tracing::info!(
                        source_task_id = %src.short_id,
                        "CoordinatorActor: human-review remediation skipped — source already held by an unresolved blocker"
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
                    let running = self.effective_running_by_user_model().await;
                    let caps = djinn_db::UserSettingsRepository::new(self.db.clone())
                        .get(creator)
                        .await
                        .ok()
                        .flatten()
                        .and_then(|s| s.max_sessions)
                        .unwrap_or_default();
                    let mut filtered: Vec<String> = Vec::new();
                    for m in &model_ids {
                        let cap = caps.get(m).copied().unwrap_or(1);
                        if model_under_user_cap(&running, creator, m, cap) {
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
            RemediationKind::HumanReview => (Vec::new(), None),
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
        };
        let review_task = match djinn_core::auth_context::SESSION_USER_ID
            .scope(
                source_creator,
                task_repo.create_in_project(
                    project_id,
                    None,
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

        // Tag the human-review remediation task with `human-review-hold` so the
        // UI can surface a "needs your review" indicator on it (the actual item
        // a human must act on; closing it revives the held source task). Write
        // only the labels column: reusing the broad `update` path here is more
        // fragile because it reserializes unrelated JSON columns and can silently
        // leave the hold unlabeled if any copied field fails validation.
        // Non-fatal: a failed label write still leaves the hold in place via the
        // blocker + comment, but tests assert this path stays healthy.
        if kind == RemediationKind::HumanReview
            && let Err(e) = task_repo
                .update_labels(&review_task.id, r#"["human-review-hold"]"#)
                .await
        {
            tracing::warn!(
                error = %e,
                review_task_id = %review_task.short_id,
                "CoordinatorActor: human-review remediation — failed to set human-review-hold label"
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

        // Human-review remediation is intentionally NOT dispatched to any agent —
        // a human must resolve it, so the source stays held until they do.
        if kind == RemediationKind::HumanReview {
            tracing::info!(
                review_task_id = %review_task.short_id,
                source_task_id = %source_task_id,
                project_id = %project_id,
                "CoordinatorActor: human-review remediation created; awaiting human resolution (no agent dispatched)"
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
            DispatchOutcome::Failed => {
                tracing::debug!(outcome = "error", task_id = %review_task.short_id, role = "planner");
                tracing::debug!(
                    "CoordinatorActor: planner escalation — no model could accept Planner dispatch"
                );
            }
        }
    }
}

#[cfg(test)]
mod telemetry_tests {
    use super::record_task_parked_metric;

    #[test]
    fn planner_second_strike_park_metric_records_after_terminal_close() {
        djinn_telemetry::init().unwrap();
        let before = djinn_telemetry::render().unwrap();
        let parked_before = unlabelled_metric_value(&before, "djinn_tasks_parked_total");

        record_task_parked_metric();

        let rendered = djinn_telemetry::render().unwrap();
        assert_eq!(
            unlabelled_metric_value(&rendered, "djinn_tasks_parked_total"),
            parked_before + 1.0
        );
    }

    fn unlabelled_metric_value(rendered: &str, metric: &str) -> f64 {
        rendered
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(' ')?;
                (name == metric).then(|| value.parse::<f64>().expect("metric value parses"))
            })
            .unwrap_or_else(|| panic!("missing metric {metric} in:\n{rendered}"))
    }
}
