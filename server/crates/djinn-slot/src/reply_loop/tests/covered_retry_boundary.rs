//! Production-boundary evidence for covered B1 retry ownership.

use super::*;
use crate::reply_loop::model_turn_admission::ModelTurnAdmissionTestHooks;
use crate::reply_loop::turn::{
    register_reply_loop_admission_test_hooks, register_reply_loop_boundary_observer,
};
use djinn_db::test_support::{
    model_turn_accounting_fixture, model_turn_decision_count_fixture,
    model_turn_launch_identities_fixture, model_turn_terminal_fixture,
    seed_model_turn_admission_fixture,
};
use djinn_db::{Database, ModelTurnBucketDebit, ModelTurnBucketKind};
use djinn_provider::provider::client::{ProviderAttemptContextV1, ProviderSseAttemptV1, SseFrame};
use djinn_provider::provider::{ProviderSseFrameParserV1, ToolChoice};
use djinn_provider::{
    ProviderAbortCapabilityV1, ProviderAttemptAbortHandleV1, ProviderAttemptCapabilitiesV1,
    ProviderAttemptLossV1, ProviderAttemptPlanV1, ProviderAttemptRouteCoverageV1,
    ProviderAttemptScopeV1, ProviderAttemptTerminalV1, ProviderCredentialRecordScopeV1,
    ProviderDiscoveryOwnershipV1, ProviderHiddenRetryCapabilityV1, ProviderNormalizedObservationV1,
    ProviderObservationDiagnosticsV1, ProviderOutcomeV1, ProviderOutputReservationSourceV1,
};
use std::sync::atomic::{AtomicUsize, Ordering};

async fn set_pool_phase(db: &Database, pool: i64, phase: &str) {
    sqlx::query("UPDATE model_turn_pools SET phase = $2 WHERE id = $1")
        .bind(pool)
        .bind(phase)
        .execute(db.pool())
        .await
        .expect("update pool phase");
}

async fn set_pool_capability(db: &Database, pool: i64, capability: &str) {
    sqlx::query("UPDATE model_turn_pools SET capability_state = $2 WHERE id = $1")
        .bind(pool)
        .bind(capability)
        .execute(db.pool())
        .await
        .expect("update pool capability");
}

fn covered_plan() -> ProviderAttemptPlanV1 {
    ProviderAttemptPlanV1 {
        scope: ProviderAttemptScopeV1 {
            credential: ProviderCredentialRecordScopeV1::from_credential_record_id(
                "credential-slot",
            ),
            provider_id: "provider".into(),
            model_id: "model".into(),
        },
        coverage: ProviderAttemptRouteCoverageV1::Covered {
            capabilities: ProviderAttemptCapabilitiesV1 {
                hidden_retries: ProviderHiddenRetryCapabilityV1::Disabled,
                abort: ProviderAbortCapabilityV1::Supported,
            },
            supported_bucket_bindings: vec![ModelTurnBucketKind::Request],
            policy: djinn_provider::ProviderAdmissionPolicyV1::Proactive,
        },
        debits: vec![ModelTurnBucketDebit {
            bucket_kind: ModelTurnBucketKind::Request,
            units: 1,
        }],
        output_reservation_source: ProviderOutputReservationSourceV1::ExplicitLimit,
        abort: ProviderAttemptAbortHandleV1::new(),
    }
}

fn terminal(terminal: ProviderAttemptTerminalV1, deadline: Option<u64>) -> ProviderOutcomeV1 {
    ProviderOutcomeV1 {
        terminal,
        authoritative_usage: None,
        observation: deadline.map(|retry_after_deadline_monotonic_ms| {
            ProviderNormalizedObservationV1 {
                authoritative_usage: None,
                available_capacity: None,
                reset_epoch: None,
                retry_after_deadline_monotonic_ms: Some(retry_after_deadline_monotonic_ms),
                ignored: None,
                diagnostics: ProviderObservationDiagnosticsV1::default(),
                discovery: ProviderDiscoveryOwnershipV1::Known,
            }
        }),
        abort: djinn_provider::ProviderAttemptAbortResultV1::NotRequested,
        token_emission: Default::default(),
    }
}

