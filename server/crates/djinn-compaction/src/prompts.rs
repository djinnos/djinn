use djinn_provider::message::{ContentBlock, Message, Role};

/// Describes *why* compaction is happening, so the prompt can be tailored.
#[derive(Debug, Clone)]
pub enum CompactionContext {
    /// Mid-session compaction: context window threshold reached while working.
    MidSession(String),
    /// Chat-session compaction: the interactive chat conversation grew past the
    /// model's window. Uses the generic summary prompt and, like `MidSession`,
    /// re-appends the user's latest turn after the summary so the model still
    /// sees the question it must answer.
    ChatSession,
}

/// Build the compaction prompt based on context.
pub(crate) fn compaction_prompt(ctx: &CompactionContext) -> &'static str {
    match ctx {
        CompactionContext::MidSession(role) if role == "worker" => MID_SESSION_WORKER_PROMPT,
        CompactionContext::MidSession(role) if role == "reviewer" || role == "task_reviewer" => {
            REVIEWER_PROMPT
        }
        _ => GENERIC_PROMPT,
    }
}

/// Build the system instruction for the summariser based on context.
pub(crate) fn summariser_system(ctx: &CompactionContext) -> &'static str {
    match ctx {
        CompactionContext::MidSession(role) if role == "worker" => {
            SUMMARISER_SYSTEM_WORKER_MID_SESSION
        }
        CompactionContext::MidSession(role) if role == "reviewer" || role == "task_reviewer" => {
            SUMMARISER_SYSTEM_TASK_REVIEWER
        }
        _ => SUMMARISER_SYSTEM_GENERIC,
    }
}

// C2: verbatim summary-template discipline. Each prompt presents the desired
// output as a fixed list of markdown section headings (the "template"), and the
// shared rules below enforce: terse bullets, `(none)` for empty sections (so the
// shape is stable and mergeable for C-4's incremental summaries), preserve exact
// identifiers, output ONLY the summary (no preamble/analysis — the summariser
// does not strip `<analysis>` tags, so requesting them leaked reasoning into the
// inserted summary), and never mention summarisation/compaction. Plain markdown
// headings (not nested XML the model must echo) keep this robust for smaller
// models (Kimi/GLM/Qwen).
pub(super) const TEMPLATE_RULES: &str = "Write the summary using EXACTLY the section headings below, in order. \
Under each heading use terse bullet points. If a section has nothing to record, write `(none)` — \
never omit a heading. Preserve exact file paths, function names, type names, line numbers, and \
error messages verbatim. Output only the summary in this format — no preamble, no reasoning, no \
closing remarks — and never mention this summary, the conversation's length, or that compaction occurred.";

// C-4: anchored / incremental summaries. The assistant continuation message that
// `rebuild_full_compaction_messages` inserts right after a summary is a fixed,
// recognisable marker. On the *next* full compaction we use it to find the prior
// summary, feed it back to the summariser as a `<previous-summary>` block to
// UPDATE/MERGE/PRUNE (rather than re-summarising the summary verbatim, which
// compounds summary-of-summary drift), and exclude the prior summary+marker pair
// from the messages being summarised. Each compaction fully rebuilds, so at most
// one such pair ever exists. Detection by content (not a persisted MessageMeta
// tag) keeps this off the bincode worker boundary and a clean no-op when absent.
/// Marker appended to summary text inside the user message so extraction can
/// locate and strip the boundary unambiguously, even when preserved tails or
/// re-appended user text follow the summary.
pub const COMPACTION_SUMMARY_END_MARKER: &str = "\n\n\n[Compaction summary complete]";

pub const FULL_COMPACTION_CONTINUATION: &str = "Your context was compacted. The previous message contains a summary of the \
     conversation so far. Continue calling tools as necessary to complete the task.";

/// Continuation message inserted after a *partial* compaction summary.
pub const PARTIAL_COMPACTION_CONTINUATION: &str = "Part of your context was compacted. The messages above the summary are \
     preserved verbatim; the summary covers your more recent work. Continue \
     calling tools as necessary to complete the task.";

/// Wrap a summary text with the full-compaction end marker so that
/// [`extract_prior_summary`] can locate the boundary later.
pub(super) fn wrap_full_summary(summary: &str) -> String {
    format!("{summary}{COMPACTION_SUMMARY_END_MARKER}")
}

/// Strip all known compaction boundary markers from extracted summary text.
///
/// Removes the end marker (and any single trailing newline left after
/// stripping) so the cleaned text can be fed into [`previous_summary_block`]
/// for the next prompt without leaking internal markers into the summariser.
pub fn strip_compaction_markers(text: &str) -> String {
    let out = text
        .strip_suffix(COMPACTION_SUMMARY_END_MARKER)
        .unwrap_or(text);
    // Remove a single trailing newline left after stripping.
    out.strip_suffix('\n').unwrap_or(out).to_string()
}

