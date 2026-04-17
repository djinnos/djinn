use crate::message::{Message, Role};

/// Describes *why* compaction is happening, so the prompt can be tailored.
#[derive(Debug, Clone)]
pub(crate) enum CompactionContext {
    /// Mid-session compaction: context window threshold reached while working.
    MidSession(String),
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

pub(super) const PARTIAL_COMPACTION_PROMPT: &str = r#"## Partial Compaction Context
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

pub(super) const PARTIAL_COMPACTION_SUMMARISER_SYSTEM: &str = "You are summarising the tail portion of a conversation. The beginning of the conversation is preserved separately and the reader will have it. Produce a dense, faithful summary of only the provided messages, connecting naturally to the earlier context the reader already has. Do not repeat early context.";

pub(crate) const SUMMARISER_SYSTEM_WORKER_MID_SESSION: &str = "You are summarising a coding agent's in-progress work session. Produce a dense, faithful summary that preserves all implementation context so the agent can continue working without re-reading files.";
pub(crate) const SUMMARISER_SYSTEM_TASK_REVIEWER: &str = "You are summarising a code review session. Produce a dense, faithful summary that preserves the review findings, issues identified, and assessment progress.";
pub(crate) const SUMMARISER_SYSTEM_GENERIC: &str =
    "You are a conversation summariser. Produce a dense, faithful summary.";

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

    new_messages
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::Conversation;

    #[test]
    fn compaction_prompt_varies_by_context() {
        let worker_mid = compaction_prompt(&CompactionContext::MidSession("worker".to_string()));
        let reviewer = compaction_prompt(&CompactionContext::MidSession("reviewer".to_string()));

        assert!(worker_mid.contains("context window is full"));
        assert!(reviewer.contains("code review"));
        assert!(worker_mid.contains("{messages}"));
        assert!(reviewer.contains("{messages}"));
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
