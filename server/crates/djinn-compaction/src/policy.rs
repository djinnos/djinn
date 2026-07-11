use djinn_provider::message::{Conversation, Message, Role};
use djinn_provider::provider::LlmProvider;

use super::prompts::{
    CompactionContext, extract_prior_summary, last_user_text, rebuild_full_compaction_messages,
    rebuild_partial_compaction_messages,
};
use super::summarizer::{do_compact, do_partial_compact, is_context_error_message};

/// Find the indices of a compaction summary anchor pair in `messages`.
///
/// Returns `(summary_idx, continuation_idx)` if present — the user message
/// containing the summary and the immediately following assistant continuation.
fn find_summary_anchor_pair(messages: &[Message]) -> Option<(usize, usize)> {
    extract_prior_summary(messages).map(|(_, si, ci)| (si, ci))
}

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

/// Rough chars-per-token estimate. Conservative (low) so we don't overshoot.
const CHARS_PER_TOKEN: usize = 3;

/// C4: absolute upper bound on the LLM-produced compaction summary, in chars.
/// A verbose model can return an enormous summary that itself eats the context
/// window; we cap it before re-inserting it into the rebuilt conversation.
const SUMMARY_CAP_MAX_CHARS: usize = 8000;

/// C4: chars-per-token used to convert the context window into a char budget for
/// the summary cap. ≈4 chars/token (a looser estimate than `CHARS_PER_TOKEN`,
/// matching the heuristic used elsewhere for token→char conversion).
const SUMMARY_CAP_CHARS_PER_TOKEN: usize = 4;

/// C4: the summary is also capped to this fraction of the context window
/// (expressed as chars), so the cap scales down on small windows.
const SUMMARY_CAP_WINDOW_FRACTION: f64 = 0.15;

/// C4: cap the LLM compaction `summary` to `min(SUMMARY_CAP_MAX_CHARS, 15% of
/// context_window-as-chars)` using the shared `smart_truncate` helper (which
/// preserves head + tail and returns the input unchanged when it already fits).
///
/// `context_window` is in tokens; it is converted to a char budget via
/// `SUMMARY_CAP_CHARS_PER_TOKEN` (≈4). A non-positive window falls back to the
/// absolute cap. A summary already within the budget passes through unchanged.
fn cap_summary(summary: String, context_window: i64) -> String {
    let window_chars = if context_window > 0 {
        (context_window as f64 * SUMMARY_CAP_CHARS_PER_TOKEN as f64 * SUMMARY_CAP_WINDOW_FRACTION)
            as usize
    } else {
        SUMMARY_CAP_MAX_CHARS
    };
    let cap = SUMMARY_CAP_MAX_CHARS.min(window_chars);
    super::truncate::smart_truncate(&summary, cap)
}

/// Return `true` if the accumulated input tokens have reached the compaction
/// threshold relative to the model's context window.
pub fn needs_compaction(total_tokens_in: u32, context_window: i64) -> bool {
    if context_window <= 0 {
        return false;
    }
    total_tokens_in as f64 / context_window as f64 >= COMPACTION_THRESHOLD
}

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

fn microcompact_with_thresholds(
    conversation: &mut Conversation,
    current_turn: usize,
    age_threshold: usize,
    exempt_recent: usize,
) -> usize {
    use djinn_provider::message::ContentBlock;

    let messages = &mut conversation.messages;
    let mut turn_map: Vec<usize> = vec![0; messages.len()];
    let mut turn_counter: usize = 0;

    for i in (0..messages.len()).rev() {
        turn_map[i] = turn_counter;
        if messages[i].role == Role::Assistant {
            turn_counter += 1;
        }
    }

    let effective_current = current_turn.max(turn_counter);
    let mut chars_reclaimed: usize = 0;

    for (i, msg) in messages.iter_mut().enumerate() {
        if msg.role != Role::User {
            continue;
        }

        let msg_turn = turn_map[i];
        if msg_turn < exempt_recent {
            continue;
        }

        let age = effective_current.saturating_sub(msg_turn);
        if age < age_threshold {
            continue;
        }

        for block in &mut msg.content {
            if let ContentBlock::ToolResult { content, .. } = block {
                let already_cleared = content.len() == 1
                    && content[0]
                        .as_text()
                        .map(|t| t.starts_with("[Cleared"))
                        .unwrap_or(false);
                if already_cleared {
                    continue;
                }

                let old_chars: usize = content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { text } => text.len(),
                        _ => 64,
                    })
                    .sum();

                let placeholder = format!("[Cleared — tool result from turn {msg_turn}]");
                let placeholder_chars = placeholder.len();

                *content = vec![ContentBlock::text(placeholder)];
                chars_reclaimed += old_chars.saturating_sub(placeholder_chars);
            }
        }
    }

    chars_reclaimed / 4
}

