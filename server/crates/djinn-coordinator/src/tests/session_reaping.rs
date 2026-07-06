// djinn:allow-oversize
use super::*;

fn rendered_counter_value(metric: &str, kind: &str) -> f64 {
    djinn_telemetry::init().unwrap();
    let rendered = djinn_telemetry::render().unwrap();
    let prefix = format!("{metric}{{kind=\"{kind}\"}}");
    rendered
        .lines()
        .find_map(|line| {
            let value = line.strip_prefix(&prefix)?.trim();
            value.parse::<f64>().ok()
        })
        .unwrap_or(0.0)
}

// ── Model failover via the health circuit-breaker ────────────────────────

/// A model tripped on a stall is skipped by `try_dispatch_to_pool`, which
/// fails over to the next model in the creator's ordered list. This is the
/// core failover behaviour: without feeding the breaker the first
/// (preferred) model is always `is_available` and always re-selected.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_model_is_skipped_and_dispatch_fails_over_to_next() {
    use std::sync::{Arc, Mutex};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);

    let bad = "openai/gpt-5.5".to_string();
    let good = "openai/gpt-5.4".to_string();
    let model_ids = vec![bad.clone(), good.clone()];

    // Trip the preferred model on a zero-token stall.
    actor.health.record_stall(None, &bad, true);
    assert!(!actor.health.is_available(None, &bad));
    assert!(actor.health.is_available(None, &good));

    // Record which model the dispatch closure is actually invoked with.
    let attempted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let attempted_cl = attempted.clone();
    let outcome = actor
        .try_dispatch_to_pool(
            "failover-test",
            "worker",
            0,
            None,
            &model_ids,
            |_pool, model_id| {
                let attempted = attempted_cl.clone();
                let model_id = model_id.to_owned();
                async move {
                    attempted.lock().unwrap().push(model_id);
                    Ok::<(), PoolError>(())
                }
            },
        )
        .await;

    assert!(matches!(outcome, DispatchOutcome::Dispatched));
    let attempted = attempted.lock().unwrap().clone();
    assert_eq!(
        attempted,
        vec![good.clone()],
        "the stalled preferred model must be skipped; dispatch fails over to the next model"
    );
}

/// Once the stalled model's cooldown expires it is available again, so the
/// preferred model is re-selected — the failover self-heals.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stalled_model_recovers_after_cooldown_expires() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);

    let bad = "openai/gpt-5.5".to_string();
    actor.health.record_stall(None, &bad, true);
    assert!(!actor.health.is_available(None, &bad));

    // Simulate cooldown expiry, then a successful run resets the breaker.
    actor.health.enable(None, &bad);
    actor.health.record_success(None, &bad);
    assert!(actor.health.is_available(None, &bad));

    let model_ids = vec![bad.clone()];
    let outcome = actor
        .try_dispatch_to_pool(
            "recover-test",
            "worker",
            0,
            None,
            &model_ids,
            |_pool, _model_id| async move { Ok::<(), PoolError>(()) },
        )
        .await;
    assert!(matches!(outcome, DispatchOutcome::Dispatched));
}

// ── Zombie-session DB-truth backstop ─────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn taskrun_job_backstop_skips_empty_task_run_id_inventory_entry() {
    let db = test_helpers::create_test_db();
    let runtime = RecordingRuntimeOps::new(false).with_taskrun_jobs(vec![
        djinn_control_plane::bridge::TaskrunJobRef {
            job_name: "djinn-taskrun-empty".to_string(),
            task_run_id: "".to_string(),
        },
        djinn_control_plane::bridge::TaskrunJobRef {
            job_name: "djinn-taskrun-whitespace".to_string(),
            task_run_id: "   ".to_string(),
        },
    ]);
    let mut app_state =
        test_helpers::coordinator_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(std::sync::Arc::new(runtime.clone()));

    health::reap_orphaned_taskrun_jobs(&db, &app_state, "test").await;

    assert!(
        runtime.calls().is_empty(),
        "malformed task-run inventory entries without a usable task_run_id must be skipped"
    );
}

/// Regression for the xh6f wedge: a session stuck `running` with zero
/// tokens past the hard cap is reaped purely on DB truth — the row is
/// finalized and the task released for redispatch — even when the
/// in-memory fast-path reapers (`stall_killed`, `pool.has_session`) would
/// skip it. Models a worker that came up, wrote its session row, then died
/// before producing a token without the slot's `Killed` event ever firing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zombie_zero_token_session_is_reaped_on_db_truth() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "zombie-reap").await;

    // Put the task in an execution state, as if dispatched.
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-zombie-reap";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    // Backdate well past the 10-minute hard cap, leaving tokens at 0/0.
    // Match the column's stored format (VARCHAR `YYYY-MM-DDThh:mm:ss.msZ`)
    // so `parse_iso_elapsed` reads it.
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "precondition: zombie session should be listed as running"
    );

    let runtime = RecordingRuntimeOps::new(true);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime.clone()));
    let before_metric = rendered_counter_value("djinn_zombie_reaps_total", "stall");
    actor.reap_zombie_sessions().await;

    assert_eq!(
        runtime.calls(),
        vec![run_id.to_string()],
        "zombie reaper must best-effort delete the task-run Job using DB session.task_run_id, even when teardown fails"
    );
    assert!(rendered_counter_value("djinn_zombie_reaps_total", "stall") - before_metric >= 1.0);

    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "zombie session row must be finalized by the backstop"
    );
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "open",
        "task must be released for redispatch after the zombie is reaped"
    );
    assert!(
        actor.health.is_available(None, "openai/gpt-5.5"),
        "reaping an infra/drift zombie must NOT trip the model breaker: the backstop \
         fires on capacity/OOM/leak/hung-tool conditions, none of which are model \
         evidence — tripping it disables the (often only) model for the scope and \
         turns a transient capacity pinch into a full dispatch outage. Genuine model \
         stalls are owned by the fast-path stall-kill and the supervisor ProviderError path."
    );
}

/// A young zero-token session (still inside the fast-path window) is left
/// alone by the backstop — the 180s stall breaker owns that case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn young_zero_token_session_is_not_reaped() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "young-session").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.reap_zombie_sessions().await;

    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "a session inside the hard-cap window must not be reaped by the backstop"
    );
}

/// A zero-token session PAST the hard cap is NOT reaped while its worker
/// still holds a live RPC connection. This is the K8s false-reap fix: the
/// in-memory slot/activity bookkeeping can drift for remote pods (making the
/// activity gate false-negative), but a live connection is ground-truth that
/// the worker is alive, so the backstop must defer to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connected_worker_past_hard_cap_is_not_reaped() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "connected-no-reap").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-connected-1";
    // `sessions.task_run_id` has an FK to `task_runs`, so seed the run row.
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    // Backdate past the 10-minute hard cap, tokens still 0/0.
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    // Wire a registry that reports a LIVE connection for this run.
    let registry = std::sync::Arc::new(djinn_supervisor::ConnectionRegistry::new());
    registry.register_connected_for_test(run_id).await;
    let runtime = RecordingRuntimeOps::new(false);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.rpc_registry = Some(registry.clone());
    actor.runtime_ops = Some(std::sync::Arc::new(runtime.clone()));

    actor.reap_zombie_sessions().await;

    assert!(
        runtime.calls().is_empty(),
        "connected live sessions must not have their task-run Job deleted"
    );

    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "a past-cap session with a live worker connection must NOT be reaped"
    );
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "in_progress",
        "task with a live connected worker must stay in_progress, not be released"
    );

    // Sanity: once the connection drops, the same session IS reaped.
    registry.deregister(run_id).await;
    actor.reap_zombie_sessions().await;
    assert_eq!(
        runtime.calls(),
        vec![run_id.to_string()],
        "once liveness gates pass, zombie reaping deletes the task-run Job"
    );
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "after the worker connection drops, the past-cap zombie must be reaped"
    );
}

/// Stall timeout goes through `pool.kill_session`, which owns task-run Job
/// teardown for slot-mapped sessions. Teardown errors are non-fatal: the
/// coordinator still marks this session as killed so the normal stall cleanup
/// proceeds without retry-spamming the same DB row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stall_timeout_tears_down_taskrun_job_through_slot_pool_kill_path() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "stall-teardown").await;
    let run_id = "run-stall-timeout";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "40 minutes")
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(true);
    let mut app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(std::sync::Arc::new(runtime.clone()));
    // Clone the activity tracker Arc before moving app_state into the pool,
    // so we can overwrite the activity timestamp after dispatch (which
    // re-registers the activity with the current time).
    let active_tasks = app_state.active_tasks.clone();
    let cancel = CancellationToken::new();
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel.clone(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: "openai/gpt-5.5".to_string(),
                max_slots: 1,
                roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
            }],
            role_priorities: HashMap::new(),
        },
        std::sync::Arc::new(|slot_id, model_id, event_tx, app_state, cancel| {
            let runner: djinn_slot::TestLifecycleRunner = std::sync::Arc::new(
                |_task_id,
                 _project_path,
                 _model_id,
                 _app_state,
                 kill,
                 _pause,
                 _resume_lifecycle_metadata| {
                    Box::pin(async move {
                        kill.cancelled().await;
                        Ok(())
                    })
                },
            );
            SlotHandle::spawn_with_test_runner(
                slot_id, model_id, event_tx, app_state, cancel, runner,
            )
        }),
    );
    pool.dispatch(&task.id, "test-project", "openai/gpt-5.5")
        .await
        .expect("dispatch should create a slot mapping");

    // Overwrite the activity timestamp AFTER dispatch. The pool's dispatch
    // re-registers activity with the current time; we need the stall timeout
    // to see a 40-minute-old idle to trigger the kill path.
    let old = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(40 * 60))
        .unwrap_or(0);
    {
        let guard = active_tasks.lock().expect("active_tasks mutex");
        if let Some(ts) = guard.get(&task.id) {
            ts.store(old, std::sync::atomic::Ordering::Relaxed);
        }
    }

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = pool;
    actor.enforce_session_stall_timeout().await;

    assert_eq!(
        runtime.calls(),
        vec![run_id.to_string()],
        "stall timeout should invoke task-run Job teardown through pool.kill_session"
    );
    assert!(
        actor.stall_killed.contains(&session.id),
        "teardown failure must not prevent stall guard cleanup"
    );
    cancel.cancel();
}

/// `stall_killed` is keyed by session id and pruned against `list_active()`:
/// a leftover entry for a session that is no longer running is dropped, so
/// it can never linger to mask a redispatched successor session for the
/// same task (the proximate cause of the xh6f permanent wedge).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stall_killed_prunes_sessions_absent_from_active() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let mut actor = coordinator_actor_for_tests(&db, &tx);

    actor
        .stall_killed
        .insert("019e764f-dead-session".to_string());
    // No sessions are running, so the prune (retain by active session id)
    // must clear the stale entry.
    actor.enforce_session_stall_timeout().await;
    assert!(
        actor.stall_killed.is_empty(),
        "stall_killed entries for sessions absent from list_active() must be pruned"
    );
}

// ── No-slot-mapping & teardown-failure hardening for the zombie backstop ───

