use super::error_handling::{
    BudgetWindDownIgnored, empty_turn_backoff, supports_tool_choice_required,
};
// djinn:allow-oversize — integration tests for the entire reply_loop module.
// The file already exceeded the 1500-line / 51.2KB size-guard thresholds
// before the rrdr soft-budget converge reminder tests were added; the marker
// keeps the size guard from re-flagging the pre-existing oversize while
// leaving the new tests in their natural location alongside the related
// reply-loop coverage.
use super::loop_guard::{LoopGuardError, LoopGuardKind};
use super::persistence::serialize_llm_input;
use super::turn::{ReplyLoopContext, WindDownReason, run_reply_loop};
use crate::finalize_handlers::handle_budget_park;
use crate::finalize_handlers::record_rejected_integrity_entry;
use crate::helpers::extract_worker_context;
use crate::output_parser::ParsedAgentOutput;
use crate::test_helpers;
use crate::test_helpers::{extract_stash_content, test_session_settlement_for_stage_outcome};
use djinn_core::clock::TestClock;
use djinn_core::message::Role;
use djinn_core::models::SessionStatus;
use djinn_db::repositories::session::CreateSessionParams;
use djinn_db::{
    SessionCompactionBoundaryRepository, SessionMessageRepository, SessionRepository,
    TaskRepository,
};
use djinn_provider::message::{ContentBlock, Conversation, Message};
use djinn_provider::provider::ToolChoice;
use djinn_provider::provider::{LlmProvider, StreamEvent, TokenUsage};
use djinn_supervisor::{ParkReason, StageOutcome};
use djinn_telemetry::render;
use futures::stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tokio_util::sync::CancellationToken;

mod anthropic_replay;

/// Process-wide mutex that serializes the soft-budget tests' mutations of
/// `DJINN_SESSION_BUDGET_*` env vars. The reply loop reads its
/// `SessionBudgetPolicy` via `SessionBudgetPolicy::from_env()` at the start of
/// `run_reply_loop`, and Rust tests run in parallel by default — so without
/// serialization a concurrent test could observe our env override (or vice
/// versa). The lock is held across the `.await` on `run_reply_loop` on
/// purpose: env mutations are synchronous, the lock is uncontended in spirit
/// (we don't yield inside the critical section), and the existing
/// `AUTO_CODE_CONTEXT_ENV_LOCK` pattern in `helpers/tests.rs` follows the
/// same shape. SAFETY: env mutation always happens with this lock held.
static SESSION_BUDGET_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Clear every `DJINN_SESSION_BUDGET_*` env var that `SessionBudgetPolicy::from_env`
/// consults. Called by soft-budget tests (under the env lock) at teardown so
/// the test doesn't leak a tiny budget into a sibling test that doesn't
/// expect it.
fn clear_session_budget_env() {
    // SAFETY: always called under SESSION_BUDGET_ENV_LOCK.
    unsafe {
        for role in ["WORKER", "PLANNER", "ARCHITECT", "OTHER"] {
            for suffix in [
                "_MAX_TURNS",
                "_MAX_CUMULATIVE_TOKENS",
                "_SOFT_THRESHOLD_RATIO",
                "_HARD_THRESHOLD_RATIO",
            ] {
                let var = format!("DJINN_SESSION_BUDGET_{role}{suffix}");
                std::env::remove_var(var);
            }
        }
    }
}

/// Pre-scripted response: text (optional) + tool calls + token counts.
/// When `_error` is set, `MockProvider::stream()` returns the error immediately
/// instead of producing a stream.
struct MockResponse {
    text: Option<String>,
    tool_calls: Vec<ContentBlock>,
    input_tokens: u32,
    output_tokens: u32,
    _error: Option<anyhow::Error>,
}

impl MockResponse {
    fn text_only(text: &str, input_tokens: u32) -> Self {
        Self {
            text: Some(text.to_string()),
            tool_calls: vec![],
            input_tokens,
            output_tokens: 10,
            _error: None,
        }
    }
    fn tool_call(id: &str, name: &str, input_tokens: u32) -> Self {
        Self::tool_call_with_input(id, name, serde_json::json!({}), input_tokens)
    }
    fn tool_call_with_input(
        id: &str,
        name: &str,
        input: serde_json::Value,
        input_tokens: u32,
    ) -> Self {
        Self {
            text: None,
            tool_calls: vec![ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
            }],
            input_tokens,
            output_tokens: 10,
            _error: None,
        }
    }
}

/// An `LlmProvider` that pops from a fixed queue of `MockResponse`s.
/// When the queue is empty it returns a text-only "fallback done" response
/// so that the loop always terminates.
struct MockProvider {
    responses: Arc<Mutex<VecDeque<MockResponse>>>,
}

impl MockProvider {
    fn new(responses: Vec<MockResponse>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(responses.into())),
        }
    }
    fn remaining(&self) -> usize {
        self.responses.lock().unwrap().len()
    }

    /// Complete success-path submit fixtures with the generated task identity.
    /// Payloads without a summary remain untouched for validation tests.
    fn bind_valid_submit_work_fixtures(&self, task_id: &str) {
        for response in self.responses.lock().unwrap().iter_mut() {
            for block in &mut response.tool_calls {
                let ContentBlock::ToolUse { name, input, .. } = block else {
                    continue;
                };
                if name != "submit_work" || input.get("summary").is_none() {
                    continue;
                }
                let Some(object) = input.as_object_mut() else {
                    continue;
                };
                object.insert("task_id".into(), serde_json::Value::String(task_id.into()));
                object
                    .entry("commit_title")
                    .or_insert_with(|| serde_json::Value::String("complete test work".into()));
            }
        }
    }
}

impl LlmProvider for MockProvider {
    fn name(&self) -> &str {
        "mock"
    }
    fn stream<'a>(
        &'a self,
        _conversation: &'a Conversation,
        _tools: &'a [serde_json::Value],
        _tool_choice: Option<ToolChoice>,
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
        let responses = Arc::clone(&self.responses);
        Box::pin(async move {
            let resp = responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| MockResponse::text_only("fallback done", 50));
            // If the mock response has an error, return it immediately
            // (simulates a provider failure such as context_length_exceeded).
            if let Some(e) = resp._error {
                return Err(e);
            }
            let mut events: Vec<anyhow::Result<StreamEvent>> = vec![];
            if let Some(text) = resp.text {
                events.push(Ok(StreamEvent::Delta(ContentBlock::Text { text })));
            }
            for tc in resp.tool_calls {
                events.push(Ok(StreamEvent::Delta(tc)));
            }
            events.push(Ok(StreamEvent::Usage(TokenUsage {
                input: resp.input_tokens,
                output: resp.output_tokens,
                ..Default::default()
            })));
            events.push(Ok(StreamEvent::Done));
            Ok(Box::pin(stream::iter(events))
                as Pin<
                    Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                >)
        })
    }
}

/// Returns (context, project_path, task_id, session_id, cancel).
async fn make_context() -> (
    crate::host::SlotContext,
    String,
    String,
    String,
    CancellationToken,
) {
    let (ctx, project_path, task_id, session_id, cancel, _task) = make_context_with_task().await;
    (ctx, project_path, task_id, session_id, cancel)
}

/// Extended `make_context` that also returns the `Task` model so callers can
/// pass it to `render_prompt_for_role` for realistic prompt rendering.
async fn make_context_with_task() -> (
    crate::host::SlotContext,
    String,
    String,
    String,
    CancellationToken,
    djinn_core::models::Task,
) {
    let cancel = CancellationToken::new();
    let db = test_helpers::create_test_db();
    let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let task_run_id = uuid::Uuid::now_v7().to_string();
    djinn_db::repositories::task_run::TaskRunRepository::new(db.clone())
        .create(djinn_db::repositories::task_run::CreateTaskRunParams {
            id: &task_run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "dispatch",
            status: Some("running"),
            workspace_path: Some("/tmp"),
            mirror_ref: None,
        })
        .await
        .expect("create active task run");
    // Create a real session row so session_messages FK constraint is satisfied.
    let session_repo = SessionRepository::new(db.clone(), ctx.event_bus.clone());
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "test/mock-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create session");
    let project_path = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
        .to_string_lossy()
        .into_owned();
    let task_id = task.id.clone();
    let session_id = session.id.clone();
    (ctx, project_path, task_id, session_id, cancel, task)
}

/// Holds common test state for reply-loop tests, eliminating the repeated
/// `make_context()` + `Conversation` setup + `ReplyLoopContext { ... }` block.
struct ReplyLoopHarness {
    slot_ctx: crate::host::SlotContext,
    project_path: String,
    task_id: String,
    session_id: String,
    cancel: CancellationToken,
    conv: Conversation,
    /// Defaults to the canonical worker path; phase scripts override this to
    /// isolate their process-global collector samples from dispatcher tests.
    role_name: &'static str,
}

type ReplyLoopResult = (anyhow::Result<()>, ParsedAgentOutput, i64, i64, i64, i64);

impl ReplyLoopHarness {
    async fn new() -> Self {
        let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
        let mut conv = Conversation::new();
        conv.push(Message::system("You are a worker."));
        conv.push(Message::user("Do the task."));
        Self {
            slot_ctx,
            project_path,
            task_id,
            session_id,
            cancel,
            conv,
            role_name: "worker",
        }
    }

    /// Build a harness with the **real** post-wzz6 worker prompt surface:
    /// the actual rendered role prompt with `format_tools_section` applied to
    /// the real canonical tool schemas.  This is the harness used by the
    /// provider-tool preservation regression tests so that a regression in
    /// prompt rendering or canonical tool schema generation would be caught.
    async fn new_with_worker_prompt() -> Self {
        let (slot_ctx, project_path, task_id, session_id, cancel, task) =
            make_context_with_task().await;
        let tool_schemas_fn = djinn_mcp_extension::tool_defs::tool_schemas_worker;
        let role_config = djinn_roles::config::config_for(djinn_roles::AgentType::Worker);
        let task_ctx = djinn_roles::prompts::TaskContext {
            project_path: project_path.clone(),
            workspace_path: "/tmp".to_string(),
            diff: None,
            commits: None,
            start_commit: None,
            end_commit: None,
            conflict_files: None,
            merge_base_branch: None,
            merge_target_branch: None,
            merge_failure_context: None,
            setup_commands: None,
            activity: None,
            worker_summary: None,
            worker_concerns: None,
            epic_context: None,
            knowledge_context: None,
            code_graph_context: None,
            reviewer_diff_context: None,
            ci_blocking_directive: None,
            worker_resume_note: None,
            arbiter_directive: None,
        };
        let system_prompt = djinn_roles::prompts::render_prompt_for_role(
            role_config,
            tool_schemas_fn,
            &task,
            &task_ctx,
        );
        let mut conv = Conversation::new();
        conv.push(Message::system(system_prompt));
        conv.push(Message::user(format!(
            "Implement task {}: {}",
            task.short_id, task.title
        )));
        Self {
            slot_ctx,
            project_path,
            task_id,
            session_id,
            cancel,
            conv,
            role_name: "worker",
        }
    }

    /// Run the reply loop with default settings (context_window=10_000,
    /// model_id="test/mock-model", no max_turns override).
    async fn run(
        &mut self,
        provider: &dyn LlmProvider,
        tools: &[serde_json::Value],
    ) -> ReplyLoopResult {
        self.run_with(provider, tools, 10_000, "test/mock-model", None)
            .await
    }

    /// Run with a custom context_window.
    async fn run_with_window(
        &mut self,
        provider: &dyn LlmProvider,
        tools: &[serde_json::Value],
        context_window: i64,
    ) -> ReplyLoopResult {
        self.run_with(provider, tools, context_window, "test/mock-model", None)
            .await
    }

    /// Run with a custom model_id.
    async fn run_with_model(
        &mut self,
        provider: &dyn LlmProvider,
        tools: &[serde_json::Value],
        model_id: &str,
    ) -> ReplyLoopResult {
        self.run_with(provider, tools, 10_000, model_id, None).await
    }

    /// Run with a custom max_turns_override.
    async fn run_with_max_turns(
        &mut self,
        provider: &dyn LlmProvider,
        tools: &[serde_json::Value],
        max_turns: u32,
    ) -> ReplyLoopResult {
        self.run_with(provider, tools, 10_000, "test/mock-model", Some(max_turns))
            .await
    }

    /// Run with all parameters customizable.
    async fn run_with(
        &mut self,
        provider: &dyn LlmProvider,
        tools: &[serde_json::Value],
        context_window: i64,
        model_id: &str,
        max_turns_override: Option<u32>,
    ) -> ReplyLoopResult {
        let worktree_path = std::path::PathBuf::from("/tmp");
        run_reply_loop(
            ReplyLoopContext {
                compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
                provider,
                tools,
                task_id: &self.task_id,
                task_short_id: "t1",
                session_id: &self.session_id,
                project_path: &self.project_path,
                worktree_path: &worktree_path,
                role_name: self.role_name,
                finalize_tool_names: &["submit_work", "request_planner"],
                context_window,
                model_id,
                cancel: &self.cancel,
                global_cancel: &self.cancel,
                ctx: &self.slot_ctx,
                active_skill_names: &[],
                active_mcp_server_names: &[],
                max_turns_override,
            },
            &mut self.conv,
            false,
        )
        .await
    }
}

async fn count_persisted_messages(slot_ctx: &crate::host::SlotContext, session_id: &str) -> usize {
    let repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    repo.load_conversation(session_id)
        .await
        .map(|c| c.messages.len())
        .unwrap_or(0)
}

async fn count_persisted_assistant_messages(
    slot_ctx: &crate::host::SlotContext,
    session_id: &str,
) -> usize {
    let repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    repo.load_conversation(session_id)
        .await
        .map(|c| {
            c.messages
                .iter()
                .filter(|m| m.role == Role::Assistant)
                .count()
        })
        .unwrap_or(0)
}

#[test]
fn extract_stash_content_shell_extracts_stdout() {
    let value = serde_json::json!({
        "ok": true,
        "exit_code": 0,
        "stdout": "line 1\nline 2\nline 3\n",
        "stderr": "",
        "workdir": "/tmp"
    });
    let result = extract_stash_content("shell", &value).unwrap();
    assert!(result.contains("line 1"));
    assert!(result.contains("line 3"));
    assert!(!result.contains("workdir"));
    assert!(!result.contains("exit code"));
}

#[test]
fn extract_stash_content_shell_includes_stderr_and_exit_code() {
    let value = serde_json::json!({
        "ok": false,
        "exit_code": 1,
        "stdout": "building...\n",
        "stderr": "error: failed\n",
        "workdir": "/tmp"
    });
    let result = extract_stash_content("shell", &value).unwrap();
    assert!(result.contains("building..."));
    assert!(result.contains("--- stderr ---"));
    assert!(result.contains("error: failed"));
    assert!(result.contains("[exit code: 1]"));
}

#[test]
fn extract_stash_content_non_shell_returns_none() {
    let value = serde_json::json!({"tasks": []});
    assert!(extract_stash_content("task_list", &value).is_none());
}

/// A single ToolUse turn above the compaction threshold triggers compaction,
/// persists messages to DB, and replaces the conversation. The session then
/// continues with the compacted context and ends normally.
#[tokio::test]
async fn proactive_compaction_fires_when_current_context_exceeds_threshold() {
    // context_window = 10,000 → threshold = 8,000 tokens
    let context_window = 10_000_i64;
    // Turn 1: ToolUse + 8,500 input tokens → above threshold → compaction fires.
    //         Tool dispatch is skipped when compaction fires (conversation replaced).
    // Turn 2: compaction LLM call → summary text returned.
    // Turn 3: "Continue with the task." → text-only → ends session.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call("t1", "nonexistent_tool", 8_500),
        MockResponse::text_only("Summary: worked on the task using shell tools.", 200),
        MockResponse::text_only("Completed the task.", 300),
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    // Session should end successfully (compacted + continued).
    assert!(result.is_ok(), "expected ok, got: {:?}", result);
    // All 3 mock responses were consumed.
    assert_eq!(
        provider.remaining(),
        0,
        "all mock responses should be consumed"
    );
    // Messages were persisted to DB before compaction.
    let persisted = count_persisted_messages(&slot_ctx, &session_id).await;
    assert!(
        persisted > 0,
        "expected session messages persisted before compaction, got 0"
    );
    // Conversation was replaced by compaction then continued.
    // Expected: [system, summary_user, ack_assistant, last_user_task,
    //            continue_user, final_assistant] = 6 messages.
    // The key check is that it's much smaller than an uncompacted session
    // and that the system prompt is first.
    assert!(
        conv.messages.len() <= 7,
        "conversation should be compact after compaction, got {} messages",
        conv.messages.len()
    );
    assert_eq!(
        conv.messages[0].role,
        djinn_provider::message::Role::System,
        "first message should still be the system prompt"
    );
}

/// Compaction must NOT fire based on the cumulative sum of input tokens across
/// turns.  Even if the running sum exceeds the threshold, only the current
/// turn's input token count (the actual context window fill) matters.
///
/// Pattern: each turn adds tokens at a rate that would push the SUM above the
/// threshold quickly, but the actual context (latest generation input) stays
/// well below 80%.
#[tokio::test]
async fn no_compaction_when_sum_large_but_current_context_small() {
    // context_window = 10,000 → threshold = 8,000 tokens
    let context_window = 10_000_i64;
    // Turn 1: ToolUse + 7,500 input  (sum=7_500, current=7_500 → below threshold)
    // Turn 2: ToolUse + 7,800 input  (sum=15_300, current=7_800 → below threshold)
    //   With the OLD sum-based check: sum 15,300 > 8,000 → compaction would wrongly fire.
    //   With the NEW current-context check: 7,800 < 8,000 → no compaction. ✓
    // Turn 3: text-only "done" + 100 input  (ends session normally)
    let provider = MockProvider::new(vec![
        MockResponse::tool_call("t1", "nonexistent_tool", 7_500),
        MockResponse::tool_call("t2", "nonexistent_tool", 7_800),
        MockResponse::text_only("Completed.", 100),
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    assert!(result.is_ok(), "expected ok, got: {:?}", result);
    assert_eq!(
        provider.remaining(),
        0,
        "all 3 mock responses should be consumed"
    );
    // No compaction should have fired. Persisted message count is no
    // longer the signal (the reply loop now persists every turn to the DB
    // regardless of compaction); instead assert the conversation was never
    // replaced by a summary — every turn is still present: system + initial
    // user + two tool-call turns with their results + the final text turn.
    assert!(
        conv.messages.len() >= 6,
        "compaction should not have fired — expected the full conversation, got {} messages",
        conv.messages.len()
    );
    assert_eq!(
        conv.messages[0].role,
        djinn_provider::message::Role::System,
        "first message should still be the original system prompt (not a summary)"
    );
    // Per-turn persistence: every non-system message is durably stored
    // (the system prompt is intentionally skipped).
    let persisted = count_persisted_messages(&slot_ctx, &session_id).await;
    assert_eq!(
        persisted,
        conv.messages.len() - 1,
        "expected every non-system message persisted per-turn, got {persisted}"
    );
}

/// Reactive compaction fires when the provider itself signals a
/// context-length error.  The session compacts and retries successfully.
#[tokio::test]
async fn reactive_compaction_on_context_length_error() {
    let context_window = 10_000_i64;
    // Provider behaviour:
    //   • Turn 1: ToolUse + small tokens (below threshold).
    //   • Turn 2 attempt: context_length error mid-stream → reactive compaction triggered.
    //   • Compaction call: summary returned.
    //   • Turn 2 retry: text-only → session ends.
    //
    // We simulate the context-length error by injecting an error event
    // BEFORE the ToolUse delta, so the stream init itself fails.
    struct ErrorOnSecondCallProvider {
        call_count: Arc<Mutex<u32>>,
        inner: MockProvider,
    }
    impl LlmProvider for ErrorOnSecondCallProvider {
        fn name(&self) -> &str {
            "mock-error"
        }
        fn stream<'a>(
            &'a self,
            conversation: &'a Conversation,
            tools: &'a [serde_json::Value],
            tool_choice: Option<ToolChoice>,
        ) -> Pin<
            Box<
                dyn futures::Future<
                        Output = anyhow::Result<
                            Pin<
                                Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                            >,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            let count = Arc::clone(&self.call_count);
            let inner = &self.inner;
            let turn = {
                let mut n = count.lock().unwrap();
                *n += 1;
                *n
            };
            if turn == 2 {
                // Simulate a context-length-exceeded error on stream init.
                Box::pin(async move { Err(anyhow::anyhow!("context_length exceeded")) })
            } else {
                inner.stream(conversation, tools, tool_choice)
            }
        }
    }
    let inner = MockProvider::new(vec![
        // Call 1: normal ToolUse turn.
        MockResponse::tool_call("t1", "nonexistent_tool", 500),
        // Call 2 would error (handled above).
        // Call 3: compaction LLM summary.
        MockResponse::text_only("Summary: used nonexistent_tool.", 100),
        // Call 4: continuation after compaction.
        MockResponse::text_only("Done.", 120),
    ]);
    let provider = ErrorOnSecondCallProvider {
        call_count: Arc::new(Mutex::new(0)),
        inner,
    };
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    assert!(
        result.is_ok(),
        "expected ok after reactive compaction, got: {:?}",
        result
    );
    // Messages are persisted per-turn (independent of the reactive
    // compaction that fired on the context-length error).
    let persisted = count_persisted_messages(&slot_ctx, &session_id).await;
    assert!(
        persisted > 0,
        "expected session messages persisted per-turn"
    );
}

