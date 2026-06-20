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
    let mut app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
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
    sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = $1")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let run_id = "run-zombie-reap";
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) VALUES ($1, $2, $3, 'manual', 'running')",
    )
    .bind(run_id)
    .bind(&task.project_id)
    .bind(&task.id)
    .execute(db.pool())
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
        })
        .await
        .unwrap();
    // Backdate well past the 10-minute hard cap, leaving tokens at 0/0.
    // Match the column's stored format (VARCHAR `YYYY-MM-DDThh:mm:ss.msZ`)
    // so `parse_iso_elapsed` reads it.
    sqlx::query(
        "UPDATE sessions SET started_at = to_char(now() AT TIME ZONE 'utc' - interval '20 minutes', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1",
    )
        .bind(&session.id)
        .execute(db.pool())
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
    sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = $1")
        .bind(&task.id)
        .execute(db.pool())
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
    sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = $1")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let run_id = "run-connected-1";
    // `sessions.task_run_id` has an FK to `task_runs`, so seed the run row.
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) VALUES ($1, $2, $3, 'manual', 'running')",
    )
        .bind(run_id)
        .bind(&task.project_id)
        .bind(&task.id)
        .execute(db.pool())
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
        })
        .await
        .unwrap();
    // Backdate past the 10-minute hard cap, tokens still 0/0.
    sqlx::query(
        "UPDATE sessions SET started_at = to_char(now() AT TIME ZONE 'utc' - interval '20 minutes', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1",
    )
        .bind(&session.id)
        .execute(db.pool())
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
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) VALUES ($1, $2, $3, 'manual', 'running')",
    )
    .bind(run_id)
    .bind(&task.project_id)
    .bind(&task.id)
    .execute(db.pool())
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
        })
        .await
        .unwrap();
    sqlx::query(
        "UPDATE sessions SET started_at = to_char(now() AT TIME ZONE 'utc' - interval '40 minutes', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1",
    )
    .bind(&session.id)
    .execute(db.pool())
    .await
    .unwrap();

    let runtime = RecordingRuntimeOps::new(true);
    let mut app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(std::sync::Arc::new(runtime.clone()));
    let activity = app_state.register_activity(&task.id);
    let old = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(40 * 60))
        .unwrap_or(0);
    activity.store(old, std::sync::atomic::Ordering::Relaxed);
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
            let runner: crate::actors::slot::TestLifecycleRunner = std::sync::Arc::new(
                |_task_id, _project_path, _model_id, _app_state, kill, _pause| {
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
    sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = $1")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let run_id = "run-zombie-no-slot";
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) VALUES ($1, $2, $3, 'manual', 'running')",
    )
    .bind(run_id)
    .bind(&task.project_id)
    .bind(&task.id)
    .execute(db.pool())
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
        })
        .await
        .unwrap();
    sqlx::query(
        "UPDATE sessions SET started_at = to_char(now() AT TIME ZONE 'utc' - interval '20 minutes', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1",
    )
    .bind(&session.id)
    .execute(db.pool())
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
    sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = $1")
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();

    let run_id = "run-zombie-teardown-fail";
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status) VALUES ($1, $2, $3, 'manual', 'running')",
    )
    .bind(run_id)
    .bind(&task.project_id)
    .bind(&task.id)
    .execute(db.pool())
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
        })
        .await
        .unwrap();
    sqlx::query(
        "UPDATE sessions SET started_at = to_char(now() AT TIME ZONE 'utc' - interval '20 minutes', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1",
    )
    .bind(&session.id)
    .execute(db.pool())
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
    sqlx::query("UPDATE tasks SET status = 'in_progress' WHERE id = $1")
        .bind(&task.id)
        .execute(db.pool())
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
        })
        .await
        .unwrap();
    sqlx::query(
        "UPDATE sessions SET started_at = to_char(now() AT TIME ZONE 'utc' - interval '20 minutes', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id = $1",
    )
    .bind(&session.id)
    .execute(db.pool())
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
        sqlx::query(
            "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status)
             VALUES ($1, $2, $3, 'manual', 'running')",
        )
        .bind(id)
        .bind(&task.project_id)
        .bind(&task.id)
        .execute(db.pool())
        .await
        .unwrap();
    } else {
        sqlx::query(
            "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status, ended_at)
             VALUES ($1, $2, $3, 'manual', $4,
                     to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'))",
        )
        .bind(id)
        .bind(&task.project_id)
        .bind(&task.id)
        .bind(status)
        .execute(db.pool())
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
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status, ended_at)
         VALUES ($1, $2, $3, 'manual', 'completed', to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'))",
    )
    .bind(finalized_run_id)
    .bind(&task.project_id)
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();

    let live_run_id = "run-live-backstop";
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status)
         VALUES ($1, $2, $3, 'manual', 'running')",
    )
    .bind(live_run_id)
    .bind(&task.project_id)
    .bind(&task.id)
    .execute(db.pool())
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
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status)
         VALUES ($1, $2, $3, 'manual', 'running')",
    )
    .bind(interrupted_run_id)
    .bind(&task.project_id)
    .bind(&task.id)
    .execute(db.pool())
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
    let mut app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
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
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status, ended_at)
         VALUES ($1, $2, $3, 'manual', 'completed', to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'))",
    )
    .bind(periodic_run_id)
    .bind(&task.project_id)
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();
    let runtime =
        RecordingRuntimeOps::new(false).with_taskrun_jobs(vec![taskrun_job_ref(periodic_run_id)]);
    let mut app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
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
    sqlx::query(
        "INSERT INTO task_runs (id, project_id, task_id, trigger_type, status, ended_at)
         VALUES ($1, $2, $3, 'manual', 'completed', to_char(now() AT TIME ZONE 'utc', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'))",
    )
    .bind(startup_run_id)
    .bind(&task.project_id)
    .bind(&task.id)
    .execute(db.pool())
    .await
    .unwrap();
    let runtime =
        RecordingRuntimeOps::new(false).with_taskrun_jobs(vec![taskrun_job_ref(startup_run_id)]);
    let mut app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
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
    let mut app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(Arc::new(runtime.clone()));

    health::reap_orphaned_taskrun_jobs(&db, &app_state, "test").await;

    assert_eq!(
        runtime.calls(),
        vec!["missing-one".to_string(), "missing-two".to_string()],
        "teardown failures are best-effort and must not stop the sweep"
    );
}