struct CoveredParser;
impl ProviderSseFrameParserV1 for CoveredParser {
    fn parse(&mut self, frame: SseFrame) -> Vec<anyhow::Result<StreamEvent>> {
        match frame {
            SseFrame::Data(text) => vec![
                Ok(StreamEvent::Delta(ContentBlock::Text { text })),
                Ok(StreamEvent::Done),
            ],
            SseFrame::Done => vec![Ok(StreamEvent::Done)],
        }
    }
}

/// `stream` panics: this fixture can only pass through covered B1 operations.
struct ScriptedCoveredB1Provider {
    plans: AtomicUsize,
    launches: AtomicUsize,
    launched: [tokio::sync::Notify; 2],
    launch_contexts: Mutex<Vec<ProviderAttemptContextV1>>,
    first_terminal: ProviderAttemptTerminalV1,
    first_retry_deadline: Option<u64>,
    pending_first_stream: bool,
    first_abort: Mutex<Option<ProviderAttemptAbortHandleV1>>,
}
impl ScriptedCoveredB1Provider {
    fn new() -> Self {
        Self::with_first_terminal(ProviderAttemptTerminalV1::Failed(
            ProviderAttemptLossV1::Transport,
        ))
    }

    fn with_first_terminal(first_terminal: ProviderAttemptTerminalV1) -> Self {
        Self::with_first_terminal_and_deadline(first_terminal, Some(0))
    }

    fn with_first_terminal_and_deadline(
        first_terminal: ProviderAttemptTerminalV1,
        first_retry_deadline: Option<u64>,
    ) -> Self {
        Self {
            plans: AtomicUsize::new(0),
            launches: AtomicUsize::new(0),
            launched: [tokio::sync::Notify::new(), tokio::sync::Notify::new()],
            launch_contexts: Mutex::new(Vec::new()),
            first_terminal,
            first_retry_deadline,
            pending_first_stream: false,
            first_abort: Mutex::new(None),
        }
    }

    fn watchdog_pending() -> Self {
        let mut provider = Self::with_first_terminal(ProviderAttemptTerminalV1::Aborted);
        provider.pending_first_stream = true;
        provider
    }
}
impl LlmProvider for ScriptedCoveredB1Provider {
    fn name(&self) -> &str {
        "provider"
    }
    fn stream<'a>(
        &'a self,
        _: &'a Conversation,
        _: &'a [serde_json::Value],
        _: Option<ToolChoice>,
    ) -> Pin<
        Box<
            dyn futures::Future<
                    Output = anyhow::Result<
                        Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        panic!("legacy LlmProvider::stream must never be called by covered retry")
    }
    fn provider_attempt_plan_v1(
        &self,
        _: &str,
        _: &Conversation,
        _: &[serde_json::Value],
        _: Option<ToolChoice>,
    ) -> Result<ProviderAttemptPlanV1, ProviderAttemptRouteCoverageV1> {
        self.plans.fetch_add(1, Ordering::SeqCst);
        Ok(covered_plan())
    }
    fn start_sse_attempt_v1(
        &self,
        _: &Conversation,
        _: &[serde_json::Value],
        _: Option<ToolChoice>,
        context: ProviderAttemptContextV1,
    ) -> Result<ProviderSseAttemptV1, ProviderAttemptRouteCoverageV1> {
        let ordinal = self.launches.fetch_add(1, Ordering::SeqCst);
        self.launch_contexts
            .lock()
            .expect("launch contexts")
            .push(context);
        self.launched[ordinal].notify_waiters();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (frames, outcome) = if ordinal == 0 && self.pending_first_stream {
            // Keep the watchdog attempt's real provider read pending until
            // the production watchdog aborts B1. A completed frame here
            // would let the parser finish before it can set
            // `StreamTurnState::watchdog_aborted`.
            (
                Box::pin(futures::stream::pending::<anyhow::Result<SseFrame>>())
                    as Pin<Box<dyn futures::Stream<Item = anyhow::Result<SseFrame>> + Send>>,
                terminal(ProviderAttemptTerminalV1::Aborted, None),
            )
        } else if ordinal == 0 {
            (
                Box::pin(futures::stream::empty())
                    as Pin<Box<dyn futures::Stream<Item = anyhow::Result<SseFrame>> + Send>>,
                terminal(self.first_terminal, self.first_retry_deadline),
            )
        } else {
            (
                Box::pin(futures::stream::iter([Ok(SseFrame::Data(
                    "replacement succeeded".into(),
                ))]))
                    as Pin<Box<dyn futures::Stream<Item = anyhow::Result<SseFrame>> + Send>>,
                terminal(ProviderAttemptTerminalV1::Completed, None),
            )
        };
        let abort = ProviderAttemptAbortHandleV1::new();
        if ordinal == 0 && self.pending_first_stream {
            *self.first_abort.lock().expect("first abort") = Some(abort.clone());
            let cancellation = abort.cancellation_token();
            tokio::spawn(async move {
                cancellation.cancelled().await;
                tx.send(terminal(ProviderAttemptTerminalV1::Aborted, None))
                    .expect("watchdog B1 terminal");
            });
        } else {
            tx.send(outcome).expect("one B1 terminal");
        }
        Ok(ProviderSseAttemptV1::for_test(frames, abort, rx))
    }
    fn sse_frame_parser_v1(&self) -> Option<Box<dyn ProviderSseFrameParserV1>> {
        Some(Box::new(CoveredParser))
    }
}

