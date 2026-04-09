use crate::context::AgentContext;
use crate::message::{Conversation, Message, Role};
use crate::provider::LlmProvider;
use djinn_db::SessionMessageRepository;

use super::prompts::{
    CompactionContext, last_user_text, rebuild_full_compaction_messages,
    rebuild_partial_compaction_messages,
};
use super::summarizer::{do_compact, do_partial_compact, is_context_error_message};

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

/// Return `true` if the accumulated input tokens have reached the compaction
/// threshold relative to the model's context window.
pub(crate) fn needs_compaction(total_tokens_in: u32, context_window: i64) -> bool {
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
    use crate::message::ContentBlock;

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
pub(crate) async fn compact_conversation(
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    session_id: &str,
    task_id: &str,
    app_state: &AgentContext,
    ctx: CompactionContext,
    context_window: i64,
) -> bool {
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

    let droppable = messages.len().saturating_sub(start);
    if droppable == 0 {
        return;
    }

    let groups = droppable / 2;
    let groups_to_drop = ((groups as f64 * fraction).ceil() as usize).max(1);
    let messages_to_drop = (groups_to_drop * 2).min(droppable);
    messages.drain(start..start + messages_to_drop);
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
        use crate::message::ContentBlock;
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
    conversation.messages =
        rebuild_partial_compaction_messages(prefix, tail.len(), summary, ctx, last_user_text);
    Ok(true)
}

fn estimate_char_budget(context_window: i64) -> usize {
    let tokens_80pct = (context_window as f64 * COMPACTION_THRESHOLD) as usize;
    tokens_80pct * CHARS_PER_TOKEN
}

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
            ContentBlock::Image { data, .. } => data.len(),
            ContentBlock::Document { data, .. } => data.len(),
            ContentBlock::Thinking { thinking } => thinking.len(),
        })
        .sum()
}