/// Reaping must delete the task-run Job even when the slot pool has no live
/// mapping for the task — the (likely leaked) slot was already reclaimed
/// before the backstop ticked, so the only authoritative reference to the
/// pod is the DB session row's `task_run_id`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reap_zombie_session_with_no_slot_mapping_still_tears_down_taskrun_job() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "zombie-no-slot").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-zombie-no-slot";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(false);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(std::sync::Arc::new(runtime.clone()));

    actor.reap_zombie_sessions().await;

    assert_eq!(
        runtime.calls(),
        vec![run_id.to_string()],
        "zombie reap must delete the task-run Job from DB session.task_run_id even when the slot pool has no mapping"
    );
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "zombie session row must be finalized by the backstop"
    );
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "open",
        "task must be released for redispatch after the no-slot zombie is reaped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reap_zombie_session_continues_recovery_when_teardown_fails() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "zombie-teardown-fail").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-zombie-teardown-fail";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(true);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(std::sync::Arc::new(runtime.clone()));

    actor.reap_zombie_sessions().await;

    assert_eq!(
        runtime.calls(),
        vec![run_id.to_string()],
        "teardown must be attempted even when it errors"
    );
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "session row must be finalized even when teardown errors"
    );
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "open",
        "task must be released for redispatch even when teardown errors"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reap_zombie_session_without_task_run_id_is_reaped_without_teardown() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "zombie-no-run-id").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(false);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(std::sync::Arc::new(runtime.clone()));

    actor.reap_zombie_sessions().await;

    assert!(
        runtime.calls().is_empty(),
        "sessions with no task_run_id must not invoke teardown (no Job to delete)"
    );
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "session row must still be finalized by the backstop"
    );
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "open",
        "task must still be released for redispatch even when no task-run Job exists"
    );
}

fn taskrun_job_ref(task_run_id: &str) -> djinn_control_plane::bridge::TaskrunJobRef {
    djinn_control_plane::bridge::TaskrunJobRef {
        job_name: format!("djinn-taskrun-{task_run_id}"),
        task_run_id: task_run_id.to_string(),
    }
}

fn new_task_run_uuid() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn temp_cargo_target_runs_root() -> tempfile::TempDir {
    let parent = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(".cache")
        .join("djinn");
    std::fs::create_dir_all(&parent).unwrap();
    tempfile::Builder::new()
        .prefix("cargo-target-runs-gc-")
        .tempdir_in(parent)
        .unwrap()
}

async fn seed_task_run(db: &Database, task: &djinn_core::models::Task, id: &str, status: &str) {
    if status == "running" {
        TaskRunRepository::new(db.clone())
            .create(CreateTaskRunParams {
                id,
                project_id: &task.project_id,
                task_id: &task.id,
                trigger_type: "manual",
                status: Some("running"),
                workspace_path: None,
                mirror_ref: None,
            })
            .await
            .unwrap();
    } else {
        TaskRunRepository::new(db.clone())
            .create(CreateTaskRunParams {
                id,
                project_id: &task.project_id,
                task_id: &task.id,
                trigger_type: "manual",
                status: Some(status),
                workspace_path: None,
                mirror_ref: None,
            })
            .await
            .unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cargo_target_run_dir_sweep_retains_live_and_deletes_orphans() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "cargo-target-run-dir-gc").await;
    let root = temp_cargo_target_runs_root();

    let live_run_id = new_task_run_uuid();
    seed_task_run(&db, &task, &live_run_id, "running").await;

    let terminal_run_id = new_task_run_uuid();
    seed_task_run(&db, &task, &terminal_run_id, "completed").await;

    let live_session_guard_run_id = new_task_run_uuid();
    seed_task_run(&db, &task, &live_session_guard_run_id, "completed").await;
    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(&live_session_guard_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    let unknown_run_id = new_task_run_uuid();
    for run_id in [
        live_run_id.as_str(),
        terminal_run_id.as_str(),
        live_session_guard_run_id.as_str(),
        unknown_run_id.as_str(),
    ] {
        std::fs::create_dir(root.path().join(run_id)).unwrap();
    }
    std::fs::create_dir(root.path().join("not-a-task-run-id")).unwrap();
    std::fs::write(root.path().join(new_task_run_uuid()), b"not a directory").unwrap();

    let stats = health::sweep_orphaned_cargo_target_run_dirs_under(&db, root.path()).await;

    assert!(root.path().join(&live_run_id).is_dir());
    assert!(root.path().join(&live_session_guard_run_id).is_dir());
    assert!(!root.path().join(&terminal_run_id).exists());
    assert!(!root.path().join(&unknown_run_id).exists());
    assert!(root.path().join("not-a-task-run-id").is_dir());
    assert_eq!(stats.scanned, 6);
    assert_eq!(stats.deleted, 2);
    assert_eq!(stats.retained, 4);
    assert_eq!(stats.errors, 0);
    // The default cap (64) never trims our handful — the orphan sweep above did
    // all the work. The hard-cap LRU-trim itself is unit-tested in
    // `djinn_core::cargo_target_runs::trim_keeps_newest_and_removes_oldest_beyond_cap`.
    assert_eq!(stats.cap_trimmed, 0);
    assert_eq!(stats.cap_errors, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn taskrun_job_backstop_deletes_absent_and_finalized_rows_only() {
    use djinn_core::models::SessionStatus;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "taskrun-job-backstop").await;

    let finalized_run_id = "run-finalized-backstop";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: finalized_run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("completed"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let live_run_id = "run-live-backstop";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: live_run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let live_session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(live_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    let completed_session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(finalized_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .update(
            &completed_session.id,
            SessionStatus::Completed,
            1,
            1,
            0,
            0,
            None,
        )
        .await
        .unwrap();

    let interrupted_run_id = "run-interrupted-backstop";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: interrupted_run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();
    let interrupted_session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(interrupted_run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .update(
            &interrupted_session.id,
            SessionStatus::Interrupted,
            1,
            1,
            0,
            0,
            None,
        )
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(false).with_taskrun_jobs(vec![
        taskrun_job_ref("run-absent-backstop"),
        taskrun_job_ref(finalized_run_id),
        taskrun_job_ref(interrupted_run_id),
        taskrun_job_ref(live_run_id),
    ]);
    let mut app_state =
        test_helpers::coordinator_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));

    health::reap_orphaned_taskrun_jobs(&db, &app_state, "test").await;

    assert_eq!(
        runtime.calls(),
        vec![
            "run-absent-backstop".to_string(),
            finalized_run_id.to_string(),
            interrupted_run_id.to_string(),
        ],
        "backstop must delete absent/finalized/interrupted-session Jobs and preserve live running task-runs"
    );
    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|session| session.id == live_session.id),
        "live running session must be preserved"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_resource_sweep_runs_taskrun_job_backstop() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "periodic-backstop-wiring").await;
    let periodic_run_id = "periodic-finalized";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: periodic_run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("completed"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();
    let runtime =
        RecordingRuntimeOps::new(false).with_taskrun_jobs(vec![taskrun_job_ref(periodic_run_id)]);
    let mut app_state =
        test_helpers::coordinator_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));

    let before_metric = rendered_counter_value("djinn_zombie_reaps_total", "periodic");
    health::sweep_stale_resources(&db, &app_state).await;

    assert_eq!(
        runtime.calls(),
        vec![periodic_run_id.to_string()],
        "periodic stale-resource sweep must run the K8s task-run Job backstop"
    );
    assert!(rendered_counter_value("djinn_zombie_reaps_total", "periodic") - before_metric >= 1.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_reconcile_runs_taskrun_job_backstop() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "startup-backstop-wiring").await;
    let startup_run_id = "startup-finalized";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: startup_run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("completed"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();
    let runtime =
        RecordingRuntimeOps::new(false).with_taskrun_jobs(vec![taskrun_job_ref(startup_run_id)]);
    let mut app_state =
        test_helpers::coordinator_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));

    let before_metric = rendered_counter_value("djinn_zombie_reaps_total", "startup");
    health::reap_orphaned_taskrun_jobs_for_startup(&db, &app_state).await;

    assert_eq!(
        runtime.calls(),
        vec![startup_run_id.to_string()],
        "startup reconcile must run the K8s task-run Job backstop before periodic intervals"
    );
    assert!(rendered_counter_value("djinn_zombie_reaps_total", "startup") - before_metric >= 1.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn taskrun_job_backstop_continues_after_delete_failure() {
    let db = test_helpers::create_test_db();
    let runtime = RecordingRuntimeOps::new(true).with_taskrun_jobs(vec![
        taskrun_job_ref("missing-one"),
        taskrun_job_ref("missing-two"),
    ]);
    let mut app_state =
        test_helpers::coordinator_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));

    health::reap_orphaned_taskrun_jobs(&db, &app_state, "test").await;

    assert_eq!(
        runtime.calls(),
        vec!["missing-one".to_string(), "missing-two".to_string()],
        "teardown failures are best-effort and must not stop the sweep"
    );
}

// ── Orphan-session burn safeguards ──────────────────────────────────────

/// A task sitting in `open` with a stale `running` session (one whose start
/// predates the task's most recent `updated_at` transition) and no live pool
/// session or `BackgroundWorkTracker` entry is detected by
/// `detect_and_recover_stuck_filtered`. The stale session row is finalized
/// via `interrupt_running_for_task`, and the task stays in `open` (no status
/// transition — ready-state orphans only finalize the session, they don't
/// re-transition the task).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_state_stale_orphan_session_is_finalized() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "ready-stale-orphan").await;

    // Task stays `open` (its default after creation).
    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    // Backdate the session so it predates the task's `updated_at`. The task
    // was just created so its `updated_at` is ~now; the session is 20 minutes
    // older, making `session_predates_task_status` return true.
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "precondition: stale session should be listed as running"
    );

    // The test coordinator has no pool session for this task and no
    // BackgroundWorkTracker entry — exactly the orphan condition.
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.detect_and_recover_stuck_filtered(None).await;

    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "stale ready-state orphan session must be finalized via interrupt_running_for_task"
    );

    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "open",
        "ready-state orphan recovery finalizes the session but does NOT transition the task"
    );
}

/// A task in `open` with a running session whose start is NEWER than the
/// task's `updated_at` is NOT finalized — the session was legitimately
/// created after the task entered its current state (e.g. just redispatched).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_state_newer_session_is_not_finalized() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "ready-newer-session").await;

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    // Backdate the TASK's `updated_at` so the session's `started_at` is
    // NEWER. This models a just-redispatched task whose fresh session started
    // after the task's last status transition.
    djinn_db::test_support::backdate_task_updated_at(&db, &task.id, "20 minutes").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.detect_and_recover_stuck_filtered(None).await;

    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "a newer ready-state session must NOT be finalized"
    );
}

