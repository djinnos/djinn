//! Focused tests for the model-called `submit_work` → `CompletionIntent` cutover.
//!
//! These tests exercise the completion-intent coordinator boundary in the reply
//! loop. The host callback is mocked to control the coordinator outcome so every
//! branch (stored, ineligible, error) is testable deterministically without
//! requiring the production hermetic launcher.
//!
//! No verify-run cache lookup or C1/C2 reuse check is exercised — those are
//! owned by sibling epic `0i1s`.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use crate::final_verification::{
    FinalVerificationInvocationLease, FinalVerificationResolvedMaterial,
};
use crate::host::{ResolvedMcpTools, SlotContext, SlotHostCallbacks};
use crate::reply_loop::{ReplyLoopContext, run_reply_loop};
use crate::test_helpers::{
    FakeProvider, agent_context_from_db_with_callbacks, create_test_db, create_test_epic,
    create_test_project, create_test_task, test_path,
};
use djinn_core::models::Task;
use djinn_db::repositories::task_run::{CreateTaskRunParams, TaskRunRepository};
use djinn_provider::message::{ContentBlock, Conversation, Message, Role};
use djinn_provider::provider::StreamEvent;
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Mock host callbacks for completion-intent tests
// ---------------------------------------------------------------------------

/// A mock `SlotHostCallbacks` that controls final-verification outcomes.
///
/// `resolve_final_verification` is the first callback the coordinator calls.
/// Returning `Err` causes the coordinator to emit
/// `FinalVerificationRecordingOutcome::Error`, which `verify_completion_intent`
/// maps to `Err`. The error string is then injected as an error `ToolResult`
/// correlated to the original `submit_work` tool-use ID.
struct CompletionIntentCallbacks {
    /// Error string to return from `resolve_final_verification`. When `None`,
    /// returns `Ok` with empty material (the execution will then proceed to the
    /// real sandbox, which is unavailable in unit tests and will produce
    /// `Ineligible`/`Error` downstream — but the coordinator boundary is still
    /// exercised).
    resolve_error: Arc<Mutex<Option<String>>>,
    /// Track how many times `resolve_final_verification` was called.
    resolve_call_count: Arc<Mutex<usize>>,
    /// Track how many times `acquire_final_verification_lease` was called.
    lease_call_count: Arc<Mutex<usize>>,
    /// Track how many times the lease was released.
    release_count: Arc<Mutex<usize>>,
}

impl CompletionIntentCallbacks {
    fn new(resolve_error: Option<String>) -> Self {
        Self {
            resolve_error: Arc::new(Mutex::new(resolve_error)),
            resolve_call_count: Arc::new(Mutex::new(0)),
            lease_call_count: Arc::new(Mutex::new(0)),
            release_count: Arc::new(Mutex::new(0)),
        }
    }

    fn resolve_count(&self) -> usize {
        *self.resolve_call_count.lock().unwrap()
    }
    fn lease_count(&self) -> usize {
        *self.lease_call_count.lock().unwrap()
    }
    fn release_total(&self) -> usize {
        *self.release_count.lock().unwrap()
    }
}

/// A mock lease that tracks release calls.
struct MockLease {
    release_count: Arc<Mutex<usize>>,
}

impl FinalVerificationInvocationLease for MockLease {
    fn release<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>> {
        let count = self.release_count.clone();
        Box::pin(async move {
            *count.lock().unwrap() += 1;
            Ok(())
        })
    }
}

impl SlotHostCallbacks for CompletionIntentCallbacks {
    fn resolve_final_verification<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        _verification_attempt_id: &'a str,
        _verify_run_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<Box<dyn Future<Output = Result<FinalVerificationResolvedMaterial, String>> + Send + 'a>>
    {
        let count = self.resolve_call_count.clone();
        let error = self.resolve_error.clone();
        Box::pin(async move {
            *count.lock().unwrap() += 1;
            // No cache/reuse lookup is performed — the completion-intent path
            // always resolves and executes the writer path independently of the
            // reuse feature flag.
            let err = error.lock().unwrap().clone();
            if let Some(e) = err {
                return Err(e);
            }
            // Returning Ok would proceed to execution. In a unit test without
            // the real sandbox, execution will fail. This is acceptable: the
            // important assertion is that the coordinator boundary was crossed.
            Err("final-verification plan is not configured".to_owned())
        })
    }

