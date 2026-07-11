#!/usr/bin/perl
use strict;
use warnings;

# Slurp the entire file
local $/;
open(my $fh, '<', 'server/crates/djinn-core/src/message.rs') or die "Cannot open: $!";
my $content = <$fh>;
close $fh;

# 1. Replace the content_block_to_anthropic function
my $old_fn = <<'OLD_FN';
fn content_block_to_anthropic(block: &ContentBlock) -> serde_json::Value {
    use serde_json::json;
    match block {
        ContentBlock::Text { text } => json!({"type": "text", "text": text}),
        ContentBlock::ToolUse { id, name, input } => json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        }),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let inner: Vec<serde_json::Value> =
                content.iter().map(content_block_to_anthropic).collect();
            json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": inner,
                "is_error": is_error,
            })
        }
        ContentBlock::Image { media_type, data } => json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        }),
        ContentBlock::Document {
            media_type,
            data,
            filename,
        } => {
            let mut block = json!({
                "type": "document",
                "source": {
                    "type": "base64",
                    "media_type": media_type,
                    "data": data,
                }
            });
            if let Some(name) = filename {
                block["title"] = json!(name);
            }
            block
        }
        // Thinking blocks are display-only; skip when serializing for the API.
        // (Signed/redacted thinking replay is owned by the provider-format layer.)
        ContentBlock::Thinking { .. } => json!({"type": "text", "text": ""}),
        ContentBlock::RedactedThinking { .. } => json!({"type": "text", "text": ""}),
        ContentBlock::Unknown { .. } => json!({"type": "text", "text": ""}),
        ContentBlock::OpenAIReasoning { .. } => json!({"type": "text", "text": ""}),
    }
}
OLD_FN

my $new_fn = <<'NEW_FN';
/// Convert a content block to Anthropic wire format.
///
/// Returns `None` for provider-internal blocks (thinking, redacted thinking,
/// unknown passthrough, OpenAI reasoning) so callers can use `filter_map` to
/// skip them rather than emitting empty-text placeholders. Native Anthropic
/// replay serialization for signed/redacted thinking is owned by sibling
/// epic `xw13`.
fn content_block_to_anthropic(block: &ContentBlock) -> Option<serde_json::Value> {
    use serde_json::json;
    match block {
        ContentBlock::Text { text } => Some(json!({"type": "text", "text": text})),
        ContentBlock::ToolUse { id, name, input } => Some(json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input,
        })),
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } => {
            let inner: Vec<serde_json::Value> =
                content.iter().filter_map(content_block_to_anthropic).collect();
            Some(json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": inner,
                "is_error": is_error,
            }))
        }
        ContentBlock::Image { media_type, data } => Some(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        })),
        ContentBlock::Document {
            media_type,
            data,
            filename,
        } => {
            let mut doc = json!({
                "type": "document",
                "source": {
                    "type": "base64",
                    "media_type": media_type,
                    "data": data,
                }
            });
            if let Some(name) = filename {
                doc["title"] = json!(name);
            }
            Some(doc)
        }
        // Provider-internal blocks must not be serialized as empty text
        // placeholders. Skip them explicitly; native Anthropic replay
        // serialization for signed/redacted thinking is owned by sibling
        // epic xw13.
        ContentBlock::Thinking { .. }
        | ContentBlock::RedactedThinking { .. }
        | ContentBlock::Unknown { .. }
        | ContentBlock::OpenAIReasoning { .. } => None,
    }
}
NEW_FN

die "old function not found\n" unless index($content, $old_fn) >= 0;
substr($content, index($content, $old_fn), length($old_fn), $new_fn);

# 2. Update User call site
my $old_user = <<'OLD_USER';
                Role::User => {
                    let content: Vec<serde_json::Value> = msg
                        .content
                        .iter()
                        .filter(|b| !is_provider_internal(b))
                        .map(content_block_to_anthropic)
                        .collect();
                    msgs.push(json!({"role": "user", "content": content}));
                }
OLD_USER

my $new_user = <<'NEW_USER';
                Role::User => {
                    let content: Vec<serde_json::Value> = msg
                        .content
                        .iter()
                        .filter_map(|b| {
                            if is_provider_internal(b) {
                                None
                            } else {
                                content_block_to_anthropic(b)
                            }
                        })
                        .collect();
                    msgs.push(json!({"role": "user", "content": content}));
                }
NEW_USER

die "old user block not found\n" unless index($content, $old_user) >= 0;
substr($content, index($content, $old_user), length($old_user), $new_user);

# 3. Update Assistant call site
my $old_asst = <<'OLD_ASST';
                Role::Assistant => {
                    let content: Vec<serde_json::Value> = msg
                        .content
                        .iter()
                        .filter(|b| !is_provider_internal(b))
                        .map(content_block_to_anthropic)
                        .collect();
                    msgs.push(json!({"role": "assistant", "content": content}));
                }
OLD_ASST

my $new_asst = <<'NEW_ASST';
                Role::Assistant => {
                    let content: Vec<serde_json::Value> = msg
                        .content
                        .iter()
                        .filter_map(|b| {
                            if is_provider_internal(b) {
                                None
                            } else {
                                content_block_to_anthropic(b)
                            }
                        })
                        .collect();
                    msgs.push(json!({"role": "assistant", "content": content}));
                }
NEW_ASST

die "old assistant block not found\n" unless index($content, $old_asst) >= 0;
substr($content, index($content, $old_asst), length($old_asst), $new_asst);

# Write back
open(my $out, '>', 'server/crates/djinn-core/src/message.rs') or die "Cannot write: $!";
print $out $content;
close $out;

print "All replacements successful\n";