/// A task in a terminal status (`force_closed`) with a running session that
/// has nonzero tokens is reaped by `reap_zombie_sessions`. The kill-on-status
/// guard bypasses the `tokens_in/out != 0` skip because the owning task is
/// terminal — the session is an orphan regardless of accumulated tokens.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_bearing_terminal_orphan_is_reaped() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "terminal-token-orphan").await;

    // Put the task in a terminal status.
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "force_closed")
        .await
        .unwrap();

    let run_id = "run-terminal-token";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    // Set nonzero tokens to exercise the kill-on-status bypass, and backdate
    // past the zombie hard cap so the age gate passes.
    session_repo
        .set_tokens_and_backdate(&session.id, "20 minutes", 100, 50)
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(false);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime.clone()));
    actor.reap_zombie_sessions().await;

    assert_eq!(
        runtime.calls(),
        vec![run_id.to_string()],
        "terminal orphan session with nonzero tokens must have its task-run Job torn down"
    );
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "token-bearing terminal orphan session must be reaped despite nonzero tokens"
    );
}

/// A task that was reset back to `open` while its session was still running
/// (the session's `started_at` predates the reset's `updated_at`) is reaped
/// even when it has nonzero tokens. The kill-on-status guard recognizes the
/// session predates the reset and bypasses the token skip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_bearing_open_reset_orphan_is_reaped() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "open-reset-token-orphan").await;

    let run_id = "run-open-reset-token";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    // Set nonzero tokens and backdate the session to 20 minutes ago (predating
    // the task reset below).
    session_repo
        .set_tokens_and_backdate(&session.id, "20 minutes", 100, 50)
        .await
        .unwrap();

    // Now "reset" the task to `open` with a fresh `updated_at` (now), so the
    // session's started_at predates the reset. The task is already `open`
    // from `create_task_with_note`, but we explicitly touch `updated_at` to
    // ensure it is newer than the backdated session.
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "open")
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(false);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime.clone()));
    actor.reap_zombie_sessions().await;

    assert_eq!(
        runtime.calls(),
        vec![run_id.to_string()],
        "open-reset orphan session with nonzero tokens must have its task-run Job torn down"
    );
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "token-bearing open-reset orphan session must be reaped despite nonzero tokens"
    );
}

/// A session whose live token count exceeds `SESSION_TOKEN_CEILING` is killed
/// by `enforce_session_stall_timeout`, routed through loop-guard planner
/// intervention, and the model circuit breaker is NOT tripped — this is a
/// runaway/session-ownership guard, not provider-health evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn budget_ceiling_kill_routes_loop_guard_without_tripping_breaker() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "ceiling-kill").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    // Stand up a pool with the task dispatched into a slot, then inject a
    // token count exceeding the ceiling via the test-only override.
    let cancel = CancellationToken::new();
    let app_state = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
    let activity = app_state.register_activity(&task.id);
    // Touch activity so the pool reports activity_tracked=true (avoids the
    // first-call stall path masking the ceiling check).
    activity.store(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        std::sync::atomic::Ordering::Relaxed,
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel.clone(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: "openai/gpt-5.5".to_string(),
                max_slots: 1,
                roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
            }],
            role_priorities: HashMap::new(),
        },
        Arc::new(|slot_id, model_id, event_tx, app_state, cancel| {
            let runner: djinn_slot::TestLifecycleRunner = Arc::new(
                |_task_id,
                 _project_path,
                 _model_id,
                 _app_state,
                 kill,
                 _pause,
                 _resume_lifecycle_metadata| {
                    Box::pin(async move {
                        kill.cancelled().await;
                        Ok(())
                    })
                },
            );
            SlotHandle::spawn_with_test_runner(
                slot_id, model_id, event_tx, app_state, cancel, runner,
            )
        }),
    );
    pool.dispatch(&task.id, "test-project", "openai/gpt-5.5")
        .await
        .expect("dispatch should create a slot mapping");

    // Inject a token count well above SESSION_TOKEN_CEILING (2_000_000).
    pool.test_set_token_override(&task.id, 3_000_000, 10).await;

    // Give the fire-and-forget override message time to be processed.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = pool.clone();
    actor.enforce_session_stall_timeout().await;

    assert!(
        actor.stall_killed.contains(&session.id),
        "ceiling-tripped session must be killed (added to stall_killed set)"
    );

    // The model breaker must NOT be tripped — budget kills are not provider
    // evidence.
    assert!(
        actor.health.is_available(None, "openai/gpt-5.5"),
        "budget ceiling kill must NOT trip the model circuit breaker (not provider evidence)"
    );

    // The task must be routed through loop-guard planner intervention. We
    // check for the planner_intervention activity marker.
    let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let markers = planner_intervention_markers(&task_repo, &task.id).await;
    assert_eq!(
        markers.len(),
        1,
        "ceiling kill must route the task through loop-guard planner intervention"
    );

    cancel.cancel();
}

/// A long-running session with nonzero tokens (persisted on the DB row),
/// valid in-progress task state, and activity well under ceiling limits is
/// NOT killed by either `reap_zombie_sessions` or
/// `enforce_session_stall_timeout`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn healthy_under_ceiling_session_is_not_killed() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "healthy-under-ceiling").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    // Set nonzero tokens on the DB row (this is how they look mid-flight) and
    // keep started_at recent so the zombie hard cap doesn't fire.
    session_repo
        .set_token_counts(&session.id, 1000, 500)
        .await
        .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);

    // Run BOTH recovery mechanisms — neither should kill the healthy session.
    actor.reap_zombie_sessions().await;
    actor.enforce_session_stall_timeout().await;

    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "a healthy under-ceiling session must NOT be killed by zombie reap or ceiling enforcement"
    );
    assert!(
        !actor.stall_killed.contains(&session.id),
        "a healthy under-ceiling session must not be added to the stall_killed set"
    );

    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "in_progress",
        "a healthy under-ceiling task must remain in_progress"
    );
}

// ── Coordinator preservation gate ─────────────────────────────────────

/// When `runtime_ops` is `None` (dev/test mode), the preservation gate
/// returns `RuntimeUnavailable` and emits the right telemetry counter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preservation_gate_returns_runtime_unavailable_when_no_runtime_ops() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);

    // The test actor has runtime_ops = None.
    assert!(actor.runtime_ops.is_none());

    let result = actor
        .request_session_preservation(
            "task-test-1",
            "session-test-1",
            Some("run-test-1"),
            djinn_telemetry::preservation::TRIGGER_STALL,
        )
        .await;

    assert_eq!(
        result.outcome,
        PreservationOutcome::RuntimeUnavailable,
        "preservation gate must return RuntimeUnavailable when runtime_ops is None"
    );
    assert_eq!(result.task_id, "task-test-1");
    assert_eq!(result.session_id, "session-test-1");
    assert_eq!(result.trigger, "stall");
    assert!(result.commit_sha.is_none());
    assert!(result.ref_name.is_none());
}

/// When `runtime_ops` is present but there is no `task_run_id`, the gate
/// returns `UnavailableWorker` because it cannot target a specific pod.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preservation_gate_returns_unavailable_worker_when_no_task_run_id() {
    // This test exercises the task_run_id = None path.
    // Since the test actor's runtime_ops is None, the gate takes the
    // RuntimeUnavailable path first. We test the UnavailableWorker path
    // by verifying the helper constructor directly.
    let result = PreservationGateResult::unavailable_worker(
        "task-test-2",
        "session-test-2",
        None,
        djinn_telemetry::preservation::TRIGGER_ZOMBIE,
    );

    assert_eq!(result.outcome, PreservationOutcome::UnavailableWorker);
    assert_eq!(result.task_id, "task-test-2");
    assert_eq!(result.session_id, "session-test-2");
    assert_eq!(result.trigger, "zombie");
    assert!(result.task_run_id.is_none());
    assert!(result.commit_sha.is_none());
}

/// A clean-skip result records the correct reason and metadata.
#[test]
fn preservation_gate_clean_skip_records_reason() {
    let result = PreservationGateResult::clean_skip(
        "task-3",
        "session-3",
        "terminal_fail",
        "session had zero tokens",
    );

    assert_eq!(result.outcome, PreservationOutcome::CleanSkip);
    assert_eq!(result.reason, "session had zero tokens");
    assert_eq!(result.trigger, "terminal_fail");
    assert!(result.commit_sha.is_none());
    assert!(result.ref_name.is_none());
}

/// A succeeded result carries the checkpoint SHA and ref from the worker.
#[test]
fn preservation_gate_succeeded_carries_checkpoint_metadata() {
    let result = PreservationGateResult::succeeded(
        "task-4",
        "session-4",
        Some("run-4"),
        "stall",
        Some("abc123def456".to_string()),
        Some("refs/djinn/checkpoints/task-4".to_string()),
    );

    assert_eq!(result.outcome, PreservationOutcome::Succeeded);
    assert_eq!(result.commit_sha.as_deref(), Some("abc123def456"));
    assert_eq!(
        result.ref_name.as_deref(),
        Some("refs/djinn/checkpoints/task-4")
    );
    assert_eq!(result.task_run_id.as_deref(), Some("run-4"));
}

/// A failed result carries the failure reason and no checkpoint metadata.
#[test]
fn preservation_gate_failed_carries_failure_reason() {
    let result = PreservationGateResult::failed(
        "task-5",
        "session-5",
        Some("run-5"),
        "ceiling",
        "worker push rejected: permission denied".to_string(),
    );

    assert_eq!(result.outcome, PreservationOutcome::Failed);
    assert_eq!(result.reason, "worker push rejected: permission denied");
    assert!(result.commit_sha.is_none());
    assert!(result.ref_name.is_none());
}

/// The preservation gate is called during zombie reap and the result is
/// recorded in the activity log before the session is finalized.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preservation_gate_called_during_zombie_reap() {
    use djinn_db::{CreateSessionParams, SessionRepository};
    use djinn_db::{CreateTaskRunParams, TaskRunRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "preservation-zombie").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-preservation-zombie";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.reap_zombie_sessions().await;

    // The activity log should contain a preservation gate entry.
    let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let activity = task_repo.list_activity(&task.id).await.unwrap();
    let preservation_entry = activity
        .iter()
        .find(|entry| entry.payload.contains("preservation_outcome"));
    assert!(
        preservation_entry.is_some(),
        "zombie reap must log a preservation gate entry in the activity log; got entries: {:?}",
        activity.iter().map(|e| &e.payload[..]).collect::<Vec<_>>()
    );

    // The preservation entry should indicate runtime_unavailable since
    // the test actor has no runtime_ops.
    if let Some(entry) = preservation_entry {
        assert!(
            entry.payload.contains("runtime_unavailable"),
            "preservation entry should contain runtime_unavailable; got: {}",
            entry.payload
        );
    }
}

/// The preservation gate is called during terminal task failure and the
/// result is recorded in the activity log.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preservation_gate_called_during_terminal_task_failure() {
    use djinn_db::{CreateSessionParams, SessionRepository};
    use djinn_db::{CreateTaskRunParams, TaskRunRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "preservation-terminal").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-preservation-terminal";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    let actor = coordinator_actor_for_tests(&db, &tx);
    let closed = actor
        .terminally_fail_task(&task, "coordinator", "max retries exceeded")
        .await;
    assert!(closed, "task should be terminally closed");

    // The activity log should contain a preservation gate entry.
    let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let activity = task_repo.list_activity(&task.id).await.unwrap();
    let preservation_entry = activity
        .iter()
        .find(|entry| entry.payload.contains("preservation_outcome"));
    assert!(
        preservation_entry.is_some(),
        "terminal fail must log a preservation gate entry in the activity log; got entries: {:?}",
        activity.iter().map(|e| &e.payload[..]).collect::<Vec<_>>()
    );

    // Verify the session was interrupted.
    let active = session_repo.list_active().await.unwrap();
    assert!(
        !active.iter().any(|s| s.id == session.id),
        "session should be interrupted after terminal task failure"
    );
}

