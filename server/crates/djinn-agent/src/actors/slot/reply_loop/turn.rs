// djinn:allow-oversize — reply loop orchestration remains intentionally co-located
// while rrdr budget wind-down hooks land; split-out is a separate refactor.
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use crate::output_parser::ParsedAgentOutput;
use crate::output_stash::OutputStash;
use djinn_core::events::DjinnEventEnvelope;
use djinn_db::SessionMessageRepository;
use djinn_provider::message::{ContentBlock, Conversation, Message, MessageMeta, Role};
use djinn_provider::provider::LlmProvider;
use djinn_provider::provider::telemetry;

use super::super::{runtime_env_diagnostics, runtime_fs_diagnostics};
use super::budget::{
    SessionBudgetPolicy, hard_budget_threshold_exceeded, soft_budget_threshold_exceeded,
};
use super::error_handling::{
    BudgetWindDownIgnored, MAX_COMPACTION_RETRIES, empty_turn_backoff, is_context_length_error,
    is_orphaned_tool_call_error, next_nudge_message, should_retry_after_tool_call_compaction,
    should_retry_empty_assistant_turn, should_retry_empty_stream, soft_budget_converge_message,
    tool_choice_for_turn, wind_down_message,
};
use super::loop_guard::{
    AssistantOutputSignature, LoopGuardCondition, LoopGuardError, LoopGuardReason, LoopGuardState,
    ToolCallSignature, ToolFailureClass,
};
use super::persistence::{persist_session_message, serialize_llm_input, serialize_message};
use super::streaming::{StreamLoopContext, StreamTurnState, consume_provider_stream};
use super::tool_dispatch::{ToolDispatchContext, collect_tool_results, tool_runtime_metadata};

fn permission_denial_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("permission denied")
        || lower.contains("access denied")
        || lower.contains("security denial")
        || lower.contains("security denied")
        || lower.contains("operation not permitted")
        || lower.contains("not allowed")
        || lower.contains("not authorized")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
}

fn push_fragment(fragments: &mut Vec<String>, value: String) {
    const MAX_FRAGMENTS: usize = 12;
    let normalized = value.replace('\n', "\\n").trim().to_string();
    if normalized.is_empty() {
        return;
    }
    let snippet: String = normalized.chars().take(160).collect();
    if fragments.len() >= MAX_FRAGMENTS {
        fragments.remove(0);
    }
    fragments.push(snippet);
}

fn corrective_message_for_loop_guard(condition: &LoopGuardCondition) -> Message {
    let signature = condition.offending_signature_label();
    Message::user(format!(
        "SYSTEM CORRECTION: You have repeated the same failing tool call signature \
         {observed} times: `{signature}`. Do not call this exact tool with these \
         exact normalized arguments again in this session. Choose a different \
         approach, change the arguments materially, or use an appropriate \
         finalize/escalation tool if you are blocked. Repeating this exact \
         failing signature again will terminate the session through the loop guard.",
        observed = condition.observed,
    ))
}

fn tool_result_text(content: &[ContentBlock]) -> String {
    content
        .iter()
        .filter_map(ContentBlock::as_text)
        .collect::<Vec<_>>()
        .join("\n")
}

fn classify_tool_failure(content: &[ContentBlock]) -> ToolFailureClass {
    if permission_denial_text(&tool_result_text(content)) {
        ToolFailureClass::PermissionOrSecurityDenial
    } else {
        ToolFailureClass::General
    }
}

fn loop_guard_error(condition: LoopGuardCondition, turns: u32, session_id: &str) -> anyhow::Error {
    let start_turn = turns.saturating_sub(condition.observed.saturating_sub(1));
    anyhow::Error::new(LoopGuardError {
        condition,
        turn_span: (start_turn, turns),
        session_id: session_id.to_string(),
    })
}

#[derive(Debug, Clone)]
pub(crate) enum WindDownReason {
    StepCap { max_turns: u32 },
    Budget { details: String },
}

impl WindDownReason {
    fn is_budget(&self) -> bool {
        matches!(self, Self::Budget { .. })
    }

    fn as_str(&self) -> &'static str {
        if self.is_budget() {
            "token_budget"
        } else {
            "turn_cap"
        }
    }

    fn details(&self) -> String {
        match self {
            Self::StepCap { max_turns } => format!("max turns ({max_turns}) reached"),
            Self::Budget { details } => details.clone(),
        }
    }

    fn hard_error(&self, max_turns: u32) -> anyhow::Error {
        match self {
            Self::StepCap { .. } => anyhow::anyhow!(
                "max turns ({}) exceeded without text-only response (wind-down summary \
                 directive was injected but the agent did not terminate)",
                max_turns
            ),
            Self::Budget { details } => BudgetWindDownIgnored {
                details: format!(
                    "{details}; hard token budget wind-down directive was injected but the agent did not produce a text-only summary"
                ),
            }
            .into(),
        }
    }
}

fn tool_call_signature_for_result(
    result_index: usize,
    tool_use_id: &str,
    turn_tool_calls: &[ContentBlock],
) -> Option<ToolCallSignature> {
    let by_id = turn_tool_calls
        .iter()
        .find(|block| matches!(block, ContentBlock::ToolUse { id, .. } if id == tool_use_id));
    let by_index = turn_tool_calls.get(result_index);
    by_id.or(by_index).and_then(|block| match block {
        ContentBlock::ToolUse { name, input, .. } => Some(ToolCallSignature::new(name, input)),
        _ => None,
    })
}

async fn inject_loop_guard_correction(
    msg_repo: &SessionMessageRepository,
    session_id: &str,
    task_id: &str,
    conversation: &mut Conversation,
    condition: &LoopGuardCondition,
) {
    let msg = corrective_message_for_loop_guard(condition);
    persist_session_message(msg_repo, session_id, task_id, &msg).await;
    conversation.push(msg);
}

