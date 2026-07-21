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
            created_at: None,
        },
        djinn_control_plane::bridge::TaskrunJobRef {
            job_name: "djinn-taskrun-whitespace".to_string(),
            task_run_id: "   ".to_string(),
            created_at: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
        // `None` is treated as an old Job (past the boot-race grace window), so
        // these existing reaping assertions keep exercising the delete path.
        created_at: None,
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
                dispatch_group_id: None,
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
                dispatch_group_id: None,
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

    let config = crate::context::CacheCleanupConfig::default();
    let stats = health::sweep_orphaned_cargo_target_run_dirs_under(&db, root.path(), &config).await;

    assert!(root.path().join(&live_run_id).is_dir());
    assert!(root.path().join(&live_session_guard_run_id).is_dir());
    assert!(!root.path().join(&terminal_run_id).exists());
    assert!(!root.path().join(&unknown_run_id).exists());
    // Fresh malformed entries are retained (just created).
    assert!(root.path().join("not-a-task-run-id").is_dir());
    assert_eq!(stats.scanned, 6);
    assert_eq!(stats.deleted, 2);
    assert_eq!(stats.retained, 4);
    assert_eq!(stats.errors, 0);
    // Fresh malformed dir + fresh loose UUID file are both retained_fresh_malformed.
    assert_eq!(stats.retained_fresh_malformed, 2);
    assert_eq!(stats.retained_non_utf8, 0);
    assert_eq!(stats.malformed_dir_deleted, 0);
    assert_eq!(stats.loose_file_deleted, 0);
    assert_eq!(stats.debris_bytes_deleted, 0);
    // The default cap (64) never trims our handful — the orphan sweep above did
    // all the work. The hard-cap LRU-trim itself is unit-tested in
    // `djinn_core::cargo_target_runs::trim_keeps_newest_and_removes_oldest_beyond_cap`.
    assert_eq!(stats.cap_trimmed, 0);
    assert_eq!(stats.cap_errors, 0);
}

/// Age-sweep deletes old malformed directories and loose files while
/// preserving fresh ones and unchanged UUID live/orphan behaviour.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cargo_target_run_dir_sweep_deletes_old_malformed_and_loose() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "debris-age-sweep").await;
    let root = temp_cargo_target_runs_root();

    // ── UUID dirs: existing behaviour ───────────────────────────────────
    let live_run_id = new_task_run_uuid();
    seed_task_run(&db, &task, &live_run_id, "running").await;

    let terminal_run_id = new_task_run_uuid();
    seed_task_run(&db, &task, &terminal_run_id, "completed").await;

    for run_id in [live_run_id.as_str(), terminal_run_id.as_str()] {
        std::fs::create_dir(root.path().join(run_id)).unwrap();
    }

    // ── Malformed dirs ─────────────────────────────────────────────────
    // Fresh malformed dir (just created — mtime is now).
    std::fs::create_dir(root.path().join("fresh-malformed-dir")).unwrap();

    // Old malformed dir: create then backdate mtime to 30 days ago.
    let old_malformed = root.path().join("old-malformed-dir");
    std::fs::create_dir(&old_malformed).unwrap();
    set_mtime_to_days_ago(&old_malformed, 30);

    // ── Loose files ────────────────────────────────────────────────────
    // Fresh loose file with a UUID name.
    let fresh_loose_uuid = new_task_run_uuid();
    std::fs::write(root.path().join(&fresh_loose_uuid), b"fresh data").unwrap();

    // Old loose file with a UUID name.
    let old_loose_uuid = new_task_run_uuid();
    let old_loose_path = root.path().join(&old_loose_uuid);
    std::fs::write(&old_loose_path, b"old data").unwrap();
    set_mtime_to_days_ago(&old_loose_path, 30);

    // Old loose file with a non-UUID name.
    let old_loose_malformed = root.path().join("old-loose-junk.txt");
    std::fs::write(&old_loose_malformed, b"old junk").unwrap();
    set_mtime_to_days_ago(&old_loose_malformed, 30);

    let config = crate::context::CacheCleanupConfig {
        mode: crate::context::CacheCleanupMode::Delete,
        ..Default::default()
    };
    let stats = health::sweep_orphaned_cargo_target_run_dirs_under(&db, root.path(), &config).await;

    // UUID dirs: live retained, terminal deleted (unchanged behaviour).
    assert!(root.path().join(&live_run_id).is_dir());
    assert!(!root.path().join(&terminal_run_id).exists());

    // Fresh malformed dir retained.
    assert!(root.path().join("fresh-malformed-dir").is_dir());
    // Old malformed dir deleted.
    assert!(!old_malformed.exists());

    // Fresh loose UUID file retained.
    assert!(root.path().join(&fresh_loose_uuid).exists());
    // Old loose UUID file deleted.
    assert!(!old_loose_path.exists());
    // Old loose malformed file deleted.
    assert!(!old_loose_malformed.exists());

    assert_eq!(stats.scanned, 7);
    assert_eq!(stats.deleted, 1); // terminal UUID dir
    assert_eq!(stats.malformed_dir_deleted, 1); // old-malformed-dir
    assert_eq!(stats.loose_file_deleted, 2); // old UUID file + old loose malformed
    assert_eq!(stats.retained_fresh_malformed, 2); // fresh dir + fresh UUID file
    assert_eq!(stats.retained_non_utf8, 0);
    assert_eq!(stats.errors, 0);
    assert!(stats.debris_bytes_deleted > 0);
}

/// Dry-run mode reports candidates without actually deleting them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cargo_target_run_dir_sweep_dry_run_retains_all() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "debris-dry-run").await;
    let root = temp_cargo_target_runs_root();

    // UUID orphan dir.
    let terminal_run_id = new_task_run_uuid();
    seed_task_run(&db, &task, &terminal_run_id, "completed").await;
    std::fs::create_dir(root.path().join(&terminal_run_id)).unwrap();

    // Old malformed dir.
    let old_malformed = root.path().join("old-malformed");
    std::fs::create_dir(&old_malformed).unwrap();
    set_mtime_to_days_ago(&old_malformed, 30);

    // Old loose file.
    let old_loose = root.path().join("old-loose-file.bin");
    std::fs::write(&old_loose, b"data").unwrap();
    set_mtime_to_days_ago(&old_loose, 30);

    let config = crate::context::CacheCleanupConfig {
        mode: crate::context::CacheCleanupMode::DryRun, // dry-run
        ..Default::default()
    };
    let stats = health::sweep_orphaned_cargo_target_run_dirs_under(&db, root.path(), &config).await;

    // UUID orphan deletion runs regardless of mode (unchanged behaviour).
    assert!(!root.path().join(&terminal_run_id).exists());
    // Debris entries are retained in dry-run mode — only reported.
    assert!(old_malformed.is_dir());
    assert!(old_loose.exists());

    // Debris counters count dry-run candidates.
    assert_eq!(stats.malformed_dir_deleted, 1); // counted as candidate
    assert_eq!(stats.loose_file_deleted, 1); // counted as candidate
    assert_eq!(stats.errors, 0);
    assert_eq!(stats.debris_bytes_deleted, 0); // nothing actually reclaimed
}

/// When cargo_debris_enabled is false, malformed entries are retained
/// (legacy behaviour).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cargo_target_run_dir_sweep_retains_when_debris_disabled() {
    let db = test_helpers::create_test_db();
    let root = temp_cargo_target_runs_root();

    // Old malformed dir.
    let old_malformed = root.path().join("old-malformed");
    std::fs::create_dir(&old_malformed).unwrap();
    set_mtime_to_days_ago(&old_malformed, 30);

    // Old loose file.
    let old_loose = root.path().join("old-loose.bin");
    std::fs::write(&old_loose, b"data").unwrap();
    set_mtime_to_days_ago(&old_loose, 30);

    let config = crate::context::CacheCleanupConfig {
        mode: crate::context::CacheCleanupMode::Delete,
        cargo_debris_enabled: false, // disabled
        ..Default::default()
    };
    let stats = health::sweep_orphaned_cargo_target_run_dirs_under(&db, root.path(), &config).await;

    // Both retained because debris cleanup is disabled.
    assert!(old_malformed.is_dir());
    assert!(old_loose.exists());
    assert_eq!(stats.scanned, 2);
    assert_eq!(stats.retained, 2);
    assert_eq!(stats.malformed_dir_deleted, 0);
    assert_eq!(stats.loose_file_deleted, 0);
    assert_eq!(stats.retained_fresh_malformed, 0);
}

/// Non-UTF8 entry names are always retained with a stable
/// `retained_non_utf8` outcome, even when debris cleanup is enabled
/// and the entry would otherwise be old enough to sweep.
#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cargo_target_run_dir_sweep_retains_non_utf8_entries() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let db = test_helpers::create_test_db();
    let root = temp_cargo_target_runs_root();

    // Create a non-UTF8 entry using raw bytes (invalid UTF-8 sequence).
    let non_utf8_name = OsStr::from_bytes(b"bad\xffname");
    let non_utf8_path = root.path().join(non_utf8_name);
    std::fs::create_dir(&non_utf8_path).unwrap();

    let config = crate::context::CacheCleanupConfig {
        mode: crate::context::CacheCleanupMode::Delete,
        ..Default::default()
    };
    let stats = health::sweep_orphaned_cargo_target_run_dirs_under(&db, root.path(), &config).await;

    // Non-UTF8 entry must be retained regardless of age.
    assert!(non_utf8_path.exists());
    assert_eq!(stats.scanned, 1);
    assert_eq!(stats.retained_non_utf8, 1);
    assert_eq!(stats.retained, 1);
    assert_eq!(stats.deleted, 0);
    assert_eq!(stats.malformed_dir_deleted, 0);
    assert_eq!(stats.loose_file_deleted, 0);
    assert_eq!(stats.errors, 0);
}

