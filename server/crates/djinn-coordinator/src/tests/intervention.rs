// djinn:allow-oversize — legacy test module over size-guard threshold; split when touched substantively.

use super::*;
use crate::supervisor_impl::disposition::{NUDGE_CAP, RunDisposition, decide_run_disposition};
use djinn_core::run_progress::RunProgress;
use djinn_core::{events::DjinnEventEnvelope, models::SessionStatus};
use djinn_db::{DispatchStateRepository, DispatchStateUpsert, UserRepository};

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
    harness
        .assert_task_status("closed", Some("planner intervention"))
        .await;
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
    assert_eq!(terminal_recheck.status, "closed");
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
        parked.status, "closed",
        "second-strike guard trip force-closes the task"
    );
    assert!(
        parked
            .close_reason
            .as_deref()
            .is_some_and(|reason| reason.contains("planner intervention")),
        "second-strike close reason should preserve the recoverable planner-intervention park message"
    );
    assert!(
        planner_intervention_markers(&repo, &task.id)
            .await
            .is_empty(),
        "second strike parks without writing a fresh marker"
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
        "src/actors/coordinator/dispatch/task_dispatch.rs",
        "src/actors/coordinator/dispatch/wave_dispatch.rs",
        "src/actors/coordinator/dispatch/session_recovery.rs",
        "src/actors/coordinator/dispatch/retry.rs",
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
/// STILL climbed back to the reopen threshold, the coordinator parks it
/// terminally instead of escalating to the Planner again — no new marker,
/// no new review task, and the task ends up `closed`. This is the loop
/// breaker for the txr4 case (rescope didn't help → stop hogging the slot).
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

    // Parked terminally — task is closed.
    let parked = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        parked.status, "closed",
        "second strike force-closes the task"
    );

    // No planner intervention marker for this reopen count, and no new
    // planner review task — the loop is broken, not re-escalated.
    assert!(
        !planner_intervention_markers(&repo, &task.id)
            .await
            .iter()
            .any(|m| m["reopen_count"] == REOPEN_INTERVENTION_THRESHOLD),
        "second strike must not write a new planner intervention marker"
    );
    let reviews = repo.list_by_status("open").await.unwrap();
    assert!(
        !reviews
            .iter()
            .any(|t| t.issue_type == "review" && t.project_id == parked.project_id),
        "second strike must not create another planner review task"
    );
}

// ── CI-enriched planner escalation regression tests ──────────────────────
//
// These tests verify that CI failure sections are properly threaded through
// escalation paths — the core wiring added by epic nnij.

/// `escalate_ci_failure_and_close` includes CI failure sections in both the
/// visibility comment and the escalation reason passed to
/// `dispatch_planner_escalation`.
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
        .escalate_ci_failure_and_close(&task, pr_url, reason, &sections)
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
        .expect("escalate_ci_failure_and_close must log a PR CI Escalation comment");
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

    // The escalation reason passed to dispatch_planner_escalation also
    // includes the sections (visible via the PLANNER_ESCALATION comment that
    // dispatch_planner_escalation logs on the source task).
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
        "escalation reason must include CI Failure Details header; got: {}",
        planner_comment.payload
    );
    assert!(
        planner_comment.payload.contains("**Workflow:** CI"),
        "escalation reason must include workflow section; got: {}",
        planner_comment.payload
    );
}

/// `escalate_ci_failure_and_close` with empty sections omits the CI Failure
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
        .escalate_ci_failure_and_close(&task, pr_url, reason, &sections)
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
        .route_planner_intervention(&task, "worker", reason, Some(sections))
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
        .route_planner_intervention(&task, "worker", reason, None)
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