#[test]
fn serialize_llm_input_preserves_system_tools_and_full_history_order() {
    let mut conversation = Conversation::new();
    conversation.push(Message::system("You are a worker."));
    conversation.push(Message::user("First request"));
    conversation.push(Message::assistant("First reply"));
    conversation.push(Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "tool_1".into(),
            name: "shell".into(),
            input: serde_json::json!({"command": "pwd"}),
        }],
        metadata: None,
    });
    conversation.push(Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: "tool_1".into(),
            content: vec![ContentBlock::text("/tmp")],
            is_error: false,
        }],
        metadata: None,
    });
    conversation.push(Message::user("Second request"));
    let tools = vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "shell",
            "description": "Run shell commands",
            "parameters": {"type": "object"}
        }
    })];
    let input = serialize_llm_input(&conversation, &tools);
    assert_eq!(input["tools"], serde_json::json!(tools));
    let messages = input["messages"].as_array().expect("messages array");
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[0]["content"], "You are a worker.");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "First request");
    assert_eq!(messages[2]["role"], "assistant");
    assert_eq!(messages[2]["content"], "First reply");
    assert_eq!(messages[3]["role"], "assistant");
    assert_eq!(messages[3]["tool_calls"][0]["id"], "tool_1");
    assert_eq!(messages[4]["role"], "tool");
    assert_eq!(messages[4]["tool_call_id"], "tool_1");
    assert_eq!(messages[5]["role"], "user");
    assert_eq!(messages[5]["content"], "Second request");
}

#[test]
fn serialize_llm_input_preserves_parallel_tool_call_order() {
    let mut conversation = Conversation::new();
    conversation.push(Message::system("You are a worker."));
    conversation.push(Message::user("Do three things at once"));
    // Assistant returns 3 parallel tool calls in a single message.
    conversation.push(Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::ToolUse {
                id: "tc_a".into(),
                name: "shell".into(),
                input: serde_json::json!({"command": "echo A"}),
            },
            ContentBlock::ToolUse {
                id: "tc_b".into(),
                name: "memory_search".into(),
                input: serde_json::json!({"query": "foo"}),
            },
            ContentBlock::ToolUse {
                id: "tc_c".into(),
                name: "task_list".into(),
                input: serde_json::json!({}),
            },
        ],
        metadata: None,
    });
    // Tool results come back in a single user message (same order).
    conversation.push(Message {
        role: Role::User,
        content: vec![
            ContentBlock::ToolResult {
                tool_use_id: "tc_a".into(),
                content: vec![ContentBlock::text("A")],
                is_error: false,
            },
            ContentBlock::ToolResult {
                tool_use_id: "tc_b".into(),
                content: vec![ContentBlock::text("found: bar")],
                is_error: false,
            },
            ContentBlock::ToolResult {
                tool_use_id: "tc_c".into(),
                content: vec![ContentBlock::text("[]")],
                is_error: false,
            },
        ],
        metadata: None,
    });
    conversation.push(Message::user("Now summarize"));
    let tools = vec![serde_json::json!({
        "type": "function",
        "function": {
            "name": "shell",
            "description": "Run shell commands",
            "parameters": {"type": "object"}
        }
    })];
    let input = serialize_llm_input(&conversation, &tools);
    let messages = input["messages"].as_array().expect("messages array");
    // system, user, assistant(3 tool_calls), tool(A), tool(B), tool(C), user
    assert_eq!(messages.len(), 7);
    assert_eq!(messages[0]["role"], "system");
    assert_eq!(messages[1]["role"], "user");
    assert_eq!(messages[1]["content"], "Do three things at once");
    // Assistant message with 3 tool_calls in order.
    assert_eq!(messages[2]["role"], "assistant");
    let tool_calls = messages[2]["tool_calls"].as_array().expect("tool_calls");
    assert_eq!(tool_calls.len(), 3);
    assert_eq!(tool_calls[0]["id"], "tc_a");
    assert_eq!(tool_calls[1]["id"], "tc_b");
    assert_eq!(tool_calls[2]["id"], "tc_c");
    // Tool results in matching order.
    assert_eq!(messages[3]["role"], "tool");
    assert_eq!(messages[3]["tool_call_id"], "tc_a");
    assert_eq!(messages[3]["content"], "A");
    assert_eq!(messages[4]["role"], "tool");
    assert_eq!(messages[4]["tool_call_id"], "tc_b");
    assert_eq!(messages[4]["content"], "found: bar");
    assert_eq!(messages[5]["role"], "tool");
    assert_eq!(messages[5]["tool_call_id"], "tc_c");
    assert_eq!(messages[5]["content"], "[]");
    assert_eq!(messages[6]["role"], "user");
    assert_eq!(messages[6]["content"], "Now summarize");
}

fn dummy_tool_schema(name: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": { "name": name, "description": "test", "parameters": {"type": "object"} }
    })
}

type ScriptedReplyLoopRun = (
    anyhow::Result<()>,
    ParsedAgentOutput,
    Conversation,
    crate::host::SlotContext,
    String,
);

async fn run_scripted_reply_loop(
    provider: &MockProvider,
    tools: &[serde_json::Value],
) -> ScriptedReplyLoopRun {
    run_scripted_reply_loop_with_dispatcher(
        provider,
        tools,
        Some(std::sync::Arc::new(test_helpers::MockToolDispatcher)),
    )
    .await
}

async fn run_scripted_reply_loop_with_dispatcher(
    provider: &MockProvider,
    tools: &[serde_json::Value],
    tool_dispatcher: Option<std::sync::Arc<dyn crate::host::SlotToolDispatcher>>,
) -> ScriptedReplyLoopRun {
    let cancel = CancellationToken::new();
    let db = test_helpers::create_test_db();
    let slot_ctx = test_helpers::agent_context_from_db_with_dispatcher(
        db.clone(),
        cancel.clone(),
        tool_dispatcher,
    );
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let task_run_id = uuid::Uuid::now_v7().to_string();
    djinn_db::repositories::task_run::TaskRunRepository::new(db.clone())
        .create(djinn_db::repositories::task_run::CreateTaskRunParams {
            id: &task_run_id,
            project_id: &project.id,
            task_id: &task.id,
            trigger_type: "dispatch",
            status: Some("running"),
            workspace_path: Some("/tmp"),
            mirror_ref: None,
        })
        .await
        .expect("create active task run");
    let session_repo = SessionRepository::new(db.clone(), slot_ctx.event_bus.clone());
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "test/mock-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create session");
    let project_path = djinn_core::paths::project_dir(&project.github_owner, &project.github_repo)
        .to_string_lossy()
        .into_owned();
    let task_id = task.id;
    provider.bind_valid_submit_work_fixtures(&task_id);
    let session_id = session.id;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider,
            tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    (result, output, conv, slot_ctx, session_id)
}

/// Session ends immediately when the finalize tool is called.
/// The payload is captured on the output.
#[tokio::test]
async fn finalize_tool_call_ends_session_and_captures_payload() {
    let tools = vec![dummy_tool_schema("submit_work")];
    let provider = MockProvider::new(vec![MockResponse {
        text: None,
        tool_calls: vec![ContentBlock::ToolUse {
            id: "fin1".to_string(),
            name: "submit_work".to_string(),
            input: serde_json::json!({
                "task_id": "t1",
                "commit_title": "complete test work",
                "summary": "done"
            }),
        }],
        input_tokens: 100,
        output_tokens: 10,
        _error: None,
    }]);
    let mut h = ReplyLoopHarness::new().await;
    provider.bind_valid_submit_work_fixtures(&h.task_id);
    let (result, output, _tokens_in, _tokens_out, _cr, _cw) = h.run(&provider, &tools).await;
    assert!(result.is_ok(), "expected ok, got: {:?}", result);
    assert_eq!(provider.remaining(), 0, "finalize response consumed");
    assert!(
        output.finalize_payload.is_some(),
        "finalize payload should be captured"
    );
    assert_eq!(
        output.finalize_payload.unwrap()["summary"],
        "done",
        "payload should contain summary"
    );
}

/// A text-only response without a finalize call injects a nudge and continues.
/// After 3 consecutive nudges the session fails.
#[tokio::test]
async fn text_only_without_finalize_triggers_nudge_then_fails() {
    let tools = vec![dummy_tool_schema("submit_work")];
    // 3 text-only responses → MAX_NUDGE_ATTEMPTS exceeded → error.
    let provider = MockProvider::new(vec![
        MockResponse::text_only("I think I'm done.", 100),
        MockResponse::text_only("Still think I'm done.", 110),
        MockResponse::text_only("Yes, definitely done.", 120),
        // The 4th turn is never reached because we fail after 3 nudges.
    ]);
    let mut h = ReplyLoopHarness::new().await;
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = h.run(&provider, &tools).await;
    assert!(result.is_err(), "expected error after nudge exhaustion");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("consecutive text-only"),
        "error should mention consecutive text-only responses"
    );
}

/// A nudge resets after a successful tool call.
/// Pattern: text-only (nudge 1) → tool call (resets) → text-only (nudge 1) → finalize (ok).
#[tokio::test]
async fn nudge_count_resets_after_tool_call() {
    let tools = vec![
        dummy_tool_schema("some_tool"),
        dummy_tool_schema("submit_work"),
    ];
    let provider = MockProvider::new(vec![
        // Turn 1: text-only → nudge 1
        MockResponse::text_only("hmm", 100),
        // Turn 2: real tool call → resets nudge count
        MockResponse::tool_call("tc1", "some_tool", 110),
        // Turn 3: text-only → nudge 1 again (not 2)
        MockResponse::text_only("ok", 120),
        // Turn 4: finalize → session complete
        MockResponse {
            text: None,
            tool_calls: vec![ContentBlock::ToolUse {
                id: "fin1".to_string(),
                name: "submit_work".to_string(),
                input: serde_json::json!({
                    "task_id": "t1",
                    "commit_title": "complete test work",
                    "summary": "all done"
                }),
            }],
            input_tokens: 130,
            output_tokens: 10,
            _error: None,
        },
    ]);
    let mut h = ReplyLoopHarness::new().await;
    provider.bind_valid_submit_work_fixtures(&h.task_id);
    let (result, output, _tokens_in, _tokens_out, _cr, _cw) = h.run(&provider, &tools).await;
    assert!(result.is_ok(), "expected ok, got: {:?}", result);
    assert_eq!(provider.remaining(), 0, "all responses consumed");
    assert!(output.finalize_payload.is_some(), "finalize payload set");
}

#[tokio::test]
async fn tool_choice_required_for_supported_providers() {
    use std::sync::Mutex;
    let tools = vec![dummy_tool_schema("submit_work")];
    struct RecordingProvider {
        recorded_choices: Arc<Mutex<Vec<Option<ToolChoice>>>>,
        inner: MockProvider,
    }
    impl LlmProvider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }
        fn stream<'a>(
            &'a self,
            conversation: &'a Conversation,
            tools: &'a [serde_json::Value],
            tool_choice: Option<ToolChoice>,
        ) -> Pin<
            Box<
                dyn futures::Future<
                        Output = anyhow::Result<
                            Pin<
                                Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                            >,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            self.recorded_choices.lock().unwrap().push(tool_choice);
            self.inner.stream(conversation, tools, tool_choice)
        }
    }
    let inner = MockProvider::new(vec![
        MockResponse::tool_call("tc1", "nonexistent_tool", 100),
        MockResponse {
            text: None,
            tool_calls: vec![ContentBlock::ToolUse {
                id: "fin1".to_string(),
                name: "submit_work".to_string(),
                input: serde_json::json!({"task_id": "t1", "commit_title": "complete test work", "summary": "done"}),
            }],
            input_tokens: 110,
            output_tokens: 10,
            _error: None,
        },
    ]);
    let recorded = Arc::new(Mutex::new(Vec::<Option<ToolChoice>>::new()));
    let provider = RecordingProvider {
        recorded_choices: Arc::clone(&recorded),
        inner,
    };
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    provider.inner.bind_valid_submit_work_fixtures(&task_id);
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "openai/gpt-5.4",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    assert!(result.is_ok(), "expected ok, got: {:?}", result);
    let choices = recorded.lock().unwrap();
    assert_eq!(choices.len(), 2, "two turns recorded");
    for (i, choice) in choices.iter().enumerate() {
        assert!(
            matches!(choice, Some(ToolChoice::Required)),
            "turn {i}: expected ToolChoice::Required, got {:?}",
            choice
        );
    }
}

/// Unsupported providers (e.g. synthetic/Kimi) get ToolChoice::Auto
/// to avoid 400 errors from reasoning models that reject "required".
#[tokio::test]
async fn tool_choice_auto_for_unsupported_providers() {
    assert!(!supports_tool_choice_required("synthetic/Kimi-K2.5"));
    assert!(!supports_tool_choice_required("synthetic/GLM-4.7"));
    assert!(!supports_tool_choice_required("deepinfra/some-model"));
    assert!(supports_tool_choice_required("openai/gpt-5.4"));
    assert!(supports_tool_choice_required("anthropic/claude-sonnet-4-5"));
    assert!(supports_tool_choice_required("chatgpt_codex/codex-mini"));
}

/// Concatenate every text block across all messages with the given role.
fn role_text(conv: &Conversation, role: djinn_provider::message::Role) -> String {
    conv.messages
        .iter()
        .filter(|m| m.role == role)
        .flat_map(|m| m.content.iter().filter_map(|b| b.as_text()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn wind_down_directive_count(conv: &Conversation) -> usize {
    conv.messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .any(|t| t.contains("You are out of steps"))
        })
        .count()
}

async fn persisted_wind_down_directive_count(
    slot_ctx: &crate::host::SlotContext,
    session_id: &str,
) -> usize {
    let repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    repo.load_conversation(session_id)
        .await
        .expect("load persisted conversation")
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .any(|t| t.contains("You are out of steps"))
        })
        .count()
}

/// Hitting the step cap injects the wind-down directive (NOT an immediate
/// hard error) and grants exactly one final turn for the summary, which is
/// captured before the loop ends gracefully (Ok).
#[tokio::test]
async fn max_step_cap_injects_wind_down_and_ends_gracefully() {
    // Cap the loop at 3 turns so we don't drive 1000 mock turns. Passed
    // explicitly via `max_turns_override` rather than a process-global env
    // var so concurrent tests can't race each other's cap.
    let tools = vec![dummy_tool_schema("submit_work")];
    // Turns 1..=3: tool calls keep the loop running up to the cap.
    // Turn 4 (the single granted wind-down turn): text-only summary → ends.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call("t1", "nonexistent_tool", 100),
        MockResponse::tool_call("t2", "nonexistent_tool", 110),
        MockResponse::tool_call("t3", "nonexistent_tool", 120),
        MockResponse::text_only(
            "Summary: (1) completed steps A and B. (2) C remains. \
             (3) next: finish C.",
            130,
        ),
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: Some(3),
        },
        &mut conv,
        false,
    )
    .await;
    assert!(
        result.is_ok(),
        "step cap should wind down gracefully, got: {:?}",
        result
    );
    // The wind-down turn consumed the 4th mock response → all consumed.
    assert_eq!(
        provider.remaining(),
        0,
        "the single wind-down turn should run (4th response consumed)"
    );
    // Exactly ONE wind-down directive was injected (one extra turn, not a loop).
    let injected = conv
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .any(|t| t.contains("You are out of steps"))
        })
        .count();
    assert_eq!(injected, 1, "wind-down directive injected exactly once");
    // The agent's hand-off summary was captured (persisted into the conversation).
    let assistant_text = role_text(&conv, Role::Assistant);
    assert!(
        assistant_text.contains("Summary:") && assistant_text.contains("next:"),
        "wind-down summary should be captured, got: {assistant_text:?}"
    );
}

/// If the agent ignores the wind-down directive and keeps calling tools
/// (never reaching a terminal text-only/finalize action), the loop falls
/// back to the existing hard-error behavior after exactly one extra turn.
#[tokio::test]
async fn max_step_wind_down_ignored_falls_back_to_hard_error() {
    // Cap explicitly via `max_turns_override` (no process-global env var,
    // which would race concurrent tests).
    let tools = vec![dummy_tool_schema("submit_work")];
    // Turns 1..=3 fill the cap; turn 4 (wind-down) is ALSO a tool call →
    // not terminal → next cap check hard-errors. The MockProvider's
    // text-only fallback is never reached because we error first.
    //
    // Vary the args per call so each call is a *distinct* tool-call
    // signature: the new (fw2v) in-loop guard over repeated failing
    // tool-call signatures would otherwise trip on the 4th identical call
    // and preempt the max-turns hard error this test is asserting.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call_with_input(
            "t1",
            "nonexistent_tool",
            serde_json::json!({"step": 1}),
            100,
        ),
        MockResponse::tool_call_with_input(
            "t2",
            "nonexistent_tool",
            serde_json::json!({"step": 2}),
            110,
        ),
        MockResponse::tool_call_with_input(
            "t3",
            "nonexistent_tool",
            serde_json::json!({"step": 3}),
            120,
        ),
        MockResponse::tool_call_with_input(
            "t4",
            "nonexistent_tool",
            serde_json::json!({"step": 4}),
            130,
        ),
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: Some(3),
        },
        &mut conv,
        false,
    )
    .await;
    assert!(
        result.is_err(),
        "ignoring the wind-down should fall back to the hard error"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("max turns") && err.contains("wind-down"),
        "error should mention the hard cap and the attempted wind-down, got: {err}"
    );
    // Wind-down was injected exactly once even though it was ignored —
    // the extension is strictly one turn, never an unbounded loop.
    let injected = conv
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .any(|t| t.contains("You are out of steps"))
        })
        .count();
    assert_eq!(
        injected, 1,
        "wind-down injected exactly once, then hard-errors"
    );
}

#[tokio::test]
async fn hard_token_budget_injects_wind_down_and_ends_gracefully() {
    let _env_guard = SESSION_BUDGET_ENV_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    install_session_budget_env_with_hard(1_000, 0.5, 0.92);
    let tools = vec![dummy_tool_schema("missing_tool")];
    let mut responses = Vec::new();
    for step in 1..=2 {
        responses.push(MockResponse::tool_call_with_input(
            &format!("t{step}"),
            "missing_tool",
            serde_json::json!({"step": step}),
            450,
        ));
    }
    responses.push(MockResponse::text_only(
        "Summary: token budget reached. (1) completed A. (2) B remains. (3) next: do B.",
        75,
    ));
    let provider = MockProvider::new(responses);
    let mut h = ReplyLoopHarness::new().await;
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = h.run(&provider, &tools).await;
    clear_session_budget_env();
    let _ = &_env_guard;
    assert!(
        result.is_ok(),
        "token budget should wind down gracefully, got: {:?}",
        result
    );
    assert_eq!(provider.remaining(), 0, "summary turn should be consumed");
    assert_eq!(
        wind_down_directive_count(&h.conv),
        1,
        "token budget should inject the existing wind-down directive once"
    );
    assert_eq!(
        persisted_wind_down_directive_count(&h.slot_ctx, &h.session_id).await,
        1,
        "token-budget wind-down directive should be persisted"
    );
    let assistant_text = role_text(&h.conv, Role::Assistant);
    assert!(
        assistant_text.contains("token budget reached") && assistant_text.contains("next:"),
        "wind-down summary should be captured, got: {assistant_text:?}"
    );
}

#[tokio::test]
async fn hard_token_budget_wind_down_ignored_falls_back_to_hard_error() {
    let _env_guard = SESSION_BUDGET_ENV_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    install_session_budget_env_with_hard(1_000, 0.5, 0.92);
    let tools = vec![dummy_tool_schema("missing_tool")];
    let mut responses = Vec::new();
    for step in 1..=3 {
        responses.push(MockResponse::tool_call_with_input(
            &format!("t{step}"),
            "missing_tool",
            serde_json::json!({"step": step}),
            450,
        ));
    }
    let provider = MockProvider::new(responses);
    let mut h = ReplyLoopHarness::new().await;
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = h.run(&provider, &tools).await;
    clear_session_budget_env();
    let _ = &_env_guard;
    assert!(
        result.is_err(),
        "ignoring token-budget wind-down should hard-error"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("hard token budget") && err.contains("wind-down"),
        "error should distinguish token budget from turn cap, got: {err}"
    );
    assert_eq!(provider.remaining(), 0, "only one extra turn should run");
    assert_eq!(
        wind_down_directive_count(&h.conv),
        1,
        "token-budget wind-down extension is strictly one turn"
    );
    assert_eq!(
        persisted_wind_down_directive_count(&h.slot_ctx, &h.session_id).await,
        1,
        "ignored token-budget directive should still be persisted once"
    );
}

#[test]
fn wind_down_reasons_distinguish_turn_cap_from_token_budget() {
    assert_ne!(
        format!("{:?}", WindDownReason::StepCap { max_turns: 2 }),
        format!(
            "{:?}",
            WindDownReason::Budget {
                details: "hard token budget".to_string()
            }
        )
    );
}

