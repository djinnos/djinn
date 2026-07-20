//! Regression coverage for the pause-is-not-a-fault contract: administrative
//! pause windows must not poison dispatch fault accounting, must not trip model
//! health/breaker state, and must not alter running session/chat/PR-poller
//! lifecycle.
//!
//! Companion to the existing scope coverage in `dispatch_flow.rs` (which locks
//! the global/project/user scope semantics). This file locks the negative-space
//! invariants the proposal calls out — pause is administrative deferral, not a
//! model/provider/infrastructure failure.

use super::*;
use djinn_db::{CreateSessionParams, DispatchStateRepository, DispatchStateUpsert};

// ── Local test helpers (kept in sync with `dispatch_flow.rs`) ────────────────

fn local_test_dispatch_pause(reason: &str) -> djinn_core::models::DispatchPause {
    djinn_core::models::DispatchPause {
        paused_by: "coordinator-test".to_owned(),
        paused_at: rfc3339(::time::OffsetDateTime::now_utc()),
        reason: reason.to_owned(),
        expires_at: None,
    }
}

async fn local_pause_dispatch(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    target: djinn_db::DispatchPauseTarget,
    reason: &str,
) {
    djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .pause(target, local_test_dispatch_pause(reason))
        .await
        .unwrap();
}

async fn local_assert_task_status(
    db: &Database,
    tx: &broadcast::Sender<DjinnEventEnvelope>,
    task: &djinn_core::models::Task,
    expected: &str,
) {
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(updated.status, expected, "task {} status", task.short_id);
}

// ── AC #1: pause does not poison dispatch fault accounting or model health ───

/// Pause-skip must NOT increment `dispatch_failure_streak`, add to
/// `dispatch_cooldowns`, mutate `last_dispatched` as a failure signal, or trip
/// the model health breaker — even when the task is seeded with a prior
/// `last_dispatched` marker and a non-zero `dispatch_failure_streak` that would
/// normally route to the same-role-failure ladder. Pause is a non-failure
/// admission deferral; it must skip cleanly with no fault accounting.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_task_dispatch_does_not_mutate_dispatch_fault_accounting_or_health() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "paused fault-accounting task").await;
    let task = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "open")
        .await
        .unwrap();

    // Seed prior failure context on the in-memory coordinator maps so a
    // hypothetical failure would advance them. The pre-seed proves pause-skip
    // does not TOUCH them either way (no spurious increment, no spurious
    // clear, no spurious insert).
    let mut actor = coordinator_actor_for_tests(&db, &tx);
    let seeded_last_dispatched = StdInstant::now();
    actor.last_dispatched.insert(
        task.id.clone(),
        DispatchMarker {
            instant: seeded_last_dispatched,
            role: "reviewer".to_owned(),
        },
    );
    actor.dispatch_failure_streak.insert(task.id.clone(), 2);
    actor.dispatch_cooldowns.insert(
        task.id.clone(),
        StdInstant::now() + std::time::Duration::from_secs(300),
    );
    // Seed a healthy breaker state for the model the dispatcher would use; a
    // pause-skip must NOT change breaker availability.
    let model_id = DEFAULT_MODEL_ID;
    assert!(actor.health.is_available(None, model_id));
    let breaker_available_before = actor.health.is_available(None, model_id);

    // Seed a matching global pause so the task's dispatch will be deferred.
    local_pause_dispatch(
        &db,
        &tx,
        djinn_db::DispatchPauseTarget::global(),
        "fault-accounting regression",
    )
    .await;

    actor.dispatch_ready_tasks(None).await;

    // Dispatch was suppressed because of the pause.
    assert_eq!(
        actor.dispatched, 0,
        "global pause must suppress dispatch of the ready task"
    );
    let has_session_after = actor.pool.has_session(&task.id).await.unwrap_or(false);
    assert!(
        !has_session_after,
        "pause-skip must not spawn a slot-pool session"
    );

    // The fault accounting was not mutated as a failure signal:
    //   - `last_dispatched` was NOT removed (no failure attribution), and was
    //     NOT advanced to a new instant (no successful re-dispatch either —
    //     pause is a deferral, not a re-dispatch).
    let marker = actor
        .last_dispatched
        .get(&task.id)
        .expect("pause-skip must not remove pre-seeded last_dispatched marker");
    assert_eq!(
        marker.role, "reviewer",
        "pause-skip must not change the pre-seeded role of last_dispatched"
    );
    assert_eq!(
        marker.instant, seeded_last_dispatched,
        "pause-skip must not advance last_dispatched as a dispatch/failure signal"
    );
    assert_eq!(
        actor.dispatch_failure_streak.get(&task.id).copied(),
        Some(2),
        "pause-skip must not increment or clear dispatch_failure_streak"
    );
    assert!(
        actor.dispatch_cooldowns.contains_key(&task.id),
        "pause-skip must not consume or remove an existing dispatch_cooldown"
    );

    // The breaker state for this model/scope was not tripped.
    assert_eq!(
        actor.health.is_available(None, model_id),
        breaker_available_before,
        "pause-skip must not trip or alter the model health breaker"
    );

    // The task itself was not transitioned to failed/closed/intervention.
    local_assert_task_status(&db, &tx, &task, "open").await;
}