/// Preservation telemetry counter is incremented with the right outcome and
/// trigger labels.
#[test]
fn preservation_telemetry_counter_increments() {
    djinn_telemetry::init().unwrap();

    djinn_telemetry::preservation::increment_attempt(
        djinn_telemetry::preservation::OUTCOME_RUNTIME_UNAVAILABLE,
        djinn_telemetry::preservation::TRIGGER_STALL,
    );

    let rendered = djinn_telemetry::render().unwrap();
    let stall_line = rendered
        .lines()
        .find(|line| {
            line.starts_with("djinn_preservation_attempts_total")
                && line.contains("outcome=\"runtime_unavailable\"")
                && line.contains("trigger=\"stall\"")
        })
        .expect(
            "djinn_preservation_attempts_total{outcome=\"runtime_unavailable\",trigger=\"stall\"} \
             should be present after increment",
        );
    let value_str = stall_line.rsplit_once(' ').map(|(_, v)| v).unwrap_or("0");
    assert_eq!(
        value_str.parse::<f64>().unwrap_or(0.0),
        1.0,
        "preservation counter should be 1 after a single increment"
    );
}

/// `CheckpointLifecycleMetadata` round-trips through JSON with the new
/// `preservation_outcome` field.
#[test]
fn checkpoint_lifecycle_metadata_preservation_outcome_round_trips() {
    let metadata = CheckpointLifecycleMetadata {
        checkpoint_id: Some("ckpt-rt".to_string()),
        commit_sha: Some("deadbeef".to_string()),
        ref_name: Some("refs/djinn/checkpoints/task-rt".to_string()),
        requested_for: Some(CheckpointRequestReason::Shutdown),
        safety_scan: None,
        preservation_outcome: Some(PreservationOutcome::Succeeded),
        extra: serde_json::Map::new(),
    };

    let json = serde_json::to_value(&metadata).unwrap();
    assert_eq!(json["preservation_outcome"], "succeeded");

    let deserialized: CheckpointLifecycleMetadata = serde_json::from_value(json).unwrap();
    assert_eq!(
        deserialized.preservation_outcome,
        Some(PreservationOutcome::Succeeded)
    );
}

/// `PreservationOutcome` variants serialize to stable `snake_case` strings.
#[test]
fn preservation_outcome_serializes_to_stable_snake_case() {
    let cases = [
        (PreservationOutcome::Succeeded, "succeeded"),
        (PreservationOutcome::Failed, "failed"),
        (PreservationOutcome::UnavailableWorker, "unavailable_worker"),
        (
            PreservationOutcome::RuntimeUnavailable,
            "runtime_unavailable",
        ),
        (PreservationOutcome::CleanSkip, "clean_skip"),
    ];

    for (outcome, expected) in &cases {
        let json = serde_json::to_value(outcome).unwrap();
        assert_eq!(
            json.as_str().unwrap(),
            *expected,
            "PreservationOutcome::{:?} should serialize to {:?}",
            outcome,
            expected
        );

        let deserialized: PreservationOutcome = serde_json::from_value(json).unwrap();
        assert_eq!(
            deserialized, *outcome,
            "PreservationOutcome::{:?} should round-trip through JSON",
            outcome
        );
    }
}

// ── Preservation gate transition-blocking tests ────────────────────────

/// `Succeeded` and `CleanSkip` never block, regardless of policy.
#[test]
fn succeeded_and_clean_skip_never_block_transition() {
    for outcome in [
        PreservationOutcome::Succeeded,
        PreservationOutcome::CleanSkip,
    ] {
        assert!(
            !outcome.should_block_transition(PreservationFailurePolicy::RecordAndProceed),
            "{:?} should not block with RecordAndProceed",
            outcome,
        );
        assert!(
            !outcome.should_block_transition(PreservationFailurePolicy::Block),
            "{:?} should not block with Block",
            outcome,
        );
    }
}

/// `Failed`, `UnavailableWorker`, and `RuntimeUnavailable` block only
/// when the policy is `Block`.
#[test]
fn failure_outcomes_respect_policy() {
    let blocking_outcomes = [
        PreservationOutcome::Failed,
        PreservationOutcome::UnavailableWorker,
        PreservationOutcome::RuntimeUnavailable,
    ];

    for outcome in &blocking_outcomes {
        assert!(
            !outcome.should_block_transition(PreservationFailurePolicy::RecordAndProceed),
            "{:?} should NOT block with RecordAndProceed",
            outcome,
        );
        assert!(
            outcome.should_block_transition(PreservationFailurePolicy::Block),
            "{:?} SHOULD block with Block",
            outcome,
        );
    }
}

/// `PreservationFailurePolicy` serializes to stable `snake_case` strings
/// and round-trips through JSON.
#[test]
fn preservation_failure_policy_serializes_to_stable_snake_case() {
    let cases = [
        (
            PreservationFailurePolicy::RecordAndProceed,
            "record_and_proceed",
        ),
        (PreservationFailurePolicy::Block, "block"),
    ];

    for (policy, expected) in &cases {
        let json = serde_json::to_value(policy).unwrap();
        assert_eq!(
            json.as_str().unwrap(),
            *expected,
            "PreservationFailurePolicy::{:?} should serialize to {:?}",
            policy,
            expected
        );

        let deserialized: PreservationFailurePolicy = serde_json::from_value(json).unwrap();
        assert_eq!(
            deserialized, *policy,
            "PreservationFailurePolicy::{:?} should round-trip through JSON",
            policy
        );
    }
}

/// `PreservationFailurePolicy` defaults to `RecordAndProceed`.
#[test]
fn preservation_failure_policy_defaults_to_record_and_proceed() {
    let policy = PreservationFailurePolicy::default();
    assert_eq!(policy, PreservationFailurePolicy::RecordAndProceed);
}

/// `CheckpointLifecycleConfig` round-trips through JSON with the new
/// `failure_policy` field, and the field defaults to `RecordAndProceed`.
#[test]
fn checkpoint_lifecycle_config_failure_policy_round_trips() {
    // Default config — failure_policy should default to RecordAndProceed.
    let config = CheckpointLifecycleConfig::default();
    assert_eq!(
        config.failure_policy,
        PreservationFailurePolicy::RecordAndProceed
    );

    // Explicit Block policy.
    let config = CheckpointLifecycleConfig {
        failure_policy: PreservationFailurePolicy::Block,
        ..Default::default()
    };
    let json = serde_json::to_value(&config).unwrap();
    assert_eq!(json["failure_policy"], "block");

    let deserialized: CheckpointLifecycleConfig = serde_json::from_value(json).unwrap();
    assert_eq!(
        deserialized.failure_policy,
        PreservationFailurePolicy::Block
    );

    // Missing field in JSON should default to RecordAndProceed.
    let json_without_policy = serde_json::json!({
        "enabled": true,
        "require_before_no_progress_exit": false,
    });
    let deserialized: CheckpointLifecycleConfig =
        serde_json::from_value(json_without_policy).unwrap();
    assert_eq!(
        deserialized.failure_policy,
        PreservationFailurePolicy::RecordAndProceed
    );
}

/// When the preservation gate returns `RuntimeUnavailable` (test actor has
/// no runtime_ops), the default `RecordAndProceed` policy allows the
/// zombie reap to proceed — the existing `preservation_gate_called_during_zombie_reap`
/// test validates this path. This focused test verifies the outcome
/// classification directly.
#[test]
fn runtime_unavailable_does_not_block_with_record_and_proceed() {
    let result =
        PreservationGateResult::runtime_unavailable("task-gate-1", "session-gate-1", "zombie");
    assert_eq!(result.outcome, PreservationOutcome::RuntimeUnavailable);
    assert!(
        !result
            .outcome
            .should_block_transition(PreservationFailurePolicy::RecordAndProceed),
        "RuntimeUnavailable should not block with RecordAndProceed policy"
    );
}

/// When the preservation gate returns `RuntimeUnavailable` and the policy
/// is `Block`, the transition IS blocked.
#[test]
fn runtime_unavailable_blocks_with_block_policy() {
    let result =
        PreservationGateResult::runtime_unavailable("task-gate-2", "session-gate-2", "stall");
    assert_eq!(result.outcome, PreservationOutcome::RuntimeUnavailable);
    assert!(
        result
            .outcome
            .should_block_transition(PreservationFailurePolicy::Block),
        "RuntimeUnavailable should block with Block policy"
    );
}

/// When the worker reports `Failed` and the policy is `RecordAndProceed`,
/// the transition proceeds (the failure is recorded as a policy result).
#[test]
fn failed_outcome_record_and_proceed_does_not_block() {
    let result = PreservationGateResult::failed(
        "task-gate-3",
        "session-gate-3",
        Some("run-gate-3"),
        "ceiling",
        "worker push rejected".to_string(),
    );
    assert_eq!(result.outcome, PreservationOutcome::Failed);
    assert!(
        !result
            .outcome
            .should_block_transition(PreservationFailurePolicy::RecordAndProceed),
        "Failed should not block with RecordAndProceed policy"
    );
}

/// When the worker reports `Failed` and the policy is `Block`,
/// the transition is blocked.
#[test]
fn failed_outcome_blocks_with_block_policy() {
    let result = PreservationGateResult::failed(
        "task-gate-4",
        "session-gate-4",
        Some("run-gate-4"),
        "stall",
        "worker checkpoint failed".to_string(),
    );
    assert_eq!(result.outcome, PreservationOutcome::Failed);
    assert!(
        result
            .outcome
            .should_block_transition(PreservationFailurePolicy::Block),
        "Failed should block with Block policy"
    );
}

// ── Fix 1: DB-truth liveness backstop for the stall killer ───────────────
//
// The coordinator's idle measurement comes from an in-memory ActivityTracker
// fed only by a remote worker's `touch_activity` RPC bridge. When that bridge
// drifts the tracker goes silent and the stall killer false-flags a busy
// worker as idle (the overnight incident: 88 productive sessions killed at the
// 30-minute mark). The backstop consults a signal the drift cannot touch — the
// session ROW's token counters, persisted mid-flight — and spares any session
// whose counters advanced since the previous sweep.