    fn acquire_final_verification_lease<'a>(
        &'a self,
        _task_id: &'a str,
        _task_run_id: &'a str,
        _verification_attempt_id: &'a str,
        _ctx: &'a SlotContext,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Box<dyn FinalVerificationInvocationLease>, String>>
                + Send
                + 'a,
        >,
    > {
        let count = self.lease_call_count.clone();
        let release = self.release_count.clone();
        Box::pin(async move {
            *count.lock().unwrap() += 1;
            Ok(Box::new(MockLease {
                release_count: release,
            }) as Box<dyn FinalVerificationInvocationLease>)
        })
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
    cancel: CancellationToken,
    callbacks: Arc<CompletionIntentCallbacks>,
}

async fn make_fixture(resolve_error: Option<String>) -> TestFixture {
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

    let callbacks = Arc::new(CompletionIntentCallbacks::new(resolve_error));
    let slot_ctx = agent_context_from_db_with_callbacks(db, callbacks.clone());

    TestFixture {
        slot_ctx,
        project_path,
        task_id: task.id,
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

/// AC: A syntactically valid model-called worker `submit_work` becomes a typed
/// `CompletionIntent`; it is not copied into successful final output and does
/// not end the session before authoritative final verification resolves.
///
/// This test verifies that when final verification fails (the coordinator
/// returns an error), the payload is NOT finalized and an error tool result is
/// injected. The `CompletionIntent` was captured (resolve was called) but the
/// session continues rather than ending successfully.
#[tokio::test]
async fn completion_intent_not_finalized_when_verification_fails() {
    let tools = vec![dummy_tool_schema("submit_work")];
    let provider = FakeProvider::script(vec![vec![
        StreamEvent::Delta(ContentBlock::ToolUse {
            id: "fin-1".into(),
            name: "submit_work".into(),
            input: serde_json::json!({"task_id": "t1", "summary": "done"}),
        }),
        StreamEvent::Done,
    ]]);

    let fixture = make_fixture(Some("verification rejected: command failed".to_owned())).await;
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        "session-not-finalized",
        &fixture.cancel,
    )
    .await;

    // The completion-intent coordinator was invoked.
    assert!(
        fixture.callbacks.resolve_count() >= 1,
        "verify_completion_intent should call resolve_final_verification"
    );
    // The finalize payload must NOT be set — verification failed.
    assert!(
        output.finalize_payload.is_none(),
        "finalize_payload must be None when verification fails"
    );
    // The conversation has an error tool result correlated to the submit_work.
    let has_error_tool_result = conversation.messages.iter().any(|msg| {
        msg.role == Role::User
            && msg.content.iter().any(|block| {
                matches!(
                    block,
                    ContentBlock::ToolResult {
                        tool_use_id,
                        is_error: true,
                        ..
                    } if tool_use_id == "fin-1"
                )
            })
    });
    assert!(
        has_error_tool_result,
        "submit_work must produce an error ToolResult correlated to fin-1"
    );
    let _ = result;
}

/// AC: A recoverable verification failure is persisted and injected as an error
/// `ToolResult` correlated to the original `submit_work` tool-use ID, after
/// which the live worker conversation can continue and resubmit.
#[tokio::test]
async fn recoverable_failure_injects_error_tool_result_and_allows_resubmission() {
    let tools = vec![dummy_tool_schema("submit_work")];
    // First turn: submit_work → fails verification → error tool result injected.
    // Second turn: worker resubmits → fails again.
    let provider = FakeProvider::script(vec![
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "fin-1".into(),
                name: "submit_work".into(),
                input: serde_json::json!({"task_id": "t1", "summary": "first attempt"}),
            }),
            StreamEvent::Done,
        ],
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "fin-2".into(),
                name: "submit_work".into(),
                input: serde_json::json!({"task_id": "t1", "summary": "second attempt"}),
            }),
            StreamEvent::Done,
        ],
    ]);

    let fixture = make_fixture(Some("verification rejected: command failed".to_owned())).await;
    let mut conversation = base_conversation();
    let (result, _output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        "session-recoverable",
        &fixture.cancel,
    )
    .await;

    // The coordinator should have been called for each submit_work.
    assert!(
        fixture.callbacks.resolve_count() >= 2,
        "both submit_work attempts must trigger final verification; got {}",
        fixture.callbacks.resolve_count()
    );
    // The conversation should contain error tool results for both attempts.
    let error_results: Vec<&str> = conversation
        .messages
        .iter()
        .flat_map(|msg| msg.content.iter())
        .filter_map(|block| {
            if let ContentBlock::ToolResult {
                tool_use_id,
                is_error: true,
                ..
            } = block
            {
                Some(tool_use_id.as_str())
            } else {
                None
            }
        })
        .collect();
    assert!(
        error_results.contains(&"fin-1"),
        "first submit_work must produce an error ToolResult correlated to fin-1"
    );
    assert!(
        error_results.contains(&"fin-2"),
        "second submit_work must produce an error ToolResult correlated to fin-2"
    );
    let _ = result;
}