/// A persistent durable dispatch_state row for the paused task must NOT be
/// mutated by the pause-skip path. Without this guard, an admin pause would
/// silently clear the `dispatch_state` cooldown that the next non-paused pass
/// relies on for escalating backoff, and the task would be re-dispatched
/// without its prior backoff context.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_task_dispatch_does_not_mutate_durable_dispatch_state_row() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "durable state preservation task").await;
    let task = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "open")
        .await
        .unwrap();

    // Seed a real durable dispatch_state row with non-zero backoff context
    // that the next non-paused pass would need to rehydrate and consult.
    let wall_now = ::time::OffsetDateTime::now_utc();
    let persisted_deadline = wall_now + ::time::Duration::seconds(120);
    let persisted_deadline_str = rfc3339(persisted_deadline);
    let last_dispatched_str = rfc3339(wall_now - ::time::Duration::seconds(10));
    let state_repo = DispatchStateRepository::new(db.clone());
    state_repo
        .upsert(DispatchStateUpsert {
            task_id: &task.id,
            failure_streak: 4,
            cooldown_until: Some(&persisted_deadline_str),
            escalation_count: 1,
            last_dispatched_at: Some(&last_dispatched_str),
            last_dispatched_role: Some("worker"),
            inflight_creator_user_id: Some("creator-1"),
            inflight_model_id: Some(DEFAULT_MODEL_ID),
        })
        .await
        .unwrap();

    // PostgreSQL normalizes RFC3339 values to the column precision on write
    // (for example, nanoseconds may round/truncate to milliseconds depending
    // on the schema). Capture the row as persisted immediately after seeding;
    // the regression we care about is that the pause-skip pass leaves this
    // durable row byte-for-byte equivalent to the stored baseline.
    let before = state_repo
        .get(&task.id)
        .await
        .unwrap()
        .expect("seeded dispatch_state row must exist before the pause-skip");

    // Set a global pause so the task's dispatch will be deferred.
    local_pause_dispatch(
        &db,
        &tx,
        djinn_db::DispatchPauseTarget::global(),
        "durable preservation",
    )
    .await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.dispatch_ready_tasks(None).await;

    // The dispatch pass must not have written back to the durable state row
    // while the task was paused. A pause-skip is a no-op for the row — the
    // failure_streak / cooldown_until / last_dispatched_* / inflight_* all
    // survive untouched.
    let after = state_repo
        .get(&task.id)
        .await
        .unwrap()
        .expect("dispatch_state row must still exist after a pause-skip");
    assert_eq!(
        after.failure_streak, 4,
        "pause-skip must not clear failure_streak"
    );
    assert_eq!(
        after.cooldown_until.as_deref(),
        before.cooldown_until.as_deref(),
        "pause-skip must not clear cooldown_until"
    );
    assert_eq!(
        after.escalation_count, 1,
        "pause-skip must not change escalation_count"
    );
    assert_eq!(
        after.last_dispatched_role.as_deref(),
        Some("worker"),
        "pause-skip must not clear last_dispatched_role"
    );
    assert_eq!(
        after.last_dispatched_at.as_deref(),
        before.last_dispatched_at.as_deref(),
        "pause-skip must not clear or rewrite last_dispatched_at"
    );
    assert_eq!(
        after.inflight_creator_user_id.as_deref(),
        Some("creator-1"),
        "pause-skip must not clear inflight_creator_user_id"
    );
    assert_eq!(
        after.inflight_model_id.as_deref(),
        Some(DEFAULT_MODEL_ID),
        "pause-skip must not clear inflight_model_id"
    );
    local_assert_task_status(&db, &tx, &task, "open").await;
}