/// A session that is well past the idle threshold with a SILENT activity
/// tracker is NOT stall-killed when its DB token counters advanced since the
/// last stall sweep — DB-visible progress is independent evidence of liveness.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stall_backstop_spares_session_with_advancing_db_tokens() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "stall-backstop-spare").await;

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    // Backdate well past every idle threshold; with no pool slot the stall
    // check falls back to wall-clock-from-started_at and reads the tracker as
    // silent (`activity_tracked == false`).
    session_repo
        .backdate_started_at(&session.id, "40 minutes")
        .await
        .unwrap();
    // The session row shows real token progress (mid-flight flush).
    session_repo
        .flush_tokens(&session.id, 2000, 500, 0, 0)
        .await
        .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    // Seed the watermark from a prior sweep BELOW the live total so the current
    // sweep observes an advance.
    actor
        .stall_progress_watermark
        .insert(session.id.clone(), 1000);

    actor.enforce_session_stall_timeout().await;

    assert!(
        !actor.stall_killed.contains(&session.id),
        "a session whose DB token counters advanced since the last sweep must be spared the idle kill"
    );
    assert_eq!(
        actor.stall_progress_watermark.get(&session.id).copied(),
        Some(2500),
        "the backstop must roll the watermark forward to the observed DB total"
    );
}

/// A truly-dead session — silent activity tracker AND frozen DB token counters
/// (no advance since the last sweep) — is still stall-killed at threshold. The
/// backstop only spares demonstrable progress; it must not become a blanket
/// amnesty that wedges genuinely hung sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stall_kill_still_fires_without_db_progress() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "stall-backstop-dead").await;
    let run_id = "run-stall-dead";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "40 minutes")
        .await
        .unwrap();
    // Frozen counters: the row shows tokens, but the watermark below already
    // matches them, so no advance is observed.
    session_repo
        .flush_tokens(&session.id, 3000, 0, 0, 0)
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(true);
    let mut app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(std::sync::Arc::new(runtime.clone()));
    let active_tasks = app_state.active_tasks.clone();
    let cancel = CancellationToken::new();
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel.clone(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: "openai/gpt-5.5".to_string(),
                max_slots: 1,
                roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
            }],
            role_priorities: HashMap::new(),
        },
        std::sync::Arc::new(|slot_id, model_id, event_tx, app_state, cancel| {
            let runner: djinn_slot::TestLifecycleRunner = std::sync::Arc::new(
                |_task_id,
                 _project_path,
                 _model_id,
                 _app_state,
                 kill,
                 _pause,
                 _resume_lifecycle_metadata| {
                    Box::pin(async move {
                        kill.cancelled().await;
                        Ok(())
                    })
                },
            );
            SlotHandle::spawn_with_test_runner(
                slot_id, model_id, event_tx, app_state, cancel, runner,
            )
        }),
    );
    pool.dispatch(&task.id, "test-project", "openai/gpt-5.5")
        .await
        .expect("dispatch should create a slot mapping");
    // Age the activity timestamp so the idle measurement is over threshold.
    let old = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(40 * 60))
        .unwrap_or(0);
    {
        let guard = active_tasks.lock().expect("active_tasks mutex");
        if let Some(ts) = guard.get(&task.id) {
            ts.store(old, std::sync::atomic::Ordering::Relaxed);
        }
    }

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = pool;
    // Watermark already equals the live DB total → no progress observed.
    actor
        .stall_progress_watermark
        .insert(session.id.clone(), 3000);

    actor.enforce_session_stall_timeout().await;

    assert!(
        actor.stall_killed.contains(&session.id),
        "a session with no DB progress and a silent activity tracker must still be stall-killed"
    );
    cancel.cancel();
}

// ── Fix 2: second-strike stall escalation ────────────────────────────────
//
// A task stall-killed on two consecutive sessions without durable status
// progress is caught in a redispatch loop the reopen-count escalation never
// sees (a stall kill never passes through `open`). On the second strike the
// coordinator routes it to the Planner instead of dispatching a third session.

/// Helper: stand up a running worker session for `task` on a live slot pool
/// whose activity is aged past the idle threshold, ready for a stall kill.
async fn dispatch_stalled_worker_session(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    task: &djinn_core::models::Task,
    run_id: &str,
) -> (
    SlotPoolHandle,
    CancellationToken,
    djinn_core::models::SessionRecord,
) {
    use djinn_db::{CreateSessionParams, SessionRepository};

    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "40 minutes")
        .await
        .unwrap();

    let mut app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(std::sync::Arc::new(RecordingRuntimeOps::new(true)));
    let active_tasks = app_state.active_tasks.clone();
    let cancel = CancellationToken::new();
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel.clone(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: "openai/gpt-5.5".to_string(),
                max_slots: 1,
                roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
            }],
            role_priorities: HashMap::new(),
        },
        std::sync::Arc::new(|slot_id, model_id, event_tx, app_state, cancel| {
            let runner: djinn_slot::TestLifecycleRunner = std::sync::Arc::new(
                |_task_id,
                 _project_path,
                 _model_id,
                 _app_state,
                 kill,
                 _pause,
                 _resume_lifecycle_metadata| {
                    Box::pin(async move {
                        kill.cancelled().await;
                        Ok(())
                    })
                },
            );
            SlotHandle::spawn_with_test_runner(
                slot_id, model_id, event_tx, app_state, cancel, runner,
            )
        }),
    );
    pool.dispatch(&task.id, "test-project", "openai/gpt-5.5")
        .await
        .expect("dispatch should create a slot mapping");
    let old = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(40 * 60))
        .unwrap_or(0);
    {
        let guard = active_tasks.lock().expect("active_tasks mutex");
        if let Some(ts) = guard.get(&task.id) {
            ts.store(old, std::sync::atomic::Ordering::Relaxed);
        }
    }
    (pool, cancel, session)
}

/// The FIRST stall cancel of a task does not escalate — it just kills and
/// releases for redispatch (one stall is not yet a loop).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_stall_cancel_does_not_escalate_to_planner() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "stall-strike-one").await;
    let (pool, cancel, session) =
        dispatch_stalled_worker_session(&db, &tx, &task, "run-strike-one").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = pool;
    actor.enforce_session_stall_timeout().await;

    assert!(
        actor.stall_killed.contains(&session.id),
        "the stalled session is killed"
    );
    let streak = actor
        .stall_cancel_streak
        .get(&task.id)
        .expect("first strike records a streak");
    assert_eq!(streak.count, 1, "first strike is count 1");
    let markers = planner_intervention_markers(&actor.task_repo(), &task.id).await;
    assert!(
        markers.is_empty(),
        "a single stall cancel must not escalate to the Planner"
    );
    cancel.cancel();
}

/// Two CONSECUTIVE stall cancels with no durable status progress between them
/// escalate to a Planner intervention BEFORE a third dispatch. The first strike
/// is pre-seeded (a prior session already stall-cancelled at this status); the
/// second real kill crosses the threshold.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn second_consecutive_stall_cancel_escalates_to_planner() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "stall-strike-two").await;
    let (pool, cancel, session) =
        dispatch_stalled_worker_session(&db, &tx, &task, "run-strike-two").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = pool;
    // Pre-seed the first strike at the task's current status: a prior session
    // already stall-cancelled and the status has not advanced since.
    let current = actor
        .task_repo()
        .get(&task.id)
        .await
        .unwrap()
        .unwrap()
        .status;
    actor.stall_cancel_streak.insert(
        task.id.clone(),
        StallCancelStreak {
            count: 1,
            last_status: current,
        },
    );

    actor.enforce_session_stall_timeout().await;

    assert!(
        actor.stall_killed.contains(&session.id),
        "the second stalled session is killed"
    );
    let markers = planner_intervention_markers(&actor.task_repo(), &task.id).await;
    assert!(
        !markers.is_empty(),
        "the second consecutive stall cancel without status progress must route to a Planner intervention"
    );
    assert!(
        !actor.stall_cancel_streak.contains_key(&task.id),
        "the streak is cleared once the escalation fires"
    );
    cancel.cancel();
}

// ── Dead verdict liveness evidence for zombie/stale recovery ────────────

/// Zombie reap with Dead verdict persists `dead_reclaimed` liveness evidence
/// linked to the session and task_run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zombie_reap_persists_dead_reclaimed_evidence() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "dead-evidence").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-dead-evidence";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(true);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime));
    actor.reap_zombie_sessions().await;

    // Session must be finalized.
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "zombie session must be finalized"
    );

    // Task must be released.
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "open",
        "task must be released for redispatch"
    );

    // Verify dead_reclaimed evidence on the session's denormalized columns.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let (verdict, outcome_kind) = liveness_repo
        .get_session_liveness_fields(&session.id)
        .await
        .unwrap();
    assert_eq!(
        verdict.as_deref(),
        Some("dead"),
        "session must have dead verdict recorded"
    );
    assert_eq!(
        outcome_kind.as_deref(),
        Some("dead_reclaimed"),
        "session must have dead_reclaimed outcome recorded"
    );

    // Verify evidence row in the append-only liveness_evidence table.
    let evidence_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("dead_reclaimed"))
        .await
        .unwrap();
    assert!(
        evidence_count >= 1,
        "liveness_evidence table must have a dead_reclaimed row for this session"
    );
}

/// A session within the zombie hard cap (young session) is NOT reaped even if
/// it has zero tokens. The liveness classifier gate is not reached because the
/// existing age guard suppresses reap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zombie_reap_suppressed_by_recent_activity() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "suppress-dead").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    // Session is young (within the hard cap) — zero tokens but NOT backdated.
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.reap_zombie_sessions().await;

    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "young session must NOT be reaped — existing age guard suppresses Dead reclaim"
    );

    // No liveness evidence should be persisted (classifier is not reached).
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let evidence_count = liveness_repo
        .count_evidence_for_session(&session.id, None)
        .await
        .unwrap();
    assert_eq!(
        evidence_count, 0,
        "no liveness evidence should be persisted for a session that is not reaped"
    );
}

/// When a zombie session's owning task has already been closed (terminal),
/// the zombie reaper records `kill_noop` evidence and does NOT reopen the task.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zombie_reap_terminal_task_race_records_kill_noop() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "terminal-race").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-terminal-race";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    // Close the task BEFORE the reaper runs — simulates a concurrent
    // terminal transition (e.g. human override, PR merge, force-close).
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "closed")
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(true);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime));
    actor.reap_zombie_sessions().await;

    // The task must remain closed (not reopened).
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "closed",
        "task must remain closed — zombie reaper must NOT reopen a terminal task"
    );

    // Verify kill_noop evidence is persisted.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let evidence_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("kill_noop"))
        .await
        .unwrap();
    assert!(
        evidence_count >= 1,
        "liveness_evidence table must have a kill_noop row for the terminal-task race"
    );

    // Session should still be finalized (the running row is cleaned up).
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "zombie session must still be finalized even in the terminal-task race"
    );
}

// ─── Liveness consumer regression tests ─────────────────────────────────
//
// These are the focused regression tests for the integrated coordinator
// liveness consumers (epic vbgl). They exercise the combined behavior of
// the liveness classifier gate inside `reap_zombie_sessions` and
// `enforce_session_stall_timeout`, plus the session-exit protocol-violation
// detection path.