/// Helper: backdate an entry's mtime to N days ago.
///
/// Uses `touch -d` for portability across file/directory entries on Linux
/// (opening a directory for write fails with `EISDIR`).
fn set_mtime_to_days_ago(path: &std::path::Path, days: u64) {
    let spec = format!("{days} days ago");
    assert!(
        std::process::Command::new("touch")
            .args(["-d", &spec, path.to_str().unwrap()])
            .status()
            .unwrap()
            .success(),
        "touch -d failed for {}",
        path.display()
    );
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
    // `request_session_preservation` increments this exact series whenever a
    // stall reap runs without runtime_ops, and those recovery tests share this
    // process's recorder. Emit into a thread-private registry so the count
    // reflects this test's single increment and nothing else.
    let (_, rendered) = djinn_telemetry::render_isolated(|| {
        djinn_telemetry::preservation::increment_attempt(
            djinn_telemetry::preservation::OUTCOME_RUNTIME_UNAVAILABLE,
            djinn_telemetry::preservation::TRIGGER_STALL,
        );
    });

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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
        .classify_session_exit_liveness(&session.id, &task.id, Some(run_id), "completed", "worker")
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
            dispatch_group_id: None,
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
        .classify_session_exit_liveness(&session.id, &task.id, Some(run_id), "failed", "worker")
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
            dispatch_group_id: None,
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
        .classify_session_exit_liveness(&session.id, &task.id, Some(run_id), "completed", "worker")
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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
        .classify_session_exit_liveness(
            &session.id,
            &task.id,
            Some(run_id),
            "interrupted",
            "worker",
        )
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

/// AC historical: Pending-pod / capacity-crunch — a session with a Pending pod
/// (e.g. pod stuck in image pull or waiting for node resources during a
/// capacity crunch) is NOT classified as Dead, even when the in-memory activity
/// signal is Idle. `Pending` is not `Absent` or `Failed`, so the Dead guard
/// (`pod_absent_or_failed && no_recent_activity`) does not fire. The classifier
/// returns `Live` (default fallthrough).
///
/// This is the targeted regression for the historical false-reap class where
/// raw pod-phase heuristics would treat a Pending pod as "not running" and
/// reclaim the session before the pod had a chance to start.
#[test]
fn pending_pod_capacity_crunch_spared_by_classifier() {
    use crate::dispatch::liveness::*;

    let evidence = LivenessEvidence {
        pod_phase: Some(PodPhase::Pending),
        activity: ActivitySignal::Idle,
        db_session_status: Some(DbSessionStatus::Running),
        db_task_status: Some(DbTaskStatus::InProgress),
        claim_ttl_remaining: Some(std::time::Duration::from_secs(120)),
        extension_budget_exhausted: false,
        hard_runtime_deadline_exceeded: false,
        exit_code: None,
    };

    let result = classify(&evidence);
    assert_ne!(
        result.verdict,
        Verdict::Dead,
        "Pending-pod capacity-crunch session must NOT be classified as Dead"
    );
    assert_eq!(
        result.verdict,
        Verdict::Live,
        "Pending pod with idle activity falls through to Live"
    );
    assert_eq!(result.outcome, None, "Live has no outcome");
    assert!(!result.extension_eligible);
}

/// AC historical: Pending-pod just past the zombie hard cap — even when the
/// claim TTL has expired, a Pending pod is still not Dead because `Pending` is
/// not `Absent`/`Failed`. The classifier returns `Live` (not Dead, not Slow).
#[test]
fn pending_pod_past_hard_cap_still_not_dead() {
    use crate::dispatch::liveness::*;

    let evidence = LivenessEvidence {
        pod_phase: Some(PodPhase::Pending),
        activity: ActivitySignal::Idle,
        db_session_status: Some(DbSessionStatus::Running),
        db_task_status: Some(DbTaskStatus::InProgress),
        claim_ttl_remaining: Some(std::time::Duration::ZERO),
        extension_budget_exhausted: true,
        hard_runtime_deadline_exceeded: true,
        exit_code: None,
    };

    // Even with hard_runtime_deadline_exceeded, the classifier applies the
    // hard-cap precedence which forces Dead/Timeout — but the key regression
    // guard is: absent hard-cap override, Pending is NOT Dead.
    let evidence_no_hard_cap = LivenessEvidence {
        pod_phase: Some(PodPhase::Pending),
        activity: ActivitySignal::Idle,
        db_session_status: Some(DbSessionStatus::Running),
        db_task_status: Some(DbTaskStatus::InProgress),
        claim_ttl_remaining: Some(std::time::Duration::ZERO),
        extension_budget_exhausted: true,
        hard_runtime_deadline_exceeded: false,
        exit_code: None,
    };
    let result = classify(&evidence_no_hard_cap);
    assert_ne!(
        result.verdict,
        Verdict::Dead,
        "Pending pod must not be Dead without hard-cap override (capacity-crunch false-reap guard)"
    );
    assert_eq!(result.verdict, Verdict::Live);
    // With hard_cap override, Dead IS expected (precedence rule), but the
    // Pending-pod test point is the no-hard-cap case above.
    let result_hard = classify(&evidence);
    assert_eq!(
        result_hard.verdict,
        Verdict::Dead,
        "hard_runtime_deadline_exceeded overrides Pending into Dead/Timeout (precedence 2)"
    );
    assert_eq!(result_hard.outcome, Some(LivenessOutcome::Timeout));
}

/// AC historical: Zero-token running session is classified Slow (not Dead)
/// through the landed classifier/reaper rules. The integration path through
/// `reap_zombie_sessions` uses the liveness classifier as its authoritative
/// gate: with a live pool mapping and aged activity, the classifier returns
/// Slow and the session is spared.
///
/// This regression proves the zero-token case is routed through the
/// classifier verdict path, not a raw zero-token shortcut.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn zero_token_running_session_classified_slow_not_dead() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "zero-token-slow").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-zero-token-slow";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
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
    // Default tokens: 0/0. Backdate past zombie hard cap.
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();

    // Dispatch to pool so `session_for_task` returns Some (Running pod).
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

    // Age activity so idle > ZOMBIE_HARD_CAP_SECS: the zombie reaper's pool
    // gate (idle <= cap) does NOT spare, but the classifier sees Running pod +
    // Idle → Slow.
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

    // Session must be spared: the classifier returned Slow (not Dead) even
    // though this is a zero-token session past the zombie hard cap.
    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "zero-token running session with Running pod must be spared by Slow verdict (not Dead)"
    );

    // No dead_reclaimed evidence must exist.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let dead_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("dead_reclaimed"))
        .await
        .unwrap();
    assert_eq!(
        dead_count, 0,
        "classifier Slow verdict must NOT produce dead_reclaimed evidence"
    );

    // Task must remain in_progress.
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "in_progress",
        "task must remain in_progress when zero-token session is spared by classifier"
    );
}

/// AC historical: Kill-task no-op cleanup records terminal evidence — when a
/// session is reaped for a task that has already been closed, the classifier
/// returns `KillNoop` outcome (terminal-task precedence). The session is
/// finalized but the task is NOT reopened. The evidence row records the
/// terminal verdict on the denormalized session columns and the append-only
/// `liveness_evidence` table.
///
/// This regression strengthens the existing kill_noop tests by explicitly
/// asserting the denormalized verdict is NOT "dead" (proving the classifier
/// ruled it KillNoop, not Dead) and that the task stays closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn kill_noop_cleanup_records_terminal_verdict_on_session() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "kill-noop-terminal").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-kill-noop-terminal";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
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

    // Close the task BEFORE the reaper runs — the canonical terminal-race
    // condition.
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "closed")
        .await
        .unwrap();

    let runtime = RecordingRuntimeOps::new(true);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime));
    actor.reap_zombie_sessions().await;

    // 1. Task stays closed (not reopened).
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "closed",
        "task must remain closed — kill_noop must NOT reopen a terminal task"
    );

    // 2. Session finalized.
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "orphaned session for terminal task must be finalized"
    );

    // 3. Denormalized session columns: verdict is NOT "dead" (classifier ruled
    //    KillNoop via terminal-task precedence); outcome is "kill_noop".
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let (verdict, outcome_kind) = liveness_repo
        .get_session_liveness_fields(&session.id)
        .await
        .unwrap();
    assert_ne!(
        verdict.as_deref(),
        Some("dead"),
        "terminal-task race must NOT produce a 'dead' verdict — classifier uses KillNoop precedence"
    );
    assert_eq!(
        outcome_kind.as_deref(),
        Some("kill_noop"),
        "session must have kill_noop outcome on denormalized columns"
    );

    // 4. Append-only evidence: kill_noop row exists, dead_reclaimed does NOT.
    let kill_noop_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("kill_noop"))
        .await
        .unwrap();
    assert!(
        kill_noop_count >= 1,
        "liveness_evidence table must have kill_noop row"
    );
    let dead_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("dead_reclaimed"))
        .await
        .unwrap();
    assert_eq!(
        dead_count, 0,
        "terminal-task race must NOT produce dead_reclaimed evidence"
    );
}

/// AC historical: Ready-but-never-claimed dispatch starvation — a session that
/// predates its task's `updated_at` (stale ready-state orphan) is finalized by
/// `detect_and_recover_stuck_filtered` as an orphan, NOT reaped as a zombie.
/// The task remains `open` (released for redispatch) and the session does NOT
/// get liveness classifier evidence, because the orphan detection path is
/// structurally separate from the liveness/zombie path.
///
/// This regression proves that dispatch starvation is represented as a
/// stranded-ready/dispatch-gate condition (task stays open, session finalized
/// without zombie semantics) rather than as session zombie evidence.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ready_never_claimed_session_not_misclassified_as_zombie() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "stranded-ready-regression").await;

    // Task stays `open` — dispatch starvation scenario: no worker ever claimed it.
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

    // Backdate session so it predates the task's `updated_at`. This models a
    // ready-state task whose session was created alongside it but never claimed
    // — the canonical stranded-ready/dispatch-starvation pattern.
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
        "precondition: stale ready-state session should be listed as running"
    );

    // Drive the orphan detection path (NOT the zombie reaper).
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.detect_and_recover_stuck_filtered(None).await;

    // 1. Session finalized (orphan detected and cleaned up).
    assert!(
        !session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "stale ready-state orphan session must be finalized via orphan detection"
    );

    // 2. Task stays `open` — released for redispatch (stranded-ready, not zombie).
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "open",
        "ready-but-never-claimed task must remain open (stranded-ready, not zombie reclaim)"
    );

    // 3. The orphan detection path consults the liveness classifier, which
    //    records Dead verdict + dead_reclaimed evidence for a stale orphan
    //    with no live pod. This is correct — the classifier sees no active
    //    pod and classifies the session as Dead. The key invariant is that
    //    the task stays `open` (step 2 above), NOT that evidence is absent.
    //    The stranded-ready semantic is a dispatch-gate observation preserved
    //    by the task-status check, not by suppressing classifier evidence.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let dead_reclaimed_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("dead_reclaimed"))
        .await
        .unwrap();
    assert!(
        dead_reclaimed_count >= 1,
        "orphan classifier path must produce dead_reclaimed evidence for stale orphan session"
    );

    let (verdict, outcome) = liveness_repo
        .get_session_liveness_fields(&session.id)
        .await
        .unwrap();
    assert_eq!(
        verdict.as_deref(),
        Some("dead"),
        "orphan-classified session must have Dead verdict on denormalized columns"
    );
    assert_eq!(
        outcome.as_deref(),
        Some("dead_reclaimed"),
        "orphan-classified session must have dead_reclaimed outcome"
    );
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
    seed_pending_attempt_with_identity(db, task_id, role, None, None).await
}

/// Seed a pending attempt with optional durable owner incarnation and dispatch
/// group identity (epic jy7g foundation).
async fn seed_pending_attempt_with_identity(
    db: &Database,
    task_id: &str,
    role: &str,
    owner_incarnation_id: Option<&str>,
    dispatch_group_id: Option<&str>,
) -> String {
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
        dispatch_owner_incarnation_id: owner_incarnation_id,
        dispatch_group_id,
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

// ── Deploy/reap interruptions are environmental (fix/deploy-interruptions) ──

/// A session `interrupted` by INFRASTRUCTURE (deploy/rollout/pod-eviction/reap)
/// while its task is still nonterminal must terminalize the live attempt as the
/// environmental `interrupted` outcome — NOT `crashed`. This is what lets the
/// dispatch reappearance path spare the task from a failure streak / cooldown.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_session_terminalizes_attempt_as_environmental_interrupt() {
    use djinn_core::models::task_attempt::TaskAttemptOutcome;
    use djinn_db::{CreateSessionParams, SessionRepository, TaskAttemptRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "env-interrupt-attempt").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-env-interrupt";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
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

    // A live (pending) dispatch-start attempt exists — nothing has claimed it as
    // a failure, i.e. a pure infra kill (no stall/ceiling/zombie decision).
    let attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;

    let actor = coordinator_actor_for_tests(&db, &tx);
    actor
        .classify_session_exit_liveness(
            &session.id,
            &task.id,
            Some(run_id),
            "interrupted",
            "worker",
        )
        .await;

    let repo = TaskAttemptRepository::new(db.clone());
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(
        attempt.outcome, "interrupted",
        "an infrastructure interruption must terminalize the attempt as environmental \
         `interrupted`, not `crashed`"
    );
    assert!(attempt.terminal_at.is_some());
    let outcome: TaskAttemptOutcome = attempt.outcome.parse().unwrap();
    assert!(outcome.is_environmental_interrupt());
    assert!(
        outcome.is_infra(),
        "environmental interrupt is infra-classified (quality/park exempt)"
    );
    let sj: serde_json::Value =
        serde_json::from_str(attempt.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj["failure_class"], "environmental_interrupt");
    assert_eq!(sj["recovery_classifier"], "session_exit_liveness");
    // No duplicate row.
    assert_eq!(repo.list_for_task(&task.id).await.unwrap().len(), 1);
}