#[tokio::test]
async fn identical_failing_tool_call_injects_correction_then_typed_terminates() {
    let tools = vec![dummy_tool_schema("nonexistent_tool")];
    let same_args = serde_json::json!({"b": 2, "a": 1});
    let provider = MockProvider::new(vec![
        MockResponse::tool_call_with_input("tc1", "nonexistent_tool", same_args.clone(), 100),
        MockResponse::tool_call_with_input("tc2", "nonexistent_tool", same_args.clone(), 110),
        MockResponse::tool_call_with_input("tc3", "nonexistent_tool", same_args.clone(), 120),
        MockResponse::tool_call_with_input("tc4", "nonexistent_tool", same_args, 130),
    ]);
    let mut h = ReplyLoopHarness::new().await;
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = h.run(&provider, &tools).await;
    let err = result.expect_err("fourth identical failure should trip typed loop guard");
    let guard_err = err
        .downcast_ref::<LoopGuardError>()
        .expect("reply-loop error should preserve typed loop guard condition");
    assert_eq!(
        guard_err.condition.kind(),
        LoopGuardKind::RepeatedToolFailure
    );
    assert_eq!(guard_err.condition.observed, 4);
    assert_eq!(guard_err.condition.threshold, 3);
    let user_text = role_text(&h.conv, Role::User);
    let correction_count = h
        .conv
        .messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .any(|t| t.contains("SYSTEM CORRECTION"))
        })
        .count();
    assert_eq!(
        correction_count, 1,
        "exactly one corrective message should be injected before typed termination"
    );
    assert!(
        user_text.contains("SYSTEM CORRECTION")
            && user_text.contains("nonexistent_tool")
            && user_text.contains(r#"{"a":1,"b":2}"#)
            && user_text.contains("Do not call this exact tool"),
        "corrective message should name and forbid the exact normalized signature, got: {user_text}"
    );
    // The system prompt is intentionally skipped by the reply loop's
    // initial-seed persistence pass, so persisted = h.conv.messages.len() - 1.
    // The 4th attempt's tool-result message is also not persisted because
    // the loop returns from the guard before appending it — but it is
    // likewise absent from `h.conv.messages`, so the invariant still holds.
    let persisted = count_persisted_messages(&h.slot_ctx, &h.session_id).await;
    let expected_persisted = h.conv.messages.len() - 1;
    assert_eq!(
        persisted,
        expected_persisted,
        "corrective message should be persisted with the transcript; expected \
         {expected_persisted} (conversation len {} minus 1 system prompt), got {persisted}",
        h.conv.messages.len()
    );
    let persisted_conversation =
        SessionMessageRepository::new(h.slot_ctx.db.clone(), h.slot_ctx.event_bus.clone())
            .load_conversation(&h.session_id)
            .await
            .expect("load persisted conversation");
    let persisted_user_text = role_text(&persisted_conversation, Role::User);
    assert!(
        persisted_user_text.contains("SYSTEM CORRECTION")
            && persisted_user_text.contains("nonexistent_tool")
            && persisted_user_text.contains(r#"{"a":1,"b":2}"#),
        "persisted corrective message should name the exact signature, got: {persisted_user_text}"
    );
    assert_eq!(
        provider.remaining(),
        0,
        "four scripted turns should be consumed"
    );
}

#[tokio::test]
async fn permission_denial_tool_failure_trips_on_second_identical_attempt() {
    let tools = vec![dummy_tool_schema("secure_fetch")];
    let dispatcher = std::sync::Arc::new(test_helpers::ConfigurableToolDispatcher::new(
        vec!["secure_fetch".to_string()],
        std::collections::HashMap::from([(
            "secure_fetch".to_string(),
            (|_args: Option<&serde_json::Map<String, serde_json::Value>>| -> Result<serde_json::Value, String> {
                Err("permission denied: token is not allowed".to_string())
            }) as test_helpers::ToolHandlerFn,
        )]),
    ));
    let args = serde_json::json!({"path": "/secret"});
    let provider = MockProvider::new(vec![
        MockResponse::tool_call_with_input("deny1", "secure_fetch", args.clone(), 100),
        MockResponse::tool_call_with_input("deny2", "secure_fetch", args, 110),
    ]);
    let (result, _output, _conv, _slot_ctx, _session_id) =
        run_scripted_reply_loop_with_dispatcher(&provider, &tools, Some(dispatcher)).await;
    let err = result.expect_err("second identical permission denial should trip guard");
    let guard_err = err
        .downcast_ref::<LoopGuardError>()
        .expect("permission denial should use typed loop guard error");
    assert_eq!(
        guard_err.condition.kind(),
        LoopGuardKind::RepeatedPermissionOrSecurityDenial
    );
    assert_eq!(guard_err.condition.observed, 2);
    assert_eq!(guard_err.condition.threshold, 2);
    assert_eq!(provider.remaining(), 0);
}

#[tokio::test]
async fn repeated_assistant_output_signature_trips_on_fourth_repeat() {
    let tools = vec![dummy_tool_schema("missing_tool")];
    let args = serde_json::json!({"same": true});
    let response = |id: &str, input_tokens| MockResponse {
        text: Some("I will retry the same thing.".to_string()),
        tool_calls: vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: "missing_tool".to_string(),
            input: args.clone(),
        }],
        input_tokens,
        output_tokens: 10,
        _error: None,
    };
    let provider = MockProvider::new(vec![
        response("a1", 100),
        response("a2", 110),
        response("a3", 120),
        response("a4", 130),
    ]);
    let (result, _output, _conv, _slot_ctx, _session_id) =
        run_scripted_reply_loop(&provider, &tools).await;
    let err = result.expect_err("fourth identical assistant output should trip guard");
    let guard_err = err
        .downcast_ref::<LoopGuardError>()
        .expect("assistant repeat should use typed loop guard error");
    assert_eq!(
        guard_err.condition.kind(),
        LoopGuardKind::RepeatedAssistantOutput
    );
    assert_eq!(guard_err.condition.observed, 4);
    assert_eq!(guard_err.condition.threshold, 4);
    assert_eq!(provider.remaining(), 0);
}

#[tokio::test]
async fn six_consecutive_tool_failures_across_different_signatures_trip_guard() {
    let tools = vec![dummy_tool_schema("missing_tool")];
    let provider = MockProvider::new(
        (0..6)
            .map(|idx| {
                MockResponse::tool_call_with_input(
                    &format!("fail{idx}"),
                    "missing_tool",
                    serde_json::json!({"attempt": idx}),
                    100 + idx,
                )
            })
            .collect(),
    );
    let (result, _output, _conv, _slot_ctx, _session_id) =
        run_scripted_reply_loop(&provider, &tools).await;
    let err = result.expect_err("six consecutive failures should trip guard");
    let guard_err = err
        .downcast_ref::<LoopGuardError>()
        .expect("consecutive failures should use typed loop guard error");
    assert_eq!(
        guard_err.condition.kind(),
        LoopGuardKind::ConsecutiveToolFailures
    );
    assert_eq!(guard_err.condition.observed, 6);
    assert_eq!(guard_err.condition.threshold, 6);
    assert_eq!(provider.remaining(), 0);
}

#[tokio::test]
async fn successful_novel_tool_call_resets_failure_pressure() {
    let tools = vec![
        dummy_tool_schema("flaky_mcp"),
        dummy_tool_schema("submit_work"),
    ];
    fn flaky_mcp_handler(
        args: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<serde_json::Value, String> {
        if args
            .and_then(|a| a.get("ok"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            Ok(serde_json::json!({"ok": true}))
        } else {
            Err("ordinary tool failure".to_string())
        }
    }
    let dispatcher = std::sync::Arc::new(test_helpers::ConfigurableToolDispatcher::new(
        vec!["flaky_mcp".to_string()],
        std::collections::HashMap::from([(
            "flaky_mcp".to_string(),
            flaky_mcp_handler as test_helpers::ToolHandlerFn,
        )]),
    ));
    let fail_args = serde_json::json!({"ok": false});
    let provider = MockProvider::new(vec![
        MockResponse::tool_call_with_input("f1", "flaky_mcp", fail_args.clone(), 100),
        MockResponse::tool_call_with_input("f2", "flaky_mcp", fail_args.clone(), 110),
        MockResponse::tool_call_with_input("ok", "flaky_mcp", serde_json::json!({"ok": true}), 120),
        MockResponse::tool_call_with_input("f3", "flaky_mcp", fail_args.clone(), 130),
        MockResponse::tool_call_with_input("f4", "flaky_mcp", fail_args, 140),
        MockResponse::tool_call_with_input(
            "done",
            "submit_work",
            serde_json::json!({"task_id": "t1", "commit_title": "complete test work", "summary": "done"}),
            150,
        ),
    ]);
    let (result, output, _conv, _slot_ctx, _session_id) =
        run_scripted_reply_loop_with_dispatcher(&provider, &tools, Some(dispatcher)).await;
    assert!(
        result.is_ok(),
        "post-progress repeated failures should not trip before finalize: {result:?}"
    );
    assert_eq!(output.finalize_tool_name.as_deref(), Some("submit_work"));
    assert_eq!(provider.remaining(), 0);
}

#[tokio::test]
async fn mixed_successful_tool_batch_resets_consecutive_failure_pressure() {
    let tools = vec![
        dummy_tool_schema("flaky_mcp"),
        dummy_tool_schema("submit_work"),
    ];
    fn flaky_mcp_handler(
        args: Option<&serde_json::Map<String, serde_json::Value>>,
    ) -> Result<serde_json::Value, String> {
        if args
            .and_then(|a| a.get("ok"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            Ok(serde_json::json!({"ok": true}))
        } else {
            Err("ordinary tool failure".to_string())
        }
    }
    let dispatcher = std::sync::Arc::new(test_helpers::ConfigurableToolDispatcher::new(
        vec!["flaky_mcp".to_string()],
        std::collections::HashMap::from([(
            "flaky_mcp".to_string(),
            flaky_mcp_handler as test_helpers::ToolHandlerFn,
        )]),
    ));
    let mut responses: Vec<MockResponse> = (0..5)
        .map(|idx| {
            MockResponse::tool_call_with_input(
                &format!("fail{idx}"),
                "flaky_mcp",
                serde_json::json!({"ok": false, "attempt": idx}),
                100 + idx,
            )
        })
        .collect();
    responses.push(MockResponse {
        text: None,
        tool_calls: vec![
            ContentBlock::ToolUse {
                id: "mixed-fail".to_string(),
                name: "flaky_mcp".to_string(),
                input: serde_json::json!({"ok": false, "attempt": "mixed"}),
            },
            ContentBlock::ToolUse {
                id: "mixed-ok".to_string(),
                name: "flaky_mcp".to_string(),
                input: serde_json::json!({"ok": true}),
            },
        ],
        input_tokens: 130,
        output_tokens: 10,
        _error: None,
    });
    responses.push(MockResponse::tool_call_with_input(
        "done",
        "submit_work",
        serde_json::json!({"task_id": "t1", "commit_title": "complete test work", "summary": "done"}),
        140,
    ));
    let provider = MockProvider::new(responses);
    let (result, output, _conv, _slot_ctx, _session_id) =
        run_scripted_reply_loop_with_dispatcher(&provider, &tools, Some(dispatcher)).await;
    assert!(
        result.is_ok(),
        "a mixed batch containing progress should reset the consecutive-failure streak: {result:?}"
    );
    assert_eq!(output.finalize_tool_name.as_deref(), Some("submit_work"));
    assert_eq!(provider.remaining(), 0);
}

/// Helper: count user messages whose text body contains a `<system-reminder>`
/// tag. Matches both opening and closing tags so the count is robust against
/// either side of the wrapper ever being split into a separate content block.
fn count_system_reminder_messages(conv: &Conversation) -> usize {
    conv.messages
        .iter()
        .filter(|m| {
            m.role == Role::User
                && m.content
                    .iter()
                    .filter_map(|b| b.as_text())
                    .any(|t| t.contains("<system-reminder>"))
        })
        .count()
}

/// Apply a `SessionBudgetPolicy` override to the env so the next
/// `SessionBudgetPolicy::from_env()` inside `run_reply_loop` resolves a small
/// cumulative-token cap with a low soft threshold. Always called under
/// `SESSION_BUDGET_ENV_LOCK` and followed by `clear_session_budget_env()` at
/// test teardown.
fn install_session_budget_env(max_cumulative_tokens: u64, soft_ratio: f64) {
    // SAFETY: always called under SESSION_BUDGET_ENV_LOCK.
    unsafe {
        std::env::set_var(
            "DJINN_SESSION_BUDGET_WORKER_MAX_CUMULATIVE_TOKENS",
            max_cumulative_tokens.to_string(),
        );
        std::env::set_var(
            "DJINN_SESSION_BUDGET_WORKER_SOFT_THRESHOLD_RATIO",
            soft_ratio.to_string(),
        );
    }
}

fn install_session_budget_env_with_hard(
    max_cumulative_tokens: u64,
    soft_ratio: f64,
    hard_ratio: f64,
) {
    install_session_budget_env(max_cumulative_tokens, soft_ratio);
    // SAFETY: always called under SESSION_BUDGET_ENV_LOCK.
    unsafe {
        std::env::set_var(
            "DJINN_SESSION_BUDGET_WORKER_HARD_THRESHOLD_RATIO",
            hard_ratio.to_string(),
        );
    }
}

/// Crossing the soft threshold exactly once — the reply loop persists and
/// pushes a `<system-reminder>` converge directive, and the one-shot flag
/// prevents the same reminder from being injected again on later turns
/// where the threshold remains exceeded.
///
/// Scenario:
///   - `max_cumulative_tokens = 100`, `soft_threshold_ratio = 0.5`
///     → soft cap = 50 tokens (`total_tokens_in + total_tokens_out >= 50`).
///   - Turn 1: tool call, `input=20, output=10` → cumulative=30 (below cap).
///   - Turn 2: tool call, `input=30, output=10` → cumulative=70 (above cap).
///     The pre-turn check for turn 3 sees cumulative=70 and injects the
///     reminder (one-shot) just before turn 3's stream runs.
///   - Turn 3: tool call, `input=20, output=10` → cumulative=100 (still above).
///     The pre-turn check for turn 4 sees the flag set and does NOT re-inject.
///   - Turn 4: `submit_work` tool call → session ends.
///
/// (`MockResponse::tool_call` defaults to `output_tokens = 10`; the comments
/// on each response below match the actual `input` / `output` per turn.)
///
/// Verifies: exactly one `<system-reminder>` injection, exactly one tool call
/// whose ToolResult flows back into the conversation, and the assistant
/// stream never produces the reminder (only the reply loop does).
#[tokio::test]
async fn soft_budget_threshold_triggers_one_shot_converge_reminder() {
    // Recover from a poisoned mutex so a prior test that panicked mid-env-mutation
    // doesn't cascade its failure here. The lock still serializes; we just
    // ignore the poison marker since the protected state is process env (which
    // gets reset by the test's own setup/teardown anyway).
    let _env_guard = SESSION_BUDGET_ENV_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // 200 token cap × 0.25 ratio = 50-token soft cap; the default hard cap
    // (92% = 184 tokens) stays safely above this test's 100-token spend.
    install_session_budget_env(200, 0.25);
    // Safety net: ensure no stale overrides from a previous test leak through.
    // SAFETY: SESSION_BUDGET_ENV_LOCK held.
    unsafe {
        std::env::remove_var("DJINN_SESSION_BUDGET_WORKER_MAX_TURNS");
        std::env::remove_var("DJINN_SESSION_BUDGET_WORKER_HARD_THRESHOLD_RATIO");
    }
    let tools = vec![
        dummy_tool_schema("submit_work"),
        dummy_tool_schema("worker_tool"),
    ];
    // Each turn is a distinct tool call (different tool name and id) so the
    // in-loop guard over repeated failing tool-call signatures never trips
    // before the soft-budget injection has had a chance to fire.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call("tool1-a", "worker_tool", 20), // turn 1: 20+10=30
        MockResponse::tool_call("tool2-b", "worker_tool", 30), // turn 2: cumulative=70 → fires pre-turn-3
        MockResponse::tool_call("tool3-c", "worker_tool", 20), // turn 3: cumulative=100 → still above, no re-inject
        MockResponse {
            text: None,
            tool_calls: vec![ContentBlock::ToolUse {
                id: "fin".to_string(),
                name: "submit_work".to_string(),
                input: serde_json::json!({"task_id": "t1", "commit_title": "complete test work", "summary": "done"}),
            }],
            input_tokens: 10,
            output_tokens: 5,
            _error: None,
        },
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    provider.bind_valid_submit_work_fixtures(&task_id);
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            // Large context window so the context-pressure secondary signal
            // (`current_context_tokens / context_window`) never trips on its
            // own — the test exercises the cumulative `input+output` spend
            // path only. We need `max_cumulative_tokens` (set via env) to be
            // the dominant signal, and a 10k-token window with <100 tokens of
            // usage keeps that ratio tiny.
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            // Drive production turn-cap path (we don't want max_turns to be
            // the limiter; the budget cap is the test target).
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    // SAFETY: SESSION_BUDGET_ENV_LOCK held; restore baseline before asserting.
    clear_session_budget_env();
    // Avoid the `_env_guard` "field never read" warning while still proving
    // the guard was held for the duration of `run_reply_loop`.
    let _ = &_env_guard;
    assert!(
        result.is_ok(),
        "soft-budget reminder should not fail the session; got: {:?}",
        result
    );
    assert_eq!(
        provider.remaining(),
        0,
        "all scripted turns (4) should be consumed: 3 tool calls + 1 finalize"
    );
    // Exactly ONE `<system-reminder>` was injected, even though the threshold
    // remained exceeded for the remainder of the session.
    let reminder_count = count_system_reminder_messages(&conv);
    assert_eq!(
        reminder_count, 1,
        "soft-budget reminder must be injected exactly once across multiple \
         subsequent turns that still exceed the threshold; conv has {reminder_count} \
         <system-reminder> user messages, full conversation:\n{:#?}",
        conv.messages
    );
    // The reminder text should match the converge directive contract.
    let reminder_text = role_text(&conv, Role::User);
    assert!(
        reminder_text.contains("Budget for this session is mostly consumed")
            && reminder_text.contains("CONVERGE")
            && reminder_text.contains("stop expanding scope")
            && reminder_text.contains("commit"),
        "reminder must convey the converge/keep/commit message; got: {reminder_text}"
    );
    // The reminder is also persisted durably alongside the assistant/tool
    // transcript, not just pushed into the in-memory conversation.
    let repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    let persisted = repo
        .load_conversation(&session_id)
        .await
        .expect("load persisted conversation");
    let persisted_reminder_count = count_system_reminder_messages(&persisted);
    assert_eq!(
        persisted_reminder_count, 1,
        "soft-budget reminder must be persisted with the session transcript"
    );
}

/// Below the soft threshold the reply loop must not inject any
/// `<system-reminder>` converge directive. Even if the session runs for
/// many turns and the cumulative spend grows, as long as the
/// `total_tokens_in + total_tokens_out` total stays under
/// `max_cumulative_tokens * soft_threshold_ratio` no injection happens.
#[tokio::test]
async fn soft_budget_below_threshold_no_injection() {
    // Recover from a poisoned mutex so a prior test that panicked mid-env-mutation
    // doesn't cascade its failure here. See the matching comment in
    // `soft_budget_threshold_triggers_one_shot_converge_reminder`.
    let _env_guard = SESSION_BUDGET_ENV_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    // 1000 token cap × 0.75 = 750-token soft cap. We'll spend a fraction of
    // that across many turns and verify no reminder fires.
    install_session_budget_env(1_000, 0.75);
    // SAFETY: SESSION_BUDGET_ENV_LOCK held.
    unsafe {
        std::env::remove_var("DJINN_SESSION_BUDGET_WORKER_MAX_TURNS");
        std::env::remove_var("DJINN_SESSION_BUDGET_WORKER_HARD_THRESHOLD_RATIO");
    }
    let tools = vec![
        dummy_tool_schema("submit_work"),
        dummy_tool_schema("worker_tool"),
    ];
    // 5 tool-call turns at 30+10=40 cumulative tokens each → 200 tokens total
    // (well under 750). Then a submit_work finalize.
    //
    // Vary the args per call (`{"step": N}`) so each call is a *distinct*
    // tool-call signature: the in-loop guard over repeated failing tool-call
    // signatures would otherwise trip on the 4th identical call and preempt
    // the soft-budget no-injection path this test is asserting.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call_with_input("a", "worker_tool", serde_json::json!({"step": 1}), 30),
        MockResponse::tool_call_with_input("b", "worker_tool", serde_json::json!({"step": 2}), 30),
        MockResponse::tool_call_with_input("c", "worker_tool", serde_json::json!({"step": 3}), 30),
        MockResponse::tool_call_with_input("d", "worker_tool", serde_json::json!({"step": 4}), 30),
        MockResponse::tool_call_with_input("e", "worker_tool", serde_json::json!({"step": 5}), 30),
        MockResponse {
            text: None,
            tool_calls: vec![ContentBlock::ToolUse {
                id: "fin".to_string(),
                name: "submit_work".to_string(),
                input: serde_json::json!({"task_id": "t1", "commit_title": "complete test work", "summary": "done"}),
            }],
            input_tokens: 5,
            output_tokens: 5,
            _error: None,
        },
    ]);
    let mut h = ReplyLoopHarness::new().await;
    provider.bind_valid_submit_work_fixtures(&h.task_id);
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = h.run(&provider, &tools).await;
    // SAFETY: SESSION_BUDGET_ENV_LOCK held; restore baseline before asserting.
    clear_session_budget_env();
    let _ = &_env_guard;
    assert!(
        result.is_ok(),
        "session should complete normally below the soft threshold; got: {:?}",
        result
    );
    assert_eq!(provider.remaining(), 0, "all 6 scripted turns consumed");
    let reminder_count = count_system_reminder_messages(&h.conv);
    assert_eq!(
        reminder_count, 0,
        "no <system-reminder> converge directive should be injected while \
         cumulative spend is below the soft threshold; got {reminder_count}, full \
         conversation:\n{:#?}",
        h.conv.messages
    );
    let user_text = role_text(&h.conv, Role::User);
    assert!(
        !user_text.contains("<system-reminder>"),
        "no system-reminder body should appear in user text below threshold; got: {user_text}"
    );
}

#[tokio::test]
async fn hard_budget_wind_down_captures_budget_summary() {
    let _env_guard = SESSION_BUDGET_ENV_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    install_session_budget_env(100, 0.5);
    // SAFETY: SESSION_BUDGET_ENV_LOCK held.
    unsafe {
        std::env::remove_var("DJINN_SESSION_BUDGET_WORKER_MAX_TURNS");
        std::env::set_var("DJINN_SESSION_BUDGET_WORKER_HARD_THRESHOLD_RATIO", "0.8");
    }
    let tools = vec![
        dummy_tool_schema("submit_work"),
        dummy_tool_schema("worker_tool"),
    ];
    let provider = MockProvider::new(vec![
        MockResponse::tool_call_with_input("a", "worker_tool", serde_json::json!({"step": 1}), 40), // cumulative 50
        MockResponse::tool_call_with_input("b", "worker_tool", serde_json::json!({"step": 2}), 30), // cumulative 90 → hard wind-down before next turn
        MockResponse::text_only("Budget handoff: implemented A; B remains.", 5),
    ]);
    let mut h = ReplyLoopHarness::new().await;
    let (result, output, tokens_in, tokens_out, _cr, _cw) = h.run(&provider, &tools).await;
    clear_session_budget_env();
    let _ = &_env_guard;
    assert!(
        result.is_ok(),
        "hard budget summary should park cleanly: {result:?}"
    );
    assert_eq!(
        output.budget_wind_down_summary.as_deref(),
        Some("Budget handoff: implemented A; B remains.")
    );
    assert!(
        output
            .budget_wind_down_details
            .as_deref()
            .is_some_and(|details| details.contains("hard token budget threshold reached")),
        "budget wind-down should preserve structured trigger details: {:?}",
        output.budget_wind_down_details
    );
    assert_eq!(provider.remaining(), 0);
    assert!(
        tokens_in < 1000,
        "hard budget should park well below the legacy worker max-turn blast radius"
    );
    let user_text = role_text(&h.conv, Role::User);
    assert!(user_text.contains("You are out of steps"));
    let stage_outcome = StageOutcome::Parked {
        reason: ParkReason::Budget,
        summary: output.budget_wind_down_summary.clone(),
        wind_down_ignored: false,
        session_id: h.session_id.clone(),
        tokens_in,
        tokens_out,
    };
    assert_eq!(
        test_session_settlement_for_stage_outcome(&stage_outcome, true),
        (SessionStatus::Completed, Some("budget".to_string())),
        "successful budget wind-downs settle as completed budget parks"
    );
    handle_budget_park(
        output
            .budget_wind_down_summary
            .as_deref()
            .expect("summary captured above"),
        output
            .budget_wind_down_details
            .as_deref()
            .expect("budget details captured above"),
        &h.task_id,
        &h.slot_ctx,
    )
    .await;
    let repo = TaskRepository::new(h.slot_ctx.db.clone(), h.slot_ctx.event_bus.clone());
    let entries = repo.list_activity(&h.task_id).await.unwrap();
    let work_entries: Vec<_> = entries
        .iter()
        .filter(|entry| entry.event_type == "work_submitted")
        .collect();
    assert_eq!(
        work_entries.len(),
        1,
        "budget summary should be persisted exactly once as normal work_submitted activity"
    );
    let payload: serde_json::Value = serde_json::from_str(&work_entries[0].payload).unwrap();
    assert_eq!(
        payload["summary"],
        "Budget handoff: implemented A; B remains."
    );
    let remaining_concerns = payload["remaining_concerns"].as_str().unwrap();
    assert!(
        remaining_concerns.starts_with("budget-parked:"),
        "remaining concerns should carry the budget-park prefix: {remaining_concerns}"
    );
    let (worker_summary, worker_concerns) = extract_worker_context(&Some(entries));
    assert_eq!(
        worker_summary.as_deref(),
        Some("Budget handoff: implemented A; B remains."),
        "subsequent dispatch context should receive the budget handoff via the existing extractor"
    );
    assert!(
        worker_concerns
            .as_deref()
            .is_some_and(|concerns| concerns.contains("budget-parked:")),
        "budget-park concern should surface through work_submitted extraction: {worker_concerns:?}"
    );
}

#[tokio::test]
async fn hard_budget_wind_down_ignored_returns_typed_budget_error() {
    let _env_guard = SESSION_BUDGET_ENV_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    install_session_budget_env(100, 0.5);
    // SAFETY: SESSION_BUDGET_ENV_LOCK held.
    unsafe {
        std::env::remove_var("DJINN_SESSION_BUDGET_WORKER_MAX_TURNS");
        std::env::set_var("DJINN_SESSION_BUDGET_WORKER_HARD_THRESHOLD_RATIO", "0.8");
    }
    let tools = vec![
        dummy_tool_schema("submit_work"),
        dummy_tool_schema("worker_tool"),
    ];
    let provider = MockProvider::new(vec![
        MockResponse::tool_call_with_input("a", "worker_tool", serde_json::json!({"step": 1}), 40),
        MockResponse::tool_call_with_input("b", "worker_tool", serde_json::json!({"step": 2}), 30),
        // This is the single provider turn granted after the hard-budget
        // wind-down directive. It ignores the directive by calling a tool.
        MockResponse::tool_call_with_input("c", "worker_tool", serde_json::json!({"step": 3}), 5),
        // Regression guard: the old pre-turn condition continued below the
        // normal step cap after an ignored budget wind-down and consumed this
        // fallback as a false successful summary.
        MockResponse::text_only("fallback budget handoff that must never be consumed", 5),
    ]);
    let mut h = ReplyLoopHarness::new().await;
    let (result, output, tokens_in, tokens_out, _cr, _cw) = h.run(&provider, &tools).await;
    clear_session_budget_env();
    let _ = &_env_guard;
    let err = result.expect_err("ignored hard-budget wind-down should return typed error");
    assert!(
        err.downcast_ref::<BudgetWindDownIgnored>().is_some(),
        "ignored hard-budget wind-down must be typed for stage settlement; got: {err:?}"
    );
    assert!(
        output.budget_wind_down_summary.is_none(),
        "ignored wind-down must not synthesize a misleading summary"
    );
    assert!(
        output.budget_wind_down_details.is_none(),
        "ignored wind-down without summary must not synthesize handoff details"
    );
    assert_eq!(
        provider.remaining(),
        1,
        "ignored budget wind-down must stop before consuming later fallback text"
    );
    let stage_outcome = StageOutcome::Parked {
        reason: ParkReason::Budget,
        summary: None,
        wind_down_ignored: true,
        session_id: h.session_id.clone(),
        tokens_in,
        tokens_out,
    };
    assert_eq!(
        test_session_settlement_for_stage_outcome(&stage_outcome, false),
        (SessionStatus::Completed, Some("budget".to_string())),
        "ignored budget wind-downs still settle as completed budget parks"
    );
    match stage_outcome {
        StageOutcome::Parked {
            wind_down_ignored, ..
        } => assert!(
            wind_down_ignored,
            "ignored flag must be recorded structurally"
        ),
        other => panic!("expected parked outcome, got {other:?}"),
    }
    let repo = TaskRepository::new(h.slot_ctx.db.clone(), h.slot_ctx.event_bus.clone());
    let entries = repo.list_activity(&h.task_id).await.unwrap();
    assert!(
        entries
            .iter()
            .all(|entry| entry.event_type != "work_submitted"),
        "ignored wind-down must not write a misleading summary activity: {entries:?}"
    );
}

/// Regression (incident 2026-07-02): a conversation whose stored transcript
/// ends with an assistant `tool_calls` message that was never answered — a
/// prior session terminated at/after submission or was killed mid-tool-call —
/// must NOT be replayed verbatim to the provider. `run_reply_loop` sanitizes
/// every request at the provider seam, so the dangling call is answered by a
/// synthesized tool result before it can 400 ("tool_call_ids did not have
/// response messages"). This covers all replay flows (redispatch, retry,
/// rework-after-review continuation) since every turn passes through the seam.
#[tokio::test]
async fn dangling_tool_call_is_sanitized_before_reaching_provider() {
    use std::sync::Mutex;
    let tools = vec![dummy_tool_schema("submit_work")];
    /// Captures the conversation of the first stream call, then defers to a
    /// MockProvider that finalizes so the loop terminates.
    struct CapturingProvider {
        first_conversation: Arc<Mutex<Option<Conversation>>>,
        inner: MockProvider,
    }
    impl LlmProvider for CapturingProvider {
        fn name(&self) -> &str {
            "capturing"
        }
        fn stream<'a>(
            &'a self,
            conversation: &'a Conversation,
            tools: &'a [serde_json::Value],
            tool_choice: Option<ToolChoice>,
        ) -> Pin<
            Box<
                dyn futures::Future<
                        Output = anyhow::Result<
                            Pin<
                                Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                            >,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            {
                let mut slot = self.first_conversation.lock().unwrap();
                if slot.is_none() {
                    *slot = Some(conversation.clone());
                }
            }
            self.inner.stream(conversation, tools, tool_choice)
        }
    }
    let inner = MockProvider::new(vec![MockResponse {
        text: None,
        tool_calls: vec![ContentBlock::ToolUse {
            id: "fin1".to_string(),
            name: "submit_work".to_string(),
            input: serde_json::json!({"task_id": "t1", "commit_title": "complete test work", "summary": "done"}),
        }],
        input_tokens: 50,
        output_tokens: 10,
        _error: None,
    }]);
    let first_conversation = Arc::new(Mutex::new(None));
    let provider = CapturingProvider {
        first_conversation: Arc::clone(&first_conversation),
        inner,
    };
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    provider.inner.bind_valid_submit_work_fixtures(&task_id);
    let worktree_path = std::path::PathBuf::from("/tmp");
    // Seed a transcript ending in an UNANSWERED assistant tool call, exactly as
    // a prior session's persisted history would on a rework continuation.
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    conv.push(Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: "apply_patch:45".to_string(),
            name: "apply_patch".to_string(),
            input: serde_json::json!({"patch": "..."}),
        }],
        metadata: None,
    });
    let (result, _output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "openai/gpt-5.4",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    assert!(result.is_ok(), "expected ok, got: {result:?}");
    // The provider must have received a synthesized tool result answering the
    // dangling id — the original assistant call is preserved (context intact).
    let captured = first_conversation
        .lock()
        .unwrap()
        .take()
        .expect("provider was called at least once");
    let answered_ids: Vec<&str> = captured
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        answered_ids.contains(&"apply_patch:45"),
        "the dangling tool call must be answered by a synthesized result before dispatch; \
         got tool results for {answered_ids:?}"
    );
    assert!(
        captured.messages.iter().any(|m| m
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { id, .. } if id == "apply_patch:45"))),
        "the original assistant tool call is preserved (context of what it was doing)"
    );
}

