//! Production-boundary evidence for covered B1 retry ownership.

use super::*;
use crate::reply_loop::turn::register_reply_loop_boundary_observer;
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
        }
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
        let (frames, outcome) = if ordinal == 0 {
            (
                Box::pin(futures::stream::empty())
                    as Pin<Box<dyn futures::Stream<Item = anyhow::Result<SseFrame>> + Send>>,
                terminal(self.first_terminal.clone(), self.first_retry_deadline),
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
        tx.send(outcome).expect("one B1 terminal");
        Ok(ProviderSseAttemptV1::for_test(
            frames,
            ProviderAttemptAbortHandleV1::new(),
            rx,
        ))
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
        !events.iter().any(|event| *event == "covered_retry_wait"),
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
    terminal_covered_attempt_does_not_replace(ProviderAttemptTerminalV1::Aborted, "cancelled")
        .await;
}