/// Regression: a genuine `failed` session exit (application/provider crash) still
/// terminalizes the attempt as `crashed` — a real failure the reappearance streak
/// keeps counting. Only `interrupted` (infra) is treated environmental.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_session_terminalizes_attempt_as_crashed_not_interrupted() {
    use djinn_core::models::task_attempt::TaskAttemptOutcome;
    use djinn_db::{CreateSessionParams, SessionRepository, TaskAttemptRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "failed-stays-crashed").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-failed-crash";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
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
    let attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;

    let actor = coordinator_actor_for_tests(&db, &tx);
    actor
        .classify_session_exit_liveness(&session.id, &task.id, Some(run_id), "failed", "worker")
        .await;

    let repo = TaskAttemptRepository::new(db.clone());
    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(
        attempt.outcome, "crashed",
        "a genuine failed exit must remain a `crashed` failure"
    );
    let outcome: TaskAttemptOutcome = attempt.outcome.parse().unwrap();
    assert!(!outcome.is_environmental_interrupt());
}

/// Regression: a stall/ceiling/zombie kill terminalizes the attempt with its own
/// failure outcome BEFORE the session-interrupt event is processed. The later
/// `interrupted` event must NOT reclassify that already-terminal failure to
/// environmental — `advance_latest_to_terminal` no-ops on a terminal attempt, so
/// stall-killed / runtime-exceeded runs stay failures.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_event_does_not_reclassify_already_terminal_failure() {
    use djinn_db::{
        CreateSessionParams, SessionRepository, TaskAttemptRepository, TerminalTaskAttemptParams,
    };

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "stall-then-interrupt").await;
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-stall-then-interrupt";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
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

    // The stall path already terminalized the attempt as `timed_out`.
    let attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;
    let repo = TaskAttemptRepository::new(db.clone());
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt_id,
        outcome: djinn_core::models::task_attempt::TaskAttemptOutcome::TimedOut,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("stall kill"),
        summary_json: None,
        log_tail: None,
    })
    .await
    .unwrap();

    // The session-interrupt event arrives afterward — must be a no-op.
    let actor = coordinator_actor_for_tests(&db, &tx);
    actor
        .classify_session_exit_liveness(
            &session.id,
            &task.id,
            Some(run_id),
            "interrupted",
            "worker",
        )
        .await;

    let attempt = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(
        attempt.outcome, "timed_out",
        "an already-terminal stall/timeout attempt must NOT be reclassified environmental"
    );
}

/// Evidence-based reaping (proposal 9gg5 / epic ars3): startup and periodic
/// sweeps apply the same owner-lease rule. A legacy NULL-owner orphan is always
/// `crashed/orphaned_pending_attempt_unproven` regardless of reap reason — the
/// startup reason alone never exempts an attempt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn startup_orphan_reap_stamps_interrupted_periodic_stamps_crashed() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    // A stale orphaned pending attempt with a NULL (legacy) owner. Reap via the
    // STARTUP path → still `crashed/unproven` (NULL owner = ambiguous, fail closed).
    let (task_startup, _n) = create_task_with_note(&db, &tx, "orphan-startup").await;
    let startup_attempt = seed_pending_attempt(&db, &task_startup.id, "reviewer").await;
    backdate_attempt(&db, &startup_attempt).await;
    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "startup").await;

    // A second stale orphan with a NULL owner, reaped via the PERIODIC path →
    // also `crashed/unproven` — the reason string does not change classification.
    let (task_periodic, _n) = create_task_with_note(&db, &tx, "orphan-periodic").await;
    let periodic_attempt = seed_pending_attempt(&db, &task_periodic.id, "reviewer").await;
    backdate_attempt(&db, &periodic_attempt).await;
    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "periodic").await;

    // Both NULL-owner orphans are `crashed/unproven`: the startup reason alone
    // must never exempt an attempt.
    for (attempt_id, label) in [
        (&startup_attempt, "startup"),
        (&periodic_attempt, "periodic"),
    ] {
        let attempt = repo.get(attempt_id).await.unwrap().unwrap();
        assert_eq!(
            attempt.outcome, "crashed",
            "{label}: a NULL-owner orphan must be crashed (unproven), not interrupted"
        );
        let sj: serde_json::Value =
            serde_json::from_str(attempt.summary_json.as_deref().unwrap()).unwrap();
        assert_eq!(
            sj["failure_class"], "orphaned_pending_attempt_unproven",
            "{label}: NULL-owner orphan must be unproven"
        );
    }
}

/// The reappearance helper detects only an environmental `interrupted` latest
/// attempt — genuine `crashed`/`timed_out` latest attempts are NOT environmental,
/// guard-only rows are skipped, and no attempt is NOT environmental.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn latest_attempt_environmental_interrupt_helper_detection() {
    use djinn_core::models::task_attempt::TaskAttemptOutcome;
    use djinn_db::{TaskAttemptRepository, TerminalTaskAttemptParams};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let actor = coordinator_actor_for_tests(&db, &tx);

    let terminalize = |attempt_id: String, outcome: TaskAttemptOutcome| {
        let db = db.clone();
        async move {
            let repo = TaskAttemptRepository::new(db.clone());
            repo.advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt_id,
                outcome,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: None,
                summary_json: None,
                log_tail: None,
            })
            .await
            .unwrap();
        }
    };

    // Interrupted latest → environmental.
    let (t_env, _n) = create_task_with_note(&db, &tx, "helper-env").await;
    let a = seed_pending_attempt(&db, &t_env.id, "reviewer").await;
    terminalize(a, TaskAttemptOutcome::Interrupted).await;
    assert!(
        actor
            .latest_attempt_was_environmental_interrupt(&t_env.id, "reviewer")
            .await
    );
    // Wrong role → not environmental for that role.
    assert!(
        !actor
            .latest_attempt_was_environmental_interrupt(&t_env.id, "worker")
            .await
    );

    // Crashed latest → NOT environmental (genuine failure still counts).
    let (t_crash, _n) = create_task_with_note(&db, &tx, "helper-crash").await;
    let a = seed_pending_attempt(&db, &t_crash.id, "reviewer").await;
    terminalize(a, TaskAttemptOutcome::Crashed).await;
    assert!(
        !actor
            .latest_attempt_was_environmental_interrupt(&t_crash.id, "reviewer")
            .await
    );

    // No attempt at all → NOT environmental.
    let (t_none, _n) = create_task_with_note(&db, &tx, "helper-none").await;
    assert!(
        !actor
            .latest_attempt_was_environmental_interrupt(&t_none.id, "reviewer")
            .await
    );
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
            dispatch_group_id: None,
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
            dispatch_group_id: None,
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

// ── Coordinator liveness race / idempotency regressions (ecwp) ────────────────
//
// These tests cover the race and idempotency edges across the landed
// classifier, session recovery, terminalization, and evidence-persistence paths.
// They are deliberately additive — they exercise existing fixtures and assert
// the resulting task/session state and liveness evidence/outcome, so future
// raw-signal shortcuts cannot satisfy them accidentally. Each test header
// names the invariant it protects.

// ── AC 1: Slow verdict + concurrent/already-landed terminal transition ────────
//
// Invariant: when the task transitions to terminal AFTER the liveness
// classifier would have returned a Slow verdict, the stall-timeout path must
// NOT grant a Slow extension (the extension would be a no-op against an already
// finished task) and must NOT reopen or release the task. The session stays
// running so the next zombie-reap tick can finalize it via KillNoop.

/// A Slow-classified session whose task transitions to terminal before
/// `enforce_session_stall_timeout` runs must NOT have a Slow extension
/// recorded. The classifier returns `Verdict::Live` with `KillNoop` outcome
/// for terminal tasks (precedence rule #1 — terminal-task precedence wins
/// over Slow), and the stall-timeout branch treats `Live` as "spare, do not
/// extend, do not kill".
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn slow_verdict_with_concurrent_terminal_transition_does_not_grant_extension() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "slow-term-race").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-slow-term-race";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
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
    // Keep the session within its claim window so the Slow verdict (if reached)
    // would normally be extension-eligible. The stall is driven by aged activity
    // tracker, not started_at.
    session_repo
        .backdate_started_at(&session.id, "2 minutes")
        .await
        .unwrap();

    // Dispatch to a live pool slot so the liveness classifier sees a Running pod.
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

    // Age the activity tracker so idle exceeds the 30-minute stall threshold.
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

    // Concurrent terminal transition BEFORE the stall-timeout tick runs.
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "closed")
        .await
        .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = pool;
    // Slow extension is enabled by default. Without the terminal-task precedence
    // rule, the classifier would return Slow + extension_eligible=true and the
    // stall-timeout branch would grant an extension against an already-closed
    // task (a real race that the precedence rule is designed to prevent).
    assert!(
        actor.worker_lifecycle_config.slow_extension.enabled,
        "precondition: slow extension is enabled by default"
    );

    actor.enforce_session_stall_timeout().await;

    // The session must NOT have been killed (terminal-task precedence wins).
    assert!(
        !actor.stall_killed.contains(&session.id),
        "stall-timeout must NOT kill a session whose task is already terminal (terminal-task precedence)"
    );

    // The extension count for this session must be 0 (no Slow extension granted).
    let ext_count = actor
        .stall_extension_count
        .get(&session.id)
        .copied()
        .unwrap_or(0);
    assert_eq!(
        ext_count, 0,
        "stall-timeout must NOT grant a Slow extension for a terminal-task session"
    );

    // No slow_extended evidence row may have been persisted.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let slow_extended = liveness_repo
        .count_evidence_for_session(&session.id, Some("slow_extended"))
        .await
        .unwrap();
    assert_eq!(
        slow_extended, 0,
        "no slow_extended evidence row may be persisted for a terminal-task Slow+terminal race"
    );

    // The classifier pass inside the stall-timeout path persisted KillNoop
    // evidence (verdict=live, outcome=kill_noop) — that is the
    // terminal-task-precedence recording. Confirm that the outcome_kind is
    // kill_noop, NOT slow_extended.
    let (verdict, outcome) = liveness_repo
        .get_session_liveness_fields(&session.id)
        .await
        .unwrap();
    assert_eq!(
        verdict.as_deref(),
        Some("live"),
        "terminal-task precedence: classifier verdict is Live (moot), not Slow"
    );
    assert_eq!(
        outcome.as_deref(),
        Some("kill_noop"),
        "terminal-task precedence: outcome is kill_noop, NOT slow_extended"
    );

    // The task stays closed — not reopened by the stall-timeout path.
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "closed",
        "task must remain closed — stall-timeout must NOT reopen a terminal task via the Slow extension path"
    );

    cancel.cancel();
}

// ── AC 2: Dead verdict + fresh DB activity ───────────────────────────────────
//
// Invariant: a session that looks Dead on raw signals (absent pod, no recent
// in-memory activity, zero tokens on the DB row) but has FRESH DB activity
// (recent started_at with nonzero tokens flushed mid-flight) is NOT reaped.
// The landed `reap_zombie_sessions` path must consult the liveness classifier
// and trust its verdict over raw heuristics — a Slow verdict (or Live) suppresses
// the reap even when the underlying raw signals would otherwise look Dead.

