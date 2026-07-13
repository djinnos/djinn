// djinn:allow-oversize — reply loop orchestration remains intentionally co-located
// while rrdr budget wind-down hooks land; split-out is a separate refactor.
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use crate::helpers::{runtime_env_diagnostics, runtime_fs_diagnostics};
use crate::host::SlotContext;
use crate::output_parser::ParsedAgentOutput;
use djinn_compaction::{
    COMPACTION_SUMMARY_END_MARKER, CompactionContext, compact_conversation, needs_compaction,
    strip_compaction_markers,
};
use djinn_core::events::DjinnEventEnvelope;
use djinn_db::SessionMessageRepository;
use djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository;
use djinn_git::{SubmissionDiffFingerprint, compute_submission_diff_fingerprint};
use djinn_provider::message::{ContentBlock, Conversation, Message, MessageMeta, Role};
use djinn_provider::provider::LlmProvider;
use djinn_provider::provider::telemetry;

use crate::lifecycle::teardown::settle_no_progress_submission;

use super::budget::{
    SessionBudgetPolicy, hard_budget_threshold_exceeded, soft_budget_threshold_exceeded,
};
use super::compaction_guard::CompactionCriticalSection;
use super::error_handling::{
    BudgetWindDownIgnored, MAX_COMPACTION_RETRIES, empty_start_streak_feeds_breaker,
    empty_turn_backoff, empty_turn_is_reasoning_only, is_context_length_error,
    is_orphaned_tool_call_error, is_provider_failure_prose, next_nudge_message,
    reasoning_only_nudge_message, should_retry_after_tool_call_compaction,
    should_retry_empty_assistant_turn, should_retry_empty_stream, soft_budget_converge_message,
    tool_choice_for_turn, wind_down_message,
};
use super::loop_guard::{
    AssistantOutputSignature, LoopGuardCondition, LoopGuardError, LoopGuardReason, LoopGuardState,
    ToolCallSignature, ToolFailureClass,
};
use super::phase::SessionPhaseTracker;
use super::persistence::{
    complete_compaction_boundary, flush_in_flight_turn, persist_session_message,
    record_compaction_started, serialize_llm_input, serialize_message,
};
use super::streaming::{StreamLoopContext, StreamTurnState, consume_provider_stream};
use super::tool_dispatch::{ToolDispatchContext, collect_tool_results, tool_runtime_metadata};
use djinn_db::SessionCompactionBoundaryRepository;

/// True when `model_id` (a `provider/model` string) is served by the Codex /
/// OpenAI consumer backend that signals over-quota by answering a turn with an
/// empty 200.
fn is_codex_openai_family(model_id: &str) -> bool {
    let provider = model_id
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    matches!(provider.as_str(), "openai" | "chatgpt_codex")
}

/// Typed terminal error for empty/no-event turns that signals a provider failure
/// suitable for failover.  Carries a [`ProviderError`] source so downstream
/// breaker/failover logic can `downcast_ref` and branch on the class.
///
/// Non-Codex providers fail on the first terminal empty occurrence (no retries).
/// Codex/OpenAI consumers may retry up to `MAX_EMPTY_TURN_RETRIES` because they
/// signal over-quota by answering with an empty 200.
fn empty_turn_terminal_error(
    kind: &str,
    retries: u32,
    model_id: &str,
    diag: String,
) -> anyhow::Error {
    let class = if is_codex_openai_family(model_id) {
        djinn_provider::provider::ProviderError::EmptyCompletion
    } else {
        djinn_provider::provider::ProviderError::ProviderInternal { status: 500 }
    };
    anyhow::Error::new(class).context(format!(
        "provider returned {retries} empty {kind} turns — the backend likely refused or \
         throttled the request (a ChatGPT-account Codex rate limit returns empty 200s, \
         not 429s; verify provider/account status or switch to an API-key backend); {diag}"
    ))
}

/// Build a typed provider-failure error from assistant text that was classified
/// as disguised rate-limit / quota / out-of-credits / provider error prose.
///
/// The returned error carries a [`ProviderError::RateLimit`] source so
/// downstream failover logic can `downcast_ref` and branch on the class.
fn provider_failure_prose_error(snippet: &str) -> anyhow::Error {
    anyhow::Error::new(djinn_provider::provider::ProviderError::RateLimit {
        retry_after_ms: None,
    })
    .context(format!(
        "assistant text classified as provider failure prose (not genuine model output) — \
         the model likely echoed back a rate-limit/quota/provider error; snippet: {snippet:?}"
    ))
}

/// Build a typed provider-failure error for a stream that ended early (without
/// `StreamEvent::Done`) after producing partial assistant content.
///
/// Observed in-flight content is already persisted via
/// [`persistence::flush_in_flight_turn`] for resume/timeline visibility, but
/// the truncated turn must **not** be finalized as a successful assistant
/// message.  This error signals that to the reply loop.
fn truncated_stream_error(text_len: usize, tool_call_count: usize) -> anyhow::Error {
    anyhow::Error::new(djinn_provider::provider::ProviderError::ProviderInternal { status: 500 })
        .context(format!(
            "provider stream ended early (without Done) after producing partial assistant content \
         (text_len={text_len}, tool_calls={tool_call_count}); the truncated output has been \
         flushed as observed artifacts for resume but must not be persisted as a complete turn"
        ))
}

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

/// Result of the no-progress integrity guard check.
#[derive(Debug, Clone, PartialEq, Eq)]
enum NoProgressGuardResult {
    /// No guard action needed — proceed with normal finalize.
    Allow,
    /// First identical no-progress submit: inject the corrective tool result and
    /// continue the reply loop so the worker can make substantive changes.
    FirstBounceCorrective(String),
    /// Second consecutive identical no-progress submit for the same rejected
    /// fingerprint: settle the session as a typed `no_progress_submission`
    /// through the normal settle path and route into planner intervention.
    SecondStrikeSettle,
}

