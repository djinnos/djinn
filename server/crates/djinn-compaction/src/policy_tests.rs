
use super::*;
use djinn_provider::message::ContentBlock;

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
        Message::assistant("Your context was compacted. The previous message contains a summary."),
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
    let would_skip = (tail_tokens as f64 / context_window as f64) < PARTIAL_COMPACTION_MIN_RECLAIM;
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
fn cap_summary_truncates_summary_longer_than_cap() {
    // Large context window so the 8K absolute cap is the binding limit.
    let context_window = 200_000;
    let long_summary = "S".repeat(20_000);
    let capped = cap_summary(long_summary.clone(), context_window);

    assert!(capped.len() < long_summary.len(), "summary should shrink");
    assert!(
        capped.len() <= SUMMARY_CAP_MAX_CHARS,
        "capped length {} exceeds absolute cap {}",
        capped.len(),
        SUMMARY_CAP_MAX_CHARS
    );
}

#[test]
fn cap_summary_passes_short_summary_unchanged() {
    let short = "A concise summary of prior work.".to_string();
    let capped = cap_summary(short.clone(), 200_000);
    assert_eq!(capped, short, "short summary must pass through unchanged");
}

#[test]
fn cap_summary_respects_smaller_of_8k_and_window_fraction() {
    // Small window: 15% of (window * 4 chars/token) is the binding limit.
    // window = 10_000 → 10_000 * 4 * 0.15 = 6_000 chars < 8_000.
    let context_window = 10_000;
    let expected_cap = (context_window as f64
        * SUMMARY_CAP_CHARS_PER_TOKEN as f64
        * SUMMARY_CAP_WINDOW_FRACTION) as usize;
    assert!(
        expected_cap < SUMMARY_CAP_MAX_CHARS,
        "test precondition: window fraction must be the smaller cap"
    );

    let long_summary = "Z".repeat(20_000);
    let capped = cap_summary(long_summary, context_window);
    assert!(
        capped.len() <= expected_cap,
        "capped length {} exceeds the window-fraction cap {}",
        capped.len(),
        expected_cap
    );
    // And it must NOT have been allowed to grow up to the 8K absolute cap.
    assert!(
        capped.len() < SUMMARY_CAP_MAX_CHARS,
        "the smaller window-fraction cap should bind below the absolute cap"
    );
}

#[test]
fn cap_summary_non_positive_window_falls_back_to_absolute_cap() {
    let long_summary = "Q".repeat(20_000);
    let capped = cap_summary(long_summary, 0);
    assert!(capped.len() <= SUMMARY_CAP_MAX_CHARS);
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

/// Helper: build an assistant message with a single `ToolUse` block.
fn tool_use_msg(id: &str, name: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input: serde_json::json!({}),
        }],
        metadata: None,
    }
}

/// Helper: build a user message with a single `ToolResult` block.
fn tool_result_msg(tool_use_id: &str, text: &str) -> Message {
    Message {
        role: Role::User,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: vec![ContentBlock::text(text)],
            is_error: false,
        }],
        metadata: None,
    }
}

/// Under a tight budget the most recent messages survive even when many
/// older messages are trimmed. The system prompt is always preserved.
#[test]
fn deterministic_compact_recent_tail_selection_under_tight_budget() {
    let messages = vec![
        Message::system("You are a coding agent."),
        Message::user("task A"),
        Message::assistant("response A"),
        Message::user("task B"),
        Message::assistant("response B"),
        Message::user("task C"),
        Message::assistant("response C"),
        Message::user("task D"),
        Message::assistant("response D"),
        Message::user("task E"),
        Message::assistant("response E"),
    ];

    // Budget large enough to hold system + 200 (notice overhead) + the last
    // pair ("task E" + "response E") but not all prior pairs.
    let sys_chars = estimate_message_chars(&messages[0]);
    let tail_chars = estimate_message_chars(&messages[9]) + estimate_message_chars(&messages[10]);
    let budget = sys_chars + 200 + tail_chars + 20;

    let result = deterministic_compact(&messages, budget);

    // System prompt is always first.
    assert_eq!(result[0].role, Role::System);
    assert_eq!(result[0].text_content(), "You are a coding agent.");

    // The very last message ("response E") must be present.
    assert_eq!(result.last().unwrap().text_content(), "response E");

    // A compacted notice must be inserted because messages were trimmed.
    let has_notice = result
        .iter()
        .any(|m| m.text_content().contains("Context compacted"));
    assert!(
        has_notice,
        "compacted notice must appear when messages are trimmed"
    );
}

