use super::error_handling::{BudgetWindDownIgnored, supports_tool_choice_required};
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
use futures::stream;
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

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
    let cancel = CancellationToken::new();
    let db = test_helpers::create_test_db();
    let ctx = test_helpers::agent_context_from_db(db.clone(), cancel.clone());
    let project = test_helpers::create_test_project(&db).await;
    let epic = test_helpers::create_test_epic(&db, &project.id).await;
    let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
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
    (ctx, project_path, task.id, session.id, cancel)
}

async fn count_persisted_messages(slot_ctx: &crate::host::SlotContext, session_id: &str) -> usize {
    let repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    repo.load_conversation(session_id)
        .await
        .map(|c| c.messages.len())
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
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
    let session_id = session.id;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            provider,
            tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
            input: serde_json::json!({"task_id": "t1", "summary": "done"}),
        }],
        input_tokens: 100,
        output_tokens: 10,
        _error: None,
    }]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
                input: serde_json::json!({"task_id": "t1", "summary": "all done"}),
            }],
            input_tokens: 130,
            output_tokens: 10,
            _error: None,
        },
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
                input: serde_json::json!({"task_id": "t1", "summary": "done"}),
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
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
    clear_session_budget_env();
    let _ = &_env_guard;
    assert!(
        result.is_ok(),
        "token budget should wind down gracefully, got: {:?}",
        result
    );
    assert_eq!(provider.remaining(), 0, "summary turn should be consumed");
    assert_eq!(
        wind_down_directive_count(&conv),
        1,
        "token budget should inject the existing wind-down directive once"
    );
    assert_eq!(
        persisted_wind_down_directive_count(&slot_ctx, &session_id).await,
        1,
        "token-budget wind-down directive should be persisted"
    );
    let assistant_text = role_text(&conv, Role::Assistant);
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
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
        wind_down_directive_count(&conv),
        1,
        "token-budget wind-down extension is strictly one turn"
    );
    assert_eq!(
        persisted_wind_down_directive_count(&slot_ctx, &session_id).await,
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
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
    let user_text = role_text(&conv, Role::User);
    let correction_count = conv
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
    // initial-seed persistence pass, so persisted = conv.messages.len() - 1.
    // The 4th attempt's tool-result message is also not persisted because
    // the loop returns from the guard before appending it — but it is
    // likewise absent from `conv.messages`, so the invariant still holds.
    let persisted = count_persisted_messages(&slot_ctx, &session_id).await;
    let expected_persisted = conv.messages.len() - 1;
    assert_eq!(
        persisted,
        expected_persisted,
        "corrective message should be persisted with the transcript; expected \
         {expected_persisted} (conversation len {} minus 1 system prompt), got {persisted}",
        conv.messages.len()
    );
    let persisted_conversation =
        SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone())
            .load_conversation(&session_id)
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
            serde_json::json!({"task_id": "t1", "summary": "done"}),
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
        serde_json::json!({"task_id": "t1", "summary": "done"}),
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
                input: serde_json::json!({"task_id": "t1", "summary": "done"}),
            }],
            input_tokens: 10,
            output_tokens: 5,
            _error: None,
        },
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
                input: serde_json::json!({"task_id": "t1", "summary": "done"}),
            }],
            input_tokens: 5,
            output_tokens: 5,
            _error: None,
        },
    ]);
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
    // SAFETY: SESSION_BUDGET_ENV_LOCK held; restore baseline before asserting.
    clear_session_budget_env();
    let _ = &_env_guard;
    assert!(
        result.is_ok(),
        "session should complete normally below the soft threshold; got: {:?}",
        result
    );
    assert_eq!(provider.remaining(), 0, "all 6 scripted turns consumed");
    let reminder_count = count_system_reminder_messages(&conv);
    assert_eq!(
        reminder_count, 0,
        "no <system-reminder> converge directive should be injected while \
         cumulative spend is below the soft threshold; got {reminder_count}, full \
         conversation:\n{:#?}",
        conv.messages
    );
    let user_text = role_text(&conv, Role::User);
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
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, tokens_in, tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
    let user_text = role_text(&conv, Role::User);
    assert!(user_text.contains("You are out of steps"));
    let stage_outcome = StageOutcome::Parked {
        reason: ParkReason::Budget,
        summary: output.budget_wind_down_summary.clone(),
        wind_down_ignored: false,
        session_id: session_id.clone(),
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
        &task_id,
        &slot_ctx,
    )
    .await;
    let repo = TaskRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    let entries = repo.list_activity(&task_id).await.unwrap();
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
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, tokens_in, tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
        session_id: session_id.clone(),
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
    let repo = TaskRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    let entries = repo.list_activity(&task_id).await.unwrap();
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
            input: serde_json::json!({"task_id": "t1", "summary": "done"}),
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
            provider: &provider,
            tools: &tools,
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
            serde_json::json!({"task_id": task_id, "summary": "done", "files_changed": []}),
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
            finalize_tool_names: &["submit_work", "request_lead"],
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
        serde_json::json!({"task_id": task_id, "summary": "done", "files_changed": []}),
        100,
    )]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
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
            finalize_tool_names: &["submit_work", "request_lead"],
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
        serde_json::json!({"task_id": task_id, "summary": "done", "files_changed": []}),
        100,
    )]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a planner."));
    conv.push(Message::user("Plan the task."));
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
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
            finalize_tool_names: &["submit_work", "request_lead"],
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
        serde_json::json!({"task_id": task_id, "summary": "done", "files_changed": []}),
        100,
    )]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
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
            finalize_tool_names: &["submit_work", "request_lead"],
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
        serde_json::json!({"task_id": task_id, "summary": "done", "files_changed": []}),
        100,
    )]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
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
            finalize_tool_names: &["submit_work", "request_lead"],
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
            serde_json::json!({"task_id": task_id, "summary": "done", "files_changed": []}),
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
            finalize_tool_names: &["submit_work", "request_lead"],
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
        serde_json::json!({"task_id": task_id, "summary": "done", "files_changed": []}),
        100,
    )]);
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, output, _, _, _, _) = run_reply_loop(
        ReplyLoopContext {
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
            finalize_tool_names: &["submit_work", "request_lead"],
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
    let (slot_ctx, project_path, task_id, session_id, cancel) = make_context().await;
    let worktree_path = std::path::PathBuf::from("/tmp");
    let mut conv = Conversation::new();
    conv.push(Message::system("You are a worker."));
    conv.push(Message::user("Do the task."));
    let (result, _output, _tokens_in, _tokens_out, _cr, _cw) = run_reply_loop(
        ReplyLoopContext {
            provider: &provider,
            tools: &[],
            task_id: &task_id,
            task_short_id: "t1",
            session_id: &session_id,
            project_path: &project_path,
            worktree_path: &worktree_path,
            role_name: "worker",
            finalize_tool_names: &["submit_work", "request_lead"],
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
    let boundary_repo = SessionCompactionBoundaryRepository::new(slot_ctx.db.clone());
    let latest_completed = boundary_repo
        .latest_completed_boundary(&session_id)
        .await
        .unwrap();
    assert!(
        latest_completed.is_none(),
        "no completed boundary should exist after failed compaction"
    );

    // The raw conversation history must still be loadable and unchanged.
    let msg_repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    let persisted = msg_repo
        .load_conversation(&session_id)
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