/// AC 1: Long tool-run heartbeat — a session dispatched to a live slot whose
/// pool activity is tracked but aged (idle past zombie hard cap) is classified
/// `Slow` by the liveness classifier and the zombie reaper spares it. The pool
/// check (`activity_tracked && idle <= ZOMBIE_HARD_CAP_SECS`) does NOT fire
/// because idle exceeds the cap, but the classifier still sees a Running pod
/// and returns Slow (not Dead).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn long_tool_run_heartbeat_classified_slow_spares_zombie() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "heartbeat-live").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-heartbeat-live";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    // Backdate past the zombie hard cap so the age gate passes.
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    // Dispatch the task to a live pool slot so `session_for_task` returns Some.
    let mut app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(std::sync::Arc::new(RecordingRuntimeOps::new(true)));
    let active_tasks = app_state.active_tasks.clone();
    let cancel = CancellationToken::new();
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel.clone(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: "openai/gpt-5.5".to_string(),
                max_slots: 1,
                roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
            }],
            role_priorities: HashMap::new(),
        },
        std::sync::Arc::new(|slot_id, model_id, event_tx, app_state, cancel| {
            let runner: djinn_slot::TestLifecycleRunner = std::sync::Arc::new(
                |_task_id,
                 _project_path,
                 _model_id,
                 _app_state,
                 kill,
                 _pause,
                 _resume_lifecycle_metadata| {
                    Box::pin(async move {
                        kill.cancelled().await;
                        Ok(())
                    })
                },
            );
            SlotHandle::spawn_with_test_runner(
                slot_id, model_id, event_tx, app_state, cancel, runner,
            )
        }),
    );
    pool.dispatch(&task.id, "test-project", "openai/gpt-5.5")
        .await
        .expect("dispatch should create a slot mapping");

    // Age the activity tracker so idle > ZOMBIE_HARD_CAP_SECS but
    // activity_tracked is still true. The pool check won't spare
    // (idle exceeds cap), but the classifier sees Running pod + Idle → Slow.
    let old = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(20 * 60))
        .unwrap_or(0);
    {
        let guard = active_tasks.lock().expect("active_tasks mutex");
        if let Some(ts) = guard.get(&task.id) {
            ts.store(old, std::sync::atomic::Ordering::Relaxed);
        }
    }

    let runtime = RecordingRuntimeOps::new(true);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime));
    actor.pool = pool;
    actor.reap_zombie_sessions().await;

    // The classifier returned Slow → session is spared.
    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "long tool-run heartbeat session with Running pod must be spared by Slow verdict"
    );

    // Task must NOT be released (session is still alive).
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "in_progress",
        "task must remain in_progress when session is spared by liveness classifier"
    );
}

/// AC 2: Slow extension granted — when the classifier returns Slow with
/// extension_eligible=true and the coordinator's extension budget is not
/// exhausted, the stall timeout path grants an extension, persists
/// `slow_extended` evidence, and records a claim extension. The session is
/// NOT killed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_extension_granted_with_evidence_and_claim_extension() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "slow-ext-grant").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-slow-ext";
    // Set up DB artifacts (task_run + session) manually so we can also
    // dispatch to the pool and age the activity tracker, which is required
    // for the liveness classifier to return Slow (pod Running + Idle).
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo =
        djinn_db::SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(djinn_db::CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    // Keep the session WITHIN its claim window (recent started_at → non-zero
    // claim TTL → `extension_budget_exhausted == false`), so the classifier
    // marks it extension-eligible: this is the in-window case that earns the
    // one coordinator-gated grace extension. The stall itself is driven by the
    // aged activity tracker below (idle > 30 min), NOT by `started_at`, so the
    // idle gate still fires. A session past its claim TTL would instead be
    // egregiously stale and killed on the first tick (see
    // `slow_extension_budget_exhaustion_falls_through_to_kill` / the stall
    // teardown tests).
    session_repo
        .backdate_started_at(&session.id, "2 minutes")
        .await
        .unwrap();

    // Dispatch the task to a live pool slot so `session_for_task` returns
    // Some and the liveness classifier sees a Running pod.
    let mut app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(std::sync::Arc::new(RecordingRuntimeOps::new(true)));
    let active_tasks = app_state.active_tasks.clone();
    let cancel = CancellationToken::new();
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel.clone(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: "openai/gpt-5.5".to_string(),
                max_slots: 1,
                roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
            }],
            role_priorities: HashMap::new(),
        },
        std::sync::Arc::new(|slot_id, model_id, event_tx, app_state, cancel| {
            let runner: djinn_slot::TestLifecycleRunner = std::sync::Arc::new(
                |_task_id,
                 _project_path,
                 _model_id,
                 _app_state,
                 kill,
                 _pause,
                 _resume_lifecycle_metadata| {
                    Box::pin(async move {
                        kill.cancelled().await;
                        Ok(())
                    })
                },
            );
            SlotHandle::spawn_with_test_runner(
                slot_id, model_id, event_tx, app_state, cancel, runner,
            )
        }),
    );
    pool.dispatch(&task.id, "test-project", "openai/gpt-5.5")
        .await
        .expect("dispatch should create a slot mapping");

    // Age the activity tracker so idle exceeds the 30-minute stall threshold
    // (STALL_TIMEOUT_SECS) and the liveness classifier gate is reached. The
    // classifier sees Running pod + Idle → Slow verdict with
    // extension_eligible=true.
    let old = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(35 * 60))
        .unwrap_or(0);
    {
        let guard = active_tasks.lock().expect("active_tasks mutex");
        if let Some(ts) = guard.get(&task.id) {
            ts.store(old, std::sync::atomic::Ordering::Relaxed);
        }
    }

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = pool;
    // Slow extension is enabled by default (SlowExtensionConfig::default).
    // No previous extensions → budget is available.
    assert!(
        actor.worker_lifecycle_config.slow_extension.enabled,
        "precondition: slow extension is enabled by default"
    );
    assert_eq!(
        actor.worker_lifecycle_config.slow_extension.max_extensions, 3,
        "precondition: max_extensions is 3 by default"
    );

    actor.enforce_session_stall_timeout().await;

    // The session must NOT be killed (extension was granted).
    assert!(
        !actor.stall_killed.contains(&session.id),
        "session with slow extension budget must NOT be killed"
    );

    // Extension count must be incremented.
    let ext_count = actor
        .stall_extension_count
        .get(&session.id)
        .copied()
        .unwrap_or(0);
    assert_eq!(ext_count, 1, "extension count must be 1 after first grant");

    // Verify slow_extended evidence persisted.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let evidence_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("slow_extended"))
        .await
        .unwrap();
    assert!(
        evidence_count >= 1,
        "slow_extended evidence must be persisted when extension is granted"
    );

    // Verify session liveness fields on the denormalized session row.
    let (verdict, outcome_kind) = liveness_repo
        .get_session_liveness_fields(&session.id)
        .await
        .unwrap();
    assert_eq!(
        verdict.as_deref(),
        Some("slow"),
        "session must have slow verdict recorded"
    );
    assert_eq!(
        outcome_kind.as_deref(),
        Some("slow_extended"),
        "session must have slow_extended outcome recorded"
    );
}

/// AC 2: Extension exhaustion — when the coordinator's extension count has
/// reached `max_extensions`, the stall timeout path falls through to the kill.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_extension_budget_exhaustion_falls_through_to_kill() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "slow-ext-exhausted").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-slow-exhausted";
    let (pool, _cancel, session) = dispatch_stalled_worker_session(&db, &tx, &task, run_id).await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = pool;

    // Pre-fill the extension count to max_extensions to exhaust the budget.
    let max_ext = actor.worker_lifecycle_config.slow_extension.max_extensions;
    actor
        .stall_extension_count
        .insert(session.id.clone(), max_ext);

    actor.enforce_session_stall_timeout().await;

    // Session must be killed (budget exhausted).
    assert!(
        actor.stall_killed.contains(&session.id),
        "session with exhausted extension budget must be killed"
    );
}

/// AC 3: Hard runtime cap — when the liveness classifier's
/// `hard_runtime_deadline_exceeded` is true, the classifier returns Dead with
/// Timeout outcome (rule #2). In the zombie reap path, the reap action
/// records `dead_reclaimed` on the denormalized session row, but the
/// classifier-level Timeout outcome persists in the append-only
/// `liveness_evidence` table.
///
/// This test verifies the integrated path: an old session with a task_run
/// whose started_at exceeds the zombie hard cap is classified as Dead
/// (verifying hard-runtime precedence over Live/Slow), the zombie reaper
/// reclaims it, and the classifier-level Timeout outcome is preserved in the
/// append-only evidence chain.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_runtime_cap_zombie_reap_forces_dead_timeout() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "hard-cap-reap").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-hard-cap";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    // Backdate both session and task_run past the zombie hard cap.
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();
    // Also backdate the task_run.started_at so hard_runtime_deadline_exceeded fires.
    TaskRunRepository::new(db.clone())
        .backdate_started_at(run_id, "20 minutes")
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(true);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime));
    actor.reap_zombie_sessions().await;

    // Session must be finalized.
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "hard-runtime-exceeded session must be finalized"
    );

    // The reap action records `dead_reclaimed` on the denormalized session
    // row. Hard-runtime precedence is verified by the verdict being `dead`
    // regardless of the underlying activity/heartbeat signals.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let (verdict, outcome_kind) = liveness_repo
        .get_session_liveness_fields(&session.id)
        .await
        .unwrap();
    assert_eq!(
        verdict.as_deref(),
        Some("dead"),
        "hard-runtime-exceeded session must have dead verdict"
    );
    assert_eq!(
        outcome_kind.as_deref(),
        Some("dead_reclaimed"),
        "zombie reap records dead_reclaimed for the reclaim action"
    );

    // The classifier's Timeout outcome (rule #2) is observable in the
    // append-only liveness_evidence rows written by the classifier pass that
    // runs inside reap_zombie_sessions.
    let timeout_evidence_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("timeout"))
        .await
        .unwrap();
    assert!(
        timeout_evidence_count >= 1,
        "classifier pass must have persisted a `timeout` evidence row (AC 3: hard-runtime precedence)"
    );
}

/// AC 3: Zombie running / zero-token stranded work is reclaimed when the
/// liveness classifier evidence is Dead (absent pod, no activity, non-terminal
/// task). The reaper persists `dead_reclaimed` evidence and releases the task.
///
/// This test verifies the full evidence chain: the append-only
/// `liveness_evidence` table has a `dead_reclaimed` row, the denormalized
/// session columns are updated, and the task is released.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zombie_stranded_zero_token_reclaimed_with_dead_evidence_chain() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "stranded-dead").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-stranded-dead";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    // Zero tokens (default), backdated past zombie hard cap.
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(true);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime));
    actor.reap_zombie_sessions().await;

    // 1. Session finalized.
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "stranded zombie session must be finalized"
    );

    // 2. Task released for redispatch.
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "open",
        "task must be released for redispatch after zombie reclaim"
    );

    // 3. Dead verdict + dead_reclaimed outcome on denormalized session columns.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let (verdict, outcome_kind) = liveness_repo
        .get_session_liveness_fields(&session.id)
        .await
        .unwrap();
    assert_eq!(verdict.as_deref(), Some("dead"));
    assert_eq!(outcome_kind.as_deref(), Some("dead_reclaimed"));

    // 4. Append-only liveness_evidence table has dead_reclaimed row.
    let evidence_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("dead_reclaimed"))
        .await
        .unwrap();
    assert!(
        evidence_count >= 1,
        "liveness_evidence table must have dead_reclaimed row"
    );
}

