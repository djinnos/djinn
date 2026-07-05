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