/// Check if a worker's `submit_work` finalize call should be intercepted because
/// the current submission diff fingerprint matches the latest rejected fingerprint
/// for this task.
///
/// Returns [`NoProgressGuardResult::FirstBounceCorrective`] on the first
/// identical no-progress submit (the caller injects the corrective tool result
/// and continues the loop). Returns [`NoProgressGuardResult::SecondStrikeSettle`]
/// when the guard already triggered once for this fingerprint in the current
/// session — the caller should settle the session as a typed
/// `no_progress_submission` and route into planner intervention.
///
/// Returns [`NoProgressGuardResult::Allow`] when:
/// - The role is not a worker or the primary finalize tool is not `submit_work`
/// - No latest rejected fingerprint exists (no-comparison path; telemetry only)
/// - The current fingerprint differs from the latest rejected (new content accepted)
/// - Fingerprint computation fails (existing empty-diff safeguards remain intact)
///
/// `guard_already_triggered` is `true` when the guard already produced a
/// corrective result in this reply-loop session. It distinguishes first-bounce
/// (corrective) from second-strike (settlement) behavior.
async fn check_worker_submit_no_progress_guard(
    role_name: &str,
    primary_finalize: &str,
    task_id: &str,
    worktree_path: &std::path::Path,
    slot_ctx: &SlotContext,
    guard_already_triggered: bool,
) -> NoProgressGuardResult {
    // Gate only worker `submit_work` finalize calls.
    if role_name != "worker" || primary_finalize != "submit_work" {
        return NoProgressGuardResult::Allow;
    }
    // Load the latest rejected fingerprint for this task.
    let rejected_repo = TaskRejectedSubmissionIntegrityRepository::new(slot_ctx.db.clone());
    let latest_rejected = match rejected_repo.latest_for_task(task_id).await {
        Ok(record) => record,
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "submit_integrity: failed to load latest rejected fingerprint; \
                 skipping comparison (existing empty-diff safeguards remain)"
            );
            return NoProgressGuardResult::Allow;
        }
    };
    let Some(rejected) = latest_rejected else {
        tracing::debug!(
            task_id = %task_id,
            "submit_integrity: no latest rejected fingerprint for task; \
             skipping comparison (no-comparison historical path)"
        );
        return NoProgressGuardResult::Allow;
    };
    // Compute the current complete submission diff fingerprint.
    let current_fp = match compute_submission_diff_fingerprint(worktree_path).await {
        Ok(SubmissionDiffFingerprint::Diff(digest)) => {
            tracing::info!(
                task_id = %task_id,
                fingerprint_len = digest.fingerprint.len(),
                changed_paths = ?digest.changed_paths,
                "submit_integrity: computed current submission fingerprint"
            );
            digest.fingerprint
        }
        Ok(SubmissionDiffFingerprint::NoDiff(no_diff)) => {
            tracing::info!(
                task_id = %task_id,
                worktree_path = %worktree_path.display(),
                merge_base = ?no_diff.merge_base,
                canonical_diff_len = no_diff.canonical_diff_len,
                "submit_integrity: no diff in worktree for fingerprint; \
                 existing empty-diff safeguards will apply"
            );
            return NoProgressGuardResult::Allow;
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                worktree_path = %worktree_path.display(),
                error = %e,
                "submit_integrity: failed to compute submission fingerprint; \
                 skipping comparison (existing empty-diff safeguards remain)"
            );
            return NoProgressGuardResult::Allow;
        }
    };
    // Compare current fingerprint with latest rejected.
    if current_fp == rejected.diff_fingerprint {
        if guard_already_triggered {
            // Second consecutive identical no-progress submit in this session.
            // Settle as typed no_progress_submission and route to planner
            // intervention.
            tracing::warn!(
                task_id = %task_id,
                rejected_at = %rejected.rejected_at,
                rejected_fingerprint = %rejected.diff_fingerprint,
                no_progress_streak = rejected.no_progress_streak,
                "submit_integrity: second consecutive identical no-progress \
                 submit_work for the same rejected fingerprint; settling as \
                 no_progress_submission"
            );
            NoProgressGuardResult::SecondStrikeSettle
        } else {
            // First identical no-progress submit. Inject a corrective tool
            // result so the worker can make substantive changes.
            tracing::warn!(
                task_id = %task_id,
                rejected_at = %rejected.rejected_at,
                rejected_fingerprint = %rejected.diff_fingerprint,
                no_progress_streak = rejected.no_progress_streak,
                "submit_integrity: worker submit_work matches latest rejected fingerprint; \
                 intercepting first no-progress submit with corrective tool result"
            );
            NoProgressGuardResult::FirstBounceCorrective(format!(
                "Your submission was intercepted: the diff fingerprint of your current \
                 worktree is identical to the latest rejected submission for this task \
                 (rejected at {rejected_at}). You have not made substantive changes since \
                 the rejection. You MUST make meaningful code changes before calling \
                 submit_work again. Review the rejection feedback, address the issues, \
                 and modify actual source files before resubmitting.",
                rejected_at = rejected.rejected_at,
            ))
        }
    } else {
        tracing::info!(
            task_id = %task_id,
            current_fingerprint = %current_fp,
            rejected_fingerprint = %rejected.diff_fingerprint,
            "submit_integrity: current fingerprint differs from latest rejected; \
             allowing submit_work to proceed"
        );
        NoProgressGuardResult::Allow
    }
}

/// Extract the `tool_use_id` from the first `ContentBlock::ToolUse` whose name
/// matches `target_name`.
fn find_tool_use_id<'a>(tool_calls: &'a [ContentBlock], target_name: &str) -> Option<&'a str> {
    tool_calls.iter().find_map(|tc| {
        if let ContentBlock::ToolUse { id, name, .. } = tc {
            if name == target_name {
                Some(id.as_str())
            } else {
                None
            }
        } else {
            None
        }
    })
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

/// One-shot soft-budget converge reminder.
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

/// Context for the reply loop, adapted for the slot crate.
pub struct ReplyLoopContext<'a> {
    pub provider: &'a dyn LlmProvider,
    pub tools: &'a [serde_json::Value],
    pub task_id: &'a str,
    pub task_short_id: &'a str,
    pub session_id: &'a str,
    pub project_path: &'a str,
    pub worktree_path: &'a std::path::Path,
    pub role_name: &'a str,
    pub finalize_tool_names: &'a [&'a str],
    pub context_window: i64,
    pub model_id: &'a str,
    pub cancel: &'a tokio_util::sync::CancellationToken,
    pub global_cancel: &'a tokio_util::sync::CancellationToken,
    pub ctx: &'a SlotContext,
    pub active_skill_names: &'a [String],
    pub active_mcp_server_names: &'a [String],
    pub max_turns_override: Option<u32>,
    /// Shared compaction critical section for this slot/reply-loop session.
    ///
    /// The reply loop enters it around every `compact_conversation` call and
    /// releases it on every exit path. Actor/pool command routing can observe
    /// the same instance to decide whether to defer or demote work that would
    /// otherwise apply to the pre-rotation transcript.
    pub compaction_cs: &'a CompactionCriticalSection,
}