/// Find a prior compaction summary in `messages`: a user message whose content
/// ends with [`COMPACTION_SUMMARY_END_MARKER`], immediately followed by an
/// assistant message whose text matches either [`FULL_COMPACTION_CONTINUATION`]
/// or [`PARTIAL_COMPACTION_CONTINUATION`].
///
/// Returns `(summary_text, summary_idx, continuation_idx)` where
/// `summary_text` has all compaction boundary markers stripped via
/// [`strip_compaction_markers`]. At most one such pair exists.
pub fn extract_prior_summary(messages: &[Message]) -> Option<(String, usize, usize)> {
    for i in 1..messages.len() {
        if messages[i].role == Role::Assistant
            && messages[i - 1].role == Role::User
            && (messages[i].text_content() == FULL_COMPACTION_CONTINUATION
                || messages[i].text_content() == PARTIAL_COMPACTION_CONTINUATION)
        {
            return Some((
                strip_compaction_markers(&messages[i - 1].text_content()),
                i - 1,
                i,
            ));
        }
    }
    None
}

/// Render the `<previous-summary>` block that asks the summariser to merge an
/// earlier summary into the new one instead of re-summarising it verbatim.
pub fn previous_summary_block(prior: &str) -> String {
    format!(
        "A summary from an earlier point in this session is provided below. UPDATE and MERGE it \
         with the newer messages that follow — keep what is still true, prune what is now \
         obsolete or superseded, and fold in new progress. Do not re-summarise it verbatim or \
         duplicate its content.\n\n<previous-summary>\n{prior}\n</previous-summary>\n"
    )
}

pub(crate) const MID_SESSION_WORKER_PROMPT: &str = r#"You are a coding agent continuing your own in-progress work session. Summarise the conversation below into a handoff so you can resume without re-reading files.

{previous_summary}**Conversation:**
{messages}

{rules}

## Task Goal
## Files Changed
## Implementation Progress
## Code Decisions
## Errors + Fixes
## Codebase Context
## Current Work
## Next Steps"#;

pub(crate) const REVIEWER_PROMPT: &str = r#"You are a code review agent continuing your own review session. Summarise the conversation below into a handoff so you can resume the review without re-examining files.

{previous_summary}**Conversation:**
{messages}

{rules}

## Review Scope
## Files Reviewed
## Issues Found
## Positive Findings
## Assessment Progress
## Remaining Checks"#;

pub(crate) const GENERIC_PROMPT: &str = r#"You are an agent continuing a working session with a user. Summarise the conversation below into a handoff so the session can continue with no loss of context. Do not introduce goals or next steps the user did not confirm.

{previous_summary}**Conversation:**
{messages}

{rules}

## User Intent
## Technical Concepts
## Files + Code
## Errors + Fixes
## Problem Solving
## Pending Tasks
## Current Work
## Next Step"#;

pub(super) const PARTIAL_COMPACTION_PROMPT: &str = r#"The earlier portion of a conversation (system prompt and initial context) is preserved verbatim and is NOT shown below — only the tail that needs summarising appears here. Your summary will be inserted immediately after the preserved prefix, so it must connect naturally to that earlier context. Do not repeat information already in the preserved earlier context.

**Tail to summarise:**
{messages}

{rules}

## Progress Since Start
## Files Changed
## Code Decisions
## Errors + Fixes
## Current Work
## Next Steps"#;

pub(super) const PARTIAL_COMPACTION_SUMMARISER_SYSTEM: &str = "You summarise the tail of a coding agent's conversation. The beginning is preserved separately and the reader has it. Produce a dense, faithful, terse summary of only the provided messages, connecting naturally to the earlier context. Follow the requested section format exactly, output only the summary, and never mention summarisation or compaction.";

pub(crate) const SUMMARISER_SYSTEM_WORKER_MID_SESSION: &str = "You summarise a coding agent's in-progress work session. Produce a dense, faithful, terse summary that preserves all implementation context so the agent can continue without re-reading files. Follow the requested section format exactly, output only the summary, and never mention summarisation or compaction.";
pub(crate) const SUMMARISER_SYSTEM_TASK_REVIEWER: &str = "You summarise a code review session. Produce a dense, faithful, terse summary that preserves the review findings, issues identified, and assessment progress. Follow the requested section format exactly, output only the summary, and never mention summarisation or compaction.";
pub(crate) const SUMMARISER_SYSTEM_GENERIC: &str = "You summarise an agent–user working session. Produce a dense, faithful, terse summary. Follow the requested section format exactly, output only the summary, and never mention summarisation or compaction.";

pub(super) fn last_user_text(messages: &[Message]) -> Option<String> {
    messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User && m.content.iter().any(|b| b.as_text().is_some()))
        .and_then(|m| {
            let t = m.text_content();
            if t.is_empty() { None } else { Some(t) }
        })
}

/// C5: number of whole conversation turns to preserve verbatim after the summary
/// in a full compaction, so the agent keeps the most recent concrete exchange
/// (not just the final user line).
const FULL_COMPACTION_PRESERVED_TURNS: usize = 2;

/// `true` if `msg` is a `User` message carrying a `ToolResult` block. Such a
/// message is the *continuation* of the preceding assistant's tool-call turn — it
/// can never start a turn, because cutting before it would orphan the result from
/// its `ToolUse`.
fn is_tool_result_message(msg: &Message) -> bool {
    msg.role == Role::User
        && msg
            .content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
}