pub(crate) fn deterministic_compact(messages: &[Message], max_chars: usize) -> Vec<Message> {
    use crate::message::ContentBlock;

    if messages.is_empty() {
        return vec![];
    }

    let system_msg = messages[0].clone();
    let system_chars = estimate_message_chars(&system_msg);
    let notice_overhead = 200;
    let available = max_chars.saturating_sub(system_chars + notice_overhead);

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
                && i + 1 < rest.len()
                && !kept_set.contains(&(i + 1))
            {
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

    if accumulated > available {
        let mut sorted: Vec<usize> = kept_set.iter().copied().collect();
        sorted.sort();
        while accumulated > available && sorted.len() > 2 {
            let oldest = sorted.remove(0);
            accumulated = accumulated.saturating_sub(estimate_message_chars(&rest[oldest]));
            kept_set.remove(&oldest);
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

#[cfg(test)]
pub(crate) fn find_orphaned_tool_result(messages: &[Message]) -> Option<String> {
    use crate::message::ContentBlock;
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
mod tests {
    use super::*;
    use crate::message::ContentBlock;

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
        let budget = estimate_message_chars(&messages[0]) + 200 + 50;
        let result = deterministic_compact(&messages, budget);

        assert_eq!(result[0].role, Role::System);
        assert_eq!(
            result[0].text_content(),
            "System prompt that must be preserved."
        );
        assert!(result[1].text_content().contains("Context compacted"));
        assert_eq!(result.last().unwrap().text_content(), "recent response");
        assert!(result.len() < messages.len());
    }

    #[test]
    fn deterministic_compact_no_trim_when_fits() {
        let messages = vec![
            Message::system("sys"),
            Message::user("hello"),
            Message::assistant("world"),
        ];
        let result = deterministic_compact(&messages, 100_000);
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
        assert_eq!(estimate_char_budget(10000), 24000);
    }

    #[test]
    fn deterministic_compact_keeps_tool_pairs_together() {
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

        let budget = estimate_message_chars(&messages[0])
            + estimate_message_chars(&messages[3])
            + estimate_message_chars(&messages[4])
            + estimate_message_chars(&messages[5])
            + 300;
        let result = deterministic_compact(&messages, budget);

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
        let messages = vec![
            Message::system("sys"),
            Message::user("summary of prior work"),
            Message::assistant("Continuing with the task."),
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
            Some("call_gone".into())
        );
    }

    #[test]
    fn find_orphaned_tool_result_multiple_tool_calls() {
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
            Some("call_c_orphan".into())
        );
    }

    #[test]
    fn llm_compaction_output_has_no_orphaned_tool_results() {
        let compacted = vec![
            Message::system("You are a coding agent."),
            Message::user("## Summary\nFiles changed: src/main.rs — added feature X"),
            Message::assistant(
                "Your context was compacted. The previous message contains a summary.",
            ),
            Message::user("Continue with the task."),
        ];
        assert!(find_orphaned_tool_result(&compacted).is_none());
    }

    #[test]
    fn appending_tool_results_after_compaction_creates_orphans() {
        let mut compacted = vec![
            Message::system("You are a coding agent."),
            Message::user("## Summary\nPrior work summary."),
            Message::assistant("Continuing with the task."),
        ];

        compacted.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_y2pswqYWoPzF2C3mROIIBbIZ".into(),
                content: vec![ContentBlock::text("bash output")],
                is_error: false,
            }],
            metadata: None,
        });

        let orphan = find_orphaned_tool_result(&compacted);
        assert_eq!(orphan, Some("call_y2pswqYWoPzF2C3mROIIBbIZ".into()));
    }

    #[test]
    fn deterministic_compact_never_produces_orphans() {
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
        assert!(tokens > 0, "expected tokens reclaimed, got {tokens}");

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

        let exempt_tool_results = MICROCOMPACT_EXEMPT_RECENT / 2;
        for (_, msg) in tool_results.iter().rev().take(exempt_tool_results) {
            for block in &msg.content {
                if let ContentBlock::ToolResult { content, .. } = block {
                    let text = content
                        .iter()
                        .filter_map(|b| b.as_text())
                        .collect::<String>();
                    assert!(!text.starts_with("[Cleared"));
                }
            }
        }

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
        let tokens_second = microcompact(&mut conv, 10);
        assert_eq!(tokens_second, 0);
    }

    #[test]
    fn microcompact_no_op_for_short_conversations() {
        let mut conv = build_tool_conversation(3);
        let tokens = microcompact(&mut conv, 3);
        assert_eq!(tokens, 0);
    }

    #[test]
    fn microcompact_preserves_conversation_integrity() {
        let mut conv = build_tool_conversation(10);
        microcompact(&mut conv, 10);
        assert!(find_orphaned_tool_result(&conv.messages).is_none());
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn partial_compaction_pivot_is_reasonable() {
        assert!(PARTIAL_COMPACTION_PIVOT > 0.0);
        assert!(PARTIAL_COMPACTION_PIVOT < 1.0);
        assert!(PARTIAL_COMPACTION_PIVOT >= 0.5);
    }

    #[test]
    #[allow(clippy::assertions_on_constants)]
    fn partial_compaction_min_reclaim_is_reasonable() {
        assert!(PARTIAL_COMPACTION_MIN_RECLAIM > 0.0);
        assert!(PARTIAL_COMPACTION_MIN_RECLAIM < 0.5);
    }

    #[test]
    fn partial_compaction_pivot_finding() {
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

        assert!(pivot_idx >= 1);
        assert!(pivot_idx + 2 <= messages.len());
    }

    #[test]
    fn partial_compaction_skips_small_tail() {
        let messages = [
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
        let would_skip =
            (tail_tokens as f64 / context_window as f64) < PARTIAL_COMPACTION_MIN_RECLAIM;
        assert!(would_skip);
    }

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
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages[0].text_content(), "sys");
        assert!(messages.len() <= 5);
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

        drop_oldest_message_groups(&mut messages, 0.5);
        assert_eq!(messages[0].role, Role::System);
        assert_eq!(messages.last().unwrap().text_content(), "recent_resp");
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
        let original_len = messages.len();

        drop_oldest_message_groups(&mut messages, COMPACTION_OVERFLOW_DROP_FRACTION);
        assert!(messages.len() < original_len);

        let after_first = messages.len();
        drop_oldest_message_groups(&mut messages, COMPACTION_OVERFLOW_DROP_FRACTION);
        assert!(messages.len() < after_first);
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
            assert!(is_compaction_context_error(&e), "should detect: {msg}");
        }
        let e = anyhow::anyhow!("rate limited");
        assert!(!is_compaction_context_error(&e));
    }

    #[test]
    fn aggressive_microcompact_clears_more_than_default() {
        let mut conv_default = build_tool_conversation(10);
        let mut conv_aggressive = build_tool_conversation(10);

        let tokens_default = microcompact(&mut conv_default, 10);
        let tokens_aggressive =
            microcompact_with_thresholds(&mut conv_aggressive, 10, AGGRESSIVE_MICROCOMPACT_AGE, 0);

        assert!(tokens_aggressive >= tokens_default);
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
