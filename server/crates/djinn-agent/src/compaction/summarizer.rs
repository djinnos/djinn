use futures::StreamExt;

use crate::message::{Conversation, Message, Role};
use crate::provider::{LlmProvider, StreamEvent};

use super::prompts::{
    CompactionContext, PARTIAL_COMPACTION_PROMPT, PARTIAL_COMPACTION_SUMMARISER_SYSTEM,
    compaction_prompt, summariser_system,
};

pub(super) async fn do_partial_compact(
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
                tracing::debug!(
                    pct,
                    "partial_compact: empty summary at removal pct, retrying"
                );
            }
            Err(e) => {
                if is_context_error_message(&e.to_string()) {
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

pub(super) async fn do_compact(
    provider: &dyn LlmProvider,
    messages: &[Message],
    ctx: &CompactionContext,
) -> anyhow::Result<String> {
    const REMOVAL_PERCENTAGES: &[u32] = &[0, 10, 20, 50, 100];

    let prompt_template = compaction_prompt(ctx);
    let system_instruction = summariser_system(ctx);

    for &pct in REMOVAL_PERCENTAGES {
        let filtered = filter_tool_responses_middle_out(messages, pct);
        let formatted = format_messages_as_text(&filtered);
        let prompt_text = prompt_template.replace("{messages}", &formatted);

        let mut compact_conv = Conversation::new();
        compact_conv.push(Message::system(system_instruction));
        compact_conv.push(Message::user(prompt_text));

        match call_llm_for_summary(provider, &compact_conv).await {
            Ok(summary) if !summary.is_empty() => return Ok(summary),
            Ok(_) => {
                tracing::debug!(pct, "compaction: empty summary at removal pct, retrying");
            }
            Err(e) => {
                if is_context_error_message(&e.to_string()) {
                    tracing::debug!(
                        pct,
                        error = %e,
                        "compaction: context length error at removal pct, retrying with more removal"
                    );
                    continue;
                }
                return Err(e);
            }
        }
    }

    Err(anyhow::anyhow!(
        "compaction: failed to summarise even with 100% tool-response removal"
    ))
}

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

pub(super) fn filter_tool_responses_middle_out(
    messages: &[Message],
    remove_percent: u32,
) -> Vec<Message> {
    if remove_percent == 0 {
        return messages.to_vec();
    }

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

pub(super) fn format_messages_as_text(messages: &[Message]) -> String {
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
                ContentBlock::Image { .. } => format!("[{role}]: [image]"),
                ContentBlock::Document { filename, .. } => {
                    format!(
                        "[{role}]: [document: {}]",
                        filename.as_deref().unwrap_or("file")
                    )
                }
                ContentBlock::Thinking { .. } => continue,
            };
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

pub(super) fn is_context_error_message(message: &str) -> bool {
    let msg = message.to_lowercase();
    msg.contains("context_length")
        || msg.contains("context limit")
        || msg.contains("too many tokens")
        || msg.contains("maximum context")
        || msg.contains("context window")
        || msg.contains("prompt is too long")
        || msg.contains("max_tokens")
        || msg.contains("token limit")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::ContentBlock;

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
    fn is_context_error_message_detects_variants() {
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
            assert!(is_context_error_message(msg), "should detect: {msg}");
        }
        assert!(!is_context_error_message("rate limited"));
    }
}