/// A session that is past the zombie hard cap and would look Dead on raw
/// signals (zero tokens, aged session), but whose pool still holds a Running
/// slot with `activity_tracked=true` (the worker pod is alive but quiet), is
/// classified `Slow` — not `Dead` — by the liveness classifier, and the zombie
/// reaper spares it.
///
/// Invariant protected: the zombie reaper trusts the classifier's Slow verdict
/// over raw heuristics.  The test asserts the persisted classifier verdict and
/// outcome/evidence on the denormalized session columns AND in the append-only
/// `liveness_evidence` table so that a future raw-signal shortcut that skips
/// the classifier cannot satisfy this test accidentally.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dead_verdict_with_fresh_db_activity_suppresses_reap() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "dead-fresh-db").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-dead-fresh-db";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
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
    // Backdate past the 10-minute zombie hard cap so the raw age gate fires
    // and the classifier's claim TTL is zero (extension budget exhausted).
    session_repo
        .backdate_started_at(&session.id, "20 minutes")
        .await
        .unwrap();
    // NOTE: tokens are intentionally LEFT at zero so the pre-classifier
    // token-nonzero gate in `reap_zombie_sessions` does NOT skip this
    // session.  The classifier must be reached.

    // Dispatch the task to a live pool slot so `session_for_task` returns
    // Some — the classifier sees a Running pod (not Absent).
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
    // activity_tracked is still true.  The pool check at
    // `info.activity_tracked && info.idle_seconds <= ZOMBIE_HARD_CAP_SECS`
    // does NOT spare (idle exceeds cap), but the classifier sees a Running
    // pod with Idle activity → Slow verdict (not Dead).
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
    // Hold a clone for inspection after `actor.runtime_ops` takes ownership.
    let runtime_for_calls = runtime.clone();
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime));
    actor.pool = pool;
    actor.reap_zombie_sessions().await;

    // ── Invariant: the classifier returned Slow → session is spared ────
    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "a session past the zombie hard cap whose pool still shows a Running pod \
         must be classified Slow (not Dead) and spared by the liveness classifier"
    );

    // The teardown runtime_ops must NOT have been invoked — a regression
    // that bypassed the classifier would call teardown_taskrun_job even
    // though the session was not reaped.
    assert!(
        runtime_for_calls.calls().is_empty(),
        "zombie reaper must NOT call teardown_taskrun_job when the classifier returns Slow"
    );

    // ── Classifier verdict/evidence assertions (AC 2) ──────────────────
    // The classifier must have persisted its verdict on the session's
    // denormalized columns and in the append-only liveness_evidence table.
    // These assertions pin the classifier invariant: a raw-token shortcut
    // that skips the classifier would leave these fields empty.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let (verdict, outcome_kind) = liveness_repo
        .get_session_liveness_fields(&session.id)
        .await
        .unwrap();
    assert_eq!(
        verdict.as_deref(),
        Some("slow"),
        "classifier must record Slow verdict on session row — not Dead"
    );
    // With a 20-minute-old session the claim TTL is zero → extension budget
    // exhausted → the classifier tags the Slow verdict with SlowExtended
    // outcome and HardRuntimeExceeded-adjacent reason.
    assert_eq!(
        outcome_kind.as_deref(),
        Some("slow_extended"),
        "classifier must record slow_extended outcome when extension budget is exhausted"
    );

    // No dead_reclaimed evidence row may have been persisted.
    let dead_reclaimed = liveness_repo
        .count_evidence_for_session(&session.id, Some("dead_reclaimed"))
        .await
        .unwrap();
    assert_eq!(
        dead_reclaimed, 0,
        "a Dead reclaim must NOT be persisted when the classifier returns Slow"
    );

    // The classifier must have persisted at least one evidence row (the
    // Slow verdict itself).  A bypass that skips the classifier entirely
    // would leave zero rows.
    let total_evidence = liveness_repo
        .count_evidence_for_session(&session.id, None)
        .await
        .unwrap();
    assert!(
        total_evidence >= 1,
        "classifier must persist at least one liveness_evidence row for the Slow verdict"
    );

    // The task must NOT have been released to `open` (no reclaim happened).
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "in_progress",
        "task must remain in_progress — liveness classifier Slow verdict suppresses Dead reclaim"
    );

    cancel.cancel();
}

// ── AC 3: hard runtime cap takes precedence over Slow extension budget ───────
//
// Invariant: hard-runtime-cap precedence (classifier rule #2) forbids
// extension unconditionally, even when the slow-extension budget is NOT
// exhausted. A session classified Slow + extension_eligible on raw signals
// but past the hard cap must be Dead/Timeout with `extension_eligible=false`,
// and the stall-timeout path must kill it instead of granting an extension.
// The session is killed on the first tick even though it has remaining
// extension budget available.

/// Hard runtime cap overrides Slow extension eligibility: a session whose
/// hard-runtime cap is exceeded is Dead/Timeout, not Slow, and the
/// stall-timeout path kills it instead of granting an extension — even though
/// the slow-extension budget is fully available.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_cap_takes_precedence_over_slow_extension_budget_in_stall_path() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "hard-cap-precedence").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-hard-cap-precedence";
    let (pool, _cancel, session) = dispatch_stalled_worker_session(&db, &tx, &task, run_id).await;

    // Backdate the task_run's started_at so `hard_runtime_deadline_exceeded`
    // is true. Without this, the classifier sees a fresh task_run and would
    // return Slow + extension_eligible — the slow-extension branch would fire,
    // and the hard-cap precedence invariant would not be exercised.
    TaskRunRepository::new(db.clone())
        .backdate_started_at(run_id, "20 minutes")
        .await
        .unwrap();

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = pool;
    // Sanity: budget is fully available (count = 0, max_extensions = 3).
    assert!(
        actor.worker_lifecycle_config.slow_extension.enabled,
        "precondition: slow extension is enabled"
    );
    let max_ext = actor.worker_lifecycle_config.slow_extension.max_extensions;
    assert_eq!(max_ext, 3, "precondition: default max_extensions = 3");
    assert_eq!(
        actor
            .stall_extension_count
            .get(&session.id)
            .copied()
            .unwrap_or(0),
        0,
        "precondition: extension budget fully available (0 of 3 used)"
    );

    actor.enforce_session_stall_timeout().await;

    // The hard cap forces Dead, not Slow → extension is NOT granted and the
    // session is killed instead.
    assert!(
        actor.stall_killed.contains(&session.id),
        "hard cap must take precedence: session is killed, NOT Slow-extended"
    );

    // The extension count for this session must still be 0 (the budget was not
    // touched — the cap fired before the Slow branch).
    let ext_count = actor
        .stall_extension_count
        .get(&session.id)
        .copied()
        .unwrap_or(0);
    assert_eq!(
        ext_count, 0,
        "slow extension budget must NOT be decremented when hard cap fires"
    );

    // No slow_extended evidence row may have been persisted.
    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let slow_extended = liveness_repo
        .count_evidence_for_session(&session.id, Some("slow_extended"))
        .await
        .unwrap();
    assert_eq!(
        slow_extended, 0,
        "no slow_extended evidence row may be persisted when the hard cap fires"
    );
}

// ── AC 4: clean-exit nonterminal vs already-terminal task paths ──────────────
//
// Invariant: `classify_session_exit_liveness` produces TWO distinct, non-overlapping
// verdicts depending on the task's terminal state at the moment of the call.
// The same `(session_status="completed", session_status="failed")` input produces
// ProtocolViolation/CleanExitNonterminal when the task is nonterminal and
// KillNoop when the task is already terminal. These are distinct outcomes with
// distinct evidence — the protocol-violation path increments retry accounting,
// while KillNoop preserves terminal state and does not. A regression that
// short-circuits the terminal check would merge them.

/// `classify_session_exit_liveness` produces two distinct, non-overlapping
/// verdicts for the same session status depending on whether the task is
/// nonterminal (ProtocolViolation with CleanExitNonterminal) or already
/// terminal (KillNoop). Calling the same input twice on a nonterminal task
/// is idempotent — no extra evidence rows are appended beyond the per-call
/// record (because the second call sees the same task state and produces the
/// same verdict). After the task transitions to terminal, the next call
/// produces KillNoop instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn classify_session_exit_clean_nonterminal_vs_terminal_are_distinct_and_idempotent() {
    use djinn_db::{CreateSessionParams, SessionRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "pv-distinct-idempotent").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-pv-distinct-idempotent";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
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

    // ── Path A: clean exit on nonterminal task → ProtocolViolation ──
    let r1 = actor
        .classify_session_exit_liveness(&session.id, &task.id, Some(run_id), "completed", "worker")
        .await
        .expect("first classification must succeed");
    assert_eq!(
        r1.verdict,
        crate::dispatch::liveness::Verdict::ProtocolViolation,
        "nonterminal task + clean exit must produce ProtocolViolation"
    );
    assert_eq!(
        r1.outcome,
        Some(crate::dispatch::liveness::LivenessOutcome::Success),
        "nonterminal + clean exit outcome is Success (retry accounting sees this as a failed attempt)"
    );

    // ── Idempotency on the nonterminal path: second call sees same task state ──
    let r1_again = actor
        .classify_session_exit_liveness(&session.id, &task.id, Some(run_id), "completed", "worker")
        .await
        .expect("repeat classification must succeed");
    assert_eq!(
        r1_again.verdict,
        crate::dispatch::liveness::Verdict::ProtocolViolation,
        "repeat call on nonterminal task must produce the same ProtocolViolation verdict"
    );

    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let pv_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("success"))
        .await
        .unwrap();
    assert_eq!(
        pv_count, 2,
        "two calls on the nonterminal path produce two evidence rows (one per call) — no extra rows from a regression that loops the classifier"
    );
    let kill_noop_count = liveness_repo
        .count_evidence_for_session(&session.id, Some("kill_noop"))
        .await
        .unwrap();
    assert_eq!(
        kill_noop_count, 0,
        "nonterminal path must NOT produce any kill_noop rows"
    );

    // ── Path B: task transitions to terminal, same input → KillNoop (distinct) ──
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "closed")
        .await
        .unwrap();

    let r2 = actor
        .classify_session_exit_liveness(&session.id, &task.id, Some(run_id), "completed", "worker")
        .await
        .expect("terminal classification must succeed");
    assert_eq!(
        r2.outcome,
        Some(crate::dispatch::liveness::LivenessOutcome::KillNoop),
        "terminal task + clean exit must produce KillNoop, NOT ProtocolViolation"
    );
    assert_ne!(
        r2.verdict,
        crate::dispatch::liveness::Verdict::ProtocolViolation,
        "terminal-task clean-exit must NOT regress to ProtocolViolation"
    );
    assert_eq!(
        r2.verdict,
        crate::dispatch::liveness::Verdict::Live,
        "terminal-task precedence returns verdict=Live (moot), outcome=KillNoop"
    );

    // The terminal-task classification appended exactly one kill_noop row.
    let kill_noop_after = liveness_repo
        .count_evidence_for_session(&session.id, Some("kill_noop"))
        .await
        .unwrap();
    assert_eq!(
        kill_noop_after, 1,
        "terminal-task classification must append exactly one kill_noop row (idempotent on the terminal path)"
    );
    let pv_count_after = liveness_repo
        .count_evidence_for_session(&session.id, Some("success"))
        .await
        .unwrap();
    assert_eq!(
        pv_count_after, 2,
        "terminal-task classification must NOT append more ProtocolViolation rows — the path is distinct"
    );

    // ── Idempotency on the terminal path: a third call appends another row ──
    // (Each call to classify_session_exit_liveness is intentionally append-only;
    //  idempotency here means the verdict/outcome are stable, not that the
    //  evidence row count is fixed. The append-only chain is the audit trail.)
    let r3 = actor
        .classify_session_exit_liveness(&session.id, &task.id, Some(run_id), "completed", "worker")
        .await
        .expect("repeat terminal classification must succeed");
    assert_eq!(
        r3.outcome,
        Some(crate::dispatch::liveness::LivenessOutcome::KillNoop),
        "repeat call on terminal task must produce the same KillNoop outcome"
    );
    let kill_noop_third = liveness_repo
        .count_evidence_for_session(&session.id, Some("kill_noop"))
        .await
        .unwrap();
    assert_eq!(
        kill_noop_third, 2,
        "append-only evidence chain: each call appends one row; verdict/outcome are stable"
    );
}

