//! Djinn-native conversation compaction.
//!
//! When the accumulated input token count reaches 80% of the model's context
//! window, `compact_conversation` summarises the conversation via the LLM and
//! replaces the in-memory `Conversation` with a compact representation. The
//! original messages are persisted to the `session_messages` table before the
//! replacement so nothing is lost.

use futures::StreamExt;

use crate::agent::message::{Conversation, Message, Role};
use crate::agent::provider::{LlmProvider, StreamEvent};
use crate::agent::AgentType;
use crate::db::repositories::session_message::SessionMessageRepository;
use crate::server::AppState;

// ─── Threshold ────────────────────────────────────────────────────────────────

/// Fraction of the context window at which compaction is triggered.
pub const COMPACTION_THRESHOLD: f64 = 0.8;

// ─── Compaction context ──────────────────────────────────────────────────────

/// Describes *why* compaction is happening, so the prompt can be tailored.
#[derive(Debug, Clone, Copy)]
pub enum CompactionContext {
    /// Mid-session compaction: context window threshold reached while working.
    MidSession(AgentType),
    /// Pre-resume compaction: compacting before re-prompting with reviewer feedback.
    PreResume(AgentType),
}

// ─── Prompts ─────────────────────────────────────────────────────────────────

/// Build the compaction prompt based on context.
fn compaction_prompt(ctx: CompactionContext) -> &'static str {
    match ctx {
        CompactionContext::PreResume(AgentType::Worker) => PRE_RESUME_WORKER_PROMPT,
        CompactionContext::MidSession(AgentType::Worker) => MID_SESSION_WORKER_PROMPT,
        CompactionContext::MidSession(AgentType::TaskReviewer) | CompactionContext::PreResume(AgentType::TaskReviewer) => REVIEWER_PROMPT,
        CompactionContext::MidSession(AgentType::ConflictResolver) | CompactionContext::PreResume(AgentType::ConflictResolver) => CONFLICT_RESOLVER_PROMPT,
        _ => GENERIC_PROMPT,
    }
}

/// Build the system instruction for the summariser based on context.
fn summariser_system(ctx: CompactionContext) -> &'static str {
    match ctx {
        CompactionContext::PreResume(AgentType::Worker) => {
            "You are summarising a coding agent's work session that is about to receive reviewer feedback. \
             Produce a dense, faithful summary focused on what was implemented, what files were changed, \
             and what the current state of the code is. Do NOT include any statements about work being \
             complete or done — the reviewer has determined it is not."
        }
        CompactionContext::MidSession(AgentType::Worker) => {
            "You are summarising a coding agent's in-progress work session. \
             Produce a dense, faithful summary that preserves all implementation context \
             so the agent can continue working without re-reading files."
        }
        CompactionContext::MidSession(AgentType::TaskReviewer) | CompactionContext::PreResume(AgentType::TaskReviewer) => {
            "You are summarising a code review session. Produce a dense, faithful summary \
             that preserves the review findings, issues identified, and assessment progress."
        }
        _ => "You are a conversation summariser. Produce a dense, faithful summary.",
    }
}

const PRE_RESUME_WORKER_PROMPT: &str = r#"## Compaction Context
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

const MID_SESSION_WORKER_PROMPT: &str = r#"## Compaction Context
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

const REVIEWER_PROMPT: &str = r#"## Compaction Context
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

const CONFLICT_RESOLVER_PROMPT: &str = r#"## Compaction Context
A merge conflict resolution agent's session needs compaction.

**Conversation History:**
{messages}

Wrap reasoning in `<analysis>` tags.

### Include These Sections:
1. **Conflict Context** – What branches are being merged and why conflicts arose
2. **Files With Conflicts** – Every conflicted file and the nature of the conflict
3. **Resolution Decisions** – How each conflict was resolved and why
4. **Remaining Conflicts** – Any unresolved conflicts
5. **Build/Test Status** – Whether the resolution compiles and passes tests

> Preserve exact file paths, branch names, and conflict markers"#;

const GENERIC_PROMPT: &str = r#"## Task Context
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

// ─── Public helpers ───────────────────────────────────────────────────────────

/// Return `true` if the accumulated input tokens have reached the compaction
/// threshold relative to the model's context window.
pub fn needs_compaction(total_tokens_in: u32, context_window: i64) -> bool {
    if context_window <= 0 {
        return false;
    }
    total_tokens_in as f64 / context_window as f64 >= COMPACTION_THRESHOLD
}

// ─── Main compaction entry point ──────────────────────────────────────────────

/// Compact `conversation` in-place using LLM summarisation.
///
/// Steps:
/// 1. Persist all current messages to `session_messages`.
/// 2. Call `do_compact` to obtain a summary string.
/// 3. Replace `conversation` with `[system, user(summary), assistant(continuation), last_user]`.
///
/// Returns `true` if compaction was performed, `false` if `do_compact` failed.
pub async fn compact_conversation(
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    session_id: &str,
    task_id: &str,
    app_state: &AppState,
    ctx: CompactionContext,
) -> bool {
    // 1. Persist current messages before replacing them.
    let repo = SessionMessageRepository::new(app_state.db().clone(), app_state.events().clone());
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

    // 2. Extract system prompt and last plain-text user message before modifying.
    let system_msg: Option<Message> = conversation
        .messages
        .iter()
        .find(|m| m.role == Role::System)
        .cloned();

    let last_user_text: Option<String> = conversation
        .messages
        .iter()
        .rev()
        .find(|m| m.role == Role::User && m.content.iter().any(|b| b.as_text().is_some()))
        .and_then(|m| {
            let t = m.text_content();
            if t.is_empty() { None } else { Some(t) }
        });

    // 3. Ask the LLM to summarise.
    match do_compact(provider, &conversation.messages, ctx).await {
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
                "compaction: do_compact failed, leaving conversation unchanged"
            );
            false
        }
    }
}

// ─── Internal helpers ─────────────────────────────────────────────────────────

/// Attempt to summarise `messages` using the LLM, with progressive tool-response
/// removal to stay within the model's own context limit.
async fn do_compact(
    provider: &dyn LlmProvider,
    messages: &[Message],
    ctx: CompactionContext,
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
    let mut stream = provider.stream(conv, &[]).await?;
    let mut summary = String::new();

    while let Some(evt) = stream.next().await {
        match evt? {
            StreamEvent::Delta(block) => {
                if let Some(text) = block.as_text() {
                    summary.push_str(text);
                }
            }
            StreamEvent::Done => break,
            StreamEvent::Usage(_) => {}
        }
    }

    Ok(summary)
}

/// Remove a percentage of `ToolResult` messages from the middle outward
/// ("middle-out" strategy mirrors Goose's approach).
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
                    .all(|b| matches!(b, crate::agent::message::ContentBlock::ToolResult { .. }))
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
    use crate::agent::message::ContentBlock;

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
            };
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::message::{ContentBlock, Message, Role};

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
    fn compaction_prompt_varies_by_context() {
        let worker_resume = compaction_prompt(CompactionContext::PreResume(AgentType::Worker));
        let worker_mid = compaction_prompt(CompactionContext::MidSession(AgentType::Worker));
        let reviewer = compaction_prompt(CompactionContext::MidSession(AgentType::TaskReviewer));

        // Each context gets a different prompt
        assert!(worker_resume.contains("rejected or needs fixes"));
        assert!(worker_mid.contains("context window is full"));
        assert!(reviewer.contains("code review"));

        // All contain the messages placeholder
        assert!(worker_resume.contains("{messages}"));
        assert!(worker_mid.contains("{messages}"));
        assert!(reviewer.contains("{messages}"));
    }
}
