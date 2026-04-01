//! Djinn-native conversation compaction.
//!
//! When the accumulated input token count reaches 80% of the model's context
//! window, `compact_conversation` summarises the conversation via the LLM and
//! replaces the in-memory `Conversation` with a compact representation. The
//! original messages are persisted to the `session_messages` table before the
//! replacement so nothing is lost.

use futures::StreamExt;

use crate::context::AgentContext;
use crate::message::{Conversation, Message, Role};
use crate::provider::{LlmProvider, StreamEvent};
use djinn_db::SessionMessageRepository;

// ─── Threshold ────────────────────────────────────────────────────────────────

/// Fraction of the context window at which compaction is triggered.
pub(crate) const COMPACTION_THRESHOLD: f64 = 0.8;

/// Microcompaction: tool results older than this many turns are cleared.
/// A "turn" is counted per assistant message, walking from the end.
const MICROCOMPACT_AGE_THRESHOLD: usize = 6;

/// Microcompaction: the most recent N turns are never cleared, regardless of age.
const MICROCOMPACT_EXEMPT_RECENT: usize = 3;

/// Partial compaction: the pivot point expressed as a fraction of total estimated
/// tokens. Messages up to this point form the stable prefix that is preserved
/// verbatim (for prompt-cache hits); only the tail is summarised.
const PARTIAL_COMPACTION_PIVOT: f64 = 0.6;

/// Partial compaction is skipped (falling through to full compaction) when the
/// tail after the pivot is estimated to reclaim less than this fraction of the
/// context window.
const PARTIAL_COMPACTION_MIN_RECLAIM: f64 = 0.2;

/// Maximum retries when the compaction request itself overflows the context.
/// On each retry the oldest 20% of message groups are dropped from the input.
const COMPACTION_OVERFLOW_MAX_RETRIES: usize = 3;

/// Fraction of message groups dropped from the oldest end on each overflow retry.
const COMPACTION_OVERFLOW_DROP_FRACTION: f64 = 0.2;

/// Aggressive microcompaction: tool results older than this many turns are
/// cleared when all overflow retries have been exhausted.
const AGGRESSIVE_MICROCOMPACT_AGE: usize = 2;

// ─── Compaction context ──────────────────────────────────────────────────────

/// Describes *why* compaction is happening, so the prompt can be tailored.
#[derive(Debug, Clone)]
pub(crate) enum CompactionContext {
    /// Mid-session compaction: context window threshold reached while working.
    MidSession(String),
    /// Pre-resume compaction: compacting before re-prompting with reviewer feedback.
    PreResume(String),
}

// ─── Prompts ─────────────────────────────────────────────────────────────────

/// Build the compaction prompt based on context.
fn compaction_prompt(ctx: &CompactionContext) -> &'static str {
    match ctx {
        CompactionContext::PreResume(role) if role == "worker" => PRE_RESUME_WORKER_PROMPT,
        CompactionContext::MidSession(role) if role == "worker" => MID_SESSION_WORKER_PROMPT,
        CompactionContext::MidSession(role) | CompactionContext::PreResume(role)
            if role == "reviewer" || role == "task_reviewer" =>
        {
            REVIEWER_PROMPT
        }
        _ => GENERIC_PROMPT,
    }
}

/// Build the system instruction for the summariser based on context.
fn summariser_system(ctx: &CompactionContext) -> &'static str {
    match ctx {
        CompactionContext::PreResume(role) if role == "worker" => {
            SUMMARISER_SYSTEM_WORKER_PRE_RESUME
        }
        CompactionContext::MidSession(role) if role == "worker" => {
            SUMMARISER_SYSTEM_WORKER_MID_SESSION
        }
        CompactionContext::MidSession(role) | CompactionContext::PreResume(role)
            if role == "reviewer" || role == "task_reviewer" =>
        {
            SUMMARISER_SYSTEM_TASK_REVIEWER
        }
        _ => SUMMARISER_SYSTEM_GENERIC,
    }
}

pub(crate) const PRE_RESUME_WORKER_PROMPT: &str = r#"## Compaction Context
A coding agent's session is being compacted before re-prompting with reviewer feedback.
The agent's previous work was rejected or needs fixes. Summarise what happened so the
agent can efficiently address the feedback without re-doing research.

**Conversation History:**
{messages}

Wrap reasoning in `<analysis>` tags.

