// djinn:allow-oversize
use super::*;
use crate::supervisor_impl::disposition::{NUDGE_CAP, RunDisposition, decide_run_disposition};
use djinn_core::models::{ReopenClass, TransitionAction};
use djinn_core::run_progress::RunProgress;
use djinn_core::{events::DjinnEventEnvelope, models::SessionStatus};
use djinn_db::{DispatchStateRepository, DispatchStateUpsert, ReadyQuery, UserRepository};

#[allow(dead_code)]
struct InterventionChaosHarness {
    db: Database,
    tx: broadcast::Sender<DjinnEventEnvelope>,
    _rx: broadcast::Receiver<DjinnEventEnvelope>,
    actor: CoordinatorActor,
    repo: TaskRepository,
    task_id: String,
    /// Stable `users.id` for the synthetic capacity-bearer that backs the
    /// `seed_running_capacity_occupancy` flow. Seeded via
    /// `UserRepository::upsert_from_github` so the FK from
    /// `sessions.created_by_user_id` is satisfied; the test only needs this
    /// id to round-trip through `count_active_by_user_and_model()` and the
    /// in-memory `inflight_dispatches` ledger.
    capacity_user_id: String,
}

#[allow(dead_code)]
impl InterventionChaosHarness {
    async fn new(initial_reopen_count: i64) -> Self {
        let db = Database::open_in_memory().unwrap();
        let (tx, rx) = broadcast::channel(256);
        let actor = coordinator_actor_for_tests(&db, &tx);
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        // Seed a real user row so the FK from `sessions.created_by_user_id`
        // to `users.id` is satisfied when `seed_running_capacity_occupancy`
        // scopes `SESSION_USER_ID` to the harness's capacity bearer.
        // The actual id minted here is opaque to the assertions; the
        // harness only uses it to round-trip the in-memory inflight
        // ledger and the `count_active_by_user_and_model` lookup.
        let capacity_user_id = UserRepository::new(db.clone())
            .upsert_from_github(
                9_001_001,
                "chaos-capacity-bearer",
                Some("Chaos Capacity Bearer"),
                None,
            )
            .await
            .unwrap()
            .id;
        let task = make_task_with_reopen_count(&db, &tx, initial_reopen_count).await;

        Self {
            db,
            tx,
            _rx: rx,
            actor,
            repo,
            task_id: task.id,
            capacity_user_id,
        }
    }

