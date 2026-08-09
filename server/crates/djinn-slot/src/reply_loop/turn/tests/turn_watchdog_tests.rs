use std::sync::atomic::Ordering;
use std::time::Duration;

use super::*;

#[tokio::test]
async fn watchdog_uses_paused_time_for_twenty_second_commits_and_forty_second_abort() {
    use djinn_provider::{ProviderAttemptAbortResultV1, ProviderOutcomeV1};

    let db = Database::ephemeral().await.expect("db");
    seed_model_turn_admission_fixture(&db, "enforce", "supported", 2).await;
    let hooks = Arc::new(ModelTurnAdmissionTestHooks::default());
    let coordinator = ModelTurnAdmissionCoordinator::with_test_hooks(
        djinn_db::ModelTurnAdmissionRepository::new(db.clone()),
        hooks.clone(),
    );
    let preparation = coordinator
        .prepare(
            &covered_admission_plan(),
            admission_request("watchdog-paused-time"),
        )
        .await
        .expect("prepare");
    let prepared_lease = match &preparation {
        ModelTurnPreparation::Permit(permit) => permit.lease.clone().expect("lease"),
        other => panic!("expected permit, got {other:?}"),
    };
    // Pool acquisition during fixture setup requires real Tokio time; pause
    // only for the deterministic watchdog chronology below.
    tokio::time::pause();
    let abort = ProviderAttemptAbortHandleV1::new();
    let observed_abort = abort.clone();
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
    let provider_polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stream_polls = provider_polls.clone();
    let provider_polled = Arc::new(tokio::sync::Notify::new());
    let poll_notice = provider_polled.clone();
    let pending_stream = futures::stream::poll_fn(move |_| {
        stream_polls.fetch_add(1, Ordering::AcqRel);
        poll_notice.notify_waiters();
        std::task::Poll::Pending
    });
    let watchdog_started = hooks.watchdog_started.notified();
    tokio::pin!(watchdog_started);
    let mut guard = launch_prepared_covered_attempt_with_lease(
        preparation,
        move || {
            Ok((
                djinn_provider::provider::client::ProviderSseAttemptV1::for_test(
                    Box::pin(pending_stream),
                    abort,
                    outcome_rx,
                ),
                Box::new(MatrixParser),
            ))
        },
        coordinator,
        tokio_util::sync::CancellationToken::new(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("launch");
    watchdog_started.await;
    let started = tokio::time::Instant::now();
    let watchdog_aborted = guard.watchdog_abort_signal();

    let cancel = tokio_util::sync::CancellationToken::new();
    let slot_ctx =
        crate::test_helpers::agent_context_from_db(db, tokio_util::sync::CancellationToken::new());
    let metadata = super::super::super::tool_dispatch::tool_runtime_metadata(&[]);
    let phase = Arc::new(Mutex::new(
        super::super::super::phase::SessionPhaseTracker::new(&slot_ctx, "worker"),
    ));
    let dispatch = super::super::super::tool_dispatch::ToolDispatchContext {
        ctx: &slot_ctx,
        task_id: "task",
        worktree_path: std::path::Path::new("/var/tmp"),
        role_name: "worker",
        tool_metadata: &metadata,
        tool_dispatcher: slot_ctx.tool_dispatcher.as_deref().expect("dispatcher"),
        otel_session: None,
        phase_tracker: None,
        cancel: &cancel,
        turn_inline_budget: None,
    };
    let activity = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let rpc = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let flush = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (mut current, mut input, mut output, mut read, mut write, mut reasoning) =
        (0, 0, 0, 0, 0, 0);
    let mut consumer = Box::pin(consume_provider_stream(StreamLoopContext {
        stream: None,
        covered_attempt: Some(&mut guard),
        tool_metadata: &metadata,
        dispatch: &dispatch,
        phase_tracker: &phase,
        task_id: "task",
        session_id: "session",
        role_name: "worker",
        project_path: "/var/tmp",
        worktree_path: std::path::Path::new("/var/tmp"),
        context_window: 100_000,
        ctx: &slot_ctx,
        cancel: &cancel,
        global_cancel: &cancel,
        activity_ts: &activity,
        last_rpc_touch: &rpc,
        last_token_flush: &flush,
        compaction_attempts: 0,
        current_context_tokens: &mut current,
        total_tokens_in: &mut input,
        total_tokens_out: &mut output,
        total_cache_read: &mut read,
        total_cache_write: &mut write,
        total_reasoning_out: &mut reasoning,
    }));
    let provider_poll = provider_polled.notified();
    tokio::pin!(provider_poll);
    tokio::select! {
        _ = &mut consumer => panic!("pending provider returned"),
        _ = &mut provider_poll => {}
    }

    tokio::time::advance(Duration::from_secs(19)).await;
    assert_eq!(hooks.heartbeats.load(Ordering::Acquire), 0);
    assert!(!observed_abort.is_aborted());

    let committed = hooks.heartbeat_committed.notified();
    tokio::pin!(committed);
    tokio::time::advance(Duration::from_secs(1)).await;
    committed.await;
    assert_eq!(hooks.heartbeats.load(Ordering::Acquire), 1);
    assert_eq!(
        hooks.heartbeat_identities.lock().expect("hooks").as_slice(),
        std::slice::from_ref(&prepared_lease),
        "the t=20 watchdog commit must use the prepared identity"
    );
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(20)
    );

    hooks.fail_heartbeat.store(true, Ordering::Release);
    let failed = hooks.heartbeat_finished.notified();
    tokio::pin!(failed);
    tokio::time::advance(Duration::from_secs(20)).await;
    failed.await;
    tokio::time::advance(Duration::from_secs(19)).await;
    assert!(!observed_abort.is_aborted(), "must not abort before t=60");

    let aborted = watchdog_aborted.cancelled();
    tokio::pin!(aborted);
    tokio::time::advance(Duration::from_secs(1)).await;
    aborted.await;
    assert!(observed_abort.is_aborted());
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(60)
    );

    let state = consumer.await.expect("typed watchdog result");
    assert!(state.watchdog_aborted);
    assert!(!state.needs_reactive_compaction && !state.provider_done && !state.early_stream_end);
    assert_eq!(provider_polls.load(Ordering::Acquire), 1);

    outcome_tx
        .send(ProviderOutcomeV1 {
            terminal: djinn_provider::ProviderAttemptTerminalV1::Aborted,
            authoritative_usage: None,
            observation: None,
            abort: ProviderAttemptAbortResultV1::Confirmed,
            token_emission: Default::default(),
        })
        .expect("outcome");
    tokio::time::resume();
    guard.finish(false).await;
}

