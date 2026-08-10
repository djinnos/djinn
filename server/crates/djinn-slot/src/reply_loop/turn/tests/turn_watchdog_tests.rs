use std::sync::atomic::Ordering;
use std::time::Duration;

use super::*;

#[tokio::test]
async fn two_slots_share_target_one_and_quarantine_after_partitioned_watchdog_abort() {
    use crate::model_turn_capability::{
        ModelTurnCapabilityCoverageV2, SlotLiveIdentity, report_for_route,
    };
    use djinn_db::{ModelTurnAdmissionWait, ModelTurnAuthoritativeUsage};
    use djinn_provider::{
        ProviderAttemptAbortResultV1, ProviderAttemptTerminalV1, ProviderOutcomeV1,
    };

    let db = Database::ephemeral().await.expect("db");
    let pool = seed_model_turn_admission_fixture(&db, "enforce", "supported", 1).await;
    let hooks = Arc::new(ModelTurnAdmissionTestHooks::default());
    let slot_a = ModelTurnAdmissionCoordinator::with_test_hooks(
        djinn_db::ModelTurnAdmissionRepository::new(db.clone()),
        Arc::clone(&hooks),
    );
    let slot_b = ModelTurnAdmissionCoordinator::with_test_hooks(
        djinn_db::ModelTurnAdmissionRepository::new(db.clone()),
        Arc::clone(&hooks),
    );
    let slot_a_identity = SlotLiveIdentity {
        pod_uid: "pod-a".into(),
        deployment_revision: "rev-1".into(),
    };
    let slot_b_identity = SlotLiveIdentity {
        pod_uid: "pod-b".into(),
        deployment_revision: "rev-1".into(),
    };
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let a_barrier = Arc::clone(&barrier);
    let b_barrier = Arc::clone(&barrier);
    let a_identity = slot_a_identity.clone();
    let b_identity = slot_b_identity.clone();
    let a = async move {
        a_barrier.wait().await;
        slot_a
            .prepare(
                &covered_admission_plan(),
                ModelTurnAdmissionRequest {
                    credential_id: "credential-slot".into(),
                    request_id: "slot-a:covered:1".into(),
                    owner_pod_uid: Some(a_identity.pod_uid.clone()),
                    generation: 1,
                },
            )
            .await
            .expect("slot a prepare")
    };
    let b = async move {
        b_barrier.wait().await;
        slot_b
            .prepare(
                &covered_admission_plan(),
                ModelTurnAdmissionRequest {
                    credential_id: "credential-slot".into(),
                    request_id: "slot-b:covered:1".into(),
                    owner_pod_uid: Some(b_identity.pod_uid.clone()),
                    generation: 1,
                },
            )
            .await
            .expect("slot b prepare")
    };
    let (a, b) = tokio::join!(a, b);
    let (winner, replacement_request, winner_identity, replacement_identity) = match (a, b) {
        (ModelTurnPreparation::Permit(permit), ModelTurnPreparation::Wait(wait)) => {
            assert!(matches!(
                wait,
                ModelTurnAdmissionWait::Concurrency { target: 1, .. }
            ));
            (permit, "slot-b:covered:2", slot_a_identity, slot_b_identity)
        }
        (ModelTurnPreparation::Wait(wait), ModelTurnPreparation::Permit(permit)) => {
            assert!(matches!(
                wait,
                ModelTurnAdmissionWait::Concurrency { target: 1, .. }
            ));
            (permit, "slot-a:covered:2", slot_b_identity, slot_a_identity)
        }
        other => panic!("target-one must yield exactly one permit and typed wait: {other:?}"),
    };
    let lease = winner.lease.clone().expect("winner lease");
    assert_eq!(model_turn_decision_count_fixture(&db, pool).await, 1);

    let launches = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let launch_count = Arc::clone(&launches);
    let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let stream_polls = Arc::clone(&polls);
    let provider_polled = Arc::new(tokio::sync::Notify::new());
    let poll_notice = Arc::clone(&provider_polled);
    let abort = ProviderAttemptAbortHandleV1::new();
    let observed_abort = abort.clone();
    let (outcome_tx, outcome_rx) = tokio::sync::oneshot::channel();
    let coordinator = ModelTurnAdmissionCoordinator::with_test_hooks(
        djinn_db::ModelTurnAdmissionRepository::new(db.clone()),
        Arc::clone(&hooks),
    );
    let started = hooks.watchdog_started.notified();
    tokio::pin!(started);
    let guard = launch_prepared_covered_attempt_with_lease(
        ModelTurnPreparation::Permit(winner),
        move || {
            launch_count.fetch_add(1, Ordering::AcqRel);
            Ok((
                djinn_provider::provider::client::ProviderSseAttemptV1::for_test(
                    Box::pin(futures::stream::poll_fn(move |_| {
                        stream_polls.fetch_add(1, Ordering::AcqRel);
                        poll_notice.notify_waiters();
                        std::task::Poll::Pending
                    })),
                    abort,
                    outcome_rx,
                ),
                Box::new(MatrixParser),
            ))
        },
        coordinator.clone(),
        tokio_util::sync::CancellationToken::new(),
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    .expect("one B1 launch");
    started.await;
    assert_eq!(launches.load(Ordering::Acquire), 1);
    assert_eq!(
        model_turn_lease_lifecycle_fixture(&db, &lease.lease_id).await,
        "active",
        "the sole B1 launch follows one committed dispatch fence and active hand-off"
    );

    let cancel = tokio_util::sync::CancellationToken::new();
    let slot_ctx = crate::test_helpers::agent_context_from_db(db.clone(), tokio_util::sync::CancellationToken::new());
    let metadata = super::super::super::tool_dispatch::tool_runtime_metadata(&[]);
    let phase = Arc::new(Mutex::new(super::super::super::phase::SessionPhaseTracker::new(&slot_ctx, "worker")));
    let dispatch = super::super::super::tool_dispatch::ToolDispatchContext { ctx: &slot_ctx, task_id: "task", worktree_path: std::path::Path::new("/var/tmp"), role_name: "worker", tool_metadata: &metadata, tool_dispatcher: slot_ctx.tool_dispatcher.as_deref().expect("dispatcher"), otel_session: None, phase_tracker: None, cancel: &cancel, turn_inline_budget: None };
    let activity = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let rpc = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let flush = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let (mut current, mut input, mut output, mut read, mut write, mut reasoning) = (0, 0, 0, 0, 0, 0);
    let mut consumer = Box::pin(consume_provider_stream(StreamLoopContext { stream: None, covered_attempt: Some(&mut guard), tool_metadata: &metadata, dispatch: &dispatch, phase_tracker: &phase, task_id: "task", session_id: "session", role_name: "worker", project_path: "/var/tmp", worktree_path: std::path::Path::new("/var/tmp"), context_window: 100_000, ctx: &slot_ctx, cancel: &cancel, global_cancel: &cancel, activity_ts: &activity, last_rpc_touch: &rpc, last_token_flush: &flush, compaction_attempts: 0, current_context_tokens: &mut current, total_tokens_in: &mut input, total_tokens_out: &mut output, total_cache_read: &mut read, total_cache_write: &mut write, total_reasoning_out: &mut reasoning }));
    let provider_poll = provider_polled.notified();
    tokio::pin!(provider_poll);
    tokio::select! { _ = &mut consumer => panic!("pending provider returned"), _ = &mut provider_poll => {} }
    assert_eq!(polls.load(Ordering::Acquire), 1);

    tokio::time::pause();
    let started_at = tokio::time::Instant::now();
    hooks.block_heartbeat.store(true, Ordering::Release);
    let reached = hooks.heartbeat_reached.notified();
    tokio::pin!(reached);
    tokio::time::advance(Duration::from_secs(20)).await;
    reached.await;
    let committed = hooks.heartbeat_committed.notified();
    tokio::pin!(committed);
    tokio::time::resume();
    hooks.heartbeat_release.notify_waiters();
    committed.await;
    tokio::time::pause();
    hooks.block_heartbeat.store(false, Ordering::Release);
    hooks.fail_heartbeat.store(true, Ordering::Release);
    let failed = hooks.heartbeat_finished.notified();
    tokio::pin!(failed);
    tokio::time::advance(Duration::from_secs(20)).await;
    failed.await;
    let watchdog_signal = guard.watchdog_abort_signal();
    let watchdog_abort = watchdog_signal.cancelled();
    tokio::pin!(watchdog_abort);
    tokio::time::advance(Duration::from_secs(20)).await;
    watchdog_abort.await;
    assert_eq!(
        tokio::time::Instant::now() - started_at,
        Duration::from_secs(60)
    );
    assert!(observed_abort.is_aborted());
    let state = consumer.await.expect("typed watchdog-aborted consumer result");
    assert!(state.watchdog_aborted);
    assert!(!state.provider_done && !state.needs_reactive_compaction);
    assert_eq!(polls.load(Ordering::Acquire), 1, "watchdog performs no second provider read");
    assert_eq!(
        launches.load(Ordering::Acquire),
        1,
        "watchdog cannot launch replacement"
    );

    let denied = coordinator
        .prepare(
            &covered_admission_plan(),
            admission_request(replacement_request),
        )
        .await
        .expect("rival preparation");
    assert!(matches!(denied, ModelTurnPreparation::Wait(_)));
    tokio::time::advance(Duration::from_secs(30)).await;
    let denied_at_boundary = coordinator
        .prepare(&covered_admission_plan(), admission_request(replacement_request))
        .await
        .expect("rival preparation at lease boundary");
    assert!(matches!(denied_at_boundary, ModelTurnPreparation::Wait(_)));
    assert_eq!(
        launches.load(Ordering::Acquire),
        1,
        "no expiry/reaper replacement at 90 seconds"
    );

    hooks.block_reconcile.store(true, Ordering::Release);
    let reconciliation_reached = hooks.reconcile_reached.notified();
    tokio::pin!(reconciliation_reached);
    outcome_tx
        .send(ProviderOutcomeV1 {
            terminal: ProviderAttemptTerminalV1::Aborted,
            authoritative_usage: None,
            observation: None,
            abort: ProviderAttemptAbortResultV1::Confirmed,
            token_emission: Default::default(),
        })
        .expect("B1 outcome");
    tokio::time::resume();
    let mut settlement = Box::pin(guard.finish(false));
    tokio::select! {
        _ = &mut settlement => panic!("partitioned reconciliation unexpectedly completed"),
        _ = &mut reconciliation_reached => {}
    }
    assert_eq!(model_turn_lease_lifecycle_fixture(&db, &lease.lease_id).await, "active");
    hooks.block_reconcile.store(false, Ordering::Release);
    hooks.reconcile_release.notify_waiters();
    settlement.await;
    assert_eq!(
        model_turn_terminal_fixture(&db, &lease.lease_id, lease.generation, &lease.request_id)
            .await
            .1,
        "quarantined"
    );
    assert_eq!(model_turn_accounting_fixture(&db, pool).await, (0, 0, 1));

    coordinator
        .reconcile(
            lease.clone(),
            &ProviderOutcomeV1 {
                terminal: ProviderAttemptTerminalV1::Aborted,
                authoritative_usage: Some(ModelTurnAuthoritativeUsage {
                    request_units: 0,
                    input_units: 0,
                    output_units: 0,
                    combined_units: 0,
                }),
                observation: None,
                abort: ProviderAttemptAbortResultV1::Confirmed,
                token_emission: Default::default(),
            },
        )
        .await
        .expect("authoritative eligibility restoration");
    let fresh = coordinator
        .prepare(
            &covered_admission_plan(),
            ModelTurnAdmissionRequest {
                credential_id: "credential-slot".into(),
                request_id: replacement_request.into(),
                owner_pod_uid: Some(replacement_identity.pod_uid.clone()),
                generation: 2,
            },
        )
        .await
        .expect("fresh preparation");
    let fresh_lease = match &fresh {
        ModelTurnPreparation::Permit(permit) => permit.lease.clone().expect("fresh lease"),
        other => panic!("eligibility restoration must permit fresh lease: {other:?}"),
    };
    assert_ne!(fresh_lease.lease_id, lease.lease_id);
    assert_ne!(fresh_lease.request_id, lease.request_id);
    assert!(fresh_lease.generation > lease.generation);
    let replacement_launches = Arc::clone(&launches);
    let (fresh_outcome_tx, fresh_outcome_rx) = tokio::sync::oneshot::channel();
    let fresh_guard = launch_prepared_covered_attempt_with_lease(
        fresh,
        move || {
            replacement_launches.fetch_add(1, Ordering::AcqRel);
            Ok((djinn_provider::provider::client::ProviderSseAttemptV1::for_test(Box::pin(futures::stream::pending()), ProviderAttemptAbortHandleV1::new(), fresh_outcome_rx), Box::new(MatrixParser)))
        },
        coordinator.clone(), tokio_util::sync::CancellationToken::new(), tokio_util::sync::CancellationToken::new(),
    ).await.expect("fresh B1 launch");
    assert_eq!(launches.load(Ordering::Acquire), 2);
    assert_eq!(model_turn_lease_lifecycle_fixture(&db, &fresh_lease.lease_id).await, "active");
    fresh_outcome_tx.send(ProviderOutcomeV1 { terminal: ProviderAttemptTerminalV1::Completed, authoritative_usage: Some(ModelTurnAuthoritativeUsage { request_units: 0, input_units: 0, output_units: 0, combined_units: 0 }), observation: None, abort: ProviderAttemptAbortResultV1::NotRequested, token_emission: Default::default() }).expect("fresh outcome");
    fresh_guard.finish(true).await;

    let plan = covered_admission_plan();
    let slot_a_report = report_for_route(
        &winner_identity,
        "provider",
        "model",
        Some(&plan),
    );
    let slot_b_report = report_for_route(
        &replacement_identity,
        "provider",
        "model",
        Some(&plan),
    );
    let winner_owner = model_turn_lease_owner_pod_uid_fixture(&db, &lease.lease_id)
        .await
        .expect("winner lease must persist a pod owner");
    let replacement_owner = model_turn_lease_owner_pod_uid_fixture(&db, &fresh_lease.lease_id)
        .await
        .expect("replacement lease must persist a pod owner");
    assert_ne!(slot_a_report.slot_pod_uid, slot_b_report.slot_pod_uid);
    for (report, identity, persisted_owner) in [
        (&slot_a_report, &winner_identity, winner_owner),
        (&slot_b_report, &replacement_identity, replacement_owner),
    ] {
        assert_eq!(report.slot_pod_uid, identity.pod_uid);
        assert_eq!(report.slot_pod_uid, persisted_owner);
        assert_eq!(report.deployment_revision, identity.deployment_revision);
        assert_eq!(report.provider, plan.scope.provider_id);
        assert_eq!(report.model_scope, plan.scope.model_id);
    }
    assert_eq!(
        slot_a_report.coverage,
        ModelTurnCapabilityCoverageV2::Covered
    );
    assert_eq!(
        slot_b_report.coverage,
        ModelTurnCapabilityCoverageV2::Covered
    );
    assert_eq!(
        report_for_route(
            &winner_identity,
            "provider",
            "model",
            None
        )
        .coverage,
        ModelTurnCapabilityCoverageV2::Uncovered
    );
}

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
    // Dispatch marking and the active transition are database-backed. Start
    // the watchdog on real time, then pause only its t=20/t=60 chronology.
    tokio::time::pause();
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

    hooks.block_heartbeat.store(true, Ordering::Release);
    let reached = hooks.heartbeat_reached.notified();
    tokio::pin!(reached);
    tokio::time::advance(Duration::from_secs(1)).await;
    reached.await;
    assert_eq!(
        tokio::time::Instant::now() - started,
        Duration::from_secs(20)
    );
    // SQLx pool acquisition and the heartbeat commit must run on real Tokio
    // time. The hook keeps the production watchdog fixed at the t=20 seam.
    let committed = hooks.heartbeat_committed.notified();
    tokio::pin!(committed);
    tokio::time::resume();
    hooks.heartbeat_release.notify_waiters();
    committed.await;
    tokio::time::pause();
    hooks.block_heartbeat.store(false, Ordering::Release);
    assert_eq!(hooks.heartbeats.load(Ordering::Acquire), 1);
    assert_eq!(
        hooks.heartbeat_identities.lock().expect("hooks").as_slice(),
        std::slice::from_ref(&prepared_lease),
        "the t=20 watchdog commit must use the prepared identity"
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
    // The launch includes the database-backed active transition. Pause only
    // after that transition and watchdog startup have completed.
    tokio::time::pause();
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
    hooks.block_heartbeat.store(true, Ordering::Release);
    let reached = hooks.heartbeat_reached.notified();
    tokio::pin!(reached);
    tokio::time::advance(Duration::from_secs(20)).await;
    reached.await;
    let committed = hooks.heartbeat_committed.notified();
    tokio::pin!(committed);
    tokio::time::resume();
    hooks.heartbeat_release.notify_waiters();
    committed.await;
    tokio::time::pause();
    hooks.block_heartbeat.store(false, Ordering::Release);
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
    // Capture the replacement while Tokio time is live. The blocked stale
    // heartbeat cannot mutate it before the exact t=20 release below.
    let replacement_before =
        djinn_db::test_support::model_turn_lease_heartbeat_snapshot_fixture(&db, &live.lease_id)
            .await;
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
    let finished = hooks.heartbeat_finished.notified();
    tokio::pin!(finished);
    // Resume before the stale mutation and every subsequent database read.
    tokio::time::resume();
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