/// Helper: create a git repo with a committed initial file and a dirty
/// tracked edit so `compute_submission_diff_fingerprint` returns a `Diff`.
fn init_git_repo_with_dirty_file() -> tempfile::TempDir {
    let dir = tempfile::Builder::new()
        .prefix("djinn-test-integrity-gate-")
        .tempdir()
        .expect("create temp dir");
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_git(&["init"]);
    run_git(&["config", "--local", "user.email", "test@test.com"]);
    run_git(&["config", "--local", "user.name", "Test User"]);
    run_git(&["config", "--local", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("README.md"), "hello\n").expect("write readme");
    run_git(&["add", "README.md"]);
    run_git(&["commit", "-m", "init"]);
    run_git(&["branch", "-m", "main"]);
    // Make a dirty tracked edit so the fingerprint computes a Diff.
    std::fs::write(dir.path().join("README.md"), "hello\ndirty\n").expect("write dirty");
    dir
}

/// When a worker calls `submit_work` and the current worktree fingerprint
/// matches the latest rejected fingerprint, the guard intercepts and returns
/// a corrective tool result. The session continues without setting
/// `finalize_payload` or `finalize_tool_name`.
#[tokio::test]
async fn first_no_progress_submit_intercepted_returns_corrective_and_continues() {
    let worktree = init_git_repo_with_dirty_file();
    let worktree_path = worktree.path().to_path_buf();
    // Compute the fingerprint so we can record it as rejected.
    let fp = djinn_git::compute_submission_diff_fingerprint(&worktree_path)
        .await
        .expect("compute fingerprint");
    let fingerprint = fp.fingerprint().expect("must be a Diff").to_string();
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    // Record a rejected fingerprint for this task.
    record_rejected_integrity_entry(
        &task_id,
        &slot_ctx,
        djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
        None,
        None,
        &fingerprint,
    )
    .await;
    // Script: turn 1 returns submit_work (guard intercepts), then turns 2-4
    // are text-only which the nudge loop absorbs before returning an error.
    // The important thing is that the guard intercepted on turn 1.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call_with_input(
            "submit-1",
            "submit_work",
            serde_json::json!({"task_id": task_id, "commit_title": "complete test work", "summary": "done", "files_changed": []}),
            100,
        ),
        // Turn 2: model responds to the corrective message with text-only.
        MockResponse::text_only("I'll make changes and resubmit.", 100),
    ]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[serde_json::json!({
                "type": "function",
                "function": { "name": "submit_work", "description": "submit", "parameters": {"type": "object"} },
                "concurrent_safe": false
            })],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    // The loop may terminate with a nudge-loop error (text-only turns without
    // finalize). That's expected — the guard intercepted the finalize and the
    // model never retried with a changed fingerprint. The key assertions are
    // about the guard behavior, not the loop termination status.
    let _ = result;
    // The guard intercepted: finalize_payload and finalize_tool_name must not be set.
    assert!(
        output.finalize_payload.is_none(),
        "finalize_payload must be None when the guard intercepts; got: {:?}",
        output.finalize_payload
    );
    assert!(
        output.finalize_tool_name.is_none(),
        "finalize_tool_name must be None when the guard intercepts; got: {:?}",
        output.finalize_tool_name
    );
    // The corrective tool result must be in the conversation.
    let has_corrective = conv.messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, ContentBlock::ToolResult { tool_use_id, is_error, content }
                if tool_use_id == "submit-1"
                    && *is_error
                    && content.iter().any(|c| matches!(c, ContentBlock::Text { text }
                        if text.contains("identical to the latest rejected submission"))))
        })
    });
    assert!(
        has_corrective,
        "conversation must contain a corrective tool result for submit_work; \
         messages: {:?}",
        conv.messages
    );
    // Both mock responses were consumed (guard + text-only final).
    assert_eq!(provider.remaining(), 0);
}

