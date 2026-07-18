//! Focused tests for the model-called `submit_work` → `CompletionIntent` cutover.
//!
//! These tests exercise the completion-intent coordinator boundary in the reply
//! loop. The host callback is mocked to control the coordinator outcome so every
//! branch (stored, ineligible, error) is testable deterministically without
//! requiring the production hermetic launcher.
//!
//! The repeat-worker hit below deliberately drives the production consultation
//! path rather than manufacturing a terminal reuse outcome.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::final_verification::{
    FinalVerificationCoordinatorRequest, FinalVerificationRecordingOutcome,
    FinalVerificationSuccessEvidence,
};
use crate::host::{ResolvedMcpTools, SlotContext, SlotHostCallbacks};
use crate::reply_loop::{ReplyLoopContext, run_reply_loop};
use crate::test_helpers::{
    FakeProvider, agent_context_from_db_with_callbacks, create_test_db, create_test_epic,
    create_test_project, create_test_task, test_path,
};
use djinn_core::models::Task;
use djinn_db::repositories::session::{CreateSessionParams, SessionRepository};
use djinn_db::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};
use djinn_provider::message::{ContentBlock, Conversation, Message};
use djinn_provider::provider::StreamEvent;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Mock host callbacks for completion-intent tests
// ---------------------------------------------------------------------------

/// A mock `SlotHostCallbacks` that supplies final typed outcomes at the
/// coordinator's execution/persistence boundary. It deliberately implements no
/// resolution, lease, sandbox, persistence, or verify-run reuse behavior.
struct CompletionIntentCallbacks {
    outcomes: Mutex<VecDeque<FinalVerificationRecordingOutcome>>,
    coordinator_calls: Mutex<usize>,
    expected_task_id: String,
}

impl CompletionIntentCallbacks {
    fn new(expected_task_id: String, outcomes: Vec<FinalVerificationRecordingOutcome>) -> Self {
        Self {
            outcomes: Mutex::new(outcomes.into()),
            coordinator_calls: Mutex::new(0),
            expected_task_id,
        }
    }

    fn coordinator_count(&self) -> usize {
        *self.coordinator_calls.lock().unwrap()
    }
}

impl SlotHostCallbacks for CompletionIntentCallbacks {
    fn final_verification_outcome_for_test(
        &self,
        request: &FinalVerificationCoordinatorRequest,
    ) -> Option<FinalVerificationRecordingOutcome> {
        assert_eq!(
            request.task_id, self.expected_task_id,
            "fixture task ID reached completion-intent verification"
        );
        *self.coordinator_calls.lock().unwrap() += 1;
        self.outcomes.lock().unwrap().pop_front()
    }

    fn interrupt_paused_worker_session<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(async {})
    }
    fn resolve_mcp_tools<'a>(
        &'a self,
        _worktree_path: &'a str,
        _role_name: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = Result<ResolvedMcpTools, String>> + Send + 'a>> {
        Box::pin(async { Err("not implemented in test".into()) })
    }
    fn render_prompt(
        &self,
        _role_name: &str,
        _task: &Task,
        _context_json: &serde_json::Value,
    ) -> String {
        String::new()
    }
    fn initial_user_message<'a>(
        &'a self,
        _task_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = String> + Send + 'a>> {
        Box::pin(async { String::new() })
    }
    fn build_mcp_state(&self, _ctx: &SlotContext) -> djinn_control_plane::McpState {
        panic!("build_mcp_state not needed in completion-intent tests")
    }
    fn require_project_id_for_task_ops<'a>(
        &'a self,
        _project: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<String, djinn_control_plane::tools::task_tools::ErrorResponse>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async {
            Err(djinn_control_plane::tools::task_tools::ErrorResponse {
                error: "not implemented".into(),
            })
        })
    }
    fn resolve_provider_credential<'a>(
        &'a self,
        _provider_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = Result<crate::helpers::ProviderCredential, String>> + Send + 'a>>
    {
        Box::pin(async { Err("not implemented".into()) })
    }
    fn run_task_dispatch<'a>(
        &'a self,
        _task_id: String,
        _project_path: String,
        _model_id: String,
        _ctx: SlotContext,
        _kill: CancellationToken,
        _pause: CancellationToken,
        _resume_lifecycle_metadata: Option<serde_json::Value>,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
    fn touch_activity_rpc<'a>(
        &'a self,
        _task_id: String,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
    fn flush_session_tokens_rpc<'a>(
        &'a self,
        _session_id: String,
        _tokens_in: i64,
        _tokens_out: i64,
        _cache_read: i64,
        _cache_write: i64,
    ) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