#[derive(Clone, Copy)]
enum DeadlineRace {
    Provider,
    Session,
    Supervisor,
}

#[tokio::test]
async fn watchdog_deadline_races_use_production_stream_cancellation_owners() {
    for case in [
        DeadlineRace::Provider,
        DeadlineRace::Session,
        DeadlineRace::Supervisor,
    ] {
        run_deadline_race(case).await;
    }
}

async fn run_deadline_race(case: DeadlineRace) {
    use djinn_provider::provider::client::SseFrame;
    use djinn_provider::{ProviderAttemptAbortResultV1, ProviderOutcomeV1};
    use tokio_util::sync::CancellationToken;
    let db = Database::ephemeral().await.expect("db");
    seed_model_turn_admission_fixture(&db, "enforce", "supported", 2).await;
    let hooks = Arc::new(ModelTurnAdmissionTestHooks::default());
    hooks.block_watchdog_deadline.store(true, Ordering::Release);
    let coordinator = ModelTurnAdmissionCoordinator::with_test_hooks(
        djinn_db::ModelTurnAdmissionRepository::new(db.clone()),
        hooks.clone(),
    );
    let preparation = coordinator
        .prepare(
            &covered_admission_plan(),
            admission_request("deadline-race"),
        )
        .await
        .expect("prepare");
    let lease = match &preparation {
        ModelTurnPreparation::Permit(p) => p.lease.clone().expect("lease"),
        other => panic!("{other:?}"),
    };
    // Keep durable fixture work on real time; only the watchdog race needs a
    // paused clock.
    tokio::time::pause();
    let (frame_tx, frame_rx) = futures::channel::mpsc::unbounded();
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
    let mut outcome_tx = Some(outcome_tx);
    let started = hooks.watchdog_started.notified();
    tokio::pin!(started);
    let mut guard = launch_prepared_covered_attempt_with_lease(
        preparation,
        move || {
            Ok((
                djinn_provider::provider::client::ProviderSseAttemptV1::for_test(
                    Box::pin(frame_rx),
                    ProviderAttemptAbortHandleV1::new(),
                    outcome_rx,
                ),
                Box::new(MatrixParser),
            ))
        },
        coordinator,
        CancellationToken::new(),
        CancellationToken::new(),
    )
    .await
    .expect("launch");
    started.await;
    let cancel = CancellationToken::new();
    let global = CancellationToken::new();
    let slot = crate::test_helpers::agent_context_from_db(db.clone(), CancellationToken::new());
    let metadata = super::super::super::tool_dispatch::tool_runtime_metadata(&[]);
    let phase = Arc::new(Mutex::new(
        super::super::super::phase::SessionPhaseTracker::new(&slot, "worker"),
    ));
    let dispatch = super::super::super::tool_dispatch::ToolDispatchContext {
        ctx: &slot,
        task_id: "task",
        worktree_path: std::path::Path::new("/var/tmp"),
        role_name: "worker",
        tool_metadata: &metadata,
        tool_dispatcher: slot.tool_dispatcher.as_deref().expect("dispatcher"),
        otel_session: None,
        phase_tracker: None,
        cancel: &cancel,
        turn_inline_budget: None,
    };
    let activity = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let rpc = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let flush = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (mut current, mut input, mut output, mut read, mut write, mut reasoning) =
        (0, 0, 0, 0, 0, 0);
    let consumer = Box::pin(consume_provider_stream(StreamLoopContext {
        stream: None,
        covered_attempt: Some(&mut guard),
        tool_metadata: &metadata,
        dispatch: &dispatch,
        phase_tracker: &phase,
        task_id: "task",
        session_id: "session",
        role_name: "worker",
        project_path: "/var/tmp",
        worktree_path: std::path::Path::new("/var/tmp"),
        context_window: 100_000,
        ctx: &slot,
        cancel: &cancel,
        global_cancel: &global,
        activity_ts: &activity,
        last_rpc_touch: &rpc,
        last_token_flush: &flush,
        compaction_attempts: 0,
        current_context_tokens: &mut current,
        total_tokens_in: &mut input,
        total_tokens_out: &mut output,
        total_cache_read: &mut read,
        total_cache_write: &mut write,
        total_reasoning_out: &mut reasoning,
    }));
    let committed = hooks.heartbeat_committed.notified();
    tokio::pin!(committed);
    tokio::time::advance(Duration::from_secs(20)).await;
    committed.await;
    hooks.fail_heartbeat.store(true, Ordering::Release);
    let failed = hooks.heartbeat_finished.notified();
    tokio::pin!(failed);
    tokio::time::advance(Duration::from_secs(20)).await;
    failed.await;
    let deadline = hooks.watchdog_deadline_reached.notified();
    tokio::pin!(deadline);
    tokio::time::advance(Duration::from_secs(20)).await;
    deadline.await;
    match case {
        DeadlineRace::Provider => {
            frame_tx.unbounded_send(Ok(SseFrame::Done)).expect("done");
            outcome_tx
                .take()
                .expect("outcome sender")
                .send(ProviderOutcomeV1 {
                    terminal: djinn_provider::ProviderAttemptTerminalV1::Completed,
                    authoritative_usage: None,
                    observation: None,
                    abort: ProviderAttemptAbortResultV1::NotRequested,
                    token_emission: Default::default(),
                })
                .expect("outcome");
        }
        DeadlineRace::Session => cancel.cancel(),
        DeadlineRace::Supervisor => global.cancel(),
    }
    hooks.watchdog_deadline_release.notify_waiters();
    let state = consumer.await.expect("consumer");
    let completed = matches!(case, DeadlineRace::Provider);
    assert_eq!(state.provider_done, completed);
    assert_eq!(
        state.interrupted,
        match case {
            DeadlineRace::Provider => None,
            DeadlineRace::Session => Some(ReplyLoopCancelled::session()),
            DeadlineRace::Supervisor => Some(ReplyLoopCancelled::supervisor_shutdown()),
        }
    );
    // Repository settlement is outside the deterministic deadline chronology.
    tokio::time::resume();
    if !completed {
        outcome_tx
            .take()
            .expect("outcome sender")
            .send(ProviderOutcomeV1 {
                terminal: djinn_provider::ProviderAttemptTerminalV1::Aborted,
                authoritative_usage: None,
                observation: None,
                abort: ProviderAttemptAbortResultV1::Confirmed,
                token_emission: Default::default(),
            })
            .expect("outcome");
    }
    guard.finish(completed).await;
    assert_eq!(
        hooks.reconciliations.lock().expect("hooks").as_slice(),
        std::slice::from_ref(&lease)
    );
    assert_eq!(
        model_turn_terminal_fixture(&db, &lease.lease_id, lease.generation, &lease.request_id)
            .await
            .0,
        if completed { "completed" } else { "cancelled" }
    );
}

