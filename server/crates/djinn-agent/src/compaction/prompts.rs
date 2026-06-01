use djinn_provider::message::{Message, Role};

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
        CompactionContext::MidSession(role)
            if role == "reviewer" || role == "task_reviewer" =>
        {
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
        CompactionContext::MidSession(role)
            if role == "reviewer" || role == "task_reviewer" =>
        {
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

pub(crate) const MID_SESSION_WORKER_PROMPT: &str = r#"You are a coding agent continuing your own in-progress work session. Summarise the conversation below into a handoff so you can resume without re-reading files.

**Conversation:**
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

**Conversation:**
{messages}

{rules}

## Review Scope
## Files Reviewed
## Issues Found
## Positive Findings
## Assessment Progress
## Remaining Checks"#;

pub(crate) const GENERIC_PROMPT: &str = r#"You are an agent continuing a working session with a user. Summarise the conversation below into a handoff so the session can continue with no loss of context. Do not introduce goals or next steps the user did not confirm.

**Conversation:**
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
pub(crate) const SUMMARISER_SYSTEM_GENERIC: &str =
    "You summarise an agent–user working session. Produce a dense, faithful, terse summary. Follow the requested section format exactly, output only the summary, and never mention summarisation or compaction.";

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

    new_messages.push(Message::user(summary));

    let continuation_msg =
        "Your context was compacted. The previous message contains a summary of the \
         conversation so far. Continue calling tools as necessary to complete the task.";
    new_messages.push(Message::assistant(continuation_msg));

    if matches!(
        ctx,
        CompactionContext::MidSession(_) | CompactionContext::ChatSession
    ) && let Some(last_user) = last_user_text
    {
        let already_appended = new_messages
            .last()
            .map(|m| m.role == Role::User && m.text_content() == last_user)
            .unwrap_or(false);
        if !already_appended {
            new_messages.push(Message::user(last_user));
        }
    }

    new_messages
}

pub(super) fn rebuild_partial_compaction_messages(
    prefix: &[Message],
    tail_len: usize,
    summary: String,
    ctx: &CompactionContext,
    last_user_text: &Option<String>,
) -> Vec<Message> {
    let mut new_messages: Vec<Message> = prefix.to_vec();

    new_messages.push(Message::user(format!(
        "[Partial compaction: the following is a summary of {} messages that were \
         compacted to free context space. Earlier messages are preserved above.]\n\n{}",
        tail_len, summary,
    )));

    let continuation_msg =
        "Part of your context was compacted. The messages above the summary are \
         preserved verbatim; the summary covers your more recent work. Continue \
         calling tools as necessary to complete the task.";
    new_messages.push(Message::assistant(continuation_msg));

    if matches!(
        ctx,
        CompactionContext::MidSession(_) | CompactionContext::ChatSession
    ) && let Some(last_user) = last_user_text
    {
        let already_appended = new_messages
            .last()
            .map(|m| m.role == Role::User && m.text_content() == *last_user)
            .unwrap_or(false);
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
        assert_eq!(rebuilt[1].text_content(), "summary");
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
        assert_eq!(rebuilt[1].text_content(), "summary");
        assert_eq!(rebuilt.last().unwrap().text_content(), "latest user");
    }

    #[test]
    fn rebuild_partial_compaction_messages_reappends_last_user() {
        let prefix = vec![Message::system("sys"), Message::user("kept")];
        let rebuilt = rebuild_partial_compaction_messages(
            &prefix,
            3,
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
}