// ---------------------------------------------------------------------------
// Test fixtures
// ---------------------------------------------------------------------------

fn dummy_tool_schema(name: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": { "name": name, "description": "test", "parameters": {"type": "object"} },
        "concurrent_safe": false
    })
}

fn base_conversation() -> Conversation {
    let mut conversation = Conversation::new();
    conversation.push(Message::system("You are a worker."));
    conversation.push(Message::user("Do the task."));
    conversation
}

struct TestFixture {
    slot_ctx: SlotContext,
    project_path: String,
    task_id: String,
    session_id: String,
    cancel: CancellationToken,
    callbacks: Arc<CompletionIntentCallbacks>,
}

async fn make_fixture(outcomes: Vec<FinalVerificationRecordingOutcome>) -> TestFixture {
    let cancel = CancellationToken::new();
    let db = create_test_db();
    let project = create_test_project(&db).await;
    let epic = create_test_epic(&db, &project.id).await;
    let task = create_test_task(&db, &project.id, &epic.id).await;
    let project_path = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
        .to_string_lossy()
        .into_owned();

    // Create an active task run so `verify_completion_intent` can find it.
    let run_id = uuid::Uuid::now_v7().to_string();
    let worktree = test_path("djinn-completion-intent-");
    TaskRunRepository::new(db.clone())
        .create(CreateTaskRunParams {
            id: &run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "dispatch",
            status: Some("running"),
            workspace_path: Some(worktree.to_str().unwrap()),
            mirror_ref: None,
        })
        .await
        .expect("create task run");

    let callbacks = Arc::new(CompletionIntentCallbacks::new(task.id.clone(), outcomes));
    let slot_ctx = agent_context_from_db_with_callbacks(db, callbacks.clone());
    let session = SessionRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone())
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "synthetic/test-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: Some(&run_id),
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create completion-intent test session");

    TestFixture {
        slot_ctx,
        project_path,
        task_id: task.id,
        session_id: session.id,
        cancel,
        callbacks,
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_with_provider(
    provider: &dyn djinn_provider::provider::LlmProvider,
    tools: &[serde_json::Value],
    conversation: &mut Conversation,
    slot_ctx: &SlotContext,
    project_path: &str,
    task_id: &str,
    session_id: &str,
    cancel: &CancellationToken,
) -> (
    anyhow::Result<()>,
    crate::output_parser::ParsedAgentOutput,
    i64,
    i64,
    i64,
    i64,
) {
    let worktree = test_path("djinn-reply-loop-ci-");
    let worktree_path = worktree.as_path();
    run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider,
            tools,
            task_id,
            task_short_id: "t1",
            session_id,
            project_path,
            worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "synthetic/test-model",
            cancel,
            global_cancel: cancel,
            ctx: slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        conversation,
        false,
    )
    .await
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

fn stored() -> FinalVerificationRecordingOutcome {
    FinalVerificationRecordingOutcome::Stored {
        verification_attempt_id: "attempt-stored".into(),
        verify_run_id: "run-stored".into(),
        evidence: Box::new(FinalVerificationSuccessEvidence {
            persisted_run_id: "persisted-stored".into(),
            completed_at: "2025-01-01T00:00:00Z".into(),
            ordered_commands: serde_json::json!([]),
            covered_checks: serde_json::json!([]),
            required_checks: vec![],
            verification_input_fingerprint: "fingerprint".into(),
            manifest_version: "manifest-v1".into(),
            environment_identity_digest: "identity".into(),
        }),
    }
}

fn ineligible(reason: &str) -> FinalVerificationRecordingOutcome {
    FinalVerificationRecordingOutcome::Ineligible {
        verification_attempt_id: "attempt-ineligible".into(),
        reason: reason.into(),
    }
}

fn coordinator_error(detail: &str) -> FinalVerificationRecordingOutcome {
    FinalVerificationRecordingOutcome::Error {
        verification_attempt_id: "attempt-error".into(),
        detail: detail.into(),
    }
}

fn submit_turn(id: &str, task_id: &str, summary: &str) -> Vec<StreamEvent> {
    vec![
        StreamEvent::Delta(ContentBlock::ToolUse {
            id: id.into(),
            name: "submit_work".into(),
            input: serde_json::json!({
                "task_id": task_id,
                "commit_title": format!("complete {summary}"),
                "summary": summary,
            }),
        }),
        StreamEvent::Done,
    ]
}

fn error_ids(conversation: &Conversation) -> Vec<&str> {
    conversation
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error: true,
                ..
            } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn stored_verification_forwards_original_payload_exactly_once() {
    let fixture = make_fixture(vec![stored()]).await;
    let expected = serde_json::json!({
        "task_id": fixture.task_id,
        "commit_title": "complete finished work",
        "summary": "finished work",
    });
    let provider = FakeProvider::script(vec![submit_turn(
        "submit-1",
        &fixture.task_id,
        "finished work",
    )]);
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[dummy_tool_schema("submit_work")],
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        &fixture.session_id,
        &fixture.cancel,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(fixture.callbacks.coordinator_count(), 1);
    assert_eq!(output.finalize_payload.as_ref(), Some(&expected));
    assert_eq!(output.finalize_tool_name.as_deref(), Some("submit_work"));
    let intent = output
        .completion_intent
        .expect("valid payload reached completion-intent verification");
    assert_eq!(intent.finalize_payload, expected);
    assert_eq!(intent.tool_use_id, "submit-1");
    assert_eq!(
        intent.final_verification_evidence,
        Some(FinalVerificationSuccessEvidence {
            persisted_run_id: "persisted-stored".into(),
            completed_at: "2025-01-01T00:00:00Z".into(),
            ordered_commands: serde_json::json!([]),
            covered_checks: serde_json::json!([]),
            required_checks: vec![],
            verification_input_fingerprint: "fingerprint".into(),
            manifest_version: "manifest-v1".into(),
            environment_identity_digest: "identity".into(),
        })
    );
    assert!(error_ids(&conversation).is_empty());
}

#[tokio::test]
async fn ineligible_result_is_persisted_and_valid_resubmission_is_reverified() {
    let fixture = make_fixture(vec![ineligible("command failed"), stored()]).await;
    let provider = FakeProvider::script(vec![
        submit_turn("submit-failed", &fixture.task_id, "first attempt"),
        submit_turn("submit-stored", &fixture.task_id, "corrected attempt"),
    ]);
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[dummy_tool_schema("submit_work")],
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        &fixture.session_id,
        &fixture.cancel,
    )
    .await;

    assert!(result.is_ok());
    assert_eq!(fixture.callbacks.coordinator_count(), 2);
    assert_eq!(error_ids(&conversation), vec!["submit-failed"]);
    assert_eq!(
        output.finalize_payload.as_ref().unwrap()["summary"],
        "corrected attempt"
    );
    let persisted = djinn_db::SessionMessageRepository::new(
        fixture.slot_ctx.db.clone(),
        fixture.slot_ctx.event_bus.clone(),
    )
    .load_conversation(&fixture.session_id)
    .await
    .expect("load persisted conversation");
    assert_eq!(error_ids(&persisted), vec!["submit-failed"]);
}