### Include These Sections (in order of importance):
1. **Files Changed** – Every file path that was read, created, or edited, with a brief description of changes made
2. **Implementation State** – What was actually implemented vs. what was planned but not done
3. **Code Decisions** – Key architectural or design decisions made and why
4. **Errors Encountered** – Compile errors, test failures, and how they were (or weren't) resolved
5. **Codebase Context** – Important patterns, types, or structures discovered during research that are needed for implementation
6. **Outstanding Issues** – Known problems, incomplete work, or things the agent said it would do but didn't

### IMPORTANT:
- Do NOT include any claims that the work is "done", "complete", or "implemented successfully"
- Do NOT include the agent's final sign-off or completion messages
- Focus on FACTS: what files exist, what code was written, what errors remain
- Preserve exact file paths, function names, and type names"#;

pub(crate) const MID_SESSION_WORKER_PROMPT: &str = r#"## Compaction Context
A coding agent's context window is full and needs compaction to continue working.

**Conversation History:**
{messages}

Wrap reasoning in `<analysis>` tags.

### Include These Sections:
1. **Task Goal** – What the agent is trying to accomplish
2. **Files Changed** – Every file path read, created, or edited, with description of changes
3. **Implementation Progress** – What's done vs. what remains
4. **Code Decisions** – Key architectural decisions and reasoning
5. **Errors + Fixes** – Bugs encountered and resolutions (or outstanding)
6. **Codebase Context** – Important types, patterns, file locations discovered
7. **Current Work** – What the agent was actively working on when compaction triggered
8. **Next Steps** – Concrete remaining work items

> Preserve exact file paths, function names, type names, and error messages"#;

pub(crate) const REVIEWER_PROMPT: &str = r#"## Compaction Context
A code review agent's session needs compaction.

**Conversation History:**
{messages}

Wrap reasoning in `<analysis>` tags.

### Include These Sections:
1. **Review Scope** – What task/PR is being reviewed and what the acceptance criteria are
2. **Files Reviewed** – Every file examined with key observations
3. **Issues Found** – All problems identified (compile errors, logic bugs, missing functionality, style)
4. **Positive Findings** – What was implemented correctly
5. **Assessment Progress** – Which acceptance criteria have been checked and their pass/fail status
6. **Remaining Checks** – What still needs to be reviewed

> Preserve exact file paths, line numbers, error messages, and acceptance criteria text"#;

pub(crate) const GENERIC_PROMPT: &str = r#"## Task Context
- An llm context limit was reached when a user was in a working session with an agent (you)
- Generate a version of the below messages with only the most verbose parts removed
- Include user requests, your responses, all technical content, and as much of the original context as possible
- This will be used to let the user continue the working session
- Use framing and tone knowing the content will be read an agent (you) on a next exchange to allow for continuation of the session

**Conversation History:**
{messages}

Wrap reasoning in `<analysis>` tags:
- Review conversation chronologically...

### Include the Following Sections:
1. **User Intent** – All goals and requests
2. **Technical Concepts** – All discussed tools, methods
3. **Files + Code** – Viewed/edited files, full code, change justifications
4. **Errors + Fixes** – Bugs, resolutions, user-driven changes
5. **Problem Solving** – Issues solved or in progress
6. **User Messages** – All user messages including tool calls, but truncate long tool call arguments or results
7. **Pending Tasks** – All unresolved user requests
8. **Current Work** – Active work at summary request time: filenames, code, alignment to latest instruction
9. **Next Step** – *Include only if* directly continues user instruction

> No new ideas unless user confirmed"#;

const PARTIAL_COMPACTION_PROMPT: &str = r#"## Partial Compaction Context
The latter portion of a conversation is being summarised while the beginning is kept intact.
The earlier messages (system prompt and initial context) are preserved verbatim and do NOT
appear below — only the tail of the conversation that needs compacting is shown.

Your summary will be inserted immediately after the preserved prefix, so it must connect
naturally to the earlier context that the reader already has.

**Tail Messages to Summarise:**
{messages}

Wrap reasoning in `<analysis>` tags.

### Include These Sections:
1. **Progress Since Start** – What was accomplished in this portion of the conversation
2. **Files Changed** – Every file path read, created, or edited, with description of changes
3. **Code Decisions** – Key architectural decisions and reasoning
4. **Errors + Fixes** – Bugs encountered and resolutions (or outstanding)
5. **Current Work** – What was actively being worked on at the end of this segment
6. **Next Steps** – Concrete remaining work items

> Preserve exact file paths, function names, type names, and error messages
> Do NOT repeat information that would already be in the preserved earlier context"#;

const PARTIAL_COMPACTION_SUMMARISER_SYSTEM: &str = "You are summarising the tail portion of a conversation. The beginning of the conversation is preserved separately and the reader will have it. Produce a dense, faithful summary of only the provided messages, connecting naturally to the earlier context the reader already has. Do not repeat early context.";

pub(crate) const SUMMARISER_SYSTEM_WORKER_PRE_RESUME: &str = "You are summarising a coding agent's work session that is about to receive reviewer feedback. Produce a dense, faithful summary focused on what was implemented, what files were changed, and what the current state of the code is. Do NOT include any statements about work being complete or done — the reviewer has determined it is not.";
pub(crate) const SUMMARISER_SYSTEM_WORKER_MID_SESSION: &str = "You are summarising a coding agent's in-progress work session. Produce a dense, faithful summary that preserves all implementation context so the agent can continue working without re-reading files.";
pub(crate) const SUMMARISER_SYSTEM_TASK_REVIEWER: &str = "You are summarising a code review session. Produce a dense, faithful summary that preserves the review findings, issues identified, and assessment progress.";
pub(crate) const SUMMARISER_SYSTEM_GENERIC: &str =
    "You are a conversation summariser. Produce a dense, faithful summary.";

// ─── Public helpers ───────────────────────────────────────────────────────────

/// Return `true` if the accumulated input tokens have reached the compaction
/// threshold relative to the model's context window.
pub(crate) fn needs_compaction(total_tokens_in: u32, context_window: i64) -> bool {
    if context_window <= 0 {
        return false;
    }
    total_tokens_in as f64 / context_window as f64 >= COMPACTION_THRESHOLD
}

// ─── Microcompaction ─────────────────────────────────────────────────────────

/// Zero-cost microcompaction pass: replace tool-result content in old turns with
/// a short placeholder. This runs before LLM-based compaction and may reclaim
/// enough tokens to skip the expensive summarisation entirely.
///
/// **Turn counting**: each `Assistant` message increments the turn counter,
/// counted from the end of the conversation (most recent = turn 0).
///
/// Returns the estimated number of tokens reclaimed (chars / 4 heuristic).
pub(crate) fn microcompact(conversation: &mut Conversation, current_turn: usize) -> usize {
    microcompact_with_thresholds(
        conversation,
        current_turn,
        MICROCOMPACT_AGE_THRESHOLD,
        MICROCOMPACT_EXEMPT_RECENT,
    )
}

/// Microcompaction with caller-supplied thresholds. Used by the normal
/// pre-pass (with module-level defaults) and the aggressive fallback (with
/// tighter settings).
fn microcompact_with_thresholds(
    conversation: &mut Conversation,
    current_turn: usize,
    age_threshold: usize,
    exempt_recent: usize,
) -> usize {
    use crate::message::ContentBlock;

    let messages = &mut conversation.messages;

    // 1. Assign a turn number to each message by walking backwards and
    //    incrementing on each Assistant message.
    let mut turn_map: Vec<usize> = vec![0; messages.len()];
    let mut turn_counter: usize = 0;
    for i in (0..messages.len()).rev() {
        turn_map[i] = turn_counter;
        if messages[i].role == Role::Assistant {
            turn_counter += 1;
        }
    }

    // Use the larger of the caller-provided current_turn or our counted turns
    // to determine the age threshold. The age of a message = current_turn - its turn.
    let effective_current = current_turn.max(turn_counter);

    let mut chars_reclaimed: usize = 0;

    // 2. Walk messages and clear old tool results.
    for (i, msg) in messages.iter_mut().enumerate() {
        if msg.role != Role::User {
            continue;
        }

        let msg_turn = turn_map[i];

        // Exempt recent turns.
        if msg_turn < exempt_recent {
            continue;
        }

        // Only clear if the turn is old enough.
        let age = effective_current.saturating_sub(msg_turn);
        if age < age_threshold {
            continue;
        }

        for block in &mut msg.content {
            if let ContentBlock::ToolResult { content, .. } = block {
                // Idempotency: skip if already cleared (single text block with our marker).
                let already_cleared = content.len() == 1
                    && content[0]
                        .as_text()
                        .map(|t| t.starts_with("[Cleared"))
                        .unwrap_or(false);
                if already_cleared {
                    continue;
                }

                // Measure the content we're about to replace.
                let old_chars: usize = content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => text.len(),
                        _ => 64, // conservative estimate for non-text blocks
                    })
                    .sum();

                let placeholder = format!("[Cleared — tool result from turn {msg_turn}]");
                let placeholder_chars = placeholder.len();

                *content = vec![ContentBlock::text(placeholder)];

                // Only count net savings.
                chars_reclaimed += old_chars.saturating_sub(placeholder_chars);
            }
        }
    }

    // Convert chars to estimated tokens (chars / 4 heuristic).
    chars_reclaimed / 4
}

// ─── Main compaction entry point ──────────────────────────────────────────────