#[tokio::test]
async fn covered_retry_reconciles_old_lease_before_fresh_preparation_and_launch() {
    // Setup completes under real Tokio time; the normalized deadline is elapsed.
    let db = Database::ephemeral().await.expect("database");
    let pool = seed_model_turn_admission_fixture(&db, "enforce", "supported", 2).await;
    let session_cancel = CancellationToken::new();
    let supervisor_cancel = CancellationToken::new();
    let slot_ctx = crate::test_helpers::agent_context_from_db(db.clone(), session_cancel.clone());
    let provider = ScriptedCoveredB1Provider::new();
    // This scoped observer is bound to a unique identity, so parallel
    // reply-loop tests cannot replace it or contribute boundaries.
    let session_id = format!("covered-retry-session-{}", uuid::Uuid::now_v7());
    let events = Arc::new(Mutex::new(Vec::new()));
    let settled = Arc::new(tokio::sync::Notify::new());
    let waited = Arc::new(tokio::sync::Notify::new());
    let observed = Arc::clone(&events);
    let settled_observer = Arc::clone(&settled);
    let waited_observer = Arc::clone(&waited);
    let _boundary_observer = register_reply_loop_boundary_observer(
        session_id.clone(),
        Arc::new(move |event| {
            observed.lock().expect("observer").push(event.name);
            if event.name == "covered_attempt_settled" {
                settled_observer.notify_waiters();
            }
            if event.name == "covered_retry_wait" {
                waited_observer.notify_waiters();
            }
        }),
    );
    let first_launch = provider.launched[0].notified();
    let second_launch = provider.launched[1].notified();
    let settlement = settled.notified();
    let retry_wait = waited.notified();
    tokio::pin!(first_launch, second_launch, settlement, retry_wait);
    let mut conversation = Conversation::new();
    conversation.push(Message::user("exercise covered retry"));
    let compaction_cs = crate::reply_loop::CompactionCriticalSection::new();
    let run = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            credential_record_id: "credential-slot",
            tools: &[],
            task_id: "covered-retry-task",
            task_short_id: "covered-retry",
            session_id: &session_id,
            project_path: "/workspace",
            worktree_path: std::path::Path::new("/workspace"),
            role_name: "worker",
            finalize_tool_names: &[],
            context_window: 10_000,
            model_id: "model",
            cancel: &session_cancel,
            global_cancel: &supervisor_cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: Some(2),
            compaction_cs: &compaction_cs,
            session_budget: None,
        },
        &mut conversation,
        false,
    );
    tokio::pin!(run);
    tokio::select! { _ = &mut first_launch => {}, result = &mut run => panic!("first B1 launch missing: {:?}", result.0) }
    tokio::select! { _ = &mut settlement => {}, result = &mut run => panic!("old attempt did not settle: {:?}", result.0) }

    let old_request = format!("{session_id}:covered:1");
    let old = model_turn_launch_identities_fixture(&db)
        .await
        .into_iter()
        .find(|(_, _, request_id)| request_id == &old_request)
        .expect("old persisted lease");
    assert_eq!(old.1, 1);
    assert_eq!(old.2, old_request);
    assert_eq!(
        model_turn_terminal_fixture(&db, &old.0, old.1, &old.2)
            .await
            .0,
        "failed"
    );
    tokio::select! { _ = &mut retry_wait => {}, result = &mut run => panic!("retry wait missing: {:?}", result.0) }
    tokio::select! { _ = &mut second_launch => {}, result = &mut run => panic!("replacement B1 launch missing: {:?}", result.0) }
    let (result, _output, ..) = run.await;
    result.expect("replacement completes reply loop");
    assert_eq!(provider.launches.load(Ordering::SeqCst), 2);

    let leases = model_turn_launch_identities_fixture(&db).await;
    assert_eq!(leases.len(), 2);
    assert_eq!(leases[0], old);
    assert_eq!(leases[1].2, format!("{session_id}:covered:2"));
    assert_ne!(leases[0].0, leases[1].0);
    assert!(leases[1].1 > leases[0].1);
    let launch_contexts = provider
        .launch_contexts
        .lock()
        .expect("launch contexts")
        .clone();
    assert_eq!(launch_contexts.len(), 2);
    for (context, (lease_id, generation, request_id)) in launch_contexts.iter().zip(&leases) {
        let identity = context
            .launch_identity
            .as_ref()
            .expect("enforced B1 launch carries its fenced lease identity");
        assert_eq!(&identity.request_id, request_id);
        assert_eq!(&identity.lease_id, lease_id);
        assert_eq!(identity.generation, *generation);
    }
    assert_eq!(model_turn_accounting_fixture(&db, pool).await, (0, 0, 2));
    let events = events.lock().expect("observer").clone();
    let settled_at = events
        .iter()
        .position(|event| *event == "covered_attempt_settled")
        .expect("settlement");
    let wait_at = events
        .iter()
        .position(|event| *event == "covered_retry_wait")
        .expect("wait");
    let second_prepare = events
        .iter()
        .enumerate()
        .filter(|(_, event)| **event == "model_turn_prepare")
        .nth(1)
        .expect("second prepare")
        .0;
    let second_launch_event = events
        .iter()
        .enumerate()
        .filter(|(_, event)| **event == "model_turn_launch")
        .nth(1)
        .expect("second launch")
        .0;
    assert!(
        settled_at < wait_at && wait_at < second_prepare && second_prepare < second_launch_event
    );
}

