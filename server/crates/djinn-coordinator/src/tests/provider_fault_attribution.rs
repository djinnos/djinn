//! Who gets blamed for a failed session: the task, or the provider?
//!
//! Incident (task `2gq7`, 2026-07-29): a planner task failed three consecutive
//! sessions on `openai/gpt-5.6-sol`, each on an independent OpenAI HTTP 500
//! (`server_is_overloaded`, `server_error`, `server_is_overloaded` — the last
//! dying in 2s with `tokens_in=0`, before the first token). The worker folded
//! all three into `ProviderFailureClass::Failure`, the coordinator counted them
//! as three strikes of the same reproducible fault, and trigger D minted a
//! "Planner remediation" task (`oftp`) whose reason asserted *"The redispatched
//! worker reproduces the same failure each time (e.g. a poisoned resume
//! transcript that the provider rejects, or an unusable credential)"* — which was
//! simply untrue. `model_health` meanwhile already showed the model
//! `auto_disabled` with 7 consecutive failures: the fault was the model's, and
//! the breaker had already said so.
//!
//! These tests drive the REAL dispatch pass (`dispatch_ready_tasks`), not the
//! classification helper, because the classification label is not the bug — the
//! bug is what the coordinator DOES with it. Each asserts a durable side effect:
//! whether a planner-remediation intervention was routed, and whether the
//! terminal `dispatch_failure_streak` advanced far enough to force-close the
//! task.

use super::*;
use djinn_db::ActivityQuery;
use djinn_provider::catalog::health::TaskFailureSignal;

/// A transient provider-side fault, exactly as the slot supervisor-runner
/// records it for a `ProviderFailureClass::Transient` report (5xx /
/// transport death).
const TRANSIENT: TaskFailureSignal = TaskFailureSignal {
    throttle: false,
    transient: true,
    retry_after_ms: None,
};

/// A REQUEST-attributable provider failure — `InvalidRequest` / `InvalidOutput`,
/// i.e. the poisoned-resume-transcript class that redispatch really does
/// reproduce. This is the control: it must still escalate at the threshold, or
/// the fix would have merely disabled trigger D wholesale.
const REQUEST_FAULT: TaskFailureSignal = TaskFailureSignal {
    throttle: false,
    transient: false,
    retry_after_ms: None,
};

/// Replay `passes` consecutive same-role failed reappearances through the real
/// dispatch pass, each carrying `signal` on the shared `HealthTracker`
/// side-channel — the same handoff `apply_provider_breaker_feedback` performs
/// in production when a task-run reports a typed provider failure.
///
/// Between passes we re-seed the `last_dispatched` marker (the pass consumes it)
/// and drop any cooldown the previous failure applied — wall-clock time is what
/// expires it in production, and we are modelling three failures spread over
/// ~11 minutes, as `2gq7`'s were.
async fn replay_failed_reappearances(
    actor: &mut CoordinatorActor,
    task_id: &str,
    role: &str,
    signal: TaskFailureSignal,
    passes: usize,
) {
    for _ in 0..passes {
        actor.last_dispatched.insert(
            task_id.to_owned(),
            DispatchMarker {
                instant: StdInstant::now(),
                role: role.to_owned(),
            },
        );
        actor.health.note_task_provider_failure(task_id, signal);
        actor.dispatch_cooldowns.remove(task_id);
        actor.dispatch_ready_tasks(None).await;
    }
}

async fn remediation_blocker_count(repo: &TaskRepository, task_id: &str) -> usize {
    repo.list_blockers(task_id).await.unwrap().len()
}

async fn arbiter_dispatched_count(repo: &TaskRepository, task_id: &str) -> usize {
    repo.query_activity(ActivityQuery {
        task_id: Some(task_id.to_owned()),
        event_type: Some("arbiter_dispatched".to_string()),
        ..ActivityQuery::default()
    })
    .await
    .unwrap()
    .len()
}