/// Compact `conversation` in-place using LLM summarisation, with a
/// deterministic truncation fallback if summarisation fails (e.g. the
/// conversation is too large to even summarise).
///
/// Steps:
/// 1. Persist all current messages to `session_messages`.
/// 2. Try `do_compact` (LLM summarisation).
/// 3. If that fails, fall back to `deterministic_compact` — a hard truncation
///    that keeps the system prompt and the most recent messages within ~80% of
///    the context window.
///
/// Returns `true` if compaction was performed, `false` only if both strategies fail.
pub(crate) async fn compact_conversation(
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    session_id: &str,
    task_id: &str,
    app_state: &AgentContext,
    ctx: CompactionContext,
    context_window: i64,
) -> bool {
    // 1. Persist current messages before replacing them.
    let repo = SessionMessageRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Err(e) = repo
        .insert_messages_batch(session_id, task_id, &conversation.messages)
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            session_id = %session_id,
            error = %e,
            "compaction: failed to persist messages before compaction"
        );
    }

    // 1b. Microcompaction pre-pass: clear old tool results in-place.
    //     Count assistant messages to determine the current turn number.
    let current_turn = conversation
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count();
    let tokens_reclaimed = microcompact(conversation, current_turn);

    if tokens_reclaimed > 0 {
        // Re-estimate total conversation size after microcompaction.
        let total_chars: usize = conversation
            .messages
            .iter()
            .map(estimate_message_chars)
            .sum();
        let estimated_tokens = total_chars / CHARS_PER_TOKEN.max(1);

        tracing::info!(
            task_id = %task_id,
            session_id = %session_id,
            tokens_reclaimed,
            estimated_tokens_after = estimated_tokens,
            context_window,
            "compaction: microcompaction reclaimed tokens"
        );

        // If microcompaction brought us below threshold, skip full compaction.
        if context_window > 0
            && (estimated_tokens as f64 / context_window as f64) < COMPACTION_THRESHOLD
        {
            tracing::info!(
                task_id = %task_id,
                session_id = %session_id,
                "compaction: microcompaction sufficient, skipping LLM compaction"
            );
            return true;
        }
    }

    // 1c. Extract last user text early (needed by both partial and full compaction).
    let last_user_text: Option<String> = conversation
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User && m.content.iter().any(|b| b.as_text().is_some()))
        .and_then(|m| {
            let t = m.text_content();
            if t.is_empty() { None } else { Some(t) }
        });

    // 1d. Partial compaction: summarise only the tail, preserving the prefix
    //     for prompt-cache hits.
    match partial_compact(provider, conversation, &ctx, context_window, &last_user_text).await {
        Ok(true) => {
            tracing::info!(
                task_id = %task_id,
                session_id = %session_id,
                "compaction: partial compaction succeeded"
            );
            return true;
        }
        Ok(false) => {
            tracing::debug!(
                task_id = %task_id,
                session_id = %session_id,
                "compaction: partial compaction skipped (tail too small), proceeding to full"
            );
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                session_id = %session_id,
                error = %e,
                "compaction: partial compaction failed, falling back to full compaction"
            );
        }
    }

    // 2. Extract system prompt and last plain-text user message before modifying.
    let system_msg: Option<Message> = conversation
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .cloned();

    // Re-extract last_user_text in case partial_compact mutated the conversation
    // (it shouldn't have on failure, but be defensive).
    let last_user_text: Option<String> = conversation
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User && m.content.iter().any(|b| b.as_text().is_some()))
        .and_then(|m| {
            let t = m.text_content();
            if t.is_empty() { None } else { Some(t) }
        });

    // 3. Ask the LLM to summarise, with overflow retry logic.
    //    If do_compact fails because the compaction input itself exceeds the
    //    model's context, progressively drop the oldest message groups and retry.
    let compact_result = do_compact_with_overflow_retry(
        provider,
        conversation,
        &ctx,
        task_id,
        session_id,
    )
    .await;

    match compact_result {
        Ok(summary) => {
            // 4. Replace conversation.
            let mut new_messages: Vec<Message> = Vec::new();

            if let Some(sys) = system_msg {
                new_messages.push(sys);
            }

            new_messages.push(Message::user(summary));

            let continuation_msg = match ctx {
                CompactionContext::PreResume(_) => {
                    "Your context was compacted before receiving reviewer feedback. \
                     The previous message contains a summary of your prior work session. \
                     You will receive feedback in the next message — read it carefully and \
                     use your tools to make the necessary changes."
                }
                _ => {
                    "Your context was compacted. The previous message contains a summary of the \
                     conversation so far. Continue calling tools as necessary to complete the task."
                }
            };
            new_messages.push(Message::assistant(continuation_msg));

            // For mid-session compaction, re-append the last user message so the
            // agent knows what it was working on. For pre-resume, skip this —
            // the feedback will be appended by the caller.
            if matches!(ctx, CompactionContext::MidSession(_))
                && let Some(last_user) = last_user_text
            {
                let already_appended = new_messages
                    .last()
                    .map(|m| m.role == Role::User && m.text_content() == last_user)
                    .unwrap_or(false);
                if !already_appended {
                    new_messages.push(Message::user(last_user));
                }
            }

            conversation.messages = new_messages;

            // NOTE: Do NOT increment continuation_count here.  That counter is
            // reserved for stale-review-cycle detection (TaskReviewRejectStale).
            // Compaction during a normal long-running session is expected and
            // must not eat into the stale-escalation budget.

            tracing::info!(
                task_id = %task_id,
                session_id = %session_id,
                context = ?ctx,
                "compaction: conversation compacted successfully"
            );
            true
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                session_id = %session_id,
                error = %e,
                "compaction: LLM summarisation failed, falling back to deterministic truncation"
            );

            // Deterministic fallback: truncate to fit within 80% of context window.
            if context_window > 0 {
                let max_chars = estimate_char_budget(context_window);
                let truncated = deterministic_compact(&conversation.messages, max_chars);
                if truncated.len() < conversation.messages.len() {
                    conversation.messages = truncated;
                    tracing::info!(
                        task_id = %task_id,
                        session_id = %session_id,
                        new_message_count = conversation.messages.len(),
                        "compaction: deterministic truncation applied"
                    );
                    return true;
                }
            }

            false
        }
    }
}

// ─── Compaction overflow retry ──────────────────────────────────────────────

/// Wrapper around `do_compact` that retries on context-length errors by
/// progressively dropping the oldest message groups from the compaction input.
///
/// Strategy:
/// 1. Try `do_compact` on the full message list.
/// 2. On context-length error, drop the oldest 20% of message groups and retry
///    (up to `COMPACTION_OVERFLOW_MAX_RETRIES` attempts).
/// 3. If all retries fail, run aggressive microcompaction (clear ALL tool
///    results older than 2 turns) and retry once more.
async fn do_compact_with_overflow_retry(
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    ctx: &CompactionContext,
    task_id: &str,
    session_id: &str,
) -> anyhow::Result<String> {
    // First attempt on the full conversation.
    let mut messages_for_compact = conversation.messages.clone();

    match do_compact(provider, &messages_for_compact, ctx).await {
        Ok(summary) => return Ok(summary),
        Err(e) if is_compaction_context_error(&e) => {
            tracing::warn!(
                task_id = %task_id,
                session_id = %session_id,
                error = %e,
                "compaction overflow: initial do_compact hit context limit, starting retry loop"
            );
        }
        Err(e) => return Err(e),
    }

    // Retry loop: drop oldest 20% of message groups on each attempt.
    for attempt in 1..=COMPACTION_OVERFLOW_MAX_RETRIES {
        drop_oldest_message_groups(&mut messages_for_compact, COMPACTION_OVERFLOW_DROP_FRACTION);

        tracing::info!(
            task_id = %task_id,
            session_id = %session_id,
            attempt,
            remaining_messages = messages_for_compact.len(),
            "compaction overflow: retrying after dropping oldest message groups"
        );

        match do_compact(provider, &messages_for_compact, ctx).await {
            Ok(summary) => return Ok(summary),
            Err(e) if is_compaction_context_error(&e) => {
                tracing::debug!(
                    task_id = %task_id,
                    session_id = %session_id,
                    attempt,
                    error = %e,
                    "compaction overflow: still over limit after dropping groups"
                );
            }
            Err(e) => return Err(e),
        }
    }

    // All retries exhausted — aggressive microcompaction fallback.
    tracing::warn!(
        task_id = %task_id,
        session_id = %session_id,
        "compaction overflow: all retries exhausted, running aggressive microcompaction"
    );

    let current_turn = conversation
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count();

    let tokens_reclaimed = microcompact_with_thresholds(
        conversation,
        current_turn,
        AGGRESSIVE_MICROCOMPACT_AGE, // age_threshold: 2 turns
        0,                           // exempt_recent: only spare the very latest turn
    );

    tracing::info!(
        task_id = %task_id,
        session_id = %session_id,
        tokens_reclaimed,
        "compaction overflow: aggressive microcompaction reclaimed tokens"
    );

    // Final attempt with the aggressively cleaned conversation.
    do_compact(provider, &conversation.messages, ctx).await
}

/// Check whether an error is a context-length / token-limit error.
/// Same heuristic used in `do_compact`'s inner retry loop.
fn is_compaction_context_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("context_length")
        || msg.contains("context limit")
        || msg.contains("too many tokens")
        || msg.contains("maximum context")
        || msg.contains("context window")
        || msg.contains("prompt is too long")
        || msg.contains("max_tokens")
        || msg.contains("token limit")
}

/// Drop the oldest `fraction` of message groups from a message list.
///
/// A "message group" is a pair of consecutive messages (typically user + assistant
/// or assistant + user). The system prompt (first message, if role == System) is
/// always preserved.
fn drop_oldest_message_groups(messages: &mut Vec<Message>, fraction: f64) {
    // Preserve system prompt.
    let start = if messages.first().map(|m| m.role == Role::System).unwrap_or(false) {
        1
    } else {
        0
    };

    let droppable = messages.len().saturating_sub(start);
    if droppable == 0 {
        return;
    }

    // Calculate how many messages to drop (group size = 2 messages).
    let groups = droppable / 2;
    let groups_to_drop = ((groups as f64 * fraction).ceil() as usize).max(1);
    let messages_to_drop = (groups_to_drop * 2).min(droppable);

    // Remove from the oldest end (right after system prompt).
    messages.drain(start..start + messages_to_drop);
}

// ─── Partial compaction ─────────────────────────────────────────────────────