/// rrdr: one-shot soft-budget converge reminder. On the first crossing of
/// the resolved soft threshold the helper persists + queues a
/// `<system-reminder>` asking the agent to converge. The `injected` flag
/// (owned by `run_reply_loop`) gates every subsequent call so the same
/// reminder cannot fire again on later turns that still see the threshold
/// exceeded.
#[allow(clippy::too_many_arguments)]
async fn maybe_inject_soft_budget_reminder(
    injected: &mut bool,
    session_budget: &super::budget::ResolvedSessionBudget,
    msg_repo: &SessionMessageRepository,
    session_id: &str,
    task_id: &str,
    role_name: &str,
    turns: u32,
    total_tokens_in: u32,
    total_tokens_out: u32,
    current_context_tokens: u32,
    conversation: &mut Conversation,
) -> bool {
    if *injected
        || !soft_budget_threshold_exceeded(
            session_budget,
            total_tokens_in,
            total_tokens_out,
            current_context_tokens,
        )
    {
        return false;
    }
    *injected = true;
    let cumulative_spend = total_tokens_in.saturating_add(total_tokens_out);
    let soft_cap =
        (session_budget.max_cumulative_tokens as f64) * session_budget.soft_threshold_ratio;
    tracing::warn!(
        task_id = %task_id,
        agent_type = %role_name,
        turns,
        total_tokens_in,
        total_tokens_out,
        cumulative_spend,
        max_cumulative_tokens = session_budget.max_cumulative_tokens,
        soft_cap = soft_cap as u64,
        soft_threshold_ratio = session_budget.soft_threshold_ratio,
        "ReplyLoop: soft budget threshold reached — injecting one-shot <system-reminder> \
         converge directive"
    );
    let msg = soft_budget_converge_message();
    persist_session_message(msg_repo, session_id, task_id, &msg).await;
    conversation.push(msg);
    true
}

pub(crate) struct ReplyLoopContext<'a> {
    pub provider: &'a dyn LlmProvider,
    pub tools: &'a [serde_json::Value],
    pub task_id: &'a str,
    pub task_short_id: &'a str,
    pub session_id: &'a str,
    pub project_path: &'a str,
    pub worktree_path: &'a std::path::Path,
    pub role_name: &'a str,
    /// Tool names that signal session completion (ADR-036).
    /// The first entry is the primary finalize tool; additional entries are
    /// alternate exit paths (e.g. `request_lead`).
    pub finalize_tool_names: &'a [&'a str],
    pub context_window: i64,
    pub model_id: &'a str,
    pub cancel: &'a tokio_util::sync::CancellationToken,
    pub global_cancel: &'a tokio_util::sync::CancellationToken,
    pub app_state: &'a crate::context::AgentContext,
    /// `SupervisorServices` handle used for routing host-only tool calls
    /// (`github_search`, `github_fetch_file`, `ci_job_log`) over RPC when the
    /// reply loop runs inside a worker Pod. On the host the same handle
    /// resolves to `DirectServices` and runs the call locally — same code
    /// path for both sides.
    pub services: &'a dyn djinn_supervisor::SupervisorServices,
    /// Optional MCP tool registry for dispatching tools to external MCP servers.
    pub mcp_registry: Option<&'a crate::mcp_client::McpToolRegistry>,
    /// Skill names active in this session (for Langfuse metadata).
    pub active_skill_names: &'a [String],
    /// MCP server names connected in this session (for Langfuse metadata).
    pub active_mcp_server_names: &'a [String],
    /// Optional override for the per-session step cap. Used by tests to
    /// exercise the graceful wind-down (G9) path with a tiny cap without
    /// driving the policy default number of mock turns. Production callers pass
    /// `None`, so the role/model-aware [`SessionBudgetPolicy`] decides.
    pub max_turns_override: Option<u32>,
}