async fn cancellation_at_covered_retry_wait_returns(
    cancel_session: bool,
    expected: ReplyLoopCancelled,
) {
    let db = Database::ephemeral().await.expect("database");
    let pool = seed_model_turn_admission_fixture(&db, "enforce", "supported", 2).await;
    let session_cancel = CancellationToken::new();
    let supervisor_cancel = CancellationToken::new();
    let slot_ctx = crate::test_helpers::agent_context_from_db(db.clone(), session_cancel.clone());
    // Keep the retry deadline in the future independently of the bounded
    // deterministic jitter so observer delivery cannot race replacement setup.
    let provider = ScriptedCoveredB1Provider::with_first_terminal_and_deadline(
        ProviderAttemptTerminalV1::Failed(ProviderAttemptLossV1::Transport),
        Some(60_000),
    );
    let session_id = format!("covered-retry-cancel-{}", uuid::Uuid::now_v7());
    let events = Arc::new(Mutex::new(Vec::new()));
    let waited = Arc::new(tokio::sync::Notify::new());
    let observed = Arc::clone(&events);
    let waited_observer = Arc::clone(&waited);
    let _observer = register_reply_loop_boundary_observer(
        session_id.clone(),
        Arc::new(move |event| {
            observed.lock().expect("observer").push(event.name);
            if event.name == "covered_retry_wait" {
                waited_observer.notify_waiters();
            }
        }),
    );
    let retry_wait = waited.notified();
    tokio::pin!(retry_wait);
    let mut conversation = Conversation::new();
    conversation.push(Message::user("cancel only after the settled retry"));
    let compaction_cs = crate::reply_loop::CompactionCriticalSection::new();
    let run = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            credential_record_id: "credential-slot",
            tools: &[],
            task_id: "covered-retry-cancel",
            task_short_id: "covered-retry-cancel",
            session_id: &session_id,
            project_path: "/workspace",
            worktree_path: std::path::Path::new("/workspace"),
            role_name: "worker",
            finalize_tool_names: &[],
            context_window: 10_000,
            model_id: "model",
            cancel: &session_cancel,
            global_cancel: &supervisor_cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: Some(2),
            compaction_cs: &compaction_cs,
            session_budget: None,
        },
        &mut conversation,
        false,
    );
    tokio::pin!(run);
    tokio::select! { _ = &mut retry_wait => {}, result = &mut run => panic!("covered retry wait missing: {:?}", result.0) }
    if cancel_session {
        session_cancel.cancel();
    } else {
        supervisor_cancel.cancel();
    }
    let (result, ..) = run.await;
    assert_eq!(
        result
            .expect_err("cancellation at retry wait must stop replacement")
            .downcast_ref::<ReplyLoopCancelled>(),
        Some(&expected)
    );
    if cancel_session {
        session_cancel.cancel();
    } else {
        supervisor_cancel.cancel();
    }
    tokio::task::yield_now().await;
    assert_eq!(provider.launches.load(Ordering::SeqCst), 1);
    assert_eq!(provider.plans.load(Ordering::SeqCst), 1);
    let leases = model_turn_launch_identities_fixture(&db).await;
    assert_eq!(leases.len(), 1);
    assert_eq!(
        model_turn_terminal_fixture(&db, &leases[0].0, leases[0].1, &leases[0].2)
            .await
            .0,
        "failed"
    );
    assert_eq!(
        model_turn_decision_count_fixture(&db, pool).await,
        0,
        "settlement removes the first decision and cancellation cannot create a replacement"
    );
    assert_eq!(model_turn_accounting_fixture(&db, pool).await, (0, 1, 1));
    let events = events.lock().expect("observer");
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "model_turn_prepare")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "model_turn_launch")
            .count(),
        1
    );
}