/// Attempt partial compaction: summarise only the tail of the conversation
/// (messages after a pivot point at ~60% of total tokens) while preserving the
/// prefix verbatim.  This preserves the system prompt and early context for
/// prompt-cache hits.
///
/// Returns `Ok(true)` if partial compaction was performed, `Ok(false)` if the
/// tail is too small to be worth compacting (caller should fall through to full
/// compaction), or `Err` on LLM failure.
async fn partial_compact(
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    ctx: &CompactionContext,
    context_window: i64,
    last_user_text: &Option<String>,
) -> Result<bool, anyhow::Error> {
    let messages = &conversation.messages;
    if messages.len() < 4 {
        // Too few messages to meaningfully split.
        return Ok(false);
    }

    // 1. Estimate per-message token counts and find the pivot.
    let msg_tokens: Vec<usize> = messages
        .iter()
        .map(|m| estimate_message_chars(m) / CHARS_PER_TOKEN.max(1))
        .collect();
    let total_tokens: usize = msg_tokens.iter().sum();

    if total_tokens == 0 {
        return Ok(false);
    }

    let pivot_token_target = (total_tokens as f64 * PARTIAL_COMPACTION_PIVOT) as usize;

    // Walk from the start accumulating tokens to find the pivot index.
    // The pivot is the first message whose cumulative total reaches the target.
    // We never split inside the system message (index 0).
    let mut cumulative: usize = 0;
    let mut pivot_idx: usize = 1; // default: right after system prompt
    for (i, &tok) in msg_tokens.iter().enumerate() {
        cumulative += tok;
        if cumulative >= pivot_token_target {
            pivot_idx = i;
            break;
        }
    }

    // Ensure pivot is at least 1 (never include system msg in the tail).
    pivot_idx = pivot_idx.max(1);

    // Ensure the tail has at least 2 messages to summarise.
    if pivot_idx + 2 > messages.len() {
        return Ok(false);
    }

    // 2. Estimate reclaimable tokens (the tail).
    let tail_tokens: usize = msg_tokens[pivot_idx..].iter().sum();

    // Check if tail is large enough to be worth compacting.
    if context_window > 0
        && (tail_tokens as f64 / context_window as f64) < PARTIAL_COMPACTION_MIN_RECLAIM
    {
        tracing::debug!(
            tail_tokens,
            context_window,
            min_reclaim = PARTIAL_COMPACTION_MIN_RECLAIM,
            "partial_compact: tail too small, skipping"
        );
        return Ok(false);
    }

    // 3. Ensure we don't split a tool-use / tool-result pair.  If the message
    //    at pivot_idx is a user message containing ToolResult blocks, move the
    //    pivot back one so the preceding ToolUse assistant message is also in
    //    the tail (keeping the pair together).
    {
        use crate::message::ContentBlock;
        let pivot_msg = &messages[pivot_idx];
        if pivot_msg.role == Role::User
            && pivot_msg
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            && pivot_idx > 1
        {
            // Include the preceding assistant ToolUse message in the tail.
            pivot_idx -= 1;
        }
    }

    let prefix = &messages[..pivot_idx];
    let tail = &messages[pivot_idx..];

    tracing::info!(
        pivot_idx,
        prefix_messages = prefix.len(),
        tail_messages = tail.len(),
        tail_tokens,
        total_tokens,
        "partial_compact: attempting partial compaction"
    );

    // 4. Summarise the tail using progressive tool-response removal (same
    //    strategy as full compaction).
    let summary = do_partial_compact(provider, tail).await?;

    // 5. Rebuild the conversation: prefix + summary + continuation.
    let mut new_messages: Vec<Message> = prefix.to_vec();

    new_messages.push(Message::user(format!(
        "[Partial compaction: the following is a summary of {} messages that were \
         compacted to free context space. Earlier messages are preserved above.]\n\n{}",
        tail.len(),
        summary,
    )));

    let continuation_msg = match ctx {
        CompactionContext::PreResume(_) => {
            "Part of your context was compacted. The messages above the summary are \
             preserved verbatim; the summary covers your more recent work. You will \
             receive feedback in the next message."
        }
        _ => {
            "Part of your context was compacted. The messages above the summary are \
             preserved verbatim; the summary covers your more recent work. Continue \
             calling tools as necessary to complete the task."
        }
    };
    new_messages.push(Message::assistant(continuation_msg));

    // Re-append the last user message for mid-session so the agent knows
    // what it was working on (same logic as full compaction).
    if matches!(ctx, CompactionContext::MidSession(_))
        && let Some(last_user) = last_user_text
    {
        let already_appended = new_messages
            .last()
            .map(|m| m.role == Role::User && m.text_content() == *last_user)
            .unwrap_or(false);
        if !already_appended {
            new_messages.push(Message::user(last_user.clone()));
        }
    }

    conversation.messages = new_messages;
    Ok(true)
}

/// Summarise a slice of tail messages for partial compaction, using the same
/// progressive tool-response removal strategy as full compaction.
async fn do_partial_compact(
    provider: &dyn LlmProvider,
    tail_messages: &[Message],
) -> anyhow::Result<String> {
    const REMOVAL_PERCENTAGES: &[u32] = &[0, 10, 20, 50, 100];

    for &pct in REMOVAL_PERCENTAGES {
        let filtered = filter_tool_responses_middle_out(tail_messages, pct);
        let formatted = format_messages_as_text(&filtered);
        let prompt_text = PARTIAL_COMPACTION_PROMPT.replace("{messages}", &formatted);

        let mut compact_conv = Conversation::new();
        compact_conv.push(Message::system(PARTIAL_COMPACTION_SUMMARISER_SYSTEM));
        compact_conv.push(Message::user(prompt_text));

        match call_llm_for_summary(provider, &compact_conv).await {
            Ok(summary) if !summary.is_empty() => return Ok(summary),
            Ok(_) => {
                tracing::debug!(pct, "partial_compact: empty summary at removal pct, retrying");
            }
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                let is_ctx_error = msg.contains("context_length")
                    || msg.contains("too many tokens")
                    || msg.contains("maximum context")
                    || msg.contains("context window")
                    || msg.contains("prompt is too long");
                if is_ctx_error {
                    tracing::debug!(
                        pct,
                        error = %e,
                        "partial_compact: context length error, retrying with more removal"
                    );
                    continue;
                }
                return Err(e);
            }
        }
    }

    Err(anyhow::anyhow!(
        "partial_compact: failed to summarise tail even with 100% tool-response removal"
    ))
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// Attempt to summarise `messages` using the LLM, with progressive tool-response
/// removal to stay within the model's own context limit.
async fn do_compact(
    provider: &dyn LlmProvider,
    messages: &[Message],
    ctx: &CompactionContext,
) -> anyhow::Result<String> {
    // Progressive removal percentages (middle-out).
    const REMOVAL_PERCENTAGES: &[u32] = &[0, 10, 20, 50, 100];

    let prompt_template = compaction_prompt(ctx);
    let system_instruction = summariser_system(ctx);

    for &pct in REMOVAL_PERCENTAGES {
        let filtered = filter_tool_responses_middle_out(messages, pct);
        let formatted = format_messages_as_text(&filtered);
        let prompt_text = prompt_template.replace("{messages}", &formatted);

        // Build a minimal conversation: system instruction + user request.
        let mut compact_conv = Conversation::new();
        compact_conv.push(Message::system(system_instruction));
        compact_conv.push(Message::user(prompt_text));

        match call_llm_for_summary(provider, &compact_conv).await {
            Ok(summary) if !summary.is_empty() => return Ok(summary),
            Ok(_) => {
                // Empty response — try next percentage.
                tracing::debug!(pct, "compaction: empty summary at removal pct, retrying");
            }
            Err(e) => {
                let msg = e.to_string().to_lowercase();
                let is_ctx_error = msg.contains("context_length")
                    || msg.contains("too many tokens")
                    || msg.contains("maximum context")
                    || msg.contains("context window")
                    || msg.contains("prompt is too long");
                if is_ctx_error {
                    tracing::debug!(
                        pct,
                        error = %e,
                        "compaction: context length error at removal pct, retrying with more removal"
                    );
                    continue;
                }
                // Non-context error — propagate immediately.
                return Err(e);
            }
        }
    }

    Err(anyhow::anyhow!(
        "compaction: failed to summarise even with 100% tool-response removal"
    ))
}