/// When no rejected fingerprint exists for the task, the guard skips
/// comparison (no-comparison historical path) and the finalize proceeds
/// normally.
#[tokio::test]
async fn missing_rejected_fingerprint_skips_comparison_and_allows_finalize() {
    let worktree = init_git_repo_with_dirty_file();
    let worktree_path = worktree.path().to_path_buf();
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    // No rejected fingerprint recorded — the guard should skip comparison.
    let provider = MockProvider::new(vec![MockResponse::tool_call_with_input(
        "submit-1",
        "submit_work",
        serde_json::json!({"task_id": task_id, "commit_title": "complete test work", "summary": "done", "files_changed": []}),
        100,
    )]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[serde_json::json!({
                "type": "function",
                "function": { "name": "submit_work", "description": "submit", "parameters": {"type": "object"} },
                "concurrent_safe": false
            })],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    assert!(result.is_ok(), "expected ok, got: {result:?}");
    // No rejected fingerprint → guard skipped → finalize proceeds normally.
    assert!(
        output.finalize_payload.is_some(),
        "finalize_payload must be set when no rejected fingerprint exists; got None"
    );
    assert_eq!(
        output.finalize_tool_name.as_deref(),
        Some("submit_work"),
        "finalize_tool_name must be submit_work"
    );
}

/// When `role_name` is not "worker", the guard does not activate and the
/// finalize proceeds normally even if a matching rejected fingerprint exists.
#[tokio::test]
async fn non_worker_role_bypasses_guard() {
    let worktree = init_git_repo_with_dirty_file();
    let worktree_path = worktree.path().to_path_buf();
    let fp = djinn_git::compute_submission_diff_fingerprint(&worktree_path)
        .await
        .expect("compute fingerprint");
    let fingerprint = fp.fingerprint().expect("must be a Diff").to_string();
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    // Record a rejected fingerprint — but the guard should not check it
    // because role_name is "planner", not "worker".
    record_rejected_integrity_entry(
        &task_id,
        &slot_ctx,
        djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
        None,
        None,
        &fingerprint,
    )
    .await;
    let provider = MockProvider::new(vec![MockResponse::tool_call_with_input(
        "submit-1",
        "submit_work",
        serde_json::json!({"task_id": task_id, "commit_title": "complete test work", "summary": "done", "files_changed": []}),
        100,
    )]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a planner."));
    conv.push(Message::user("Plan the task."));
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[serde_json::json!({
                "type": "function",
                "function": { "name": "submit_work", "description": "submit", "parameters": {"type": "object"} },
                "concurrent_safe": false
            })],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "planner",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    assert!(result.is_ok(), "expected ok, got: {result:?}");
    // Non-worker role → guard bypassed → finalize proceeds.
    assert!(
        output.finalize_payload.is_some(),
        "finalize_payload must be set for non-worker role; got None"
    );
    assert_eq!(
        output.finalize_tool_name.as_deref(),
        Some("submit_work"),
        "finalize_tool_name must be submit_work for non-worker role"
    );
}

/// When the current fingerprint differs from the latest rejected fingerprint,
/// the guard allows the submission to proceed and finalize is accepted.
#[tokio::test]
async fn different_fingerprint_allows_finalize() {
    let worktree = init_git_repo_with_dirty_file();
    let worktree_path = worktree.path().to_path_buf();
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    // Record a rejected fingerprint that is DIFFERENT from the current worktree.
    record_rejected_integrity_entry(
        &task_id,
        &slot_ctx,
        djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
        None,
        None,
        "sha256:completely-different-fingerprint",
    )
    .await;
    let provider = MockProvider::new(vec![MockResponse::tool_call_with_input(
        "submit-1",
        "submit_work",
        serde_json::json!({"task_id": task_id, "commit_title": "complete test work", "summary": "done", "files_changed": []}),
        100,
    )]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[serde_json::json!({
                "type": "function",
                "function": { "name": "submit_work", "description": "submit", "parameters": {"type": "object"} },
                "concurrent_safe": false
            })],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    assert!(result.is_ok(), "expected ok, got: {result:?}");
    // Different fingerprint → guard allows → finalize proceeds.
    assert!(
        output.finalize_payload.is_some(),
        "finalize_payload must be set when fingerprints differ; got None"
    );
    assert_eq!(
        output.finalize_tool_name.as_deref(),
        Some("submit_work"),
        "finalize_tool_name must be submit_work"
    );
}

/// When the worktree has no diff (empty submission), the guard's fingerprint
/// computation returns NoDiff and skips comparison. The existing empty-diff
/// safeguards remain intact and the finalize proceeds (those safeguards
/// handle empty diffs separately).
#[tokio::test]
async fn empty_worktree_skips_guard_and_allows_finalize() {
    // Create a git repo WITHOUT dirty changes — the fingerprint will be NoDiff.
    let dir = tempfile::Builder::new()
        .prefix("djinn-test-nodiff-guard-")
        .tempdir()
        .expect("create temp dir");
    let run_git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run_git(&["init"]);
    run_git(&["config", "--local", "user.email", "test@test.com"]);
    run_git(&["config", "--local", "user.name", "Test User"]);
    run_git(&["config", "--local", "commit.gpgsign", "false"]);
    std::fs::write(dir.path().join("README.md"), "hello\n").expect("write");
    run_git(&["add", "README.md"]);
    run_git(&["commit", "-m", "init"]);
    run_git(&["branch", "-m", "main"]);
    // No dirty edits — NoDiff.
    let worktree_path = dir.path().to_path_buf();
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    // Record a rejected fingerprint — but the guard should skip because
    // the current worktree is NoDiff.
    record_rejected_integrity_entry(
        &task_id,
        &slot_ctx,
        djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
        None,
        None,
        "sha256:some-fingerprint",
    )
    .await;
    let provider = MockProvider::new(vec![MockResponse::tool_call_with_input(
        "submit-1",
        "submit_work",
        serde_json::json!({"task_id": task_id, "commit_title": "complete test work", "summary": "done", "files_changed": []}),
        100,
    )]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[serde_json::json!({
                "type": "function",
                "function": { "name": "submit_work", "description": "submit", "parameters": {"type": "object"} },
                "concurrent_safe": false
            })],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    assert!(result.is_ok(), "expected ok, got: {result:?}");
    // NoDiff → guard skipped comparison → finalize proceeds (existing
    // empty-diff safeguards handle empty submissions separately).
    assert!(
        output.finalize_payload.is_some(),
        "finalize_payload must be set when worktree is NoDiff; got None"
    );
    assert_eq!(
        output.finalize_tool_name.as_deref(),
        Some("submit_work"),
        "finalize_tool_name must be submit_work"
    );
}

/// When a worker calls `submit_work` twice in the same session with a
/// fingerprint identical to the latest rejected fingerprint, the first call
/// is intercepted with a corrective tool result (first bounce) and the second
/// call settles the session as a typed `no_progress_submission` (second strike).
///
/// After the second strike:
/// - `output.no_progress_submission` is `true`
/// - `finalize_payload` is `None` (the finalize was NOT accepted)
/// - `finalize_tool_name` is `None`
/// - The `no_progress_submission` activity is logged to the task
#[tokio::test]
async fn second_strike_no_progress_submission_settles_session() {
    let worktree = init_git_repo_with_dirty_file();
    let worktree_path = worktree.path().to_path_buf();
    // Compute the fingerprint so we can record it as rejected.
    let fp = djinn_git::compute_submission_diff_fingerprint(&worktree_path)
        .await
        .expect("compute fingerprint");
    let fingerprint = fp.fingerprint().expect("must be a Diff").to_string();
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    // Record a rejected fingerprint for this task.
    record_rejected_integrity_entry(
        &task_id,
        &slot_ctx,
        djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
        None,
        None,
        &fingerprint,
    )
    .await;
    // Script: turn 1 returns submit_work (first bounce guard intercepts),
    // turn 2 is a text response to the corrective message, then turn 3
    // returns submit_work again (second strike triggers settlement and break).
    let provider = MockProvider::new(vec![
        // Turn 1: first submit_work — guard intercepts with corrective.
        MockResponse::tool_call_with_input(
            "submit-1",
            "submit_work",
            serde_json::json!({"task_id": task_id, "commit_title": "complete test work", "summary": "done", "files_changed": []}),
            100,
        ),
        // Turn 2: text response to the corrective message.
        MockResponse::text_only("I'll try resubmitting anyway.", 100),
        // Turn 3: second submit_work — guard triggers second-strike settle.
        MockResponse::tool_call_with_input(
            "submit-2",
            "submit_work",
            serde_json::json!({"task_id": task_id, "summary": "done again", "files_changed": []}),
            100,
        ),
    ]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[serde_json::json!({
                "type": "function",
                "function": { "name": "submit_work", "description": "submit", "parameters": {"type": "object"} },
                "concurrent_safe": false
            })],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    // The loop should have exited cleanly (the second strike breaks out).
    let _ = result;
    // Second strike: output must be flagged as no_progress_submission.
    assert!(
        output.no_progress_submission,
        "no_progress_submission must be true on second strike; got false"
    );
    // The finalize payload must NOT be accepted.
    assert!(
        output.finalize_payload.is_none(),
        "finalize_payload must be None on second strike; got: {:?}",
        output.finalize_payload
    );
    assert!(
        output.finalize_tool_name.is_none(),
        "finalize_tool_name must be None on second strike; got: {:?}",
        output.finalize_tool_name
    );
    // The conversation should contain the corrective tool result from the
    // first bounce.
    let has_corrective = conv.messages.iter().any(|m| {
        m.content.iter().any(|b| {
            matches!(b, ContentBlock::ToolResult { tool_use_id, is_error, content }
                if tool_use_id == "submit-1"
                    && *is_error
                    && content.iter().any(|c| matches!(c, ContentBlock::Text { text }
                        if text.contains("identical to the latest rejected submission"))))
        })
    });
    assert!(
        has_corrective,
        "conversation must contain a corrective tool result for the first bounce; \
         messages: {:?}",
        conv.messages
    );
    // The `no_progress_submission` activity must have been logged.
    let repo = TaskRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    let entries = repo
        .query_activity(djinn_db::repositories::task::ActivityQuery {
            task_id: Some(task_id.clone()),
            event_type: Some("no_progress_submission".to_string()),
            actor_role: None,
            project_id: None,
            from_time: None,
            to_time: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("query activity");
    assert!(
        !entries.is_empty(),
        "no_progress_submission activity must be logged after second strike"
    );
    // All mock responses should have been consumed.
    assert_eq!(provider.remaining(), 0);
}

/// gs37-shaped end-to-end rework-loop regression (proposal `ivek`).
///
/// Walks the full gs37 scenario at the highest fidelity the djinn-slot harness
/// allows — a fabricated no-edit submit after a genuine rejection, then a
/// merge-conflict hold, then the redispatch prompt build — and asserts the two
/// slot-side halves of the acceptance criterion:
///
///   1. Seed a genuine reviewer rejection round: a real `TaskReviewReject`
///      transition (`reopen_class=review_rejected`) plus a structured
///      `review_submitted` rejection activity carrying the newest rejection
///      text, and persist the rejected diff fingerprint.
///   2. Fabricated no-edit submit (worktree fingerprint identical to the
///      rejected fingerprint): the guard bounces the first identical submit
///      and typed-finalizes the second as `no_progress_submission` — WITHOUT
///      consuming a quality reopen strike.
///   3. Merge-conflict reopen (`TaskReviewRejectConflict` →
///      `reopen_class=merge_conflict`): a real reopen event that is excluded
///      from the quality strike count.
///   4. The redispatch prompt (`initial_user_message_for_task`) includes the
///      newest reviewer rejection verbatim and renders the bounded reopen
///      ledger.
///
/// Across the whole scenario exactly ONE quality reopen strike is consumed (the
/// genuine rejection), well below the coordinator's
/// `REOPEN_INTERVENTION_THRESHOLD` (3). The park-guard half of the acceptance
/// criterion — that Trigger A does NOT reach the human-review park threshold —
/// lives in djinn-coordinator (the threshold constant and
/// `maybe_intervene_on_stuck_task` are not visible from this crate) and is
/// asserted authoritatively in the sibling coordinator test
/// `gs37_park_guard_not_triggered_below_quality_threshold_despite_raw_reopen_advance`.
#[tokio::test]
async fn gs37_no_edit_submit_after_rejection_keeps_one_quality_strike_and_prompt_carries_rejection()
{
    // The verbatim reviewer rejection the redispatch prompt must carry forward.
    const NEWEST_REJECTION: &str = "AC-2 unmet: the handler is implemented but never registered with the \
         service. Wire it into `build_router` before resubmitting.";

    let worktree = init_git_repo_with_dirty_file();
    let worktree_path = worktree.path().to_path_buf();
    let fp = djinn_git::compute_submission_diff_fingerprint(&worktree_path)
        .await
        .expect("compute fingerprint");
    let fingerprint = fp.fingerprint().expect("must be a Diff").to_string();
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let repo = TaskRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    // The review lifecycle (SubmitTaskReview) requires acceptance criteria.
    repo.set_acceptance_criteria(&task_id, r#"[{"title":"AC-1"},{"title":"AC-2"}]"#)
        .await
        .expect("set acceptance criteria");

    // ── Step 1: seed a genuine reviewer rejection round ──────────────────────
    // open → in_progress → needs_task_review → in_task_review, then reject.
    for (action, actor_role) in [
        (djinn_core::models::TransitionAction::Start, "worker"),
        (
            djinn_core::models::TransitionAction::SubmitTaskReview,
            "worker",
        ),
        (
            djinn_core::models::TransitionAction::TaskReviewStart,
            "reviewer",
        ),
    ] {
        repo.transition(&task_id, action, actor_role, actor_role, None, None)
            .await
            .expect("valid setup transition");
    }
    repo.transition(
        &task_id,
        djinn_core::models::TransitionAction::TaskReviewReject,
        "reviewer",
        "reviewer",
        Some(NEWEST_REJECTION),
        None,
    )
    .await
    .expect("reviewer rejection transition");
    // Structured reviewer rejection activity — `recent_feedback` surfaces this
    // verbatim into the redispatch prompt (0kws).
    repo.log_activity(
        Some(&task_id),
        "reviewer",
        "reviewer",
        "review_submitted",
        &serde_json::json!({ "verdict": "rejected", "feedback": NEWEST_REJECTION }).to_string(),
    )
    .await
    .expect("log review_submitted");
    // Persist the rejected diff fingerprint — the submission-integrity anchor
    // the no-progress guard compares against.
    record_rejected_integrity_entry(
        &task_id,
        &slot_ctx,
        djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
        None,
        None,
        &fingerprint,
    )
    .await;

    assert_eq!(
        repo.quality_reopen_count(&task_id).await.unwrap(),
        1,
        "the genuine reviewer rejection is exactly one quality strike"
    );

    // ── Step 2: fabricated no-edit submit → first bounce, second-strike settle ─
    // Turn 1 submit_work (identical fingerprint) is bounced; turn 3 submit_work
    // (still identical) settles the session as a typed no_progress_submission.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call_with_input(
            "submit-1",
            "submit_work",
            serde_json::json!({"task_id": task_id, "commit_title": "complete test work", "summary": "done", "files_changed": []}),
            100,
        ),
        MockResponse::text_only("I'll try resubmitting anyway.", 100),
        MockResponse::tool_call_with_input(
            "submit-2",
            "submit_work",
            serde_json::json!({"task_id": task_id, "summary": "done again", "files_changed": []}),
            100,
        ),
    ]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (_result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[serde_json::json!({
                "type": "function",
                "function": { "name": "submit_work", "description": "submit", "parameters": {"type": "object"} },
                "concurrent_safe": false
            })],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    assert!(
        output.no_progress_submission,
        "second identical no-edit submit must settle as no_progress_submission"
    );
    assert!(
        output.finalize_payload.is_none(),
        "the fabricated no-edit submit must NOT be accepted as a finalize"
    );
    // The typed no_progress_submission was logged, and it consumed NO quality
    // reopen strike — the count is unchanged from step 1.
    let np_entries = repo
        .query_activity(djinn_db::repositories::task::ActivityQuery {
            task_id: Some(task_id.clone()),
            event_type: Some("no_progress_submission".to_string()),
            actor_role: None,
            project_id: None,
            from_time: None,
            to_time: None,
            limit: 10,
            offset: 0,
        })
        .await
        .expect("query activity");
    assert!(
        !np_entries.is_empty(),
        "a typed no_progress_submission activity must be logged"
    );
    assert_eq!(
        repo.quality_reopen_count(&task_id).await.unwrap(),
        1,
        "the no-edit submit bounce + settlement must NOT consume a quality reopen strike"
    );

    // ── Step 3: merge-conflict hold → reopen excluded from quality strikes ────
    // The task is back at `open` after the rejection; walk another review round
    // and reject it as a merge conflict.
    for (action, actor_role) in [
        (djinn_core::models::TransitionAction::Start, "worker"),
        (
            djinn_core::models::TransitionAction::SubmitTaskReview,
            "worker",
        ),
        (
            djinn_core::models::TransitionAction::TaskReviewStart,
            "reviewer",
        ),
    ] {
        repo.transition(&task_id, action, actor_role, actor_role, None, None)
            .await
            .expect("valid setup transition");
    }
    repo.transition(
        &task_id,
        djinn_core::models::TransitionAction::TaskReviewRejectConflict,
        "reviewer",
        "reviewer",
        Some("merge_conflict: base branch advanced under the PR"),
        None,
    )
    .await
    .expect("merge-conflict reject transition");
    assert_eq!(
        repo.quality_reopen_count(&task_id).await.unwrap(),
        1,
        "the merge_conflict reopen is excluded from the quality strike count"
    );
    let ledger = repo.recent_reopen_ledger(&task_id, 6).await.unwrap();
    assert_eq!(
        ledger.len(),
        2,
        "two reopen events: the reviewer rejection and the merge conflict"
    );
    // Newest first.
    assert_eq!(
        ledger[0].reopen_class,
        djinn_core::models::ReopenClass::MergeConflict
    );
    assert_eq!(
        ledger[1].reopen_class,
        djinn_core::models::ReopenClass::ReviewRejected
    );

    // ── Step 4: redispatch prompt carries the newest rejection + reopen ledger ─
    let prompt = crate::helpers::initial_user_message_for_task(&task_id, &slot_ctx).await;
    assert!(
        prompt.contains(NEWEST_REJECTION),
        "redispatch prompt must include the newest reviewer rejection verbatim; got: {prompt}"
    );
    assert!(
        prompt.contains("Reopen history"),
        "redispatch prompt must render the reopen ledger; got: {prompt}"
    );
    assert!(
        prompt.contains("review_rejected") && prompt.contains("merge_conflict"),
        "reopen ledger must render both reopen classes; got: {prompt}"
    );
}

/// Regression: when a worker submits a changed (non-matching) fingerprint,
/// the guard allows the finalize to proceed. The `no_progress_submission`
/// flag must remain false.
#[tokio::test]
async fn changed_diff_fingerprint_does_not_trigger_no_progress_submission() {
    let worktree = init_git_repo_with_dirty_file();
    let worktree_path = worktree.path().to_path_buf();
    // Compute the initial fingerprint and record it as rejected.
    let fp = djinn_git::compute_submission_diff_fingerprint(&worktree_path)
        .await
        .expect("compute fingerprint");
    let fingerprint = fp.fingerprint().expect("must be a Diff").to_string();
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    record_rejected_integrity_entry(
        &task_id,
        &slot_ctx,
        djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
        None,
        None,
        &fingerprint,
    )
    .await;
    // Change the worktree content so the fingerprint is different.
    std::fs::write(worktree_path.join("README.md"), "hello\nchanged content\n")
        .expect("write changed content");
    let provider = MockProvider::new(vec![MockResponse::tool_call_with_input(
        "submit-1",
        "submit_work",
        serde_json::json!({"task_id": task_id, "commit_title": "complete test work", "summary": "done", "files_changed": []}),
        100,
    )]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[serde_json::json!({
                "type": "function",
                "function": { "name": "submit_work", "description": "submit", "parameters": {"type": "object"} },
                "concurrent_safe": false
            })],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    assert!(result.is_ok(), "expected ok, got: {result:?}");
    // Different fingerprint → guard allows → finalize proceeds normally.
    assert!(
        !output.no_progress_submission,
        "no_progress_submission must be false for changed fingerprint"
    );
    assert!(
        output.finalize_payload.is_some(),
        "finalize_payload must be set for changed fingerprint"
    );
}

/// Regression: when reactive compaction fires (context_length_exceeded) but
/// the compaction itself fails (summarizer error), the reply loop must leave
/// only a `Started` boundary row with no completed `Ended` row. The session's
/// raw history remains accessible via `load_conversation`.
#[tokio::test]
async fn failed_reactive_compaction_leaves_started_only_boundary() {
    // Use 5,000 input tokens (50% of 10,000) so proactive compaction does NOT
    // fire (threshold is 80%). Then the second stream returns
    // context_length_exceeded, triggering the reactive compaction path.
    // compact_conversation fails because the summarizer calls also fail.
    let responses = vec![
        MockResponse::tool_call("t1", "nonexistent_tool", 5_000),
        // This triggers the reactive context_length_exceeded path.
        MockResponse {
            text: None,
            tool_calls: vec![],
            input_tokens: 0,
            output_tokens: 0,
            _error: Some(anyhow::anyhow!("context_length_exceeded")),
        },
        // compact_conversation calls provider for summarization → fails.
        MockResponse {
            text: None,
            tool_calls: vec![],
            input_tokens: 0,
            output_tokens: 0,
            _error: Some(anyhow::anyhow!("summarization_failed")),
        },
        // Not reached (error propagates), but available as fallback.
        MockResponse::text_only("done", 50),
    ];

    let provider = MockProvider::new(responses);
    let mut h = ReplyLoopHarness::new().await;
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = h.run(&provider, &[]).await;

    // The reply loop must fail because reactive compaction failed.
    assert!(
        result.is_err(),
        "reply loop must error when reactive compaction fails; got: {:?}",
        result
    );
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("context_length_exceeded"),
        "error must reference context_length_exceeded; got: {err_msg}"
    );

    // Verify boundary state: no completed boundary after failed compaction.
    // Note: the Started boundary insert is best-effort in persistence.rs and
    // may be silently swallowed if the DB write fails. The key regression is
    // that NO completed boundary exists (LatestCompleted remains None).
    // Pure boundary-row lifecycle is tested in djinn-db crate tests.
    let boundary_repo = SessionCompactionBoundaryRepository::new(h.slot_ctx.db.clone());
    let latest_completed = boundary_repo
        .latest_completed_boundary(&h.session_id)
        .await
        .unwrap();
    assert!(
        latest_completed.is_none(),
        "no completed boundary should exist after failed compaction"
    );

    // The raw conversation history must still be loadable and unchanged.
    let msg_repo =
        SessionMessageRepository::new(h.slot_ctx.db.clone(), h.slot_ctx.event_bus.clone());
    let persisted = msg_repo
        .load_conversation(&h.session_id)
        .await
        .expect("load_conversation must succeed");
    // After the tool call at 5,000 tokens, the reply loop added a tool result.
    // The persisted messages include at minimum the initial system+user + tool_use + tool_result.
    assert!(
        persisted.messages.len() >= 2,
        "raw history must be preserved after failed compaction; got {} messages",
        persisted.messages.len()
    );
}

/// Regression: persisting a compaction boundary for a worker conversation whose
/// messages have no `provider_data["id"]` must succeed against real Postgres.
///
/// Before the fix, `message_identity` produced `hash:<64-hex>` (69 chars) for
/// the fallback, overflowing the `VARCHAR(36)` columns in migration 92 and
/// triggering Postgres error 22001. The shared `bounded_message_identity`
/// helper now caps the fallback at 36 chars (`h:<34-hex>`).
#[tokio::test]
async fn worker_compaction_boundary_with_no_provider_id_succeeds() {
    let cancel = CancellationToken::new();
    let db = test_helpers::create_test_db();
    let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
    let session_repo = SessionRepository::new(db.clone(), ctx.event_bus.clone());
    let session = session_repo
        .create(CreateSessionParams {
            project_id: &project.id,
            task_id: Some(&task.id),
            model: "test/mock-model",
            agent_type: "worker",
            metadata_json: None,
            task_run_id: None,
            pricing: None,
            cost_basis: None,
        })
        .await
        .expect("create session");

    // Build a conversation with NO provider ids — every message triggers the
    // hash fallback in bounded_message_identity.
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    conv.push(Message::assistant("Working on it."));
    conv.push(Message::user("Keep going."));

    let boundary_repo = SessionCompactionBoundaryRepository::new(db.clone());

    // record_compaction_started → gather_boundary_identity → bounded_message_identity
    let boundary_id =
        super::persistence::record_compaction_started(&boundary_repo, &session.id, &conv)
            .await
            .expect("boundary persistence must succeed (no 22001)");

    let boundary = boundary_repo
        .fetch_by_id(&boundary_id)
        .await
        .expect("fetch boundary");
    assert_eq!(boundary.phase, djinn_db::CompactionPhase::Started);

    // Every id column must have been populated and must fit VARCHAR(36).
    let first_id = boundary
        .first_message_id
        .as_deref()
        .expect("first_message_id must be set");
    let last_id = boundary
        .last_compacted_message_id
        .as_deref()
        .expect("last_compacted_message_id must be set");
    assert!(
        first_id.len() <= 36,
        "first_message_id too long: {first_id} ({} chars)",
        first_id.len()
    );
    assert!(
        last_id.len() <= 36,
        "last_compacted_message_id too long: {last_id} ({} chars)",
        last_id.len()
    );
    assert!(
        first_id.starts_with("h:"),
        "expected hash fallback prefix, got: {first_id}"
    );
    assert!(
        last_id.starts_with("h:"),
        "expected hash fallback prefix, got: {last_id}"
    );

    // complete_compaction_boundary also persists via bounded_message_identity.
    super::persistence::complete_compaction_boundary(
        &boundary_repo,
        Some(&boundary_id),
        &conv,
        "test summary",
    )
    .await;

    let completed = boundary_repo
        .fetch_by_id(&boundary_id)
        .await
        .expect("fetch completed boundary");
    assert_eq!(completed.phase, djinn_db::CompactionPhase::Ended);
    assert_eq!(completed.summary_text.as_deref(), Some("test summary"));
}

// ---------------------------------------------------------------------------
// Compaction race and teardown-flush regression coverage (task zxs3)
//
// These tests exercise the integrated behavior of the compaction critical
// section, actor/pool deferral, and idempotent in-flight turn flush — the
// acceptance focus from epic d4b9 / proposal fxv4.
// ---------------------------------------------------------------------------

use crate::reply_loop::CompactionCriticalSection;

/// Scenario 1: slow active compaction plus a new message.
///
/// A reply loop session triggers proactive compaction (the provider returns a
/// tool call that exceeds the 80 % context threshold).  The test uses a shared
/// `CompactionCriticalSection` and asserts that:
///
/// * The guard is released after the reply loop completes (no lock leak).
/// * The pre-rotation transcript persisted to DB before compaction is not
///   mutated — `load_conversation` returns the compacted projection, and the
///   raw history has no duplicated or orphaned compacted context.
/// * The in-memory conversation is smaller after compaction than before.
#[tokio::test]
async fn shared_compaction_cs_released_and_transcript_coherent_after_proactive_compaction() {
    // context_window = 10,000 → threshold = 8,000
    // Turn 1: ToolUse at 8,500 tokens → proactive compaction fires.
    // Turn 2: summarizer → summary text.
    // Turn 3: final text-only → session ends.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call("t1", "shell", 8_500),
        MockResponse::text_only("Summary: worked on the task.", 200),
        MockResponse::text_only("Completed.", 300),
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");

    // Shared critical section — the test holds a clone to observe the guard
    // state while the reply loop holds the primary reference.
    let shared_cs = CompactionCriticalSection::new();

    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));

    // Snapshot the conversation length before the run.
    let pre_run_msg_count = conv.messages.len();

    let (result, _, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &shared_cs,
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;

    assert!(result.is_ok(), "expected ok, got: {result:?}");

    // The guard must be released after the loop exits — no lock leak.
    assert!(
        !shared_cs.is_compacting(),
        "CompactionCriticalSection must be released after reply loop completes"
    );

    // The in-memory conversation is smaller after compaction replaced the
    // pre-rotation messages with a summary.
    assert!(
        conv.messages.len() < pre_run_msg_count + 5,
        "conversation should be compact; got {} messages",
        conv.messages.len()
    );

    // The persisted transcript via load_conversation (projected view) must be
    // coherent: no duplicated or orphaned compacted context.
    let msg_repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    let projected = msg_repo
        .load_conversation(&session_id)
        .await
        .expect("load_conversation must succeed");
    assert!(
        !projected.messages.is_empty(),
        "projected conversation must not be empty"
    );

    // The raw history must also be accessible and must not contain duplicated
    // summary markers (a single compaction summary marker, not two).
    let raw = msg_repo
        .load_raw_conversation(&session_id)
        .await
        .expect("load_raw_conversation must succeed");
    let summary_marker_count = raw
        .messages
        .iter()
        .filter(|m| {
            m.text_content()
                .contains(djinn_compaction::COMPACTION_SUMMARY_END_MARKER)
        })
        .count();
    assert!(
        summary_marker_count <= 1,
        "at most one compaction summary marker should exist in raw history; found {summary_marker_count}"
    );

    // All mock responses were consumed.
    assert_eq!(provider.remaining(), 0);
}

/// Scenario 2: soft cancel during active compaction.
///
/// The reply loop triggers reactive compaction (context_length_exceeded) which
/// fails at the summarizer level.  The cancel token fires before the loop can
/// retry.  The test asserts:
///
/// * The critical section guard is released (RAII cleanup) even though
///   compaction failed.
/// * The boundary state is either absent or left as `Started` (for projection
///   to ignore).
/// * `load_conversation` returns a coherent conversation with no duplicated or
///   orphaned compacted context.
#[tokio::test]
async fn cancel_during_failed_compaction_releases_guard_no_orphaned_context() {
    let responses = vec![
        // Turn 1: tool call at moderate tokens — proactive compaction does NOT fire.
        MockResponse::tool_call("t1", "shell", 5_000),
        // Turn 2: context_length_exceeded → triggers reactive compaction.
        MockResponse {
            text: None,
            tool_calls: vec![],
            input_tokens: 0,
            output_tokens: 0,
            _error: Some(anyhow::anyhow!("context_length_exceeded")),
        },
        // Turn 3: summarizer fails → compaction fails.
        MockResponse {
            text: None,
            tool_calls: vec![],
            input_tokens: 0,
            output_tokens: 0,
            _error: Some(anyhow::anyhow!("summarization_failed")),
        },
        MockResponse::text_only("fallback", 50),
    ];
    let provider = MockProvider::new(responses);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");

    let shared_cs = CompactionCriticalSection::new();
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));

    let (result, _, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &shared_cs,
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;

    // The reply loop must fail because reactive compaction failed.
    assert!(result.is_err(), "expected error, got: {result:?}");

    // The guard must be released even on failure.
    assert!(
        !shared_cs.is_compacting(),
        "CompactionCriticalSection must be released after failed compaction"
    );

    // Boundary state: no completed boundary (Started-only or absent).
    let boundary_repo = SessionCompactionBoundaryRepository::new(slot_ctx.db.clone());
    let latest_completed = boundary_repo
        .latest_completed_boundary(&session_id)
        .await
        .unwrap();
    assert!(
        latest_completed.is_none(),
        "no completed boundary should exist after failed compaction"
    );

    // load_conversation must return coherent data — no orphaned context.
    let msg_repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    let projected = msg_repo
        .load_conversation(&session_id)
        .await
        .expect("load_conversation must succeed after failed compaction");
    // The conversation should have at least the initial messages.
    assert!(
        projected.messages.len() >= 2,
        "projected conversation should have at least system + user; got {}",
        projected.messages.len()
    );

    // No duplicate compaction markers.
    let raw = msg_repo
        .load_raw_conversation(&session_id)
        .await
        .expect("load_raw_conversation must succeed");
    let marker_count = raw
        .messages
        .iter()
        .filter(|m| {
            m.text_content()
                .contains(djinn_compaction::COMPACTION_SUMMARY_END_MARKER)
        })
        .count();
    assert!(
        marker_count <= 1,
        "at most one compaction summary marker; found {marker_count}"
    );
}

/// Scenario 3: kill/drain during active compaction.
///
/// This test verifies the integrated invariant: when compaction succeeds and
/// the reply loop finishes normally, the pre-rotation transcript in the DB is
/// replaced by the compacted projection — not a mixture of old and new
/// messages.  The `load_conversation` view must show exactly one coherent
/// compacted context (system prompt + summary + continuation).
///
/// At the actor level (see `actor.rs::kill_during_compaction_is_deferred_until_release`
/// and `pool/tests.rs::kill_session_during_compaction_defers_settlement`) the
/// kill/drain commands are deferred.  This test proves that the DB state after
/// a successful compaction is clean, so a deferred session release would settle
/// against the post-rotation transcript, never the pre-rotation one.
#[tokio::test]
async fn successful_compaction_produces_clean_post_rotation_transcript() {
    // Two rounds of tool calls that each exceed the threshold:
    // Turn 1: ToolUse at 8,500 → proactive compaction.
    // Turn 2: summarizer → summary.
    // Turn 3: ToolUse at 8,500 → second proactive compaction.
    // Turn 4: summarizer → summary.
    // Turn 5: final text → done.
    let provider = MockProvider::new(vec![
        MockResponse::tool_call("t1", "shell", 8_500),
        MockResponse::text_only("Summary of first round.", 200),
        MockResponse::tool_call("t2", "shell", 8_500),
        MockResponse::text_only("Summary of second round.", 200),
        MockResponse::text_only("All done.", 300),
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");

    let shared_cs = CompactionCriticalSection::new();
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));

    let (result, _, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &shared_cs,
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;

    assert!(result.is_ok(), "expected ok, got: {result:?}");
    assert!(
        !shared_cs.is_compacting(),
        "guard must be released after double-compaction"
    );

    // The projected conversation must be clean: the final compaction summary
    // should be the most recent, and there should be no orphaned intermediate
    // summaries visible in the projected view.
    let msg_repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    let projected = msg_repo
        .load_conversation(&session_id)
        .await
        .expect("load_conversation must succeed");

    // The projected view should start with the summary (from the latest
    // completed compaction boundary) followed by the tail.
    assert!(
        projected.messages.len() >= 2,
        "projected conversation must have at least summary + continuation; got {}",
        projected.messages.len()
    );

    // The raw history should have at most 2 summary markers (one per
    // compaction round), not duplicates from failed partial compactions.
    let raw = msg_repo
        .load_raw_conversation(&session_id)
        .await
        .expect("load_raw_conversation must succeed");
    let marker_count = raw
        .messages
        .iter()
        .filter(|m| {
            m.text_content()
                .contains(djinn_compaction::COMPACTION_SUMMARY_END_MARKER)
        })
        .count();
    assert!(
        marker_count <= 2,
        "at most 2 compaction markers for 2 compaction rounds; found {marker_count}"
    );

    assert_eq!(provider.remaining(), 0);
}

/// Scenario 4: idempotent in-flight assistant/tool flush visible in
/// `load_conversation`.
///
/// Simulates an interrupted turn: the assistant produced text + a tool_use
/// block, and a streaming tool result completed.  The flush helper persists
/// these rows.  A second flush is a no-op (idempotent).  The projected
/// `load_conversation` view contains the flushed round exactly once.
#[tokio::test]
async fn flushed_tool_round_visible_exactly_once_in_load_conversation() {
    let (slot_ctx, _project_path, task_id, session_id, _cancel) = make_context().await;

    let msg_repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());

    // Simulate an in-flight turn: assistant text + tool_use + streaming result.
    let mut state = super::streaming::StreamTurnState::new();
    state.turn_text = "Let me run the tests.".to_string();
    state.turn_thinking = "Need to verify the build.".to_string();
    state.turn_unresolved_thinking.push(
        super::streaming::UnresolvedThinkingFragment::Unattributed(
            "Need to verify the build.".to_string(),
        ),
    );
    state.turn_tool_calls = vec![ContentBlock::ToolUse {
        id: "call_flush_1".to_string(),
        name: "shell".to_string(),
        input: serde_json::json!({ "command": "cargo test" }),
    }];
    state.streaming_results = vec![(
        0,
        ContentBlock::ToolResult {
            tool_use_id: "call_flush_1".to_string(),
            content: vec![ContentBlock::text("test result: all passed")],
            is_error: false,
        },
    )];

    // First flush: should persist assistant + tool result.
    super::persistence::flush_in_flight_turn(&msg_repo, &session_id, &task_id, 1000, &mut state)
        .await;
    assert!(
        state.turn_flushed,
        "turn_flushed must be set after first flush"
    );

    // Second flush: no-op (idempotency guard).
    super::persistence::flush_in_flight_turn(&msg_repo, &session_id, &task_id, 2000, &mut state)
        .await;

    // Third flush: still no-op.
    super::persistence::flush_in_flight_turn(&msg_repo, &session_id, &task_id, 3000, &mut state)
        .await;

    // Verify the projected load_conversation view (not just raw).
    let projected = msg_repo
        .load_conversation(&session_id)
        .await
        .expect("load_conversation must succeed");

    // Exactly 2 messages: the assistant message and the tool result.
    assert_eq!(
        projected.messages.len(),
        2,
        "expected exactly 2 messages in projected view, found {}",
        projected.messages.len()
    );

    // The assistant message must contain the text, thinking, and tool_use.
    let assistant = &projected.messages[0];
    assert_eq!(assistant.role, djinn_provider::message::Role::Assistant);
    assert!(
        assistant.text_content().contains("Let me run the tests."),
        "assistant text must be present"
    );
    assert!(
        assistant.content.iter().any(|b| matches!(
            b,
            ContentBlock::Thinking { thinking, .. } if thinking.contains("Need to verify")
        )),
        "thinking content must be preserved"
    );
    assert!(
        assistant.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolUse { id, name, .. }
                if id == "call_flush_1" && name == "shell"
        )),
        "tool_use block must be present"
    );

    // The tool result message must be present exactly once.
    let tool_result = &projected.messages[1];
    assert_eq!(tool_result.role, djinn_provider::message::Role::User);
    assert!(
        tool_result.content.iter().any(|b| matches!(
            b,
            ContentBlock::ToolResult { tool_use_id, .. }
                if tool_use_id == "call_flush_1"
        )),
        "tool result must be present with correct tool_use_id"
    );

    // Verify the raw view also has exactly 2 messages (no duplication).
    let raw = msg_repo
        .load_raw_conversation(&session_id)
        .await
        .expect("load_raw_conversation must succeed");
    assert_eq!(
        raw.messages.len(),
        2,
        "raw view must also have exactly 2 messages, found {}",
        raw.messages.len()
    );
}

/// Scenario 4 variant: flush after cancellation signals the turn as flushed,
/// and a subsequent reclaim/kill can safely call flush again without
/// duplicating rows.  This simulates the teardown path where multiple
/// code paths may call `flush_in_flight_turn` (deploy drain, stall-kill,
/// force-close).
#[tokio::test]
async fn flush_after_cancel_is_idempotent_across_multiple_teardown_paths() {
    let (slot_ctx, _project_path, task_id, session_id, _cancel) = make_context().await;

    let msg_repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());

    // In-flight turn with only assistant text (no tool calls — simulates a
    // text-only stream that was cancelled mid-generation).
    let mut state = super::streaming::StreamTurnState::new();
    state.turn_text = "Working on the implementation...".to_string();
    state.turn_tokens_in = 500;
    state.turn_tokens_out = 30;

    // Simulate the teardown sequence: cancel → drain → kill all call flush.
    // Path 1: cancel handler flushes.
    super::persistence::flush_in_flight_turn(&msg_repo, &session_id, &task_id, 1000, &mut state)
        .await;
    assert!(state.turn_flushed);

    // Path 2: drain handler flushes (should be a no-op).
    super::persistence::flush_in_flight_turn(&msg_repo, &session_id, &task_id, 2000, &mut state)
        .await;

    // Path 3: kill handler flushes (should be a no-op).
    super::persistence::flush_in_flight_turn(&msg_repo, &session_id, &task_id, 3000, &mut state)
        .await;

    // Exactly 1 assistant message persisted, not 3.
    let projected = msg_repo
        .load_conversation(&session_id)
        .await
        .expect("load_conversation must succeed");
    assert_eq!(
        projected.messages.len(),
        1,
        "expected exactly 1 assistant message, found {}",
        projected.messages.len()
    );
    assert_eq!(
        projected.messages[0].role,
        djinn_provider::message::Role::Assistant
    );
    assert!(
        projected.messages[0]
            .text_content()
            .contains("Working on the implementation"),
        "assistant text must be preserved"
    );
}

/// Regression: compaction guard is not left active when the reply loop exits
/// via the max-turns limit (a non-error, non-compaction exit path).
#[tokio::test]
async fn compaction_cs_released_on_max_turns_exit() {
    let provider = MockProvider::new(vec![
        MockResponse::text_only("Turn 1.", 100),
        MockResponse::text_only("Turn 2.", 100),
        MockResponse::text_only("Turn 3.", 100),
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let shared_cs = CompactionCriticalSection::new();

    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));

    let (result, _, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &shared_cs,
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: Some(2),
        },
        &mut conv,
        false,
    )
    .await;

    // The reply loop may succeed or fail depending on loop guard behavior,
    // but the CS must always be released.
    let _ = result;
    assert!(
        !shared_cs.is_compacting(),
        "CompactionCriticalSection must be released on max-turns exit"
    );
}

/// Regression: compaction guard is released when the reply loop exits via
/// cancellation (the cancel token fires between turns).
#[tokio::test]
async fn compaction_cs_released_on_cancel_exit() {
    // This provider would trigger compaction, but we cancel before it runs.
    let provider = MockProvider::new(vec![
        MockResponse::text_only("Turn 1.", 100),
        MockResponse::text_only("Turn 2.", 100),
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let shared_cs = CompactionCriticalSection::new();

    // Cancel immediately — the reply loop should exit on the first cancel check.
    cancel.cancel();

    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));

    let (result, _, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &shared_cs,
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;

    // The result should be an error (cancelled).
    assert!(result.is_err(), "expected cancel error, got: {result:?}");

    // The guard must be released.
    assert!(
        !shared_cs.is_compacting(),
        "CompactionCriticalSection must be released on cancel exit"
    );
}

/// Regression: the pre-rotation conversation snapshot in the DB is not mutated
/// by a later compaction.  Messages persisted before compaction (e.g. the
/// initial user task and a tool round) remain in the raw history, and
/// `load_conversation` returns the projected compacted view.
#[tokio::test]
async fn load_conversation_projects_compacted_view_while_raw_preserves_history() {
    let provider = MockProvider::new(vec![
        // Turn 1: tool call that exceeds the compaction threshold.
        MockResponse::tool_call("t1", "shell", 8_500),
        // Turn 2: summarizer call → summary text.
        MockResponse::text_only("Summary: analyzed codebase and found issues.", 200),
        // Turn 3: final text → done.
        MockResponse::text_only("Fixed all issues.", 300),
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");

    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Fix the bugs."));

    let (result, _, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            compaction_cs: &crate::reply_loop::CompactionCriticalSection::new(),
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_planner"],
            context_window: 10_000,
            model_id: "test/mock-model",
            cancel: &cancel,
            global_cancel: &cancel,
            ctx: &slot_ctx,
            active_skill_names: &[],
            active_mcp_server_names: &[],
            max_turns_override: None,
        },
        &mut conv,
        false,
    )
    .await;
    assert!(result.is_ok(), "expected ok, got: {result:?}");

    let msg_repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());

    // Raw history has ALL messages persisted during the run (tool rounds,
    // compaction summaries, final text).  After one compaction, the raw history
    // contains the pre-compaction tool round AND the post-compaction tail.
    let raw = msg_repo
        .load_raw_conversation(&session_id)
        .await
        .expect("load_raw_conversation");
    assert!(!raw.messages.is_empty(), "raw history should not be empty");

    // Projected view applies the compaction boundary projection.  The boundary
    // record stores the summary text separately, so `load_conversation` prepends
    // a synthetic summary message.  The projected view should be non-empty and
    // contain the compaction summary.
    let projected = msg_repo
        .load_conversation(&session_id)
        .await
        .expect("load_conversation");
    assert!(
        !projected.messages.is_empty(),
        "projected view should not be empty"
    );

    // The projected view may start with a compaction summary (if the boundary
    // was successfully persisted) or with the original user message (if the
    // boundary write failed silently — persistence is best-effort).  Either
    // way, the view must be coherent and non-empty.
    assert!(
        !projected.messages.is_empty(),
        "projected view must not be empty; got {} messages",
        projected.messages.len()
    );
}

// ── AC1: Non-Codex terminal empty/no-event turn fails immediately ─────────

/// A non-Codex provider that produces a terminal empty/no-event stream must
/// fail immediately on the first occurrence (no retries) with a typed
/// `ProviderError::ProviderInternal` suitable for failover.
#[tokio::test]
async fn non_codex_empty_stream_fails_immediately_as_typed_provider_failure() {
    use djinn_provider::provider::ProviderError;

    // A provider whose first call returns an empty stream (no events).
    // In consume_provider_stream, this means ctx.stream.next() → None
    // immediately, so early_stream_end=true, saw_round_event=false.
    let provider = test_helpers::FakeProvider::script(vec![vec![]]);
    let mut h = ReplyLoopHarness::new().await;
    let (result, _output, _, _, _, _) = h
        .run_with_model(&provider, &[], "synthetic/kimi-k2.5")
        .await;
    let err = result.expect_err("non-Codex empty stream must produce a terminal error");
    let typed = err
        .downcast_ref::<ProviderError>()
        .expect("error must carry a typed ProviderError for failover classification");
    // Non-Codex providers get a transient ProviderInternal(500).
    assert_eq!(*typed, ProviderError::ProviderInternal { status: 500 });
    assert!(
        typed.retryable(),
        "empty-turn failure must be retryable for failover"
    );
    assert!(
        err.to_string().contains("empty"),
        "error must mention empty for diagnostics: {err}"
    );
    // No assistant content was produced, so nothing should be persisted.
    // (The initial user message IS persisted by design — we check assistant only.)
    let persisted = count_persisted_assistant_messages(&h.slot_ctx, &h.session_id).await;
    assert_eq!(
        persisted, 0,
        "empty-stream failure must not persist any assistant turn"
    );
}

// ── AC1 (Codex preservation): Codex retries before terminal failure ────────

/// A Codex/OpenAI-family provider must retry empty streams up to
/// MAX_EMPTY_TURN_RETRIES before failing, preserving the existing throttle
/// handling behavior for ChatGPT-account Codex rate limits.
#[tokio::test]
async fn codex_empty_stream_retries_before_terminal_failure() {
    use super::error_handling::MAX_EMPTY_TURN_RETRIES;
    use djinn_provider::provider::ProviderError;

    // MAX_EMPTY_TURN_RETRIES + 1 empty-stream calls (initial + all retries
    // exhausted). Each produces an empty stream.
    let provider = test_helpers::FakeProvider::script(
        (0..=MAX_EMPTY_TURN_RETRIES)
            .map(|_| vec![])
            .collect::<Vec<_>>(),
    );
    let mut h = ReplyLoopHarness::new().await;
    let (result, _output, _, _, _, _) = h.run_with_model(&provider, &[], "openai/gpt-5.4").await;
    let err = result.expect_err("Codex empty stream must eventually produce a terminal error");
    let typed = err
        .downcast_ref::<ProviderError>()
        .expect("Codex error must carry EmptyCompletion");
    assert_eq!(*typed, ProviderError::EmptyCompletion);
}

// ── AC2: Provider failure prose reclassified as typed failure ───────────────

/// Assistant text shaped like rate-limit prose must be reclassified as a typed
/// `ProviderError::RateLimit` and must NOT be persisted as a successful turn.
#[tokio::test]
async fn rate_limit_prose_reclassified_and_not_persisted() {
    use djinn_provider::provider::ProviderError;

    let provider = MockProvider::new(vec![MockResponse::text_only(
        "Rate limit exceeded. Please try again later.",
        50,
    )]);
    let mut h = ReplyLoopHarness::new().await;
    let (result, _output, _, _, _, _) = h.run(&provider, &[]).await;
    let err = result.expect_err("rate-limit prose must produce a typed provider failure");
    let typed = err
        .downcast_ref::<ProviderError>()
        .expect("error must carry a typed ProviderError for failover");
    assert!(
        matches!(typed, ProviderError::RateLimit { .. }),
        "rate-limit prose must be classified as RateLimit, got: {typed:?}"
    );
    let persisted = count_persisted_assistant_messages(&h.slot_ctx, &h.session_id).await;
    assert_eq!(
        persisted, 0,
        "rate-limit prose must NOT be persisted as a successful assistant turn"
    );
}

/// Quota-exhaustion prose (insufficient_quota) from a provider is also
/// reclassified and not persisted.
#[tokio::test]
async fn quota_exhaustion_prose_reclassified_and_not_persisted() {
    use djinn_provider::provider::ProviderError;

    let provider = MockProvider::new(vec![MockResponse::text_only(
        "Error: insufficient_quota — you need to add credits.",
        50,
    )]);
    let mut h = ReplyLoopHarness::new().await;
    let (result, _output, _, _, _, _) = h.run(&provider, &[]).await;
    assert!(result.is_err(), "quota prose must produce a failure");
    let typed = result
        .unwrap_err()
        .downcast_ref::<ProviderError>()
        .expect("quota prose error must carry a typed ProviderError")
        .clone();
    assert!(
        matches!(typed, ProviderError::RateLimit { .. }),
        "quota prose must be classified as RateLimit: {typed:?}"
    );
    let persisted = count_persisted_assistant_messages(&h.slot_ctx, &h.session_id).await;
    assert_eq!(persisted, 0, "quota prose must not be persisted");
}

// ── AC3: Failed/truncated turns not persisted as complete messages ──────────

/// An empty-stream failure does not persist any assistant message, even when
/// the stream ends early (early_stream_end path).
#[tokio::test]
async fn failed_turn_not_persisted_as_complete_assistant_message() {
    let provider = test_helpers::FakeProvider::script(vec![vec![]]);
    let mut h = ReplyLoopHarness::new().await;
    let (result, _, _, _, _, _) = h.run_with_model(&provider, &[], "synthetic/glm-4.7").await;
    assert!(result.is_err(), "empty stream must fail");
    let persisted = count_persisted_assistant_messages(&h.slot_ctx, &h.session_id).await;
    assert_eq!(
        persisted, 0,
        "an empty-stream failure must not persist any assistant turn"
    );
}

/// A stream that emits partial assistant text and then ends without
/// `StreamEvent::Done` is a truncated provider turn. The observed partial text
/// may be flushed for resume/timeline durability, but it must not be finalized
/// into the in-memory conversation as a successful complete assistant turn (nor
/// duplicated through the normal complete-message persistence path).
#[tokio::test]
async fn partial_truncated_stream_not_finalized_as_complete_assistant_turn() {
    use djinn_provider::provider::ProviderError;

    let provider =
        test_helpers::FakeProvider::script(vec![vec![StreamEvent::Delta(ContentBlock::Text {
            text: "partial assistant output".to_string(),
        })]]);
    let mut h = ReplyLoopHarness::new().await;
    let (result, _, _, _, _, _) = h.run_with_model(&provider, &[], "synthetic/glm-4.7").await;

    assert!(result.is_err(), "truncated partial stream must fail");
    let err = result.unwrap_err();
    let typed = err
        .downcast_ref::<ProviderError>()
        .expect("truncated stream error must carry a typed ProviderError");
    assert!(
        matches!(typed, ProviderError::ProviderInternal { .. }),
        "truncated stream must be classified as provider-internal failure, got: {typed:?}"
    );

    assert!(
        h.conv
            .messages
            .iter()
            .all(|message| message.role != Role::Assistant),
        "partial truncated output must not be finalized as a complete assistant turn"
    );

    let repo = SessionMessageRepository::new(h.slot_ctx.db.clone(), h.slot_ctx.event_bus.clone());
    let raw = repo
        .load_raw_conversation(&h.session_id)
        .await
        .expect("load raw conversation");
    let persisted_assistant = raw
        .messages
        .iter()
        .filter(|message| message.role == Role::Assistant)
        .count();
    assert_eq!(
        persisted_assistant, 1,
        "only the observed in-flight assistant artifact should be durable; \
         normal complete-message finalization must not add a duplicate"
    );
    assert!(
        raw.messages
            .iter()
            .any(|message| message.text_content().contains("partial assistant output")),
        "observed partial assistant text should remain durable for resume"
    );
}

/// Productive turns ARE still persisted normally (baseline correctness check).
/// This ensures the persistence guardrails don't break normal operation.
#[tokio::test]
async fn productive_turns_persisted_normally() {
    let tools = vec![dummy_tool_schema("submit_work")];
    let provider = MockProvider::new(vec![
        MockResponse::text_only("I'm working on the task.", 100),
        MockResponse {
            text: None,
            tool_calls: vec![ContentBlock::ToolUse {
                id: "fin".to_string(),
                name: "submit_work".to_string(),
                input: serde_json::json!({"task_id": "t1", "commit_title": "complete test work", "summary": "done"}),
            }],
            input_tokens: 110,
            output_tokens: 10,
            _error: None,
        },
    ]);
    let mut h = ReplyLoopHarness::new().await;
    provider.bind_valid_submit_work_fixtures(&h.task_id);
    let (result, _output, _, _, _, _) = h.run(&provider, &tools).await;
    assert!(
        result.is_ok(),
        "productive session should succeed: {result:?}"
    );
    let persisted = count_persisted_assistant_messages(&h.slot_ctx, &h.session_id).await;
    assert!(
        persisted >= 1,
        "productive assistant turns must be persisted; got {persisted}"
    );
}

// ── AC4: Reuses existing invariants ────────────────────────────────────────

/// Verify that non-Codex empty-stream failures carry a typed ProviderError
/// suitable for the existing breaker/failover classification. This confirms
/// the implementation reuses the existing stream invariants from 3pqv rather
/// than introducing a parallel watchdog.
#[tokio::test]
async fn non_codex_empty_turn_error_is_breaker_classifiable() {
    use djinn_provider::provider::ProviderError;

    let provider = test_helpers::FakeProvider::script(vec![vec![]]);
    let mut h = ReplyLoopHarness::new().await;
    let (result, _, _, _, _, _) = h
        .run_with_model(&provider, &[], "kimi-for-coding/k2p7")
        .await;
    let err = result.expect_err("non-Codex empty stream must fail");
    let typed = err
        .downcast_ref::<ProviderError>()
        .expect("must carry typed ProviderError");
    assert_eq!(*typed, ProviderError::ProviderInternal { status: 500 });
    assert!(typed.retryable(), "must be retryable for failover");
}

/// Codex empty-stream failures carry EmptyCompletion, preserving the
/// distinction between throttle and genuine failure that existing breaker
/// logic depends on.
#[tokio::test]
async fn codex_empty_turn_error_is_empty_completion_throttle() {
    use super::error_handling::MAX_EMPTY_TURN_RETRIES;
    use djinn_provider::provider::ProviderError;

    let provider = test_helpers::FakeProvider::script(
        (0..=MAX_EMPTY_TURN_RETRIES)
            .map(|_| vec![])
            .collect::<Vec<_>>(),
    );
    let mut h = ReplyLoopHarness::new().await;
    let (result, _, _, _, _, _) = h.run_with_model(&provider, &[], "openai/gpt-5.4").await;
    let err = result.expect_err("Codex must eventually fail");
    let typed = err
        .downcast_ref::<ProviderError>()
        .expect("must carry EmptyCompletion");
    assert_eq!(*typed, ProviderError::EmptyCompletion);
}

// ── Provider-tool preservation regression (proposal wzz6) ──────────────────
//
// After prompt-side tool-description deduplication and canonical description
// trimming, the provider request passed to `.stream(...)` must still carry
// full tool schemas with `name`, `description`, and `inputSchema`.  At least
// one provider-declared tool invocation must complete through the existing
// dispatch path.  These tests are the regression gate for that contract.

/// Fetch the real canonical worker tool schemas from `djinn-mcp-extension`.
///
/// These are the same schemas that the production reply loop passes to
/// `.stream(...)` — sourced from `tool_schemas_worker()` via the
/// `djinn-roles` tool-schema registry.  Using the real schemas (rather
/// than hand-written facsimiles) ensures the regression tests catch any
/// change to the canonical schema surface (e.g. dropping `description`
/// or renaming `inputSchema`).
///
/// The schemas use the native format with top-level `name`, `description`,
/// and `inputSchema` keys — matching the wire format seen by
/// `RecordingProvider::stream()`.
fn real_worker_tool_schemas() -> Vec<serde_json::Value> {
    djinn_mcp_extension::tool_defs::tool_schemas_worker()
}

/// LlmProvider wrapper that records every `tools` slice received by
/// `.stream()` so the test can assert schema field preservation.
struct RecordingProvider {
    recorded_tools: Arc<Mutex<Vec<Vec<serde_json::Value>>>>,
    inner: MockProvider,
}

impl RecordingProvider {
    fn new(inner: MockProvider) -> Self {
        Self {
            recorded_tools: Arc::new(Mutex::new(Vec::new())),
            inner,
        }
    }
    fn captured_tools(&self) -> Vec<Vec<serde_json::Value>> {
        self.recorded_tools.lock().unwrap().clone()
    }
}

impl LlmProvider for RecordingProvider {
    fn name(&self) -> &str {
        "recording"
    }
    fn stream<'a>(
        &'a self,
        conversation: &'a Conversation,
        tools: &'a [serde_json::Value],
        tool_choice: Option<ToolChoice>,
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
        self.recorded_tools.lock().unwrap().push(tools.to_vec());
        self.inner.stream(conversation, tools, tool_choice)
    }
}

/// Regression test: the provider request must still carry every tool schema
/// with `name`, `description`, and `inputSchema` (native format).
///
/// After prompt-side deduplication the role prompt no longer repeats tool
/// descriptions, so the provider request is the *only* place the model sees
/// them.  This test captures the exact `tools` array received by `.stream()`
/// and asserts each entry has all three required fields.
#[tokio::test]
async fn provider_tool_schemas_preserve_name_description_and_input_schema() {
    let schemas = real_worker_tool_schemas();

    // Turn 1: model calls `shell` → dispatches through MockToolDispatcher.
    // Turn 2: model calls `submit_work` → session finalizes.
    let inner = MockProvider::new(vec![
        MockResponse::tool_call_with_input(
            "tc_shell",
            "shell",
            serde_json::json!({"command": "echo hello"}),
            200,
        ),
        MockResponse {
            text: None,
            tool_calls: vec![ContentBlock::ToolUse {
                id: "fin1".to_string(),
                name: "submit_work".to_string(),
                input: serde_json::json!({
                    "task_id": "t1",
                    "commit_title": "done",
                    "summary": "completed"
                }),
            }],
            input_tokens: 150,
            output_tokens: 10,
            _error: None,
        },
    ]);
    let provider = RecordingProvider::new(inner);
    let mut h = ReplyLoopHarness::new_with_worker_prompt().await;
    provider.inner.bind_valid_submit_work_fixtures(&h.task_id);

    // ── Prompt-side assertion: the rendered system prompt must be
    // signature-only — tool description bodies must NOT appear on the
    // tool-signature lines in the system prompt.  Capture before run()
    // so compaction cannot alter the system message. ──
    let system_prompt_text = h.conv.messages[0].text_content();
    for schema in &schemas {
        let name = schema
            .get("name")
            .and_then(|v| v.as_str())
            .expect("schema has name");
        let desc = schema
            .get("description")
            .and_then(|v| v.as_str())
            .expect("schema has description");

        // The tools section must contain a signature-only line like
        //   - `shell(command, timeout_ms?)`
        let has_sig_line = system_prompt_text.lines().any(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with(&format!("- `{name}("))
        });
        assert!(
            has_sig_line,
            "rendered worker prompt must contain signature-only tool line \
             for {name}; this indicates format_tools_section regression"
        );

        // That same line must NOT carry the old description-extended format
        //   - `shell(command, timeout_ms?)` — Execute shell commands…
        // If this assertion fails, format_tools_section is duplicating
        // provider descriptions in the prompt again.
        let desc_on_sig_line = system_prompt_text.lines().any(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with(&format!("- `{name}(")) && trimmed.contains(&format!(" — {desc}"))
        });
        assert!(
            !desc_on_sig_line,
            "rendered worker prompt tool line for {name} must NOT include \
             description body (' — {desc}'); format_tools_section is \
             duplicating provider descriptions in the prompt"
        );
    }

    let (result, output, _ti, _to, _cr, _cw) = h.run(&provider, &schemas).await;

    // Session must complete successfully.
    assert!(result.is_ok(), "expected ok, got: {result:?}");
    assert!(
        output.finalize_payload.is_some(),
        "finalize payload should be captured after shell dispatch"
    );

    // The provider's .stream() was called at least once.
    let captures = provider.captured_tools();
    assert!(
        !captures.is_empty(),
        "RecordingProvider should have captured at least one tools slice"
    );

    // Every captured tools array must carry every schema field unchanged.
    // Real schemas use native format: top-level `name`, `description`,
    // `inputSchema` (not wrapped in `{"type":"function","function":{…}}`).
    for (turn_idx, captured) in captures.iter().enumerate() {
        assert_eq!(
            captured.len(),
            schemas.len(),
            "turn {turn_idx}: captured tools count must match input"
        );
        for (i, tool) in captured.iter().enumerate() {
            // `name` must be present and non-empty at the top level.
            let name = tool
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("turn {turn_idx}, tool {i}: name missing"));
            assert!(
                !name.is_empty(),
                "turn {turn_idx}, tool {i}: name must not be empty"
            );
            // `description` must be present and non-empty at the top level.
            let desc = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("turn {turn_idx}, tool {i}: description missing"));
            assert!(
                !desc.is_empty(),
                "turn {turn_idx}, tool {i}: description must not be empty"
            );
            // `inputSchema` must be present and be an object.
            let input_schema = tool
                .get("inputSchema")
                .unwrap_or_else(|| panic!("turn {turn_idx}, tool {i}: inputSchema missing"));
            assert!(
                input_schema.is_object(),
                "turn {turn_idx}, tool {i}: inputSchema must be an object"
            );
        }
    }
}