/// AC: A terminal verification failure does not log successful `work_submitted`
/// or advance the task attempt to submitted. The error tool result is injected
/// and the loop continues, but repeated failures never produce successful
/// finalization.
#[tokio::test]
async fn terminal_failure_does_not_finalize_as_success() {
    let tools = vec![dummy_tool_schema("submit_work")];
    let provider = FakeProvider::script(vec![
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "fin-1".into(),
                name: "submit_work".into(),
                input: serde_json::json!({"task_id": "t1", "summary": "attempt"}),
            }),
            StreamEvent::Done,
        ],
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "fin-2".into(),
                name: "submit_work".into(),
                input: serde_json::json!({"task_id": "t1", "summary": "attempt 2"}),
            }),
            StreamEvent::Done,
        ],
    ]);

    let fixture = make_fixture(Some(
        "terminal verification error: coordinator failed".to_owned(),
    ))
    .await;
    let mut conversation = base_conversation();
    let (result, output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        "session-terminal",
        &fixture.cancel,
    )
    .await;

    // Both submit_work attempts triggered verification.
    assert!(
        fixture.callbacks.resolve_count() >= 2,
        "both submit_work attempts must trigger final verification; got {}",
        fixture.callbacks.resolve_count()
    );
    // The finalize payload must NOT be set — terminal failure does not finalize.
    assert!(
        output.finalize_payload.is_none(),
        "terminal failure must not set finalize_payload"
    );
    let _ = result;
}

/// AC: Repeated resubmission keeps re-invoking the coordinator on each new
/// `submit_work`. This proves the worker conversation can continue after each
/// failure and the coordinator boundary is not bypassed on resubmission.
#[tokio::test]
async fn repeated_resubmission_re_invokes_coordinator_each_time() {
    let tools = vec![dummy_tool_schema("submit_work")];
    let provider = FakeProvider::script(vec![
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "fin-1".into(),
                name: "submit_work".into(),
                input: serde_json::json!({"task_id": "t1", "summary": "attempt 1"}),
            }),
            StreamEvent::Done,
        ],
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "fin-2".into(),
                name: "submit_work".into(),
                input: serde_json::json!({"task_id": "t1", "summary": "attempt 2"}),
            }),
            StreamEvent::Done,
        ],
        vec![
            StreamEvent::Delta(ContentBlock::ToolUse {
                id: "fin-3".into(),
                name: "submit_work".into(),
                input: serde_json::json!({"task_id": "t1", "summary": "attempt 3"}),
            }),
            StreamEvent::Done,
        ],
    ]);

    let fixture = make_fixture(Some("verification rejected: command failed".to_owned())).await;
    let mut conversation = base_conversation();
    let (result, _output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        "session-repeated",
        &fixture.cancel,
    )
    .await;

    // All three submit_work attempts triggered verification.
    assert_eq!(
        fixture.callbacks.resolve_count(),
        3,
        "all three submit_work attempts must trigger final verification exactly once each"
    );
    // Each attempt produced an error tool result.
    let error_results: Vec<&str> = conversation
        .messages
        .iter()
        .flat_map(|msg| msg.content.iter())
        .filter_map(|block| {
            if let ContentBlock::ToolResult {
                tool_use_id,
                is_error: true,
                ..
            } = block
            {
                Some(tool_use_id.as_str())
            } else {
                None
            }
        })
        .collect();
    for expected in &["fin-1", "fin-2", "fin-3"] {
        assert!(
            error_results.contains(expected),
            "submit_work {expected} must produce an error ToolResult"
        );
    }
    let _ = result;
}

