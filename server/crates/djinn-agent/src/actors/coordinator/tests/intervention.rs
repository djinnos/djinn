use super::*;
use crate::supervisor_impl::disposition::{NUDGE_CAP, RunDisposition, decide_run_disposition};
use djinn_core::{events::DjinnEventEnvelope, models::SessionStatus};
use djinn_core::run_progress::RunProgress;
use djinn_db::{DispatchStateRepository, DispatchStateUpsert};

#[allow(dead_code)]
struct InterventionChaosHarness {
    db: Database,
    tx: broadcast::Sender<DjinnEventEnvelope>,
    _rx: broadcast::Receiver<DjinnEventEnvelope>,
    actor: CoordinatorActor,
    repo: TaskRepository,
    task_id: String,
}

#[allow(dead_code)]
impl InterventionChaosHarness {
    async fn new(initial_reopen_count: i64) -> Self {
        let db = test_helpers::create_test_db();
        let (tx, rx) = broadcast::channel(256);
        let actor = coordinator_actor_for_tests(&db, &tx);
        let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
        let task = make_task_with_reopen_count(&db, &tx, initial_reopen_count).await;

        Self {
            db,
            tx,
            _rx: rx,
            actor,
            repo,
            task_id: task.id,
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

    async fn complete_planner_intervention_and_reset_ladder(&self) -> djinn_core::models::Task {
        self.repo
            .reset_intervention_counters(&self.task_id)
            .await
            .unwrap();
        self.task().await
    }

    async fn seed_same_role_redispatch_state(&mut self, role: &'static str, streak: u32) {
        let cooldown = std::time::Duration::from_secs(300);
        self.actor.dispatch_failure_streak.insert(self.task_id.clone(), streak);
        self.actor.dispatch_cooldowns.insert(
            self.task_id.clone(),
            StdInstant::now() + cooldown,
        );
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

    async fn advance_same_role_cycle(&mut self, role: &'static str) -> (bool, djinn_core::models::Task) {
        let next_streak = self
            .actor
            .dispatch_failure_streak
            .get(&self.task_id)
            .copied()
            .unwrap_or(0)
            .saturating_add(1);
        self.seed_same_role_redispatch_state(role, next_streak).await;
        let task = self.task().await;
        let handled = self
            .actor
            .maybe_intervene_on_cycling_task(&task, role, next_streak)
            .await;
        let refreshed = self.task().await;
        (handled, refreshed)
    }

    async fn planner_intervention_markers(&self) -> Vec<serde_json::Value> {
        planner_intervention_markers(&self.repo, &self.task_id).await
    }

    async fn open_planner_intervention_reviews(&self) -> Vec<djinn_core::models::Task> {
        let task = self.task().await;
        self.repo
            .list_by_status("open")
            .await
            .unwrap()
            .into_iter()
            .filter(|candidate| candidate.issue_type == "review" && candidate.project_id == task.project_id)
            .collect()
    }

    async fn assert_open_planner_review_count(&self, expected: usize) {
        assert_eq!(
            self.open_planner_intervention_reviews().await.len(),
            expected,
            "open Planner intervention review count"
        );
    }

    async fn assert_marker_reopen_counts(&self, expected: &[i64]) {
        let actual: Vec<i64> = self
            .planner_intervention_markers()
            .await
            .iter()
            .map(|marker| marker["reopen_count"].as_i64().expect("marker reopen_count"))
            .collect();
        assert_eq!(actual, expected, "planner_intervention marker reopen counts");
    }

    async fn assert_task_status(&self, status: &str, close_reason_contains: Option<&str>) {
        let task = self.task().await;
        assert_eq!(task.status, status, "task status");
        if let Some(needle) = close_reason_contains {
            assert!(
                task.close_reason.as_deref().is_some_and(|reason| reason.contains(needle)),
                "close_reason should contain {needle:?}; got {:?}",
                task.close_reason
            );
        }
    }

    async fn assert_dispatch_backoff_cleared(&self) {
        assert!(
            !self.actor.last_dispatched.contains_key(&self.task_id),
            "last_dispatched should be cleared"
        );
        assert!(
            !self.actor.dispatch_failure_streak.contains_key(&self.task_id),
            "dispatch_failure_streak should be cleared"
        );
        assert!(
            !self.actor.dispatch_cooldowns.contains_key(&self.task_id),
            "dispatch_cooldowns should be cleared"
        );

        let durable = DispatchStateRepository::new(self.db.clone())
            .get(&self.task_id)
            .await
            .unwrap();
        if let Some(durable) = durable {
            assert_eq!(durable.failure_streak, 0, "durable failure_streak");
            assert!(durable.cooldown_until.is_none(), "durable cooldown_until");
            assert!(durable.last_dispatched_at.is_none(), "durable last_dispatched_at");
            assert!(durable.last_dispatched_role.is_none(), "durable last_dispatched_role");
        }
    }
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