/// When the conversation contains multiple tool-use/tool-result pairs and
/// the budget allows only the most recent pair, the pair-closure step keeps
/// the recent pair adjacent and pruning removes the oldest pair together,
/// never leaving an orphan `ToolResult`.
#[test]
fn deterministic_compact_pair_preservation_and_orphan_freedom() {
    let messages = vec![
        Message::system("sys"),
        Message::user("turn 1"),
        tool_use_msg("call_old", "bash"),
        tool_result_msg("call_old", "old output"),
        Message::assistant("continuing after old call"),
        tool_use_msg("call_new", "bash"),
        tool_result_msg("call_new", "new output"),
        Message::assistant("done"),
    ];

    // Budget: system + 200 + last three messages (tool_use, tool_result, "done")
    // but NOT the old pair.  This forces pair-closure and then pruning.
    let sys_chars = estimate_message_chars(&messages[0]);
    let recent_chars = estimate_message_chars(&messages[5])
        + estimate_message_chars(&messages[6])
        + estimate_message_chars(&messages[7]);
    let budget = sys_chars + 200 + recent_chars + 20;

    let result = deterministic_compact(&messages, budget);

    // System prompt preserved.
    assert_eq!(result[0].role, Role::System);

    // No orphaned ToolResult in the output.
    let orphan = find_orphaned_tool_result(&result);
    assert!(
        orphan.is_none(),
        "deterministic_compact produced orphaned tool result: {orphan:?}"
    );

    // Every ToolResult in the result is immediately preceded by a ToolUse.
    for (i, msg) in result.iter().enumerate() {
        if msg.role == Role::User
            && msg
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
        {
            assert!(
                i > 0,
                "ToolResult at result index 0 has no preceding ToolUse"
            );
            let prev = &result[i - 1];
            assert!(
                prev.role == Role::Assistant
                    && prev
                        .content
                        .iter()
                        .any(|b| matches!(b, ContentBlock::ToolUse { .. })),
                "ToolResult at result index {i} is not preceded by assistant ToolUse"
            );
        }
    }
}

/// When the budget allows every message, `deterministic_compact` must not
/// insert the "[Context compacted …]" notice.
#[test]
fn deterministic_compact_no_notice_when_nothing_trimmed() {
    let messages = vec![
        Message::system("sys"),
        Message::user("hello"),
        Message::assistant("world"),
    ];
    let result = deterministic_compact(&messages, 100_000);

    // No trimmed messages → no compacted notice.
    assert_eq!(result.len(), 3, "all messages should be preserved");
    assert!(
        !result
            .iter()
            .any(|m| m.text_content().contains("Context compacted")),
        "no compacted notice should appear when budget fits all messages"
    );
}

/// With several tool-use/tool-result pairs and a very tight budget, the
/// over-budget pruning removes the oldest pairs first. The result contains
/// zero orphan `ToolResult` entries across a range of budget sizes.
#[test]
fn deterministic_compact_over_budget_pruning_removes_oldest_pairs() {
    // 4 tool call rounds, each round: assistant(tool_use) + user(tool_result)
    let messages = vec![
        Message::system("sys"),
        Message::user("do the task"),
        tool_use_msg("c1", "bash"),
        tool_result_msg("c1", "output 1"),
        Message::assistant("step 1 done"),
        tool_use_msg("c2", "read_file"),
        tool_result_msg("c2", "output 2"),
        Message::assistant("step 2 done"),
        tool_use_msg("c3", "edit_file"),
        tool_result_msg("c3", "output 3"),
        Message::assistant("step 3 done"),
        tool_use_msg("c4", "bash"),
        tool_result_msg("c4", "output 4"),
        Message::assistant("all done"),
    ];

    // Sweep budget multipliers to exercise different pruning depths.
    for multiplier in [1usize, 2, 3, 5, 8, 10] {
        let budget = estimate_message_chars(&messages[0]) + 200 + (multiplier * 40);
        let result = deterministic_compact(&messages, budget);
        let orphan = find_orphaned_tool_result(&result);
        assert!(
            orphan.is_none(),
            "orphaned tool result {orphan:?} at budget multiplier {multiplier}"
        );

        // Every ToolResult must be preceded by ToolUse.
        for (i, msg) in result.iter().enumerate() {
            if msg.role == Role::User
                && msg
                    .content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::ToolResult { .. }))
            {
                assert!(
                    i > 0
                        && result[i - 1].role == Role::Assistant
                        && result[i - 1]
                            .content
                            .iter()
                            .any(|b| matches!(b, ContentBlock::ToolUse { .. })),
                    "orphaned ToolResult at result index {i} (multiplier {multiplier})"
                );
            }
        }
    }
}