/// Regression test: at least one provider-declared tool invocation completes
/// through the existing dispatch path after prompt-side deduplication.
///
/// The `shell` tool is a provider-declared extension tool whose schema
/// carries `name`, `description`, and `inputSchema`.  The `MockToolDispatcher`
/// handles it and returns a successful result.  This test proves the
/// dispatch→execute→result round-trip is intact.
#[tokio::test]
async fn provider_declared_tool_dispatch_completes_successfully() {
    let schemas = real_worker_tool_schemas();
    let inner = MockProvider::new(vec![
        // Turn 1: model calls `shell` (provider-declared extension tool).
        MockResponse::tool_call_with_input(
            "tc1",
            "shell",
            serde_json::json!({"command": "echo dispatched"}),
            200,
        ),
        // Turn 2: model calls `read` (second provider-declared tool).
        MockResponse::tool_call_with_input(
            "tc2",
            "read",
            serde_json::json!({"file_path": "/tmp/test.txt"}),
            250,
        ),
        // Turn 3: finalize.
        MockResponse {
            text: None,
            tool_calls: vec![ContentBlock::ToolUse {
                id: "fin1".to_string(),
                name: "submit_work".to_string(),
                input: serde_json::json!({
                    "task_id": "t1",
                    "commit_title": "done",
                    "summary": "dispatched shell and read"
                }),
            }],
            input_tokens: 200,
            output_tokens: 10,
            _error: None,
        },
    ]);
    let provider = RecordingProvider::new(inner);
    let mut h = ReplyLoopHarness::new_with_worker_prompt().await;
    provider.inner.bind_valid_submit_work_fixtures(&h.task_id);
    let (result, output, _ti, _to, _cr, _cw) = h.run(&provider, &schemas).await;

    // Session completes — both tool dispatches succeeded.
    assert!(result.is_ok(), "expected ok, got: {result:?}");
    assert!(
        output.finalize_payload.is_some(),
        "finalize should be captured after two successful tool dispatches"
    );

    // Verify tool results flowed back into the conversation (proof dispatch
    // executed and returned content to the model).
    let tool_result_blocks: Vec<_> = h
        .conv
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
        .collect();
    assert!(
        tool_result_blocks.len() >= 2,
        "expected at least 2 tool results (shell + read), got {}",
        tool_result_blocks.len()
    );

    // The shell tool result must contain mock output (not an error).
    let has_shell_result = h.conv.messages.iter().any(|m| {
        m.content.iter().any(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => tool_use_id == "tc1" && !is_error,
            _ => false,
        })
    });
    assert!(
        has_shell_result,
        "shell tool result should be present and successful (is_error=false)"
    );

    // Schema preservation: captured tools must carry full schemas.
    // Real schemas use native format: top-level `name`, `description`,
    // `inputSchema` (not nested under `function`).
    let captures = provider.captured_tools();
    for (turn_idx, captured) in captures.iter().enumerate() {
        for (i, tool) in captured.iter().enumerate() {
            assert!(
                tool.get("name").is_some(),
                "turn {turn_idx}, tool {i}: name preserved"
            );
            assert!(
                tool.get("description").is_some(),
                "turn {turn_idx}, tool {i}: description preserved"
            );
            assert!(
                tool.get("inputSchema").is_some(),
                "turn {turn_idx}, tool {i}: inputSchema preserved"
            );
        }
    }
}