// ── AC #2: matching pause does not transition task to failed/closed/intervention ─

/// Project pause + a task in a terminal/transition-vulnerable status must
/// remain in its existing dispatchable status (`needs_task_review` is the
/// `list_by_status_filtered` review path). Pause must not trigger a
/// terminal close, an intervention transition, or any other status mutation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn project_pause_keeps_review_status_task_dispatchable_for_resume() {
    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "review task in paused project").await;
    let task = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "needs_task_review")
        .await
        .unwrap();

    local_pause_dispatch(
        &db,
        &tx,
        djinn_db::DispatchPauseTarget::project(task.project_id.clone()),
        "project freeze during review",
    )
    .await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.dispatch_ready_tasks(None).await;

    assert_eq!(actor.dispatched, 0, "project pause must suppress dispatch");
    // The task must remain in its pre-pause review status, ready to dispatch
    // the moment the pause is lifted. The reviewer needs a stable handoff
    // surface; pause must not move the task to closed/failed/intervention.
    local_assert_task_status(&db, &tx, &task, "needs_task_review").await;

    // Sanity: after the pause is lifted, the same task dispatches without
    // needing any extra status fixup.
    djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .resume(djinn_db::DispatchPauseTarget::project(
            task.project_id.clone(),
        ))
        .await
        .unwrap();
    actor.dispatch_ready_tasks(None).await;
    assert_eq!(
        actor.dispatched, 1,
        "review task should resume dispatch after the project pause is lifted"
    );
    assert!(actor.last_dispatched.contains_key(&task.id));
    local_assert_task_status(&db, &tx, &task, "needs_task_review").await;
}

// ── AC #3: active session lifecycle is untouched by dispatch pause enforcement ─

