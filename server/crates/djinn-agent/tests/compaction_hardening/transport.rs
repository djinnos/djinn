#![allow(clippy::expect_used, clippy::unwrap_used)]

use djinn_provider::message::{ContentBlock, Conversation, Message};
use djinn_provider::provider::{
    ExhaustedTransportCategory, ExhaustedTransportDiagnostic, LlmProvider, ProviderError,
    StreamEvent, ToolChoice,
};
use djinn_slot::reply_loop::error_handling::{
    ExhaustedTransportCategory as ClassifiedCategory, TransportClassificationInput,
    classify_exhausted_transport, is_oversized_transport_payload,
};
use djinn_slot::reply_loop::turn::{
    ReplyLoopContext, TransportRecoveryBudgetCheckHook, TransportRecoveryCompactionHook,
    set_transport_recovery_budget_check_hook_for_test,
    set_transport_recovery_compaction_hook_for_test,
};
use djinn_slot::{CompactionCriticalSection, run_reply_loop};
use futures::stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio_util::sync::CancellationToken;

#[test]
fn utf8_byte_classifier_boundaries() {
    let below = "éééx";
    let equal = "éééé";
    let above = "ééééx";
    assert_eq!((below.len(), equal.len(), above.len()), (7, 8, 9));
    assert!(!is_oversized_transport_payload(below.len(), 2));
    assert!(is_oversized_transport_payload(equal.len(), 2));
    assert!(is_oversized_transport_payload(above.len(), 2));
    assert!(!is_oversized_transport_payload(usize::MAX, 0));
    for c in [
        ClassifiedCategory::ConnectionReset,
        ClassifiedCategory::UnexpectedEof,
        ClassifiedCategory::RequestBodyWrite,
        ClassifiedCategory::ResponseHeaderTimeout,
        ClassifiedCategory::SseFirstEventTimeout,
    ] {
        assert_eq!(
            classify_exhausted_transport(true, None, TransportClassificationInput::Eligible(c), 8)
                .map(|d| d.category),
            Some(c)
        );
    }
    for x in [
        TransportClassificationInput::ProviderBody,
        TransportClassificationInput::Authentication,
        TransportClassificationInput::RateLimit,
        TransportClassificationInput::Cancellation,
        TransportClassificationInput::OrdinaryServerError,
        TransportClassificationInput::PostFirstEvent,
        TransportClassificationInput::UnknownTransport,
    ] {
        assert!(classify_exhausted_transport(true, None, x, 8).is_none());
    }
}

#[derive(Clone, Copy)]
enum Outcome {
    Eligible,
    Empty200,
    Excluded,
    ToolCall,
    Success,
}
struct ScriptedProvider {
    script: Mutex<VecDeque<Outcome>>,
    calls: AtomicUsize,
}
impl ScriptedProvider {
    fn new(script: Vec<Outcome>) -> Self {
        Self {
            script: Mutex::new(script.into()),
            calls: AtomicUsize::new(0),
        }
    }
}
impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "transport-recovery-fixture"
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        let outcome = self
            .script
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected provider retry");
        Box::pin(async move {
            match outcome {
                Outcome::Eligible => Err(anyhow::Error::new(ProviderError::ExhaustedTransport(
                    ExhaustedTransportDiagnostic {
                        category: ExhaustedTransportCategory::ConnectionReset,
                        estimated_payload_chars: 8,
                    },
                ))),
                Outcome::Excluded => Err(anyhow::Error::new(ProviderError::Transport)),
                Outcome::Empty200 => {
                    Ok(Box::pin(stream::empty()) as Pin<Box<dyn futures::Stream<Item = _> + Send>>)
                }
                Outcome::ToolCall => Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::Delta(ContentBlock::ToolUse {
                        id: "fixture-tool-call".into(),
                        name: "fixture_tool".into(),
                        input: serde_json::json!({}),
                    })),
                    Ok(StreamEvent::Done),
                ]))
                    as Pin<Box<dyn futures::Stream<Item = _> + Send>>),
                Outcome::Success => Ok(Box::pin(stream::iter(vec![
                    Ok(StreamEvent::Delta(ContentBlock::Text {
                        text: "done".into(),
                    })),
                    Ok(StreamEvent::Done),
                ]))
                    as Pin<Box<dyn futures::Stream<Item = _> + Send>>),
            }
        })
    }
    fn stream_request_body_bytes(
        &self,
        _: &Conversation,
        _: &[serde_json::Value],
        _: Option<ToolChoice>,
    ) -> Option<usize> {
        Some(8)
    }
}
static RECOVERY_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

