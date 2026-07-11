#!/usr/bin/perl
use strict;
use warnings;

local $/;
open(my $fh, '<', 'server/crates/djinn-core/src/message.rs') or die "Cannot open: $!";
my $content = <$fh>;
close $fh;

# Find the last test and insert new tests before the closing }
my $new_tests = <<'NEW_TESTS';

    // ── Provider-facing Anthropic conversion: skip guards ──────────────────

    /// Provider-internal blocks (Thinking with signature, RedactedThinking,
    /// Unknown, OpenAIReasoning) must be skipped by `to_anthropic_messages`
    /// rather than serialized as empty-text placeholders.
    #[test]
    fn anthropic_conversion_skips_signed_thinking_block() {
        let mut c = Conversation::new();
        c.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "internal reasoning".into(),
                    signature: Some("sig_abc".into()),
                },
                ContentBlock::text("visible output"),
            ],
            metadata: None,
        });

        let (_, msgs) = c.to_anthropic_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "assistant");
        let content = msgs[0]["content"].as_array().unwrap();
        // Only the text block should appear; the thinking block is skipped.
        assert_eq!(content.len(), 1);
        assert_eq!(content[0], json!({"type": "text", "text": "visible output"}));
        // Must not contain an empty-text placeholder.
        assert!(!content.iter().any(|b| b["type"] == "text" && b["text"] == ""));
    }

    #[test]
    fn anthropic_conversion_skips_redacted_thinking_block() {
        let mut c = Conversation::new();
        c.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::RedactedThinking {
                    data: "opaque_data_blob".into(),
                },
                ContentBlock::text("visible output"),
            ],
            metadata: None,
        });

        let (_, msgs) = c.to_anthropic_messages();
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0], json!({"type": "text", "text": "visible output"}));
        assert!(!content.iter().any(|b| b["type"] == "text" && b["text"] == ""));
    }

    #[test]
    fn anthropic_conversion_skips_unknown_passthrough_block() {
        let mut extra = serde_json::Map::new();
        extra.insert("foo".into(), json!("bar"));
        let mut c = Conversation::new();
        c.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Unknown {
                    content_type: "custom_block".into(),
                    extra,
                },
                ContentBlock::text("visible output"),
            ],
            metadata: None,
        });

        let (_, msgs) = c.to_anthropic_messages();
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0], json!({"type": "text", "text": "visible output"}));
        assert!(!content.iter().any(|b| b["type"] == "text" && b["text"] == ""));
    }

    #[test]
    fn anthropic_conversion_skips_openai_reasoning_block() {
        let mut c = Conversation::new();
        c.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::OpenAIReasoning {
                    id: Some("rs_1".into()),
                    encrypted_content: "encrypted".into(),
                    summary: Some(json!([])),
                    status: Some("completed".into()),
                },
                ContentBlock::text("visible output"),
            ],
            metadata: None,
        });

        let (_, msgs) = c.to_anthropic_messages();
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0], json!({"type": "text", "text": "visible output"}));
        assert!(!content.iter().any(|b| b["type"] == "text" && b["text"] == ""));
    }

    /// When ALL content blocks are provider-internal, the Anthropic
    /// content array should be empty rather than full of empty-text
    /// placeholders.
    #[test]
    fn anthropic_conversion_empty_content_when_all_blocks_are_internal() {
        let mut c = Conversation::new();
        c.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "deep thoughts".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::RedactedThinking {
                    data: "redacted_data".into(),
                },
            ],
            metadata: None,
        });

        let (_, msgs) = c.to_anthropic_messages();
        let content = msgs[0]["content"].as_array().unwrap();
        // Empty array, not two empty-text placeholders.
        assert_eq!(content.len(), 0);
    }

    /// Mixed provider-internal and visible blocks: only visible blocks appear.
    #[test]
    fn anthropic_conversion_mixed_visible_and_internal_blocks() {
        let mut c = Conversation::new();
        c.push(Message {
            role: Role::User,
            content: vec![
                ContentBlock::OpenAIReasoning {
                    id: None,
                    encrypted_content: "encrypted".into(),
                    summary: None,
                    status: None,
                },
                ContentBlock::text("user question"),
                ContentBlock::Thinking {
                    thinking: "thinking about user question".into(),
                    signature: None,
                },
            ],
            metadata: None,
        });

        let (_, msgs) = c.to_anthropic_messages();
        let content = msgs[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0], json!({"type": "text", "text": "user question"}));
    }

    // ── Non-Anthropic providers: do not emit thinking blocks ───────────────

    /// OpenAI Chat Completions serialization must skip Thinking, RedactedThinking,
    /// Unknown, and OpenAIReasoning provider-internal blocks.
    #[test]
    fn openai_serialization_skips_all_provider_internal_blocks() {
        let mut c = Conversation::new();
        c.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "reasoning".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::RedactedThinking {
                    data: "redacted".into(),
                },
                ContentBlock::Unknown {
                    content_type: "custom".into(),
                    extra: serde_json::Map::new(),
                },
                ContentBlock::OpenAIReasoning {
                    id: None,
                    encrypted_content: "enc".into(),
                    summary: None,
                    status: None,
                },
                ContentBlock::text("visible"),
            ],
            metadata: None,
        });

        let msgs = c.to_openai_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "assistant");
        // Only the text content should be present.
        assert_eq!(msgs[0]["content"], "visible");
    }

    /// Google serialization also skips all provider-internal blocks.
    #[test]
    fn google_serialization_skips_all_provider_internal_blocks() {
        let mut c = Conversation::new();
        c.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "reasoning".into(),
                    signature: Some("sig".into()),
                },
                ContentBlock::RedactedThinking {
                    data: "redacted".into(),
                },
                ContentBlock::Unknown {
                    content_type: "custom".into(),
                    extra: serde_json::Map::new(),
                },
                ContentBlock::OpenAIReasoning {
                    id: None,
                    encrypted_content: "enc".into(),
                    summary: None,
                    status: None,
                },
                ContentBlock::text("visible"),
            ],
            metadata: None,
        });

        let (_, contents) = c.to_google_contents();
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "visible");
    }

    // ── Existing text/tool/image/document behavior preserved ───────────────

    /// Text, tool use, tool result, image, and document blocks continue to
    /// serialize correctly through the Anthropic path after the skip guard
    /// change. This is a regression guard for the acceptance criterion that
    /// existing visible content behavior remains unchanged.
    #[test]
    fn anthropic_conversion_preserves_visible_content_blocks() {
        let mut c = Conversation::new();
        c.push(Message::user("hello"));
        c.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::text("I'll read the file."),
                ContentBlock::ToolUse {
                    id: "tu_1".into(),
                    name: "read".into(),
                    input: json!({"path": "/tmp/x"}),
                },
            ],
            metadata: None,
        });
        c.push(Message {
            role: Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "tu_1".into(),
                    content: vec![ContentBlock::text("file contents")],
                    is_error: false,
                },
                ContentBlock::text("thanks"),
            ],
            metadata: None,
        });
        c.push(Message {
            role: Role::User,
            content: vec![ContentBlock::Image {
                media_type: "image/png".into(),
                data: "iVBOR...".into(),
            }],
            metadata: None,
        });

        let (_, msgs) = c.to_anthropic_messages();
        assert_eq!(msgs.len(), 4);

        // User text
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"][0]["type"], "text");

        // Assistant text + tool_use
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["type"], "text");
        assert_eq!(msgs[1]["content"][1]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][1]["id"], "tu_1");

        // User tool_result + text
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[2]["content"][1]["type"], "text");

        // User image
        assert_eq!(msgs[3]["role"], "user");
        assert_eq!(msgs[3]["content"][0]["type"], "image");
    }
NEW_TESTS

# Find the closing of the last test and the module close
my $insert_point = rindex($content, "}\n}\n");
if ($insert_point < 0) {
    # Try alternate pattern
    $insert_point = rindex($content, "    }\n}\n");
}
die "Cannot find insertion point\n" if $insert_point < 0;

# Insert just before the last "}\n"
my $last_brace = rindex($content, "}\n");
substr($content, $last_brace, 0, $new_tests);

open(my $out, '>', 'server/crates/djinn-core/src/message.rs') or die "Cannot write: $!";
print $out $content;
close $out;

print "Tests added successfully\n";