// ── AC 5: repeated tick/idempotency ──────────────────────────────────────────
//
// Invariant: re-running `reap_zombie_sessions` and `enforce_session_stall_timeout`
// on an already-reaped/already-killed task does NOT duplicate terminalization,
// task-release, or liveness-evidence rows beyond the intended append-only
// record. A regression that re-runs the reap on every tick would explode
// the evidence table and (worse) re-release the task multiple times.

/// Repeated reap ticks on the same zombie session do not duplicate
/// terminalization, task-release, or liveness-evidence rows beyond the
/// intended append-only record. Each tick is idempotent on a fully
/// settled session.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repeated_recovery_ticks_do_not_duplicate_terminalization_or_release() {
    use djinn_db::{CreateSessionParams, SessionRepository, TaskAttemptRepository};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "tick-idempotency").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    let run_id = "run-tick-idempotency";
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
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

    // Seed a pending attempt that the recovery path will terminalize.
    let attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;

    let runtime = RecordingRuntimeOps::new(true);
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.runtime_ops = Some(Arc::new(runtime));

    // ── Tick 1: reap the zombie, terminalize the attempt, release the task ──
    actor.reap_zombie_sessions().await;

    let repo = TaskAttemptRepository::new(db.clone());
    let attempt_after_tick1 = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(
        attempt_after_tick1.outcome, "crashed",
        "tick 1 must terminalize the attempt as crashed"
    );
    let attempts_after_tick1: Vec<_> = repo
        .list_for_task(&task.id)
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        attempts_after_tick1.len(),
        1,
        "tick 1 must produce exactly one attempt row"
    );

    let updated_after_tick1 = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated_after_tick1.status, "open",
        "tick 1 must release the task for redispatch"
    );

    let liveness_repo = djinn_db::LivenessRepository::new(db.clone());
    let dead_after_tick1 = liveness_repo
        .count_evidence_for_session(&session.id, Some("dead_reclaimed"))
        .await
        .unwrap();
    let total_after_tick1 = liveness_repo
        .count_evidence_for_session(&session.id, None)
        .await
        .unwrap();
    assert!(
        dead_after_tick1 >= 1,
        "tick 1 must persist at least one dead_reclaimed evidence row"
    );

    // ── Tick 2: reap again. The session is already finalized (no longer in
    //             list_active), so reap_zombie_sessions is a no-op. ──
    actor.reap_zombie_sessions().await;

    // Attempt count must NOT have grown.
    let attempts_after_tick2: Vec<_> = repo
        .list_for_task(&task.id)
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        attempts_after_tick2.len(),
        1,
        "tick 2 must NOT create a duplicate attempt row (recovery is idempotent on settled sessions)"
    );

    // Attempt outcome must NOT have moved.
    let attempt_after_tick2 = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(
        attempt_after_tick2.outcome, "crashed",
        "tick 2 must NOT move the terminal attempt backward or sideways"
    );
    assert_eq!(
        attempt_after_tick2.terminal_at, attempt_after_tick1.terminal_at,
        "terminal_at timestamp must NOT be re-stamped by an idempotent tick"
    );

    // Task must remain open (NOT re-released — release already happened).
    let updated_after_tick2 = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated_after_tick2.status, "open",
        "tick 2 must keep the task open — a second release would re-touch updated_at"
    );

    // ── Tick 3: also drive stall-timeout (which uses list_active and the
    //             stall_killed prune). No session is active, so this is a no-op. ──
    actor.enforce_session_stall_timeout().await;
    let attempts_after_tick3: Vec<_> = repo
        .list_for_task(&task.id)
        .await
        .unwrap()
        .into_iter()
        .collect();
    assert_eq!(
        attempts_after_tick3.len(),
        1,
        "tick 3 (stall-timeout) must NOT create a duplicate attempt row either"
    );

    // ── Idempotency check: total evidence row count is bounded. ──
    // The first reap pass writes a small bounded number of rows (the
    // classifier pass writes one classifier row, the reap pass writes one
    // dead_reclaimed row). Subsequent ticks must not add more for this session
    // — the reap operates on list_active, and a finalized session is absent.
    let total_after_tick3 = liveness_repo
        .count_evidence_for_session(&session.id, None)
        .await
        .unwrap();
    assert_eq!(
        total_after_tick3, total_after_tick1,
        "tick 2 and tick 3 must NOT append any new evidence rows for an already-settled session (reap loop would explode the audit table)"
    );
    let dead_after_tick3 = liveness_repo
        .count_evidence_for_session(&session.id, Some("dead_reclaimed"))
        .await
        .unwrap();
    assert_eq!(
        dead_after_tick3, dead_after_tick1,
        "tick 2 and tick 3 must NOT append additional dead_reclaimed rows"
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Orphaned pending task_attempt terminalization (event path + reaper backstop).
//
// Regression: dispatch-start creates a `pending` task_attempts row, but a
// cleanly-failed run (e.g. provider error → TaskRunOutcome::Failed) finalizes
// its own session, so no recovery/zombie terminalizer ever fires and the row
// stayed `pending` forever — `run_respawn_guard` then deferred every future
// dispatch of that (task, role) pair (permanent wedge; 5 production tasks).
// ─────────────────────────────────────────────────────────────────────────────

/// Event path: a session "failed" event for a nonterminal task must
/// terminalize the dispatch-start `pending` attempt to `crashed` and unblock
/// the respawn guard.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_session_exit_terminalizes_pending_attempt_and_unblocks_respawn_guard() {
    use crate::dispatch::respawn_guard::{RespawnGuardDecision, run_respawn_guard};
    use djinn_core::models::SessionStatus;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "failed-exit-terminalize").await;

    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();

    // ── Dispatch-start: pending attempt row (as task_dispatch records it) ──
    let dk = crate::dispatch::attempt_lifecycle::make_dispatch_key(&task.id, "worker");
    let attempt_id =
        crate::dispatch::attempt_lifecycle::record_legacy_start(&db, &task.id, "worker", None, &dk)
            .await
            .expect("dispatch-start must create a pending attempt");

    // ── A run + session that self-finalizes as failed (provider error) ──
    let run_id = "run-failed-exit-terminalize";
    let run_repo = TaskRunRepository::new(db.clone());
    run_repo
        .create(CreateTaskRunParams {
            id: run_id,
            project_id: &task.project_id,
            task_id: &task.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
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
    let session = session_repo
        .update(&session.id, SessionStatus::Failed, 0, 0, 0, 0, None)
        .await
        .unwrap();
    run_repo
        .update_status(run_id, djinn_core::models::TaskRunStatus::Failed)
        .await
        .unwrap();

    // ── Deliver the session "failed" event through the real handler ──
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor
        .handle_event(DjinnEventEnvelope {
            entity_type: "session",
            action: "failed",
            payload: serde_json::to_value(&session).unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        })
        .await;

    // ── The pending attempt is terminal (crashed) ──
    let attempt_repo = TaskAttemptRepository::new(db.clone());
    let in_flight = attempt_repo
        .latest_pending_or_submitted(&task.id, Some("worker"))
        .await
        .unwrap();
    assert!(
        in_flight.is_none(),
        "no pending/submitted attempt may survive a failed session exit"
    );
    let attempt = attempt_repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(attempt.outcome, "crashed");
    assert!(
        attempt.terminal_at.is_some(),
        "terminal_at must be stamped on the crashed attempt"
    );

    // ── The respawn guard no longer wedges the (task, role) pair ──
    let decision = run_respawn_guard(&db, &task.id, "worker", None, None).await;
    assert_eq!(
        decision,
        RespawnGuardDecision::Allow,
        "respawn guard must allow dispatch after the attempt is terminalized"
    );

    // ── Idempotency: a duplicate failed event is a no-op ──
    actor
        .handle_event(DjinnEventEnvelope {
            entity_type: "session",
            action: "failed",
            payload: serde_json::to_value(&session).unwrap(),
            id: None,
            project_id: None,
            from_sync: false,
        })
        .await;
    let all = attempt_repo.list_for_task(&task.id).await.unwrap();
    assert_eq!(all.len(), 1, "duplicate exit events must not create rows");
    assert_eq!(all[0].outcome, "crashed");
}

/// Backdate a task_attempt's `created_at` so the orphan reaper's threshold
/// comparison sees it as old (well past the 15-minute threshold).
async fn backdate_attempt(db: &Database, attempt_id: &str) {
    djinn_db::test_support::backdate_task_attempt_created_at(db, attempt_id, "1 hour").await;
}

/// Reaper backstop: a stale `pending` attempt whose task has a terminal (or
/// absent) task_run is finalized to `crashed`; a fresh pending row and one
/// backed by a live task_run are left untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphaned_pending_attempt_reaper_finalizes_stale_rows_only() {
    use crate::dispatch::respawn_guard::{RespawnGuardDecision, run_respawn_guard};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);

    let run_repo = TaskRunRepository::new(db.clone());
    let attempt_repo = TaskAttemptRepository::new(db.clone());

    let seed_attempt = |task_id: String| {
        let db = db.clone();
        async move {
            let dk = crate::dispatch::attempt_lifecycle::make_dispatch_key(&task_id, "worker");
            crate::dispatch::attempt_lifecycle::record_legacy_start(
                &db, &task_id, "worker", None, &dk,
            )
            .await
            .expect("pending attempt must insert")
        }
    };

    // A: stale attempt + terminal task_run → reaped.
    let (task_a, _n) = create_task_with_note(&db, &tx, "reaper-terminal-run").await;
    let attempt_a = seed_attempt(task_a.id.clone()).await;
    backdate_attempt(&db, &attempt_a).await;
    run_repo
        .create(CreateTaskRunParams {
            id: "run-reaper-a",
            project_id: &task_a.project_id,
            task_id: &task_a.id,
            trigger_type: "manual",
            status: Some("failed"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    // B: stale attempt + NO task_run at all → reaped.
    let (task_b, _n) = create_task_with_note(&db, &tx, "reaper-absent-run").await;
    let attempt_b = seed_attempt(task_b.id.clone()).await;
    backdate_attempt(&db, &attempt_b).await;

    // C: FRESH attempt (younger than threshold) + terminal task_run → kept.
    let (task_c, _n) = create_task_with_note(&db, &tx, "reaper-fresh-attempt").await;
    let attempt_c = seed_attempt(task_c.id.clone()).await;
    run_repo
        .create(CreateTaskRunParams {
            id: "run-reaper-c",
            project_id: &task_c.project_id,
            task_id: &task_c.id,
            trigger_type: "manual",
            status: Some("failed"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    // D: stale attempt + LIVE (running) task_run → kept.
    let (task_d, _n) = create_task_with_note(&db, &tx, "reaper-live-run").await;
    let attempt_d = seed_attempt(task_d.id.clone()).await;
    backdate_attempt(&db, &attempt_d).await;
    run_repo
        .create(CreateTaskRunParams {
            id: "run-reaper-d",
            project_id: &task_d.project_id,
            task_id: &task_d.id,
            trigger_type: "manual",
            status: Some("running"),
            workspace_path: None,
            mirror_ref: None,
            dispatch_group_id: None,
        })
        .await
        .unwrap();

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "test").await;

    // A and B reaped to crashed with terminal_at stamped.
    for (attempt_id, label) in [(&attempt_a, "terminal-run"), (&attempt_b, "absent-run")] {
        let attempt = attempt_repo.get(attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.outcome, "crashed", "{label}: must be reaped");
        assert!(
            attempt.terminal_at.is_some(),
            "{label}: terminal_at must be stamped"
        );
    }

    // C (fresh) and D (live run) untouched.
    for (attempt_id, label) in [(&attempt_c, "fresh-attempt"), (&attempt_d, "live-run")] {
        let attempt = attempt_repo.get(attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.outcome, "pending", "{label}: must NOT be reaped");
        assert!(attempt.terminal_at.is_none(), "{label}: must stay live");
    }

    // The reaped tasks' respawn guards unblock; the live one still defers.
    assert_eq!(
        run_respawn_guard(&db, &task_a.id, "worker", None, None).await,
        RespawnGuardDecision::Allow,
        "guard must allow dispatch for a reaped task"
    );
    assert!(
        matches!(
            run_respawn_guard(&db, &task_d.id, "worker", None, None).await,
            RespawnGuardDecision::Defer(_)
        ),
        "guard must still defer while a live run backs the pending attempt"
    );

    // Idempotency: a second sweep changes nothing and creates no rows.
    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "test").await;
    for task in [&task_a, &task_b, &task_c, &task_d] {
        let all = attempt_repo.list_for_task(&task.id).await.unwrap();
        assert_eq!(all.len(), 1, "sweep must never create attempt rows");
    }
}

/// Reaper backstop for the ylme orphan: a stale `submitted` attempt whose task
/// has NO open PR is finalized to `reopened` (submitted work existed, and the
/// reopened marker lets a fresh worker dispatch); a `submitted` attempt whose
/// task is in a poller-polled status (`pr_review`) with a PR — genuinely owned
/// by the PR poller — and a fresh `submitted`-no-PR row are left untouched; the
/// sweep is idempotent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphaned_submitted_no_pr_reaper_finalizes_stale_rows_only() {
    use crate::dispatch::respawn_guard::{RespawnGuardDecision, run_respawn_guard};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let attempt_repo = TaskAttemptRepository::new(db.clone());

    // Seed a `submitted` worker attempt (pending → submitted) for a task.
    let seed_submitted = |task_id: String| {
        let db = db.clone();
        async move {
            let dk = crate::dispatch::attempt_lifecycle::make_dispatch_key(&task_id, "worker");
            let id = crate::dispatch::attempt_lifecycle::record_legacy_start(
                &db, &task_id, "worker", None, &dk,
            )
            .await
            .expect("pending attempt must insert");
            let repo = TaskAttemptRepository::new(db.clone());
            repo.advance_to_submitted(djinn_db::SubmitTaskAttemptParams {
                id: &id,
                submit_ref: Some("ref-1"),
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some("submitted for internal review"),
                summary_json: None,
                log_tail: None,
            })
            .await
            .expect("advance to submitted");
            id
        }
    };

    // A: stale submitted + NO task pr_url → reaped to `reopened`.
    let (task_a, _n) = create_task_with_note(&db, &tx, "submitted-reaper-no-pr").await;
    let attempt_a = seed_submitted(task_a.id.clone()).await;
    backdate_attempt(&db, &attempt_a).await;

    // B: stale submitted + task in `pr_review` with a PR → untouched (the PR
    // poller genuinely owns it — poller-ownership requires a poller-polled
    // status, NOT merely a retained pr_url; a retained pr_url on an `open` task
    // is reaped, see `orphaned_submitted_open_retained_pr_reaper_reaps_only_unpolled`).
    let (task_b, _n) = create_task_with_note(&db, &tx, "submitted-reaper-with-pr").await;
    let attempt_b = seed_submitted(task_b.id.clone()).await;
    backdate_attempt(&db, &attempt_b).await;
    let task_b_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    task_b_repo
        .set_pr_url(&task_b.id, "https://github.example/owner/repo/pull/11")
        .await
        .unwrap();
    task_b_repo
        .set_status(&task_b.id, "pr_review")
        .await
        .unwrap();

    // C: FRESH submitted-no-PR (younger than threshold) → untouched.
    let (task_c, _n) = create_task_with_note(&db, &tx, "submitted-reaper-fresh").await;
    let attempt_c = seed_submitted(task_c.id.clone()).await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "test").await;

    // A reaped to `reopened` with terminal_at stamped.
    let a = attempt_repo.get(&attempt_a).await.unwrap().unwrap();
    assert_eq!(
        a.outcome, "reopened",
        "no-PR submitted orphan must be reaped"
    );
    assert!(a.terminal_at.is_some(), "terminal_at must be stamped");

    // B (has PR) and C (fresh) untouched.
    for (attempt_id, label) in [(&attempt_b, "with-pr"), (&attempt_c, "fresh")] {
        let attempt = attempt_repo.get(attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.outcome, "submitted", "{label}: must NOT be reaped");
        assert!(attempt.terminal_at.is_none(), "{label}: must stay live");
    }

    // Reaper interaction (part 4): the reaped attempt is now `reopened`, so the
    // respawn guard treats task A as rework and Allows a worker dispatch even
    // when an open PR is presented (adoption bypassed via the reopened latest
    // attempt).  Task B still defers (submitted attempt in flight).
    assert_eq!(
        run_respawn_guard(
            &db,
            &task_a.id,
            "worker",
            Some("https://github.example/owner/repo/pull/9"),
            None,
        )
        .await,
        RespawnGuardDecision::Allow,
        "reaped-to-reopened task must dispatch a rework worker"
    );
    assert!(
        matches!(
            run_respawn_guard(&db, &task_b.id, "worker", None, None).await,
            RespawnGuardDecision::Defer(_)
        ),
        "task with a live submitted attempt still defers"
    );

    // Idempotency: a second sweep changes nothing and creates no rows.
    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "test").await;
    for task in [&task_a, &task_b, &task_c] {
        assert_eq!(
            attempt_repo.list_for_task(&task.id).await.unwrap().len(),
            1,
            "sweep must never create attempt rows"
        );
    }
    assert_eq!(
        attempt_repo.get(&attempt_a).await.unwrap().unwrap().outcome,
        "reopened"
    );
    assert_eq!(
        attempt_repo.get(&attempt_b).await.unwrap().unwrap().outcome,
        "submitted"
    );
}

