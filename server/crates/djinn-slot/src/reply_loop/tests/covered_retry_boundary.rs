//! Production-boundary evidence for covered B1 retry ownership.

use super::*;
use crate::reply_loop::turn::set_reply_loop_boundary_observer;
use djinn_db::test_support::{
    model_turn_accounting_fixture, model_turn_terminal_fixture, seed_model_turn_admission_fixture,
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
    launches: AtomicUsize,
    launched: [tokio::sync::Notify; 2],
    launch_contexts: Mutex<Vec<ProviderAttemptContextV1>>,
}
impl ScriptedCoveredB1Provider {
    fn new() -> Self {
        Self {
            launches: AtomicUsize::new(0),
            launched: [tokio::sync::Notify::new(), tokio::sync::Notify::new()],
            launch_contexts: Mutex::new(Vec::new()),
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
                terminal(
                    ProviderAttemptTerminalV1::Failed(ProviderAttemptLossV1::Transport),
                    Some(0),
                ),
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
    let events = Arc::new(Mutex::new(Vec::new()));
    let settled = Arc::new(tokio::sync::Notify::new());
    let waited = Arc::new(tokio::sync::Notify::new());
    let observed = Arc::clone(&events);
    let settled_observer = Arc::clone(&settled);
    let waited_observer = Arc::clone(&waited);
    set_reply_loop_boundary_observer(Some(Arc::new(move |event| {
        // The observer hook is process-global because it is test-only. Filter
        // its events by this loop's stable session identity so concurrently
        // executing reply-loop tests cannot be mistaken for replacement work.
        if event.session_id != "covered-retry-session" {
            return;
        }
        observed.lock().expect("observer").push(event.name);
        if event.name == "covered_attempt_settled" {
            settled_observer.notify_waiters();
        }
        if event.name == "covered_retry_wait" {
            waited_observer.notify_waiters();
        }
    })));
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
            session_id: "covered-retry-session",
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

    let old_request = "covered-retry-session:covered:1";
    let old: (String, i64, String) = sqlx::query_as("SELECT lease_id::text, generation, request_id FROM model_turn_leases WHERE request_id = $1")
        .bind(old_request).fetch_one(db.pool()).await.expect("old persisted lease");
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
    set_reply_loop_boundary_observer(None);
    result.expect("replacement completes reply loop");
    assert_eq!(provider.launches.load(Ordering::SeqCst), 2);

    let leases: Vec<(String, i64, String)> = sqlx::query_as(
        "SELECT lease_id::text, generation, request_id FROM model_turn_leases ORDER BY generation",
    )
    .fetch_all(db.pool())
    .await
    .expect("both lifecycle records");
    assert_eq!(leases.len(), 2);
    assert_eq!(leases[0], old);
    assert_eq!(leases[1].2, "covered-retry-session:covered:2");
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