/// Djinn-native reply loop. Drives an `LlmProvider` stream, dispatches tool
/// calls via the extension layer, and continues until the assistant produces a
/// text-only response or a termination condition is reached.
///
/// Context-length-exceeded errors trigger reactive compaction and retry
/// (up to `MAX_COMPACTION_RETRIES` times) before failing the session.
pub(crate) async fn run_reply_loop(
    ctx: ReplyLoopContext<'_>,
    conversation: &mut Conversation,
    is_resumed_session: bool,
) -> (anyhow::Result<()>, ParsedAgentOutput, i64, i64, i64, i64) {
    let ReplyLoopContext {
        provider,
        tools,
        task_id,
        task_short_id,
        session_id,
        project_path,
        worktree_path,
        role_name,
        finalize_tool_names,
        context_window,
        model_id,
        cancel,
        global_cancel,
        app_state,
        services,
        mcp_registry,
        active_skill_names,
        active_mcp_server_names,
        max_turns_override,
    } = ctx;

    let tool_metadata = tool_runtime_metadata(tools);

    // Register activity tracker — stall detection uses this to kill idle sessions.
    let activity_ts = app_state.register_activity(task_id);
    // Separate clock for the throttled worker→host activity-touch RPC.
    // The host's coordinator stall poller reads its OWN ActivityTracker, not
    // the worker's `activity_ts` above (K8s workers build a fresh empty
    // tracker at startup — see agent-worker main.rs). Calls to
    // `services.touch_activity(..)` route over RPC on the worker side
    // (`WorkerSupervisorServices` → `DirectServices::touch_activity`) and
    // bridge the two trackers. Initialised to 0 so the first touch always
    // fires immediately; subsequent touches throttle to once per
    // `TOUCH_ACTIVITY_RPC_INTERVAL_SECS` (in streaming.rs).
    let last_rpc_touch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Clock for the throttled mid-flight session-row token flush (see
    // `TOKEN_FLUSH_INTERVAL_SECS` in streaming.rs). Initialised to 0 so the
    // first generation's usage is persisted immediately.
    let last_token_flush = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // Session-scoped stash for full tool outputs that exceed truncation limits.
    // The agent can navigate stashed outputs via `output_view` and `output_grep`.
    let output_stash = Arc::new(Mutex::new(OutputStash::new()));

    let mut output =
        ParsedAgentOutput::new(role_name == "reviewer" || role_name == "task_reviewer");

    // Persist the conversation as it unfolds so post-session knowledge
    // extraction (and the durable UI transcript) has something to read. A
    // resumed session's conversation was reloaded FROM `session_messages`, so
    // re-persisting it would duplicate every row — only seed the opening
    // turn(s) for fresh sessions. The system prompt is intentionally skipped
    // (large, static, and not part of the transcript — mirrors the chat path).
    let msg_repo = SessionMessageRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if !is_resumed_session {
        for msg in conversation
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
        {
            persist_session_message(&msg_repo, session_id, task_id, msg).await;
        }
    }

    // Token counts and last assistant text are declared outside the async block
    // so they survive the borrow and can be used for telemetry/return values.
    let mut total_tokens_in: u32 = 0;
    let mut total_tokens_out: u32 = 0;
    // Prompt-cache accounting (ADR-043): cumulative cache-read / cache-write /
    // reasoning token aggregates summed across all turns, for telemetry.
    let mut total_cache_read: u32 = 0;
    let mut total_cache_write: u32 = 0;
    let mut total_reasoning_out: u32 = 0;
    // Tracks the actual context-window fill for the most recent generation.
    // Each generation sends the entire conversation, so `usage.input` IS the
    // current context size — it overwrites the previous value rather than
    // accumulating.  This is the correct metric for the compaction threshold
    // and for the usage_pct SSE event.  `total_tokens_in` is kept as a
    // billing / telemetry aggregate (sum across all turns).
    let mut current_context_tokens: u32 = 0;
    let mut final_assistant_text = String::new();

    // Resumed sessions may respond text-only if the model determines the
    // reviewer's concerns are already addressed in the existing code.

    // ── Create session-level OTel span (root trace) ──────────────────────────
    let otel_session = if telemetry::is_active() {
        let session = telemetry::SessionSpan::start(&telemetry::SessionSpanAttributes {
            provider: provider.name(),
            model: model_id,
            task_short_id,
            task_id,
            agent_type: role_name,
            session_id,
        });
        // Record active skills and MCP servers as metadata.
        session.record_skills(active_skill_names);
        session.record_mcp_servers(active_mcp_server_names);
        // Record system prompt from the first message.
        if let Some(sys_msg) = conversation.messages.first()
            && sys_msg.role == Role::System
        {
            let sys_text: String = sys_msg
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            if !sys_text.is_empty() {
                session.record_system_prompt(&sys_text);
            }
        }
        // Record the user message as the trace-level input
        // (shows in the Langfuse trace list Input column).
        // For resumed sessions use the *last* user message (reviewer feedback);
        // for fresh sessions use the first.
        let input_msg = if is_resumed_session {
            conversation.messages.iter().rfind(|m| m.role == Role::User)
        } else {
            conversation.messages.iter().find(|m| m.role == Role::User)
        };
        if let Some(user_msg) = input_msg {
            let input_text: String = user_msg
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            if !input_text.is_empty() {
                session.record_trace_input(&input_text);
            }
        }
        Some(session)
    } else {
        None
    };

    let run_result: anyhow::Result<()> = async {
        let mut saw_any_event = false;
        let mut assistant_message_count: usize = 0;
        let mut assistant_fragments: Vec<String> = Vec::new();
        let mut compaction_attempts: u32 = 0;
        let mut empty_turn_retries: u32 = 0;
        // Consecutive text-only turns without a finalize or tool call (for nudge loop).
        let mut consecutive_nudge_count: u32 = 0;

        // Deterministic in-session guard over repeated failing tool-call
        // signatures. A signature gets exactly one corrective turn at the
        // threshold; repeating it after that terminates through LoopGuardError.
        let mut tool_failure_guard_state = LoopGuardState::default();
        let mut corrected_tool_failure_signatures: HashSet<ToolCallSignature> = HashSet::new();

        // Track the last assistant text for output parsing.
        let mut last_assistant_text = String::new();

        let mut turns: u32 = 0;
        let session_budget = SessionBudgetPolicy::from_env()
            .unwrap_or_else(|err| {
                tracing::warn!(
                    error = %err,
                    "ReplyLoop: invalid session budget configuration; using defaults"
                );
                SessionBudgetPolicy::default()
            })
            .resolve(role_name, model_id, context_window, max_turns_override);
        let max_turns = session_budget.effective_max_turns;
        tracing::debug!(
            task_id = %task_id,
            session_id = %session_id,
            agent_type = %session_budget.role_name,
            model_id = %session_budget.model_id,
            context_window_tokens = session_budget.context_window_tokens,
            context_window_known = session_budget.context_window_known,
            max_turns,
            max_cumulative_tokens = session_budget.max_cumulative_tokens,
            soft_threshold_ratio = session_budget.soft_threshold_ratio,
            hard_threshold_ratio = session_budget.hard_threshold_ratio,
            "ReplyLoop: resolved session budget policy"
        );
        // G9: graceful step-cap wind-down. When the loop reaches the final
        // permitted turn we inject a one-shot directive asking the agent to
        // summarize work done + what remains, then allow EXACTLY ONE more turn
        // to capture that hand-off (instead of hard-erroring with no context).
        // `wind_down_injected` makes the step-cap extension strictly one turn:
        // once the wind-down turn runs, the next cap check falls through to the
        // hard error if the agent still hasn't reached a terminal action.
        // Budget-triggered wind-downs may happen far before `max_turns`, so they
        // need their own post-injection state to re-enter the stop branch after
        // exactly one granted provider turn even while `turns < max_turns`.
        let mut wind_down_injected = false;
        let mut wind_down_reason: Option<WindDownReason> = None;
        let mut budget_wind_down_final_turn_spent = false;
        // rrdr: one-shot soft-budget converge reminder flag — see
        // `maybe_inject_soft_budget_reminder` for the contract.
        let mut soft_budget_reminder_injected = false;

        loop {
            let hard_budget_exceeded = hard_budget_threshold_exceeded(
                &session_budget,
                total_tokens_in,
                total_tokens_out,
                current_context_tokens,
            );
            let budget_wind_down_should_stop = budget_wind_down_final_turn_spent
                && wind_down_reason
                    .as_ref()
                    .is_some_and(WindDownReason::is_budget);
            if turns >= max_turns
                || (!wind_down_injected && hard_budget_exceeded)
                || budget_wind_down_should_stop
            {
                if !wind_down_injected {
                    // One turn before the hard cap: inject the wind-down
                    // directive and grant a single extra turn for the summary.
                    let reason = if hard_budget_exceeded && turns < max_turns {
                        let cumulative_spend = total_tokens_in.saturating_add(total_tokens_out);
                        let hard_cap = (session_budget.max_cumulative_tokens as f64)
                            * session_budget.hard_threshold_ratio;
                        WindDownReason::Budget {
                            details: format!(
                                "hard token budget threshold reached (hard budget threshold reached): cumulative_tokens={cumulative_spend}, hard_cap={}, max_cumulative_tokens={}, hard_threshold_ratio={}",
                                hard_cap as u64,
                                session_budget.max_cumulative_tokens,
                                session_budget.hard_threshold_ratio
                            ),
                        }
                    } else {
                        WindDownReason::StepCap { max_turns }
                    };
                    let wind_down_reason_label = reason.as_str();
                    let budget_triggered = reason.is_budget();
                    wind_down_reason = Some(reason);
                    wind_down_injected = true;
                    tracing::warn!(
                        task_id = %task_id,
                        agent_type = %role_name,
                        turns,
                        max_turns,
                        wind_down_reason = wind_down_reason_label,
                        budget_triggered,
                        total_tokens_in,
                        total_tokens_out,
                        current_context_tokens,
                        max_cumulative_tokens = session_budget.max_cumulative_tokens,
                        hard_threshold_ratio = session_budget.hard_threshold_ratio,
                        "ReplyLoop: wind-down threshold reached — injecting summary directive \
                         (one final turn before hard stop)"
                    );
                    let msg = wind_down_message();
                    persist_session_message(&msg_repo, session_id, task_id, &msg).await;
                    conversation.push(msg);
                    // Do NOT increment `turns`: the wind-down turn below is the
                    // single granted extension. The next iteration re-enters
                    // this branch with `wind_down_injected == true` and hard-errors.
                } else {
                    // The agent ignored the wind-down (or produced no terminal
                    // action) — fall back to the existing hard-error behavior.
                    let reason = wind_down_reason
                        .clone()
                        .unwrap_or(WindDownReason::StepCap { max_turns });
                    tracing::warn!(
                        task_id = %task_id,
                        session_id = %session_id,
                        agent_type = %role_name,
                        turns,
                        max_turns,
                        wind_down_ignored = true,
                        wind_down_reason = reason.as_str(),
                        "ReplyLoop: wind-down directive ignored; stopping session"
                    );
                    return Err(reason.hard_error(max_turns));
                }
            }

            // rrdr soft-threshold converge reminder. Runs at the same
            // pre-turn branch as the G9 step-cap check (BEFORE `turns += 1`)
            // so the reminder is queued ahead of the next provider stream
            // and counted in the upcoming turn's input.
            maybe_inject_soft_budget_reminder(
                &mut soft_budget_reminder_injected,
                &session_budget,
                &msg_repo,
                session_id,
                task_id,
                role_name,
                turns,
                total_tokens_in,
                total_tokens_out,
                current_context_tokens,
                conversation,
            )
            .await;
            turns += 1;

            let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
            tracing::info!(
                task_id = %task_id,
                session_id = %session_id,
                turn = turns,
                worktree = %worktree_path.display(),
                "ReplyLoop: starting provider stream; {}",
                env_diag
            );

            // ── Start OTel generation span for this turn ─────────────────────
            let otel_llm = otel_session.as_ref().map(|session| {
                let llm = telemetry::LlmSpan::start(
                    session.context(),
                    provider.name(),
                    model_id,
                    turns,
                );
                let input = serialize_llm_input(conversation, tools);
                llm.record_input(&serde_json::to_string(&input).unwrap());
                llm
            });

            // ── Start streaming from the provider ────────────────────────────
            // Only force tool_choice=required for providers known to handle it
            // well.  Many reasoning models (Kimi K2.5, GLM-4.7, Qwen 3.5)
            // reject or mishandle "required" when thinking mode is active.
            let tool_choice = tool_choice_for_turn(model_id, tools);
            let stream_result = provider.stream(conversation, tools, tool_choice).await;
            let stream = match stream_result {
                Ok(s) => s,
                Err(e) if (is_context_length_error(&e) || is_orphaned_tool_call_error(&e)) && compaction_attempts < MAX_COMPACTION_RETRIES => {
                    // Reactive compaction: context exceeded or orphaned tool
                    // call references on stream init.
                    tracing::warn!(
                        task_id = %task_id,
                        compaction_attempts,
                        error = %e,
                        "ReplyLoop: recoverable provider error on stream init; compacting reactively"
                    );
                    if let Some(llm) = otel_llm {
                        llm.end_error("context_length_exceeded");
                    }
                    let compacted = crate::compaction::compact_conversation(
                        provider, conversation, session_id, task_id,
                        crate::compaction::CompactionContext::MidSession(role_name.to_string()),
                        context_window,
                    ).await;
                    if compacted {
                        total_tokens_in = 0;
                        total_tokens_out = 0;
                        current_context_tokens = 0;
                        compaction_attempts += 1;
                        output_stash.lock().unwrap().clear();
                        conversation.push(djinn_provider::message::Message::user(
                            "Continue with the task.",
                        ));
                        continue;
                    }
                    return Err(anyhow::anyhow!(
                        "context_length_exceeded and reactive compaction failed"
                    ));
                }
                Err(e) => {
                    if let Some(llm) = otel_llm {
                        llm.end_error(&e.to_string());
                    }
                    let diag = runtime_fs_diagnostics(project_path, worktree_path);
                    let env_diag = runtime_env_diagnostics(session_id, project_path, worktree_path);
                    return Err(anyhow::anyhow!(
                        "provider stream init failed: display={} debug={:?}; {}; {}",
                        e, e, diag, env_diag
                    ));
                }
            };

            let dispatch_ctx = ToolDispatchContext {
                app_state,
                services,
                task_id,
                worktree_path,
                role_name,
                mcp_registry,
                tool_metadata: &tool_metadata,
                output_stash: Arc::clone(&output_stash),
                otel_session: otel_session.as_ref(),
            };
            let stream_state = consume_provider_stream(StreamLoopContext {
                provider,
                stream,
                tool_metadata: &tool_metadata,
                dispatch: &dispatch_ctx,
                task_id,
                session_id,
                role_name,
                project_path,
                worktree_path,
                context_window,
                app_state,
                cancel,
                global_cancel,
                activity_ts: &activity_ts,
                last_rpc_touch: &last_rpc_touch,
                last_token_flush: &last_token_flush,
                services,
                compaction_attempts,
                current_context_tokens: &mut current_context_tokens,
                total_tokens_in: &mut total_tokens_in,
                total_tokens_out: &mut total_tokens_out,
                total_cache_read: &mut total_cache_read,
                total_cache_write: &mut total_cache_write,
                total_reasoning_out: &mut total_reasoning_out,
            })
            .await?;

            let StreamTurnState {
                turn_text,
                turn_thinking,
                turn_provider_state,
                turn_tool_calls,
                turn_tokens_in,
                turn_tokens_out,
                turn_cache_read,
                turn_cache_write,
                turn_reasoning_out,
                interrupted,
                saw_round_event,
                needs_reactive_compaction,
                streaming_results,
                streaming_dispatched,
            } = stream_state;
            saw_any_event |= saw_round_event;

            // ── End OTel generation span for this turn ───────────────────────
            if let Some(llm) = otel_llm {
                if interrupted.is_some() {
                    llm.end_error("interrupted");
                } else {
                    llm.record_usage(turn_tokens_in, turn_tokens_out);
                    llm.record_cache_usage(
                        turn_cache_read,
                        turn_cache_write,
                        turn_reasoning_out,
                    );
                    // Record assistant output text (current turn, not stale).
                    if !turn_text.is_empty() {
                        llm.record_output(&turn_text);
                    }
                    // Record thinking/reasoning content on the generation metadata.
                    if !turn_thinking.is_empty() {
                        llm.record_thinking(&turn_thinking);
                    }
                    // Record tool call names on the generation span.
                    let tool_names: Vec<String> = turn_tool_calls
                        .iter()
                        .filter_map(|tc| {
                            if let ContentBlock::ToolUse { name, .. } = tc {
                                Some(name.clone())
                            } else {
                                None
                            }
                        })
                        .collect();
                    llm.record_tool_calls(&tool_names);
                    llm.end_ok();
                }
            }

            if let Some(reason) = interrupted {
                return Err(anyhow::anyhow!(reason));
            }

            // ── Reactive compaction: mid-stream context overflow ─────────────
            if needs_reactive_compaction {
                tracing::warn!(
                    task_id = %task_id,
                    compaction_attempts,
                    "ReplyLoop: context_length_exceeded mid-stream; compacting reactively"
                );
                let compacted = crate::compaction::compact_conversation(
                    provider, conversation, session_id, task_id,
                    crate::compaction::CompactionContext::MidSession(role_name.to_string()),
                    context_window,
                ).await;
                if compacted {
                    total_tokens_in = 0;
                    total_tokens_out = 0;
                    current_context_tokens = 0;
                    compaction_attempts += 1;
                    output_stash.lock().unwrap().clear();
                    conversation.push(djinn_provider::message::Message::user(
                        "Continue with the task.",
                    ));
                    continue;
                }
                return Err(anyhow::anyhow!(
                    "context_length_exceeded and reactive compaction failed"
                ));
            }

            if !saw_round_event {
                if let Some(next_retry) = should_retry_empty_stream(saw_round_event, empty_turn_retries) {
                    empty_turn_retries = next_retry;
                    let backoff = empty_turn_backoff(empty_turn_retries);
                    tracing::warn!(
                        task_id = %task_id,
                        retry = empty_turn_retries,
                        backoff_secs = backoff.as_secs(),
                        "ReplyLoop: provider stream ended without events; backing off then retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                let diag = runtime_fs_diagnostics(project_path, worktree_path);
                return Err(anyhow::anyhow!(
                    "provider returned {} empty/no-event turns — the backend likely refused or \
                     throttled the request (a ChatGPT-account Codex rate limit returns empty 200s, \
                     not 429s; verify provider/account status or switch to an API-key backend); {}",
                    empty_turn_retries, diag
                ));
            }

            // ── Build the assistant message from this turn ───────────────────
            let mut assistant_content: Vec<ContentBlock> = Vec::new();
            assistant_content.extend(turn_provider_state);
            if !turn_thinking.is_empty() {
                assistant_content.push(ContentBlock::Thinking { thinking: turn_thinking.clone() });
            }
            if !turn_text.is_empty() {
                push_fragment(&mut assistant_fragments, format!("text:{}", turn_text));
                last_assistant_text = turn_text.clone();
                final_assistant_text = turn_text.clone();
                assistant_content.push(ContentBlock::Text { text: turn_text.clone() });
            }
            for tool_call in &turn_tool_calls {
                if let ContentBlock::ToolUse { id, .. } = tool_call {
                    push_fragment(&mut assistant_fragments, format!("tool_use:{}", id));
                }
                assistant_content.push(tool_call.clone());
            }

            if assistant_content.is_empty() {
                if let Some(next_retry) =
                    should_retry_empty_assistant_turn(assistant_content.is_empty(), empty_turn_retries)
                {
                    empty_turn_retries = next_retry;
                    let backoff = empty_turn_backoff(empty_turn_retries);
                    tracing::warn!(
                        task_id = %task_id,
                        retry = empty_turn_retries,
                        backoff_secs = backoff.as_secs(),
                        "ReplyLoop: provider returned empty assistant turn; backing off then retrying"
                    );
                    tokio::time::sleep(backoff).await;
                    continue;
                }
                let diag = runtime_fs_diagnostics(project_path, worktree_path);
                return Err(anyhow::anyhow!(
                    "provider returned {} empty assistant turns — the backend likely refused or \
                     throttled the request (a ChatGPT-account Codex rate limit returns empty 200s, \
                     not 429s; verify provider/account status or switch to an API-key backend); {}",
                    empty_turn_retries, diag
                ));
            }
            // Reset retry counter on successful content.
            empty_turn_retries = 0;

            let assistant_msg = Message {
                role: Role::Assistant,
                content: assistant_content,
                metadata: Some(MessageMeta {
                    input_tokens: Some(turn_tokens_in),
                    output_tokens: Some(turn_tokens_out),
                    timestamp: Some(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                    ),
                    provider_data: None,
                }),
            };

            assistant_message_count += 1;

            // Emit the complete assistant message as an SSE event.
            app_state.event_bus.send(DjinnEventEnvelope::session_message(
                session_id,
                task_id,
                role_name,
                &serialize_message(&assistant_msg),
            ));

            // Durably persist the turn before it can be discarded by a
            // subsequent compaction (which replaces/truncates the in-memory
            // conversation a few lines below).
            persist_session_message(&msg_repo, session_id, task_id, &assistant_msg).await;

            conversation.push(assistant_msg);

            // rrdr budget park: after a hard-budget wind-down directive, the
            // single granted provider turn must be a text-only hand-off summary.
            // Tool calls (including finalize tools) or an empty response are an
            // ignored wind-down, not permission to keep running until the normal
            // step cap. Mark the final turn spent and re-enter the pre-turn stop
            // branch immediately; do not dispatch the requested tools or consume
            // any later fallback text as a false successful summary.
            if wind_down_injected
                && wind_down_reason
                    .as_ref()
                    .is_some_and(WindDownReason::is_budget)
            {
                let summary = turn_text.trim();
                if turn_tool_calls.is_empty() && !summary.is_empty() {
                    output.budget_wind_down_summary = Some(summary.to_string());
                    output.budget_wind_down_details =
                        wind_down_reason.as_ref().map(WindDownReason::details);
                    tracing::info!(
                        task_id = %task_id,
                        agent_type = %role_name,
                        turns,
                        assistant_message_count,
                        wind_down_reason = "budget",
                        "ReplyLoop: wind-down summary captured — session complete (graceful step-cap stop)"
                    );
                    break;
                }

                budget_wind_down_final_turn_spent = true;
                continue;
            }

            // ── Compaction threshold check ────────────────────────────────────
            if crate::compaction::needs_compaction(current_context_tokens, context_window) {
                tracing::info!(
                    task_id = %task_id,
                    current_context_tokens,
                    context_window,
                    usage_pct = current_context_tokens as f64 / context_window as f64,
                    "ReplyLoop: compaction threshold reached, compacting"
                );
                let compacted = crate::compaction::compact_conversation(
                    provider,
                    conversation,
                    session_id,
                    task_id,
                    crate::compaction::CompactionContext::MidSession(role_name.to_string()),
                    context_window,
                )
                .await;
                if compacted {
                    // Reset token counters — the compacted conversation is much
                    // smaller.  The next turn's usage report will set accurate
                    // values for the new context size.
                    total_tokens_in = 0;
                    total_tokens_out = 0;
                    current_context_tokens = 0;
                    output_stash.lock().unwrap().clear();

                    // After compaction the conversation was replaced — the
                    // assistant message containing this turn's ToolUse blocks
                    // is gone. If we dispatched tool calls and appended
                    // ToolResults, those would reference call_ids that no
                    // longer exist, causing "No tool call found for function
                    // call output" from the OpenAI API.
                    //
                    // Skip tool dispatch and let the LLM produce a fresh
                    // response against the compacted conversation.
                    if should_retry_after_tool_call_compaction(compacted, !turn_tool_calls.is_empty()) {
                        compaction_attempts += 1;
                        conversation.push(Message::user(
                            "Continue with the task.",
                        ));
                        continue;
                    }
                }
                // Text-only after compaction = worker thinks it's done.
                // Let it fall through to the normal text-only exit below —
                // the reviewer will validate and resume if needed.
            }

            // ── Finalize-tool detection (ADR-036) ────────────────────────────
            // The primary finalize tool (first in the list, e.g. submit_work) is
            // a virtual tool — its payload is captured here and processed after
            // the loop by finalize_handlers. We break immediately (no dispatch).
            //
            // Alternate finalize tools (e.g. request_lead) are real extension tools
            // that must be dispatched to execute their side effects. We mark them
            // but fall through to the dispatch section; a post-dispatch check
            // breaks the loop after the tool results are collected.
            let primary_finalize = finalize_tool_names.first().copied().unwrap_or("");
            if let Some(finalize_call) = turn_tool_calls
                .iter()
                .find(|tc| matches!(tc, ContentBlock::ToolUse { name, .. } if name == primary_finalize))
            {
                let payload = if let ContentBlock::ToolUse { input, .. } = finalize_call {
                    input.clone()
                } else {
                    serde_json::Value::Null
                };
                tracing::info!(
                    task_id = %task_id,
                    agent_type = %role_name,
                    finalize_tool = %primary_finalize,
                    turns,
                    assistant_message_count,
                    "ReplyLoop: primary finalize tool called — session complete"
                );
                output.finalize_payload = Some(payload);
                output.finalize_tool_name = Some(primary_finalize.to_string());
                break;
            }
            // Check for alternate finalize tools — mark but don't break yet.
            let alternate_finalize = turn_tool_calls
                .iter()
                .find(|tc| matches!(tc, ContentBlock::ToolUse { name, .. } if finalize_tool_names[1..].contains(&name.as_str())))
                .and_then(|tc| if let ContentBlock::ToolUse { name, input, .. } = tc {
                    Some((name.clone(), input.clone()))
                } else {
                    None
                });

            if alternate_finalize.is_none() && !turn_text.trim().is_empty() {
                let signature = AssistantOutputSignature::from_content_blocks(
                    turn_text.trim().to_string(),
                    &turn_tool_calls,
                );
                if let Some(condition) = tool_failure_guard_state.record_assistant_output(signature)
                {
                    tracing::warn!(
                        task_id = %task_id,
                        session_id = %session_id,
                        kind = ?condition.kind(),
                        signature = %condition.offending_signature_label(),
                        observed = condition.observed,
                        threshold = condition.threshold,
                        "ReplyLoop: repeated assistant-output signature reached threshold; terminating"
                    );
                    return Err(loop_guard_error(condition, turns, session_id));
                }
            }

            // ── G9 wind-down: capture the one-shot summary and end gracefully ─
            // If the wind-down directive was injected, THIS is the single
            // granted final turn. A text-only response IS the hand-off summary:
            // it was captured into `last_assistant_text` / `final_assistant_text`
            // and persisted above, so end the session now instead of nudging.
            // If the agent instead kept calling tools, hard-error now: the
            // single extra model turn was consumed, and dispatching ignored
            // tool calls would let unrelated loop guards preempt this reason.
            if wind_down_injected && turn_tool_calls.is_empty() {
                let fallback_reason = WindDownReason::StepCap { max_turns };
                let reason = wind_down_reason.as_ref().unwrap_or(&fallback_reason);
                tracing::info!(
                    task_id = %task_id,
                    agent_type = %role_name,
                    wind_down_reason = reason.as_str(),
                    turns,
                    assistant_message_count,
                    "ReplyLoop: wind-down summary captured — session complete"
                );
                break;
            }
            if wind_down_injected {
                let reason = wind_down_reason
                    .clone()
                    .unwrap_or(WindDownReason::StepCap { max_turns });
                return Err(reason.hard_error(max_turns));
            }

            // ── Nudge loop: text-only without finalize ────────────────────────
            if let Some((next_nudge_count, nudge_message)) = next_nudge_message(
                !turn_tool_calls.is_empty(),
                !tools.is_empty(),
                consecutive_nudge_count,
                finalize_tool_names,
            )? {
                consecutive_nudge_count = next_nudge_count;
                tracing::warn!(
                    task_id = %task_id,
                    agent_type = %role_name,
                    nudge = consecutive_nudge_count,
                    finalize_tools = %finalize_tool_names.join("` or `"),
                    "ReplyLoop: text-only turn without finalize — injecting nudge"
                );
                conversation.push(nudge_message);
                continue;
            }
            if turn_tool_calls.is_empty() && tools.is_empty() {
                // No tools registered at all → text-only is a valid session end.
                tracing::info!(
                    task_id = %task_id,
                    agent_type = %role_name,
                    turns,
                    assistant_message_count,
                    "ReplyLoop: text-only turn (no tools) — session complete"
                );
                break;
            }

            // Non-finalize tool calls: reset nudge counter and dispatch normally.
            consecutive_nudge_count = 0;

            let tool_result_blocks = collect_tool_results(
                &turn_tool_calls,
                streaming_results,
                &streaming_dispatched,
                &tool_metadata,
                &dispatch_ctx,
            )
            .await;

            let signatures_corrected_before_dispatch = corrected_tool_failure_signatures.clone();
            let mut loop_guard_condition_to_inject: Option<LoopGuardCondition> = None;
            let tool_batch_has_success = tool_result_blocks.iter().any(|result_block| {
                matches!(
                    result_block,
                    ContentBlock::ToolResult {
                        is_error: false,
                        ..
                    }
                )
            });
            if tool_batch_has_success {
                tool_failure_guard_state.record_tool_success();
                corrected_tool_failure_signatures.clear();
            }
            for (result_index, result_block) in tool_result_blocks.iter().enumerate() {
                let ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } = result_block
                else {
                    continue;
                };

                let Some(signature) =
                    tool_call_signature_for_result(result_index, tool_use_id, &turn_tool_calls)
                else {
                    tracing::warn!(
                        task_id = %task_id,
                        tool_use_id = %tool_use_id,
                        "ReplyLoop: tool result could not be correlated to a tool-use signature"
                    );
                    continue;
                };

                if *is_error {
                    let failure_text = tool_result_text(content);
                    let condition = if tool_batch_has_success {
                        tool_failure_guard_state.record_tool_failure_after_progress(
                            signature.clone(),
                            classify_tool_failure(content),
                        )
                    } else {
                        tool_failure_guard_state
                            .record_tool_failure(signature.clone(), classify_tool_failure(content))
                    };
                    if let Some(condition) = condition {
                        if matches!(&condition.reason, LoopGuardReason::RepeatedToolFailure { .. })
                            && !signatures_corrected_before_dispatch.contains(&signature)
                        {
                            tracing::warn!(
                                task_id = %task_id,
                                signature = %signature.display_label(),
                                observed = condition.observed,
                                threshold = condition.threshold,
                                failure = %failure_text,
                                "ReplyLoop: repeated failing tool-call signature reached threshold; injecting corrective message"
                            );
                            corrected_tool_failure_signatures.insert(signature);
                            loop_guard_condition_to_inject.get_or_insert(condition);
                        } else {
                            tracing::warn!(
                                task_id = %task_id,
                                signature = %signature.display_label(),
                                kind = ?condition.kind(),
                                observed = condition.observed,
                                threshold = condition.threshold,
                                failure = %failure_text,
                                "ReplyLoop: tool-failure loop guard reached threshold; terminating"
                            );
                            return Err(loop_guard_error(condition, turns, session_id));
                        }
                    }
                }
            }

            // Touch activity after tool execution — tool calls are legitimate
            // work and can take a while (e.g. cargo build).
            {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                activity_ts.store(now, Ordering::Relaxed);
                // Also bridge to the host's ActivityTracker. Unthrottled
                // here because this fires at most once per LLM turn that
                // dispatched tools — a tiny fraction of the per-token
                // stream-event volume that streaming.rs throttles. Update
                // `last_rpc_touch` so the streaming loop on the next turn
                // doesn't re-touch immediately. Fire-and-forget — local
                // tracker still serves worker diagnostics.
                last_rpc_touch.store(now, Ordering::Relaxed);
                if let Err(e) = services.touch_activity(task_id.to_string()).await {
                    tracing::warn!(
                        task_id = %task_id,
                        error = %e,
                        "reply_loop: post-tool touch_activity RPC failed; \
                         host stall poller may see stale idle for this turn"
                    );
                }
            }

            // Push a user message containing all tool results.
            let tool_result_msg = Message {
                role: Role::User,
                content: tool_result_blocks,
                metadata: None,
            };
            persist_session_message(&msg_repo, session_id, task_id, &tool_result_msg).await;
            conversation.push(tool_result_msg);

            if let Some(condition) = loop_guard_condition_to_inject {
                inject_loop_guard_correction(
                    &msg_repo,
                    session_id,
                    task_id,
                    conversation,
                    &condition,
                )
                .await;
            }

            // ── Post-dispatch finalize check for alternate finalize tools ─────
            // Alternate finalize tools (e.g. request_lead) were dispatched above
            // so their side effects ran. Now break the loop.
            if let Some((name, payload)) = alternate_finalize {
                tracing::info!(
                    task_id = %task_id,
                    agent_type = %role_name,
                    finalize_tool = %name,
                    turns,
                    assistant_message_count,
                    "ReplyLoop: alternate finalize tool dispatched — session complete"
                );
                output.finalize_payload = Some(payload);
                output.finalize_tool_name = Some(name);
                break;
            }

            // Continue to next turn.
        }

        if !saw_any_event {
            let diag = runtime_fs_diagnostics(project_path, worktree_path);
            return Err(anyhow::anyhow!(
                "provider session produced no events; {}",
                diag
            ));
        }

        // Parse the final assistant text for runtime errors and reviewer feedback.
        if !last_assistant_text.is_empty() {
            output.ingest_text(&last_assistant_text);
        }

        tracing::info!(
            task_id = %task_id,
            agent_type = %role_name,
            saw_any_event,
            assistant_message_count,
            turns,
            finalize_called = output.finalize_payload.is_some(),
            "ReplyLoop: session completed normally"
        );

        Ok(())
    }
    .await;

    // ── End session-level OTel span ──────────────────────────────────────────
    if let Some(session) = otel_session {
        session.record_usage(total_tokens_in, total_tokens_out);
        session.record_cache_usage(total_cache_read, total_cache_write, total_reasoning_out);
        // Record the last assistant text as the trace-level output
        // (shows in the Langfuse trace list Output column).
        if !final_assistant_text.is_empty() {
            session.record_trace_output(&final_assistant_text);
        }
        match &run_result {
            Ok(()) => session.end_ok(),
            Err(e) => session.end_error(&e.to_string()),
        }
    }

    // Deregister activity tracker — session is done.
    app_state.deregister_activity(task_id);

    (
        run_result,
        output,
        total_tokens_in as i64,
        total_tokens_out as i64,
        total_cache_read as i64,
        total_cache_write as i64,
    )
}
