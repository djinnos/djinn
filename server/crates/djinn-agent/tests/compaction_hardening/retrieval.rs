use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use djinn_agent::output_stash::{DurableOutputDetails, OutputStash, handle_stash_tool};
use djinn_agent::test_helpers::{
    persist_tool_results_before_compaction_for_test, test_persistent_dir,
};
use djinn_provider::message::{ContentBlock, Conversation, Message, Role};
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};
use djinn_slot::reply_loop::turn::compact_conversation_after_persist_for_test;
use serde_json::Value;

struct SummaryProvider;
impl LlmProvider for SummaryProvider {
    fn name(&self) -> &str {
        "compaction-hardening-summary"
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
        Box::pin(async {
            Ok(Box::pin(futures::stream::iter([
                Ok(StreamEvent::Delta(ContentBlock::text("fixture summary"))),
                Ok(StreamEvent::Done),
            ]))
                as Pin<
                    Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                >)
        })
    }
}

/// Fails the partial attempt, forcing the policy's full-compaction retry.
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
                ]))
                    as Pin<
                        Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                    >)
            }
        })
    }
}

/// Makes the summarizer overflow so policy must take its aggressive micro path.
struct OverflowProvider;
impl LlmProvider for OverflowProvider {
    fn name(&self) -> &str {
        "compaction-hardening-overflow"
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
        Box::pin(async { Err(anyhow::anyhow!("context limit exceeded")) })
    }
}

fn output_bytes(prefix: &str, turn: usize) -> String {
    format!("{prefix} durable bytes {turn}: {}", "x".repeat(300))
}

fn tool_conversation(turns: usize, prefix: &str) -> Conversation {
    let mut messages = vec![Message::system("system"), Message::user("start")];
    for turn in 0..turns {
        let id = format!("{prefix}-{turn}");
        messages.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.clone(),
                name: "shell".into(),
                input: serde_json::json!({}),
            }],
            metadata: None,
        });
        messages.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id,
                content: vec![ContentBlock::text(output_bytes(prefix, turn))],
                is_error: false,
            }],
            metadata: None,
        });
        messages.push(Message::assistant(format!("resolved {turn}")));
    }
    Conversation { messages }
}

async fn compact(
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    stash: &mut OutputStash,
    window: i64,
) -> Result<bool, String> {
    // This invokes the same private helper used by the reply loop's production
    // critical section, rather than duplicating its write-before-replacement order.
    compact_conversation_after_persist_for_test(provider, conversation, window, |results| {
        persist_tool_results_before_compaction_for_test(stash, results)
    })
    .await
}

fn view(stash: &Mutex<OutputStash>, id: &str) -> Result<String, String> {
    let args = serde_json::json!({"tool_use_id": id})
        .as_object()
        .unwrap()
        .clone();
    handle_stash_tool(stash, "output_view", Some(&args))
}

fn placeholder_count(conversation: &Conversation) -> usize {
    conversation
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|block| match block {
            ContentBlock::ToolResult { content, .. } => {
                content.first().and_then(|item| item.as_text())
            }
            _ => None,
        })
        .filter(|text| text.starts_with("[Cleared") && text.contains("output_view"))
        .count()
}

fn tool_result_text<'a>(conversation: &'a Conversation, tool_use_id: &str) -> &'a str {
    conversation
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id: id,
                content,
                ..
            } if id == tool_use_id => content.first().and_then(ContentBlock::as_text),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing tool result {tool_use_id}"))
}

#[derive(Debug, PartialEq, Eq)]
struct ExpectedOutput {
    turn: u64,
    bytes: String,
}

fn expected_outputs(prefix: &str, turns: usize) -> BTreeMap<String, ExpectedOutput> {
    (0..turns)
        .map(|turn| {
            (
                format!("{prefix}-{turn}"),
                ExpectedOutput {
                    // Each fixture turn has a tool-use assistant message and a
                    // resolving assistant message; dispatcher turn numbering counts both.
                    turn: (2 * (turns - turn) - 1) as u64,
                    bytes: output_bytes(prefix, turn),
                },
            )
        })
        .collect()
}