/// `true` if `msg` is an assistant message that issues a `ToolUse` which is NOT
/// answered by a `ToolResult` for the same id anywhere in `following`. A turn
/// ending on such a message is an *unanswered tool call* — re-inserting it after
/// the summary would leave the model owing a tool result it can never produce
/// (compaction often fires on exactly this message, before dispatch ran), so the
/// preserved tail must never end here.
fn has_unanswered_tool_use(msg: &Message, following: &[Message]) -> bool {
    if msg.role != Role::Assistant {
        return false;
    }
    msg.content.iter().any(|b| {
        if let ContentBlock::ToolUse { id, .. } = b {
            !following.iter().any(|f| {
                f.content.iter().any(|fb| {
                    matches!(fb, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == id)
                })
            })
        } else {
            false
        }
    })
}

/// C5: the `[start, end)` slice of `messages` to preserve verbatim after the
/// summary — the last `max_turns` whole, fully-resolved turns at/after `floor`
/// (`None` when there is nothing safe to preserve).
///
/// A turn boundary is the start of either an assistant message or a fresh user
/// prompt; a `User` message carrying a `ToolResult` is NOT a boundary (it belongs
/// to the preceding assistant's tool-call turn, so cutting before it would orphan
/// the result from its `ToolUse` — the same invariant `find_orphaned_tool_result`
/// guards). `start` is therefore guaranteed never to land on a tool-result
/// message, keeping every `ToolUse`/`ToolResult` pair intact.
///
/// The conversation often ends on a bare assistant `ToolUse` (compaction fires
/// before the tool ran), so `end` first excludes any trailing
/// unanswered-tool-call turn; only the resolved prefix is then preserved.
///
/// Only messages at/after `floor` are considered, so the system message (passed
/// as the floor when present) is never pulled into the preserved tail.
pub(super) fn preserved_tail_range(
    messages: &[Message],
    floor: usize,
    max_turns: usize,
) -> Option<(usize, usize)> {
    if max_turns == 0 || floor >= messages.len() {
        return None;
    }

    let mut end = messages.len();
    while end > floor && has_unanswered_tool_use(&messages[end - 1], &messages[end..]) {
        end -= 1;
    }
    if end <= floor {
        return None;
    }

    let mut turns_seen = 0usize;
    let mut start: Option<usize> = None;
    for i in (floor..end).rev() {
        if !is_tool_result_message(&messages[i]) {
            turns_seen += 1;
            start = Some(i);
            if turns_seen == max_turns {
                break;
            }
        }
    }

    match start {
        Some(idx) if !is_tool_result_message(&messages[idx]) => Some((idx, end)),
        _ => None,
    }
}

pub(super) fn rebuild_full_compaction_messages(
    original_messages: &[Message],
    summary: String,
    ctx: &CompactionContext,
) -> Vec<Message> {
    let system_msg = original_messages
        .iter()
        .find(|m| m.role == Role::System)
        .cloned();
    let last_user_text = last_user_text(original_messages);

    let mut new_messages: Vec<Message> = Vec::new();
    if let Some(sys) = system_msg {
        new_messages.push(sys);
    }

    new_messages.push(Message::user(wrap_full_summary(&summary)));

    new_messages.push(Message::assistant(FULL_COMPACTION_CONTINUATION));

    if matches!(
        ctx,
        CompactionContext::MidSession(_) | CompactionContext::ChatSession
    ) {
        // C5: preserve the last 1-2 WHOLE turns verbatim after the summary so the
        // agent keeps the most recent concrete exchange. The floor sits just
        // after the system message (if any) so the system prompt is never pulled
        // into the preserved tail; with no system message the floor is 0 so the
        // very first turn is still eligible.
        let floor = original_messages
            .iter()
            .position(|m| m.role == Role::System)
            .map(|i| i + 1)
            .unwrap_or(0);

        if let Some((start, end)) =
            preserved_tail_range(original_messages, floor, FULL_COMPACTION_PRESERVED_TURNS)
        {
            for msg in &original_messages[start..end] {
                new_messages.push(msg.clone());
            }
        }

        // Re-append the last user text only if the preserved tail did not already
        // end with it (avoids a double-appended last-user line). When no tail was
        // preserved, fall back to the prior behaviour of re-appending it so the
        // model still sees the question it must answer.
        if let Some(last_user) = last_user_text {
            let already_present = new_messages
                .last()
                .map(|m| m.role == Role::User && m.text_content() == last_user)
                .unwrap_or(false);
            if !already_present {
                new_messages.push(Message::user(last_user));
            }
        }
    }

    new_messages
}