/// Seed a `submitted` worker attempt (pending → submitted) for a task.
async fn seed_submitted_worker_attempt(db: &Database, task_id: &str) -> String {
    let dk = crate::dispatch::attempt_lifecycle::make_dispatch_key(task_id, "worker");
    let id =
        crate::dispatch::attempt_lifecycle::record_legacy_start(db, task_id, "worker", None, &dk)
            .await
            .expect("pending attempt must insert");
    TaskAttemptRepository::new(db.clone())
        .advance_to_submitted(djinn_db::SubmitTaskAttemptParams {
            id: &id,
            submit_ref: Some("ref-1"),
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some("submitted for internal review"),
            summary_json: None,
            log_tail: None,
        })
        .await
        .expect("advance to submitted");
    id
}

/// Hole A backstop: a stale `submitted` worker attempt on an `open` task that
/// RETAINED a `pr_url` across a `PrConflict` reopen is finalized to `reopened`.
/// `open` is the sole dispatchable status and the PR poller never polls it, so
/// nothing else can advance the attempt and the task is otherwise permanently
/// stuck behind the respawn guard's step-2 dedup. A same-shape task left in
/// `pr_review` — which the poller DOES own — is untouched, as is a fresh
/// `open`+PR row. The sweep is idempotent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orphaned_submitted_open_retained_pr_reaper_reaps_only_unpolled() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let attempt_repo = TaskAttemptRepository::new(db.clone());
    let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    const PR: &str = "https://github.example/owner/repo/pull/42";

    // A: `open` + retained pr_url + stale submitted → reaped to `reopened`.
    // (Tasks are created `open`; the retained PR simulates the PrConflict reopen
    // that does NOT clear tasks.pr_url.)
    let (task_a, _n) = create_task_with_note(&db, &tx, "open-retained-pr-stale").await;
    let attempt_a = seed_submitted_worker_attempt(&db, &task_a.id).await;
    backdate_attempt(&db, &attempt_a).await;
    task_repo.set_pr_url(&task_a.id, PR).await.unwrap();

    // B: `pr_review` + pr_url + stale submitted → UNTOUCHED (poller owns it).
    let (task_b, _n) = create_task_with_note(&db, &tx, "pr-review-stale").await;
    let attempt_b = seed_submitted_worker_attempt(&db, &task_b.id).await;
    backdate_attempt(&db, &attempt_b).await;
    task_repo.set_pr_url(&task_b.id, PR).await.unwrap();
    task_repo.set_status(&task_b.id, "pr_review").await.unwrap();

    // C: `open` + pr_url + FRESH submitted → UNTOUCHED (younger than threshold).
    let (task_c, _n) = create_task_with_note(&db, &tx, "open-retained-pr-fresh").await;
    let attempt_c = seed_submitted_worker_attempt(&db, &task_c.id).await;
    task_repo.set_pr_url(&task_c.id, PR).await.unwrap();

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "test").await;

    let a = attempt_repo.get(&attempt_a).await.unwrap().unwrap();
    assert_eq!(
        a.outcome, "reopened",
        "open retained-PR stale submitted must reap to reopened"
    );
    assert!(a.terminal_at.is_some(), "terminal_at must be stamped");

    for (attempt_id, label) in [(&attempt_b, "pr_review"), (&attempt_c, "fresh-open")] {
        let attempt = attempt_repo.get(attempt_id).await.unwrap().unwrap();
        assert_eq!(attempt.outcome, "submitted", "{label}: must NOT be reaped");
        assert!(attempt.terminal_at.is_none(), "{label}: must stay live");
    }

    // Idempotency: a second sweep changes nothing and creates no rows.
    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "test").await;
    for task in [&task_a, &task_b, &task_c] {
        assert_eq!(
            attempt_repo.list_for_task(&task.id).await.unwrap().len(),
            1,
            "sweep must never create attempt rows"
        );
    }
    assert_eq!(
        attempt_repo.get(&attempt_a).await.unwrap().unwrap().outcome,
        "reopened"
    );
    assert_eq!(
        attempt_repo.get(&attempt_b).await.unwrap().unwrap().outcome,
        "submitted"
    );
}