/// THE regression: three consecutive sessions killed by transient provider
/// faults must route NO planner-remediation intervention and must NOT advance
/// the terminal dispatch-failure streak.
///
/// Without the fix the third pass crosses `FAILURE_ESCALATION_THRESHOLD`, writes
/// a `planner_intervention` marker and creates the blocking remediation task —
/// the `oftp` this test exists to prevent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_transient_provider_faults_route_no_planner_remediation() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "planner task hit by openai 500s").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    // One more strike than the threshold: the escalation must not fire even
    // once past the rung that produced `oftp`.
    let passes = usize::try_from(FAILURE_ESCALATION_THRESHOLD).unwrap() + 1;
    replay_failed_reappearances(&mut actor, &task.id, "reviewer", TRANSIENT, passes).await;

    assert!(
        planner_intervention_markers(&repo, &task.id).await.is_empty(),
        "a run of transient provider faults must never route a planner-remediation \
         intervention — the provider failed, the task did not"
    );
    assert_eq!(
        remediation_blocker_count(&repo, &task.id).await,
        0,
        "no remediation task may be created to block the healthy task"
    );
    assert_eq!(
        arbiter_dispatched_count(&repo, &task.id).await,
        0,
        "the arbiter ladder must not be entered either"
    );
    assert!(
        !actor.provider_failure_streak.contains_key(&task.id),
        "transient faults must not even accumulate a provider-failure strike"
    );

    // The terminal counter — the one that force-closes a task at
    // MAX_DISPATCH_FAILURES — must be untouched, and the task must still be
    // alive and dispatchable.
    assert!(
        !actor.dispatch_failure_streak.contains_key(&task.id),
        "a transient provider fault must not advance the terminal dispatch-failure streak"
    );
    let after = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        after.status, "needs_task_review",
        "the task must stay exactly where it was, awaiting a healthy provider"
    );

    // …but the task IS still backed off: sparing the counters must not turn
    // into hammering the provider every tick.
    assert!(
        actor.dispatch_cooldowns.contains_key(&task.id),
        "the escalating redispatch cooldown still applies to a transient fault"
    );
}

/// Control for the test above: the REQUEST-attributable class (the poisoned
/// resume transcript a redispatch genuinely reproduces) must still escalate on
/// the `FAILURE_ESCALATION_THRESHOLD`-th consecutive strike. If this test and
/// the one above ever pass for the same reason, trigger D was disabled rather
/// than corrected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_attributable_provider_failures_still_escalate_at_threshold() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "task the provider keeps rejecting").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);

    // Strikes below the threshold: streak advances, nothing is routed.
    let below = usize::try_from(FAILURE_ESCALATION_THRESHOLD).unwrap() - 1;
    replay_failed_reappearances(&mut actor, &task.id, "reviewer", REQUEST_FAULT, below).await;
    assert_eq!(
        actor.provider_failure_streak.get(&task.id).map(|s| s.count),
        Some(FAILURE_ESCALATION_THRESHOLD - 1),
        "each request-attributable failure advances the provider-failure streak"
    );
    assert!(
        planner_intervention_markers(&repo, &task.id).await.is_empty(),
        "below the threshold nothing is routed"
    );

    // The threshold strike escalates.
    replay_failed_reappearances(&mut actor, &task.id, "reviewer", REQUEST_FAULT, 1).await;
    assert_eq!(
        planner_intervention_markers(&repo, &task.id).await.len(),
        1,
        "the {FAILURE_ESCALATION_THRESHOLD}th consecutive request-attributable provider failure \
         must still route a planner-remediation intervention"
    );
    assert_eq!(
        remediation_blocker_count(&repo, &task.id).await,
        1,
        "the escalation still creates the remediation task that holds the source task"
    );
}