pub(super) fn rebuild_partial_compaction_messages(
    prefix: &[Message],
    compacted_len: usize,
    preserved_tail: &[Message],
    summary: String,
    ctx: &CompactionContext,
    last_user_text: &Option<String>,
) -> Vec<Message> {
    let mut new_messages: Vec<Message> = prefix.to_vec();

    new_messages.push(Message::user(wrap_full_summary(&format!(
        "[Partial compaction: the following is a summary of {} messages that were \
         compacted to free context space. Earlier messages are preserved above.]\n\n{}",
        compacted_len, summary,
    ))));

    new_messages.push(Message::assistant(PARTIAL_COMPACTION_CONTINUATION));

    // Keep the closed tail after the continuation marker so the cacheable pivot
    // prefix remains byte-for-byte unchanged and the latest resolved turns are
    // not folded into the summary.
    new_messages.extend_from_slice(preserved_tail);

    if matches!(
        ctx,
        CompactionContext::MidSession(_) | CompactionContext::ChatSession
    ) && let Some(last_user) = last_user_text
    {
        let already_appended = new_messages
            .iter()
            .any(|m| m.role == Role::User && m.text_content() == *last_user);
        if !already_appended {
            new_messages.push(Message::user(last_user.clone()));
        }
    }

    new_messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_provider::message::Conversation;

    #[test]
    fn compaction_prompt_varies_by_context() {
        let worker_mid = compaction_prompt(&CompactionContext::MidSession("worker".to_string()));
        let reviewer = compaction_prompt(&CompactionContext::MidSession("reviewer".to_string()));

        assert!(worker_mid.contains("Implementation Progress"));
        assert!(reviewer.contains("Issues Found"));
        // C2: both carry the {messages} and {rules} placeholders the summariser
        // substitutes; the rules enforce the verbatim-template discipline.
        for p in [worker_mid, reviewer] {
            assert!(p.contains("{messages}"), "{p}");
            assert!(p.contains("{rules}"), "{p}");
        }
    }

    #[test]
    fn template_rules_enforce_discipline() {
        // C2: the shared rules must request `(none)` for empty sections, terse
        // bullets, and never mentioning compaction — the properties C-4's
        // mergeable summaries depend on.
        assert!(TEMPLATE_RULES.contains("(none)"));
        assert!(TEMPLATE_RULES.to_lowercase().contains("terse"));
        assert!(TEMPLATE_RULES.to_lowercase().contains("compaction"));
        assert!(TEMPLATE_RULES.to_lowercase().contains("only the summary"));
    }

    #[test]
    fn prompts_exist_for_expected_compaction_contexts() {
        let contexts = [
            CompactionContext::MidSession("worker".to_string()),
            CompactionContext::MidSession("reviewer".to_string()),
        ];

        for ctx in contexts {
            let prompt = compaction_prompt(&ctx);
            let system = summariser_system(&ctx);
            assert!(!prompt.is_empty());
            assert!(!system.is_empty());
        }
    }

    #[test]
    fn partial_compaction_prompt_has_messages_placeholder() {
        assert!(PARTIAL_COMPACTION_PROMPT.contains("{messages}"));
    }

    #[test]
    fn extract_prior_summary_roundtrips_rebuild() {
        // C-4: a summary produced by rebuild_full_compaction_messages must be
        // detectable on the next compaction (via the continuation marker), even
        // after the conversation grows with new turns.
        let original = vec![
            Message::system("sys"),
            Message::user("old work"),
            Message::assistant("did stuff"),
            Message::user("latest"),
        ];
        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "SUMMARY TEXT".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );
        let mut grown = rebuilt;
        grown.push(Message::assistant("more"));
        grown.push(Message::user("even more"));

        let (text, summary_idx, continuation_idx) =
            extract_prior_summary(&grown).expect("prior summary detected");
        assert_eq!(text, "SUMMARY TEXT");
        // The stored message includes the end marker; extraction strips it.
        assert_eq!(
            strip_compaction_markers(&grown[summary_idx].text_content()),
            "SUMMARY TEXT"
        );
        assert_eq!(
            grown[continuation_idx].text_content(),
            FULL_COMPACTION_CONTINUATION
        );
    }

    #[test]
    fn extract_prior_summary_none_without_marker() {
        let msgs = vec![
            Message::system("s"),
            Message::user("u"),
            Message::assistant("a"),
        ];
        assert!(extract_prior_summary(&msgs).is_none());
    }

    #[test]
    fn previous_summary_block_wraps_and_instructs_merge() {
        let block = previous_summary_block("PRIOR");
        assert!(block.contains("<previous-summary>"));
        assert!(block.contains("PRIOR"));
        assert!(block.to_lowercase().contains("merge"));
    }

    #[test]
    fn full_prompts_carry_previous_summary_slot() {
        for ctx in [
            CompactionContext::MidSession("worker".to_string()),
            CompactionContext::MidSession("reviewer".to_string()),
            CompactionContext::ChatSession,
        ] {
            assert!(
                compaction_prompt(&ctx).contains("{previous_summary}"),
                "{ctx:?} prompt should carry the C-4 previous-summary slot"
            );
        }
    }

    #[test]
    fn chat_session_uses_generic_prompt_and_system() {
        // C3: chat compaction routes to the generic prompt/system (it is neither
        // the worker nor reviewer flow).
        let prompt = compaction_prompt(&CompactionContext::ChatSession);
        let system = summariser_system(&CompactionContext::ChatSession);
        assert_eq!(prompt, GENERIC_PROMPT);
        assert_eq!(system, SUMMARISER_SYSTEM_GENERIC);
        assert!(prompt.contains("{messages}"));
    }

    #[test]
    fn rebuild_full_compaction_reappends_last_user_for_chat_session() {
        // C3: a compacted chat conversation must still end with the user's
        // latest turn so the model sees the question it has to answer.
        let original = vec![
            Message::system("sys"),
            Message::user("old"),
            Message::assistant("mid"),
            Message::user("what is the latest answer"),
        ];

        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "summary".to_string(),
            &CompactionContext::ChatSession,
        );

        assert_eq!(rebuilt[0].role, Role::System);
        assert_eq!(
            strip_compaction_markers(&rebuilt[1].text_content()),
            "summary"
        );
        assert_eq!(
            rebuilt.last().unwrap().text_content(),
            "what is the latest answer"
        );
    }

    #[test]
    fn rebuild_full_compaction_messages_preserves_surface_shape() {
        let original = vec![
            Message::system("sys"),
            Message::user("old"),
            Message::assistant("mid"),
            Message::user("latest user"),
        ];

        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "summary".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );

        assert_eq!(rebuilt[0].role, Role::System);
        assert_eq!(
            strip_compaction_markers(&rebuilt[1].text_content()),
            "summary"
        );
        assert_eq!(rebuilt.last().unwrap().text_content(), "latest user");
    }

    #[test]
    fn rebuild_partial_compaction_messages_reappends_last_user() {
        let prefix = vec![Message::system("sys"), Message::user("kept")];
        let rebuilt = rebuild_partial_compaction_messages(
            &prefix,
            3,
            &[],
            "summary".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
            &Some("latest user".to_string()),
        );

        assert_eq!(rebuilt[0].role, Role::System);
        assert_eq!(rebuilt[1].text_content(), "kept");

        let summary_message = rebuilt
            .iter()
            .find(|message| {
                message.role == Role::User
                    && message
                        .text_content()
                        .contains("[Partial compaction: the following is a summary of 3 messages")
            })
            .expect("partial-compaction summary message should be inserted");
        assert!(summary_message.text_content().contains("summary"));

        let continuation_message = rebuilt
            .iter()
            .find(|message| {
                message.role == Role::Assistant
                    && message
                        .text_content()
                        .contains("Part of your context was compacted")
            })
            .expect("continuation assistant message should be inserted");
        assert!(
            continuation_message
                .text_content()
                .contains("Continue calling tools as necessary to complete the task")
        );

        assert_eq!(rebuilt.last().unwrap().text_content(), "latest user");
    }

    #[test]
    fn last_user_text_skips_empty_text_messages() {
        let messages = vec![
            Message::system("sys"),
            Message {
                role: Role::User,
                content: vec![],
                metadata: None,
            },
            Message::user("real user text"),
        ];

        assert_eq!(
            last_user_text(&messages),
            Some("real user text".to_string())
        );
    }

    #[test]
    fn last_user_text_returns_none_when_absent() {
        let conversation = Conversation::new();
        assert_eq!(last_user_text(&conversation.messages), None);
    }

    // ─── C5: preserve last whole turns after the summary ─────────────────────

    use djinn_provider::message::ContentBlock as CB;

    fn tool_use(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![CB::ToolUse {
                id: id.into(),
                name: name.into(),
                input: serde_json::json!({"k": "v"}),
            }],
            metadata: None,
        }
    }

    fn tool_result(id: &str, text: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![CB::ToolResult {
                tool_use_id: id.into(),
                content: vec![CB::text(text)],
                is_error: false,
            }],
            metadata: None,
        }
    }

    /// C5: the last 1-2 WHOLE turns are re-appended verbatim after the
    /// summary+continuation, so the model keeps the most recent concrete
    /// exchange — not just the final user line.
    #[test]
    fn full_compaction_preserves_last_whole_turns_verbatim() {
        let original = vec![
            Message::system("sys"),
            Message::user("very old"),
            Message::assistant("old reply"),
            Message::user("recent question"),
            Message::assistant("recent answer to it"),
        ];

        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "summary".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );

        // system, summary, continuation, then the preserved tail verbatim.
        assert_eq!(rebuilt[0].role, Role::System);
        assert_eq!(
            strip_compaction_markers(&rebuilt[1].text_content()),
            "summary"
        );
        assert_eq!(rebuilt[2].text_content(), FULL_COMPACTION_CONTINUATION);

        // The last two whole turns (the recent user + recent assistant) appear
        // verbatim, in order, after the continuation.
        assert_eq!(rebuilt[3].text_content(), "recent question");
        assert_eq!(rebuilt[3].role, Role::User);
        assert_eq!(rebuilt[4].text_content(), "recent answer to it");
        assert_eq!(rebuilt[4].role, Role::Assistant);

        // The much older turn is NOT carried over verbatim (only summarised).
        assert!(!rebuilt.iter().any(|m| m.text_content() == "very old"));
    }

    /// C5 CRITICAL: a tool-call/tool-result pair in the preserved tail must stay
    /// together — never orphaned. Asserted via the existing orphan invariant.
    #[test]
    fn full_compaction_preserved_tail_keeps_tool_pair_intact() {
        let original = vec![
            Message::system("sys"),
            Message::user("kick off"),
            Message::assistant("older reply"),
            // Last whole turn: assistant ToolUse + its User ToolResult.
            tool_use("call_42", "bash"),
            tool_result("call_42", "command output"),
        ];

        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "summary".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );

        // No orphaned tool result anywhere in the rebuilt conversation.
        assert!(
            crate::policy::find_orphaned_tool_result(&rebuilt).is_none(),
            "preserved tail orphaned a tool result"
        );

        // The pair survived verbatim and adjacent: the ToolUse is immediately
        // followed by its ToolResult.
        let use_idx = rebuilt
            .iter()
            .position(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, CB::ToolUse { id, .. } if id == "call_42"))
            })
            .expect("ToolUse preserved");
        assert!(matches!(
            &rebuilt[use_idx + 1].content[0],
            CB::ToolResult { tool_use_id, .. } if tool_use_id == "call_42"
        ));
    }

    /// C5 CRITICAL: the preserved tail must START on a turn boundary, never on a
    /// bare tool-result message (which would orphan it). Here only the last turn
    /// fits within the 2-turn budget but is itself a tool-call turn — the cut
    /// must land on the assistant ToolUse, not between it and its result.
    #[test]
    fn full_compaction_never_starts_tail_on_orphan_tool_result() {
        let original = vec![
            Message::system("sys"),
            Message::assistant("turn one reply"),
            tool_use("call_a", "read"),
            tool_result("call_a", "file a"),
            tool_use("call_b", "read"),
            tool_result("call_b", "file b"),
        ];

        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "summary".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );

        assert!(
            crate::policy::find_orphaned_tool_result(&rebuilt).is_none(),
            "tail started mid-pair and orphaned a tool result"
        );

        // First preserved message after the continuation marker must be an
        // assistant ToolUse (a boundary), not a bare User ToolResult.
        let continuation_idx = rebuilt
            .iter()
            .position(|m| m.text_content() == FULL_COMPACTION_CONTINUATION)
            .unwrap();
        let first_preserved = &rebuilt[continuation_idx + 1];
        assert!(!is_tool_result_message(first_preserved));
        assert_eq!(first_preserved.role, Role::Assistant);
    }

    /// C5 CRITICAL: when compaction fires on a bare assistant ToolUse (before the
    /// tool ran, so no ToolResult exists), that dangling-tool-call turn must NOT be
    /// preserved — it would leave the model owing a result it can never produce.
    #[test]
    fn full_compaction_trims_trailing_unanswered_tool_call() {
        let original = vec![
            Message::system("sys"),
            Message::user("Do the task."),
            // Compaction fired here: a tool call with no result yet.
            tool_use("t1", "nonexistent_tool"),
        ];

        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "summary".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );

        // No ToolUse without a matching result should survive into the rebuilt
        // conversation.
        let dangling = rebuilt.iter().any(|m| {
            m.content
                .iter()
                .any(|b| matches!(b, CB::ToolUse { id, .. } if id == "t1"))
        });
        assert!(
            !dangling,
            "dangling tool call was preserved after the summary"
        );
        assert!(crate::policy::find_orphaned_tool_result(&rebuilt).is_none());

        // The user line is still preserved/re-appended so the model has the task.
        assert_eq!(rebuilt.last().unwrap().text_content(), "Do the task.");
        // And it is not double-appended.
        let occ = rebuilt
            .iter()
            .filter(|m| m.role == Role::User && m.text_content() == "Do the task.")
            .count();
        assert_eq!(occ, 1);
    }

    /// C5: do NOT double-append the last user text when the preserved tail
    /// already ends with it.
    #[test]
    fn full_compaction_does_not_double_append_last_user() {
        let original = vec![
            Message::system("sys"),
            Message::user("old"),
            Message::assistant("reply"),
            Message::user("the final user line"),
        ];

        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "summary".to_string(),
            &CompactionContext::ChatSession,
        );

        let occurrences = rebuilt
            .iter()
            .filter(|m| m.role == Role::User && m.text_content() == "the final user line")
            .count();
        assert_eq!(
            occurrences, 1,
            "last user line must appear exactly once, not double-appended"
        );
        assert_eq!(
            rebuilt.last().unwrap().text_content(),
            "the final user line"
        );
    }

    /// C5: re-append the last user line when the preserved tail ends on an
    /// assistant turn (so the model still sees the question), but only once.
    #[test]
    fn full_compaction_reappends_last_user_when_tail_ends_on_assistant() {
        let original = vec![
            Message::system("sys"),
            Message::user("older"),
            Message::user("the question"),
            Message::assistant("a partial answer"),
        ];

        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "summary".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );

        // Tail ends on an assistant message, so the last user line is re-appended
        // at the very end so the model still sees the question it must answer.
        assert_eq!(rebuilt.last().unwrap().role, Role::User);
        assert_eq!(rebuilt.last().unwrap().text_content(), "the question");
        // It appears within the preserved tail AND re-appended at the end (the
        // double-append guard only suppresses when the tail already ENDS with it).
        let occurrences = rebuilt
            .iter()
            .filter(|m| m.role == Role::User && m.text_content() == "the question")
            .count();
        assert_eq!(occurrences, 2);
    }

    /// C5: short conversation — only the system + summary scaffolding exists, so
    /// preservation is a graceful no-op (nothing to carry over verbatim).
    #[test]
    fn full_compaction_short_conversation_no_op() {
        // System + a single user line is too short for a whole prior turn beyond
        // it; tail_turn_start must still behave and we must not panic or orphan.
        let original = vec![Message::system("sys"), Message::user("only line")];

        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "summary".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );

        assert!(crate::policy::find_orphaned_tool_result(&rebuilt).is_none());
        // No system-only / summary-only message gets preserved as a "turn"; the
        // last user line is still re-appended once for the model to answer.
        assert_eq!(rebuilt[0].role, Role::System);
        assert_eq!(
            strip_compaction_markers(&rebuilt[1].text_content()),
            "summary"
        );
        assert_eq!(rebuilt[2].text_content(), FULL_COMPACTION_CONTINUATION);
        assert_eq!(rebuilt.last().unwrap().text_content(), "only line");
    }

    /// C5: the system message itself is never pulled into the preserved tail.
    #[test]
    fn full_compaction_preserved_tail_excludes_system() {
        let original = vec![
            Message::system("SYSTEM PROMPT"),
            Message::user("q"),
            Message::assistant("a"),
        ];

        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "summary".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );

        // Exactly one System message (the rebuilt header), never duplicated in
        // the preserved tail.
        let system_count = rebuilt.iter().filter(|m| m.role == Role::System).count();
        assert_eq!(system_count, 1);
    }

    /// C5: with no system message present, the very first turn is still eligible
    /// for preservation (floor = 0).
    #[test]
    fn full_compaction_no_system_preserves_from_start() {
        let original = vec![
            Message::user("first question"),
            Message::assistant("first answer"),
        ];

        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "summary".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );

        // summary, continuation, then both original turns verbatim.
        assert_eq!(
            strip_compaction_markers(&rebuilt[0].text_content()),
            "summary"
        );
        assert_eq!(rebuilt[1].text_content(), FULL_COMPACTION_CONTINUATION);
        assert_eq!(rebuilt[2].text_content(), "first question");
        assert_eq!(rebuilt[3].text_content(), "first answer");
        // Tail ended on assistant → last user re-appended once.
        assert_eq!(rebuilt.last().unwrap().text_content(), "first question");
    }

    /// C5 (regression): the C-4 continuation marker still round-trips through
    /// `extract_prior_summary` even though a verbatim tail now follows it.
    #[test]
    fn full_compaction_with_preserved_tail_still_detectable_by_c4() {
        let original = vec![
            Message::system("sys"),
            Message::user("old work"),
            Message::assistant("did stuff"),
            Message::user("latest"),
            Message::assistant("latest reply"),
        ];
        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "SUMMARY TEXT".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );

        let (text, summary_idx, continuation_idx) =
            extract_prior_summary(&rebuilt).expect("prior summary still detected");
        assert_eq!(text, "SUMMARY TEXT");
        // The stored message includes the end marker; extraction strips it.
        assert_eq!(
            strip_compaction_markers(&rebuilt[summary_idx].text_content()),
            "SUMMARY TEXT"
        );
        assert_eq!(
            rebuilt[continuation_idx].text_content(),
            FULL_COMPACTION_CONTINUATION
        );
    }

    // ─── 52te: boundary marker and extraction hardening tests ──────────────

    ///52te: the end marker is stripped on extraction so the prior summary fed
    /// to the summariser is clean text, not internal boundary markup.
    #[test]
    fn extract_prior_summary_strips_end_marker() {
        let msgs = vec![
            Message::system("sys"),
            Message::user(format!("summary text{COMPACTION_SUMMARY_END_MARKER}")),
            Message::assistant(FULL_COMPACTION_CONTINUATION),
        ];
        let (text, _, _) = extract_prior_summary(&msgs).expect("should detect");
        assert_eq!(text, "summary text");
        assert!(!text.contains(COMPACTION_SUMMARY_END_MARKER));
    }

    ///52te: partial compaction summaries are detected by extraction so a
    /// later full compaction treats them as prior summaries, not ordinary
    /// conversation.
    #[test]
    fn extract_prior_summary_detects_partial_compaction() {
        let partial_summary = wrap_full_summary(&format!(
            "[Partial compaction: the following is a summary of 5 messages that were \
             compacted to free context space. Earlier messages are preserved above.]\n\n{}",
            "partial summary body",
        ));
        let msgs = vec![
            Message::system("sys"),
            Message::user(partial_summary.clone()),
            Message::assistant(PARTIAL_COMPACTION_CONTINUATION),
            Message::assistant("more work"),
            Message::user("even more"),
        ];
        let (text, summary_idx, continuation_idx) =
            extract_prior_summary(&msgs).expect("partial summary detected");
        assert_eq!(summary_idx, 1);
        assert_eq!(continuation_idx, 2);
        // End marker is stripped; partial prefix is preserved.
        assert!(text.starts_with("[Partial compaction:"));
        assert!(text.contains("partial summary body"));
        assert!(!text.contains(COMPACTION_SUMMARY_END_MARKER));
    }

    ///52te: a partial-to-full transition does not treat the partial summary
    /// prefix/marker as ordinary conversation — extraction recognises and
    /// strips the boundary so the full compaction summariser sees clean text.
    #[test]
    fn partial_to_full_transition_strips_markers() {
        // Simulate: partial compaction was done, then new messages grew, and
        // now a full compaction is needed.
        let partial_summary = wrap_full_summary(&format!(
            "[Partial compaction: the following is a summary of 3 messages ...]\n\n{}",
            "partial body",
        ));
        let partial_rebuilt = vec![
            Message::system("sys"),
            Message::user("old kept work"),
            Message::user(partial_summary),
            Message::assistant(PARTIAL_COMPACTION_CONTINUATION),
            Message::assistant("new work"),
            Message::user("new question"),
        ];
        // Simulate full compaction extraction on partial_rebuilt.
        let (text, _, _) =
            extract_prior_summary(&partial_rebuilt).expect("partial detected as prior");
        assert_eq!(
            text,
            "[Partial compaction: the following is a summary of 3 messages ...]\n\npartial body"
        );
        assert!(!text.contains(COMPACTION_SUMMARY_END_MARKER));

        // Feed into previous_summary_block — should contain the partial
        // prefix but NOT the end marker.
        let block = previous_summary_block(&text);
        assert!(block.contains("<previous-summary>"));
        assert!(block.contains("[Partial compaction:"));
        assert!(block.contains("partial body"));
        assert!(!block.contains(COMPACTION_SUMMARY_END_MARKER));
    }

    ///52te: `previous_summary_block` delimits merged prior context with
    /// `<previous-summary>` tags so the summariser sees a bounded block,
    /// and the end marker is absent from the rendered block.
    #[test]
    fn previous_summary_block_delimits_and_strips_marker() {
        let prior_with_marker = format!("my summary{COMPACTION_SUMMARY_END_MARKER}");
        let prior_clean = strip_compaction_markers(&prior_with_marker);
        let block = previous_summary_block(&prior_clean);

        assert!(block.contains("<previous-summary>"));
        assert!(block.contains("</previous-summary>"));
        assert!(block.contains("my summary"));
        assert!(!block.contains(COMPACTION_SUMMARY_END_MARKER));
        assert!(block.to_lowercase().contains("merge"));
    }

    ///52te: `previous_summary_block` with nested XML content preserves the
    /// inner markup while the outer `<previous-summary>` tags remain intact.
    #[test]
    fn previous_summary_block_preserves_nested_xml() {
        let prior = "key decision: use <foo>bar</foo> for now";
        let block = previous_summary_block(prior);
        assert!(block.contains("<previous-summary>"));
        assert!(block.contains("</previous-summary>"));
        assert!(block.contains("<foo>bar</foo>"));
    }

    ///52te: `wrap_full_summary` and `strip_compaction_markers` are inverse
    /// operations — wrapping then stripping returns the original text.
    #[test]
    fn wrap_and_strip_are_inverse() {
        let texts = [
            "simple summary",
            "summary with\nnewlines",
            "summary with [brackets] and (parens)",
            "summary ending in newline\n",
        ];
        for t in texts {
            let wrapped = wrap_full_summary(t);
            assert!(wrapped.contains(COMPACTION_SUMMARY_END_MARKER));
            let stripped = strip_compaction_markers(&wrapped);
            // If the original ended in \n, the trailing \n is also stripped.
            let expected = t.strip_suffix('\n').unwrap_or(t);
            assert_eq!(stripped, expected, "roundtrip failed for: {t:?}");
        }
    }

    ///52te: full rebuild renders the end marker in the user message and
    /// the continuation marker in the assistant message.
    #[test]
    fn full_rebuild_renders_both_markers() {
        let original = vec![
            Message::system("sys"),
            Message::user("old"),
            Message::user("latest"),
        ];
        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "the summary".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );
        // The summary user message contains the end marker.
        assert!(
            rebuilt[1]
                .text_content()
                .contains(COMPACTION_SUMMARY_END_MARKER)
        );
        // The continuation is the full compaction constant.
        assert_eq!(rebuilt[2].text_content(), FULL_COMPACTION_CONTINUATION);
    }

    ///52te: partial rebuild renders the end marker and uses the extracted
    /// `PARTIAL_COMPACTION_CONTINUATION` constant.
    #[test]
    fn partial_rebuild_renders_end_marker_and_uses_continuation_const() {
        let prefix = vec![Message::system("sys"), Message::user("kept")];
        let rebuilt = rebuild_partial_compaction_messages(
            &prefix,
            3,
            &[],
            "partial summary".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
            &None,
        );
        // The partial summary message carries the end marker.
        let summary_msg = &rebuilt[2];
        assert!(
            summary_msg
                .text_content()
                .contains(COMPACTION_SUMMARY_END_MARKER)
        );
        assert!(
            summary_msg
                .text_content()
                .contains("[Partial compaction: the following is a summary of 3 messages")
        );
        assert!(summary_msg.text_content().contains("partial summary"));
        // The continuation uses the constant.
        assert_eq!(rebuilt[3].text_content(), PARTIAL_COMPACTION_CONTINUATION);
    }

    ///52te: full round-trip — rebuild, grow conversation, extract, and feed
    /// into `previous_summary_block`. Marker never leaks into the block.
    #[test]
    fn full_roundtrip_rebuild_extract_block_no_marker_leak() {
        let original = vec![
            Message::system("sys"),
            Message::user("old work"),
            Message::assistant("did stuff"),
            Message::user("latest"),
        ];
        let rebuilt = rebuild_full_compaction_messages(
            &original,
            "SUMMARY TEXT".to_string(),
            &CompactionContext::MidSession("worker".to_string()),
        );
        let mut grown = rebuilt;
        grown.push(Message::assistant("more"));
        grown.push(Message::user("even more"));

        let (text, _, _) = extract_prior_summary(&grown).expect("detected");
        assert_eq!(text, "SUMMARY TEXT");
        let block = previous_summary_block(&text);
        assert!(block.contains("SUMMARY TEXT"));
        assert!(!block.contains(COMPACTION_SUMMARY_END_MARKER));
        assert!(block.contains("<previous-summary>"));
    }
}