#[tokio::test]
async fn stale_watchdog_heartbeat_leaves_replacement_generation_unchanged() {
    use djinn_provider::{ProviderAttemptAbortResultV1, ProviderOutcomeV1};

    let db = Database::ephemeral().await.expect("db");
    let pool = seed_model_turn_admission_fixture(&db, "enforce", "supported", 2).await;
    let hooks = Arc::new(ModelTurnAdmissionTestHooks::default());
    hooks.block_heartbeat.store(true, Ordering::Release);
    let coordinator = ModelTurnAdmissionCoordinator::with_test_hooks(
        djinn_db::ModelTurnAdmissionRepository::new(db.clone()),
        hooks.clone(),
    );
    let mut permit = match coordinator
        .prepare(
            &covered_admission_plan(),
            ModelTurnAdmissionRequest {
                generation: 2,
                ..admission_request("watchdog-live-replacement")
            },
        )
        .await
        .expect("prepare")
    {
        ModelTurnPreparation::Permit(permit) => permit,
        other => panic!("expected permit, got {other:?}"),
    };
    let live = permit.lease.clone().expect("live replacement lease");
    permit.mark_active().await.expect("active");
    let stale = ModelTurnLeaseIdentity {
        generation: live.generation - 1,
        ..live.clone()
    };
    // The replacement fixture is fully persisted before the watchdog-only
    // timeline switches to paused time.
    tokio::time::pause();
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
    let guard = CoveredAttemptTerminalGuard::new(
        djinn_provider::provider::client::ProviderSseAttemptV1::for_test(
            Box::pin(futures::stream::pending()),
            ProviderAttemptAbortHandleV1::new(),
            outcome_rx,
        ),
        Box::new(MatrixParser),
        coordinator,
        Some(stale.clone()),
    );
    let started = hooks.watchdog_started.notified();
    tokio::pin!(started);
    guard.start_watchdog();
    started.await;
    let reached = hooks.heartbeat_reached.notified();
    tokio::pin!(reached);
    tokio::time::advance(Duration::from_secs(20)).await;
    reached.await;
    let replacement_before =
        djinn_db::test_support::model_turn_lease_heartbeat_snapshot_fixture(&db, &live.lease_id)
            .await;
    let finished = hooks.heartbeat_finished.notified();
    tokio::pin!(finished);
    hooks.heartbeat_release.notify_waiters();
    finished.await;
    assert_eq!(
        hooks.heartbeat_identities.lock().expect("hooks").as_slice(),
        std::slice::from_ref(&stale)
    );
    assert_eq!(
        djinn_db::test_support::model_turn_lease_heartbeat_snapshot_fixture(&db, &live.lease_id)
            .await,
        replacement_before,
        "stale watchdog heartbeat must leave replacement generation and heartbeat_at unchanged"
    );
    assert_eq!(
        model_turn_lease_lifecycle_fixture(&db, &live.lease_id).await,
        "active"
    );
    assert_eq!(model_turn_accounting_fixture(&db, pool).await, (1, 1, 0));
    tokio::time::resume();
    outcome_tx
        .send(ProviderOutcomeV1 {
            terminal: djinn_provider::ProviderAttemptTerminalV1::Aborted,
            authoritative_usage: None,
            observation: None,
            abort: ProviderAttemptAbortResultV1::Confirmed,
            token_emission: Default::default(),
        })
        .expect("outcome");
    guard.finish(false).await;
}