    async fn task(&self) -> djinn_core::models::Task {
        self.repo
            .get(&self.task_id)
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("seed task {} should exist", self.task_id))
    }

    async fn drive_review_failure_cycle(&mut self) -> djinn_core::models::Task {
        self.repo.set_status(&self.task_id, "closed").await.unwrap();
        self.repo.set_status(&self.task_id, "open").await.unwrap()
    }

    async fn drive_review_failure_cycles(&mut self, cycles: i64) -> djinn_core::models::Task {
        assert!(cycles >= 0, "cycle count must be non-negative");
        for _ in 0..cycles {
            self.drive_review_failure_cycle().await;
        }
        self.task().await
    }

    async fn drive_review_failures_through_threshold(&mut self) -> djinn_core::models::Task {
        self.drive_to_reopen_count(REOPEN_INTERVENTION_THRESHOLD)
            .await
    }

    async fn drive_review_failures_beyond_threshold(
        &mut self,
        extra_cycles: i64,
    ) -> djinn_core::models::Task {
        assert!(extra_cycles >= 0, "extra cycle count must be non-negative");
        self.drive_to_reopen_count(REOPEN_INTERVENTION_THRESHOLD + extra_cycles)
            .await
    }

    async fn drive_to_reopen_count(&mut self, target: i64) -> djinn_core::models::Task {
        let mut task = self.task().await;
        assert!(
            target >= task.reopen_count,
            "harness only advances reopen_count deterministically"
        );
        while task.reopen_count < target {
            task = self.drive_review_failure_cycle().await;
        }
        assert_eq!(task.reopen_count, target, "harness reopen_count target");
        task
    }

    async fn route_reopen_intervention(&mut self) -> (bool, djinn_core::models::Task) {
        let task = self.task().await;
        let handled = self.actor.maybe_intervene_on_stuck_task(&task).await;
        let refreshed = self.task().await;
        (handled, refreshed)
    }

    async fn dispatch_same_role_reappearance_like_dispatch(
        &mut self,
        role: &'static str,
        had_provider_failure: bool,
    ) -> (bool, djinn_core::models::Task) {
        let next_streak = self
            .actor
            .dispatch_failure_streak
            .get(&self.task_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.seed_same_role_redispatch_state(role, next_streak)
            .await;
        let task = self.task().await;
        self.actor.last_dispatched.remove(&task.id);

        if should_route_cycling_intervention(role, next_streak, had_provider_failure)
            && self
                .actor
                .maybe_intervene_on_cycling_task(&task, role, next_streak)
                .await
        {
            self.actor.dispatch_failure_streak.remove(&task.id);
            self.actor.dispatch_cooldowns.remove(&task.id);
            self.actor.inflight_dispatches.remove(&task.id);
            self.actor
                .clear_durable_dispatch_backoff_state(
                    &task.id,
                    Some(&task.short_id),
                    "test_cycling_planner_intervention_handoff_clear",
                )
                .await;
            return (true, self.task().await);
        }

        if !had_provider_failure && next_streak >= MAX_DISPATCH_FAILURES {
            let reason = "repeated dispatch failures: the task could not complete after multiple \
                          attempts. Resolve the underlying issue and reopen.";
            self.repo
                .transition(
                    &task.id,
                    djinn_core::models::TransitionAction::ForceClose,
                    "coordinator",
                    "system",
                    Some(reason),
                    None,
                )
                .await
                .unwrap();
            self.actor.dispatch_failure_streak.remove(&task.id);
            self.actor.dispatch_cooldowns.remove(&task.id);
            self.actor.inflight_dispatches.remove(&task.id);
            SessionRepository::new(self.db.clone(), crate::events::event_bus_for(&self.tx))
                .interrupt_running_for_task(&task.id)
                .await
                .unwrap();
            self.actor
                .clear_durable_dispatch_backoff_state(
                    &task.id,
                    Some(&task.short_id),
                    "test_same_role_terminal_close_clear",
                )
                .await;
            return (true, self.task().await);
        }

        self.persist_same_role_backoff_after_reappearance(next_streak)
            .await;
        (false, task)
    }

    async fn dispatch_same_role_reappearances_like_dispatch(
        &mut self,
        role: &'static str,
        cycles: u32,
    ) -> (bool, djinn_core::models::Task) {
        let mut last = (false, self.task().await);
        for _ in 0..cycles {
            last = self
                .dispatch_same_role_reappearance_like_dispatch(role, false)
                .await;
        }
        last
    }

    async fn complete_planner_intervention_and_reset_ladder(&self) -> djinn_core::models::Task {
        self.repo
            .reset_intervention_counters(&self.task_id)
            .await
            .unwrap();
        self.task().await
    }

    async fn seed_same_role_redispatch_state(&mut self, role: &'static str, streak: u32) {
        let cooldown = std::time::Duration::from_secs(300);
        self.actor
            .dispatch_failure_streak
            .insert(self.task_id.clone(), streak);
        self.actor
            .dispatch_cooldowns
            .insert(self.task_id.clone(), StdInstant::now() + cooldown);
        self.actor.last_dispatched.insert(
            self.task_id.clone(),
            DispatchMarker {
                instant: StdInstant::now(),
                role: role.to_owned(),
            },
        );

        let last_dispatched_at = rfc3339(::time::OffsetDateTime::now_utc());
        let cooldown_until = rfc3339(
            ::time::OffsetDateTime::now_utc()
                + ::time::Duration::try_from(cooldown).expect("cooldown duration fits time"),
        );
        DispatchStateRepository::new(self.db.clone())
            .upsert(DispatchStateUpsert {
                task_id: &self.task_id,
                failure_streak: i64::from(streak),
                cooldown_until: Some(&cooldown_until),
                escalation_count: 0,
                last_dispatched_at: Some(&last_dispatched_at),
                last_dispatched_role: Some(role),
                inflight_creator_user_id: None,
                inflight_model_id: None,
            })
            .await
            .unwrap();
    }

    async fn advance_same_role_cycle(
        &mut self,
        role: &'static str,
    ) -> (bool, djinn_core::models::Task) {
        let next_streak = self
            .actor
            .dispatch_failure_streak
            .get(&self.task_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.seed_same_role_redispatch_state(role, next_streak)
            .await;
        let task = self.task().await;
        let handled = self
            .actor
            .maybe_intervene_on_cycling_task(&task, role, next_streak)
            .await;
        let refreshed = self.task().await;
        (handled, refreshed)
    }

    async fn advance_same_role_cycles(
        &mut self,
        role: &'static str,
        cycles: u32,
    ) -> (bool, djinn_core::models::Task) {
        let mut last = (false, self.task().await);
        for _ in 0..cycles {
            last = self.advance_same_role_cycle(role).await;
        }
        last
    }

    async fn advance_same_role_to_streak(
        &mut self,
        role: &'static str,
        target_streak: u32,
    ) -> (bool, djinn_core::models::Task) {
        let current = self
            .actor
            .dispatch_failure_streak
            .get(&self.task_id)
            .copied()
            .unwrap_or(0);
        assert!(
            target_streak >= current,
            "harness only advances same-role streaks deterministically"
        );
        self.advance_same_role_cycles(role, target_streak - current)
            .await
    }

    async fn planner_intervention_markers(&self) -> Vec<serde_json::Value> {
        planner_intervention_markers(&self.repo, &self.task_id).await
    }

    async fn durable_dispatch_state(&self) -> Option<djinn_db::DispatchStateRecord> {
        DispatchStateRepository::new(self.db.clone())
            .get(&self.task_id)
            .await
            .unwrap()
    }

    async fn seed_running_capacity_occupancy(&mut self) {
        let task = self.task().await;
        let session_repo =
            SessionRepository::new(self.db.clone(), crate::events::event_bus_for(&self.tx));
        let capacity_user_id = self.capacity_user_id.clone();
        djinn_core::auth_context::SESSION_USER_ID
            .scope(Some(capacity_user_id.clone()), async {
                session_repo
                    .create(CreateSessionParams {
                        project_id: &task.project_id,
                        task_id: Some(&task.id),
                        model: DEFAULT_MODEL_ID,
                        agent_type: "worker",
                        metadata_json: None,
                        task_run_id: None,
                        pricing: None,
                        cost_basis: None,
                    })
                    .await
                    .unwrap()
            })
            .await;

        self.actor.inflight_dispatches.insert(
            self.task_id.clone(),
            (Some(capacity_user_id.clone()), DEFAULT_MODEL_ID.to_owned()),
        );

        let existing = self.durable_dispatch_state().await;
        DispatchStateRepository::new(self.db.clone())
            .upsert(DispatchStateUpsert {
                task_id: &self.task_id,
                failure_streak: existing.as_ref().map_or(0, |record| record.failure_streak),
                cooldown_until: existing
                    .as_ref()
                    .and_then(|record| record.cooldown_until.as_deref()),
                escalation_count: existing
                    .as_ref()
                    .map_or(0, |record| record.escalation_count),
                last_dispatched_at: existing
                    .as_ref()
                    .and_then(|record| record.last_dispatched_at.as_deref()),
                last_dispatched_role: existing
                    .as_ref()
                    .and_then(|record| record.last_dispatched_role.as_deref()),
                inflight_creator_user_id: Some(&capacity_user_id),
                inflight_model_id: Some(DEFAULT_MODEL_ID),
            })
            .await
            .unwrap();

        self.assert_capacity_occupied().await;
    }

    async fn active_capacity_count(&self) -> i64 {
        SessionRepository::new(self.db.clone(), crate::events::event_bus_for(&self.tx))
            .count_active_by_user_and_model()
            .await
            .unwrap()
            .into_iter()
            .find(|(creator, model, _)| {
                creator.as_deref() == Some(self.capacity_user_id.as_str())
                    && model == DEFAULT_MODEL_ID
            })
            .map_or(0, |(_, _, count)| count)
    }

    async fn assert_capacity_occupied(&self) {
        assert_eq!(
            self.active_capacity_count().await,
            1,
            "seeded running session should occupy the user/model capacity signal dispatch reads"
        );
        assert_eq!(
            self.actor.inflight_dispatches.get(&self.task_id),
            Some(&(
                Some(self.capacity_user_id.clone()),
                DEFAULT_MODEL_ID.to_owned()
            )),
            "in-memory in-flight capacity ledger should be seeded"
        );
        let durable = self
            .durable_dispatch_state()
            .await
            .expect("durable dispatch state should be seeded");
        assert_eq!(
            durable.inflight_creator_user_id.as_deref(),
            Some(self.capacity_user_id.as_str())
        );
        assert_eq!(durable.inflight_model_id.as_deref(), Some(DEFAULT_MODEL_ID));
    }

    async fn assert_capacity_released(&self) {
        assert_eq!(
            self.active_capacity_count().await,
            0,
            "terminal path should interrupt the running session so dispatch capacity is released"
        );
        assert!(
            !self.actor.inflight_dispatches.contains_key(&self.task_id),
            "terminal path should clear the in-memory in-flight dispatch ledger"
        );
        if let Some(durable) = self.durable_dispatch_state().await {
            assert!(
                durable.inflight_creator_user_id.is_none(),
                "durable in-flight creator should be cleared"
            );
            assert!(
                durable.inflight_model_id.is_none(),
                "durable in-flight model should be cleared"
            );
        }
    }

    async fn open_planner_intervention_reviews(&self) -> Vec<djinn_core::models::Task> {
        let task = self.task().await;
        self.repo
            .list_by_status("open")
            .await
            .unwrap()
            .into_iter()
            .filter(|candidate| {
                candidate.issue_type == "review" && candidate.project_id == task.project_id
            })
            .collect()
    }

    async fn assert_open_planner_review_count(&self, expected: usize) {
        assert_eq!(
            self.open_planner_intervention_reviews().await.len(),
            expected,
            "open Planner intervention review count"
        );
    }

    async fn assert_latest_status_change_reason_contains(&self, needle: &str) {
        let entries = self.repo.list_activity(&self.task_id).await.unwrap();
        let reason = entries
            .iter()
            .rev()
            .find(|entry| entry.event_type == "status_changed")
            .and_then(|entry| serde_json::from_str::<serde_json::Value>(&entry.payload).ok())
            .and_then(|payload| {
                payload
                    .get("reason")
                    .and_then(|reason| reason.as_str())
                    .map(str::to_owned)
            });

        assert!(
            reason
                .as_deref()
                .is_some_and(|reason| reason.contains(needle)),
            "latest status-change reason should contain {needle:?}; got {reason:?}"
        );
    }

    async fn assert_marker_reopen_counts(&self, expected: &[i64]) {
        let actual: Vec<i64> = self
            .planner_intervention_markers()
            .await
            .iter()
            .map(|marker| {
                marker["reopen_count"]
                    .as_i64()
                    .expect("marker reopen_count")
            })
            .collect();
        assert_eq!(
            actual, expected,
            "planner_intervention marker reopen counts"
        );
    }

    async fn assert_planner_marker_count(&self, expected: usize) {
        assert_eq!(
            self.planner_intervention_markers().await.len(),
            expected,
            "planner_intervention marker count"
        );
    }

    async fn assert_same_role_backoff_seeded(&self, role: &str, expected_streak: u32) {
        assert_eq!(
            self.actor
                .dispatch_failure_streak
                .get(&self.task_id)
                .copied(),
            Some(expected_streak),
            "in-memory dispatch_failure_streak"
        );
        assert!(
            self.actor.dispatch_cooldowns.contains_key(&self.task_id),
            "in-memory dispatch_cooldowns should be seeded"
        );
        assert_eq!(
            self.actor
                .last_dispatched
                .get(&self.task_id)
                .map(|marker| marker.role.as_str()),
            Some(role),
            "in-memory last_dispatched role"
        );

        let durable = self
            .durable_dispatch_state()
            .await
            .expect("durable dispatch state should be seeded");
        assert_eq!(durable.failure_streak, i64::from(expected_streak));
        assert!(durable.cooldown_until.is_some(), "durable cooldown_until");
        assert!(
            durable.last_dispatched_at.is_some(),
            "durable last_dispatched_at"
        );
        assert_eq!(durable.last_dispatched_role.as_deref(), Some(role));
    }

    async fn persist_same_role_backoff_after_reappearance(&self, streak: u32) {
        let cooldown_until = rfc3339(
            ::time::OffsetDateTime::now_utc()
                + ::time::Duration::try_from(std::time::Duration::from_secs(300))
                    .expect("cooldown duration fits time"),
        );
        DispatchStateRepository::new(self.db.clone())
            .upsert(DispatchStateUpsert {
                task_id: &self.task_id,
                failure_streak: i64::from(streak),
                cooldown_until: Some(&cooldown_until),
                escalation_count: 0,
                last_dispatched_at: None,
                last_dispatched_role: None,
                inflight_creator_user_id: None,
                inflight_model_id: None,
            })
            .await
            .unwrap();
    }

    async fn assert_same_role_backoff_after_reappearance(&self, expected_streak: u32) {
        assert_eq!(
            self.actor
                .dispatch_failure_streak
                .get(&self.task_id)
                .copied(),
            Some(expected_streak),
            "in-memory dispatch_failure_streak"
        );
        assert!(
            self.actor.dispatch_cooldowns.contains_key(&self.task_id),
            "in-memory dispatch_cooldowns should be seeded"
        );
        assert!(
            !self.actor.last_dispatched.contains_key(&self.task_id),
            "same-role reappearance consumes last_dispatched before backing off"
        );

        let durable = self
            .durable_dispatch_state()
            .await
            .expect("durable dispatch state should be seeded");
        assert_eq!(durable.failure_streak, i64::from(expected_streak));
        assert!(durable.cooldown_until.is_some(), "durable cooldown_until");
        assert!(
            durable.last_dispatched_at.is_none(),
            "durable last_dispatched_at should be cleared after reappearance"
        );
        assert!(
            durable.last_dispatched_role.is_none(),
            "durable last_dispatched_role should be cleared after reappearance"
        );
    }

    async fn assert_task_status(&self, status: &str, close_reason_contains: Option<&str>) {
        let task = self.task().await;
        assert_eq!(task.status, status, "task status");
        if let Some(needle) = close_reason_contains {
            assert!(
                task.close_reason
                    .as_deref()
                    .is_some_and(|reason| reason.contains(needle)),
                "close_reason should contain {needle:?}; got {:?}",
                task.close_reason
            );
        }
    }

    async fn assert_source_task_not_ready_open(&self) {
        let open_tasks = self.repo.list_by_status("open").await.unwrap();
        assert!(
            open_tasks.iter().all(|task| task.id != self.task_id),
            "source task must not remain as a ready open task after terminal close"
        );
    }

    async fn assert_dispatch_backoff_cleared(&self) {
        assert!(
            !self.actor.last_dispatched.contains_key(&self.task_id),
            "last_dispatched should be cleared"
        );
        assert!(
            !self
                .actor
                .dispatch_failure_streak
                .contains_key(&self.task_id),
            "dispatch_failure_streak should be cleared"
        );
        assert!(
            !self.actor.dispatch_cooldowns.contains_key(&self.task_id),
            "dispatch_cooldowns should be cleared"
        );
        assert!(
            !self.actor.inflight_dispatches.contains_key(&self.task_id),
            "inflight_dispatches should be cleared"
        );

        let durable = DispatchStateRepository::new(self.db.clone())
            .get(&self.task_id)
            .await
            .unwrap();
        if let Some(durable) = durable {
            assert_eq!(durable.failure_streak, 0, "durable failure_streak");
            assert!(durable.cooldown_until.is_none(), "durable cooldown_until");
            assert!(
                durable.last_dispatched_at.is_none(),
                "durable last_dispatched_at"
            );
            assert!(
                durable.last_dispatched_role.is_none(),
                "durable last_dispatched_role"
            );
            assert!(
                durable.inflight_creator_user_id.is_none(),
                "durable inflight_creator_user_id"
            );
            assert!(
                durable.inflight_model_id.is_none(),
                "durable inflight_model_id"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reopen_loop_guard_second_strike_chaos_parks_without_rearming() {
    let mut harness = InterventionChaosHarness::new(0).await;

    // Trigger A is keyed to the configured reopen threshold. The first climb
    // asserts escalation at the production threshold; the post-Planner climb
    // intentionally runs one cycle beyond it so the same task experiences 4+
    // consecutive review/CI-style failures before the terminal second strike.
    const {
        assert!(
            REOPEN_INTERVENTION_THRESHOLD + 1 >= 4,
            "threshold-plus-one should exercise 4+ reopen cycles"
        );
    }

    let first_threshold = harness.drive_review_failures_through_threshold().await;
    assert_eq!(first_threshold.reopen_count, REOPEN_INTERVENTION_THRESHOLD);
    assert_eq!(first_threshold.intervention_count, 0);

    let (handled, first_routed) = harness.route_reopen_intervention().await;
    assert!(handled, "first threshold crossing routes to Planner");
    assert_eq!(first_routed.status, "open", "source task stays open");
    harness
        .assert_marker_reopen_counts(&[REOPEN_INTERVENTION_THRESHOLD])
        .await;
    harness.assert_open_planner_review_count(1).await;

    let (handled_again, unchanged_after_repeat) = harness.route_reopen_intervention().await;
    assert!(
        !handled_again,
        "same reopen-count check is suppressed by the intervention marker"
    );
    assert_eq!(
        unchanged_after_repeat.reopen_count,
        REOPEN_INTERVENTION_THRESHOLD
    );
    harness
        .assert_marker_reopen_counts(&[REOPEN_INTERVENTION_THRESHOLD])
        .await;
    harness.assert_open_planner_review_count(1).await;

    let reset = harness
        .complete_planner_intervention_and_reset_ladder()
        .await;
    assert_eq!(reset.reopen_count, 0, "Planner completion resets ladder");
    assert_eq!(
        reset.intervention_count, MAX_PLANNER_INTERVENTIONS,
        "Planner completion records the first strike"
    );

    harness.drive_review_failures_beyond_threshold(1).await;
    let second_threshold = harness.task().await;
    assert_eq!(
        second_threshold.reopen_count,
        REOPEN_INTERVENTION_THRESHOLD + 1
    );
    assert_eq!(
        second_threshold.intervention_count,
        MAX_PLANNER_INTERVENTIONS
    );

    harness.seed_same_role_redispatch_state("worker", 2).await;
    harness.assert_same_role_backoff_seeded("worker", 2).await;
    harness.seed_running_capacity_occupancy().await;

    let (second_handled, parked) = harness.route_reopen_intervention().await;
    assert!(
        second_handled,
        "second strike is handled terminally instead of redispatching"
    );
    assert_eq!(parked.reopen_count, REOPEN_INTERVENTION_THRESHOLD + 1);
    // Second strike now HOLDS the task on a human rather than force-closing it:
    // it stays `open` and blocked (the first-strike planner review is still an
    // unresolved blocker, so no fresh remediation is stacked) so it consumes no
    // dispatch slot yet is revivable when a human resolves the remediation.
    harness.assert_task_status("open", None).await;
    assert!(
        !harness
            .repo
            .list_blockers(&harness.task_id)
            .await
            .unwrap()
            .is_empty(),
        "parked source must remain held by an unresolved remediation blocker"
    );
    harness.assert_dispatch_backoff_cleared().await;
    harness.assert_capacity_released().await;
    harness
        .assert_marker_reopen_counts(&[REOPEN_INTERVENTION_THRESHOLD])
        .await;
    harness.assert_open_planner_review_count(1).await;

    let marker_count_after_park = harness.planner_intervention_markers().await.len();
    let review_count_after_park = harness.open_planner_intervention_reviews().await.len();

    let (terminal_recheck_handled, terminal_recheck) = harness.route_reopen_intervention().await;
    assert!(
        terminal_recheck_handled,
        "terminal second-strike recheck is consumed rather than redispatched"
    );
    assert_eq!(terminal_recheck.status, "open");
    assert_eq!(
        harness.planner_intervention_markers().await.len(),
        marker_count_after_park,
        "terminal recheck must not write another Planner marker"
    );
    assert_eq!(
        harness.open_planner_intervention_reviews().await.len(),
        review_count_after_park,
        "terminal recheck must not create another Planner review"
    );
    harness.assert_dispatch_backoff_cleared().await;
    harness.assert_capacity_released().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_role_cycling_trigger_b_chaos_intervenes_then_terminally_closes() {
    let mut harness = InterventionChaosHarness::new(0).await;
    let role = "worker";

    assert_eq!(
        STREAK_INTERVENTION_THRESHOLD, 4,
        "Trigger B chaos coverage is pinned to the production threshold"
    );
    assert!(should_route_cycling_intervention(
        role,
        STREAK_INTERVENTION_THRESHOLD,
        false
    ));
    assert!(!should_route_cycling_intervention(
        role,
        STREAK_INTERVENTION_THRESHOLD,
        true
    ));
    assert!(!should_route_cycling_intervention(
        "planner",
        STREAK_INTERVENTION_THRESHOLD,
        false
    ));

    let (below_handled, below_threshold) = harness
        .dispatch_same_role_reappearances_like_dispatch(role, STREAK_INTERVENTION_THRESHOLD - 1)
        .await;
    assert!(
        !below_handled,
        "same-role cycles below threshold only seed backoff"
    );
    assert_eq!(below_threshold.status, "open");
    harness
        .assert_same_role_backoff_after_reappearance(STREAK_INTERVENTION_THRESHOLD - 1)
        .await;
    harness.assert_planner_marker_count(0).await;
    harness.assert_open_planner_review_count(0).await;

    let (threshold_handled, first_routed) = harness
        .dispatch_same_role_reappearance_like_dispatch(role, false)
        .await;
    assert!(
        threshold_handled,
        "threshold crossing routes to the cycling Planner intervention"
    );
    assert_eq!(first_routed.status, "open", "source task stays open");
    assert_eq!(
        first_routed.reopen_count, 0,
        "Trigger B does not need reopen_count"
    );
    harness.assert_planner_marker_count(1).await;
    harness.assert_marker_reopen_counts(&[0]).await;
    harness.assert_open_planner_review_count(1).await;
    harness.assert_dispatch_backoff_cleared().await;

    let (suppressed_handled, suppressed) = harness
        .dispatch_same_role_reappearances_like_dispatch(role, STREAK_INTERVENTION_THRESHOLD)
        .await;
    assert!(
        !suppressed_handled,
        "same reopen-count loop is idempotently suppressed after the first Planner handoff"
    );
    assert_eq!(suppressed.status, "open");
    harness.assert_planner_marker_count(1).await;
    harness.assert_open_planner_review_count(1).await;
    harness
        .assert_same_role_backoff_after_reappearance(STREAK_INTERVENTION_THRESHOLD)
        .await;

    let remaining_before_hard_cap = MAX_DISPATCH_FAILURES - STREAK_INTERVENTION_THRESHOLD - 1;
    let (pre_terminal_handled, pre_terminal) = harness
        .dispatch_same_role_reappearances_like_dispatch(role, remaining_before_hard_cap)
        .await;
    assert!(
        !pre_terminal_handled,
        "cycles up to one below the hard cap should only refresh backoff"
    );
    assert_eq!(pre_terminal.status, "open");
    harness
        .assert_same_role_backoff_after_reappearance(MAX_DISPATCH_FAILURES - 1)
        .await;
    harness.seed_running_capacity_occupancy().await;

    let (terminal_handled, terminal) = harness
        .dispatch_same_role_reappearance_like_dispatch(role, false)
        .await;
    assert!(
        terminal_handled,
        "hard cap must consume the cycle instead of redispatching indefinitely"
    );
    assert_eq!(terminal.status, "closed");
    assert_eq!(
        terminal.close_reason.as_deref(),
        Some("force_closed"),
        "terminal close should use force_close semantics"
    );
    harness
        .assert_latest_status_change_reason_contains("repeated dispatch failures")
        .await;
    harness.assert_dispatch_backoff_cleared().await;
    harness.assert_capacity_released().await;
    harness.assert_source_task_not_ready_open().await;
    harness.assert_planner_marker_count(1).await;
    harness.assert_open_planner_review_count(1).await;

    let mut provider_guard = InterventionChaosHarness::new(0).await;
    let (provider_handled, provider_task) = provider_guard
        .dispatch_same_role_reappearances_like_dispatch(role, STREAK_INTERVENTION_THRESHOLD - 1)
        .await;
    assert!(!provider_handled);
    assert_eq!(provider_task.status, "open");
    let (provider_threshold_handled, provider_threshold_task) = provider_guard
        .dispatch_same_role_reappearance_like_dispatch(role, true)
        .await;
    assert!(
        !provider_threshold_handled,
        "typed provider failure at threshold must stay on backoff path, not Planner intervention"
    );
    assert_eq!(provider_threshold_task.status, "open");
    provider_guard.assert_planner_marker_count(0).await;
    provider_guard.assert_open_planner_review_count(0).await;
    provider_guard
        .assert_same_role_backoff_after_reappearance(STREAK_INTERVENTION_THRESHOLD)
        .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn below_threshold_does_not_intervene() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD - 1).await;

    let intervened = actor.maybe_intervene_on_stuck_task(&task).await;
    assert!(!intervened, "below threshold must not intervene");

    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    assert!(
        planner_intervention_markers(&repo, &task.id)
            .await
            .is_empty(),
        "no intervention marker should be written below threshold"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loop_guard_routes_to_planner_without_dispatch_failure_streak() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    actor.dispatch_failure_streak.insert(task.id.clone(), 2);
    actor.last_dispatched.insert(
        task.id.clone(),
        DispatchMarker {
            instant: StdInstant::now(),
            role: "worker".into(),
        },
    );

    let handled = actor
        .route_loop_guard_planner_intervention(
            &task.id,
            "worker",
            "Reply-loop guard `identical_tool_failure` tripped: offending_signature=`shell:cargo-test`, threshold=3, observed=4, turn_span=7..=12",
        )
        .await;
    assert!(
        handled,
        "loop guard trip must be routed through Planner intervention"
    );

    assert!(
        !actor.dispatch_failure_streak.contains_key(&task.id),
        "route_planner_intervention clears stale streak state instead of incrementing it"
    );
    assert!(
        !actor.last_dispatched.contains_key(&task.id),
        "loop guard path must gate identical worker re-dispatch"
    );

    let markers = planner_intervention_markers(&repo, &task.id).await;
    assert_eq!(
        markers.len(),
        1,
        "loop guard writes planner_intervention marker"
    );
    assert_eq!(markers[0]["reopen_count"], 0);

    let reviews = repo.list_by_status("open").await.unwrap();
    assert!(
        reviews
            .iter()
            .any(|t| t.issue_type == "review" && t.project_id == task.project_id),
        "loop guard trip must create a Planner intervention review task, not redispatch the worker"
    );
}

/// Drain the already-buffered broadcast events (the transition + any
/// `emit_unblocked_tasks` follow-ups are emitted synchronously before the
/// awaited transition returns) and report whether a `task_updated` for
/// `task_id` was observed.
async fn wait_for_task_updated(
    events: &mut broadcast::Receiver<DjinnEventEnvelope>,
    task_id: &str,
) -> bool {
    loop {
        match events.try_recv() {
            Ok(env) => {
                if env.entity_type == "task"
                    && env.action == "updated"
                    && env
                        .payload
                        .get("task")
                        .and_then(|t| t.get("id"))
                        .and_then(|v| v.as_str())
                        == Some(task_id)
                {
                    return true;
                }
            }
            Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(_) => return false,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loop_guard_second_strike_parks_task() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.reset_intervention_counters(&task.id).await.unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(task.intervention_count, MAX_PLANNER_INTERVENTIONS);

    let handled = actor
        .route_loop_guard_planner_intervention(
            &task.id,
            "worker",
            "Reply-loop guard `identical_tool_failure` tripped: offending_signature=`shell:cargo-test`, threshold=3, observed=4, turn_span=7..=12",
        )
        .await;
    assert!(handled, "second-strike guard trip must be handled");

    let parked = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        parked.status, "open",
        "second-strike guard trip HOLDS the task (open + blocked) instead of force-closing it"
    );
    assert!(
        parked.close_reason.is_none(),
        "a held (parked) task must not carry a close_reason; got {:?}",
        parked.close_reason
    );

    // The hold creates a HUMAN-review remediation task and blocks the source on
    // it, so the source is held until a human resolves the remediation.
    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(
        blockers.len(),
        1,
        "second strike must block the source on a single human-review remediation task"
    );
    let remediation_id = blockers[0].task_id.clone();
    let remediation = repo.get(&remediation_id).await.unwrap().unwrap();
    assert_eq!(remediation.issue_type, "review");
    assert!(
        remediation.title.starts_with("Planner remediation ["),
        "remediation keeps the `Planner remediation [<short_id>]: <title>` convention; got {:?}",
        remediation.title
    );

    assert!(
        planner_intervention_markers(&repo, &task.id)
            .await
            .is_empty(),
        "second strike parks without writing a fresh marker"
    );

    // Closing the remediation revives the held source via emit_unblocked_tasks.
    let mut events = tx.subscribe();
    repo.transition(
        &remediation_id,
        djinn_core::models::TransitionAction::Close,
        "human",
        "user",
        None,
        None,
    )
    .await
    .unwrap();
    let revived = wait_for_task_updated(&mut events, &task.id).await;
    assert!(
        revived,
        "closing the human-review remediation must emit a TaskUpdated reviving the held source"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ci_loop_human_review_hold_excludes_source_from_ready_dispatch_tick() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = make_task_with_reopen_count(&db, &tx, 0).await;

    // Simulate a task whose draft PR has red, non-converging required CI.
    repo.set_status(&task.id, "pr_draft").await.unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(task.status, "pr_draft");

    actor
        .escalate_ci_failure_and_park(
            &task,
            "https://github.com/acme/repo/pull/7",
            "Required CI keeps failing on the same fingerprint; the worker is not converging.",
            &["**Failed job:** server-clippy (failure)".to_string()],
        )
        .await;

    // The source is PARKED: `open` (NOT closed, NOT pr_draft) and held by a
    // remediation blocker, so `list_ready` filters it out — no slot consumed.
    let parked = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        parked.status, "open",
        "CI-loop park leaves the source open, not closed/pr_draft"
    );
    assert_eq!(
        parked.close_reason, None,
        "a parked (held) source must not carry a close_reason"
    );
    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(
        blockers.len(),
        1,
        "CI-loop park must hold the source on a single remediation blocker"
    );
    let remediation_id = blockers[0].task_id.clone();
    let remediation = repo.get(&remediation_id).await.unwrap().unwrap();
    assert_eq!(remediation.issue_type, "review");
    assert!(
        remediation.labels.contains("human-review-hold"),
        "human-review remediation must carry the dispatch-hold label; labels={}",
        remediation.labels
    );
    assert!(
        remediation.title.starts_with("Planner remediation ["),
        "remediation keeps the `Planner remediation [<short_id>]: <title>` convention; got {:?}",
        remediation.title
    );

    let ready = repo
        .list_ready(djinn_db::ReadyQuery {
            project_id: Some(task.project_id.clone()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        !ready.iter().any(|candidate| candidate.id == task.id),
        "same list_ready path used by dispatch must exclude the source while the human-review hold is open"
    );

    actor.dispatch_ready_tasks(Some(&task.project_id)).await;

    assert_eq!(
        actor.dispatched, 0,
        "ready-dispatch tick must not spawn a worker for the source while the human-review hold is open"
    );
    assert!(
        !actor.last_dispatched.contains_key(&task.id),
        "dispatch tick must not record a worker dispatch marker for the held source"
    );
    let still_parked = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        still_parked.status, "open",
        "dispatch tick must leave the held source open, not transition it for worker execution"
    );
    let active_sessions =
        djinn_db::SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .list_active()
            .await
            .unwrap();
    assert!(
        !active_sessions
            .iter()
            .any(|session| session.task_id.as_deref() == Some(task.id.as_str())),
        "dispatch tick must not create an active worker session for the held source"
    );

    // Closing the remediation revives the held source via emit_unblocked_tasks.
    let mut events = tx.subscribe();
    repo.transition(
        &remediation_id,
        djinn_core::models::TransitionAction::Close,
        "human",
        "user",
        None,
        None,
    )
    .await
    .unwrap();
    assert!(
        wait_for_task_updated(&mut events, &task.id).await,
        "closing the CI-loop remediation must emit a TaskUpdated reviving the parked source"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_park_governance_does_not_route_trigger_b_or_touch_breaker_state() {
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task_id = "budget-parked-task".to_string();

    actor
        .dispatch_failure_streak
        .insert(task_id.clone(), MAX_DISPATCH_FAILURES - 1);
    actor.last_dispatched.insert(
        task_id.clone(),
        DispatchMarker {
            instant: StdInstant::now(),
            role: "worker".into(),
        },
    );
    let breaker_available_before = actor.health.is_available(None, DEFAULT_MODEL_ID);

    for (wind_down_ignored, continuation_count, expected) in [
        (false, 0, RunDisposition::Nudge),
        (true, 1, RunDisposition::Nudge),
        (true, NUDGE_CAP, RunDisposition::Close),
    ] {
        assert_eq!(
            decide_run_disposition(RunProgress::NoOp, continuation_count, NUDGE_CAP),
            expected,
            "budget park wind_down_ignored={wind_down_ignored} must stay on the continuation_count/NUDGE_CAP ladder"
        );
    }

    actor
        .clear_planned_dispatch_completion(&task_id, "budget_park_test_clear")
        .await;

    assert_eq!(
        actor.dispatch_failure_streak.get(&task_id).copied(),
        None,
        "budget-park completion clears stale streak state rather than incrementing toward MAX_DISPATCH_FAILURES"
    );
    assert!(
        !actor.last_dispatched.contains_key(&task_id),
        "budget-park completion clears same-role failure attribution before continuation dispatch"
    );
    assert_eq!(
        actor.health.is_available(None, DEFAULT_MODEL_ID),
        breaker_available_before,
        "budget parks must not alter model health/breaker availability"
    );
    assert!(
        actor.health.take_task_provider_failure(&task_id).is_none(),
        "budget parks must not seed provider-failure side-channel state"
    );
    assert!(
        !actor.dispatch_cooldowns.contains_key(&task_id),
        "budget parks must not create dispatch-failure cooldown state"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_budget_park_sessions_clear_recovery_backoff_without_fault_routing() {
    let db = Database::open_in_memory().unwrap();
    let (tx, _rx) = broadcast::channel(256);

    for (wind_down_ignored, label) in [
        (false, "summary-budget-park"),
        (true, "ignored-wind-down-budget-park"),
    ] {
        let task_id = format!("task-{label}");
        let session = djinn_core::models::SessionRecord {
            id: format!("session-{label}"),
            project_id: Some(format!("project-{label}")),
            task_id: Some(task_id.clone()),
            model_id: DEFAULT_MODEL_ID.to_owned(),
            agent_type: "worker".to_owned(),
            started_at: "2026-06-15T00:00:00.000Z".to_owned(),
            ended_at: Some("2026-06-15T00:05:00.000Z".to_owned()),
            status: SessionStatus::Completed.as_str().to_owned(),
            tokens_in: 100,
            tokens_out: 50,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            task_run_id: Some(format!("run-{label}")),
            title: None,
            parked_reason: Some("budget".to_owned()),
            cost_usd: None,
            input_price_per_million_snapshot: None,
            output_price_per_million_snapshot: None,
            cache_read_price_per_million_snapshot: None,
            cache_write_price_per_million_snapshot: None,
            cost_basis: "unpriced".to_owned(),
            billing_source: None,
        };
        assert_eq!(session.status, SessionStatus::Completed.as_str());
        assert_eq!(session.parked_reason.as_deref(), Some("budget"));

        let mut actor = coordinator_actor_for_tests(&db, &tx);
        actor
            .dispatch_failure_streak
            .insert(task_id.clone(), MAX_DISPATCH_FAILURES - 1);
        actor.dispatch_cooldowns.insert(
            task_id.clone(),
            StdInstant::now() + std::time::Duration::from_secs(300),
        );
        actor.last_dispatched.insert(
            task_id.clone(),
            DispatchMarker {
                instant: StdInstant::now(),
                role: "worker".into(),
            },
        );
        let breaker_available_before = actor.health.is_available(None, DEFAULT_MODEL_ID);

        actor
            .clear_planned_dispatch_completion(
                &task_id,
                if wind_down_ignored {
                    "budget_park_ignored_wind_down_completion"
                } else {
                    "budget_park_summary_completion"
                },
            )
            .await;

        assert!(
            !actor.dispatch_failure_streak.contains_key(&task_id),
            "parked_reason=budget wind_down_ignored={wind_down_ignored} must clear stale failure streak, not advance toward MAX_DISPATCH_FAILURES"
        );
        assert!(
            !actor.dispatch_cooldowns.contains_key(&task_id),
            "parked_reason=budget wind_down_ignored={wind_down_ignored} must not leave dispatch-failure cooldown state"
        );
        assert!(
            !actor.last_dispatched.contains_key(&task_id),
            "parked_reason=budget wind_down_ignored={wind_down_ignored} must clear same-role attribution before continuation dispatch"
        );
        assert_eq!(
            actor.health.is_available(None, DEFAULT_MODEL_ID),
            breaker_available_before,
            "parked_reason=budget wind_down_ignored={wind_down_ignored} must not trip provider/model breaker state"
        );
        assert!(
            actor.health.take_task_provider_failure(&task_id).is_none(),
            "budget parks must not seed typed provider-failure side-channel state"
        );
    }
}

#[test]
fn budget_park_source_paths_do_not_enter_dispatch_fault_routing() {
    let manifest_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let guarded_paths = [
        "src/dispatch/task_dispatch.rs",
        "src/dispatch/wave_dispatch.rs",
        "src/dispatch/session_recovery.rs",
        "src/dispatch/retry.rs",
    ];

    let mut offenders = Vec::new();
    for relative in guarded_paths {
        let path = manifest_dir.join(relative);
        let source = std::fs::read_to_string(&path).expect("read coordinator dispatch source");
        if source.contains("TaskRunOutcome::Parked")
            || source.contains("StageOutcome::Parked")
            || source.contains("parked_reason")
        {
            offenders.push(relative);
        }
    }

    assert!(
        offenders.is_empty(),
        "budget parks are planned lifecycle endings; coordinator dispatch fault/routing paths must not special-case them as failures: {offenders:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn at_threshold_routes_to_planner_intervention() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD).await;

    let intervened = actor.maybe_intervene_on_stuck_task(&task).await;
    assert!(
        intervened,
        "at threshold must route to planner intervention"
    );

    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    // Exactly one intervention marker, keyed to the current reopen count.
    let markers = planner_intervention_markers(&repo, &task.id).await;
    assert_eq!(markers.len(), 1, "exactly one intervention marker");
    assert_eq!(markers[0]["reopen_count"], REOPEN_INTERVENTION_THRESHOLD);

    // A Planner review task was created in the same project.
    let reviews = repo.list_by_status("open").await.unwrap();
    assert!(
        reviews
            .iter()
            .any(|t| t.issue_type == "review" && t.project_id == task.project_id),
        "a review (planner intervention) task must be created"
    );

    // The source task carries a PLANNER_ESCALATION comment linking it.
    let comments = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("comment".to_string()),
            actor_role: None,
            project_id: None,
            from_time: None,
            to_time: None,
            limit: 100,
            offset: 0,
        })
        .await
        .unwrap();
    assert!(
        comments
            .iter()
            .any(|c| c.payload.contains("PLANNER_ESCALATION")),
        "source task must record a PLANNER_ESCALATION comment"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intervention_is_idempotent_per_reopen_count() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    // First pass intervenes; subsequent passes at the SAME reopen count are
    // suppressed by the marker — no Planner storm while one is in flight.
    assert!(actor.maybe_intervene_on_stuck_task(&task).await);
    assert!(!actor.maybe_intervene_on_stuck_task(&task).await);
    assert!(!actor.maybe_intervene_on_stuck_task(&task).await);

    assert_eq!(
        planner_intervention_markers(&repo, &task.id).await.len(),
        1,
        "idempotent: a single marker for one reopen-count value"
    );

    // A genuine new reopen (count bumps past threshold again) re-arms one
    // fresh intervention.
    repo.set_status(&task.id, "closed").await.unwrap();
    let bumped = repo.set_status(&task.id, "open").await.unwrap();
    assert_eq!(bumped.reopen_count, REOPEN_INTERVENTION_THRESHOLD + 1);

    assert!(
        actor.maybe_intervene_on_stuck_task(&bumped).await,
        "a higher reopen count must re-arm intervention"
    );
    assert_eq!(
        planner_intervention_markers(&repo, &task.id).await.len(),
        2,
        "one marker per distinct reopen-count value"
    );
}

/// Second strike: once the Planner has already intervened
/// (`intervention_count >= MAX_PLANNER_INTERVENTIONS`) and the task has
/// STILL climbed back to the reopen threshold, the coordinator HOLDS it on a
/// human instead of escalating to the Planner again — it stays `open` (never
/// auto-closed), blocked on a freshly created human-review remediation task,
/// and writes no new intervention marker. This is the loop breaker for the
/// txr4 case (rescope didn't help → stop hogging the slot), now revivable when
/// a human resolves the remediation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_strike_parks_task_after_prior_intervention() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    // Reach the threshold once, simulate a completed planner intervention
    // (bumps intervention_count, resets reopen_count), then climb back to the
    // threshold a second time.
    let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD).await;
    repo.reset_intervention_counters(&task.id).await.unwrap();
    for _ in 0..REOPEN_INTERVENTION_THRESHOLD {
        repo.set_status(&task.id, "closed").await.unwrap();
        repo.set_status(&task.id, "open").await.unwrap();
    }
    let task = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(task.intervention_count, 1, "one prior planner intervention");
    assert_eq!(task.reopen_count, REOPEN_INTERVENTION_THRESHOLD);

    let handled = actor.maybe_intervene_on_stuck_task(&task).await;
    assert!(
        handled,
        "second strike must handle the task (caller skips worker dispatch)"
    );

    // Held on a human — task stays open, never auto-closed.
    let parked = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        parked.status, "open",
        "second strike HOLDS the task open (blocked) instead of force-closing it"
    );
    assert_eq!(
        parked.close_reason, None,
        "a held (parked) task must not carry a close_reason"
    );

    // No planner intervention marker for this reopen count — the rework loop is
    // broken (not re-escalated to the planner). The hold instead creates a
    // single HUMAN-review remediation task that blocks the source.
    assert!(
        !planner_intervention_markers(&repo, &task.id)
            .await
            .iter()
            .any(|m| m["reopen_count"] == REOPEN_INTERVENTION_THRESHOLD),
        "second strike must not write a new planner intervention marker"
    );
    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(
        blockers.len(),
        1,
        "second strike holds the source on exactly one human-review remediation blocker"
    );
    let remediation = repo.get(&blockers[0].task_id).await.unwrap().unwrap();
    assert_eq!(remediation.issue_type, "review");
    assert!(
        remediation.title.starts_with("Planner remediation ["),
        "remediation keeps the `Planner remediation [<short_id>]: <title>` convention; got {:?}",
        remediation.title
    );
}

// ── CI-enriched planner escalation regression tests ──────────────────────
//
// These tests verify that CI failure sections are properly threaded through
// escalation paths — the core wiring added by epic nnij.

/// `escalate_ci_failure_and_park` includes CI failure sections in both the
/// visibility comment and the escalation reason passed to
/// `create_remediation_task`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escalate_ci_failure_includes_sections_in_comment_and_reason() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let reason = "PR #42 stuck: required checks (build) failed across 5 rounds.";
    let sections = vec![
        "**Workflow:** CI".to_string(),
        "**Failed job:** build (failure)".to_string(),
        "**Failed step:** Run tests (step #3, failure)".to_string(),
        "Job URL: https://github.com/owner/repo/actions/runs/123/jobs/456".to_string(),
    ];
    let pr_url = "https://github.com/owner/repo/pull/42";

    actor
        .escalate_ci_failure_and_park(&task, pr_url, reason, &sections)
        .await;

    // The visibility comment must contain "**CI Failure Details:**" and each
    // section line so humans / the Planner see the real CI context.
    let comments = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("comment".to_string()),
            actor_role: None,
            project_id: None,
            from_time: None,
            to_time: None,
            limit: 100,
            offset: 0,
        })
        .await
        .unwrap();

    let escalation_comment = comments
        .iter()
        .find(|c| c.payload.contains("**PR CI Escalation**"))
        .expect("escalate_ci_failure_and_park must log a PR CI Escalation comment");
    assert!(
        escalation_comment
            .payload
            .contains("**CI Failure Details:**"),
        "visibility comment must include CI Failure Details header; got: {}",
        escalation_comment.payload
    );
    assert!(
        escalation_comment.payload.contains("**Workflow:** CI"),
        "visibility comment must include workflow section; got: {}",
        escalation_comment.payload
    );
    assert!(
        escalation_comment
            .payload
            .contains("**Failed job:** build (failure)"),
        "visibility comment must include failed-job section; got: {}",
        escalation_comment.payload
    );
    assert!(
        escalation_comment
            .payload
            .contains("Job URL: https://github.com/owner/repo/actions/runs/123/jobs/456"),
        "visibility comment must include job URL section; got: {}",
        escalation_comment.payload
    );

    // The escalation reason passed to create_remediation_task also
    // includes the sections (visible via the HUMAN_REVIEW_HOLD comment that
    // create_remediation_task logs on the source task).
    let planner_comments: Vec<_> = comments
        .iter()
        .filter(|c| c.payload.contains("HUMAN_REVIEW_HOLD"))
        .collect();
    assert!(
        !planner_comments.is_empty(),
        "HUMAN_REVIEW_HOLD comment must be logged"
    );
    let planner_comment = &planner_comments[0];
    assert!(
        planner_comment.payload.contains("**CI Failure Details:**"),
        "escalation reason must include CI Failure Details header; got: {}",
        planner_comment.payload
    );
    assert!(
        planner_comment.payload.contains("**Workflow:** CI"),
        "escalation reason must include workflow section; got: {}",
        planner_comment.payload
    );
}

/// `escalate_ci_failure_and_park` with empty sections omits the CI Failure
/// Details block from both the visibility comment and the escalation reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn escalate_ci_failure_with_empty_sections_omits_details() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let reason = "PR #42 stuck: required checks (build) failed.";
    let sections: Vec<String> = vec![];
    let pr_url = "https://github.com/owner/repo/pull/42";

    actor
        .escalate_ci_failure_and_park(&task, pr_url, reason, &sections)
        .await;

    let comments = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("comment".to_string()),
            actor_role: None,
            project_id: None,
            from_time: None,
            to_time: None,
            limit: 100,
            offset: 0,
        })
        .await
        .unwrap();

    let escalation_comment = comments
        .iter()
        .find(|c| c.payload.contains("**PR CI Escalation**"))
        .expect("PR CI Escalation comment must still be logged");
    assert!(
        !escalation_comment
            .payload
            .contains("**CI Failure Details:**"),
        "visibility comment must NOT include CI Failure Details when sections are empty; got: {}",
        escalation_comment.payload
    );

    let planner_comments: Vec<_> = comments
        .iter()
        .filter(|c| c.payload.contains("HUMAN_REVIEW_HOLD"))
        .collect();
    assert!(
        !planner_comments.is_empty(),
        "HUMAN_REVIEW_HOLD comment must be logged"
    );
    assert!(
        !planner_comments[0]
            .payload
            .contains("**CI Failure Details:**"),
        "escalation reason must NOT include CI Failure Details when sections are empty; got: {}",
        planner_comments[0].payload
    );
}

/// `route_planner_intervention` appends CI failure sections to the escalation
/// reason when `ci_failure_sections` is `Some(...)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_planner_intervention_appends_ci_failure_sections() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let reason = "Internal review loop exceeded threshold.";
    let sections = "**Workflow:** CI\n**Failed job:** test (failure)";

    let handled = actor
        .route_planner_intervention(&task, "worker", reason, Some(sections), task.reopen_count)
        .await;
    assert!(handled, "route_planner_intervention must handle the task");

    // The PLANNER_ESCALATION comment logged by dispatch_planner_escalation
    // should contain the CI failure sections in the reason.
    let comments = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("comment".to_string()),
            actor_role: None,
            project_id: None,
            from_time: None,
            to_time: None,
            limit: 100,
            offset: 0,
        })
        .await
        .unwrap();

    let planner_comments: Vec<_> = comments
        .iter()
        .filter(|c| c.payload.contains("PLANNER_ESCALATION"))
        .collect();
    assert!(
        !planner_comments.is_empty(),
        "PLANNER_ESCALATION comment must be logged"
    );
    let planner_comment = &planner_comments[0];
    assert!(
        planner_comment.payload.contains("**CI Failure Details:**"),
        "escalation reason must include CI Failure Details header when sections provided; got: {}",
        planner_comment.payload
    );
    assert!(
        planner_comment.payload.contains("**Workflow:** CI"),
        "escalation reason must include workflow section; got: {}",
        planner_comment.payload
    );
    assert!(
        planner_comment
            .payload
            .contains("**Failed job:** test (failure)"),
        "escalation reason must include failed-job section; got: {}",
        planner_comment.payload
    );
}