#[tokio::test]
async fn session_cancellation_at_covered_retry_wait_prevents_replacement() {
    cancellation_at_covered_retry_wait_returns(true, ReplyLoopCancelled::session()).await;
}

#[tokio::test]
async fn supervisor_shutdown_at_covered_retry_wait_prevents_replacement() {
    cancellation_at_covered_retry_wait_returns(false, ReplyLoopCancelled::supervisor_shutdown())
        .await;
}

async fn terminal_covered_attempt_does_not_replace(
    first_terminal: ProviderAttemptTerminalV1,
    expected_lifecycle: &str,
) {
    let db = Database::ephemeral().await.expect("database");
    let pool = seed_model_turn_admission_fixture(&db, "enforce", "supported", 2).await;
    let session_cancel = CancellationToken::new();
    let supervisor_cancel = CancellationToken::new();
    let slot_ctx = crate::test_helpers::agent_context_from_db(db.clone(), session_cancel.clone());
    let provider = ScriptedCoveredB1Provider::with_first_terminal(first_terminal);
    let session_id = format!("covered-terminal-{}", uuid::Uuid::now_v7());
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&events);
    let _observer = register_reply_loop_boundary_observer(
        session_id.clone(),
        Arc::new(move |event| observed.lock().expect("observer").push(event.name)),
    );
    let mut conversation = Conversation::new();
    conversation.push(Message::user("terminal B1 must not replace"));
    let compaction_cs = crate::reply_loop::CompactionCriticalSection::new();
    let (result, ..) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            credential_record_id: "credential-slot",
            tools: &[],
            task_id: "covered-terminal",
            task_short_id: "covered-terminal",
            session_id: &session_id,
            project_path: "/workspace",
            worktree_path: std::path::Path::new("/workspace"),
            role_name: "worker",
            finalize_tool_names: &[],
            context_window: 10_000,
            model_id: "model",
            cancel: &session_cancel,
            global_cancel: &supervisor_cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: Some(2),
            compaction_cs: &compaction_cs,
            session_budget: None,
        },
        &mut conversation,
        false,
    )
    .await;
    result.expect_err("terminal B1 state must terminate the real reply loop");

    assert_eq!(provider.plans.load(Ordering::SeqCst), 1);
    assert_eq!(provider.launches.load(Ordering::SeqCst), 1);
    let leases = model_turn_launch_identities_fixture(&db).await;
    assert_eq!(leases.len(), 1);
    assert_eq!(
        model_turn_terminal_fixture(&db, &leases[0].0, leases[0].1, &leases[0].2)
            .await
            .0,
        expected_lifecycle
    );
    assert_eq!(
        model_turn_decision_count_fixture(&db, pool).await,
        0,
        "terminal settlement must leave no replacement decision"
    );
    assert_eq!(model_turn_accounting_fixture(&db, pool).await, (0, 1, 1));
    let events = events.lock().expect("observer");
    assert!(
        !events.contains(&"covered_retry_wait"),
        "terminal state must not enter the replacement wait"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "model_turn_prepare")
            .count(),
        1,
        "terminal state must not prepare a replacement"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "model_turn_launch")
            .count(),
        1,
        "terminal state must not launch a replacement"
    );
}