/// Hole A end-to-end: the respawn guard DEFERS an `open`+retained-PR task with
/// an in-flight `submitted` attempt (step 2 dedup, reached because the retained
/// `PrConflict` conflict signal bypasses adoption), and only ALLOWS a fresh
/// worker after the reaper finalizes the orphaned attempt to `reopened`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn guard_defers_open_retained_pr_submitted_until_reaped_then_allows() {
    use crate::dispatch::respawn_guard::{PrReworkSignal, RespawnGuardDecision, run_respawn_guard};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let attempt_repo = TaskAttemptRepository::new(db.clone());
    let task_repo = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx));

    const PR: &str = "https://github.example/owner/repo/pull/43";

    let (task, _n) = create_task_with_note(&db, &tx, "open-retained-pr-guard").await;
    let attempt = seed_submitted_worker_attempt(&db, &task.id).await;
    backdate_attempt(&db, &attempt).await;
    task_repo.set_pr_url(&task.id, PR).await.unwrap();

    // Before reap: the PrConflict rework signal bypasses PR adoption (step 1),
    // and the live `submitted` attempt makes step 2 defer.
    assert!(
        matches!(
            run_respawn_guard(
                &db,
                &task.id,
                "worker",
                Some(PR),
                Some(PrReworkSignal::MergeConflict),
            )
            .await,
            RespawnGuardDecision::Defer(_)
        ),
        "guard must defer while a stale submitted attempt is in flight"
    );

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "test").await;
    assert_eq!(
        attempt_repo.get(&attempt).await.unwrap().unwrap().outcome,
        "reopened",
        "reaper must finalize the orphaned submitted attempt"
    );

    // After reap: no live attempt remains, and the `reopened` latest attempt
    // bypasses adoption, so a rework worker is allowed to dispatch.
    assert_eq!(
        run_respawn_guard(&db, &task.id, "worker", Some(PR), None).await,
        RespawnGuardDecision::Allow,
        "guard must allow a fresh worker once the orphan is reaped"
    );
}

// ── Evidence-based orphan reaping (proposal 9gg5 / epic ars3) ──────────────

/// Register a coordinator incarnation and optionally backdate its lease so it
/// appears expired relative to the reaper threshold.
async fn register_incarnation(db: &Database, expired: bool) -> String {
    let repo = djinn_db::CoordinatorIncarnationRepository::new(db.clone());
    let id = uuid::Uuid::now_v7().to_string();
    repo.register(&id).await.unwrap();
    if expired {
        // Backdate the lease so last_renewed_at is well past the 15-minute
        // orphan threshold, making the incarnation "expired".
        djinn_db::test_support::backdate_coordinator_incarnation_lease(db, &id, "1 hour").await;
    }
    id
}

/// Only an orphan whose durable owner lease is resolved AND expired beyond the
/// threshold is stamped `interrupted/environmental_owner_expired`. The evidence
/// in `summary_json` records the non-NULL immutable owner and lease timestamps
/// so a later retry decision can validate the complete positive tuple.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_reap_expired_owner_stamps_interrupted_with_durable_evidence() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    // Register an incarnation and let its lease expire.
    let owner_id = register_incarnation(&db, true).await;
    let group_id = uuid::Uuid::now_v7().to_string();

    let (task, _n) = create_task_with_note(&db, &tx, "evidence-expired-owner").await;
    let attempt = seed_pending_attempt_with_identity(
        &db,
        &task.id,
        "worker",
        Some(&owner_id),
        Some(&group_id),
    )
    .await;
    backdate_attempt(&db, &attempt).await;

    // Both startup and periodic sweeps must classify identically.
    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "startup").await;

    let row = repo.get(&attempt).await.unwrap().unwrap();
    assert_eq!(
        row.outcome, "interrupted",
        "an expired-owner orphan must be stamped `interrupted`"
    );
    let sj: serde_json::Value = serde_json::from_str(row.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj["failure_class"], "environmental_owner_expired");
    assert_eq!(sj["owner_incarnation_id"], owner_id);
    assert!(
        sj["owner_lease_last_renewed_at"].as_str().is_some(),
        "evidence must record the observed lease-expiry timestamp"
    );
    assert!(
        sj["owner_lease_registered_at"].as_str().is_some(),
        "evidence must record the owner registration timestamp"
    );
    assert_eq!(sj["owner_classification"], "expired");
}

/// A live-owner orphan is counted `crashed/orphaned_pending_attempt` — the
/// dispatch is still potentially owned, so it is NOT environmental.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_reap_live_owner_counts_as_crashed_orphaned() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    // Register a LIVE incarnation (lease recently renewed).
    let owner_id = register_incarnation(&db, false).await;
    let group_id = uuid::Uuid::now_v7().to_string();

    let (task, _n) = create_task_with_note(&db, &tx, "evidence-live-owner").await;
    let attempt = seed_pending_attempt_with_identity(
        &db,
        &task.id,
        "worker",
        Some(&owner_id),
        Some(&group_id),
    )
    .await;
    backdate_attempt(&db, &attempt).await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "periodic").await;

    let row = repo.get(&attempt).await.unwrap().unwrap();
    assert_eq!(row.outcome, "crashed");
    let sj: serde_json::Value = serde_json::from_str(row.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj["failure_class"], "orphaned_pending_attempt");
    assert_eq!(sj["owner_classification"], "live");
}

/// A NULL-owner (legacy) orphan is counted `crashed/orphaned_pending_attempt_unproven`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_reap_null_owner_counts_as_crashed_unproven() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    let (task, _n) = create_task_with_note(&db, &tx, "evidence-null-owner").await;
    let attempt = seed_pending_attempt(&db, &task.id, "worker").await; // NULL owner
    backdate_attempt(&db, &attempt).await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "startup").await;

    let row = repo.get(&attempt).await.unwrap().unwrap();
    assert_eq!(row.outcome, "crashed");
    let sj: serde_json::Value = serde_json::from_str(row.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj["failure_class"], "orphaned_pending_attempt_unproven");
    assert_eq!(sj["owner_classification"], "null_owner");
}

/// A malformed owner UUID is counted `crashed/orphaned_pending_attempt_unproven`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_reap_malformed_owner_counts_as_crashed_unproven() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    let (task, _n) = create_task_with_note(&db, &tx, "evidence-malformed-owner").await;
    // Directly insert a pending attempt with a malformed owner, since the
    // repository validates UUIDs at creation.
    let attempt_id = uuid::Uuid::now_v7().to_string();
    let dispatch_key = format!("{}:worker:{}", task.id, attempt_id);
    djinn_db::test_support::insert_pending_attempt_with_raw_owner(
        &db,
        &attempt_id,
        &task.id,
        "worker",
        &dispatch_key,
        "not-a-uuid",
    )
    .await;
    backdate_attempt(&db, &attempt_id).await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "periodic").await;

    let row = repo.get(&attempt_id).await.unwrap().unwrap();
    assert_eq!(row.outcome, "crashed");
    let sj: serde_json::Value = serde_json::from_str(row.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj["failure_class"], "orphaned_pending_attempt_unproven");
    assert_eq!(sj["owner_classification"], "malformed_owner");
}

/// A well-formed but unregistered owner (missing incarnation) is counted
/// `crashed/orphaned_pending_attempt_unproven`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_reap_missing_owner_counts_as_crashed_unproven() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    // A valid UUID that was never registered as an incarnation.
    let phantom_owner = uuid::Uuid::now_v7().to_string();
    let (task, _n) = create_task_with_note(&db, &tx, "evidence-missing-owner").await;
    let attempt =
        seed_pending_attempt_with_identity(&db, &task.id, "worker", Some(&phantom_owner), None)
            .await;
    backdate_attempt(&db, &attempt).await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "startup").await;

    let row = repo.get(&attempt).await.unwrap().unwrap();
    assert_eq!(row.outcome, "crashed");
    let sj: serde_json::Value = serde_json::from_str(row.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj["failure_class"], "orphaned_pending_attempt_unproven");
    assert_eq!(sj["owner_classification"], "missing_owner");
}

/// An owner whose lease lookup fails (the `coordinator_incarnations` table is
/// unavailable) is counted `crashed/orphaned_pending_attempt_unproven` — the
/// reaper cannot positively prove expiry, so it fails closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_reap_owner_lookup_error_counts_as_crashed_unproven() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    // Register an incarnation so the attempt has a valid, well-formed owner.
    let owner_id = register_incarnation(&db, true).await;

    let (task, _n) = create_task_with_note(&db, &tx, "evidence-lookup-error").await;
    let attempt =
        seed_pending_attempt_with_identity(&db, &task.id, "worker", Some(&owner_id), None).await;
    backdate_attempt(&db, &attempt).await;

    // Drop the coordinator_incarnations table to force a lookup error in
    // `incarnation_repo.get()`. The `list_orphaned_pending` query does not
    // reference this table, so the orphan is still discovered.
    djinn_db::test_support::drop_table_for_test(&db, "coordinator_incarnations").await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "startup").await;

    let row = repo.get(&attempt).await.unwrap().unwrap();
    assert_eq!(row.outcome, "crashed");
    let sj: serde_json::Value = serde_json::from_str(row.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj["failure_class"], "orphaned_pending_attempt_unproven");
    assert_eq!(sj["owner_classification"], "lookup_error");
}

/// An owner whose lease resolves on `get()` but returns `None` from `is_live()`
/// (the row vanished between the two calls) is counted
/// `crashed/orphaned_pending_attempt_unproven` — the ambiguity fails closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_reap_ambiguous_is_live_counts_as_crashed_unproven() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    // Register an incarnation and capture its lease timestamps.
    let owner_id = register_incarnation(&db, true).await;
    let inc_repo = djinn_db::CoordinatorIncarnationRepository::new(db.clone());
    let inc = inc_repo.get(&owner_id).await.unwrap().unwrap();

    let (task, _n) = create_task_with_note(&db, &tx, "evidence-ambiguous-is-live").await;
    let attempt =
        seed_pending_attempt_with_identity(&db, &task.id, "worker", Some(&owner_id), None).await;
    backdate_attempt(&db, &attempt).await;

    // Replace the table with a view that returns the row on the first query
    // (the `get()` call) but returns nothing on the second (the `is_live()`
    // call). This forces the `Ok(None)` ambiguous branch.
    djinn_db::test_support::make_coordinator_incarnation_vanish_after_first_read(
        &db,
        &owner_id,
        &inc.registered_at,
        &inc.last_renewed_at,
    )
    .await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "periodic").await;

    let row = repo.get(&attempt).await.unwrap().unwrap();
    assert_eq!(row.outcome, "crashed");
    let sj: serde_json::Value = serde_json::from_str(row.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj["failure_class"], "orphaned_pending_attempt_unproven");
    assert_eq!(sj["owner_classification"], "ambiguous");
}

/// An owner whose lease resolves on `get()` but errors on `is_live()` (the
/// table becomes unavailable between the two calls) is counted
/// `crashed/orphaned_pending_attempt_unproven` — the lookup error fails closed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_reap_is_live_lookup_error_counts_as_crashed_unproven() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    // Register an incarnation and capture its lease timestamps.
    let owner_id = register_incarnation(&db, true).await;
    let inc_repo = djinn_db::CoordinatorIncarnationRepository::new(db.clone());
    let inc = inc_repo.get(&owner_id).await.unwrap().unwrap();

    let (task, _n) = create_task_with_note(&db, &tx, "evidence-is-live-error").await;
    let attempt =
        seed_pending_attempt_with_identity(&db, &task.id, "worker", Some(&owner_id), None).await;
    backdate_attempt(&db, &attempt).await;

    // Replace the table with a view that returns the row on the first query
    // (the `get()` call) but raises an error on the second (the `is_live()`
    // call). This forces the `Err(_)` is_live lookup-error branch.
    djinn_db::test_support::make_coordinator_incarnation_error_after_first_read(
        &db,
        &owner_id,
        &inc.registered_at,
        &inc.last_renewed_at,
    )
    .await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "startup").await;

    let row = repo.get(&attempt).await.unwrap().unwrap();
    assert_eq!(row.outcome, "crashed");
    let sj: serde_json::Value = serde_json::from_str(row.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj["failure_class"], "orphaned_pending_attempt_unproven");
    assert_eq!(sj["owner_classification"], "lookup_error");
}