/// An active/running session MUST NOT be killed, reaped, or drained while a
/// dispatch pause is active on the matching scope. The coordinator
/// `reap_zombie_sessions` backstop is responsible for genuine infra drift
/// (pod never scheduled, OOM, leaked slot, hung tool past the fast path) — but
/// an administrative pause is not infra drift and must not feed the same
/// reap path. Without this guard, pausing dispatch would silently drop every
/// in-flight worker session and waste their work.
///
/// The fixture MUST model a real active coordinator session — i.e. the slot
/// pool holds a `task_to_slot` mapping for the task, in addition to the DB
/// `running` session row. `detect_and_recover_stuck_filtered` (the orphan
/// reconciler) gates on `pool.has_session(&task.id)`; with only a DB row and
/// no slot mapping it would treat the task as orphaned and transition it
/// back to `open` via `interrupt_running_for_task`, which would invalidate
/// this test. Seeding a real slot-pool session makes the test reflect the
/// proposal's "active session" contract: a worker that holds both the
/// in-memory slot AND the DB `running` row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_pause_does_not_reap_or_kill_active_worker_sessions() {
    use djinn_db::SessionRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, project_path) =
        create_simple_task(&db, &tx, "task", "active session during pause").await;

    // Put the task in the dispatchable execution state and create a real
    // `running` session row for it — i.e. an in-flight worker that has not
    // yet reported a token/turn.
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "in_progress")
        .await
        .unwrap();
    let run_id = "pause-active-run";
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
            model: "test/mock",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();

    // Pre-condition: the DB session row is active.
    assert!(
        session_repo
            .list_active()
            .await
            .unwrap()
            .iter()
            .any(|s| s.id == session.id),
        "test fixture: DB session row must be active before pause enforcement"
    );

    // Build a slot pool whose slots stay busy for the lifetime of the test
    // (the test runner parks on the kill signal, so the slot never returns
    // `Ok` and the pool keeps the `task_to_slot` mapping). This is the
    // "active" half of the fixture — without it the orphan reconciler below
    // would treat the in_progress task as leaked and finalize the session.
    let runtime = RecordingRuntimeOps::new(true);
    let mut app_state = test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    app_state.runtime_ops = Some(std::sync::Arc::new(runtime.clone()));
    let cancel = CancellationToken::new();
    let pool = SlotPoolHandle::spawn_with_factory(
        app_state,
        cancel.clone(),
        SlotPoolConfig {
            models: vec![ModelSlotConfig {
                model_id: DEFAULT_MODEL_ID.to_owned(),
                max_slots: 1,
                roles: ["worker", "reviewer"]
                    .into_iter()
                    .map(ToOwned::to_owned)
                    .collect(),
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
                        // Block until the slot's kill signal fires — the slot
                        // stays busy and the pool's `task_to_slot` mapping
                        // survives. This is the "active session" state the
                        // proposal requires the coordinator to leave alone.
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
    pool.dispatch(&task.id, &project_path, DEFAULT_MODEL_ID)
        .await
        .expect("dispatch should claim a slot for the active worker fixture");
    assert!(
        pool.has_session(&task.id).await.expect("has_session"),
        "test fixture: slot pool must report an active session for the task"
    );

    // Set a global pause and run every active-session lifecycle method the
    // coordinator ticks. None of them should call into kill / reap / drain
    // paths that touch the running session.
    local_pause_dispatch(
        &db,
        &tx,
        djinn_db::DispatchPauseTarget::global(),
        "no-active-session-touch regression",
    )
    .await;

    let mut actor = coordinator_actor_for_tests(&db, &tx);
    actor.pool = pool;
    actor.runtime_ops = Some(std::sync::Arc::new(runtime.clone()));

    // All four lifecycle methods run under the same actor reference; assert
    // none of them call teardown / kill / drain on the running session.
    actor.reap_zombie_sessions().await;
    actor.enforce_session_stall_timeout().await;
    actor.reap_idle_chat_sessions().await;
    actor.detect_and_recover_stuck_filtered(None).await;

    // Runtime teardown was never invoked — the active session still belongs
    // to its original task, untouched.
    assert!(
        runtime.calls().is_empty(),
        "pause must not trigger task-run Job teardown for an active session with a task_run_id"
    );

    // The DB session row is still `running` and listed by `list_active()`.
    let active = session_repo.list_active().await.unwrap();
    assert!(
        active.iter().any(|s| s.id == session.id),
        "pause enforcement must not remove or finalize the active DB session"
    );

    // The slot pool still holds the task→slot mapping; nothing in the
    // lifecycle methods evicted or replaced it.
    assert!(
        actor.pool.has_session(&task.id).await.expect("has_session"),
        "pause enforcement must not evict the slot pool's task→slot mapping"
    );

    // The task was not released for redispatch — it stays in_progress, just
    // like it would without the pause, because `detect_and_recover_stuck` is
    // keyed on `has_session` and the slot pool still holds it.
    let updated = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .get(&task.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        updated.status, "in_progress",
        "pause must not release the task from its execution state"
    );

    cancel.cancel();
}

// ── AC #4: chat and PR poller paths remain unaffected by dispatch pause ──────

/// Contract test: no `dispatch_pause`/`DispatchPauseTarget` symbol reaches the
/// PR poller or the chat dispatch entry point. A future regression that wires
/// pause into the PR poller or chat would silently break proposal 6ngx's
/// scope ("chat and PR poller are not gated by pause state"). The check is
/// scoped to the coordinator module and the chat entry point so a new
/// consumer can opt in explicitly with intent.
#[test]
fn chat_and_pr_poller_have_no_dispatch_pause_dependency() {
    // Source-level contract: the coordinator PR poller module must not
    // import or use the dispatch-pause gate. The two PR poller files
    // (pr_watcher.rs, pr_review_watcher.rs) are checked here at compile
    // time of the rest of the crate, but the negative test is "no symbol
    // import" — and that has to be asserted at the SOURCE level, because a
    // positive `use` could be wired in later. The presence/absence of
    // matching text in the source is the only stable contract we can lock.
    let coordinator_pr_poller_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/pr_poller");
    let chat_entry = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/chat_tools.rs");

    let mut offenders: Vec<(std::path::PathBuf, String)> = Vec::new();
    let forbidden = [
        "DispatchPauseRepository",
        "DispatchPauseTarget",
        "load_dispatch_pause_state",
        "matching_task_dispatch_pause",
    ];

    for entry in std::fs::read_dir(&coordinator_pr_poller_dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let contents = std::fs::read_to_string(&path).unwrap();
        for needle in &forbidden {
            if contents.contains(needle) {
                offenders.push((path.clone(), (*needle).to_owned()));
            }
        }
    }

    // Also scan the chat dispatch entry point if it exists in this crate
    // (djinn-coordinator may not have chat_tools.rs; the facade in djinn-agent does).
    if chat_entry.exists() {
        let chat_contents = std::fs::read_to_string(&chat_entry).unwrap();
        for needle in &forbidden {
            if chat_contents.contains(needle) {
                offenders.push((chat_entry.clone(), (*needle).to_owned()));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "dispatch-pause symbols must not leak into the PR poller or chat entry point; offenders: {offenders:?}"
    );
}

/// Behavioral contract: a global dispatch pause set in the durable repository
/// must NOT change how the PR poller lists its candidate tasks. The PR
/// poller pulls `pr_draft`/`pr_review` candidates directly from the task
/// table and queries GitHub for each — pause enforcement, which gates NEW
/// dispatch, must be invisible to that observe/merge loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pr_poller_lists_pr_draft_candidates_independent_of_dispatch_pause() {
    use djinn_db::TaskRepository;

    let db = test_helpers::create_test_db();
    let (tx, _rx) = broadcast::channel(256);
    let (task, _project_path) =
        create_simple_task(&db, &tx, "task", "pr-draft task during pause").await;
    let task = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_status(&task.id, "pr_draft")
        .await
        .unwrap();
    // Stamp a dummy PR URL so the poller keeps the row (it filters to
    // `pr_url.is_some()`).
    TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .set_pr_url(&task.id, "https://github.com/owner/repo/pull/1")
        .await
        .unwrap();

    // Set a global pause.
    local_pause_dispatch(
        &db,
        &tx,
        djinn_db::DispatchPauseTarget::global(),
        "pr-poller unaffected regression",
    )
    .await;

    // The task repo (which the PR poller uses to enumerate candidates) must
    // still return the `pr_draft` row regardless of pause state. The pause
    // lives in a separate repository/table; the poller does not consult it.
    let pr_draft = TaskRepository::new(db.clone(), crate::events::event_bus_for(&tx))
        .list_by_status("pr_draft")
        .await
        .unwrap();
    assert!(
        pr_draft.iter().any(|t| t.id == task.id),
        "PR poller candidate list must NOT be filtered by dispatch pause state"
    );

    // And the pause state is unchanged — PR poller observation never
    // touches the pause repository.
    let state_after =
        djinn_db::DispatchPauseRepository::new(db.clone(), crate::events::event_bus_for(&tx))
            .get_status()
            .await
            .unwrap();
    assert!(
        state_after.global.is_some(),
        "PR-poller observation must not clear or modify the durable pause state"
    );
}
