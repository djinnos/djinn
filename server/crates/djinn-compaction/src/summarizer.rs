use futures::StreamExt;

use djinn_provider::message::{Conversation, Message, Role};
use djinn_provider::provider::{LlmProvider, ReasoningEffort, StreamEvent};

use super::prompts::{
    CompactionContext, PARTIAL_COMPACTION_PROMPT, PARTIAL_COMPACTION_SUMMARISER_SYSTEM,
    TEMPLATE_RULES, compaction_prompt, summariser_system,
};

pub(super) async fn do_partial_compact(
    provider: &dyn LlmProvider,
    tail_messages: &[Message],
) -> anyhow::Result<String> {
    const REMOVAL_PERCENTAGES: &[u32] = &[0, 10, 20, 50, 100];

    for &pct in REMOVAL_PERCENTAGES {
        let filtered = filter_tool_responses_middle_out(tail_messages, pct);
        let formatted = format_messages_as_text(&filtered);
        let prompt_text = PARTIAL_COMPACTION_PROMPT
            .replace("{messages}", &formatted)
            .replace("{rules}", TEMPLATE_RULES);

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

    // C-4: pull a prior summary out of the input and feed it back as a
    // <previous-summary> block to update/merge/prune, rather than letting it be
    // re-summarised verbatim (summary-of-summary drift). Exclude the prior
    // summary + its continuation marker from the messages being summarised. The
    // pair is plain text, so removing it cannot orphan a tool result.
    let (previous_summary, messages): (String, Vec<Message>) =
        match super::prompts::extract_prior_summary(messages) {
            Some((prior, summary_idx, continuation_idx)) => {
                let kept = messages
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != summary_idx && *i != continuation_idx)
                    .map(|(_, m)| m.clone())
                    .collect();
                (super::prompts::previous_summary_block(&prior), kept)
            }
            None => (String::new(), messages.to_vec()),
        };

    for &pct in REMOVAL_PERCENTAGES {
        let filtered = filter_tool_responses_middle_out(&messages, pct);
        let formatted = format_messages_as_text(&filtered);
        let prompt_text = prompt_template
            .replace("{previous_summary}", &previous_summary)
            .replace("{messages}", &formatted)
            .replace("{rules}", TEMPLATE_RULES);

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
    // B5a: compaction summarization is a cheap background call (condense the
    // conversation tail into a summary), not the agent's main reasoning loop.
    // Force the weakest reasoning tier so it doesn't waste deep-thinking
    // tokens/latency. `with_reasoning_effort` returns `None` for config-less
    // providers (e.g. test mocks), in which case we stream through the original
    // provider unchanged.
    let weak = provider.with_reasoning_effort(ReasoningEffort::Minimal);
    let summary_provider: &dyn LlmProvider = match weak.as_deref() {
        Some(p) => p,
        None => provider,
    };

    let mut stream = summary_provider.stream(conv, &[], None).await?;
    let mut summary = String::new();
    let mut saw_done = false;

    while let Some(evt) = stream.next().await {
        match evt? {
            StreamEvent::Delta(block) => {
                if let Some(text) = block.as_text() {
                    summary.push_str(text);
                }
            }
            StreamEvent::Done => {
                saw_done = true;
                break;
            }
            StreamEvent::Usage(_)
            | StreamEvent::Thinking(_)
            | StreamEvent::ThinkingDelta { .. }
            | StreamEvent::ThinkingBlockComplete { .. } => {}
        }
    }

    if !saw_done {
        // The provider stream ended without ever emitting `StreamEvent::Done`.
        // The accumulated `summary` may be a partial, truncated dump of the
        // tail — surface that as an error so callers (`do_compact`,
        // `do_partial_compact`, `do_compact_with_overflow_retry`) treat the
        // summary as failed and route through their existing retry / fallback
        // paths instead of silently accepting an incomplete compaction.
        return Err(anyhow::anyhow!(
            "incomplete compaction summary stream: provider stream ended before Done \
             (stream exhausted after {} accumulated chars)",
            summary.len()
        ));
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
                    .all(|b| matches!(b, djinn_provider::message::ContentBlock::ToolResult { .. }))
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
    use djinn_provider::message::ContentBlock;

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
                ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::Unknown { .. }
                | ContentBlock::OpenAIReasoning { .. } => continue,
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
    use djinn_provider::message::ContentBlock;
    use djinn_provider::provider::ToolChoice;
    use serde_json::Value;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Fake provider that records whether `with_reasoning_effort` was requested
    /// (and with which tier) and whether `stream` ran on the DOWNGRADED instance.
    struct RecordingProvider {
        downgraded: bool,
        requested_effort: Arc<std::sync::Mutex<Option<ReasoningEffort>>>,
        streamed_on_downgraded: Arc<AtomicBool>,
    }

    impl LlmProvider for RecordingProvider {
        fn name(&self) -> &str {
            "recording"
        }

        fn with_reasoning_effort(&self, effort: ReasoningEffort) -> Option<Box<dyn LlmProvider>> {
            *self.requested_effort.lock().unwrap() = Some(effort);
            Some(Box::new(RecordingProvider {
                downgraded: true,
                requested_effort: self.requested_effort.clone(),
                streamed_on_downgraded: self.streamed_on_downgraded.clone(),
            }))
        }

        fn stream<'a>(
            &'a self,
            _conversation: &'a Conversation,
            _tools: &'a [Value],
            _tool_choice: Option<ToolChoice>,
        ) -> Pin<
            Box<
                dyn futures::Future<
                        Output = anyhow::Result<
                            Pin<
                                Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                            >,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            if self.downgraded {
                self.streamed_on_downgraded.store(true, Ordering::SeqCst);
            }
            Box::pin(async move {
                let events = vec![
                    Ok(StreamEvent::Delta(ContentBlock::text("summary text"))),
                    Ok(StreamEvent::Done),
                ];
                let stream: Pin<Box<dyn futures::Stream<Item = _> + Send>> =
                    Box::pin(futures::stream::iter(events));
                Ok(stream)
            })
        }
    }

    #[tokio::test]
    async fn compaction_summary_runs_at_weakest_reasoning_tier() {
        let requested_effort = Arc::new(std::sync::Mutex::new(None));
        let streamed_on_downgraded = Arc::new(AtomicBool::new(false));
        let provider = RecordingProvider {
            downgraded: false,
            requested_effort: requested_effort.clone(),
            streamed_on_downgraded: streamed_on_downgraded.clone(),
        };

        let mut conv = Conversation::new();
        conv.push(Message::system("summarise"));
        conv.push(Message::user("a long tail of messages"));

        let summary = call_llm_for_summary(&provider, &conv)
            .await
            .expect("summary");
        assert_eq!(summary, "summary text");

        // The cheap compaction call must request the weakest tier …
        assert_eq!(
            *requested_effort.lock().unwrap(),
            Some(ReasoningEffort::Minimal),
            "compaction summary must run at the weakest reasoning tier"
        );
        // … and actually stream through the downgraded provider.
        assert!(
            streamed_on_downgraded.load(Ordering::SeqCst),
            "compaction summary must stream through the downgraded (Minimal) provider"
        );
    }

    /// Fake provider whose `stream` returns a caller-supplied sequence of events —
    /// used to exercise `call_llm_for_summary` completion semantics (Done vs.
    /// premature stream exhaustion) without needing a real network round-trip.
    struct ScriptedEventProvider {
        events: Vec<StreamEvent>,
    }

    impl LlmProvider for ScriptedEventProvider {
        fn name(&self) -> &str {
            "scripted"
        }

        fn with_reasoning_effort(&self, _effort: ReasoningEffort) -> Option<Box<dyn LlmProvider>> {
            Some(Box::new(ScriptedEventProvider {
                events: self.events.clone(),
            }))
        }

        fn stream<'a>(
            &'a self,
            _conversation: &'a Conversation,
            _tools: &'a [Value],
            _tool_choice: Option<ToolChoice>,
        ) -> Pin<
            Box<
                dyn futures::Future<
                        Output = anyhow::Result<
                            Pin<
                                Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                            >,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                let events: Vec<anyhow::Result<StreamEvent>> =
                    self.events.iter().cloned().map(Ok).collect();
                let stream: Pin<Box<dyn futures::Stream<Item = _> + Send>> =
                    Box::pin(futures::stream::iter(events));
                Ok(stream)
            })
        }
    }

    /// Normal streams that emit deltas followed by `StreamEvent::Done` must
    /// return the accumulated summary text unchanged.
    #[tokio::test]
    async fn call_llm_for_summary_returns_text_on_done() {
        let provider = ScriptedEventProvider {
            events: vec![
                StreamEvent::Delta(ContentBlock::text("hello ")),
                StreamEvent::Delta(ContentBlock::text("world")),
                StreamEvent::Done,
            ],
        };

        let mut conv = Conversation::new();
        conv.push(Message::user("tail"));

        let summary = call_llm_for_summary(&provider, &conv)
            .await
            .expect("stream terminated with Done must return the summary");
        assert_eq!(summary, "hello world");
    }

    /// A stream that emits deltas but never yields `StreamEvent::Done` is an
    /// incomplete compaction summary stream. `call_llm_for_summary` must
    /// surface that as an error rather than silently accepting whatever deltas
    /// happened to arrive — so the caller routes through its existing retry
    /// / fallback path instead of treating a partial summary as authoritative.
    #[tokio::test]
    async fn call_llm_for_summary_errors_when_stream_ends_without_done() {
        let provider = ScriptedEventProvider {
            events: vec![
                StreamEvent::Delta(ContentBlock::text("partial ")),
                StreamEvent::Delta(ContentBlock::text("dump")),
                // stream ends here — no `StreamEvent::Done`
            ],
        };

        let mut conv = Conversation::new();
        conv.push(Message::user("tail"));

        let err = call_llm_for_summary(&provider, &conv)
            .await
            .expect_err("stream that ends without Done must not be accepted as a summary");

        let msg = err.to_string();
        assert!(
            msg.contains("incomplete compaction summary stream")
                || msg.contains("stream ended before Done"),
            "error must clearly identify incomplete compaction summary stream exhaustion, got: {msg}"
        );
    }

    /// A stream that emits neither deltas nor `Done` (immediately exhausted)
    /// must still be flagged as incomplete, so an empty-then-silent provider
    /// cannot be misinterpreted as "no content to summarise".
    #[tokio::test]
    async fn call_llm_for_summary_errors_on_empty_stream_without_done() {
        let provider = ScriptedEventProvider { events: vec![] };

        let mut conv = Conversation::new();
        conv.push(Message::user("tail"));

        let err = call_llm_for_summary(&provider, &conv)
            .await
            .expect_err("an empty stream without Done must be treated as incomplete");

        assert!(
            err.to_string()
                .contains("incomplete compaction summary stream"),
            "expected incomplete-stream error, got: {err}"
        );
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