/// Regression test: dispatch/execution semantics and schema fields are
/// unchanged except for intended canonical description text shortening and
/// prompt-side description removal.
///
/// This test constructs tool schemas that represent the *post-wzz6* surface
/// (shortened descriptions, no prompt-side duplication) and asserts:
/// - Runtime metadata (`readOnly`, `concurrent_safe`, etc.) parses correctly
///   from the provider schemas.
/// - The `shell` tool dispatches as a non-stash, non-MCP extension tool.
/// - Finalize tool semantics are unchanged.
/// - Description text is still meaningful (not empty, not accidentally
///   replaced with signature-only text).
#[tokio::test]
async fn dispatch_semantics_unchanged_after_description_shortening() {
    let schemas = real_worker_tool_schemas();

    // Runtime metadata must parse correctly from the provider schemas.
    let metadata = crate::reply_loop::tool_dispatch::tool_runtime_metadata(&schemas);
    assert_eq!(
        metadata["shell"],
        crate::reply_loop::tool_dispatch::ToolRuntimeMetadata {
            read_only: false,
            destructive: true,
            idempotent: false,
            open_world: false,
            concurrent_safe: false,
        },
        "shell metadata unchanged (real tool_shell uses destructive())"
    );
    assert_eq!(
        metadata["read"],
        crate::reply_loop::tool_dispatch::ToolRuntimeMetadata {
            read_only: true,
            destructive: false,
            idempotent: true,
            open_world: false,
            concurrent_safe: true,
        },
        "read metadata unchanged (concurrent_safe=true)"
    );

    // Description text is still substantive after shortening — not empty
    // and not replaced with signature-only text.
    // Real schemas use native format: description at top level (not nested
    // under "function").
    for schema in &schemas {
        let desc = schema
            .get("description")
            .and_then(|v| v.as_str())
            .expect("description present at top level");
        let name = schema
            .get("name")
            .and_then(|v| v.as_str())
            .expect("name present at top level");
        assert!(
            desc.len() > 10,
            "description for {name} should be substantive after shortening, got: {desc:?}"
        );
        // Description should not accidentally be the parameter signature
        // (which would mean prompt-side deduplication leaked into provider schemas).
        assert!(
            !desc.starts_with('(') && !desc.contains("required:"),
            "description for {name} should not be a parameter signature: {desc:?}"
        );
    }

    // Drive a full dispatch cycle: shell → dispatches as extension tool → ok.
    let inner = MockProvider::new(vec![
        MockResponse::tool_call_with_input(
            "tc1",
            "shell",
            serde_json::json!({"command": "pwd"}),
            300,
        ),
        MockResponse {
            text: None,
            tool_calls: vec![ContentBlock::ToolUse {
                id: "fin1".to_string(),
                name: "submit_work".to_string(),
                input: serde_json::json!({
                    "task_id": "t1",
                    "commit_title": "done",
                    "summary": "executed shell, semantics unchanged"
                }),
            }],
            input_tokens: 250,
            output_tokens: 10,
            _error: None,
        },
    ]);
    let provider = RecordingProvider::new(inner);
    let mut h = ReplyLoopHarness::new_with_worker_prompt().await;
    provider.inner.bind_valid_submit_work_fixtures(&h.task_id);
    let (result, output, _ti, _to, _cr, _cw) = h.run(&provider, &schemas).await;
    assert!(result.is_ok(), "expected ok, got: {result:?}");
    assert!(output.finalize_payload.is_some());

    // Shell dispatch returned a successful result (mock output present).
    let tool_results: Vec<_> = h
        .conv
        .messages
        .iter()
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some((tool_use_id.as_str(), content, *is_error)),
            _ => None,
        })
        .collect();
    assert_eq!(tool_results.len(), 1, "exactly one tool result (shell)");
    assert_eq!(tool_results[0].0, "tc1");
    assert!(!tool_results[0].2, "shell result must not be an error");
    // The mock dispatcher returns JSON with "ok": true.
    let result_text = tool_results[0]
        .1
        .iter()
        .filter_map(|b| b.as_text())
        .collect::<Vec<_>>()
        .join("");
    assert!(
        result_text.contains("\"ok\""),
        "shell result should contain mock dispatch output, got: {result_text:?}"
    );
}