/// `route_planner_intervention` passes the original reason unchanged when
/// `ci_failure_sections` is `None`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_planner_intervention_with_none_sections_preserves_reason() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, REOPEN_INTERVENTION_THRESHOLD).await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    let reason = "Internal review loop exceeded threshold.";

    let handled = actor
        .route_planner_intervention(&task, "worker", reason, None, task.reopen_count)
        .await;
    assert!(handled, "route_planner_intervention must handle the task");

    let comments = repo
        .query_activity(ActivityQuery {
            task_id: Some(task.id.clone()),
            event_type: Some("comment".to_string()),
            actor_role: None,
            project_id: None,
            from_time: None,
            to_time: None,
            limit: 100,
            offset: 0,
        })
        .await
        .unwrap();

    let planner_comments: Vec<_> = comments
        .iter()
        .filter(|c| c.payload.contains("PLANNER_ESCALATION"))
        .collect();
    assert!(
        !planner_comments.is_empty(),
        "PLANNER_ESCALATION comment must be logged"
    );
    assert!(
        !planner_comments[0]
            .payload
            .contains("**CI Failure Details:**"),
        "escalation reason must NOT include CI Failure Details when sections are None; got: {}",
        planner_comments[0].payload
    );
    // The original reason should appear verbatim in the escalation.
    assert!(
        planner_comments[0].payload.contains(reason),
        "escalation reason must contain the original reason text; got: {}",
        planner_comments[0].payload
    );
}

