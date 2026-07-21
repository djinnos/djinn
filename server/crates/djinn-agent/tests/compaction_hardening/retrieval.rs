use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use djinn_agent::output_stash::{DurableOutputDetails, OutputStash, handle_stash_tool};
use djinn_agent::test_helpers::{persist_tool_results_before_compaction_for_test, test_persistent_dir};
use djinn_provider::message::{ContentBlock, Conversation, Message, Role};
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};
use djinn_slot::reply_loop::turn::compact_conversation_after_persist_for_test;
use serde_json::Value;

struct SummaryProvider;
impl LlmProvider for SummaryProvider {
    fn name(&self) -> &str { "compaction-hardening-summary" }
    fn stream<'a>(&'a self, _: &'a Conversation, _: &'a [Value], _: Option<ToolChoice>) -> Pin<Box<dyn futures::Future<Output = anyhow::Result<Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>>> + Send + 'a>> {
        Box::pin(async { Ok(Box::pin(futures::stream::iter([Ok(StreamEvent::Delta(ContentBlock::text("fixture summary"))), Ok(StreamEvent::Done)])) as Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>) })
    }
}

/// Fails the first (partial) summarization request so policy takes full mode.
#[derive(Default)]
struct FullProvider(AtomicUsize);
impl LlmProvider for FullProvider {
    fn name(&self) -> &str {
        "compaction-hardening-full"
    }
    fn stream<'a>(
        &'a self,
        _: &'a Conversation,
        _: &'a [Value],
        _: Option<ToolChoice>,
    ) -> Pin<
        Box<
            dyn futures::Future<
                    Output = anyhow::Result<
                        Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let call = self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if call == 0 {
                Err(anyhow::anyhow!("partial fixture failure"))
            } else {
                Ok(Box::pin(futures::stream::iter([
                    Ok(StreamEvent::Delta(ContentBlock::text("fixture summary"))),
                    Ok(StreamEvent::Done),
                ])) as Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>)
            }
        })
    }
}
struct OverflowProvider;
impl LlmProvider for OverflowProvider {
    fn name(&self) -> &str { "compaction-hardening-overflow" }
    fn stream<'a>(&'a self, _: &'a Conversation, _: &'a [Value], _: Option<ToolChoice>) -> Pin<Box<dyn futures::Future<Output = anyhow::Result<Pin<Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>>>> + Send + 'a>> {
        Box::pin(async { Err(anyhow::anyhow!("context limit exceeded")) })
    }
}

fn tool_conversation(turns: usize, prefix: &str) -> Conversation {
    let mut messages = vec![Message::system("system"), Message::user("start")];
    for turn in 0..turns {
        let id = format!("{prefix}-{turn}");
        messages.push(Message { role: Role::Assistant, content: vec![ContentBlock::ToolUse { id: id.clone(), name: "shell".into(), input: serde_json::json!({}) }], metadata: None });
        messages.push(Message { role: Role::User, content: vec![ContentBlock::ToolResult { tool_use_id: id, content: vec![ContentBlock::text(format!("{prefix} durable bytes {turn}: {}", "x".repeat(300)))], is_error: false }], metadata: None });
        messages.push(Message::assistant(format!("resolved {turn}")));
    }
    Conversation { messages }
}

async fn compact(provider: &dyn LlmProvider, conversation: &mut Conversation, stash: &mut OutputStash, window: i64) -> Result<bool, String> {
    compact_conversation_after_persist_for_test(provider, conversation, window, |results| persist_tool_results_before_compaction_for_test(stash, results)).await
}
fn view(stash: &Mutex<OutputStash>, id: &str) -> Result<String, String> {
    let args = serde_json::json!({"tool_use_id": id}).as_object().unwrap().clone();
    handle_stash_tool(stash, "output_view", Some(&args))
}
fn placeholder_count(conversation: &Conversation) -> usize {
    conversation.messages.iter().flat_map(|m| &m.content).filter_map(|block| match block { ContentBlock::ToolResult { content, .. } => content.first().and_then(|item| item.as_text()), _ => None }).filter(|text| text.starts_with("[Cleared") && text.contains("output_view")).count()
}