/// Call the LLM with `conv` and collect all streamed text deltas into a string.
async fn call_llm_for_summary(
    provider: &dyn LlmProvider,
    conv: &Conversation,
) -> anyhow::Result<String> {
    let mut stream = provider.stream(conv, &[], None).await?;
    let mut summary = String::new();

    while let Some(evt) = stream.next().await {
        match evt? {
            StreamEvent::Delta(block) => {
                if let Some(text) = block.as_text() {
                    summary.push_str(text);
                }
            }
            StreamEvent::Done => break,
            StreamEvent::Usage(_) | StreamEvent::Thinking(_) => {}
        }
    }

    Ok(summary)
}

/// Remove a percentage of `ToolResult` messages from the middle outward
/// ("middle-out" strategy: drop from the centre outward).
fn filter_tool_responses_middle_out(messages: &[Message], remove_percent: u32) -> Vec<Message> {
    if remove_percent == 0 {
        return messages.to_vec();
    }

    // Collect indices of User messages that contain only ToolResult blocks.
    let tool_result_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.role == Role::User
                && !m.content.is_empty()
                && m.content
                    .iter()
                    .all(|b| matches!(b, crate::message::ContentBlock::ToolResult { .. }))
        })
        .map(|(i, _)| i)
        .collect();

    let total = tool_result_indices.len();
    if total == 0 {
        return messages.to_vec();
    }

    let to_remove = ((total as f64 * remove_percent as f64 / 100.0).ceil() as usize).min(total);
    if to_remove == 0 {
        return messages.to_vec();
    }

    // Remove from the middle outward.
    let mid = total / 2;
    let start = mid.saturating_sub(to_remove / 2);
    let end = (start + to_remove).min(total);
    let indices_to_remove: std::collections::HashSet<usize> =
        tool_result_indices[start..end].iter().copied().collect();

    messages
        .iter()
        .enumerate()
        .filter(|(i, _)| !indices_to_remove.contains(i))
        .map(|(_, m)| m.clone())
        .collect()
}

/// Render a message list as plain text for inclusion in the summary prompt.
fn format_messages_as_text(messages: &[Message]) -> String {
    use crate::message::ContentBlock;

    let mut out = String::new();
    for msg in messages {
        let role = match msg.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
        };
        for block in &msg.content {
            let line = match block {
                ContentBlock::Text { text } => format!("[{role}]: {text}"),
                ContentBlock::ToolUse { name, input, .. } => {
                    format!("[{role}]: tool_use({name}): {input}")
                }
                ContentBlock::ToolResult { content, .. } => {
                    let result: String = content
                        .iter()
                        .filter_map(|b| b.as_text())
                        .collect::<Vec<_>>()
                        .join("");
                    format!("[{role}]: tool_response: {result}")
                }
                // Thinking blocks are not relevant for compaction summaries.
                ContentBlock::Thinking { .. } => continue,
            };
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

// ─── Deterministic truncation ─────────────────────────────────────────────

/// Rough chars-per-token estimate. Conservative (low) so we don't overshoot.
const CHARS_PER_TOKEN: usize = 3;

/// Convert a token-based context window to a character budget at 80%.
fn estimate_char_budget(context_window: i64) -> usize {
    let tokens_80pct = (context_window as f64 * COMPACTION_THRESHOLD) as usize;
    tokens_80pct * CHARS_PER_TOKEN
}

/// Estimate the character size of a single message (all content blocks).
fn estimate_message_chars(msg: &Message) -> usize {
    use crate::message::ContentBlock;
    msg.content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.len(),
            ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
            ContentBlock::ToolResult { content, .. } => content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { text } => text.len(),
                    _ => 64,
                })
                .sum(),
            ContentBlock::Thinking { thinking } => thinking.len(),
        })
        .sum()
}

/// Deterministic compaction: keep the system prompt (first message) and as many
/// recent messages as fit within `max_chars`. Older messages in the middle are
/// dropped. A notice message is inserted where the cut happened.
///
/// Tool call/result pairs are kept together: if a user message with ToolResult
/// blocks is kept, the preceding assistant message with the corresponding
/// ToolUse blocks is also kept (and vice-versa). This prevents the "No tool
/// call found for function call output" error from the Responses API.
pub(crate) fn deterministic_compact(messages: &[Message], max_chars: usize) -> Vec<Message> {
    use crate::message::ContentBlock;

    if messages.is_empty() {
        return vec![];
    }

    // Always keep the system prompt.
    let system_msg = messages[0].clone();
    let system_chars = estimate_message_chars(&system_msg);

    // Reserve space for system + compaction notice.
    let notice_overhead = 200;
    let available = max_chars.saturating_sub(system_chars + notice_overhead);

    // Walk from the end backwards, keeping recent messages until budget is exhausted.
    let rest = &messages[1..];
    let mut kept_set: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut accumulated = 0usize;

    for (i, msg) in rest.iter().enumerate().rev() {
        let msg_chars = estimate_message_chars(msg);
        if accumulated + msg_chars > available {
            break;
        }
        accumulated += msg_chars;
        kept_set.insert(i);
    }

    // Ensure tool call/result pairs stay together.
    // If we kept a user message with ToolResult, ensure the preceding assistant
    // message with the matching ToolUse is also kept (and vice-versa).
    let mut changed = true;
    while changed {
        changed = false;
        let snapshot: Vec<usize> = kept_set.iter().copied().collect();
        for &i in &snapshot {
            let msg = &rest[i];
            if msg.role == Role::User
                && msg
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            {
                // Find preceding assistant message with ToolUse.
                if i > 0 && !kept_set.contains(&(i - 1)) {
                    let prev = &rest[i - 1];
                    if prev.role == Role::Assistant
                        && prev
                            .content
                            .iter()
                            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
                    {
                        accumulated += estimate_message_chars(prev);
                        kept_set.insert(i - 1);
                        changed = true;
                    }
                }
            } else if msg.role == Role::Assistant
                && msg
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
            {
                // Find following user message with ToolResult.
                if i + 1 < rest.len() && !kept_set.contains(&(i + 1)) {
                    let next = &rest[i + 1];
                    if next.role == Role::User
                        && next
                            .content
                            .iter()
                            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
                    {
                        accumulated += estimate_message_chars(next);
                        kept_set.insert(i + 1);
                        changed = true;
                    }
                }
            }
        }
    }

    // If pulling in pairs exceeded budget, drop the oldest pairs until we fit.
    if accumulated > available {
        let mut sorted: Vec<usize> = kept_set.iter().copied().collect();
        sorted.sort();
        while accumulated > available && sorted.len() > 2 {
            let oldest = sorted.remove(0);
            accumulated = accumulated.saturating_sub(estimate_message_chars(&rest[oldest]));
            kept_set.remove(&oldest);
            // If removing one half of a pair, remove the other too.
            if !sorted.is_empty() {
                let partner = sorted[0];
                let is_pair = (rest[oldest].role == Role::Assistant
                    && rest[partner].role == Role::User)
                    || (rest[oldest].role == Role::User && rest[partner].role == Role::Assistant);
                if is_pair {
                    accumulated =
                        accumulated.saturating_sub(estimate_message_chars(&rest[partner]));
                    kept_set.remove(&partner);
                    sorted.remove(0);
                }
            }
        }
    }

    let mut kept_indices: Vec<usize> = kept_set.into_iter().collect();
    kept_indices.sort();

    let mut result = vec![system_msg];

    if kept_indices.len() < rest.len() {
        let trimmed_count = rest.len() - kept_indices.len();
        result.push(Message::user(format!(
            "[Context compacted: {trimmed_count} earlier messages were trimmed to fit the context window. \
             The system prompt and most recent messages are preserved. Use `task_activity_list` or \
             `task_show` if you need historical context.]"
        )));
    }

    for &i in &kept_indices {
        result.push(rest[i].clone());
    }

    result
}

// ─── Conversation integrity validation ────────────────────────────────────────