#[tokio::test]
async fn non_retryable_b1_rejection_does_not_replace_in_real_reply_loop() {
    terminal_covered_attempt_does_not_replace(
        ProviderAttemptTerminalV1::Failed(ProviderAttemptLossV1::ProviderRejected),
        "failed",
    )
    .await;
}

#[tokio::test]
async fn watchdog_aborted_b1_terminal_does_not_replace_in_real_reply_loop() {
    let db = Database::ephemeral().await.expect("database");
    let pool = seed_model_turn_admission_fixture(&db, "enforce", "supported", 2).await;
    let session_cancel = CancellationToken::new();
    let supervisor_cancel = CancellationToken::new();
    let slot_ctx = crate::test_helpers::agent_context_from_db(db.clone(), session_cancel.clone());
    let provider = ScriptedCoveredB1Provider::watchdog_pending();
    let session_id = format!("covered-watchdog-{}", uuid::Uuid::now_v7());
    let hooks = Arc::new(ModelTurnAdmissionTestHooks::default());
    hooks.fail_heartbeat.store(true, Ordering::Release);
    hooks.block_watchdog_deadline.store(true, Ordering::Release);
    let _hooks = register_reply_loop_admission_test_hooks(session_id.clone(), Arc::clone(&hooks));
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&events);
    let _observer = register_reply_loop_boundary_observer(
        session_id.clone(),
        Arc::new(move |event| observed.lock().expect("observer").push(event.name)),
    );
    let started = hooks.watchdog_started.notified();
    tokio::pin!(started);
    let mut conversation = Conversation::new();
    conversation.push(Message::user("watchdog abort must suppress replacement"));
    let compaction_cs = crate::reply_loop::CompactionCriticalSection::new();
    let run = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            credential_record_id: "credential-slot",
            tools: &[],
            task_id: "covered-watchdog",
            task_short_id: "covered-watchdog",
            session_id: &session_id,
            project_path: "/workspace",
            worktree_path: std::path::Path::new("/workspace"),
            role_name: "worker",
            finalize_tool_names: &[],
            context_window: 10_000,
            model_id: "model",
            cancel: &session_cancel,
            global_cancel: &supervisor_cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: Some(2),
            compaction_cs: &compaction_cs,
            session_budget: None,
        },
        &mut conversation,
        false,
    );
    tokio::pin!(run);
    tokio::select! { _ = &mut started => {}, result = &mut run => panic!("watchdog did not start: {:?}", result.0) }
    tokio::time::pause();
    let heartbeat = hooks.heartbeat_finished.notified();
    tokio::pin!(heartbeat);
    tokio::time::advance(std::time::Duration::from_secs(20)).await;
    heartbeat.await;
    let deadline = hooks.watchdog_deadline_reached.notified();
    tokio::pin!(deadline);
    tokio::time::advance(std::time::Duration::from_secs(20)).await;
    deadline.await;
    // Resume before terminal reconciliation, then let the production watchdog
    // fire its abort after this test has observed the deadline seam.
    tokio::time::resume();
    hooks.watchdog_deadline_release.notify_waiters();
    let (result, ..) = run.await;
    assert!(
        result
            .expect_err("watchdog abort must terminate loop")
            .to_string()
            .contains("watchdog aborted")
    );
    // The watchdog's initial interval tick and the explicit 20-second tick
    // both attempt their failed heartbeat before its 40-second deadline wins.
    assert_eq!(hooks.heartbeats.load(Ordering::Acquire), 2);
    assert!(
        provider
            .first_abort
            .lock()
            .expect("first abort")
            .as_ref()
            .expect("B1 abort")
            .is_aborted()
    );
    assert_eq!(provider.plans.load(Ordering::SeqCst), 1);
    assert_eq!(provider.launches.load(Ordering::SeqCst), 1);
    let leases = model_turn_launch_identities_fixture(&db).await;
    assert_eq!(leases.len(), 1);
    assert_eq!(
        model_turn_terminal_fixture(&db, &leases[0].0, leases[0].1, &leases[0].2)
            .await
            .0,
        "cancelled"
    );
    assert_eq!(model_turn_decision_count_fixture(&db, pool).await, 0);
    assert_eq!(model_turn_accounting_fixture(&db, pool).await, (0, 1, 1));
    let events = events.lock().expect("observer");
    assert!(!events.contains(&"covered_retry_wait"));
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "model_turn_prepare")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "model_turn_launch")
            .count(),
        1
    );
}