/// Djinn-native reply loop. Drives an `LlmProvider` stream, dispatches tool
/// calls via the extension layer, and continues until the assistant produces a
/// text-only response or a termination condition is reached.
pub async fn run_reply_loop(
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
        ctx: slot_ctx,
        active_skill_names,
        active_mcp_server_names,
        max_turns_override,
        compaction_cs,
    } = ctx;
    let tool_dispatcher = match slot_ctx.tool_dispatcher.as_ref() {
        Some(d) => d.as_ref(),
        None => {
            return (
                Err(anyhow::anyhow!(
                    "reply loop requires a SlotToolDispatcher; none was provided in SlotContext"
                )),
                ParsedAgentOutput::new(role_name == "reviewer" || role_name == "task_reviewer"),
                0,
                0,
                0,
                0,
            );
        }
    };
    let tool_metadata = tool_runtime_metadata(tools);
    let phase_tracker = Arc::new(Mutex::new(SessionPhaseTracker::new(slot_ctx, role_name)));
    // Register activity tracker.
    let activity_ts = slot_ctx.register_activity(task_id);
    let last_rpc_touch = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let last_token_flush = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut output =
        ParsedAgentOutput::new(role_name == "reviewer" || role_name == "task_reviewer");
    // Persist the conversation as it unfolds.
    let msg_repo = SessionMessageRepository::new(slot_ctx.db.clone(), slot_ctx.event_bus.clone());
    if !is_resumed_session {
        for msg in conversation
            .messages
            .iter()
            .filter(|m| m.role != Role::System)
        {
            persist_session_message(&msg_repo, session_id, task_id, msg).await;
        }
    }
    let mut total_tokens_in: u32 = 0;
    let mut total_tokens_out: u32 = 0;
    let mut total_cache_read: u32 = 0;
    let mut total_cache_write: u32 = 0;
    let mut total_reasoning_out: u32 = 0;
    let mut current_context_tokens: u32 = 0;
    let mut final_assistant_text = String::new();
    let otel_session = if telemetry::is_active() {
        let session = telemetry::SessionSpan::start(&telemetry::SessionSpanAttributes {
            provider: provider.name(),
            model: model_id,
            task_short_id,
            task_id,
            agent_type: role_name,
            session_id,
        });
        session.record_skills(active_skill_names);
        session.record_mcp_servers(active_mcp_server_names);
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
        let is_codex = is_codex_openai_family(model_id);
        let mut consecutive_nudge_count: u32 = 0;
        // Early breaker-feed guard: count zero-progress turns *before* the
        // session's first productive turn so a model that accepts a dispatch and
        // produces nothing fails over quickly instead of only after the
        // coordinator's 30-minute stall kill. `saw_productive_turn` disarms it
        // once real work happens, so a lone mid-session empty turn never trips.
        let mut empty_start_turns: u32 = 0;
        let mut saw_productive_turn = false;
        let mut tool_failure_guard_state = LoopGuardState::default();
        let mut corrected_tool_failure_signatures: HashSet<ToolCallSignature> = HashSet::new();
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
        let mut wind_down_injected = false;
        let mut wind_down_reason: Option<WindDownReason> = None;
        let mut budget_wind_down_final_turn_spent = false;
        let mut soft_budget_reminder_injected = false;
        // Track whether the no-progress integrity guard already intercepted a
        // submit_work call in this session. On the first identical rejected
        // fingerprint match we inject a corrective tool result; on the second
        // we settle the session as a typed no_progress_submission.
        let mut no_progress_guard_triggered = false;
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
                    let reason = if hard_budget_exceeded && turns < max_turns {
                        let cumulative_spend = total_tokens_in.saturating_add(total_tokens_out);
                        let hard_cap = (session_budget.max_cumulative_tokens as f64)
                            * session_budget.hard_threshold_ratio;
                        WindDownReason::Budget {
                            details: format!(
                                "hard token budget threshold reached: cumulative_tokens={cumulative_spend}, hard_cap={}, max_cumulative_tokens={}, hard_threshold_ratio={}",
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
                } else {
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
            let otel_llm = otel_session.as_ref().map(|session| {
                let llm = telemetry::LlmSpan::start(
                    session.context(),
                    provider.name(),
                    model_id,
                    turns,
                );
                let input = serialize_llm_input(conversation, tools);
                llm.record_input(&serde_json::to_string(&input).unwrap_or_default());
                llm
            });
            let tool_choice = tool_choice_for_turn(model_id, tools);
            // Repair any dangling tool calls (assistant tool_use ids with no
            // persisted result) left by a prior session that was cancelled
            // mid-tool-execution. Replaying such a transcript verbatim 400s the
            // whole request on strict OpenAI/Anthropic-compatible APIs, wedging
            // the task on every redispatch. Sanitizing here — one pass on the
            // provider-agnostic conversation — covers all wire formats without
            // mutating stored history.
            let request_conversation = conversation.with_synthesized_tool_results();
            phase_tracker.lock().unwrap_or_else(std::sync::PoisonError::into_inner).enter_provider_wait();
            let stream_result = provider
                .stream(request_conversation.as_ref(), tools, tool_choice)
                .await;
            let stream = match stream_result {
                Ok(s) => s,
                Err(e) if (is_context_length_error(&e) || is_orphaned_tool_call_error(&e))
                    && compaction_attempts < MAX_COMPACTION_RETRIES =>
                {
                    tracing::warn!(
                        task_id = %task_id,
                        compaction_attempts,
                        error = %e,
                        "ReplyLoop: recoverable provider error on stream init; compacting reactively"
                    );
                    if let Some(llm) = otel_llm {
                        llm.end_error("context_length_exceeded");
                    }
                    let compacted = compact_conversation_in_critical_section(
                        provider,
                        conversation,
                        session_id,
                        task_id,
                        role_name,
                        context_window,
                        compaction_cs,
                        slot_ctx,
                    )
                    .await;
                    if compacted {
                        total_tokens_in = 0;
                        total_tokens_out = 0;
                        current_context_tokens = 0;
                        compaction_attempts += 1;
                        tool_dispatcher.clear_stash();
                        conversation.push(Message::user("Continue with the task."));
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
                ctx: slot_ctx,
                task_id,
                worktree_path,
                role_name,
                tool_metadata: &tool_metadata,
                tool_dispatcher,
                otel_session: otel_session.as_ref(),
            };
            let mut stream_state = consume_provider_stream(StreamLoopContext {
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
                ctx: slot_ctx,
                cancel,
                global_cancel,
                activity_ts: &activity_ts,
                last_rpc_touch: &last_rpc_touch,
                last_token_flush: &last_token_flush,
                compaction_attempts,
                current_context_tokens: &mut current_context_tokens,
                total_tokens_in: &mut total_tokens_in,
                total_tokens_out: &mut total_tokens_out,
                total_cache_read: &mut total_cache_read,
                total_cache_write: &mut total_cache_write,
                total_reasoning_out: &mut total_reasoning_out,
            })
            .await?;
            phase_tracker.lock().unwrap_or_else(std::sync::PoisonError::into_inner).exit_provider_wait();
            // Flush any observed assistant/tool content before returning on
            // interrupt, cancellation, or early stream end.  This persists the
            // in-flight turn so it survives session release and is visible on
            // resume/timeline.  The flush is idempotent: repeated calls within the
            // same turn are no-ops once the turn is marked flushed.
            let has_in_flight_content =
                !stream_state.turn_text.is_empty()
                || !stream_state.turn_thinking.is_empty()
                || !stream_state.turn_provider_state.is_empty()
                || !stream_state.turn_tool_calls.is_empty()
                || !stream_state.streaming_results.is_empty();
            let should_flush = stream_state.interrupted.is_some()
                || (stream_state.early_stream_end && has_in_flight_content);
            if should_flush {
                let now_ts = slot_ctx.clock.now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                flush_in_flight_turn(&msg_repo, session_id, task_id, now_ts, &mut stream_state).await;
            }
            // Normal completion: the turn will be finalized below.  Mark it
            // flushed so any future caller cannot accidentally re-persist the
            // same rows.
            if stream_state.interrupted.is_none() && !stream_state.early_stream_end {
                stream_state.turn_flushed = true;
            }
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
                early_stream_end,
                turn_flushed: _,
            } = stream_state;
            saw_any_event |= saw_round_event;
            if let Some(llm) = otel_llm {
                if interrupted.is_some() || early_stream_end {
                    llm.end_error(if interrupted.is_some() { "interrupted" } else { "truncated" });
                } else {
                    llm.record_usage(turn_tokens_in, turn_tokens_out);
                    llm.record_cache_usage(
                        turn_cache_read,
                        turn_cache_write,
                        turn_reasoning_out,
                    );
                    if !turn_text.is_empty() {
                        llm.record_output(&turn_text);
                    }
                    if !turn_thinking.is_empty() {
                        llm.record_thinking(&turn_thinking);
                    }
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
            // AC3: When the provider stream ended early (without StreamEvent::Done)
            // and partial content was produced, the truncated turn must not be
            // persisted as a complete assistant message.  The in-flight flush
            // above already preserved observed artifacts for resume; now return
            // a typed provider failure so the reply loop exits cleanly.
            if early_stream_end && has_in_flight_content {
                return Err(truncated_stream_error(
                    turn_text.len(),
                    turn_tool_calls.len(),
                ));
            }
            if needs_reactive_compaction {
                tracing::warn!(
                    task_id = %task_id,
                    compaction_attempts,
                    "ReplyLoop: context_length_exceeded mid-stream; compacting reactively"
                );
                let compacted = compact_conversation_in_critical_section(
                    provider,
                    conversation,
                    session_id,
                    task_id,
                    role_name,
                    context_window,
                    compaction_cs,
                    slot_ctx,
                )
                .await;
                if compacted {
                    total_tokens_in = 0;
                    total_tokens_out = 0;
                    current_context_tokens = 0;
                    compaction_attempts += 1;
                    tool_dispatcher.clear_stash();
                    conversation.push(Message::user("Continue with the task."));
                    continue;
                }
                return Err(anyhow::anyhow!(
                    "context_length_exceeded and reactive compaction failed"
                ));
            }
            if !saw_round_event {
                if let Some(next_retry) = should_retry_empty_stream(saw_round_event, empty_turn_retries, is_codex) {
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
                return Err(empty_turn_terminal_error(
                    "no-event",
                    empty_turn_retries,
                    model_id,
                    diag,
                ));
            }
            let mut assistant_content: Vec<ContentBlock> = Vec::new();
            assistant_content.extend(turn_provider_state);
            if !turn_thinking.is_empty() {
                assistant_content.push(ContentBlock::Thinking { thinking: turn_thinking.clone(), signature: None });
            }
            if !turn_text.is_empty() {
                push_fragment(&mut assistant_fragments, format!("text:{turn_text}"));
                last_assistant_text = turn_text.clone();
                final_assistant_text = turn_text.clone();
                assistant_content.push(ContentBlock::Text { text: turn_text.clone() });
            }
            for tool_call in &turn_tool_calls {
                if let ContentBlock::ToolUse { id, .. } = tool_call {
                    push_fragment(&mut assistant_fragments, format!("tool_use:{id}"));
                }
                assistant_content.push(tool_call.clone());
            }
            if assistant_content.is_empty() {
                if empty_turn_is_reasoning_only(turn_reasoning_out) {
                    tracing::warn!(
                        task_id = %task_id,
                        session_id = %session_id,
                        turn = turns,
                        reasoning_tokens_out = turn_reasoning_out,
                        "ReplyLoop: reasoning-only empty turn — nudging instead of failing over"
                    );
                    let nudge = reasoning_only_nudge_message();
                    persist_session_message(&msg_repo, session_id, task_id, &nudge).await;
                    conversation.push(nudge);
                    continue;
                }
                // Session-start early breaker-feed guard: this turn produced no
                // usable output and (being past the reasoning-only branch) no
                // token usage — a zero-progress turn. While the session has not
                // yet made real progress, count it; a short run of these means
                // the provider accepted the dispatch and is doing nothing, so
                // fail over NOW (feeding the host breaker via the typed terminal
                // error) instead of waiting for the coordinator's 30-min stall.
                if !saw_productive_turn && turn_tokens_out == 0 {
                    empty_start_turns += 1;
                    if empty_start_streak_feeds_breaker(empty_start_turns, saw_productive_turn) {
                        let diag = runtime_fs_diagnostics(project_path, worktree_path);
                        tracing::warn!(
                            task_id = %task_id,
                            session_id = %session_id,
                            empty_start_turns,
                            "ReplyLoop: consecutive zero-progress turns at session start — \
                             failing over to feed the model breaker early (no stall wait)"
                        );
                        return Err(empty_turn_terminal_error(
                            "zero-progress-start",
                            empty_start_turns,
                            model_id,
                            diag,
                        ));
                    }
                }
                if let Some(next_retry) =
                    should_retry_empty_assistant_turn(assistant_content.is_empty(), empty_turn_retries, is_codex)
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
                return Err(empty_turn_terminal_error(
                    "assistant",
                    empty_turn_retries,
                    model_id,
                    diag,
                ));
            }
            empty_turn_retries = 0;
            // The session has produced a real turn — disarm the session-start
            // zero-progress guard for the rest of the run so a later isolated
            // empty turn can't fail a working session over.
            saw_productive_turn = true;
            // AC2: Success-shaped assistant prose that actually reports rate
            // limit, provider error, out-of-credits, or quota exhaustion must
            // be reclassified as a typed provider failure and must NOT be
            // persisted as a successful assistant turn.
            if !turn_text.is_empty() && is_provider_failure_prose(&turn_text) {
                let snippet: String = turn_text.chars().take(120).collect();
                tracing::warn!(
                    task_id = %task_id,
                    session_id = %session_id,
                    turn = turns,
                    text_len = turn_text.len(),
                    snippet = %snippet,
                    "ReplyLoop: assistant text classified as provider failure prose — \
                     reclassifying as typed provider failure; not persisting as successful turn"
                );
                return Err(provider_failure_prose_error(&snippet));
            }
            let assistant_msg = Message {
                role: Role::Assistant,
                content: assistant_content,
                metadata: Some(MessageMeta {
                    input_tokens: Some(turn_tokens_in),
                    output_tokens: Some(turn_tokens_out),
                    timestamp: Some(
                        slot_ctx.clock.now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs() as i64)
                            .unwrap_or(0),
                    ),
                    provider_data: None,
                }),
            };
            assistant_message_count += 1;
            slot_ctx.event_bus.send(DjinnEventEnvelope::session_message(
                session_id,
                task_id,
                role_name,
                &serialize_message(&assistant_msg),
            ));
            persist_session_message(&msg_repo, session_id, task_id, &assistant_msg).await;
            conversation.push(assistant_msg);
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
            if needs_compaction(current_context_tokens, context_window) {
                tracing::info!(
                    task_id = %task_id,
                    current_context_tokens,
                    context_window,
                    usage_pct = current_context_tokens as f64 / context_window as f64,
                    "ReplyLoop: compaction threshold reached, compacting"
                );
                let compacted = compact_conversation_in_critical_section(
                    provider,
                    conversation,
                    session_id,
                    task_id,
                    role_name,
                    context_window,
                    compaction_cs,
                    slot_ctx,
                )
                .await;
                if compacted {
                    total_tokens_in = 0;
                    total_tokens_out = 0;
                    current_context_tokens = 0;
                    tool_dispatcher.clear_stash();
                    if should_retry_after_tool_call_compaction(compacted, !turn_tool_calls.is_empty()) {
                        compaction_attempts += 1;
                        conversation.push(Message::user("Continue with the task."));
                        continue;
                    }
                }
            }
            // Before accepting a worker's finalize payload, check whether the
            // current worktree diff fingerprint matches the latest rejected
            // fingerprint for this task. On the first identical no-progress
            // submit, return a corrective tool result and continue the session
            // so the worker can make substantive changes.
            let primary_finalize = finalize_tool_names.first().copied().unwrap_or("");
            if !turn_tool_calls.is_empty() {
                match check_worker_submit_no_progress_guard(
                    role_name,
                    primary_finalize,
                    task_id,
                    worktree_path,
                    slot_ctx,
                    no_progress_guard_triggered,
                )
                .await
                {
                    NoProgressGuardResult::FirstBounceCorrective(corrective_text)
                        if let Some(tool_use_id) =
                            find_tool_use_id(&turn_tool_calls, primary_finalize) =>
                    {
                        no_progress_guard_triggered = true;
                        let corrective_result = ContentBlock::ToolResult {
                            tool_use_id: tool_use_id.to_string(),
                            content: vec![ContentBlock::Text {
                                text: corrective_text,
                            }],
                            is_error: true,
                        };
                        let result_msg = Message {
                            role: Role::User,
                            content: vec![corrective_result],
                            metadata: None,
                        };
                        persist_session_message(&msg_repo, session_id, task_id, &result_msg)
                            .await;
                        conversation.push(result_msg);
                        // Skip finalize and tool dispatch for this turn; the
                        // loop will continue and the worker will see the
                        // corrective message.
                        continue;
                    }
                    NoProgressGuardResult::SecondStrikeSettle => {
                        // Second consecutive identical no-progress submit.
                        // Settle the session as a typed no_progress_submission
                        // — do NOT accept the finalize payload and do NOT
                        // inject a corrective. Log the activity, increment
                        // the no_progress_streak, and route into planner
                        // intervention via the normal settle path. Break out
                        // of the loop so lifecycle teardown skips re-settlement.
                        settle_no_progress_submission(task_id, slot_ctx).await;
                        output.no_progress_submission = true;
                        tracing::warn!(
                            task_id = %task_id,
                            "ReplyLoop: second-strike no_progress_submission settle — \
                             session ending without accepting finalize payload"
                        );
                        break;
                    }
                    _ => {
                        // Allow or FirstBounceCorrective without a matching
                        // tool_use_id — fall through to normal finalize
                        // detection below.
                    }
                }
            }
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
                tracing::info!(
                    task_id = %task_id,
                    agent_type = %role_name,
                    turns,
                    assistant_message_count,
                    "ReplyLoop: text-only turn (no tools) — session complete"
                );
                break;
            }
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
            // Touch activity after tool execution.
            {
                let now = slot_ctx.clock.now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                activity_ts.store(now, Ordering::Relaxed);
                last_rpc_touch.store(now, Ordering::Relaxed);
                if let Err(e) = slot_ctx.callbacks.touch_activity_rpc(task_id.to_string()).await {
                    tracing::warn!(
                        task_id = %task_id,
                        error = %e,
                        "reply_loop: post-tool touch_activity RPC failed; \
                         host stall poller may see stale idle for this turn"
                    );
                }
            }
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
        }
        if !saw_any_event {
            let diag = runtime_fs_diagnostics(project_path, worktree_path);
            return Err(anyhow::anyhow!(
                "provider session produced no events; {}",
                diag
            ));
        }
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
    if let Some(session) = otel_session {
        session.record_usage(total_tokens_in, total_tokens_out);
        session.record_cache_usage(total_cache_read, total_cache_write, total_reasoning_out);
        if !final_assistant_text.is_empty() {
            session.record_trace_output(&final_assistant_text);
        }
        match &run_result {
            Ok(()) => session.end_ok(),
            Err(e) => session.end_error(&e.to_string()),
        }
    }
    slot_ctx.deregister_activity(task_id);
    (
        run_result,
        output,
        total_tokens_in as i64,
        total_tokens_out as i64,
        total_cache_read as i64,
        total_cache_write as i64,
    )
}

/// Run `compact_conversation` inside the shared compaction critical section.
///
/// Enters the critical section (via the RAII [`CompactionGuard`]), records a
/// `Started` boundary, invokes the compaction, and — only when compaction is
/// accepted — completes the boundary. The guard releases the critical section
/// on every exit path (success, summarizer failure, early return, panic
/// unwinding); retry/backoff decisions stay with the caller so this helper does
/// not alter loop semantics.
#[allow(clippy::too_many_arguments)]
async fn compact_conversation_in_critical_section(
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    session_id: &str,
    task_id: &str,
    role_name: &str,
    context_window: i64,
    compaction_cs: &CompactionCriticalSection,
    slot_ctx: &SlotContext,
) -> bool {
    // Held for the whole record -> compact -> complete sequence; `Drop` releases
    // the section on any exit path below.
    let _guard = compaction_cs.guard();

    let boundary_repo = SessionCompactionBoundaryRepository::new(slot_ctx.db.clone());
    let boundary_id = record_compaction_started(&boundary_repo, session_id, conversation).await;

    let compacted = compact_conversation(
        provider,
        conversation,
        session_id,
        task_id,
        CompactionContext::MidSession(role_name.to_string()),
        context_window,
    )
    .await;

    if compacted {
        let summary = conversation
            .messages
            .iter()
            .find(|m| {
                m.role == Role::User && m.text_content().contains(COMPACTION_SUMMARY_END_MARKER)
            })
            .map(|m| strip_compaction_markers(&m.text_content()))
            .unwrap_or_default();
        complete_compaction_boundary(
            &boundary_repo,
            boundary_id.as_deref(),
            conversation,
            &summary,
        )
        .await;
    }

    compacted
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_provider::provider::ProviderError;
    #[test]
    fn empty_turn_terminal_error_is_breaker_classifiable() {
        for kind in ["no-event", "assistant"] {
            let err =
                empty_turn_terminal_error(kind, 2, "kimi-for-coding/k2p7", "diag".to_string());
            let typed = err
                .downcast_ref::<ProviderError>()
                .expect("terminal empty-turn error must carry a typed ProviderError source");
            assert_eq!(*typed, ProviderError::ProviderInternal { status: 500 });
            assert!(
                typed.retryable(),
                "{kind}: a server-internal empty-turn failure is transient/retryable"
            );
            let display = err.to_string();
            assert!(display.contains("empty"), "{kind}: {display}");
            assert!(display.contains("diag"), "{kind}: {display}");
        }
    }
    #[test]
    fn empty_turn_terminal_error_codex_family_is_throttle_not_failure() {
        for kind in ["no-event", "assistant"] {
            let err = empty_turn_terminal_error(kind, 2, "openai/gpt-5.4", "diag".to_string());
            let typed = err
                .downcast_ref::<ProviderError>()
                .expect("Codex empty-turn error must carry EmptyCompletion");
            assert_eq!(*typed, ProviderError::EmptyCompletion);
        }
    }

    // ---- compaction critical-section guard (nlus) -------------------------
    //
    // These tests exercise the real `compact_conversation_in_critical_section`
    // helper (not a hand-rolled mimic) and assert the shared critical section is
    // released after every outcome of the underlying compaction: a successful
    // compaction, a benign no-op that returns false, a summarizer error, and an
    // abnormal early exit (a summarizer panic that unwinds through the helper).

    use crate::test_helpers::{
        FailingProvider, FakeProvider, agent_context_from_db, create_test_db, create_test_epic,
        create_test_project, create_test_task,
    };
    use djinn_provider::provider::StreamEvent;

    fn guard_test_slot_ctx() -> SlotContext {
        agent_context_from_db(create_test_db(), tokio_util::sync::CancellationToken::new())
    }

    /// A conversation whose old tool-result turns can be micro-compacted, so
    /// `compact_conversation` reports a real (`true`) compaction without ever
    /// invoking the summarizer provider.
    fn micro_compactable_conversation(num_turns: usize) -> Conversation {
        let mut conversation = Conversation::new();
        conversation.push(Message::system("You are a coding agent."));
        conversation.push(Message::user("Do the task."));
        for i in 0..num_turns {
            let call_id = format!("call_{i}");
            conversation.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: call_id.clone(),
                    name: "bash".into(),
                    input: serde_json::json!({ "command": format!("echo turn {i}") }),
                }],
                metadata: None,
            });
            conversation.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: call_id,
                    content: vec![ContentBlock::text(format!(
                        "verbose tool result from turn {i}: {}",
                        "x".repeat(400)
                    ))],
                    is_error: false,
                }],
                metadata: None,
            });
            conversation.push(Message::assistant(format!("Processed turn {i}.")));
        }
        conversation
    }

    fn tiny_conversation() -> Conversation {
        let mut conversation = Conversation::new();
        conversation.push(Message::system("You are a worker."));
        conversation.push(Message::user("Do the task."));
        conversation
    }

    /// Successful compaction: the guard releases and the helper returns `true`.
    #[tokio::test]
    async fn helper_releases_guard_on_successful_compaction() {
        let section = CompactionCriticalSection::new();
        // A provider that would panic if the summarizer were ever called — this
        // path succeeds via micro-compaction, before any LLM round-trip.
        let provider = FailingProvider::new("summarizer must not be called on the micro path");
        let mut conversation = micro_compactable_conversation(12);
        let slot_ctx = guard_test_slot_ctx();

        let compacted = compact_conversation_in_critical_section(
            &provider,
            &mut conversation,
            "s-success",
            "t-success",
            "worker",
            1_000_000,
            &section,
            &slot_ctx,
        )
        .await;

        assert!(
            compacted,
            "micro-compaction should report a successful compaction"
        );
        assert!(
            !section.is_compacting(),
            "guard must release after a successful compaction"
        );
    }

    /// False / no-compaction: the summarizer yields nothing usable and the
    /// conversation is too small to truncate, so the helper returns `false`.
    /// Distinct from the summarizer-error case: here the provider does not error,
    /// it simply produces empty summaries.
    #[tokio::test]
    async fn helper_releases_guard_when_compaction_is_a_noop() {
        let section = CompactionCriticalSection::new();
        // Empty summaries across every removal-percentage attempt: a benign
        // no-op, not an error. Extra scripted turns are harmless if unused.
        let provider = FakeProvider::script(vec![
            vec![StreamEvent::Done],
            vec![StreamEvent::Done],
            vec![StreamEvent::Done],
            vec![StreamEvent::Done],
            vec![StreamEvent::Done],
            vec![StreamEvent::Done],
        ]);
        let mut conversation = tiny_conversation();
        let slot_ctx = guard_test_slot_ctx();

        let compacted = compact_conversation_in_critical_section(
            &provider,
            &mut conversation,
            "s-noop",
            "t-noop",
            "worker",
            10_000,
            &section,
            &slot_ctx,
        )
        .await;

        assert!(!compacted, "an empty-summary no-op yields no compaction");
        assert!(
            !section.is_compacting(),
            "guard must release after a no-op compaction"
        );
    }

    /// Summarizer error / `compact_conversation` error path: the provider stream
    /// errors, so no compaction happens; the guard must still release. This is a
    /// distinct test (and a distinct provider/failure mode) from the no-op case.
    #[tokio::test]
    async fn helper_releases_guard_on_summarizer_error() {
        let section = CompactionCriticalSection::new();
        let provider = FailingProvider::new("summarizer boom");
        let mut conversation = tiny_conversation();
        let slot_ctx = guard_test_slot_ctx();

        let compacted = compact_conversation_in_critical_section(
            &provider,
            &mut conversation,
            "s-error",
            "t-error",
            "worker",
            10_000,
            &section,
            &slot_ctx,
        )
        .await;

        assert!(!compacted, "a summarizer error yields no compaction");
        assert!(
            !section.is_compacting(),
            "guard must release after a summarizer error"
        );
    }

    /// Early / abnormal exit: the summarizer panics mid-compaction and the panic
    /// unwinds *through* the real helper. The RAII guard must still release the
    /// critical section during unwinding.
    #[tokio::test]
    async fn helper_releases_guard_on_panic_early_exit() {
        use futures::FutureExt;

        let section = CompactionCriticalSection::new();
        // An exhausted script: the first `stream()` call panics, standing in for
        // a summarizer that blows up mid-compaction.
        let provider = FakeProvider::script(vec![]);
        let mut conversation = tiny_conversation();
        // Enough turns that micro-compaction is skipped and the provider is
        // actually invoked (and thus panics).
        conversation.push(Message::assistant("first"));
        conversation.push(Message::user("second"));
        conversation.push(Message::assistant("third"));
        let slot_ctx = guard_test_slot_ctx();

        let outcome = std::panic::AssertUnwindSafe(compact_conversation_in_critical_section(
            &provider,
            &mut conversation,
            "s-panic",
            "t-panic",
            "worker",
            10_000,
            &section,
            &slot_ctx,
        ))
        .catch_unwind()
        .await;

        assert!(
            outcome.is_err(),
            "the summarizer panic must unwind out of the helper"
        );
        assert!(
            !section.is_compacting(),
            "guard must release on a panic / early-exit unwinding through the helper"
        );
    }

    // ---- idempotent in-flight turn flush (djxg) --------------------------

    use super::super::persistence::flush_in_flight_turn;
    use super::super::streaming::StreamTurnState;
    use djinn_db::SessionMessageRepository;
    use djinn_db::repositories::session::{CreateSessionParams, SessionRepository};

    /// Create a project + epic + task + session in the test DB and return
    /// `(session_id, task_id)` for flush tests that need a valid FK target.
    async fn create_flush_test_session(db: &djinn_db::Database) -> (String, String) {
        let project = create_test_project(db).await;
        let epic = create_test_epic(db, &project.id).await;
        let task = create_test_task(db, &project.id, &epic.id).await;
        let session_repo = SessionRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let session = session_repo
            .create(CreateSessionParams {
                project_id: &project.id,
                task_id: Some(&task.id),
                model: "test-model",
                agent_type: "worker",
                metadata_json: None,
                task_run_id: None,
                pricing: None,
                cost_basis: None,
            })
            .await
            .expect("create session");
        (session.id, task.id)
    }

    /// Build a `StreamTurnState` with assistant text and a completed streaming
    /// tool result — the typical in-flight state at interrupt time.
    fn inflight_turn_state() -> StreamTurnState {
        let mut state = StreamTurnState::new();
        state.turn_text = "Let me check the file.".to_string();
        state.turn_thinking = "Thinking about the task.".to_string();
        state.turn_tool_calls = vec![ContentBlock::ToolUse {
            id: "call_1".to_string(),
            name: "read".to_string(),
            input: serde_json::json!({ "file_path": "/tmp/test.rs" }),
        }];
        state.streaming_results = vec![(
            0,
            ContentBlock::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: vec![ContentBlock::text("file contents here")],
                is_error: false,
            },
        )];
        state
    }

    /// Repeated `flush_in_flight_turn` calls must not duplicate persisted
    /// session messages: the `turn_flushed` guard is set after the first call.
    #[tokio::test]
    async fn flush_in_flight_turn_is_idempotent() {
        let db = create_test_db();
        db.ensure_initialized().await.expect("db init");
        let (session_id, task_id) = create_flush_test_session(&db).await;
        let msg_repo =
            SessionMessageRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let mut state = inflight_turn_state();

        // First flush: should persist assistant + tool-result messages.
        flush_in_flight_turn(&msg_repo, &session_id, &task_id, 0, &mut state).await;
        assert!(
            state.turn_flushed,
            "turn_flushed must be set after first flush"
        );

        // Second flush: no-op due to idempotency guard.
        flush_in_flight_turn(&msg_repo, &session_id, &task_id, 0, &mut state).await;

        // Verify exactly 2 messages (assistant + tool result) were persisted,
        // not 4 (which would happen without idempotency).
        let conversation = msg_repo
            .load_raw_conversation(&session_id)
            .await
            .expect("load_raw_conversation should succeed");
        assert_eq!(
            conversation.messages.len(),
            2,
            "expected exactly 2 messages (assistant + tool result), found {}",
            conversation.messages.len()
        );
    }

    /// An empty turn (no text, no thinking, no tool calls) should not persist
    /// any messages and should still mark the turn as flushed.
    #[tokio::test]
    async fn flush_in_flight_turn_empty_noop() {
        let db = create_test_db();
        db.ensure_initialized().await.expect("db init");
        let (session_id, task_id) = create_flush_test_session(&db).await;
        let msg_repo =
            SessionMessageRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let mut state = StreamTurnState::new();

        flush_in_flight_turn(&msg_repo, &session_id, &task_id, 0, &mut state).await;
        assert!(
            state.turn_flushed,
            "turn_flushed must be set even for empty turn"
        );

        let conversation = msg_repo
            .load_raw_conversation(&session_id)
            .await
            .expect("load_raw_conversation should succeed");
        assert!(
            conversation.messages.is_empty(),
            "empty turn should not persist any messages"
        );
    }

    /// Flushed assistant/tool messages are visible in the raw conversation
    /// (load_raw_conversation) — the timeline/resume path.
    #[tokio::test]
    async fn flush_in_flight_turn_visible_on_resume_timeline() {
        let db = create_test_db();
        db.ensure_initialized().await.expect("db init");
        let (session_id, task_id) = create_flush_test_session(&db).await;
        let msg_repo =
            SessionMessageRepository::new(db.clone(), djinn_core::events::EventBus::noop());
        let mut state = inflight_turn_state();

        flush_in_flight_turn(&msg_repo, &session_id, &task_id, 0, &mut state).await;

        let conversation = msg_repo
            .load_raw_conversation(&session_id)
            .await
            .expect("load_raw_conversation should succeed");

        // Should have an assistant message and a tool result message.
        assert_eq!(conversation.messages.len(), 2);

        let assistant = &conversation.messages[0];
        assert_eq!(assistant.role, Role::Assistant);
        let text = assistant.text_content();
        assert!(
            text.contains("Let me check the file."),
            "assistant text should be present on resume: {text}"
        );
        // Thinking should be preserved.
        assert!(
            assistant
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Thinking { thinking, .. } if thinking.contains("Thinking about"))),
            "thinking content should be preserved in the assistant message"
        );
        // Tool use block should be present.
        assert!(
            assistant.content.iter().any(|b| matches!(
                b,
                ContentBlock::ToolUse { id, name, .. } if id == "call_1" && name == "read"
            )),
            "tool_use block should be present in the assistant message"
        );

        let tool_result = &conversation.messages[1];
        assert_eq!(tool_result.role, Role::User);
        assert!(
            tool_result.content.iter().any(|b| matches!(
                b,
                ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "call_1"
            )),
            "tool result should be present with correct tool_use_id"
        );
    }

    /// `consume_provider_stream` records `early_stream_end` when the provider
    /// stream ends without `StreamEvent::Done`, preserving observed text so the
    /// reply loop can flush it before session release.
    #[tokio::test]
    async fn consume_provider_stream_early_end_records_flush_signal() {
        use super::super::streaming::StreamLoopContext;
        use std::sync::Arc;
        use std::sync::atomic::AtomicU64;
        use tokio_util::sync::CancellationToken;

        let db = create_test_db();
        db.ensure_initialized().await.expect("db init");
        let slot_ctx = agent_context_from_db(db, CancellationToken::new());
        let provider = FakeProvider::script(vec![vec![
            StreamEvent::Delta(ContentBlock::Text {
                text: "partial output".to_string(),
            }),
            // No StreamEvent::Done — simulate early stream end.
        ]]);
        let tool_metadata = super::super::tool_dispatch::tool_runtime_metadata(&[]);
        let conversation = Conversation::new();
        let stream = provider
            .stream(&conversation, &[], None)
            .await
            .expect("stream");
        let cancel = CancellationToken::new();
        let global_cancel = CancellationToken::new();
        let activity_ts = Arc::new(AtomicU64::new(0));
        let last_rpc_touch = Arc::new(AtomicU64::new(0));
        let last_token_flush = Arc::new(AtomicU64::new(0));
        let mut current_context_tokens = 0u32;
        let mut total_tokens_in = 0u32;
        let mut total_tokens_out = 0u32;
        let mut total_cache_read = 0u32;
        let mut total_cache_write = 0u32;
        let mut total_reasoning_out = 0u32;
        let dispatcher = slot_ctx
            .tool_dispatcher
            .as_deref()
            .expect("test SlotContext has a tool dispatcher");
        let dispatch_ctx = super::super::tool_dispatch::ToolDispatchContext {
            ctx: &slot_ctx,
            task_id: "task",
            worktree_path: std::path::Path::new("/tmp"),
            role_name: "worker",
            tool_metadata: &tool_metadata,
            tool_dispatcher: dispatcher,
            otel_session: None,
        };

        let state = consume_provider_stream(StreamLoopContext {
            provider: &provider,
            stream,
            tool_metadata: &tool_metadata,
            dispatch: &dispatch_ctx,
            task_id: "task",
            session_id: "session",
            role_name: "worker",
            project_path: "/tmp",
            worktree_path: std::path::Path::new("/tmp"),
            context_window: 100_000,
            ctx: &slot_ctx,
            cancel: &cancel,
            global_cancel: &global_cancel,
            activity_ts: &activity_ts,
            last_rpc_touch: &last_rpc_touch,
            last_token_flush: &last_token_flush,
            compaction_attempts: 0,
            current_context_tokens: &mut current_context_tokens,
            total_tokens_in: &mut total_tokens_in,
            total_tokens_out: &mut total_tokens_out,
            total_cache_read: &mut total_cache_read,
            total_cache_write: &mut total_cache_write,
            total_reasoning_out: &mut total_reasoning_out,
        })
        .await
        .expect("consume_provider_stream");

        assert!(
            state.early_stream_end,
            "early stream end must be recorded when stream ends without Done"
        );
        assert_eq!(state.turn_text, "partial output");
        assert!(
            state.interrupted.is_none(),
            "early stream end is not an interruption"
        );
    }

    // ---- thinking persistence regressions (nbky) --------------------------
    //
    // These tests validate that the new ContentBlock variants introduced by
    // task `a7wa` (signed Thinking, RedactedThinking, Unknown/passthrough)
    // survive the session-message persistence boundary — Message → JSON → DB →
    // JSON → Message — without data loss or semantic drift.
    //
    // Old stored Thinking blocks that lack `signature` must continue to
    // deserialize as `signature: None` (the serde default). Signed thinking,
    // redacted thinking, and opaque passthrough blocks must preserve their
    // fields exactly through the round-trip.
    //
    // The `flush_in_flight_turn` path (which constructs a Thinking block from
    // the plain `turn_thinking` string with `signature: None`) is also
    // exercised to confirm it does not accidentally attach or drop signatures.

    /// Old stored Thinking blocks that only contain `thinking` (no `signature`
    /// key in JSON) must deserialize as `signature: None` after a persistence
    /// round-trip through the session-message DB. This is the backward-
    /// compatibility guarantee for pre-`a7wa` data.
    #[tokio::test]
    async fn persisted_old_style_thinking_loads_with_none_signature() {
        let db = create_test_db();
        db.ensure_initialized().await.expect("db init");
        let (session_id, task_id) = create_flush_test_session(&db).await;
        let msg_repo =
            SessionMessageRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        // Simulate an old assistant message whose Thinking block has no
        // signature field — the shape that existed before `a7wa`.
        let msg = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "legacy reasoning without signature".into(),
                signature: None,
            }],
            metadata: None,
        };
        persist_session_message(&msg_repo, &session_id, &task_id, &msg).await;

        let loaded = msg_repo
            .load_raw_conversation(&session_id)
            .await
            .expect("load conversation");
        assert_eq!(loaded.messages.len(), 1);
        match &loaded.messages[0].content[0] {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "legacy reasoning without signature");
                assert_eq!(
                    signature, &None,
                    "old stored Thinking must have signature == None"
                );
            }
            other => panic!("expected Thinking block, got: {other:?}"),
        }
    }

    /// Signed Thinking blocks (Anthropic extended-thinking with a cryptographic
    /// signature) must survive the persistence boundary with the signature
    /// intact. This is the critical fidelity path for provider replay.
    #[tokio::test]
    async fn persisted_signed_thinking_preserves_signature() {
        let db = create_test_db();
        db.ensure_initialized().await.expect("db init");
        let (session_id, task_id) = create_flush_test_session(&db).await;
        let msg_repo =
            SessionMessageRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        let original_signature = "eyJhbGciOiJFUzI1NiJ9.test_sig_payload";
        let msg = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Thinking {
                thinking: "deep model reasoning about the problem".into(),
                signature: Some(original_signature.into()),
            }],
            metadata: None,
        };
        persist_session_message(&msg_repo, &session_id, &task_id, &msg).await;

        let loaded = msg_repo
            .load_raw_conversation(&session_id)
            .await
            .expect("load conversation");
        assert_eq!(loaded.messages.len(), 1);
        match &loaded.messages[0].content[0] {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "deep model reasoning about the problem");
                assert_eq!(
                    signature.as_deref(),
                    Some(original_signature),
                    "signed Thinking signature must survive the DB round-trip"
                );
            }
            other => panic!("expected Thinking block, got: {other:?}"),
        }
    }

    /// RedactedThinking blocks (opaque base64 `data` from Anthropic's safety
    /// filter) must round-trip through persistence with the data blob intact.
    #[tokio::test]
    async fn persisted_redacted_thinking_preserves_data() {
        let db = create_test_db();
        db.ensure_initialized().await.expect("db init");
        let (session_id, task_id) = create_flush_test_session(&db).await;
        let msg_repo =
            SessionMessageRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        let redacted_data = "dGhpcyBpcyBhIHJlZGFjdGVkIHRoaW5raW5nIGJsb2I=";
        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Text {
                    text: "Here is my answer.".into(),
                },
                ContentBlock::RedactedThinking {
                    data: redacted_data.into(),
                },
            ],
            metadata: None,
        };
        persist_session_message(&msg_repo, &session_id, &task_id, &msg).await;

        let loaded = msg_repo
            .load_raw_conversation(&session_id)
            .await
            .expect("load conversation");
        assert_eq!(loaded.messages.len(), 1);
        assert_eq!(
            loaded.messages[0].content.len(),
            2,
            "both content blocks must be present"
        );
        // Text block is first.
        assert!(
            matches!(&loaded.messages[0].content[0], ContentBlock::Text { text } if text == "Here is my answer."),
            "text block must survive"
        );
        // RedactedThinking is second.
        match &loaded.messages[0].content[1] {
            ContentBlock::RedactedThinking { data } => {
                assert_eq!(
                    data, redacted_data,
                    "RedactedThinking data blob must survive the DB round-trip exactly"
                );
            }
            other => panic!("expected RedactedThinking block, got: {other:?}"),
        }
    }

    /// Unknown/passthrough blocks (provider-owned content not modeled by the
    /// shared schema) must survive persistence with `content_type` and all
    /// `extra` fields intact, including nested JSON.
    #[tokio::test]
    async fn persisted_unknown_passthrough_preserves_extra_fields() {
        let db = create_test_db();
        db.ensure_initialized().await.expect("db init");
        let (session_id, task_id) = create_flush_test_session(&db).await;
        let msg_repo =
            SessionMessageRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        let mut extra = serde_json::Map::new();
        extra.insert("provider_specific_flag".into(), serde_json::json!(true));
        extra.insert(
            "nested".into(),
            serde_json::json!({"a": [1, 2, {"b": true}]}),
        );
        let msg = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Unknown {
                content_type: "custom_thinking_block".into(),
                extra,
            }],
            metadata: None,
        };
        persist_session_message(&msg_repo, &session_id, &task_id, &msg).await;

        let loaded = msg_repo
            .load_raw_conversation(&session_id)
            .await
            .expect("load conversation");
        assert_eq!(loaded.messages.len(), 1);
        match &loaded.messages[0].content[0] {
            ContentBlock::Unknown {
                content_type,
                extra,
            } => {
                assert_eq!(
                    content_type, "custom_thinking_block",
                    "content_type must survive the DB round-trip"
                );
                assert_eq!(
                    extra.get("provider_specific_flag"),
                    Some(&serde_json::json!(true)),
                    "boolean extra field must survive"
                );
                assert_eq!(
                    extra.get("nested"),
                    Some(&serde_json::json!({"a": [1, 2, {"b": true}]})),
                    "nested JSON in extra must survive the DB round-trip"
                );
            }
            other => panic!("expected Unknown block, got: {other:?}"),
        }
    }

    /// `flush_in_flight_turn` constructs a Thinking block from the plain
    /// `turn_thinking` string with `signature: None`. After persistence and
    /// reload, the block must still carry `signature: None` — this confirms
    /// the flush path does not accidentally attach or drop signatures.
    #[tokio::test]
    async fn flush_unsigned_thinking_survives_persistence_boundary() {
        let db = create_test_db();
        db.ensure_initialized().await.expect("db init");
        let (session_id, task_id) = create_flush_test_session(&db).await;
        let msg_repo =
            SessionMessageRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        let mut state = StreamTurnState::new();
        state.turn_thinking = "internal reasoning about the task".into();
        state.turn_text = "Here is the answer.".into();

        flush_in_flight_turn(&msg_repo, &session_id, &task_id, 0, &mut state).await;
        assert!(state.turn_flushed);

        let loaded = msg_repo
            .load_raw_conversation(&session_id)
            .await
            .expect("load conversation");
        assert_eq!(loaded.messages.len(), 1, "one assistant message");
        let assistant = &loaded.messages[0];
        assert_eq!(assistant.role, Role::Assistant);

        // Thinking block from flush must have signature: None.
        let thinking_block = assistant
            .content
            .iter()
            .find(|b| matches!(b, ContentBlock::Thinking { .. }));
        match thinking_block {
            Some(ContentBlock::Thinking {
                thinking,
                signature,
            }) => {
                assert_eq!(thinking, "internal reasoning about the task");
                assert_eq!(
                    signature, &None,
                    "flush-created Thinking must carry signature: None"
                );
            }
            None => panic!("Thinking block must be present after flush"),
            _ => unreachable!(),
        }
        // Text block must also be present.
        assert!(
            assistant
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Text { text } if text == "Here is the answer.")),
            "text block must survive flush"
        );
    }

    /// A mixed assistant message containing signed Thinking, RedactedThinking,
    /// regular Text, and an Unknown passthrough block must survive persistence
    /// with every block preserved in order and with all fields intact.
    #[tokio::test]
    async fn mixed_thinking_content_survives_persistence_boundary() {
        let db = create_test_db();
        db.ensure_initialized().await.expect("db init");
        let (session_id, task_id) = create_flush_test_session(&db).await;
        let msg_repo =
            SessionMessageRepository::new(db.clone(), djinn_core::events::EventBus::noop());

        let mut extra = serde_json::Map::new();
        extra.insert("lang".into(), serde_json::json!("en"));

        let msg = Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "signed reasoning".into(),
                    signature: Some("sig_v2_abc".into()),
                },
                ContentBlock::RedactedThinking {
                    data: "cmVkYWN0ZWQ=".into(),
                },
                ContentBlock::Text {
                    text: "The answer is 42.".into(),
                },
                ContentBlock::Unknown {
                    content_type: "provider_meta".into(),
                    extra,
                },
            ],
            metadata: None,
        };
        persist_session_message(&msg_repo, &session_id, &task_id, &msg).await;

        let loaded = msg_repo
            .load_raw_conversation(&session_id)
            .await
            .expect("load conversation");
        assert_eq!(loaded.messages.len(), 1);
        let blocks = &loaded.messages[0].content;
        assert_eq!(blocks.len(), 4, "all four content blocks must survive");

        // 1. Signed Thinking.
        match &blocks[0] {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "signed reasoning");
                assert_eq!(signature.as_deref(), Some("sig_v2_abc"));
            }
            other => panic!("expected signed Thinking, got: {other:?}"),
        }
        // 2. RedactedThinking.
        match &blocks[1] {
            ContentBlock::RedactedThinking { data } => {
                assert_eq!(data, "cmVkYWN0ZWQ=");
            }
            other => panic!("expected RedactedThinking, got: {other:?}"),
        }
        // 3. Text.
        assert!(
            matches!(&blocks[2], ContentBlock::Text { text } if text == "The answer is 42."),
            "text block must survive in order"
        );
        // 4. Unknown passthrough.
        match &blocks[3] {
            ContentBlock::Unknown {
                content_type,
                extra,
            } => {
                assert_eq!(content_type, "provider_meta");
                assert_eq!(extra.get("lang"), Some(&serde_json::json!("en")));
            }
            other => panic!("expected Unknown block, got: {other:?}"),
        }
    }
}
