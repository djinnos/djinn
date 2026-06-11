use super::*;

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
    actor.health.record_stall(None, &bad);
    assert!(!actor.health.is_available(None, &bad));
    assert!(actor.health.is_available(None, &good));

    // Record which model the dispatch closure is actually invoked with.
    let attempted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let attempted_cl = attempted.clone();
    let outcome = actor
        .try_dispatch_to_pool("failover-test", None, &model_ids, |_pool, model_id| {
            let attempted = attempted_cl.clone();
            let model_id = model_id.to_owned();
            async move {
                attempted.lock().unwrap().push(model_id);
                Ok::<(), PoolError>(())
            }
        })
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
    actor.health.record_stall(None, &bad);
    assert!(!actor.health.is_available(None, &bad));

    // Simulate cooldown expiry, then a successful run resets the breaker.
    actor.health.enable(None, &bad);
    actor.health.record_success(None, &bad);
    assert!(actor.health.is_available(None, &bad));

    let model_ids = vec![bad.clone()];
    let outcome = actor
        .try_dispatch_to_pool(
            "recover-test",
            None,
            &model_ids,
            |_pool, _model_id| async move { Ok::<(), PoolError>(()) },
        )
        .await;
    assert!(matches!(outcome, DispatchOutcome::Dispatched));
}

// ── Zombie-session DB-truth backstop ─────────────────────────────────────

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

    let session_repo = SessionRepository::new(db.clone(), crate::events::event_bus_for(&tx));
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &task.project_id,
            task_id: Some(&task.id),
            model: "openai/gpt-5.5",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
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

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.reap_zombie_sessions().await;

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
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.rpc_registry = Some(registry.clone());

    actor.reap_zombie_sessions().await;

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