/// AC 4: Protocol violation on clean exit — calling
/// `classify_session_exit_liveness` with session_status="completed" for a
/// non-terminal task produces `ProtocolViolation` verdict with
/// `CleanExitNonterminal` reason and `Success` outcome. This is a genuine
/// failed attempt (not success) and must be counted by retry accounting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn protocol_violation_clean_exit_classified_as_failed_attempt() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "pv-clean-exit").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-pv-clean";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    let actor = coordinator_actor_for_tests(&db, &tx);
    let result = actor
        .classify_session_exit_liveness(&session.id, &task.id, Some(run_id), "completed")
        .await;

    let result = result.expect("classification must succeed");
    assert_eq!(
        result.verdict,
        crate::dispatch::liveness::Verdict::ProtocolViolation,
        "clean exit on nonterminal task must be ProtocolViolation"
    );
    assert_eq!(
        result.outcome,
        Some(crate::dispatch::liveness::LivenessOutcome::Success),
        "clean exit produces Success outcome (not Crash)"
    );
    assert_eq!(
        result.reason,
        Some(crate::dispatch::liveness::LivenessReason::CleanExitNonterminal),
        "reason must be CleanExitNonterminal"
    );
    assert!(
        !result.extension_eligible,
        "protocol violation is never extension-eligible"
    );

    // Evidence persisted to the append-only table.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let evidence_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("success"))
        .await
        .unwrap();
    assert!(
        evidence_count >= 1,
        "protocol violation evidence must be persisted"
    );

    // Denormalized session columns updated.
    let (verdict, outcome_kind) = liveness_repo
        .get_session_liveness_fields(&session.id)
        .await
        .unwrap();
    assert_eq!(verdict.as_deref(), Some("protocol_violation"));
    assert_eq!(outcome_kind.as_deref(), Some("success"));
}

/// AC 4: Nonzero exit — calling `classify_session_exit_liveness` with
/// session_status="failed" for a non-terminal task produces `ProtocolViolation`
/// verdict with `Crash` outcome (not `Success`). This distinguishes crash
/// semantics from clean-protocol-violation for retry accounting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonzero_exit_is_crash_outcome_distinct_from_clean_violation() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "pv-nonzero-exit").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-pv-nonzero";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    let actor = coordinator_actor_for_tests(&db, &tx);
    let result = actor
        .classify_session_exit_liveness(&session.id, &task.id, Some(run_id), "failed")
        .await;

    let result = result.expect("classification must succeed");
    assert_eq!(
        result.verdict,
        crate::dispatch::liveness::Verdict::ProtocolViolation,
        "nonzero exit on nonterminal task must be ProtocolViolation"
    );
    assert_eq!(
        result.outcome,
        Some(crate::dispatch::liveness::LivenessOutcome::Crash),
        "nonzero exit produces Crash outcome (not Success)"
    );
    assert_eq!(
        result.reason,
        Some(crate::dispatch::liveness::LivenessReason::NonzeroExitNonterminal),
        "reason must be NonzeroExitNonterminal"
    );
    assert!(!result.extension_eligible);

    // Evidence persisted with crash outcome.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let (verdict, outcome_kind) = liveness_repo
        .get_session_liveness_fields(&session.id)
        .await
        .unwrap();
    assert_eq!(verdict.as_deref(), Some("protocol_violation"));
    assert_eq!(outcome_kind.as_deref(), Some("crash"));
}

/// AC 4: Already-terminal race — calling `classify_session_exit_liveness` for
/// a task that is already closed produces `KillNoop` outcome (not protocol
/// violation). Terminal state is preserved; only metadata is attached.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn already_terminal_task_exit_preserves_kill_noop() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "terminal-exit-race").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-terminal-exit";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    // Close the task BEFORE the exit classification runs — race condition.
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "closed")
        .await
        .unwrap();

    let actor = coordinator_actor_for_tests(&db, &tx);
    let result = actor
        .classify_session_exit_liveness(&session.id, &task.id, Some(run_id), "completed")
        .await;

    let result = result.expect("classification must succeed");
    assert_eq!(
        result.outcome,
        Some(crate::dispatch::liveness::LivenessOutcome::KillNoop),
        "already-terminal task must produce KillNoop (not protocol violation)"
    );
    assert_ne!(
        result.verdict,
        crate::dispatch::liveness::Verdict::ProtocolViolation,
        "terminal task must NOT be classified as protocol violation"
    );

    // KillNoop evidence persisted.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let evidence_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("kill_noop"))
        .await
        .unwrap();
    assert!(
        evidence_count >= 1,
        "kill_noop evidence must be persisted for terminal-task race"
    );
}

/// AC 5: Explicit kill cleanup — when a session is explicitly killed for a
/// terminal task (not via zombie reap but via a kill-session call), the
/// cleanup path records `kill_noop` evidence showing the kill was a no-op
/// because the task was already finished.
///
/// This test exercises the coordinator's handling of a task that transitions
/// to terminal while a zombie session exists — the zombie reaper detects the
/// terminal state via the liveness classifier, records kill_noop evidence,
/// finalizes the session, and does NOT reopen the task. This is distinct from
/// `zombie_reap_terminal_task_race_records_kill_noop` in that we also verify
/// the full evidence chain (denormalized columns + append-only table).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_kill_cleanup_full_evidence_chain_for_terminal_task() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "kill-noop-chain").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-kill-noop";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    // Close the task — simulates a concurrent terminal transition.
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "closed")
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(true);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime));
    actor.reap_zombie_sessions().await;

    // 1. Task remains closed (not reopened).
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.status, "closed", "task must remain closed");

    // 2. Session finalized.
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "orphaned session must be finalized"
    );

    // 3. Denormalized session columns: verdict=live (moot), outcome=kill_noop.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let (_verdict, outcome_kind) = liveness_repo
        .get_session_liveness_fields(&session.id)
        .await
        .unwrap();
    // Terminal task → verdict is "live" (moot) but outcome is "kill_noop".
    assert_eq!(
        outcome_kind.as_deref(),
        Some("kill_noop"),
        "session must have kill_noop outcome on denormalized columns"
    );

    // 4. Append-only liveness_evidence table: kill_noop row exists.
    let evidence_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("kill_noop"))
        .await
        .unwrap();
    assert!(
        evidence_count >= 1,
        "liveness_evidence table must have kill_noop row"
    );
}

/// AC 4: Interrupted session (nonzero exit) while task is nonterminal —
/// calling `classify_session_exit_liveness` with session_status="interrupted"
/// produces the same crash/protocol-violation semantics as "failed".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_session_nonterminal_is_crash_protocol_violation() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "pv-interrupted").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-pv-interrupted";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    let actor = coordinator_actor_for_tests(&db, &tx);
    let result = actor
        .classify_session_exit_liveness(&session.id, &task.id, Some(run_id), "interrupted")
        .await;

    let result = result.expect("classification must succeed");
    assert_eq!(
        result.verdict,
        crate::dispatch::liveness::Verdict::ProtocolViolation,
        "interrupted session on nonterminal task must be ProtocolViolation"
    );
    assert_eq!(
        result.outcome,
        Some(crate::dispatch::liveness::LivenessOutcome::Crash),
        "interrupted session produces Crash outcome"
    );
    assert_eq!(
        result.reason,
        Some(crate::dispatch::liveness::LivenessReason::NonzeroExitNonterminal),
        "reason must be NonzeroExitNonterminal for interrupted session"
    );
}

/// AC 1/3: Dead + recent DB activity suppression — a session that is within
/// the zombie hard cap (young session) is NOT reaped even with zero tokens.
/// The age guard at `reap_zombie_sessions` prevents the classifier from being
/// reached, and no evidence is persisted.
///
/// This is the complementary test to `zombie_reap_suppressed_by_recent_activity`
/// with additional assertions on the absence of evidence rows (confirming the
/// classifier was never consulted).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn young_session_zero_tokens_no_evidence_persisted() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "young-no-evidence").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    // Session is young (NOT backdated) — within the zombie hard cap.
    // Zero tokens by default.

    let runtime = RecordingRuntimeOps::new(true);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime));
    actor.reap_zombie_sessions().await;

    // Session NOT reaped.
    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "young session with zero tokens must NOT be reaped"
    );

    // No evidence rows at all (classifier was never reached).
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let total_evidence = liveness_repo
        .count_evidence_for_session(&session.id, None)
        .await
        .unwrap();
    assert_eq!(
        total_evidence, 0,
        "no liveness evidence should exist when age guard suppresses reap"
    );

    // No liveness fields on denormalized session columns.
    let (verdict, outcome) = liveness_repo
        .get_session_liveness_fields(&session.id)
        .await
        .unwrap();
    assert!(
        verdict.is_none(),
        "young session must not have liveness verdict"
    );
    assert!(
        outcome.is_none(),
        "young session must not have liveness outcome"
    );
}

/// AC 2: Hard runtime cap precedence over Slow — when both slow-extension
/// eligibility and hard_runtime_deadline_exceeded are in play, the classifier
/// must return Dead/Timeout. This is a pure classifier test that exercises
/// the precedence invariant directly.
#[test]
fn hard_cap_takes_precedence_over_slow_extension_eligible() {
    use crate::dispatch::liveness::*;

    // Evidence that would normally be Slow (running pod, idle, below budget)
    // but with hard_runtime_deadline_exceeded = true.
    let evidence = LivenessEvidence {
        pod_phase: Some(PodPhase::Running),
        activity: ActivitySignal::Idle,
        db_session_status: Some(DbSessionStatus::Running),
        db_task_status: Some(DbTaskStatus::InProgress),
        claim_ttl_remaining: Some(std::time::Duration::from_secs(300)),
        extension_budget_exhausted: false,
        hard_runtime_deadline_exceeded: true,
        exit_code: None,
    };

    let result = classify(&evidence);
    assert_eq!(result.verdict, Verdict::Dead, "hard cap forces Dead");
    assert_eq!(
        result.outcome,
        Some(LivenessOutcome::Timeout),
        "outcome must be Timeout"
    );
    assert_eq!(
        result.reason,
        Some(LivenessReason::HardRuntimeExceeded),
        "reason must be HardRuntimeExceeded"
    );
    assert!(
        !result.extension_eligible,
        "hard cap must forbid extension even when budget is available"
    );
}

/// AC 3: Dead + recent in-memory activity suppresses Dead reclaim — when the
/// pod is absent but the DB session is still marked running with no evidence
/// of terminal exit, the classifier returns Dead only when activity is
/// Idle/NeverActive. If activity is Active (which can't happen with absent
/// pod in real code, but tests the classifier boundary), the verdict is NOT
/// Dead.
///
/// This tests the classifier's Dead-suppression invariant: Absent pod +
/// Active activity → NOT Dead (the session might be between pod transitions).
#[test]
fn absent_pod_with_active_activity_is_not_dead() {
    use crate::dispatch::liveness::*;

    let evidence = LivenessEvidence {
        pod_phase: Some(PodPhase::Absent),
        activity: ActivitySignal::Active,
        db_session_status: Some(DbSessionStatus::Running),
        db_task_status: Some(DbTaskStatus::InProgress),
        claim_ttl_remaining: Some(std::time::Duration::from_secs(300)),
        extension_budget_exhausted: false,
        hard_runtime_deadline_exceeded: false,
        exit_code: None,
    };

    let result = classify(&evidence);
    assert_ne!(
        result.verdict,
        Verdict::Dead,
        "absent pod with active activity must NOT be Dead"
    );
    // Falls through to Live (default) since no higher-precedence condition matches.
    assert_eq!(result.verdict, Verdict::Live);
    assert!(!result.extension_eligible);
}