#[tokio::test]
async fn terminal_error_exhausts_conversation_without_success_or_submission() {
    let fixture = make_fixture(vec![coordinator_error("persistence unavailable")]).await;
    let provider = FakeProvider::script_with_terminal_error(
        vec![submit_turn("submit-error", &fixture.task_id, "attempt")],
        "terminal provider failure after submit-error",
    );
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[dummy_tool_schema("submit_work")],
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        &fixture.session_id,
        &fixture.cancel,
    )
    .await;

    assert!(
        result.is_err(),
        "explicit provider failure terminates the real reply loop"
    );
    assert_eq!(provider.remaining(), 0, "terminal provider turn consumed");
    assert_eq!(fixture.callbacks.coordinator_count(), 1);
    assert!(output.finalize_payload.is_none());
    assert!(output.completion_intent.is_none());
    assert_eq!(error_ids(&conversation), vec!["submit-error"]);
}

#[tokio::test]
async fn three_non_stored_attempts_each_reach_verification_and_never_succeed() {
    let fixture = make_fixture(vec![
        ineligible("command one failed"),
        coordinator_error("writer failed"),
        ineligible("command three failed"),
    ])
    .await;
    let provider = FakeProvider::script_with_terminal_error(
        vec![
            submit_turn("submit-1", &fixture.task_id, "attempt one"),
            submit_turn("submit-2", &fixture.task_id, "attempt two"),
            submit_turn("submit-3", &fixture.task_id, "attempt three"),
        ],
        "terminal provider failure after three non-stored attempts",
    );
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &[dummy_tool_schema("submit_work")],
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        &fixture.session_id,
        &fixture.cancel,
    )
    .await;

    assert!(result.is_err());
    assert_eq!(provider.remaining(), 0, "terminal provider turn consumed");
    assert_eq!(fixture.callbacks.coordinator_count(), 3);
    assert!(output.finalize_payload.is_none());
    assert!(output.completion_intent.is_none());
    assert_eq!(
        error_ids(&conversation),
        vec!["submit-1", "submit-2", "submit-3"]
    );
}