/// End-to-end regression (pdn6 release-side): the full lifecycle through the
/// coordinator proves that closing the human-review hold task releases the
/// parked source back into the dispatch readiness query — and that a normal
/// (non-review) blocker has identical semantics.
///
/// This test exercises the real coordinator path (`route_loop_guard_planner_intervention`)
/// and then queries `list_ready` — the same dispatch readiness query the
/// coordinator's dispatch tick uses — to prove:
/// 1. Source is NOT ready while the hold task is open.
/// 2. Closing the hold restores readiness.
/// 3. A normal (non-review) blocker also blocks and releases identically.
/// 4. `emit_unblocked_tasks` fires a `TaskUpdated` event for the source on release.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn review_hold_release_lifecycle_proves_dispatch_readiness_recovery() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    // ── Phase 1: Create source task and trigger the hold via the coordinator ──
    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.reset_intervention_counters(&task.id).await.unwrap();
    // Drive reopen count to the threshold so intervention fires.
    let task = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(task.intervention_count, MAX_PLANNER_INTERVENTIONS);

    // Route to the loop guard second-strike path (the actual pdn6 hold mechanism).
    let handled = actor
        .route_loop_guard_planner_intervention(
            &task.id,
            "worker",
            "Reply-loop guard `identical_tool_failure` tripped: offending_signature=`shell:cargo-test`, threshold=3, observed=4, turn_span=7..=12",
        )
        .await;
    assert!(handled, "loop guard trip must be handled");

    // Source is open (parked, NOT closed) and blocked by the hold.
    let parked = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(parked.status, "open", "held source stays open");

    // The review hold task was created.
    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(
        blockers.len(),
        1,
        "source must have exactly one hold blocker"
    );
    let hold_id = blockers[0].task_id.clone();
    let hold = repo.get(&hold_id).await.unwrap().unwrap();
    assert_eq!(hold.issue_type, "review", "hold must be review-type");

    // ── Phase 2: Assert the source is NOT in dispatch readiness ──
    let ready = repo
        .list_ready(ReadyQuery {
            project_id: Some(parked.project_id.clone()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        !ready.iter().any(|t| t.id == task.id),
        "source must NOT be in list_ready while the review hold is open"
    );

    // ── Phase 3: Close the hold — source must reappear in list_ready ──
    let mut events = tx.subscribe();
    repo.transition(
        &hold_id,
        djinn_core::models::TransitionAction::Close,
        "human",
        "user",
        None,
        None,
    )
    .await
    .unwrap();

    // The TaskUpdated event for the source must fire (from emit_unblocked_tasks).
    let revived = wait_for_task_updated(&mut events, &task.id).await;
    assert!(
        revived,
        "closing the review hold must emit TaskUpdated for the source via emit_unblocked_tasks"
    );

    // The dispatch readiness query must now return the source.
    let ready = repo
        .list_ready(ReadyQuery {
            project_id: Some(parked.project_id.clone()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        ready.iter().any(|t| t.id == task.id),
        "source must be in list_ready after the review hold is closed"
    );

    // ── Phase 4: Prove a normal (non-review) blocker has identical semantics ──
    // Add a normal blocker to the same source.
    let normal_blocker = repo
        .create_in_project(
            &parked.project_id,
            parked.epic_id.as_deref(),
            "Normal blocker task",
            "",
            "",
            "task",
            0,
            "system",
            Some("open"),
            None,
        )
        .await
        .unwrap();
    repo.add_blocker(&task.id, &normal_blocker.id)
        .await
        .unwrap();

    // Source is NOT ready with the normal blocker.
    let ready = repo
        .list_ready(ReadyQuery {
            project_id: Some(parked.project_id.clone()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        !ready.iter().any(|t| t.id == task.id),
        "source must NOT be ready while a normal (non-review) blocker is open"
    );

    // Close the normal blocker — source is ready again.
    // Use set_status (bypasses state machine) because a full-lifecycle `task`
    // cannot be Close'd from `open`; we only need the blocker to be resolved
    // for the readiness query. The review hold release via `transition(Close)`
    // above already proved the emit_unblocked_tasks path.
    repo.set_status(&normal_blocker.id, "closed").await.unwrap();

    let ready = repo
        .list_ready(ReadyQuery {
            project_id: Some(parked.project_id.clone()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        ready.iter().any(|t| t.id == task.id),
        "source must be ready after the normal blocker is closed (identical semantics to review hold)"
    );
}

/// End-to-end regression (pdn6 release-side): prove that closing a normal
/// (non-review) blocker via `transition(Close)` — the same path used for
/// the review hold — fires `emit_unblocked_tasks` and releases the blocked
/// source back into dispatch readiness.
///
/// The `review_hold_release_lifecycle_proves_dispatch_readiness_recovery`
/// test above proves the `emit_unblocked_tasks` event path for review-type
/// holds but uses `set_status` for the normal blocker (bypassing the event
/// path). This test fills that gap so any future predicate change that
/// special-cases `review` blockers or alters `emit_unblocked_tasks` for
/// task blockers is caught.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn normal_blocker_release_via_transition_fires_unblocked_event() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let project = test_helpers::create_test_project(&db).await;
    let epic = EpicRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .create_for_project(
            &project.id,
            djinn_db::EpicCreateInput {
                title: "Release lifecycle epic",
                description: "",
                emoji: "",
                color: "",
                owner: "",
                memory_refs: None,
                status: None,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .unwrap();

    // Source task — a normal full-lifecycle work item.
    let source = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Source task",
            "desc",
            "",
            "task",
            0,
            "",
            Some("open"),
            Some(r#"[{"title":"ac1"}]"#),
        )
        .await
        .unwrap();

    // Normal blocker — `spike` uses simple lifecycle (open → in_progress →
    // closed) so `transition(Close)` from `open` is a valid state-machine
    // move. This mirrors a real non-review blocker task.
    let normal_blocker = repo
        .create_in_project(
            &project.id,
            Some(&epic.id),
            "Dependency spike",
            "",
            "",
            "spike",
            0,
            "system",
            Some("open"),
            None,
        )
        .await
        .unwrap();

    // Wire the blocker edge: source is blocked by the normal blocker.
    repo.add_blocker(&source.id, &normal_blocker.id)
        .await
        .unwrap();

    // ── Pre-condition: source is NOT ready while the blocker is open ──
    let ready = repo
        .list_ready(ReadyQuery {
            project_id: Some(project.id.clone()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        !ready.iter().any(|t| t.id == source.id),
        "source must NOT be ready while the normal blocker is open"
    );

    // Subscribe to events before closing the blocker.
    let mut events = tx.subscribe();

    // Close the blocker via `transition(Close)` — the same path used for
    // the review hold. This calls `emit_unblocked_tasks` internally, which
    // must fire a `TaskUpdated` for the now-unblocked source.
    repo.transition(
        &normal_blocker.id,
        djinn_core::models::TransitionAction::Close,
        "system",
        "coordinator",
        None,
        None,
    )
    .await
    .unwrap();

    // ── emit_unblocked_tasks must fire a TaskUpdated event for the source ──
    let released = wait_for_task_updated(&mut events, &source.id).await;
    assert!(
        released,
        "closing a normal blocker via transition(Close) must emit TaskUpdated \
         for the blocked source via emit_unblocked_tasks (identical to review hold)"
    );

    // ── The dispatch readiness query must now return the source ──
    let ready = repo
        .list_ready(ReadyQuery {
            project_id: Some(project.id.clone()),
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    assert!(
        ready.iter().any(|t| t.id == source.id),
        "source must be ready after the normal blocker is closed via transition(Close)"
    );
}

// ── Trigger D: provider-error failure-streak escalation ───────────────────────

/// The escalation thresholds form one family: the stall-cancel second strike
/// (2) and the provider-error failure strike (3) are distinct, and the failure
/// threshold sits one rung higher so the cooldown ladder absorbs a transient
/// provider blip at streaks 1-2 before the Planner is involved.
#[test]
fn failure_and_stall_escalation_thresholds_are_distinct() {
    assert_eq!(
        STALL_CANCEL_ESCALATION_THRESHOLD, 2,
        "stall-cancel second-strike threshold is unchanged (PR #1429)"
    );
    assert_eq!(
        FAILURE_ESCALATION_THRESHOLD, 3,
        "provider-error failure escalation fires on the third consecutive strike"
    );
    const {
        assert!(
            FAILURE_ESCALATION_THRESHOLD > STALL_CANCEL_ESCALATION_THRESHOLD,
            "failure streak escalates one rung later than the stall streak"
        )
    };
}

/// Three consecutive provider-error FAILED sessions without durable status
/// progress route the task to a Planner intervention (the second-strike PARK
/// path here, since the task is already at MAX_PLANNER_INTERVENTIONS) instead
/// of another backoff+redispatch cycle. The first two strikes only advance the
/// streak; the third escalates and clears the task's backoff state.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn third_provider_failure_without_progress_routes_to_planner() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = make_task_with_reopen_count(&db, &tx, 0).await;
    repo.reset_intervention_counters(&task.id).await.unwrap();
    let task = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(task.intervention_count, MAX_PLANNER_INTERVENTIONS);

    // Seed stale backoff state to prove the escalation clears it.
    actor
        .dispatch_failure_streak
        .insert(task.id.clone(), MAX_DISPATCH_FAILURES - 1);
    actor.dispatch_cooldowns.insert(
        task.id.clone(),
        StdInstant::now() + std::time::Duration::from_secs(300),
    );

    // Strike 1 and 2: below threshold, no routing, streak advances.
    for expected_count in 1..FAILURE_ESCALATION_THRESHOLD {
        let routed = actor
            .maybe_escalate_provider_failure_streak(&task, "worker")
            .await;
        assert!(
            !routed,
            "strike {expected_count} is below FAILURE_ESCALATION_THRESHOLD and must not escalate"
        );
        assert_eq!(
            actor.provider_failure_streak.get(&task.id).map(|s| s.count),
            Some(expected_count),
            "streak advances by one per consecutive failure"
        );
    }

    // Strike 3: escalates.
    let routed = actor
        .maybe_escalate_provider_failure_streak(&task, "worker")
        .await;
    assert!(
        routed,
        "the third consecutive provider-error failure routes to a Planner intervention"
    );

    // The task is HELD (parked open + blocked on a human-review remediation),
    // the second-strike behavior of the shared loop-guard machinery.
    let parked = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        parked.status, "open",
        "escalated task is parked, not redispatched"
    );
    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(
        blockers.len(),
        1,
        "escalation blocks the task on a single human-review remediation"
    );

    // Escalation clears the streak and the stale backoff state.
    assert!(
        !actor.provider_failure_streak.contains_key(&task.id),
        "the streak is cleared on escalation so a post-intervention run starts fresh"
    );
    assert!(
        !actor.dispatch_failure_streak.contains_key(&task.id),
        "escalation clears the terminal dispatch-failure streak"
    );
    assert!(
        !actor.dispatch_cooldowns.contains_key(&task.id),
        "escalation clears the dispatch cooldown"
    );
}

/// Durable task-status progress between strikes resets the failure streak, so a
/// task that keeps advancing never escalates.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn status_progress_resets_provider_failure_streak() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, 0).await;

    // Two consecutive failures at the same status → streak reaches 2.
    for _ in 0..2 {
        assert!(
            !actor
                .maybe_escalate_provider_failure_streak(&task, "worker")
                .await
        );
    }
    assert_eq!(
        actor.provider_failure_streak.get(&task.id).map(|s| s.count),
        Some(2),
        "two same-status failures accumulate a streak of two"
    );

    // The task then makes durable progress (status advances). The next failure
    // observes a different status and resets the streak to one — no escalation.
    let mut progressed = task.clone();
    progressed.status = "in_progress".to_string();
    let routed = actor
        .maybe_escalate_provider_failure_streak(&progressed, "worker")
        .await;
    assert!(!routed, "a failure after status progress must not escalate");
    assert_eq!(
        actor.provider_failure_streak.get(&task.id).map(|s| s.count),
        Some(1),
        "durable status progress between strikes resets the streak to one"
    );
}

/// A successful settlement (`clear_planned_dispatch_completion`) drops the
/// provider-error failure streak so a recovered task starts fresh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn planned_completion_clears_provider_failure_streak() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let task = make_task_with_reopen_count(&db, &tx, 0).await;

    assert!(
        !actor
            .maybe_escalate_provider_failure_streak(&task, "worker")
            .await
    );
    assert!(actor.provider_failure_streak.contains_key(&task.id));

    actor
        .clear_planned_dispatch_completion(&task.id, "test_planned_completion_clear")
        .await;
    assert!(
        !actor.provider_failure_streak.contains_key(&task.id),
        "a planned completion clears the provider-error failure streak"
    );
}

// ── Quality-strike intervention tests (886z) ────────────────────────────────
//
// These tests verify that `maybe_intervene_on_stuck_task` gates on
// DB-backed `quality_reopen_count` rather than raw `task.reopen_count`,
// and that excluded-class reopens (merge_conflict, superseded) do not
// arm planner interventions while quality classes still trigger at the
// configured threshold.

/// Walk a task through a full review cycle with a specific rejection action.
/// Starts from `open`, moves through the state machine to `in_task_review`,
/// then applies the given rejection action (returning the task to `open`).
async fn walk_review_reject_cycle(
    repo: &TaskRepository,
    task_id: &str,
    action: djinn_core::models::TransitionAction,
    reason: &str,
) {
    repo.transition(
        task_id,
        djinn_core::models::TransitionAction::Start,
        "worker",
        "worker",
        None,
        None,
    )
    .await
    .unwrap();
    repo.transition(
        task_id,
        djinn_core::models::TransitionAction::SubmitTaskReview,
        "worker",
        "worker",
        None,
        None,
    )
    .await
    .unwrap();
    repo.transition(
        task_id,
        djinn_core::models::TransitionAction::TaskReviewStart,
        "reviewer",
        "reviewer",
        None,
        None,
    )
    .await
    .unwrap();
    repo.transition(task_id, action, "reviewer", "reviewer", Some(reason), None)
        .await
        .unwrap();
}

/// Below-threshold quality reopens must NOT trigger intervention.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quality_strikes_below_threshold_does_not_intervene() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = make_task_with_reopen_count(&db, &tx, 0).await;

    // Drive quality reopens to just below the threshold.
    for _ in 0..(REOPEN_INTERVENTION_THRESHOLD - 1) {
        walk_review_reject_cycle(
            &repo,
            &task.id,
            djinn_core::models::TransitionAction::TaskReviewReject,
            "below threshold reject",
        )
        .await;
    }

    let t = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(t.reopen_count, REOPEN_INTERVENTION_THRESHOLD - 1);

    let quality = repo.quality_reopen_count(&task.id).await.unwrap();
    assert_eq!(quality, REOPEN_INTERVENTION_THRESHOLD - 1);

    let intervened = actor.maybe_intervene_on_stuck_task(&t).await;
    assert!(
        !intervened,
        "quality_strikes below threshold must not trigger intervention"
    );
    assert!(
        planner_intervention_markers(&repo, &task.id)
            .await
            .is_empty(),
        "no marker should be written below threshold"
    );
}

/// At-threshold quality reopens MUST trigger intervention.
/// Marker stores both `quality_strikes` and raw `reopen_count`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quality_strikes_at_threshold_triggers_intervention() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = make_task_with_reopen_count(&db, &tx, 0).await;

    // Drive quality reopens to the threshold.
    for _ in 0..REOPEN_INTERVENTION_THRESHOLD {
        walk_review_reject_cycle(
            &repo,
            &task.id,
            djinn_core::models::TransitionAction::TaskReviewReject,
            "at threshold reject",
        )
        .await;
    }

    let t = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(t.reopen_count, REOPEN_INTERVENTION_THRESHOLD);

    let quality = repo.quality_reopen_count(&task.id).await.unwrap();
    assert_eq!(quality, REOPEN_INTERVENTION_THRESHOLD);

    let intervened = actor.maybe_intervene_on_stuck_task(&t).await;
    assert!(
        intervened,
        "quality_strikes at threshold must trigger intervention"
    );

    // Marker stores both quality_strikes and reopen_count.
    let markers = planner_intervention_markers(&repo, &task.id).await;
    assert_eq!(markers.len(), 1, "exactly one intervention marker");
    assert_eq!(
        markers[0]["reopen_count"], REOPEN_INTERVENTION_THRESHOLD,
        "marker reopen_count = raw reopen_count"
    );
    assert_eq!(
        markers[0]["quality_strikes"], REOPEN_INTERVENTION_THRESHOLD,
        "marker quality_strikes = quality reopen count"
    );
}

/// Excluded-class reopens (merge_conflict) do not count toward the quality
/// threshold. Mixed quality + non-quality reopens reach intervention only
/// when the QUALITY count crosses the threshold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn excluded_class_reopens_do_not_arm_intervention() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = make_task_with_reopen_count(&db, &tx, 0).await;

    // Phase 1: add merge_conflict reopens. These are excluded from quality
    // count (reopen_class=merge_conflict, no raw reopen increment).
    for _ in 0..2 {
        walk_review_reject_cycle(
            &repo,
            &task.id,
            djinn_core::models::TransitionAction::TaskReviewRejectConflict,
            "merge_conflict:{}",
        )
        .await;
    }

    // Merge conflicts don't increment raw reopen_count OR quality count.
    let t = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        t.reopen_count, 0,
        "merge_conflict must not increment raw reopen_count"
    );
    let quality = repo.quality_reopen_count(&task.id).await.unwrap();
    assert_eq!(
        quality, 0,
        "merge_conflict must not count toward quality_reopen_count"
    );

    // No intervention at zero quality strikes.
    let intervened = actor.maybe_intervene_on_stuck_task(&t).await;
    assert!(
        !intervened,
        "merge_conflict reopens alone must not trigger intervention"
    );

    // Phase 2: add one quality reopen below threshold.
    walk_review_reject_cycle(
        &repo,
        &task.id,
        djinn_core::models::TransitionAction::TaskReviewReject,
        "quality reject",
    )
    .await;

    let t = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(t.reopen_count, 1, "one quality reopen increments raw count");
    let quality = repo.quality_reopen_count(&task.id).await.unwrap();
    assert_eq!(quality, 1, "one quality reopen counted");

    let intervened = actor.maybe_intervene_on_stuck_task(&t).await;
    assert!(
        !intervened,
        "quality_strikes=1 below threshold must not trigger intervention"
    );

    // Phase 3: drive quality count to threshold.
    for _ in 0..(REOPEN_INTERVENTION_THRESHOLD - 1) {
        walk_review_reject_cycle(
            &repo,
            &task.id,
            djinn_core::models::TransitionAction::TaskReviewReject,
            "quality reject",
        )
        .await;
    }

    let t = repo.get(&task.id).await.unwrap().unwrap();
    let quality = repo.quality_reopen_count(&task.id).await.unwrap();
    assert_eq!(
        quality, REOPEN_INTERVENTION_THRESHOLD,
        "quality count reaches threshold"
    );
    // Raw reopen_count equals quality count (merge_conflict doesn't increment).
    assert_eq!(t.reopen_count, quality, "raw count = quality count here");

    let intervened = actor.maybe_intervene_on_stuck_task(&t).await;
    assert!(
        intervened,
        "quality_strikes at threshold must trigger intervention despite prior merge_conflicts"
    );

    let markers = planner_intervention_markers(&repo, &task.id).await;
    assert_eq!(markers.len(), 1);
    assert_eq!(
        markers[0]["quality_strikes"], REOPEN_INTERVENTION_THRESHOLD,
        "marker records quality strike count"
    );
    assert_eq!(
        markers[0]["reopen_count"], REOPEN_INTERVENTION_THRESHOLD,
        "marker records raw reopen count"
    );
}

/// DB-backed quality_reopen_count diverges from raw reopen_count and
/// intervention fires on the QUALITY count, not the raw count.
///
/// This proves that `maybe_intervene_on_stuck_task` reads from the DB
/// and uses `quality_reopen_count` rather than `task.reopen_count`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn intervention_uses_db_quality_count_not_raw_reopen_count() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = make_task_with_reopen_count(&db, &tx, 0).await;

    // Drive 2 quality reopens (raw_count=2, quality=2).
    for _ in 0..2 {
        walk_review_reject_cycle(
            &repo,
            &task.id,
            djinn_core::models::TransitionAction::TaskReviewReject,
            "quality reject",
        )
        .await;
    }

    // Add 2 merge_conflict reopens (raw_count stays 2, quality stays 2 —
    // merge_conflict does not increment reopen_count and is excluded from
    // quality count).
    for _ in 0..2 {
        walk_review_reject_cycle(
            &repo,
            &task.id,
            djinn_core::models::TransitionAction::TaskReviewRejectConflict,
            "merge_conflict:{}",
        )
        .await;
    }

    let t = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(t.reopen_count, 2, "raw count = 2 (merge_conflict excluded)");
    let quality = repo.quality_reopen_count(&task.id).await.unwrap();
    assert_eq!(quality, 2, "quality count = 2");

    // Below threshold: no intervention.
    let intervened = actor.maybe_intervene_on_stuck_task(&t).await;
    assert!(!intervened, "quality=2 below threshold must not intervene");

    // Add one more quality reopen (raw=3, quality=3 = threshold).
    walk_review_reject_cycle(
        &repo,
        &task.id,
        djinn_core::models::TransitionAction::TaskReviewReject,
        "final quality reject",
    )
    .await;

    let t = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(t.reopen_count, 3, "raw count = 3");
    let quality = repo.quality_reopen_count(&task.id).await.unwrap();
    assert_eq!(quality, 3, "quality count = 3 = threshold");

    let intervened = actor.maybe_intervene_on_stuck_task(&t).await;
    assert!(
        intervened,
        "quality=3 at threshold triggers intervention (even with merge_conflict reopens)"
    );

    // Calling again at the same quality/raw count must not re-intervene because
    // the marker for this reopen-count value already exists.
    let t = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(t.reopen_count, 3, "raw count = 3");
    let quality = repo.quality_reopen_count(&task.id).await.unwrap();
    assert_eq!(quality, 3, "quality count stays at threshold");

    let intervened = actor.maybe_intervene_on_stuck_task(&t).await;
    assert!(
        !intervened,
        "no re-intervention when quality count hasn't changed and marker exists"
    );
}

// ── End-to-end mixed reopen quality-strike regression tests (wfui) ───────────

/// Walk a task to `pr_review` and apply `PrChangesRequested`.
async fn walk_pr_changes_requested_cycle(repo: &TaskRepository, task_id: &str) {
    repo.transition(task_id, TransitionAction::Start, "w", "worker", None, None)
        .await
        .unwrap();
    repo.transition(
        task_id,
        TransitionAction::SubmitTaskReview,
        "w",
        "worker",
        None,
        None,
    )
    .await
    .unwrap();
    repo.transition(
        task_id,
        TransitionAction::TaskReviewStart,
        "r",
        "reviewer",
        None,
        None,
    )
    .await
    .unwrap();
    repo.transition(
        task_id,
        TransitionAction::TaskReviewApprove,
        "r",
        "reviewer",
        None,
        None,
    )
    .await
    .unwrap();
    repo.transition(
        task_id,
        TransitionAction::PrCreated,
        "sys",
        "system",
        None,
        None,
    )
    .await
    .unwrap();
    repo.transition(
        task_id,
        TransitionAction::PrUndraft,
        "sys",
        "system",
        None,
        None,
    )
    .await
    .unwrap();
    repo.transition(
        task_id,
        TransitionAction::PrChangesRequested,
        "sys",
        "system",
        Some("changes requested on PR"),
        None,
    )
    .await
    .unwrap();
}

/// Walk a task to `pr_draft` and apply `PrCiFailed`.
async fn walk_pr_ci_failed_cycle(repo: &TaskRepository, task_id: &str) {
    repo.transition(task_id, TransitionAction::Start, "w", "worker", None, None)
        .await
        .unwrap();
    repo.transition(
        task_id,
        TransitionAction::SubmitTaskReview,
        "w",
        "worker",
        None,
        None,
    )
    .await
    .unwrap();
    repo.transition(
        task_id,
        TransitionAction::TaskReviewStart,
        "r",
        "reviewer",
        None,
        None,
    )
    .await
    .unwrap();
    repo.transition(
        task_id,
        TransitionAction::TaskReviewApprove,
        "r",
        "reviewer",
        None,
        None,
    )
    .await
    .unwrap();
    repo.transition(
        task_id,
        TransitionAction::PrCreated,
        "sys",
        "system",
        None,
        None,
    )
    .await
    .unwrap();
    repo.transition(
        task_id,
        TransitionAction::PrCiFailed,
        "sys",
        "system",
        None,
        None,
    )
    .await
    .unwrap();
}

/// Inject a synthetic `status_changed` activity row with an optional
/// `reopen_class` payload. Used to exercise historical rows and classes that
/// have no production transition.
async fn inject_reopen_activity(
    repo: &TaskRepository,
    task_id: &str,
    from_status: &str,
    to_status: &str,
    reopen_class: Option<&str>,
) {
    let payload = if let Some(class) = reopen_class {
        serde_json::json!({
            "from_status": from_status,
            "to_status": to_status,
            "reopen_class": class,
        })
    } else {
        serde_json::json!({
            "from_status": from_status,
            "to_status": to_status,
        })
    };
    repo.log_activity(
        Some(task_id),
        "test",
        "system",
        "status_changed",
        &payload.to_string(),
    )
    .await
    .unwrap();
}

/// End-to-end regression for typed reopen classification and quality-strike
/// aggregation. A mixed ledger (review_rejected, stale review_rejected,
/// PrChangesRequested, merge_queue_failed, merge_conflict, superseded,
/// historical other, and an unclassified raw-only reopen) yields
/// `raw_reopen_count > quality_reopen_count`. The second-strike human park
/// decision uses the DB-backed quality count after a fresh reload, and the park
/// telemetry emits the requested strike-class breakdown labels.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mixed_reopen_ledger_park_uses_quality_count_and_emits_telemetry_breakdown() {
    djinn_telemetry::init().unwrap();

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = make_task_with_reopen_count(&db, &tx, 0).await;

    // Pretend a prior planner intervention already happened: reset raw counters
    // and bump intervention_count so the next intervention routes to the
    // human-review park path.
    repo.reset_intervention_counters(&task.id).await.unwrap();

    // 1. Quality strike: plain review rejection.
    walk_review_reject_cycle(
        &repo,
        &task.id,
        TransitionAction::TaskReviewReject,
        "implementation rejected",
    )
    .await;

    // 2. Quality strike: stale review rejection.
    walk_review_reject_cycle(
        &repo,
        &task.id,
        TransitionAction::TaskReviewRejectStale,
        "stale context",
    )
    .await;

    // 3. Excluded class: conflict rejection does not increment raw reopen_count.
    walk_review_reject_cycle(
        &repo,
        &task.id,
        TransitionAction::TaskReviewRejectConflict,
        "merge_conflict:{\"head\":\"abc\"}",
    )
    .await;

    // 4. Quality strike: PR changes requested (review_rejected).
    walk_pr_changes_requested_cycle(&repo, &task.id).await;

    // 5. Quality strike: merge/verification failure (merge_queue_failed).
    walk_pr_ci_failed_cycle(&repo, &task.id).await;

    // 6. Excluded class: superseded reopen injected directly.
    inject_reopen_activity(&repo, &task.id, "pr_review", "open", Some("superseded")).await;

    // 7. Historical other: status_changed with no `reopen_class` field. It
    // defaults to `other` and counts as a quality strike.
    repo.set_status(&task.id, "closed").await.unwrap();
    repo.set_status(&task.id, "open").await.unwrap();

    // 8. Unclassified raw-only reopen: set_status from `in_progress` to `open`.
    // Increments `task.reopen_count` but is excluded from the ledger's
    // from_status allow-list, so it is not counted as a quality strike.
    repo.set_status(&task.id, "in_progress").await.unwrap();
    repo.set_status(&task.id, "open").await.unwrap();

    // Reload from DB and verify the mixed ledger divergence.
    let t = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        t.reopen_count, 6,
        "raw count includes six reopen increments (four quality + one historical + one unclassified)"
    );
    let quality = repo.quality_reopen_count(&task.id).await.unwrap();
    assert_eq!(
        quality, 5,
        "quality count excludes merge_conflict, superseded, and the unclassified in_progress->open transition"
    );
    assert!(
        t.reopen_count > quality,
        "raw_reopen_count must be greater than quality_reopen_count in this mixed ledger"
    );

    // The recent ledger contains the classified/historical rows; the raw-only
    // set_status row is excluded from its allow-list.
    let ledger = repo.recent_reopen_ledger(&task.id, 10).await.unwrap();
    assert_eq!(
        ledger.len(),
        7,
        "ledger contains classified and historical reopen rows, not the raw-only transition"
    );
    let quality_in_ledger = ledger
        .iter()
        .filter(|e| e.reopen_class.is_quality_strike())
        .count();
    assert_eq!(
        quality_in_ledger, 5,
        "ledger quality rows match quality_reopen_count"
    );
    let conflict_in_ledger = ledger
        .iter()
        .filter(|e| e.reopen_class == ReopenClass::MergeConflict)
        .count();
    assert_eq!(
        conflict_in_ledger, 1,
        "ledger contains one merge_conflict row"
    );
    let superseded_in_ledger = ledger
        .iter()
        .filter(|e| e.reopen_class == ReopenClass::Superseded)
        .count();
    assert_eq!(
        superseded_in_ledger, 1,
        "ledger contains one superseded row"
    );

    // The park guard uses the DB-backed quality count, not the in-memory raw
    // counter, so it fires even though the raw count is strictly above the
    // quality count.
    let intervened = actor.maybe_intervene_on_stuck_task(&t).await;
    assert!(
        intervened,
        "quality=5 above threshold must trigger second-strike park"
    );

    // Verify the source is held (open + blocked on a human-review remediation).
    let parked = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(parked.status, "open", "parked task stays open");
    assert!(
        parked.close_reason.is_none(),
        "parked task must not carry a close_reason"
    );
    let blockers = repo.list_blockers(&task.id).await.unwrap();
    assert_eq!(
        blockers.len(),
        1,
        "second-strike park must create a single human-review remediation blocker"
    );
    let remediation = repo.get(&blockers[0].task_id).await.unwrap().unwrap();
    assert_eq!(remediation.issue_type, "review");
    assert!(
        planner_intervention_markers(&repo, &task.id)
            .await
            .is_empty(),
        "second-strike park must not write a fresh planner intervention marker"
    );

    // Park telemetry emitted the requested breakdown labels.
    let rendered = djinn_telemetry::render().unwrap();
    let line = rendered
        .lines()
        .find(|l| {
            l.starts_with("djinn_tasks_parked_total")
                && l.contains("quality_strikes=\"5\"")
                && l.contains("merge_conflict_reopens=\"1\"")
                && l.contains("superseded_reopens=\"1\"")
                && l.contains("raw_reopen_count=\"6\"")
        })
        .expect("parked metric line with strike-class breakdown not found");
    let value: f64 = line
        .rsplit_once(' ')
        .and_then(|(_, v)| v.parse().ok())
        .expect("metric value parses");
    assert!(value >= 1.0, "parked counter should be >= 1.0, got {value}");
}