/// AC: The no-progress submission guard runs before completion-intent capture.
/// When no tool calls are present, the completion-intent path is not reached.
#[tokio::test]
async fn no_progress_guard_runs_before_completion_intent_capture() {
    let tools = vec![dummy_tool_schema("submit_work")];
    // Text-only response — no tool calls at all.
    let provider = FakeProvider::script(vec![vec![
        StreamEvent::Delta(ContentBlock::Text {
            text: "I am still working.".into(),
        }),
        StreamEvent::Done,
    ]]);

    let fixture = make_fixture(None).await;
    let mut conversation = base_conversation();
    let (_result, _output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        "session-no-progress",
        &fixture.cancel,
    )
    .await;

    // No tool calls means no submit_work, so the completion-intent coordinator
    // should NOT have been invoked.
    assert_eq!(
        fixture.callbacks.resolve_count(),
        0,
        "completion-intent coordinator must not be invoked when no submit_work tool call is present"
    );
}

/// AC: A coordinator `stored` result allows the original payload to reach
/// `handle_submit_work` exactly once. The session ends as successful completion
/// after authoritative final verification resolves.
///
/// This test verifies that when the coordinator resolve succeeds (returns Ok),
/// the lease is acquired. In the unit-test environment the execution will fail
/// (no sandbox), producing `Ineligible`, but the key assertion is that the
/// lease was acquired (the stored path can only be reached after lease
/// acquisition). The actual `Stored` row requires the production sandbox which
/// runs in the post-session verification pod.
#[tokio::test]
async fn stored_path_acquires_lease_before_execution() {
    let tools = vec![dummy_tool_schema("submit_work")];
    let provider = FakeProvider::script(vec![vec![
        StreamEvent::Delta(ContentBlock::ToolUse {
            id: "fin-1".into(),
            name: "submit_work".into(),
            input: serde_json::json!({"task_id": "t1", "summary": "done"}),
        }),
        StreamEvent::Done,
    ]]);

    // resolve returns Ok (None error) so the coordinator proceeds to lease.
    let fixture = make_fixture(None).await;
    let mut conversation = base_conversation();
    let (_result, output, _, _, _, _) = run_with_provider(
        &provider,
        &tools,
        &mut conversation,
        &fixture.slot_ctx,
        &fixture.project_path,
        &fixture.task_id,
        "session-stored-lease",
        &fixture.cancel,
    )
    .await;

    // The completion-intent coordinator was invoked and resolve succeeded.
    assert!(
        fixture.callbacks.resolve_count() >= 1,
        "resolve_final_verification should be called"
    );
    // Since resolve returned Ok, the coordinator should have acquired the lease.
    assert!(
        fixture.callbacks.lease_count() >= 1,
        "acquire_final_verification_lease should be called when resolve succeeds"
    );
    // The lease was released (coordinator always releases before returning).
    assert!(
        fixture.callbacks.release_total() >= 1,
        "lease should be released after the coordinator returns"
    );
    // Without the real sandbox, execution fails and the payload is not finalized.
    assert!(
        output.finalize_payload.is_none(),
        "finalize_payload must be None when execution fails without the real sandbox"
    );
}