/// The terminal force-close at `MAX_DISPATCH_FAILURES` must also be spared.
///
/// A task that already carries a near-terminal streak (from earlier, genuine
/// failures) and then meets a provider outage must not be closed by that
/// outage: rehydrate the streak one below the cap, deliver ONE transient fault,
/// and the task must survive with its streak unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn transient_provider_fault_cannot_force_close_a_task_at_the_cap() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "near-terminal task hit by an outage").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();
    // Pre-existing planner-intervention marker: keeps trigger B out of the way
    // so the ONLY thing under test is the terminal-close branch.
    repo.log_activity(
        Some(&task.id),
        "coordinator",
        "system",
        PLANNER_INTERVENTION_MARKER,
        &serde_json::json!({ "reopen_count": task.reopen_count }).to_string(),
    )
    .await
    .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor
        .dispatch_failure_streak
        .insert(task.id.clone(), MAX_DISPATCH_FAILURES - 1);
    replay_failed_reappearances(&mut actor, &task.id, "reviewer", TRANSIENT, 1).await;

    let after = repo.get(&task.id).await.unwrap().unwrap();
    assert_ne!(
        after.status, "closed",
        "a provider outage must never be the strike that terminally closes a task"
    );
    assert_eq!(
        actor.dispatch_failure_streak.get(&task.id).copied(),
        Some(MAX_DISPATCH_FAILURES - 1),
        "the terminal streak is left exactly where it was"
    );
}

/// Control for the test above, mirroring
/// `restart_rehydrated_failure_streak_continues_to_terminal_close_threshold`:
/// the same near-terminal streak plus a REQUEST-attributable failure still ends
/// in the terminal close. The safety backstop is intact; only the attribution
/// changed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn request_attributable_failure_at_the_cap_still_force_closes() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "near-terminal undispatchable task").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();
    repo.log_activity(
        Some(&task.id),
        "coordinator",
        "system",
        PLANNER_INTERVENTION_MARKER,
        &serde_json::json!({ "reopen_count": task.reopen_count }).to_string(),
    )
    .await
    .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor
        .dispatch_failure_streak
        .insert(task.id.clone(), MAX_DISPATCH_FAILURES - 1);
    replay_failed_reappearances(&mut actor, &task.id, "reviewer", REQUEST_FAULT, 1).await;

    let after = repo.get(&task.id).await.unwrap().unwrap();
    assert_eq!(
        after.status, "closed",
        "a request-attributable failure at the cap must still close the task terminally"
    );
}

/// Defense in depth (E): even if the wire class regressed to the coarse
/// pre-fix `Failure` — a version-skewed worker, or a future
/// misclassification — a model whose own health breaker is auto-disabled for
/// this scope must not have its outage blamed on the task. During `2gq7` the
/// breaker had already tripped (7 consecutive failures, `auto_disabled`) while
/// the coordinator was minting a remediation task about the transcript.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn auto_disabled_model_breaker_blocks_the_escalation() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "task on an auto-disabled model").await;
    let repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let task = repo
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();

    // The task's last reviewer session ran on this model.
    let model_id = "openai/gpt-5.6-sol";
    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: model_id,
            agent_type: "reviewer",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    // Trip the per-(scope, model) breaker for the task creator's scope, as a run
    // of failures on that model does in production.
    let scope = Some(task.created_by_user_id.as_str());
    while !actor.health.model_health(scope, model_id).auto_disabled {
        actor.health.record_failure(scope, model_id);
    }

    // Deliver the COARSE pre-fix class: `throttle: false, transient: false` —
    // indistinguishable from a poisoned transcript at the call-site gate.
    let passes = usize::try_from(FAILURE_ESCALATION_THRESHOLD).unwrap() + 1;
    replay_failed_reappearances(&mut actor, &task.id, "reviewer", REQUEST_FAULT, passes).await;

    assert!(
        planner_intervention_markers(&repo, &task.id).await.is_empty(),
        "a tripped model-wide breaker is evidence the MODEL is at fault; the task must not be \
         escalated over it"
    );
    assert!(
        !actor.provider_failure_streak.contains_key(&task.id),
        "the guard stands the streak down entirely while the breaker blames the model"
    );
}