async fn run_case(
    script: Vec<Outcome>,
    compaction: Result<bool, &'static str>,
) -> (usize, usize, usize, anyhow::Result<()>) {
    let _serial = RECOVERY_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    let compactions = Arc::new(AtomicUsize::new(0));
    let budget_checks = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&compactions);
    let result = compaction.map_err(str::to_owned);
    let hook: TransportRecoveryCompactionHook = Arc::new(move || {
        count.fetch_add(1, Ordering::SeqCst);
        result.clone()
    });
    set_transport_recovery_compaction_hook_for_test(Some(hook));
    let budget_count = Arc::clone(&budget_checks);
    let budget_hook: TransportRecoveryBudgetCheckHook = Arc::new(move || {
        budget_count.fetch_add(1, Ordering::SeqCst);
    });
    set_transport_recovery_budget_check_hook_for_test(Some(budget_hook));
    let provider = ScriptedProvider::new(script);
    let db = djinn_slot::test_helpers::create_test_db();
    let cancel = CancellationToken::new();
    let slot = djinn_slot::test_helpers::agent_context_from_db(db.clone(), cancel.clone());
    let project = djinn_slot::test_helpers::create_test_project(&db).await;
    let epic = djinn_slot::test_helpers::create_test_epic(&db, &project.id).await;
    let task = djinn_slot::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let session = djinn_db::SessionRepository::new(db, slot.event_bus.clone())
        .create(djinn_db::repositories::session::CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "fixture/model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    let cs = CompactionCriticalSection::new();
    let mut conversation = Conversation::new();
    conversation.push(Message::user("éééé"));
    let output = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &[],
            task_id: &task.id,
            task_short_id: "fixture",
            session_id: &session.id,
            project_path: "/tmp",
            worktree_path: std::path::Path::new("/tmp"),
            role_name: "worker",
            finalize_tool_names: &["submit_work"],
            context_window: 2,
            model_id: "fixture/model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: Some(8),
            compaction_cs: &cs,
        },
        &mut conversation,
        false,
    )
    .await
    .0;
    set_transport_recovery_compaction_hook_for_test(None);
    set_transport_recovery_budget_check_hook_for_test(None);
    (
        provider.calls.load(Ordering::SeqCst),
        compactions.load(Ordering::SeqCst),
        budget_checks.load(Ordering::SeqCst),
        output,
    )
}

#[tokio::test]
async fn recovery_is_one_shot() {
    let (calls, compacted, budget_checks, result) =
        run_case(vec![Outcome::Eligible, Outcome::Success], Ok(true)).await;
    assert_eq!((calls, compacted, budget_checks), (2, 1, 2));
    assert!(result.is_ok());
    let (calls, compacted, budget_checks, result) =
        run_case(vec![Outcome::Eligible], Err("summary failed")).await;
    assert_eq!((calls, compacted, budget_checks), (1, 1, 1));
    assert!(format!("{:#}", result.unwrap_err()).contains("exhausted transport error"));
    let (calls, compacted, budget_checks, result) =
        run_case(vec![Outcome::Eligible, Outcome::Eligible], Ok(true)).await;
    assert_eq!((calls, compacted, budget_checks), (2, 1, 2));
    assert!(format!("{:#}", result.unwrap_err()).contains("exhausted transport error"));
    let (calls, compacted, budget_checks, result) =
        run_case(vec![Outcome::Empty200, Outcome::Success], Ok(true)).await;
    assert_eq!((calls, compacted, budget_checks), (2, 1, 2));
    assert!(result.is_ok());
    let (calls, compacted, budget_checks, result) =
        run_case(vec![Outcome::Excluded], Ok(true)).await;
    assert_eq!((calls, compacted, budget_checks), (1, 0, 1));
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("provider stream init failed")
    );

    // Recovery is one-shot per provider turn, not per reply-loop session. The
    // first recovery retry completes as a tool-call turn, then the independent
    // next provider turn may compact and retry once for its own oversized
    // failure. This executes the production reset, tool-dispatch, and retry
    // paths and keeps the still-oversized immediate-retry assertion above.
    let (calls, compacted, budget_checks, result) = run_case(
        vec![
            Outcome::Eligible,
            Outcome::ToolCall,
            Outcome::Eligible,
            Outcome::Success,
        ],
        Ok(true),
    )
    .await;
    assert_eq!((calls, compacted, budget_checks), (4, 2, 4));
    assert!(result.is_ok());
}

/// Unlike the hook-driven one-shot fixture, this drives the production writer
/// and verifies its durable cause and occupancy snapshots.
#[tokio::test]
async fn oversized_transport_compaction_persists_boundary_occupancy() {
    let _serial = RECOVERY_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap();
    set_transport_recovery_compaction_hook_for_test(None);
    set_transport_recovery_budget_check_hook_for_test(None);

    let provider =
        ScriptedProvider::new(vec![Outcome::Eligible, Outcome::Success, Outcome::Success]);
    let db = djinn_slot::test_helpers::create_test_db();
    let cancel = CancellationToken::new();
    let slot = djinn_slot::test_helpers::agent_context_from_db(db.clone(), cancel.clone());
    let project = djinn_slot::test_helpers::create_test_project(&db).await;
    let epic = djinn_slot::test_helpers::create_test_epic(&db, &project.id).await;
    let task = djinn_slot::test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let session = djinn_db::SessionRepository::new(db.clone(), slot.event_bus.clone())
        .create(djinn_db::repositories::session::CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "fixture/model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .unwrap();
    let cs = CompactionCriticalSection::new();
    let mut conversation = Conversation::new();
    conversation.push(Message::user("éééé"));
    let result = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &[],
            task_id: &task.id,
            task_short_id: "fixture",
            session_id: &session.id,
            project_path: "/tmp",
            worktree_path: std::path::Path::new("/tmp"),
            role_name: "worker",
            finalize_tool_names: &["submit_work"],
            context_window: 2,
            model_id: "fixture/model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: Some(8),
            compaction_cs: &cs,
        },
        &mut conversation,
        false,
    )
    .await
    .0;
    assert!(
        result.is_ok(),
        "oversized recovery should complete: {result:?}"
    );
    let boundary = djinn_db::SessionCompactionBoundaryRepository::new(db)
        .latest_completed_boundary(&session.id)
        .await
        .unwrap()
        .expect("oversized recovery must persist a boundary");
    assert_eq!(
        boundary.trigger,
        Some(djinn_db::CompactionTrigger::OversizedTransport)
    );
    assert_eq!(boundary.current_context_tokens_before, Some(0));
    assert_eq!(boundary.current_context_tokens_after, Some(0));
}