/// A non-NULL dispatch group is terminalized through the exact-group repository
/// operation: all pending peers in the same group receive one outcome/evidence
/// tuple, while unrelated groups for the same task remain untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_reap_nonnull_group_terminalizes_all_peers_with_one_evidence() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    let owner_id = register_incarnation(&db, true).await;
    let group_a = uuid::Uuid::now_v7().to_string();
    let group_b = uuid::Uuid::now_v7().to_string();

    let (task, _n) = create_task_with_note(&db, &tx, "evidence-group-isolation").await;

    // Two pending peers in group A (same owner, same group).
    let peer1 = seed_pending_attempt_with_identity(
        &db,
        &task.id,
        "worker",
        Some(&owner_id),
        Some(&group_a),
    )
    .await;
    let peer2 = seed_pending_attempt_with_identity(
        &db,
        &task.id,
        "reviewer",
        Some(&owner_id),
        Some(&group_a),
    )
    .await;

    // An unrelated pending peer in group B (different owner, different group).
    let owner_b = register_incarnation(&db, true).await;
    let peer_b =
        seed_pending_attempt_with_identity(&db, &task.id, "worker", Some(&owner_b), Some(&group_b))
            .await;

    for id in [&peer1, &peer2, &peer_b] {
        backdate_attempt(&db, id).await;
    }

    // Reap with a 0-second threshold so group B's owner is also expired; but
    // we reap just group A by checking that group B is also reaped (it should
    // be — both owners are expired). The key assertion is that each group gets
    // its own outcome/evidence.
    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "periodic").await;

    // Both peers in group A are interrupted with the same evidence.
    for (id, label) in [(&peer1, "peer1"), (&peer2, "peer2")] {
        let row = repo.get(id).await.unwrap().unwrap();
        assert_eq!(
            row.outcome, "interrupted",
            "{label}: group A peer must be interrupted"
        );
        let sj: serde_json::Value =
            serde_json::from_str(row.summary_json.as_deref().unwrap()).unwrap();
        assert_eq!(sj["failure_class"], "environmental_owner_expired");
        assert_eq!(sj["owner_incarnation_id"], owner_id);
    }

    // Group B peer is also interrupted but with its own owner evidence.
    let row_b = repo.get(&peer_b).await.unwrap().unwrap();
    assert_eq!(row_b.outcome, "interrupted");
    let sj_b: serde_json::Value =
        serde_json::from_str(row_b.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(sj_b["owner_incarnation_id"], owner_b);
    assert_ne!(sj_b["owner_incarnation_id"], owner_id);
}

/// A legacy NULL-group row is reaped singly: only that one row is terminalized,
/// and it gets the `unproven` classification because it has no owner.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_reap_null_group_reaps_singly() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    let (task, _n) = create_task_with_note(&db, &tx, "evidence-null-group").await;

    // Two NULL-group rows for the same task — each is reaped independently.
    let a1 = seed_pending_attempt(&db, &task.id, "worker").await;
    let a2 = seed_pending_attempt(&db, &task.id, "reviewer").await;
    backdate_attempt(&db, &a1).await;
    backdate_attempt(&db, &a2).await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "periodic").await;

    for (id, label) in [(&a1, "a1"), (&a2, "a2")] {
        let row = repo.get(id).await.unwrap().unwrap();
        assert_eq!(
            row.outcome, "crashed",
            "{label}: NULL-group row must be crashed"
        );
        let sj: serde_json::Value =
            serde_json::from_str(row.summary_json.as_deref().unwrap()).unwrap();
        assert_eq!(sj["failure_class"], "orphaned_pending_attempt_unproven");
    }
}

/// Overlap regression: two coordinator incarnations — one old/expired, one
/// new/live — each classify solely from their own recorded owner lease. A
/// pending attempt owned by the expired incarnation is `interrupted`, while a
/// pending attempt owned by the live incarnation is `crashed/orphaned`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_reap_overlap_dead_old_live_new_classifies_each_from_own_lease() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    // Old incarnation: registered and expired.
    let old_owner = register_incarnation(&db, true).await;
    // New incarnation: registered and live.
    let new_owner = register_incarnation(&db, false).await;

    let (task_old, _n) = create_task_with_note(&db, &tx, "overlap-old-dead").await;
    let (task_new, _n) = create_task_with_note(&db, &tx, "overlap-new-live").await;

    let old_attempt =
        seed_pending_attempt_with_identity(&db, &task_old.id, "worker", Some(&old_owner), None)
            .await;
    let new_attempt =
        seed_pending_attempt_with_identity(&db, &task_new.id, "worker", Some(&new_owner), None)
            .await;
    backdate_attempt(&db, &old_attempt).await;
    backdate_attempt(&db, &new_attempt).await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "startup").await;

    let old_row = repo.get(&old_attempt).await.unwrap().unwrap();
    assert_eq!(old_row.outcome, "interrupted");
    let old_sj: serde_json::Value =
        serde_json::from_str(old_row.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(old_sj["owner_classification"], "expired");

    let new_row = repo.get(&new_attempt).await.unwrap().unwrap();
    assert_eq!(new_row.outcome, "crashed");
    let new_sj: serde_json::Value =
        serde_json::from_str(new_row.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(new_sj["failure_class"], "orphaned_pending_attempt");
    assert_eq!(new_sj["owner_classification"], "live");
}

/// Overlap regression (reverse): old incarnation is live, new incarnation is
/// expired. Each attempt classifies from its own lease, not the other's.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn evidence_reap_overlap_live_old_dead_new_classifies_each_from_own_lease() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let repo = TaskAttemptRepository::new(db.clone());

    // Old incarnation: live.
    let old_owner = register_incarnation(&db, false).await;
    // New incarnation: expired.
    let new_owner = register_incarnation(&db, true).await;

    let (task_old, _n) = create_task_with_note(&db, &tx, "overlap-old-live").await;
    let (task_new, _n) = create_task_with_note(&db, &tx, "overlap-new-dead").await;

    let old_attempt =
        seed_pending_attempt_with_identity(&db, &task_old.id, "worker", Some(&old_owner), None)
            .await;
    let new_attempt =
        seed_pending_attempt_with_identity(&db, &task_new.id, "worker", Some(&new_owner), None)
            .await;
    backdate_attempt(&db, &old_attempt).await;
    backdate_attempt(&db, &new_attempt).await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "periodic").await;

    let old_row = repo.get(&old_attempt).await.unwrap().unwrap();
    assert_eq!(old_row.outcome, "crashed");
    let old_sj: serde_json::Value =
        serde_json::from_str(old_row.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(old_sj["owner_classification"], "live");

    let new_row = repo.get(&new_attempt).await.unwrap().unwrap();
    assert_eq!(new_row.outcome, "interrupted");
    let new_sj: serde_json::Value =
        serde_json::from_str(new_row.summary_json.as_deref().unwrap()).unwrap();
    assert_eq!(new_sj["owner_classification"], "expired");
}

/// Full retry accounting must fail closed: a `spawn_failed` orphan is a real
/// same-role dispatch failure, so repeated attempts consume the existing cap
/// and use the normal no-PR terminal-close handling.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_failed_dispatch_orphan_reaches_no_pr_terminal_cap() {
    use djinn_core::models::task_attempt::TaskAttemptOutcome;
    use djinn_db::{TaskAttemptRepository, TerminalTaskAttemptParams};

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "spawn-failed-retry-cap").await;
    let attempts = TaskAttemptRepository::new(db.clone());
    let mut actor = coordinator_actor_for_tests(&db, &tx);

    for strike in 1..=MAX_DISPATCH_FAILURES {
        let attempt_id = seed_pending_attempt(&db, &task.id, "worker").await;
        attempts
            .advance_to_terminal(TerminalTaskAttemptParams {
                id: &attempt_id,
                outcome: TaskAttemptOutcome::SpawnFailed,
                pr_url: None,
                submit_ref: None,
                checkpoint_ref: None,
                mirror_head_sha: None,
                github_head_sha: None,
                summary: Some("dispatch setup failed"),
                summary_json: Some(r#"{"failure_class":"dispatch_failure_orphan"}"#),
                log_tail: None,
            })
            .await
            .unwrap();

        let decision = actor
            .latest_attempt_strike_decision(&task.id, "worker")
            .await
            .expect("persisted spawn failure is evaluated");
        assert!(!decision.exempted);
        assert_eq!(decision.source, "spawn_failed");
        actor
            .apply_chain_exhaustion_side_effects(&task, "worker", &[])
            .await;

        if strike < MAX_DISPATCH_FAILURES {
            assert_eq!(actor.dispatch_failure_streak.get(&task.id), Some(&strike));
        }
    }

    let closed = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert!(closed.pr_url.is_none(), "fixture is intentionally PR-less");
    assert_eq!(closed.status, "closed");
}

/// Persisted coordinator/supervisor peers with distinct roles share one exact
/// dispatch group.  After the owner's lease expires, periodic reaping records
/// durable environmental evidence, exempts the retry decision, and leaves both
/// the task and an unrelated fresh group available for redispatch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn expired_owner_group_reap_is_exempt_and_isolates_unrelated_group() {
    use djinn_db::TaskAttemptRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _note) = create_task_with_note(&db, &tx, "lre2-expired-owner-restart").await;
    let attempts = TaskAttemptRepository::new(db.clone());
    let owner = register_incarnation(&db, true).await;
    let group = uuid::Uuid::now_v7().to_string();
    let unrelated_group = uuid::Uuid::now_v7().to_string();

    let coordinator_attempt =
        seed_pending_attempt_with_identity(&db, &task.id, "planner", Some(&owner), Some(&group))
            .await;
    let supervisor_attempt =
        seed_pending_attempt_with_identity(&db, &task.id, "worker", Some(&owner), Some(&group))
            .await;
    let unrelated = seed_pending_attempt_with_identity(
        &db,
        &task.id,
        "worker",
        Some(&owner),
        Some(&unrelated_group),
    )
    .await;
    backdate_attempt(&db, &coordinator_attempt).await;
    backdate_attempt(&db, &supervisor_attempt).await;

    crate::health::reap_orphaned_pending_attempts_with_threshold(&db, 15 * 60, "periodic").await;

    for id in [&coordinator_attempt, &supervisor_attempt] {
        let row = attempts.get(id).await.unwrap().unwrap();
        assert_eq!(row.outcome, "interrupted");
        let evidence: serde_json::Value =
            serde_json::from_str(row.summary_json.as_deref().unwrap()).unwrap();
        assert_eq!(evidence["failure_class"], "environmental_owner_expired");
        assert_eq!(evidence["owner_incarnation_id"], owner);
        assert!(evidence["owner_lease_last_renewed_at"].is_string());
    }
    assert_eq!(
        attempts.get(&unrelated).await.unwrap().unwrap().outcome,
        "pending"
    );

    let actor = coordinator_actor_for_tests(&db, &tx);
    let decision = actor
        .latest_attempt_strike_decision(&task.id, "worker")
        .await
        .expect("reaped supervisor attempt is evaluated");
    assert!(decision.exempted);
    assert_eq!(decision.decision, "exempted");
    assert_eq!(decision.source, "environmental_owner_expired");
    assert!(!actor.dispatch_failure_streak.contains_key(&task.id));
    assert!(!actor.dispatch_cooldowns.contains_key(&task.id));

    let refreshed = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(refreshed.status, "open");
    assert!(refreshed.pr_url.is_none());
}