/// Check that every `ToolResult` in the conversation references a `ToolUse`
/// with the same `id` in a preceding assistant message.  Returns the first
/// orphaned `tool_use_id` found, or `None` if the conversation is valid.
///
/// This can be used as a debug assertion after compaction and in tests.
#[cfg(test)]
pub(crate) fn find_orphaned_tool_result(messages: &[Message]) -> Option<String> {
    use crate::message::ContentBlock;
    use std::collections::HashSet;

    // Collect all ToolUse IDs emitted by assistant messages, in order.
    let mut known_tool_ids = HashSet::new();

    for msg in messages {
        if msg.role == Role::Assistant {
            for block in &msg.content {
                if let ContentBlock::ToolUse { id, .. } = block {
                    known_tool_ids.insert(id.clone());
                }
            }
        }
        if msg.role == Role::User {
            for block in &msg.content {
                if let ContentBlock::ToolResult { tool_use_id, .. } = block
                    && !known_tool_ids.contains(tool_use_id)
                {
                    return Some(tool_use_id.clone());
                }
            }
        }
    }

    None
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ContentBlock, Message, Role};

    #[test]
    fn needs_compaction_at_threshold() {
        assert!(needs_compaction(8000, 10000));
    }

    #[test]
    fn needs_compaction_below_threshold() {
        assert!(!needs_compaction(7999, 10000));
    }

    #[test]
    fn needs_compaction_zero_context_window() {
        assert!(!needs_compaction(99999, 0));
        assert!(!needs_compaction(99999, -1));
    }

    #[test]
    fn filter_tool_responses_zero_percent_unchanged() {
        let messages = vec![
            Message::user("hello"),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: vec![ContentBlock::text("result")],
                    is_error: false,
                }],
                metadata: None,
            },
        ];
        let filtered = filter_tool_responses_middle_out(&messages, 0);
        assert_eq!(filtered.len(), 2);
    }

    #[test]
    fn filter_tool_responses_100_percent_removes_all() {
        let messages = vec![
            Message::user("hello"),
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: vec![ContentBlock::text("r1")],
                    is_error: false,
                }],
                metadata: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: vec![ContentBlock::text("r2")],
                    is_error: false,
                }],
                metadata: None,
            },
        ];
        let filtered = filter_tool_responses_middle_out(&messages, 100);
        // Only the plain user message remains.
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].text_content(), "hello");
    }

    #[test]
    fn format_messages_as_text_includes_roles() {
        let messages = vec![
            Message::system("You are helpful."),
            Message::user("What is 2+2?"),
            Message::assistant("4"),
        ];
        let text = format_messages_as_text(&messages);
        assert!(text.contains("[system]: You are helpful."));
        assert!(text.contains("[user]: What is 2+2?"));
        assert!(text.contains("[assistant]: 4"));
    }

    #[test]
    fn deterministic_compact_keeps_system_and_recent() {
        let messages = vec![
            Message::system("System prompt that must be preserved."),
            Message::user("old message 1"),
            Message::assistant("old response 1"),
            Message::user("old message 2"),
            Message::assistant("old response 2"),
            Message::user("recent message"),
            Message::assistant("recent response"),
        ];
        // Budget that fits system + ~2 messages
        let budget = estimate_message_chars(&messages[0]) + 200 + 50;
        let result = deterministic_compact(&messages, budget);

        // System prompt is always first
        assert_eq!(result[0].role, Role::System);
        assert_eq!(
            result[0].text_content(),
            "System prompt that must be preserved."
        );

        // A compaction notice was inserted
        assert!(result[1].text_content().contains("Context compacted"));

        // Most recent message(s) are kept
        let last = result.last().unwrap();
        assert_eq!(last.text_content(), "recent response");

        // We have fewer messages than the original
        assert!(result.len() < messages.len());
    }

    #[test]
    fn deterministic_compact_no_trim_when_fits() {
        let messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("world"),
        ];
        // Generous budget
        let result = deterministic_compact(&messages, 100_000);

        // No trimming needed — same count, no notice
        assert_eq!(result.len(), 3);
        assert!(!result[1].text_content().contains("Context compacted"));
    }

    #[test]
    fn deterministic_compact_empty_input() {
        let result = deterministic_compact(&[], 1000);
        assert!(result.is_empty());
    }

    #[test]
    fn estimate_char_budget_80_percent() {
        // 10000 tokens * 0.8 * 3 chars/token = 24000
        assert_eq!(estimate_char_budget(10000), 24000);
    }

    #[test]
    fn deterministic_compact_keeps_tool_pairs_together() {
        // Simulate: system, old text, assistant(tool_use), user(tool_result), recent text
        let messages = vec![
            Message::system("sys"),
            Message::user("old message"),
            Message::assistant("old response"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "foo.rs"}),
                }],
                metadata: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: vec![ContentBlock::text("file contents")],
                    is_error: false,
                }],
                metadata: None,
            },
            Message::assistant("done"),
        ];

        // Budget that can fit system + last 3 messages but not all 5.
        // The tool pair (indices 2,3 in rest) should be kept together.
        let budget = estimate_message_chars(&messages[0])
            + estimate_message_chars(&messages[3])
            + estimate_message_chars(&messages[4])
            + estimate_message_chars(&messages[5])
            + 300;
        let result = deterministic_compact(&messages, budget);

        // Verify no orphaned tool results: every ToolResult must be preceded
        // by a ToolUse from the assistant.
        for (i, msg) in result.iter().enumerate() {
            if msg.role == Role::User
                && msg
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            {
                assert!(i > 0, "ToolResult at index 0 has no preceding ToolUse");
                let prev = &result[i - 1];
                assert!(
                    prev.role == Role::Assistant
                        && prev
                            .content
                            .iter()
                            .any(|b| matches!(b, ContentBlock::ToolUse { .. })),
                    "ToolResult at index {i} is not preceded by an assistant ToolUse message"
                );
            }
        }
    }

    #[test]
    fn compaction_prompt_varies_by_context() {
        let worker_resume = compaction_prompt(&CompactionContext::PreResume("worker".to_string()));
        let worker_mid = compaction_prompt(&CompactionContext::MidSession("worker".to_string()));
        let reviewer = compaction_prompt(&CompactionContext::MidSession("reviewer".to_string()));

        // Each context gets a different prompt
        assert!(worker_resume.contains("rejected or needs fixes"));
        assert!(worker_mid.contains("context window is full"));
        assert!(reviewer.contains("code review"));

        // All contain the messages placeholder
        assert!(worker_resume.contains("{messages}"));
        assert!(worker_mid.contains("{messages}"));
        assert!(reviewer.contains("{messages}"));
    }

    // ── Orphaned tool result detection ───────────────────────────────────────

    #[test]
    fn find_orphaned_tool_result_valid_conversation() {
        let messages = vec![
            Message::system("sys"),
            Message::user("do something"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "echo hi"}),
                }],
                metadata: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: vec![ContentBlock::text("hi")],
                    is_error: false,
                }],
                metadata: None,
            },
            Message::assistant("done"),
        ];
        assert!(find_orphaned_tool_result(&messages).is_none());
    }

    #[test]
    fn find_orphaned_tool_result_detects_orphan() {
        // Simulate what happens if compaction removes the assistant ToolUse
        // but a ToolResult referencing its call_id survives.
        let messages = vec![
            Message::system("sys"),
            Message::user("summary of prior work"),
            Message::assistant("Continuing with the task."),
            // Orphaned: no preceding ToolUse with "call_gone"
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_gone".into(),
                    content: vec![ContentBlock::text("result from vanished call")],
                    is_error: false,
                }],
                metadata: None,
            },
        ];
        assert_eq!(
            find_orphaned_tool_result(&messages),
            Some("call_gone".into()),
        );
    }

    #[test]
    fn find_orphaned_tool_result_multiple_tool_calls() {
        // Second tool result references a call_id that doesn't exist.
        let messages = vec![
            Message::system("sys"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::ToolUse {
                        id: "call_a".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path": "a.rs"}),
                    },
                    ContentBlock::ToolUse {
                        id: "call_b".into(),
                        name: "read_file".into(),
                        input: serde_json::json!({"path": "b.rs"}),
                    },
                ],
                metadata: None,
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call_a".into(),
                        content: vec![ContentBlock::text("contents a")],
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_b".into(),
                        content: vec![ContentBlock::text("contents b")],
                        is_error: false,
                    },
                    ContentBlock::ToolResult {
                        tool_use_id: "call_c_orphan".into(),
                        content: vec![ContentBlock::text("orphaned")],
                        is_error: false,
                    },
                ],
                metadata: None,
            },
        ];
        assert_eq!(
            find_orphaned_tool_result(&messages),
            Some("call_c_orphan".into()),
        );
    }

    /// Regression test: LLM compaction output (system + summary + continuation)
    /// must never contain orphaned tool results.
    #[test]
    fn llm_compaction_output_has_no_orphaned_tool_results() {
        // Simulate the conversation that compact_conversation builds on success.
        let compacted = vec![
            Message::system("You are a coding agent."),
            Message::user("## Summary\nFiles changed: src/main.rs — added feature X"),
            Message::assistant(
                "Your context was compacted. The previous message contains a summary.",
            ),
            Message::user("Continue with the task."),
        ];
        assert!(
            find_orphaned_tool_result(&compacted).is_none(),
            "LLM compaction output must not contain orphaned tool results",
        );
    }

    /// Regression test: simulates the scenario where proactive compaction
    /// replaces the conversation but tool results from the current turn would
    /// be appended — producing orphaned tool results. The fix ensures the
    /// reply loop skips tool dispatch after compaction; this test validates
    /// the invariant from the compaction side.
    #[test]
    fn appending_tool_results_after_compaction_creates_orphans() {
        // Step 1: Build a compacted conversation (what compact_conversation produces).
        let mut compacted = vec![
            Message::system("You are a coding agent."),
            Message::user("## Summary\nPrior work summary."),
            Message::assistant("Continuing with the task."),
        ];

        // Step 2: Simulate what the OLD buggy code did — append tool results
        // from the pre-compaction turn onto the compacted conversation.
        compacted.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_y2pswqYWoPzF2C3mROIIBbIZ".into(),
                content: vec![ContentBlock::text("bash output")],
                is_error: false,
            }],
            metadata: None,
        });

        // This must be detected as invalid.
        let orphan = find_orphaned_tool_result(&compacted);
        assert_eq!(
            orphan,
            Some("call_y2pswqYWoPzF2C3mROIIBbIZ".into()),
            "Appending tool results after compaction must produce an orphan — \
             the reply loop must skip tool dispatch after proactive compaction",
        );
    }

    /// Deterministic compaction must never produce orphaned tool results.
    #[test]
    fn deterministic_compact_never_produces_orphans() {
        // A conversation with interleaved tool call/result pairs and plain text.
        let messages = vec![
            Message::system("sys"),
            Message::user("task description"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                }],
                metadata: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: vec![ContentBlock::text("file1 file2")],
                    is_error: false,
                }],
                metadata: None,
            },
            Message::assistant("I see two files."),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_2".into(),
                    name: "read_file".into(),
                    input: serde_json::json!({"path": "file1"}),
                }],
                metadata: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_2".into(),
                    content: vec![ContentBlock::text("contents of file1")],
                    is_error: false,
                }],
                metadata: None,
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: "call_3".into(),
                    name: "edit_file".into(),
                    input: serde_json::json!({"path": "file1", "content": "new"}),
                }],
                metadata: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "call_3".into(),
                    content: vec![ContentBlock::text("ok")],
                    is_error: false,
                }],
                metadata: None,
            },
            Message::assistant("done editing"),
        ];

        // Try various tight budgets that force different amounts of truncation.
        for budget_multiplier in [1usize, 2, 3, 5, 10] {
            let budget = estimate_message_chars(&messages[0]) + 200 + (budget_multiplier * 50);
            let result = deterministic_compact(&messages, budget);

            let orphan = find_orphaned_tool_result(&result);
            assert!(
                orphan.is_none(),
                "deterministic_compact produced orphaned tool result {:?} at budget multiplier {}",
                orphan,
                budget_multiplier,
            );
        }
    }

    #[test]
    fn prompts_exist_for_expected_compaction_contexts() {
        let contexts = [
            CompactionContext::MidSession("worker".to_string()),
            CompactionContext::PreResume("worker".to_string()),
            CompactionContext::MidSession("reviewer".to_string()),
        ];

        for ctx in contexts {
            let prompt = compaction_prompt(&ctx);
            let system = summariser_system(&ctx);
            assert!(!prompt.is_empty());
            assert!(!system.is_empty());
        }
    }

    // ── Microcompaction tests ───────────────────────────────────────────────

    /// Build a conversation with N assistant turns, each preceded by a tool
    /// call/result pair.
    fn build_tool_conversation(num_turns: usize) -> Conversation {
        let mut conv = Conversation::new();
        conv.push(Message::system("You are a coding agent."));
        conv.push(Message::user("Do the task."));

        for i in 0..num_turns {
            let call_id = format!("call_{i}");
            conv.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::ToolUse {
                    id: call_id.clone(),
                    name: "bash".into(),
                    input: serde_json::json!({"command": format!("echo turn {i}")}),
                }],
                metadata: None,
            });
            conv.push(Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: call_id,
                    content: vec![ContentBlock::text(format!(
                        "This is a verbose result from turn {i} with lots of content: {}",
                        "x".repeat(200)
                    ))],
                    is_error: false,
                }],
                metadata: None,
            });
            conv.push(Message::assistant(format!("Processed turn {i}.")));
        }

        conv
    }

    #[test]
    fn microcompact_clears_old_tool_results() {
        let mut conv = build_tool_conversation(10);
        let tokens = microcompact(&mut conv, 10);

        // Should have reclaimed some tokens.
        assert!(tokens > 0, "expected tokens reclaimed, got {tokens}");

        // Recent turns (last MICROCOMPACT_EXEMPT_RECENT) should be untouched.
        // Check that the last few tool results are NOT cleared.
        let tool_results: Vec<(usize, &Message)> = conv
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| {
                m.role == Role::User
                    && m.content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            })
            .collect();

        // The last MICROCOMPACT_EXEMPT_RECENT tool results should not be cleared.
        for (_, msg) in tool_results.iter().rev().take(MICROCOMPACT_EXEMPT_RECENT) {
            for block in &msg.content {
                if let ContentBlock::ToolResult { content, .. } = block {
                    let text = content.iter().filter_map(|b| b.as_text()).collect::<String>();
                    assert!(
                        !text.starts_with("[Cleared"),
                        "recent tool result should not be cleared: {text}"
                    );
                }
            }
        }

        // Some old tool results should be cleared.
        let cleared_count = tool_results
            .iter()
            .filter(|(_, msg)| {
                msg.content.iter().any(|b| {
                    if let ContentBlock::ToolResult { content, .. } = b {
                        content
                            .first()
                            .and_then(|c| c.as_text())
                            .map(|t| t.starts_with("[Cleared"))
                            .unwrap_or(false)
                    } else {
                        false
                    }
                })
            })
            .count();
        assert!(
            cleared_count > 0,
            "expected some tool results to be cleared"
        );
    }

    #[test]
    fn microcompact_is_idempotent() {
        let mut conv = build_tool_conversation(10);
        let tokens_first = microcompact(&mut conv, 10);
        assert!(tokens_first > 0);

        // Second pass should reclaim nothing (already cleared).
        let tokens_second = microcompact(&mut conv, 10);
        assert_eq!(
            tokens_second, 0,
            "second microcompaction pass should reclaim 0 tokens"
        );
    }

    #[test]
    fn microcompact_no_op_for_short_conversations() {
        let mut conv = build_tool_conversation(3);
        let tokens = microcompact(&mut conv, 3);

        // With only 3 turns and MICROCOMPACT_EXEMPT_RECENT=3, nothing should
        // be cleared (all turns are exempt).
        assert_eq!(tokens, 0, "short conversation should not be microcompacted");
    }

    #[test]
    fn microcompact_preserves_conversation_integrity() {
        let mut conv = build_tool_conversation(10);
        microcompact(&mut conv, 10);

        // Every ToolResult should still reference a valid ToolUse.
        assert!(
            find_orphaned_tool_result(&conv.messages).is_none(),
            "microcompaction must not create orphaned tool results"
        );
    }

    // ── Partial compaction constants ────────────────────────────────────────

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn partial_compaction_pivot_is_reasonable() {
        // Pivot should be between 0 and 1, and significantly past the midpoint
        // to preserve a large prefix.
        assert!(PARTIAL_COMPACTION_PIVOT > 0.0);
        assert!(PARTIAL_COMPACTION_PIVOT < 1.0);
        assert!(PARTIAL_COMPACTION_PIVOT >= 0.5, "pivot should preserve at least half");
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn partial_compaction_min_reclaim_is_reasonable() {
        assert!(PARTIAL_COMPACTION_MIN_RECLAIM > 0.0);
        assert!(PARTIAL_COMPACTION_MIN_RECLAIM < 0.5);
    }

    #[test]
    fn partial_compaction_pivot_finding() {
        // Simulate the pivot-finding logic from partial_compact.
        let messages = vec![
            Message::system("System prompt with moderate length content."),
            Message::user("First user message"),
            Message::assistant("First assistant response with some content"),
            Message::user("Second user message"),
            Message::assistant("Second assistant response"),
            Message::user("Third user message - this is longer to shift the pivot"),
            Message::assistant("Third assistant response"),
            Message::user("Fourth user message"),
            Message::assistant("Fourth assistant response"),
        ];

        let msg_tokens: Vec<usize> = messages
            .iter()
            .map(|m| estimate_message_chars(m) / CHARS_PER_TOKEN.max(1))
            .collect();
        let total_tokens: usize = msg_tokens.iter().sum();
        let pivot_target = (total_tokens as f64 * PARTIAL_COMPACTION_PIVOT) as usize;

        let mut cumulative: usize = 0;
        let mut pivot_idx: usize = 1;
        for (i, &tok) in msg_tokens.iter().enumerate() {
            cumulative += tok;
            if cumulative >= pivot_target {
                pivot_idx = i;
                break;
            }
        }
        pivot_idx = pivot_idx.max(1);

        // Pivot should be somewhere in the middle, not at the very start or end.
        assert!(pivot_idx >= 1, "pivot must be after system message");
        assert!(
            pivot_idx + 2 <= messages.len(),
            "pivot must leave at least 2 messages in the tail"
        );
    }

    #[test]
    fn partial_compaction_skips_small_tail() {
        // When the tail would reclaim less than 20% of context window,
        // partial compaction should be skipped.
        let messages = [
            // Large system prompt that dominates the token count.
            Message::system("x".repeat(3000)),
            Message::user("tiny tail"),
            Message::assistant("tiny response"),
        ];

        let msg_tokens: Vec<usize> = messages
            .iter()
            .map(|m| estimate_message_chars(m) / CHARS_PER_TOKEN.max(1))
            .collect();
        let total_tokens: usize = msg_tokens.iter().sum();
        let pivot_target = (total_tokens as f64 * PARTIAL_COMPACTION_PIVOT) as usize;

        let mut cumulative: usize = 0;
        let mut pivot_idx: usize = 1;
        for (i, &tok) in msg_tokens.iter().enumerate() {
            cumulative += tok;
            if cumulative >= pivot_target {
                pivot_idx = i;
                break;
            }
        }
        pivot_idx = pivot_idx.max(1);

        let tail_tokens: usize = msg_tokens[pivot_idx..].iter().sum();
        let context_window: i64 = 10_000;

        // The tail is small relative to the context window, so partial compaction
        // should be skipped.
        let would_skip =
            (tail_tokens as f64 / context_window as f64) < PARTIAL_COMPACTION_MIN_RECLAIM;
        assert!(
            would_skip,
            "expected tail ({tail_tokens} tokens) to be too small for context window ({context_window})"
        );
    }

    #[test]
    fn partial_compaction_prompt_has_messages_placeholder() {
        assert!(PARTIAL_COMPACTION_PROMPT.contains("{messages}"));
    }

    // ── Compaction overflow retry helpers ───────────────────────────────────

    #[test]
    fn drop_oldest_message_groups_preserves_system() {
        let mut messages = vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a1"),
            Message::user("u2"),
            Message::assistant("a2"),
            Message::user("u3"),
            Message::assistant("a3"),
        ];

        drop_oldest_message_groups(&mut messages, 0.2);

        // System prompt must survive.
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[0].text_content(), "sys");

        // Should have dropped at least 1 group (2 messages).
        assert!(
            messages.len() <= 5,
            "expected at most 5 messages after dropping 20%, got {}",
            messages.len()
        );
    }

    #[test]
    fn drop_oldest_message_groups_drops_from_oldest_end() {
        let mut messages = vec![
            Message::system("sys"),
            Message::user("old1"),
            Message::assistant("old_resp1"),
            Message::user("old2"),
            Message::assistant("old_resp2"),
            Message::user("recent"),
            Message::assistant("recent_resp"),
        ];

        // Drop 50% of groups (3 groups total -> drop 2 -> 4 messages).
        drop_oldest_message_groups(&mut messages, 0.5);

        // System + remaining messages.
        assert_eq!(messages[0].role, Role::System);

        // The most recent messages should survive.
        let last = messages.last().unwrap();
        assert_eq!(last.text_content(), "recent_resp");
    }

    #[test]
    fn drop_oldest_message_groups_no_op_on_empty_or_system_only() {
        let mut messages = vec![Message::system("sys")];
        drop_oldest_message_groups(&mut messages, 0.5);
        assert_eq!(messages.len(), 1);

        let mut empty: Vec<Message> = vec![];
        drop_oldest_message_groups(&mut empty, 0.5);
        assert!(empty.is_empty());
    }

    #[test]
    fn drop_oldest_message_groups_multiple_rounds() {
        let mut messages = vec![
            Message::system("sys"),
            Message::user("u1"),
            Message::assistant("a1"),
            Message::user("u2"),
            Message::assistant("a2"),
            Message::user("u3"),
            Message::assistant("a3"),
            Message::user("u4"),
            Message::assistant("a4"),
            Message::user("u5"),
            Message::assistant("a5"),
        ];
        // 5 groups, drop 20% each time -> drop 1 group (2 messages) per round.
        let original_len = messages.len();

        drop_oldest_message_groups(&mut messages, COMPACTION_OVERFLOW_DROP_FRACTION);
        assert!(messages.len() < original_len);

        let after_first = messages.len();
        drop_oldest_message_groups(&mut messages, COMPACTION_OVERFLOW_DROP_FRACTION);
        assert!(messages.len() < after_first);

        // System prompt still there.
        assert_eq!(messages[0].role, Role::System);
    }

    #[test]
    fn is_compaction_context_error_detects_variants() {
        let cases = [
            "context_length exceeded",
            "too many tokens in prompt",
            "maximum context reached",
            "context window overflow",
            "prompt is too long",
            "max_tokens exceeded",
            "token limit reached",
            "context limit exceeded",
        ];
        for msg in cases {
            let e = anyhow::anyhow!("{msg}");
            assert!(
                is_compaction_context_error(&e),
                "should detect: {msg}"
            );
        }

        // Non-context errors should not match.
        let e = anyhow::anyhow!("rate limited");
        assert!(!is_compaction_context_error(&e));
    }

    #[test]
    fn aggressive_microcompact_clears_more_than_default() {
        let mut conv_default = build_tool_conversation(10);
        let mut conv_aggressive = build_tool_conversation(10);

        let tokens_default = microcompact(&mut conv_default, 10);
        let tokens_aggressive = microcompact_with_thresholds(
            &mut conv_aggressive,
            10,
            AGGRESSIVE_MICROCOMPACT_AGE,
            0,
        );

        // Aggressive should clear more than default.
        assert!(
            tokens_aggressive >= tokens_default,
            "aggressive ({tokens_aggressive}) should reclaim >= default ({tokens_default})"
        );
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn overflow_constants_are_reasonable() {
        assert!(COMPACTION_OVERFLOW_MAX_RETRIES >= 1);
        assert!(COMPACTION_OVERFLOW_MAX_RETRIES <= 5);
        assert!(COMPACTION_OVERFLOW_DROP_FRACTION > 0.0);
        assert!(COMPACTION_OVERFLOW_DROP_FRACTION < 0.5);
        assert!(AGGRESSIVE_MICROCOMPACT_AGE <= MICROCOMPACT_AGE_THRESHOLD);
        assert!(AGGRESSIVE_MICROCOMPACT_AGE >= 1);
    }
}