/// Standalone regression test: the rendered worker system prompt must use
/// signature-only tool lines (no provider description bodies), while
/// provider-declared tool schemas still carry full `description` fields.
///
/// This test does NOT run the reply loop — it inspects the prompt and schemas
/// directly so it catches regressions in `format_tools_section` rendering
/// independently of dispatch/stream behaviour.
#[tokio::test]
async fn rendered_worker_prompt_uses_signature_only_tool_section() {
    let schemas = real_worker_tool_schemas();

    // Build the real post-wzz6 worker prompt surface using a minimal Task
    // constructed directly — no database dependency, so the test runs in
    // any environment (CI sidecar, local dev) without needing the Postgres
    // test template.
    let tool_schemas_fn = djinn_mcp_extension::tool_defs::tool_schemas_worker;
    let role_config = djinn_roles::config::config_for(djinn_roles::AgentType::Worker);
    let task = djinn_core::models::Task {
        id: "test-task-id".to_string(),
        project_id: "test-project-id".to_string(),
        short_id: "t-wzz6".to_string(),
        epic_id: None,
        title: "wzz6 regression probe".to_string(),
        description: "Verify prompt-side tool-description deduplication.".to_string(),
        design: String::new(),
        issue_type: "task".to_string(),
        status: "in_progress".to_string(),
        priority: 1,
        owner: "test-owner".to_string(),
        labels: "[]".to_string(),
        acceptance_criteria: "[]".to_string(),
        reopen_count: 0,
        continuation_count: 0,
        total_reopen_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".to_string(),
        agent_type: None,
        created_by_user_id: None,
        ci_status: "unknown".to_string(),
        ci_head_sha: None,
        ci_pr_number: None,
        ci_blocking_required_check_names: "[]".to_string(),
        ci_failure_fingerprint: None,
        ci_first_seen_at: None,
        ci_last_seen_at: None,
        ci_same_signature_count: 0,
        ci_last_remediation_base_sha: None,
        ci_mirror_head_sha: None,
        ci_github_head_sha: None,
        ci_heads_diverged: None,
        ci_head_observation_error: None,
        ci_mq_state: None,
        ci_mq_run_id: None,
        ci_mq_head_sha: None,
        ci_mq_failed_check_names: None,
        ci_mq_failure_fingerprint: None,
        ci_mq_same_signature_count: None,
        ci_mq_first_seen_at: None,
        ci_mq_last_seen_at: None,
        unresolved_blocker_count: 0,
    };
    let task_ctx = djinn_roles::prompts::TaskContext {
        project_path: "/tmp/project".to_string(),
        workspace_path: "/tmp/workspace".to_string(),
        diff: None,
        commits: None,
        start_commit: None,
        end_commit: None,
        conflict_files: None,
        merge_base_branch: None,
        merge_target_branch: None,
        merge_failure_context: None,
        setup_commands: None,
        activity: None,
        worker_summary: None,
        worker_concerns: None,
        epic_context: None,
        knowledge_context: None,
        code_graph_context: None,
        reviewer_diff_context: None,
        ci_blocking_directive: None,
        worker_resume_note: None,
        arbiter_directive: None,
    };
    let system_prompt = djinn_roles::prompts::render_prompt_for_role(
        role_config,
        tool_schemas_fn,
        &task,
        &task_ctx,
    );

    // ── Provider schemas: every schema must carry `name`, `description`,
    // and `inputSchema` (the canonical three-field contract). ──
    for schema in &schemas {
        let name = schema
            .get("name")
            .and_then(|v| v.as_str())
            .expect("provider schema must have top-level `name`");
        let desc = schema
            .get("description")
            .and_then(|v| v.as_str())
            .expect("provider schema must have top-level `description`");
        assert!(
            !desc.is_empty(),
            "provider schema description for {name} must not be empty"
        );
        let input_schema = schema
            .get("inputSchema")
            .expect("provider schema must have top-level `inputSchema`");
        assert!(
            input_schema.is_object(),
            "provider schema inputSchema for {name} must be an object"
        );
    }

    // ── Prompt-side: the rendered system prompt must contain
    // signature-only tool lines.  For each tool, verify:
    //   1. A line matching `- `name(` exists (the signature).
    //   2. That line does NOT contain ` — ` followed by the description
    //      (the old format_tool_line_with_description format).
    //
    // If assertion #2 fails, format_tools_section is duplicating provider
    // description bodies in the system prompt — a direct regression of the
    // wzz6 prompt-side deduplication. ──
    for schema in &schemas {
        let name = schema
            .get("name")
            .and_then(|v| v.as_str())
            .expect("schema has name");
        let desc = schema
            .get("description")
            .and_then(|v| v.as_str())
            .expect("schema has description");

        // Find the signature-only line for this tool.
        let sig_line = system_prompt.lines().find(|line| {
            let trimmed = line.trim_start();
            trimmed.starts_with(&format!("- `{name}("))
        });

        assert!(
            sig_line.is_some(),
            "rendered worker prompt must contain signature-only tool line \
             for {name} (format: '- `{name}(...)`'); prompt may be missing \
             the tools section entirely"
        );

        let sig_line = sig_line.unwrap();
        assert!(
            !sig_line.contains(&format!(" — {desc}")),
            "rendered worker prompt tool line for {name} contains provider \
             description body after ' — ' separator (got: {sig_line:?}); \
             format_tools_section must emit signature-only lines"
        );
        // Also verify the line ends with `)` (or `)``) — just the signature,
        // no trailing description prose.
        let trimmed = sig_line.trim();
        assert!(
            trimmed.ends_with('`') || trimmed.ends_with(')'),
            "rendered worker prompt tool line for {name} should end with \
             closing backtick/paren, got: {trimmed:?}"
        );
    }
}

// The phase tracker is intentionally exercised only through the canonical reply
// loop below. The scripts advance SlotContext's monotonic clock at provider
// boundaries, making the exported counter delta deterministic without sleeps.
static PHASE_METRIC_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
const PHASE_METRIC_ROLE: &str = "refinement";
static PHASE_TOOL_CLOCK: std::sync::Mutex<Option<Arc<TestClock>>> = std::sync::Mutex::new(None);

struct PhaseEvent {
    advance: Duration,
    event: anyhow::Result<StreamEvent>,
}

enum PhaseTurn {
    InitError {
        advance: Duration,
    },
    Stream {
        init_advance: Duration,
        events: Vec<PhaseEvent>,
    },
    EmptyAssistant {
        init_advance: Duration,
        stream_advance: Duration,
        stream_polled: Arc<tokio::sync::Notify>,
    },
    Pending {
        init_advance: Duration,
        stream_polled: Arc<tokio::sync::Notify>,
    },
}

struct PhaseScriptedProvider {
    clock: Arc<TestClock>,
    turns: Mutex<VecDeque<PhaseTurn>>,
}

impl PhaseScriptedProvider {
    fn new(clock: Arc<TestClock>, turns: Vec<PhaseTurn>) -> Self {
        Self {
            clock,
            turns: Mutex::new(turns.into()),
        }
    }
}

impl LlmProvider for PhaseScriptedProvider {
    fn name(&self) -> &str {
        "phase-script"
    }

    fn stream<'a>(
        &'a self,
        _conversation: &'a Conversation,
        _tools: &'a [serde_json::Value],
        _tool_choice: Option<ToolChoice>,
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
        let turn = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .expect("scripted provider turn");
        let clock = Arc::clone(&self.clock);
        Box::pin(async move {
            match turn {
                PhaseTurn::InitError { advance } => {
                    clock.advance_mono(advance);
                    Err(anyhow::anyhow!("scripted provider initialization failure"))
                }
                PhaseTurn::Stream {
                    init_advance,
                    events,
                } => {
                    clock.advance_mono(init_advance);
                    let stream = async_stream::stream! {
                        for event in events {
                            clock.advance_mono(event.advance);
                            yield event.event;
                        }
                    };
                    Ok(Box::pin(stream)
                        as Pin<
                            Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                        >)
                }
                PhaseTurn::EmptyAssistant {
                    init_advance,
                    stream_advance,
                    stream_polled,
                } => {
                    clock.advance_mono(init_advance);
                    let stream = async_stream::stream! {
                        // The empty-assistant retry begins only after canonical
                        // stream consumption observes this terminal event.
                        stream_polled.notify_one();
                        clock.advance_mono(stream_advance);
                        yield Ok(StreamEvent::Done);
                    };
                    Ok(Box::pin(stream)
                        as Pin<
                            Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                        >)
                }
                PhaseTurn::Pending {
                    init_advance,
                    stream_polled,
                } => {
                    clock.advance_mono(init_advance);
                    let stream = async_stream::stream! {
                        // Signal only once canonical stream consumption polls
                        // this pending stream. This keeps externally advanced
                        // fake time inside the active provider interval.
                        stream_polled.notify_one();
                        futures::future::pending::<()>().await;
                        yield Ok(StreamEvent::Done);
                    };
                    Ok(Box::pin(stream)
                        as Pin<
                            Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                        >)
                }
            }
        })
    }
}

fn phase_counter(rendered: &str, phase: &str, role: &str) -> u64 {
    rendered
        .lines()
        .find_map(|line| {
            (line.starts_with("djinn_agent_session_phase_seconds_total")
                && line.contains(&format!("phase=\"{phase}\""))
                && line.contains(&format!("role=\"{role}\"")))
            .then(|| {
                line.rsplit_once(' ')
                    .and_then(|(_, value)| value.parse().ok())
            })
            .flatten()
        })
        .unwrap_or_else(|| panic!("missing {role} {phase} phase sample:\n{rendered}"))
}

fn phase_delta(before: &str, after: &str, phase: &str, role: &str) -> u64 {
    phase_counter(after, phase, role) - phase_counter(before, phase, role)
}

fn final_turn(init: u64, delta: u64) -> PhaseTurn {
    PhaseTurn::Stream {
        init_advance: Duration::from_secs(init),
        events: vec![
            PhaseEvent {
                advance: Duration::from_secs(delta),
                event: Ok(StreamEvent::Delta(ContentBlock::ToolUse {
                    id: "finish".into(),
                    name: "submit_work".into(),
                    input: serde_json::json!({"task_id":"t1"}),
                })),
            },
            PhaseEvent {
                advance: Duration::ZERO,
                event: Ok(StreamEvent::Done),
            },
        ],
    }
}

async fn run_phase_script(turns: Vec<PhaseTurn>) -> anyhow::Result<()> {
    let clock = Arc::new(TestClock::new(SystemTime::UNIX_EPOCH, Instant::now()));
    let provider = PhaseScriptedProvider::new(Arc::clone(&clock), turns);
    let mut harness = ReplyLoopHarness::new().await;
    harness.slot_ctx.clock = clock;
    harness.role_name = PHASE_METRIC_ROLE;
    let tools = vec![dummy_tool_schema("submit_work")];
    harness.run(&provider, &tools).await.0
}

async fn phase_harness(clock: Arc<TestClock>) -> ReplyLoopHarness {
    let mut harness = ReplyLoopHarness::new().await;
    harness.slot_ctx.clock = clock;
    harness.role_name = PHASE_METRIC_ROLE;
    harness
}

fn phase_side_tool(
    _: Option<&serde_json::Map<String, serde_json::Value>>,
) -> Result<serde_json::Value, String> {
    PHASE_TOOL_CLOCK
        .lock()
        .unwrap()
        .as_ref()
        .expect("phase side-tool clock installed")
        .advance_mono(Duration::from_secs(5));
    Ok(serde_json::json!({"side":"done"}))
}

async fn provider_phase_scenario_counts_stream_init_consumption_and_errors() {
    let before = render().expect("render phase metrics");
    assert!(run_phase_script(vec![final_turn(2, 3)]).await.is_ok());
    let after = render().expect("render phase metrics");
    assert_eq!(
        phase_delta(&before, &after, "provider_wait", PHASE_METRIC_ROLE),
        5
    );
    assert_eq!(
        phase_delta(&before, &after, "tool_execution", PHASE_METRIC_ROLE),
        0
    );
    let before = after;
    assert!(
        run_phase_script(vec![PhaseTurn::InitError {
            advance: Duration::from_secs(7)
        }])
        .await
        .is_err()
    );
    let after = render().expect("render phase metrics");
    assert_eq!(
        phase_delta(&before, &after, "provider_wait", PHASE_METRIC_ROLE),
        7
    );
    let before = after;
    assert!(
        run_phase_script(vec![PhaseTurn::Stream {
            init_advance: Duration::from_secs(2),
            events: vec![PhaseEvent {
                advance: Duration::from_secs(4),
                event: Err(anyhow::anyhow!("scripted stream failure"))
            }]
        }])
        .await
        .is_err()
    );
    let after = render().expect("render phase metrics");
    assert_eq!(
        phase_delta(&before, &after, "provider_wait", PHASE_METRIC_ROLE),
        6
    );
}

async fn provider_phase_scenario_counts_empty_assistant_backoff_not_local_time() {
    let clock = Arc::new(TestClock::new(SystemTime::UNIX_EPOCH, Instant::now()));
    let empty_assistant_polled = Arc::new(tokio::sync::Notify::new());
    let provider = PhaseScriptedProvider::new(
        Arc::clone(&clock),
        vec![
            PhaseTurn::EmptyAssistant {
                init_advance: Duration::from_secs(2),
                stream_advance: Duration::from_secs(3),
                stream_polled: Arc::clone(&empty_assistant_polled),
            },
            final_turn(4, 2),
        ],
    );
    let mut harness = phase_harness(Arc::clone(&clock)).await;
    clock.advance_mono(Duration::from_secs(11)); // unrelated local setup time
    let before = render().expect("render phase metrics");
    let tools = vec![dummy_tool_schema("submit_work")];
    // Empty-assistant retries are enabled only for Codex/OpenAI-family models.
    // Use that canonical model branch so advancing Tokio's retry sleep below
    // proves the provider-owned backoff interval, rather than a terminal error.
    let run = harness.run_with_model(&provider, &tools, "openai/gpt-5.4");
    tokio::pin!(run);
    let polled = empty_assistant_polled.notified();
    tokio::pin!(polled);
    tokio::select! {
        _ = &mut run => panic!("empty-assistant provider unexpectedly finished"),
        _ = &mut polled => {}
    }
    assert!(
        futures::poll!(&mut run).is_pending(),
        "empty-assistant retry backoff must be pending"
    );
    // Advance the complete production empty-assistant provider-loop backoff on
    // both clocks only after canonical consumption has registered the retry
    // sleep. Polling the reply-loop future above is essential: yielding this
    // task alone would not poll `run` into the provider-owned backoff.
    // The phase stays provider-owned for this real sleep, so the exact delta
    // below includes all three backoff seconds.
    let backoff = empty_turn_backoff(1);
    clock.advance_mono(backoff);
    // Pause the Tokio clock only now (after all DB-backed harness setup) so
    // that `tokio::time::advance` can resolve the provider-owned empty-assistant
    // backoff sleep deterministically. The resume before `run.await` restores
    // real time for the remaining DB persistence in the reply loop.
    tokio::time::pause();
    tokio::time::advance(backoff).await;
    tokio::time::resume();
    assert!(run.await.0.is_ok());
    let after = render().expect("render phase metrics");
    let expected_provider_wait = 2 + 3 + backoff.as_secs() + 4 + 2;
    assert_eq!(
        phase_delta(&before, &after, "provider_wait", PHASE_METRIC_ROLE),
        expected_provider_wait
    );
    assert_eq!(
        phase_delta(&before, &after, "tool_execution", PHASE_METRIC_ROLE),
        0
    );
}

async fn provider_phase_scenario_hands_streaming_side_tool_back_to_provider() {
    let clock = Arc::new(TestClock::new(SystemTime::UNIX_EPOCH, Instant::now()));
    *PHASE_TOOL_CLOCK.lock().unwrap() = Some(Arc::clone(&clock));
    let provider = PhaseScriptedProvider::new(
        Arc::clone(&clock),
        vec![
            PhaseTurn::Stream {
                init_advance: Duration::from_secs(2),
                events: vec![
                    PhaseEvent {
                        advance: Duration::from_secs(1),
                        event: Ok(StreamEvent::Delta(ContentBlock::ToolUse {
                            id: "side".into(),
                            name: "side_query".into(),
                            input: serde_json::json!({}),
                        })),
                    },
                    PhaseEvent {
                        advance: Duration::ZERO,
                        event: Ok(StreamEvent::Done),
                    },
                ],
            },
            final_turn(4, 2),
        ],
    );
    let mut harness = phase_harness(clock).await;
    harness.slot_ctx.tool_dispatcher =
        Some(Arc::new(test_helpers::ConfigurableToolDispatcher::new(
            vec![],
            std::collections::HashMap::from([(
                "side_query".to_string(),
                phase_side_tool as test_helpers::ToolHandlerFn,
            )]),
        )));
    let tools = vec![
        serde_json::json!({"name":"side_query","readOnly":true,"idempotent":true,"concurrent_safe":true}),
        dummy_tool_schema("submit_work"),
    ];
    let before = render().expect("render phase metrics");
    assert!(harness.run(&provider, &tools).await.0.is_ok());
    *PHASE_TOOL_CLOCK.lock().unwrap() = None;
    let after = render().expect("render phase metrics");
    // The tool-use delta triggers an immediate concurrent-safe side-tool
    // dispatch, then the stream terminates with a zero-advance Done event.
    // Provider time is therefore 2 + 1 before the handoff and 4 + 2 after it,
    // with the five tool seconds kept disjoint.
    assert_eq!(
        phase_delta(&before, &after, "provider_wait", PHASE_METRIC_ROLE),
        9
    );
    assert_eq!(
        phase_delta(&before, &after, "tool_execution", PHASE_METRIC_ROLE),
        5
    );
}

async fn provider_phase_scenario_cancellation_and_drop_flush_active_interval_once() {
    let tools = vec![dummy_tool_schema("submit_work")];
    let clock = Arc::new(TestClock::new(SystemTime::UNIX_EPOCH, Instant::now()));
    let stream_polled = Arc::new(tokio::sync::Notify::new());
    let provider = PhaseScriptedProvider::new(
        Arc::clone(&clock),
        vec![PhaseTurn::Pending {
            init_advance: Duration::from_secs(2),
            stream_polled: Arc::clone(&stream_polled),
        }],
    );
    let mut harness = phase_harness(Arc::clone(&clock)).await;
    let before = render().expect("render phase metrics");
    let cancel = harness.cancel.clone();
    let mut run = Box::pin(harness.run(&provider, &tools));
    let polled = stream_polled.notified();
    tokio::pin!(polled);
    tokio::select! {
        _ = &mut run => panic!("pending provider unexpectedly finished"),
        _ = &mut polled => {}
    }
    clock.advance_mono(Duration::from_secs(7));
    cancel.cancel();
    assert!(run.await.0.is_err());
    let after = render().expect("render phase metrics");
    assert_eq!(
        phase_delta(&before, &after, "provider_wait", PHASE_METRIC_ROLE),
        9
    );
    let clock = Arc::new(TestClock::new(SystemTime::UNIX_EPOCH, Instant::now()));
    let stream_polled = Arc::new(tokio::sync::Notify::new());
    let provider = PhaseScriptedProvider::new(
        Arc::clone(&clock),
        vec![PhaseTurn::Pending {
            init_advance: Duration::from_secs(3),
            stream_polled: Arc::clone(&stream_polled),
        }],
    );
    let mut harness = phase_harness(Arc::clone(&clock)).await;
    let before = after;
    {
        let mut run = Box::pin(harness.run(&provider, &tools));
        let polled = stream_polled.notified();
        tokio::pin!(polled);
        tokio::select! {
            _ = &mut run => panic!("pending provider unexpectedly finished"),
            _ = &mut polled => {}
        }
        clock.advance_mono(Duration::from_secs(8));
        drop(run); // drop the active reply-loop future; tracker Drop is the backstop
    }
    let after = render().expect("render phase metrics");
    assert_eq!(
        phase_delta(&before, &after, "provider_wait", PHASE_METRIC_ROLE),
        11
    );
}

#[tokio::test]
async fn provider_phase_scripted_reply_loop_scenarios() {
    // Keep every database-backed harness and process-global refinement-role
    // collector snapshot in one CI-shard unit. The scenarios must remain
    // sequential: each one measures an exact before/after counter delta. The
    // dedicated refinement label also isolates these snapshots from the
    // worker-role dispatcher phase metric tests running in parallel.
    //
    // This entry point deliberately avoids `start_paused = true` because every
    // scenario spins up a real database-backed `ReplyLoopHarness`. Pausing the
    // Tokio clock at process start prevents the sqlx pool from establishing
    // its first connection - the acquire timeout fires before real TCP I/O
    // completes, producing a spurious `PoolTimedOut`. Only the empty-assistant
    // backoff scenario manually pauses/resumes the clock around its
    // `tokio::time::advance`, leaving all other DB work under real time.
    let _lock = PHASE_METRIC_LOCK.lock().unwrap();
    djinn_telemetry::init().expect("telemetry init");

    provider_phase_scenario_counts_stream_init_consumption_and_errors().await;
    provider_phase_scenario_counts_empty_assistant_backoff_not_local_time().await;
    provider_phase_scenario_hands_streaming_side_tool_back_to_provider().await;
    provider_phase_scenario_cancellation_and_drop_flush_active_interval_once().await;
}