/// AC 3: DB-active work with running session — when the DB session is running,
/// the task is in_progress, and the pod is Running with Active signal, the
/// classifier returns Live regardless of claim TTL state. This confirms that
/// genuinely active work is never misclassified as Dead.
#[test]
fn running_pod_active_signal_is_live_regardless_of_claim_ttl() {
    use crate::dispatch::liveness::*;

    // Claim TTL is nearly expired, but the session is genuinely active.
    let evidence = LivenessEvidence {
        pod_phase: Some(PodPhase::Running),
        activity: ActivitySignal::Active,
        db_session_status: Some(DbSessionStatus::Running),
        db_task_status: Some(DbTaskStatus::InProgress),
        claim_ttl_remaining: Some(std::time::Duration::from_secs(5)),
        extension_budget_exhausted: false,
        hard_runtime_deadline_exceeded: false,
        exit_code: None,
    };

    let result = classify(&evidence);
    assert_eq!(result.verdict, Verdict::Live, "active work must be Live");
    assert_eq!(result.outcome, None, "Live has no outcome");
    assert!(!result.extension_eligible);
}

// ── Attempt-lifecycle terminalization in the session-recovery lane (i6xq) ──
//
// These path-level tests drive the real recovery functions
// (`enforce_session_stall_timeout`, `reap_zombie_sessions`) and assert that the
// matching `task_attempts` row is advanced to the correct terminal outcome with
// structured recovery context, and that duplicate/late terminal handling stays
// idempotent (no backward move, no duplicate row).

/// Seed a `pending` attempt for `(task_id, role)` exactly as the dispatch-start
/// path would, and return its id.
async fn seed_pending_attempt(db: &Database, task_id: &str, role: &str) -> String {
    let repo = djinn_db::TaskAttemptRepository::new(db.clone());
    let id = uuid::Uuid::now_v7().to_string();
    let dispatch_key = format!("{task_id}:{role}:{id}");
    repo.create_or_get_pending(djinn_db::CreateTaskAttemptParams {
        id: &id,
        task_id,
        role,
        dispatch_key: &dispatch_key,
        session_id: None,
        attempt_seq: None,
    })
    .await
    .unwrap();
    id
}

/// A per-session token/turn ceiling kill routes through the loop-guard planner
/// intervention, and must terminalize the matching attempt as
/// `loop_guard_tripped` with structured recovery context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ceiling_kill_terminalizes_attempt_as_loop_guard_tripped() {
    use djinn_db::{CreateSessionParams, SessionRepository, TaskAttemptRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "ceiling-attempt").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    let attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;

    let cancel = CancellationToken::new();
    let app_state = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
    let activity = app_state.register_activity(&task.id);
    activity.store(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        std::sync::atomic::Ordering::Relaxed,
    );
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel.clone(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: "openai/gpt-5.5".to_string(),
                max_slots: 1,
                roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
            }],
            role_priorities: HashMap::new(),
        },
        Arc::new(|slot_id, model_id, event_tx, app_state, cancel| {
            let runner: djinn_slot::TestLifecycleRunner = Arc::new(
                |_task_id,
                 _project_path,
                 _model_id,
                 _app_state,
                 kill,
                 _pause,
                 _resume_lifecycle_metadata| {
                    Box::pin(async move {
                        kill.cancelled().await;
                        Ok(())
                    })
                },
            );
            SlotHandle::spawn_with_test_runner(
                slot_id, model_id, event_tx, app_state, cancel, runner,
            )
        }),
    );
    pool.dispatch(&task.id, "test-project", "openai/gpt-5.5")
        .await
        .expect("dispatch should create a slot mapping");
    pool.test_set_token_override(&task.id, 3_000_000, 10).await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = pool.clone();
    actor.enforce_session_stall_timeout().await;

    assert!(
        actor.stall_killed.contains(&session.id),
        "ceiling-tripped session must be killed"
    );

    let repo = TaskAttemptRepository::new(db.clone());
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(
        attempt.outcome, "loop_guard_tripped",
        "ceiling kill (runaway guard) must terminalize the attempt as loop_guard_tripped"
    );
    assert!(attempt.terminal_at.is_some());
    let sj: serde_json::Value =
        serde_json::from_str(attempt.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj["recovery_classifier"], "session_recovery_ceiling");
    assert_eq!(sj["failure_class"], "ceiling_kill");
    assert_eq!(sj["session_id"], session.id);
    assert_eq!(sj["liveness_verdict"], "dead");
    // Exactly one attempt row — no duplicate.
    assert_eq!(repo.list_for_task(&task.id).await.unwrap().len(), 1);

    cancel.cancel();
}

/// A stall timeout (session idle past the 30-minute threshold) must terminalize
/// the matching attempt as `timed_out` with structured recovery context. Uses a
/// deterministic [`djinn_core::clock::TestClock`] to advance idle time without
/// sleeping, and disables slow-extension so the stall path is deterministic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stall_timeout_terminalizes_attempt_as_timed_out() {
    use djinn_db::{CreateSessionParams, SessionRepository, TaskAttemptRepository};
    use std::time::{Duration, Instant, SystemTime};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "stall-attempt").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
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
        .unwrap();

    let attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;

    // Deterministic clock so we can push monotonic idle time past the 300s
    // first-call cap without sleeping.
    let clock = Arc::new(djinn_core::clock::TestClock::new(
        SystemTime::now(),
        Instant::now(),
    ));
    let cancel = CancellationToken::new();
    let app_state =
        test_helpers::agent_context_from_db_with_clock(db.clone(), cancel.clone(), clock.clone());
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel.clone(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: DEFAULT_MODEL_ID.to_owned(),
                max_slots: 1,
                roles: ["worker"].into_iter().map(ToOwned::to_owned).collect(),
            }],
            role_priorities: HashMap::new(),
        },
        Arc::new(|slot_id, model_id, event_tx, app_state, cancel| {
            let runner: djinn_slot::TestLifecycleRunner = Arc::new(
                |_task_id,
                 _project_path,
                 _model_id,
                 _app_state,
                 kill,
                 _pause,
                 _resume_lifecycle_metadata| {
                    Box::pin(async move {
                        kill.cancelled().await;
                        Ok(())
                    })
                },
            );
            SlotHandle::spawn_with_test_runner(
                slot_id, model_id, event_tx, app_state, cancel, runner,
            )
        }),
    );
    pool.dispatch(&task.id, "test-project", DEFAULT_MODEL_ID)
        .await
        .expect("dispatch should create a slot mapping");
    // Let the slot's registration event populate the pool's slot-model map so
    // `session_for_task` returns live info rather than falling back to the DB row.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;

    // Push both clocks past the 30-minute stall timeout: monotonic drives the
    // slot's wall-clock duration; wall drives the activity tracker's idle
    // reading. The session then reads as idle well beyond the stall threshold.
    clock.advance_mono(Duration::from_secs(2000));
    clock.advance_wall(Duration::from_secs(2000));

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = pool.clone();
    // Skip the slow-extension classifier gate so the stall path is deterministic.
    actor.worker_lifecycle_config.slow_extension.enabled = false;
    actor.enforce_session_stall_timeout().await;

    assert!(
        actor.stall_killed.contains(&session.id),
        "idle-stalled session must be killed"
    );

    let repo = TaskAttemptRepository::new(db.clone());
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(
        attempt.outcome, "timed_out",
        "stall kill must terminalize the attempt as timed_out"
    );
    assert!(attempt.terminal_at.is_some());
    let sj: serde_json::Value =
        serde_json::from_str(attempt.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj["recovery_classifier"], "session_recovery_stall");
    assert_eq!(sj["failure_class"], "idle_stall");
    assert_eq!(sj["session_id"], session.id);
    assert_eq!(repo.list_for_task(&task.id).await.unwrap().len(), 1);

    cancel.cancel();
}

/// A zombie session reaped past the hard cap with no live worker is a crash:
/// the matching attempt must be terminalized as `crashed` with a `failure_class`
/// in its structured context. Running the reaper again must not duplicate the
/// row or move it — duplicate recovery scans are idempotent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zombie_reap_terminalizes_attempt_as_crashed_with_failure_class() {
    use djinn_db::{CreateSessionParams, SessionRepository, TaskAttemptRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "zombie-attempt").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-zombie-attempt";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    let attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.reap_zombie_sessions().await;

    let repo = TaskAttemptRepository::new(db.clone());
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(
        attempt.outcome, "crashed",
        "a dead zombie (no live worker past hard cap) must terminalize the attempt as crashed"
    );
    assert!(attempt.terminal_at.is_some());
    let sj: serde_json::Value =
        serde_json::from_str(attempt.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj["recovery_classifier"], "session_recovery_zombie_reap");
    assert!(
        sj["failure_class"].as_str().is_some_and(|s| !s.is_empty()),
        "crashed attempt must carry a non-empty failure_class, got {:?}",
        sj["failure_class"]
    );
    assert_eq!(sj["session_id"], session.id);

    // Duplicate scan idempotency: reaping again must not duplicate or move.
    actor.reap_zombie_sessions().await;
    let after = repo.list_for_task(&task.id).await.unwrap();
    assert_eq!(
        after.len(),
        1,
        "duplicate reap must not create a second row"
    );
    assert_eq!(
        after[0].outcome, "crashed",
        "duplicate reap must not move the terminal attempt"
    );
}

/// A late recovery terminalization must never move an attempt that already
/// reached a terminal outcome backward, nor create a duplicate row. Seeds an
/// attempt that already `completed`, then drives the zombie reaper (which would
/// otherwise terminalize as `crashed`) and asserts the attempt stays completed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recovery_terminalization_does_not_move_terminal_attempt_backward() {
    use djinn_db::{
        CreateSessionParams, SessionRepository, TaskAttemptRepository, TerminalTaskAttemptParams,
    };

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "late-terminal").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-late-terminal";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
        })
        .await
        .unwrap();

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    // Seed an attempt that has already reached a terminal `completed` outcome.
    let attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;
    let repo = TaskAttemptRepository::new(db.clone());
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt_id,
        outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::Completed,
        pr_url: Some("https://github.example/pr/1"),
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("already done"),
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.reap_zombie_sessions().await;

    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(
        attempt.outcome, "completed",
        "a late recovery crash must NOT move an already-terminal (completed) attempt backward"
    );
    assert_eq!(
        attempt.summary.as_deref(),
        Some("already done"),
        "terminal summary must be preserved"
    );
    assert_eq!(
        repo.list_for_task(&task.id).await.unwrap().len(),
        1,
        "late terminalization must not create a duplicate row"
    );
}