/// Compact `conversation` in-place using LLM summarisation, with a
/// deterministic truncation fallback if summarisation fails.
pub async fn compact_conversation(
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    session_id: &str,
    task_id: &str,
    ctx: CompactionContext,
    context_window: i64,
) -> bool {
    // NOTE: the conversation is already persisted incrementally by the reply
    // loop (`persist_session_message` per assistant/tool-result turn), so we no
    // longer batch-insert the full history here — doing so would duplicate
    // every row in `session_messages`.
    let current_turn = conversation
        .messages
        .iter()
        .filter(|m| m.role == Role::Assistant)
        .count();
    let tokens_reclaimed = microcompact(conversation, current_turn);

    if tokens_reclaimed > 0 {
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

    let last_user_text = last_user_text(&conversation.messages);

    match partial_compact(
        provider,
        conversation,
        &ctx,
        context_window,
        &last_user_text,
    )
    .await
    {
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

    let compact_result =
        do_compact_with_overflow_retry(provider, conversation, &ctx, task_id, session_id).await;

    match compact_result {
        Ok(summary) => {
            // C4: cap the summary length before it is re-inserted, so a verbose
            // model cannot blow the context window with its own summary.
            let summary = cap_summary(summary, context_window);
            conversation.messages =
                rebuild_full_compaction_messages(&conversation.messages, summary, &ctx);

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

async fn do_compact_with_overflow_retry(
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    ctx: &CompactionContext,
    task_id: &str,
    session_id: &str,
) -> anyhow::Result<String> {
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

    let tokens_reclaimed =
        microcompact_with_thresholds(conversation, current_turn, AGGRESSIVE_MICROCOMPACT_AGE, 0);

    tracing::info!(
        task_id = %task_id,
        session_id = %session_id,
        tokens_reclaimed,
        "compaction overflow: aggressive microcompaction reclaimed tokens"
    );

    do_compact(provider, &conversation.messages, ctx).await
}

fn is_compaction_context_error(e: &anyhow::Error) -> bool {
    is_context_error_message(&e.to_string())
}

fn drop_oldest_message_groups(messages: &mut Vec<Message>, fraction: f64) {
    let start = if messages
        .first()
        .map(|m| m.role == Role::System)
        .unwrap_or(false)
    {
        1
    } else {
        0
    };

    // Detect a compaction summary anchor pair to preserve during drops.
    let anchor = find_summary_anchor_pair(&messages[start..]).map(|(a, b)| (a + start, b + start));

    let droppable = messages.len().saturating_sub(start);
    if droppable == 0 {
        return;
    }

    // Exclude anchor from the droppable count so it doesn't inflate drop budget.
    let anchor_len = anchor.map(|(a, b)| b - a + 1).unwrap_or(0);
    let droppable_excl_anchor = droppable.saturating_sub(anchor_len);
    if droppable_excl_anchor == 0 {
        return;
    }

    let groups = droppable_excl_anchor / 2;
    let groups_to_drop = ((groups as f64 * fraction).ceil() as usize).max(1);
    let remaining = (groups_to_drop * 2).min(droppable_excl_anchor);

    if let Some((anchor_start, anchor_end)) = anchor {
        // Drain in phases, skipping the anchor range.
        let pre_anchor_count = anchor_start - start;
        let phase1 = remaining.min(pre_anchor_count);
        if phase1 > 0 {
            messages.drain(start..start + phase1);
        }
        let still_needed = remaining - phase1;
        if still_needed > 0 {
            // After draining phase1, the anchor shifted left by phase1.
            let new_anchor_end = anchor_end - phase1;
            let post_start = new_anchor_end + 1;
            let available_after = messages.len().saturating_sub(post_start);
            let phase2 = still_needed.min(available_after);
            if phase2 > 0 {
                messages.drain(post_start..post_start + phase2);
            }
        }
    } else {
        messages.drain(start..start + remaining);
    }
}

async fn partial_compact(
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    ctx: &CompactionContext,
    context_window: i64,
    last_user_text: &Option<String>,
) -> Result<bool, anyhow::Error> {
    let messages = &conversation.messages;
    if messages.len() < 4 {
        return Ok(false);
    }

    let msg_tokens: Vec<usize> = messages
        .iter()
        .map(|m| estimate_message_chars(m) / CHARS_PER_TOKEN.max(1))
        .collect();
    let total_tokens: usize = msg_tokens.iter().sum();

    if total_tokens == 0 {
        return Ok(false);
    }

    let pivot_token_target = (total_tokens as f64 * PARTIAL_COMPACTION_PIVOT) as usize;
    let mut cumulative: usize = 0;
    let mut pivot_idx: usize = 1;
    for (i, &tok) in msg_tokens.iter().enumerate() {
        cumulative += tok;
        if cumulative >= pivot_token_target {
            pivot_idx = i;
            break;
        }
    }

    pivot_idx = pivot_idx.max(1);
    if pivot_idx + 2 > messages.len() {
        return Ok(false);
    }

    let tail_tokens: usize = msg_tokens[pivot_idx..].iter().sum();
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

    {
        use djinn_provider::message::ContentBlock;
        let pivot_msg = &messages[pivot_idx];
        if pivot_msg.role == Role::User
            && pivot_msg
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            && pivot_idx > 1
        {
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

    let summary = do_partial_compact(provider, tail).await?;
    // C4: cap the summary length before it is re-inserted, so a verbose model
    // cannot blow the context window with its own summary.
    let summary = cap_summary(summary, context_window);
    conversation.messages =
        rebuild_partial_compaction_messages(prefix, tail.len(), summary, ctx, last_user_text);
    Ok(true)
}

fn estimate_char_budget(context_window: i64) -> usize {
    let tokens_80pct = (context_window as f64 * COMPACTION_THRESHOLD) as usize;
    tokens_80pct * CHARS_PER_TOKEN
}

fn estimate_message_chars(msg: &Message) -> usize {
    use djinn_provider::message::ContentBlock;
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
            ContentBlock::Image { data, .. } => data.len(),
            ContentBlock::Document { data, .. } => data.len(),
            ContentBlock::Thinking { thinking, .. } => thinking.len(),
            ContentBlock::RedactedThinking { data } => data.len(),
            ContentBlock::Unknown { extra, .. } => extra.len(),
            ContentBlock::OpenAIReasoning {
                encrypted_content, ..
            } => encrypted_content.len(),
        })
        .sum()
}

/// Entry point for deterministic compaction. Orchestrates budget setup, recent-tail
/// selection, tool-pair closure, budget pruning, and result assembly helpers.
pub(crate) fn deterministic_compact(messages: &[Message], max_chars: usize) -> Vec<Message> {
    if messages.is_empty() {
        return vec![];
    }

    let (system_msg, rest, available) = deterministic_budget_setup(messages, max_chars);

    // Detect a compaction summary anchor pair to preserve.
    let anchor = find_summary_anchor_pair(rest);

    let mut state = deterministic_select_recent_tail(rest, available);
    deterministic_close_tool_pairs(rest, &mut state);

    // Force-include the anchor in the kept set so it survives pruning.
    if let Some((a, b)) = anchor {
        for &idx in &[a, b] {
            if !state.kept_set.contains(&idx) {
                state.accumulated += estimate_message_chars(&rest[idx]);
                state.kept_set.insert(idx);
            }
        }
    }

    deterministic_prune_over_budget(rest, &mut state, available, anchor);
    deterministic_assemble_result(system_msg, rest, &state, anchor.is_some())
}

/// Budget setup: extract the system message and compute the available character
/// budget after accounting for the system message and compacted-notice overhead.
fn deterministic_budget_setup(
    messages: &[Message],
    max_chars: usize,
) -> (Message, &[Message], usize) {
    let system_msg = messages[0].clone();
    let system_chars = estimate_message_chars(&system_msg);
    let notice_overhead = 200;
    let available = max_chars.saturating_sub(system_chars + notice_overhead);
    let rest = &messages[1..];
    (system_msg, rest, available)
}

/// Intermediate state for the deterministic compaction algorithm.
struct DetCompactState {
    kept_set: std::collections::HashSet<usize>,
    accumulated: usize,
}

/// Initial recent-tail selection: walk from the end of `rest`, greedily keeping
/// messages until the accumulated size would exceed the available budget.
fn deterministic_select_recent_tail(rest: &[Message], available: usize) -> DetCompactState {
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

    DetCompactState {
        kept_set,
        accumulated,
    }
}

/// Return `true` when `content` contains a `ToolResult` block.
fn content_block_contains_tool_result(content: &[djinn_provider::message::ContentBlock]) -> bool {
    content
        .iter()
        .any(|b| matches!(b, djinn_provider::message::ContentBlock::ToolResult { .. }))
}

/// Return `true` when `content` contains a `ToolUse` block.
fn content_block_contains_tool_use(content: &[djinn_provider::message::ContentBlock]) -> bool {
    content
        .iter()
        .any(|b| matches!(b, djinn_provider::message::ContentBlock::ToolUse { .. }))
}

/// Return `true` when `msg` is a user message containing tool results.
fn is_tool_result_msg(msg: &Message) -> bool {
    msg.role == Role::User && content_block_contains_tool_result(&msg.content)
}

/// Return `true` when `msg` is an assistant message containing tool uses.
fn is_tool_use_msg(msg: &Message) -> bool {
    msg.role == Role::Assistant && content_block_contains_tool_use(&msg.content)
}

/// Tool-pair closure: ensure that every kept tool-result message has its
/// preceding tool-use message in the kept set, and vice versa. This prevents
/// orphaned tool results/uses in the compacted output.
fn deterministic_close_tool_pairs(rest: &[Message], state: &mut DetCompactState) {
    let mut changed = true;
    while changed {
        changed = false;
        let snapshot: Vec<usize> = state.kept_set.iter().copied().collect();
        for &i in &snapshot {
            let msg = &rest[i];
            if is_tool_result_msg(msg) {
                // Keep the preceding assistant ToolUse if it exists but isn't kept yet.
                if i > 0 && !state.kept_set.contains(&(i - 1)) && is_tool_use_msg(&rest[i - 1]) {
                    state.accumulated += estimate_message_chars(&rest[i - 1]);
                    state.kept_set.insert(i - 1);
                    changed = true;
                }
            } else if is_tool_use_msg(msg)
                && i + 1 < rest.len()
                && !state.kept_set.contains(&(i + 1))
                && is_tool_result_msg(&rest[i + 1])
            {
                // Keep the following user ToolResult if it exists but isn't kept yet.
                state.accumulated += estimate_message_chars(&rest[i + 1]);
                state.kept_set.insert(i + 1);
                changed = true;
            }
        }
    }
}

/// Budget pruning: when the accumulated kept messages exceed the available
/// budget, remove the oldest entries (together with their pair partner when
/// the next entry forms a consecutive pair) until we fit or only two entries
/// remain. When `anchor` is `Some((a, b))`, those indices are never pruned.
fn deterministic_prune_over_budget(
    rest: &[Message],
    state: &mut DetCompactState,
    available: usize,
    anchor: Option<(usize, usize)>,
) {
    if state.accumulated <= available {
        return;
    }

    let protected: std::collections::HashSet<usize> = match anchor {
        Some((a, b)) => [a, b].into_iter().collect(),
        None => std::collections::HashSet::new(),
    };

    let mut sorted: Vec<usize> = state.kept_set.iter().copied().collect();
    sorted.sort();
    while state.accumulated > available && sorted.len() > 2 {
        // Find the oldest non-protected entry.
        let remove_pos = match sorted.iter().position(|idx| !protected.contains(idx)) {
            Some(p) => p,
            None => break,
        };

        let oldest = sorted.remove(remove_pos);
        state.accumulated = state
            .accumulated
            .saturating_sub(estimate_message_chars(&rest[oldest]));
        state.kept_set.remove(&oldest);
        if remove_pos < sorted.len() {
            let partner = sorted[remove_pos];
            if !protected.contains(&partner) {
                let is_pair = (rest[oldest].role == Role::Assistant
                    && rest[partner].role == Role::User)
                    || (rest[oldest].role == Role::User && rest[partner].role == Role::Assistant);
                if is_pair {
                    state.accumulated = state
                        .accumulated
                        .saturating_sub(estimate_message_chars(&rest[partner]));
                    state.kept_set.remove(&partner);
                    sorted.remove(remove_pos);
                }
            }
        }
    }
}

/// Final result assembly: prepend the system message, insert a compacted-notice
/// when messages were trimmed (unless a prior-summary anchor provides the
/// context instead), and append the kept messages in order.
fn deterministic_assemble_result(
    system_msg: Message,
    rest: &[Message],
    state: &DetCompactState,
    has_anchor: bool,
) -> Vec<Message> {
    let mut kept_indices: Vec<usize> = state.kept_set.iter().copied().collect();
    kept_indices.sort();

    let mut result = vec![system_msg];
    if kept_indices.len() < rest.len() && !has_anchor {
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

#[cfg(test)]
pub(crate) fn find_orphaned_tool_result(messages: &[Message]) -> Option<String> {
    use djinn_provider::message::ContentBlock;
    use std::collections::HashSet;

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

#[cfg(test)]
#[path = "policy_tests.rs"]
mod tests;