#[tokio::test]
async fn preclear_stash_is_atomic() {
    let root = test_persistent_dir("compaction-hardening-atomic-");
    let mut stash = OutputStash::with_session_id_and_durable_root("owner", root);
    stash.insert_with_metadata("atomic-2".into(), "shell".into(), "truncated bytes".into(), DurableOutputDetails { turn: 77, result_kind: "shell_stdout".into(), original_chars: 900, stored_chars: 15, completeness: "truncated".into() }).unwrap();
    stash.insert_with_metadata("atomic-3".into(), "shell".into(), "spill bytes".into(), DurableOutputDetails { turn: 78, result_kind: "shell_stdout".into(), original_chars: 901, stored_chars: 11, completeness: "partial-spill".into() }).unwrap();
    let provider = SummaryProvider;
    let mut conversation = tool_conversation(12, "atomic");
    let before = conversation.clone();
    assert!(compact(&provider, &mut conversation, &mut stash, 1_000_000).await.unwrap());
    assert_ne!(conversation, before, "production compaction must replace inline results");
    assert!(placeholder_count(&conversation) > 0, "micro replacement must create durable pointers");
    let records = stash.list_durable_outputs().unwrap();
    assert_eq!(records.iter().find(|r| r.tool_use_id == "atomic-2").unwrap().completeness, "truncated");
    assert_eq!(records.iter().find(|r| r.tool_use_id == "atomic-3").unwrap().completeness, "partial-spill");
    assert_eq!(records.iter().find(|r| r.tool_use_id == "atomic-2").unwrap().turn, 77);
    let mut overflow = tool_conversation(4, "overflow");
    assert!(compact(&OverflowProvider, &mut overflow, &mut stash, 1_000_000).await.is_ok());
    assert!(placeholder_count(&overflow) > 0, "overflow fallback must aggressively microcompact");
    let mut failed = tool_conversation(12, "failure");
    let untouched = failed.clone();
    stash.set_fail_durable_writes_for_test(true);
    let error = compact(&provider, &mut failed, &mut stash, 1_000_000).await.unwrap_err();
    assert!(error.contains("injected durable output write failure"));
    assert_eq!(failed, untouched, "failed durable write must abort before transcript replacement");
}

#[tokio::test]
async fn survives_modes_reload_and_enforces_session() {
    let root = test_persistent_dir("compaction-hardening-reload-");
    let mut original = OutputStash::with_session_id_and_durable_root("trusted-owner", root.clone());
    let summary = SummaryProvider;
    let full = FullProvider::default();
    let modes: [(&str, &dyn LlmProvider, usize, i64); 4] = [
        ("micro", &summary, 12, 1_000_000),
        ("partial", &summary, 12, 100),
        ("full", &full, 2, 100),
        ("fallback", &OverflowProvider, 4, 1_000_000),
    ];
    for (mode, provider, turns, window) in modes {
        let mut conversation = tool_conversation(turns, mode);
        let before = conversation.clone();
        let _ = compact(provider, &mut conversation, &mut original, window)
            .await
            .unwrap();
        assert_ne!(conversation, before, "{mode} fixture must run production replacement");
        let transcript = conversation
            .messages
            .iter()
            .map(Message::text_content)
            .collect::<Vec<_>>()
            .join("\n");
        match mode {
            "micro" => assert!(placeholder_count(&conversation) > 0),
            "partial" => assert!(transcript.contains("[Partial compaction:")),
            "full" => {
                assert!(transcript.contains("fixture summary"));
                assert!(!transcript.contains("[Partial compaction:"));
            }
            "fallback" => assert!(placeholder_count(&conversation) > 0),
            _ => unreachable!(),
        },
    }
    drop(original);
    let owner = Mutex::new(OutputStash::with_session_id_and_durable_root("trusted-owner", root.clone()));
    let listed: Vec<Value> = serde_json::from_str(&handle_stash_tool(&owner, "output_list", None).unwrap()).unwrap();
    assert!(!listed.is_empty());
    for record in &listed {
        let id = record["tool_use_id"].as_str().unwrap();
        assert!(record["turn"].is_u64()); assert_eq!(record["result_kind"], "tool_result"); assert!(record["original_chars"].is_u64()); assert!(record["stored_chars"].is_u64()); assert_eq!(record["completeness"], "complete");
        assert!(!view(&owner, id).unwrap().is_empty(), "{id} must remain viewable after reopen");
    }
    let foreign = Mutex::new(OutputStash::with_session_id_and_durable_root("other-trusted-session", root));
    assert_eq!(handle_stash_tool(&foreign, "output_list", None).unwrap(), "[]");
    for record in &listed { assert!(view(&foreign, record["tool_use_id"].as_str().unwrap()).is_err()); }
    assert!(view(&owner, listed[0]["tool_use_id"].as_str().unwrap()).is_ok());
}