fn conversation_bytes(conversation: &Conversation) -> Vec<u8> {
    serde_json::to_vec(&conversation.messages).unwrap()
}

#[tokio::test]
async fn preclear_stash_is_atomic() {
    let root = test_persistent_dir("compaction-hardening-atomic-");
    let mut stash = OutputStash::with_session_id_and_durable_root("owner", root);

    // This already-stashed large complete result has a deliberately different
    // inline value, proving the pre-clear walk reuses the durable blob.
    let large = format!("large durable blob:{}", "L".repeat(40_000));
    stash
        .insert_with_metadata(
            "atomic-1".into(),
            "shell".into(),
            large.clone(),
            DurableOutputDetails {
                turn: 76,
                result_kind: "shell_stdout".into(),
                original_chars: large.chars().count(),
                stored_chars: large.chars().count(),
                completeness: "complete".into(),
            },
        )
        .unwrap();
    let truncated = "truncated bytes".to_owned();
    stash
        .insert_with_metadata(
            "atomic-2".into(),
            "shell".into(),
            truncated.clone(),
            DurableOutputDetails {
                turn: 77,
                result_kind: "shell_stdout".into(),
                original_chars: 900,
                stored_chars: truncated.chars().count(),
                completeness: "truncated".into(),
            },
        )
        .unwrap();
    let partial_spill = "spill bytes".to_owned();
    stash
        .insert_with_metadata(
            "atomic-3".into(),
            "shell".into(),
            partial_spill.clone(),
            DurableOutputDetails {
                turn: 78,
                result_kind: "shell_stdout".into(),
                original_chars: 901,
                stored_chars: partial_spill.chars().count(),
                completeness: "partial-spill".into(),
            },
        )
        .unwrap();

    let provider = SummaryProvider;
    let mut conversation = tool_conversation(12, "atomic");
    let before = conversation_bytes(&conversation);
    assert!(
        compact(&provider, &mut conversation, &mut stash, 1_000_000)
            .await
            .unwrap()
    );
    assert_ne!(
        conversation_bytes(&conversation),
        before,
        "micro compaction must replace inline results only after persistence"
    );
    assert!(
        placeholder_count(&conversation) > 0,
        "micro replacement must create durable pointers"
    );
    assert_eq!(
        tool_result_text(&conversation, "atomic-1"),
        output_bytes("atomic", 1),
        "the ordinary age-six micro pass must leave this recent result inline"
    );

    let records = stash.list_durable_outputs().unwrap();
    for (id, turn, completeness, bytes) in [
        ("atomic-1", 76, "complete", &large),
        ("atomic-2", 77, "truncated", &truncated),
        ("atomic-3", 78, "partial-spill", &partial_spill),
    ] {
        let record = records
            .iter()
            .find(|record| record.tool_use_id == id)
            .unwrap();
        assert_eq!(
            (
                record.turn,
                record.completeness.as_str(),
                record.stored_chars
            ),
            (turn, completeness, bytes.chars().count())
        );
        assert!(
            stash
                .view(id, 0, bytes.chars().count() + 100)
                .unwrap()
                .contains(bytes),
            "{id} durable bytes must be reused unchanged"
        );
    }

    // A second production orchestration pass must reuse, not overwrite, existing ids.
    let reused = stash
        .view("atomic-1", 0, large.chars().count() + 100)
        .unwrap();
    let _ = compact(&provider, &mut conversation, &mut stash, 1_000_000)
        .await
        .unwrap();
    assert_eq!(
        stash
            .view("atomic-1", 0, large.chars().count() + 100)
            .unwrap(),
        reused
    );

    // A non-positive budget prevents the ordinary micro pass from returning early
    // and prevents fallback truncation from hiding the aggressive retry's result.
    let mut overflow = tool_conversation(12, "overflow");
    assert!(
        compact(&OverflowProvider, &mut overflow, &mut stash, 0)
            .await
            .is_ok()
    );
    let aggressive_only = tool_result_text(&overflow, "overflow-1");
    assert!(
        aggressive_only.starts_with("[Cleared")
            && aggressive_only.contains("output_view")
            && aggressive_only.contains("overflow-1"),
        "the age-three overflow-1 result, left inline by the ordinary age-six pass, \
         must be replaced with its durable pointer by the aggressive age-two retry: \
         {aggressive_only}"
    );
    assert_eq!(
        stash
            .list_durable_outputs()
            .unwrap()
            .iter()
            .filter(|record| record.tool_use_id.starts_with("overflow-"))
            .count(),
        12,
        "overflow compaction must persist every replacement target before its retry path"
    );

    let mut failed = tool_conversation(12, "failure");
    let untouched = conversation_bytes(&failed);
    stash.set_fail_durable_writes_for_test(true);
    let error = compact(&provider, &mut failed, &mut stash, 1_000_000)
        .await
        .unwrap_err();
    assert!(error.contains("injected durable output write failure"));
    assert_eq!(
        conversation_bytes(&failed),
        untouched,
        "failed durable write must abort before production compaction replaces inline content"
    );
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
        ("fallback", &OverflowProvider, 12, 100),
    ];
    let mut expected = BTreeMap::new();
    for (mode, provider, turns, window) in modes {
        expected.extend(expected_outputs(mode, turns));
        let mut conversation = tool_conversation(turns, mode);
        let before = conversation_bytes(&conversation);
        let message_count_before = conversation.messages.len();
        let compacted = compact(provider, &mut conversation, &mut original, window)
            .await
            .unwrap();
        assert!(compacted, "{mode} fixture must compact");
        assert_ne!(
            conversation_bytes(&conversation),
            before,
            "{mode} fixture must run production replacement"
        );
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
            "fallback" => {
                assert!(
                    conversation.messages.len() < message_count_before,
                    "deterministic fallback must reduce the message set"
                );
                assert!(
                    transcript.contains("[Context compacted:"),
                    "fallback must insert the deterministic compaction notice: {transcript}"
                );
            }
            _ => unreachable!(),
        }
    }

    drop(original);
    let owner = Mutex::new(OutputStash::with_session_id_and_durable_root(
        "trusted-owner",
        root.clone(),
    ));
    let listed: Vec<Value> =
        serde_json::from_str(&handle_stash_tool(&owner, "output_list", None).unwrap()).unwrap();
    let listed_ids = listed
        .iter()
        .map(|record| record["tool_use_id"].as_str().unwrap().to_owned())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        listed_ids,
        expected.keys().cloned().collect(),
        "output_list must be the complete authoritative index after reopen"
    );
    assert_eq!(
        listed.len(),
        expected.len(),
        "output_list must not omit or duplicate retained records"
    );
    for record in &listed {
        let id = record["tool_use_id"].as_str().unwrap();
        let expected = expected.get(id).unwrap();
        let chars = expected.bytes.chars().count();
        assert_eq!(record["turn"].as_u64(), Some(expected.turn));
        assert_eq!(record["result_kind"], "tool_result");
        assert_eq!(record["original_chars"].as_u64(), Some(chars as u64));
        assert_eq!(record["stored_chars"].as_u64(), Some(chars as u64));
        assert_eq!(record["completeness"], "complete");
        assert!(
            view(&owner, id).unwrap().contains(&expected.bytes),
            "routed output_view must return exact stored bytes for {id}"
        );
    }

    let foreign = Mutex::new(OutputStash::with_session_id_and_durable_root(
        "other-trusted-session",
        root,
    ));
    assert_eq!(
        handle_stash_tool(&foreign, "output_list", None).unwrap(),
        "[]"
    );
    for id in expected.keys() {
        assert!(
            view(&foreign, id).is_err(),
            "foreign session must not view {id}"
        );
    }
    for id in expected.keys() {
        assert!(view(&owner, id).is_ok(), "owner must retain {id}");
    }
}