#[derive(Clone, Copy)]
enum ReplacementPreparationCase {
    Wait,
    Rejected,
    DispatchFenced,
}

async fn typed_replacement_preparation_outcome(case: ReplacementPreparationCase) {
    // Database setup deliberately completes before Tokio time is paused.
    let db = Database::ephemeral().await.expect("database");
    let pool = seed_model_turn_admission_fixture(&db, "enforce", "supported", 2).await;
    let session_cancel = CancellationToken::new();
    let supervisor_cancel = CancellationToken::new();
    let slot_ctx = crate::test_helpers::agent_context_from_db(db.clone(), session_cancel.clone());
    let provider = ScriptedCoveredB1Provider::with_first_terminal_and_deadline(
        ProviderAttemptTerminalV1::Failed(ProviderAttemptLossV1::Transport),
        Some(60_000),
    );
    let session_id = format!("covered-retry-preparation-{}", uuid::Uuid::now_v7());
    let hooks = Arc::new(ModelTurnAdmissionTestHooks::default());
    if matches!(case, ReplacementPreparationCase::DispatchFenced) {
        hooks
            .block_dispatching_at_prepare
            .store(2, Ordering::Release);
    }
    let _hooks = register_reply_loop_admission_test_hooks(session_id.clone(), Arc::clone(&hooks));
    let events = Arc::new(Mutex::new(Vec::new()));
    let settled = Arc::new(tokio::sync::Notify::new());
    let waited = Arc::new(tokio::sync::Notify::new());
    let observed = Arc::clone(&events);
    let settled_observer = Arc::clone(&settled);
    let waited_observer = Arc::clone(&waited);
    let _observer = register_reply_loop_boundary_observer(
        session_id.clone(),
        Arc::new(move |event| {
            observed.lock().expect("observer").push(event.name);
            if event.name == "covered_attempt_settled" {
                settled_observer.notify_waiters();
            }
            if event.name == "covered_retry_wait" {
                waited_observer.notify_waiters();
            }
        }),
    );
    let settled = settled.notified();
    let waited = waited.notified();
    tokio::pin!(settled, waited);
    let mut conversation = Conversation::new();
    conversation.push(Message::user("exercise typed replacement preparation"));
    let compaction_cs = crate::reply_loop::CompactionCriticalSection::new();
    let run = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            credential_record_id: "credential-slot",
            tools: &[],
            task_id: "covered-retry-preparation",
            task_short_id: "covered-retry-preparation",
            session_id: &session_id,
            project_path: "/workspace",
            worktree_path: std::path::Path::new("/workspace"),
            role_name: "worker",
            finalize_tool_names: &[],
            context_window: 10_000,
            model_id: "model",
            cancel: &session_cancel,
            global_cancel: &supervisor_cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: Some(2),
            compaction_cs: &compaction_cs,
            session_budget: None,
        },
        &mut conversation,
        false,
    );
    tokio::pin!(run);
    tokio::select! { _ = &mut settled => {}, result = &mut run => panic!("old attempt did not settle: {:?}", result.0) }
    tokio::select! { _ = &mut waited => {}, result = &mut run => panic!("retry wait missing: {:?}", result.0) }
    let old_request = format!("{session_id}:covered:1");
    let old = model_turn_launch_identities_fixture(&db)
        .await
        .into_iter()
        .find(|(_, _, request)| request == &old_request)
        .expect("settled old lease");
    assert_eq!(
        model_turn_terminal_fixture(&db, &old.0, old.1, &old.2)
            .await
            .0,
        "failed"
    );

    match case {
        ReplacementPreparationCase::Wait => set_pool_phase(&db, pool, "draining").await,
        ReplacementPreparationCase::Rejected => set_pool_capability(&db, pool, "unsupported").await,
        ReplacementPreparationCase::DispatchFenced => {}
    }
    // Register before advancing time so the run-scoped hook cannot notify
    // between the replacement acquire and this test's await.
    let reached = hooks.dispatching_reached.notified();
    tokio::pin!(reached);
    tokio::time::pause();
    tokio::time::advance(std::time::Duration::from_secs(61)).await;
    if matches!(case, ReplacementPreparationCase::DispatchFenced) {
        tokio::select! { _ = &mut reached => {}, result = &mut run => panic!("replacement did not acquire before fence: {:?}", result.0) }
        let replacement = hooks.acquired_identities.lock().expect("identities")[1].clone();
        assert_eq!(
            djinn_db::ModelTurnAdmissionRepository::new(db.clone())
                .cancel_before_send(replacement.clone())
                .await
                .expect("cancel replacement"),
            djinn_db::ModelTurnLeaseMutationOutcome::Applied
        );
        hooks.dispatching_release.notify_waiters();
    }
    tokio::time::resume();
    let (result, ..) = run.await;
    let error = result.expect_err("typed preparation must terminate replacement scheduling");
    let outcome = error
        .downcast_ref::<crate::reply_loop::turn::ModelTurnAdmissionOutcome>()
        .expect("reply loop must preserve the concrete admission outcome");
    match (case, outcome) {
        (
            ReplacementPreparationCase::Wait,
            crate::reply_loop::turn::ModelTurnAdmissionOutcome::Wait(
                djinn_db::ModelTurnAdmissionWait::Draining,
            ),
        ) => {}
        (
            ReplacementPreparationCase::Rejected,
            crate::reply_loop::turn::ModelTurnAdmissionOutcome::Rejected(
                djinn_db::ModelTurnAdmissionRejection::UnsupportedCapability {
                    state: djinn_db::ModelTurnCapabilityState::Unsupported,
                },
            ),
        ) => {}
        (
            ReplacementPreparationCase::DispatchFenced,
            crate::reply_loop::turn::ModelTurnAdmissionOutcome::DispatchFenced(
                djinn_db::ModelTurnLeaseMutationOutcome::Fenced,
            ),
        ) => {}
        (_, other) => panic!("wrong concrete replacement outcome: {other:?}"),
    }
    assert_eq!(
        provider.launches.load(Ordering::SeqCst),
        1,
        "typed preparation must not send a replacement"
    );
    assert_eq!(
        provider.plans.load(Ordering::SeqCst),
        2,
        "the production loop must plan and prepare exactly once more"
    );
    assert_eq!(model_turn_decision_count_fixture(&db, pool).await, 0);
    assert_eq!(
        model_turn_accounting_fixture(&db, pool).await.0,
        0,
        "no active accounting survives cleanup"
    );
    let leases = model_turn_launch_identities_fixture(&db).await;
    assert_eq!(
        model_turn_terminal_fixture(&db, &old.0, old.1, &old.2)
            .await
            .0,
        "failed"
    );
    if matches!(case, ReplacementPreparationCase::DispatchFenced) {
        assert_eq!(
            leases.len(),
            2,
            "the fenced replacement is terminal, never launched"
        );
        assert_eq!(
            model_turn_terminal_fixture(&db, &leases[1].0, leases[1].1, &leases[1].2)
                .await
                .0,
            "cancelled"
        );
    } else {
        assert_eq!(leases.len(), 1, "wait/rejection must not prepare a lease");
    }
    let events = events.lock().expect("observer");
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "covered_attempt_settled")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "covered_retry_wait")
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "model_turn_prepare")
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| **event == "model_turn_launch")
            .count(),
        1
    );
}

#[tokio::test]
async fn covered_retry_replacement_prepare_returns_typed_wait_without_send() {
    typed_replacement_preparation_outcome(ReplacementPreparationCase::Wait).await;
}

#[tokio::test]
async fn covered_retry_replacement_prepare_returns_typed_rejection_without_send() {
    typed_replacement_preparation_outcome(ReplacementPreparationCase::Rejected).await;
}

#[tokio::test]
async fn covered_retry_replacement_prepare_returns_typed_fence_without_send() {
    typed_replacement_preparation_outcome(ReplacementPreparationCase::DispatchFenced).await;
}
