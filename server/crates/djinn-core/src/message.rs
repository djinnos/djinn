// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
//! Djinn-native message and conversation types.
//!
//! These are the core data structures the reply loop, compaction, session
//! storage, and SSE streaming all operate on. The provider-agnostic model can
//! be serialized into OpenAI or Anthropic wire formats as needed.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─── Role ─────────────────────────────────────────────────────────────────────

/// The role of a participant in a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

// ─── ContentBlock ─────────────────────────────────────────────────────────────

/// A single unit of content within a message.
///
/// Uses an adjacently-tagged serde representation (`"type"` discriminant) so
/// that the JSON round-trips cleanly to and from DB storage.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// Plain text content.
    Text { text: String },

    /// A request to invoke a tool.
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },

    /// The result returned from a tool invocation.
    ToolResult {
        tool_use_id: String,
        content: Vec<ContentBlock>,
        is_error: bool,
    },

    /// Base64-encoded image content.
    Image {
        /// MIME type, e.g. `"image/png"`, `"image/jpeg"`.
        media_type: String,
        /// Raw base64-encoded image data (no `data:` prefix).
        data: String,
    },

    /// Base64-encoded document (e.g. PDF).
    Document {
        /// MIME type, e.g. `"application/pdf"`.
        media_type: String,
        /// Raw base64-encoded document data.
        data: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
    },

    /// Model reasoning/thinking content (extended thinking, chain-of-thought).
    ///
    /// The optional `signature` preserves Anthropic extended-thinking signatures
    /// so signed thinking blocks can round-trip through shared storage for later
    /// replay. Old stored JSON that lacks `signature` deserializes as `None`.
    Thinking {
        thinking: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },

    /// Anthropic redacted thinking content.
    ///
    /// When the safety filter redacts a thinking block, Anthropic replaces it
    /// with an opaque base64 `data` blob that must be replayed verbatim on
    /// subsequent turns. This variant preserves that `data` through serde.
    RedactedThinking { data: String },

    /// Opaque passthrough for provider-owned content blocks whose schema is not
    /// fully modeled by the shared representation.
    ///
    /// The original `"type"` discriminant is captured in `content_type` and any
    /// additional raw fields are preserved in `extra` so the block can survive a
    /// serde write/read cycle through shared storage without data loss.
    Unknown {
        content_type: String,
        #[serde(flatten)]
        extra: serde_json::Map<String, Value>,
    },

    /// OpenAI Responses reasoning item used to preserve stateless reasoning
    /// context across tool-call turns when `store=false`.
    ///
    /// This is provider state, not user-visible text. It should be serialized
    /// back only to the Responses API and skipped by other providers.
    OpenAIReasoning {
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        encrypted_content: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        summary: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
}

impl ContentBlock {
    /// Convenience constructor for a `Text` block.
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text { text: s.into() }
    }

    /// Return the contained text if this is a `Text` block.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        }
    }
}

// ─── MessageMeta ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheBreakpoint {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// Optional metadata attached to a single message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageMeta {
    /// Approximate input token count reported by the provider.
    pub input_tokens: Option<u32>,
    /// Approximate output token count reported by the provider.
    pub output_tokens: Option<u32>,
    /// Unix timestamp (seconds) when the message was created.
    pub timestamp: Option<i64>,
    /// Provider-specific message-level metadata used during request serialization.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_data: Option<Value>,
}

// ─── Message ──────────────────────────────────────────────────────────────────

/// A single turn in a conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<MessageMeta>,
}

impl Message {
    /// Create a user message containing a single text block.
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![ContentBlock::text(text)],
            metadata: None,
        }
    }

    /// Create an assistant message containing a single text block.
    pub fn assistant(text: impl Into<String>) -> Self {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::text(text)],
            metadata: None,
        }
    }

    /// Create a system message containing a single text block.
    pub fn system(text: impl Into<String>) -> Self {
        Message {
            role: Role::System,
            content: vec![ContentBlock::text(text)],
            metadata: None,
        }
    }

    /// Create a system message containing a single text block with metadata.
    pub fn system_with_metadata(text: impl Into<String>, metadata: MessageMeta) -> Self {
        Message {
            role: Role::System,
            content: vec![ContentBlock::text(text)],
            metadata: Some(metadata),
        }
    }

    /// Returns `true` if any content block is a `ToolUse`.
    pub fn has_tool_use(&self) -> bool {
        self.content
            .iter()
            .any(|b| matches!(b, ContentBlock::ToolUse { .. }))
    }

    /// Return the concatenated text of all `Text` content blocks.
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("")
    }
}

// ─── Conversation ─────────────────────────────────────────────────────────────

/// An ordered list of messages forming a conversation.
///
/// A `Conversation` may begin with a `System` message that sets the agent's
/// persona; all subsequent messages alternate between `User` and `Assistant`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Conversation {
    pub messages: Vec<Message>,
}

/// Per-string ceiling for OpenAI Responses `input` items.
///
/// The Responses API rejects any single string field longer than 10 MiB with
/// `string_above_max_length` (HTTP 400), which wedges *every* subsequent turn
/// of a session whose persisted history contains an oversized item — e.g. a
/// pre-truncation tool result. Upstream paths now clamp tool results to ~30k
/// chars ([`djinn_agent::output_stash::MAX_TOOL_RESULT_CHARS`]), so this is a
/// last-resort backstop: generous enough never to touch legitimate content,
/// safely under the provider's hard limit, and applied at serialization time
/// so it also rescues legacy sessions on their next turn.
const MAX_RESPONSES_STRING_BYTES: usize = 8 * 1024 * 1024; // 8 MiB

/// Clamp a string to [`MAX_RESPONSES_STRING_BYTES`], on a UTF-8 char boundary,
/// appending a marker noting how many bytes were dropped. Returns the input
/// unchanged (no allocation) when already within the limit.
fn clamp_responses_string(s: &str) -> std::borrow::Cow<'_, str> {
    if s.len() <= MAX_RESPONSES_STRING_BYTES {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut end = MAX_RESPONSES_STRING_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    let omitted = s.len() - end;
    std::borrow::Cow::Owned(format!(
        "{}\n\n[... {omitted} bytes truncated: this field exceeded the provider's \
         per-string size limit. The full content was available to the agent when \
         it was produced.]",
        &s[..end]
    ))
}

impl Conversation {
    /// Create an empty conversation.
    pub fn new() -> Self {
        Conversation::default()
    }

    /// Append a message to the conversation.
    pub fn push(&mut self, msg: Message) {
        self.messages.push(msg);
    }

    /// Return the text of the first `System` message if one exists.
    pub fn system_prompt(&self) -> Option<&str> {
        self.messages.iter().find_map(|m| {
            if m.role == Role::System {
                m.content.first().and_then(|b| b.as_text())
            } else {
                None
            }
        })
    }

    /// Iterate over non-system messages.
    pub fn user_messages(&self) -> impl Iterator<Item = &Message> {
        self.messages.iter().filter(|m| m.role != Role::System)
    }

    /// Return the last assistant message, if any.
    pub fn last_assistant(&self) -> Option<&Message> {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
    }

    /// Rough token estimate based on total character count divided by 4.
    pub fn token_estimate(&self) -> usize {
        let chars: usize = self
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .map(|b| match b {
                ContentBlock::Text { text } => text.len(),
                ContentBlock::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
                ContentBlock::ToolResult { content, .. } => content
                    .iter()
                    .filter_map(|c| c.as_text())
                    .map(|t| t.len())
                    .sum(),
                ContentBlock::Image { data, .. } => data.len(),
                ContentBlock::Document { data, .. } => data.len(),
                ContentBlock::Thinking { thinking, .. } => thinking.len(),
                ContentBlock::RedactedThinking { data } => data.len(),
                ContentBlock::Unknown { extra, .. } => extra.len(),
                ContentBlock::OpenAIReasoning {
                    encrypted_content, ..
                } => encrypted_content.len(),
            })
            .sum();
        chars / 4
    }

    /// Number of messages in the conversation.
    pub fn len(&self) -> usize {
        self.messages.len()
    }

    /// Return `true` if the conversation has no messages.
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    // ─── OpenAI serialization ────────────────────────────────────────────────

    /// Serialize to OpenAI chat completion messages format.
    ///
    /// - `System` messages → `{"role": "system", "content": "<text>"}`
    /// - `User` messages with only text → `{"role": "user", "content": "<text>"}`
    /// - `Assistant` messages with text only → `{"role": "assistant", "content": "<text>"}`
    /// - `Assistant` messages with tool use → message with `tool_calls` array
    /// - `User` messages with `ToolResult` blocks → `{"role": "tool", ...}` entries
    pub fn to_openai_messages(&self) -> Vec<serde_json::Value> {
        use serde_json::json;
        let mut out = Vec::new();

        for msg in &self.messages {
            match &msg.role {
                Role::System => {
                    let text = msg.text_content();
                    out.push(json!({"role": "system", "content": text}));
                }
                Role::User => {
                    // Separate tool results from plain text blocks.
                    let tool_results: Vec<&ContentBlock> = msg
                        .content
                        .iter()
                        .filter(|b| matches!(b, ContentBlock::ToolResult { .. }))
                        .collect();
                    let text_blocks: Vec<&ContentBlock> = msg
                        .content
                        .iter()
                        .filter(|b| !matches!(b, ContentBlock::ToolResult { .. }))
                        .collect();

                    // Emit one "tool" message per ToolResult.
                    for block in tool_results {
                        if let ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } = block
                        {
                            let result_text: String = content
                                .iter()
                                .filter_map(|c| c.as_text())
                                .collect::<Vec<_>>()
                                .join("");
                            out.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": result_text,
                                "is_error": is_error,
                            }));
                        }
                    }

                    // Emit user message for any plain content.
                    if !text_blocks.is_empty() {
                        let text: String = text_blocks
                            .iter()
                            .filter_map(|b| b.as_text())
                            .collect::<Vec<_>>()
                            .join("");
                        if !text.is_empty() {
                            out.push(json!({"role": "user", "content": text}));
                        }
                    }
                }
                Role::Assistant => {
                    let text_blocks: Vec<&ContentBlock> = msg
                        .content
                        .iter()
                        .filter(|b| matches!(b, ContentBlock::Text { .. }))
                        .collect();
                    let tool_uses: Vec<&ContentBlock> = msg
                        .content
                        .iter()
                        .filter(|b| matches!(b, ContentBlock::ToolUse { .. }))
                        .collect();
                    // Thinking/provider-state blocks are not sent to Chat Completions.

                    if tool_uses.is_empty() {
                        // Plain assistant message.
                        let text: String = text_blocks
                            .iter()
                            .filter_map(|b| b.as_text())
                            .collect::<Vec<_>>()
                            .join("");
                        out.push(json!({"role": "assistant", "content": text}));
                    } else {
                        // Build tool_calls array in OpenAI format.
                        let tool_calls: Vec<serde_json::Value> = tool_uses
                            .iter()
                            .map(|b| {
                                if let ContentBlock::ToolUse { id, name, input } = b {
                                    json!({
                                        "id": id,
                                        "type": "function",
                                        "function": {
                                            "name": name,
                                            "arguments": input.to_string(),
                                        }
                                    })
                                } else {
                                    unreachable!()
                                }
                            })
                            .collect();

                        let text: String = text_blocks
                            .iter()
                            .filter_map(|b| b.as_text())
                            .collect::<Vec<_>>()
                            .join("");

                        let content = if text.is_empty() {
                            serde_json::Value::Null
                        } else {
                            serde_json::Value::String(text)
                        };

                        out.push(json!({
                            "role": "assistant",
                            "content": content,
                            "tool_calls": tool_calls,
                        }));
                    }
                }
            }
        }

        out
    }

    // ─── Anthropic serialization ─────────────────────────────────────────────

    /// Serialize to Anthropic messages API format.
    ///
    /// Returns `(system_prompt, messages_array)`. The system prompt is
    /// extracted from the first `System` message and returned as a plain
    /// string; it must be passed as a top-level `"system"` field in the API
    /// request, NOT inside the messages array.
    ///
    /// Non-system messages use Anthropic's content-array format.
    pub fn to_anthropic_messages(&self) -> (Option<String>, Vec<serde_json::Value>) {
        use serde_json::json;
        let mut system: Option<String> = None;
        let mut msgs: Vec<serde_json::Value> = Vec::new();

        for msg in &self.messages {
            match &msg.role {
                Role::System => {
                    // Only the first system message is used.
                    if system.is_none() {
                        system = Some(msg.text_content());
                    }
                }
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
            }
        }

        (system, msgs)
    }

    // ─── Google serialization ────────────────────────────────────────────────

    /// Serialize to Google AI Studio / Vertex AI `contents` format.
    ///
    /// Returns `(system_instruction, contents_array)`. The system instruction
    /// is extracted from the first `System` message; non-system messages use
    /// Google's `parts` format with `user` / `model` roles.
    pub fn to_google_contents(&self) -> (Option<String>, Vec<serde_json::Value>) {
        use serde_json::json;
        let mut system: Option<String> = None;
        let mut contents: Vec<serde_json::Value> = Vec::new();

        for msg in &self.messages {
            match &msg.role {
                Role::System => {
                    if system.is_none() {
                        system = Some(msg.text_content());
                    }
                }
                role => {
                    let google_role = match role {
                        Role::User => "user",
                        Role::Assistant => "model",
                        Role::System => unreachable!(),
                    };

                    let parts: Vec<serde_json::Value> = msg
                        .content
                        .iter()
                        .flat_map(|block| match block {
                            ContentBlock::Text { text } => {
                                vec![json!({"text": text})]
                            }
                            ContentBlock::ToolUse { name, input, .. } => {
                                vec![json!({"functionCall": {"name": name, "args": input}})]
                            }
                            ContentBlock::Image { media_type, data } => {
                                vec![json!({"inlineData": {"mimeType": media_type, "data": data}})]
                            }
                            ContentBlock::Document {
                                media_type, data, ..
                            } => {
                                vec![json!({"inlineData": {"mimeType": media_type, "data": data}})]
                            }
                            ContentBlock::Thinking { .. } => {
                                // Thinking blocks are display-only; skip for Google.
                                vec![]
                            }
                            ContentBlock::RedactedThinking { .. } => {
                                // Redacted thinking is provider-internal; skip for Google.
                                vec![]
                            }
                            ContentBlock::Unknown { .. } => {
                                // Unknown provider blocks are passthrough-only; skip for Google.
                                vec![]
                            }
                            ContentBlock::OpenAIReasoning { .. } => {
                                // Responses reasoning state is provider-private.
                                vec![]
                            }
                            ContentBlock::ToolResult { content, .. } => content
                                .iter()
                                .filter_map(|c| {
                                    if let ContentBlock::Text { text } = c {
                                        Some(json!({"text": text}))
                                    } else {
                                        None
                                    }
                                })
                                .collect(),
                        })
                        .collect();

                    contents.push(json!({"role": google_role, "parts": parts}));
                }
            }
        }

        (system, contents)
    }

    // ─── OpenAI Responses serialization ──────────────────────────────────────

    /// Serialize to OpenAI Responses API `input` format.
    ///
    /// Returns `(instructions, input_items)`. System messages are merged into
    /// a single `instructions` string; tool calls become `function_call` items
    /// and tool results become `function_call_output` items.
    pub fn to_openai_responses_input(&self) -> (Option<String>, Vec<serde_json::Value>) {
        use serde_json::json;
        use std::collections::HashSet;
        let mut input_items: Vec<serde_json::Value> = Vec::new();
        let mut instructions: Option<String> = None;

        // Tool-call / tool-result pairing invariant. The Responses API
        // rejects a `function_call` whose `call_id` has no matching
        // `function_call_output` ("No tool output found for function call
        // …", HTTP 400) and, symmetrically, an output with no call. A turn
        // interrupted mid-tool-call (provider stream error, crash) or a
        // legacy session persisted before tool results were stored can leave
        // such an orphan in the history; emitting it wedges every subsequent
        // turn of the session. Collect the ids that are actually paired and
        // drop the unpaired blocks below so one bad turn can't poison the
        // conversation. (The live chat loop always appends a result before
        // the next request, so this only ever drops genuinely-orphaned data.)
        let mut tool_use_ids: HashSet<&str> = HashSet::new();
        let mut tool_result_ids: HashSet<&str> = HashSet::new();
        for msg in &self.messages {
            for block in &msg.content {
                match block {
                    ContentBlock::ToolUse { id, .. } => {
                        tool_use_ids.insert(id.as_str());
                    }
                    ContentBlock::ToolResult { tool_use_id, .. } => {
                        tool_result_ids.insert(tool_use_id.as_str());
                    }
                    _ => {}
                }
            }
        }

        for msg in &self.messages {
            match msg.role {
                Role::System => {
                    let text = msg.text_content();
                    if !text.is_empty() {
                        match &mut instructions {
                            Some(existing) => {
                                existing.push_str("\n\n");
                                existing.push_str(&text);
                            }
                            None => {
                                instructions = Some(text);
                            }
                        }
                    }
                }
                Role::User => {
                    let mut text_items: Vec<serde_json::Value> = Vec::new();

                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } if !text.is_empty() => {
                                text_items.push(json!({
                                    "type": "input_text",
                                    "text": clamp_responses_string(text)
                                }));
                            }
                            ContentBlock::Image { media_type, data } => {
                                text_items.push(json!({
                                    "type": "input_image",
                                    "image_url": format!("data:{media_type};base64,{data}")
                                }));
                            }
                            ContentBlock::Document {
                                data,
                                media_type,
                                filename,
                            } => {
                                // OpenAI Responses API supports file content via input_file
                                text_items.push(json!({
                                    "type": "input_file",
                                    "filename": filename.as_deref().unwrap_or("document"),
                                    "file_data": format!("data:{media_type};base64,{data}")
                                }));
                            }
                            ContentBlock::ToolResult {
                                tool_use_id,
                                content,
                                is_error,
                            } => {
                                // Orphaned result (no matching call) — skip;
                                // the Responses API rejects an output whose
                                // call_id it never saw.
                                if !tool_use_ids.contains(tool_use_id.as_str()) {
                                    continue;
                                }
                                if !text_items.is_empty() {
                                    input_items.push(json!({
                                        "role": "user",
                                        "content": std::mem::take(&mut text_items)
                                    }));
                                }

                                let result_text: String = content
                                    .iter()
                                    .filter_map(|c| c.as_text())
                                    .collect::<Vec<_>>()
                                    .join("\n");

                                let output = if *is_error {
                                    format!("Error: {}", result_text)
                                } else {
                                    result_text
                                };

                                input_items.push(json!({
                                    "type": "function_call_output",
                                    "call_id": tool_use_id,
                                    "output": clamp_responses_string(&output)
                                }));
                            }
                            _ => {}
                        }
                    }

                    if !text_items.is_empty() {
                        input_items.push(json!({
                            "role": "user",
                            "content": text_items
                        }));
                    }
                }
                Role::Assistant => {
                    let mut text_items: Vec<serde_json::Value> = Vec::new();

                    for block in &msg.content {
                        match block {
                            ContentBlock::Text { text } if !text.is_empty() => {
                                text_items.push(json!({
                                    "type": "output_text",
                                    "text": clamp_responses_string(text)
                                }));
                            }
                            ContentBlock::ToolUse { id, name, input } => {
                                // Orphaned call (no matching result) — skip so
                                // the Responses API doesn't 400. Keep buffered
                                // text; it's still valid assistant output.
                                if !tool_result_ids.contains(id.as_str()) {
                                    continue;
                                }
                                if !text_items.is_empty() {
                                    input_items.push(json!({
                                        "role": "assistant",
                                        "content": std::mem::take(&mut text_items)
                                    }));
                                }

                                let arguments_str = serde_json::to_string(input)
                                    .unwrap_or_else(|_| "{}".to_string());

                                input_items.push(json!({
                                    "type": "function_call",
                                    "call_id": id,
                                    "name": name,
                                    "arguments": arguments_str
                                }));
                            }
                            ContentBlock::OpenAIReasoning {
                                id,
                                encrypted_content,
                                summary,
                                status,
                            } if !encrypted_content.is_empty() => {
                                if !text_items.is_empty() {
                                    input_items.push(json!({
                                        "role": "assistant",
                                        "content": std::mem::take(&mut text_items)
                                    }));
                                }

                                let mut item = json!({
                                    "type": "reasoning",
                                    "encrypted_content": encrypted_content,
                                });
                                if let Some(id) = id {
                                    item["id"] = json!(id);
                                }
                                if let Some(summary) = summary {
                                    item["summary"] = summary.clone();
                                }
                                if let Some(status) = status {
                                    item["status"] = json!(status);
                                }
                                input_items.push(item);
                            }
                            _ => {}
                        }
                    }

                    if !text_items.is_empty() {
                        input_items.push(json!({
                            "role": "assistant",
                            "content": text_items
                        }));
                    }
                }
            }
        }

        (instructions, input_items)
    }

    // ─── Dangling tool-call sanitization ─────────────────────────────────────

    /// Return a view of this conversation with a synthesized tool result for
    /// every assistant `ToolUse` whose `tool_use_id` has no matching
    /// `ToolResult` anywhere in the history.
    ///
    /// Strict OpenAI-compatible and Anthropic APIs reject a request whose
    /// assistant `tool_calls` / `tool_use` message is not answered by a tool
    /// message for every `tool_call_id` (HTTP 400 `invalid_request_error`). A
    /// session cancelled or killed *between* emitting the assistant tool-call
    /// message and persisting the corresponding tool results leaves such
    /// dangling ids at the tail of the stored transcript. Replaying that history
    /// verbatim on resume/redispatch 400s the whole request, so the session
    /// fails instantly on every retry and the task wedges in dispatch backoff.
    ///
    /// Synthesizing a placeholder result — rather than dropping the assistant
    /// turn — preserves the model's context of what it was doing while
    /// satisfying the tool-call/tool-result pairing invariant. Because this
    /// operates on the provider-agnostic [`Conversation`], a single pass repairs
    /// every wire format (OpenAI chat, OpenAI Responses, Anthropic, Google);
    /// the downstream serializers just see a well-formed history.
    ///
    /// The repaired history is a transient view used only for request
    /// serialization: this borrows the receiver unchanged when the transcript is
    /// already well-formed, and never mutates stored history.
    pub fn with_synthesized_tool_results(&self) -> std::borrow::Cow<'_, Conversation> {
        use std::collections::HashSet;

        // Every tool_use_id that already has a result somewhere in the history.
        let mut answered: HashSet<&str> = HashSet::new();
        for msg in &self.messages {
            for block in &msg.content {
                if let ContentBlock::ToolResult { tool_use_id, .. } = block {
                    answered.insert(tool_use_id.as_str());
                }
            }
        }

        let is_dangling = |block: &ContentBlock| matches!(block, ContentBlock::ToolUse { id, .. } if !answered.contains(id.as_str()));
        let has_dangling = self
            .messages
            .iter()
            .any(|m| m.role == Role::Assistant && m.content.iter().any(is_dangling));
        if !has_dangling {
            return std::borrow::Cow::Borrowed(self);
        }

        let mut repaired: Vec<Message> = Vec::with_capacity(self.messages.len() + 1);
        let mut i = 0;
        while i < self.messages.len() {
            let msg = &self.messages[i];
            repaired.push(msg.clone());

            if msg.role == Role::Assistant {
                let synth_blocks: Vec<ContentBlock> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolUse { id, .. } if !answered.contains(id.as_str()) => {
                            Some(ContentBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content: vec![ContentBlock::text(INTERRUPTED_TOOL_RESULT)],
                                is_error: true,
                            })
                        }
                        _ => None,
                    })
                    .collect();

                if !synth_blocks.is_empty() {
                    // Merge the synthesized results into the immediately
                    // following user message when one exists (a partial-result
                    // turn, or a resume nudge). This preserves Anthropic's
                    // strict user/assistant alternation — two consecutive user
                    // messages would themselves be rejected. Tool results lead
                    // the user turn, as Anthropic requires. Otherwise insert a
                    // fresh user turn carrying only the synthesized results.
                    match self.messages.get(i + 1) {
                        Some(next) if next.role == Role::User => {
                            let mut merged = next.clone();
                            let mut content = synth_blocks;
                            content.extend(merged.content);
                            merged.content = content;
                            repaired.push(merged);
                            i += 2;
                            continue;
                        }
                        _ => {
                            repaired.push(Message {
                                role: Role::User,
                                content: synth_blocks,
                                metadata: None,
                            });
                        }
                    }
                }
            }
            i += 1;
        }

        std::borrow::Cow::Owned(Conversation { messages: repaired })
    }
}

/// Placeholder content synthesized for a tool call whose result was never
/// persisted because the session was cancelled/killed mid-tool-execution.
/// See [`Conversation::with_synthesized_tool_results`].
const INTERRUPTED_TOOL_RESULT: &str = "[tool execution interrupted before completion — the \
     session was cancelled; re-run the tool if the result is still needed]";

// ─── Anthropic content-block helpers ─────────────────────────────────────────

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
            let inner: Vec<serde_json::Value> = content
                .iter()
                .filter_map(content_block_to_anthropic)
                .collect();
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

/// Returns `true` for content blocks that are internal to one provider and
/// should not be serialized to unrelated provider APIs.
fn is_provider_internal(block: &ContentBlock) -> bool {
    matches!(
        block,
        ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. }
            | ContentBlock::Unknown { .. }
            | ContentBlock::OpenAIReasoning { .. }
    )
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Basic constructors ────────────────────────────────────────────────────

    #[test]
    fn user_message_has_text_content_block() {
        let msg = Message::user("hello");
        assert_eq!(msg.content.len(), 1);
        assert_eq!(
            msg.content[0],
            ContentBlock::Text {
                text: "hello".into()
            }
        );
    }

    #[test]
    fn assistant_message_role() {
        let msg = Message::assistant("done");
        assert_eq!(msg.role, Role::Assistant);
    }

    #[test]
    fn system_message_role() {
        let msg = Message::system("You are a helpful assistant.");
        assert_eq!(msg.role, Role::System);
    }

    #[test]
    fn content_block_text_helper() {
        let b = ContentBlock::text("hi");
        assert_eq!(b.as_text(), Some("hi"));
    }

    // ── Conversation helpers ──────────────────────────────────────────────────

    #[test]
    fn conversation_system_prompt() {
        let mut c = Conversation::new();
        c.push(Message::system("Be terse."));
        c.push(Message::user("hello"));
        assert_eq!(c.system_prompt(), Some("Be terse."));
    }

    #[test]
    fn conversation_last_assistant() {
        let mut c = Conversation::new();
        c.push(Message::user("ping"));
        c.push(Message::assistant("pong"));
        assert_eq!(c.last_assistant().unwrap().text_content(), "pong");
    }

    #[test]
    fn conversation_user_messages_excludes_system() {
        let mut c = Conversation::new();
        c.push(Message::system("sys"));
        c.push(Message::user("u"));
        c.push(Message::assistant("a"));
        let non_sys: Vec<_> = c.user_messages().collect();
        assert_eq!(non_sys.len(), 2);
        assert!(non_sys.iter().all(|m| m.role != Role::System));
    }

    #[test]
    fn conversation_len_and_is_empty() {
        let mut c = Conversation::new();
        assert!(c.is_empty());
        c.push(Message::user("x"));
        assert_eq!(c.len(), 1);
        assert!(!c.is_empty());
    }

    // ── Serde round-trip ──────────────────────────────────────────────────────

    #[test]
    fn message_round_trip() {
        let msg = Message::user("round trip");
        let serialized = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&serialized).unwrap();
        assert_eq!(msg, back);
    }

    #[test]
    fn tool_use_round_trip() {
        let block = ContentBlock::ToolUse {
            id: "call_1".into(),
            name: "bash".into(),
            input: json!({"command": "ls"}),
        };
        let serialized = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&serialized).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn tool_result_round_trip() {
        let block = ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: vec![ContentBlock::text("output")],
            is_error: false,
        };
        let serialized = serde_json::to_string(&block).unwrap();
        let back: ContentBlock = serde_json::from_str(&serialized).unwrap();
        assert_eq!(block, back);
    }

    #[test]
    fn conversation_round_trip() {
        let mut c = Conversation::new();
        c.push(Message::system("sys prompt"));
        c.push(Message::user("hello"));
        c.push(Message::assistant("hi"));
        let serialized = serde_json::to_string(&c).unwrap();
        let back: Conversation = serde_json::from_str(&serialized).unwrap();
        assert_eq!(c.messages, back.messages);
    }

    fn mixed_provider_conversation() -> Conversation {
        Conversation {
            messages: vec![
                Message::system("Follow policy."),
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::text("Need weather"),
                        ContentBlock::ToolResult {
                            tool_use_id: "orphan".into(),
                            content: vec![ContentBlock::text("cached")],
                            is_error: true,
                        },
                        ContentBlock::text(" now"),
                    ],
                    metadata: None,
                },
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::text("Checking."),
                        ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "weather".into(),
                            input: json!({"city": "Paris"}),
                        },
                        ContentBlock::text("Done."),
                    ],
                    metadata: None,
                },
                Message {
                    role: Role::User,
                    content: vec![
                        ContentBlock::ToolResult {
                            tool_use_id: "call_1".into(),
                            content: vec![ContentBlock::text("72F"), ContentBlock::text(" sunny")],
                            is_error: false,
                        },
                        ContentBlock::text("Thanks"),
                        ContentBlock::Text {
                            text: String::new(),
                        },
                    ],
                    metadata: None,
                },
                Message::assistant("It is 72F and sunny."),
            ],
        }
    }

    // ── OpenAI serialization ──────────────────────────────────────────────────

    #[test]
    fn to_openai_messages_simple() {
        let mut c = Conversation::new();
        c.push(Message::system("Be helpful."));
        c.push(Message::user("What is 2+2?"));
        c.push(Message::assistant("4"));

        let msgs = c.to_openai_messages();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "Be helpful.");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[1]["content"], "What is 2+2?");
        assert_eq!(msgs[2]["role"], "assistant");
        assert_eq!(msgs[2]["content"], "4");
    }

    #[test]
    fn to_openai_messages_tool_use() {
        let mut c = Conversation::new();
        c.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tc_1".into(),
                name: "bash".into(),
                input: json!({"command": "echo hi"}),
            }],
            metadata: None,
        });
        c.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tc_1".into(),
                content: vec![ContentBlock::text("hi")],
                is_error: false,
            }],
            metadata: None,
        });

        let msgs = c.to_openai_messages();
        assert_eq!(msgs[0]["role"], "assistant");
        assert!(msgs[0]["tool_calls"].is_array());
        assert_eq!(msgs[0]["tool_calls"][0]["id"], "tc_1");
        assert_eq!(msgs[1]["role"], "tool");
        assert_eq!(msgs[1]["tool_call_id"], "tc_1");
        assert_eq!(msgs[1]["content"], "hi");
    }

    #[test]
    fn to_openai_messages_preserves_current_tool_result_ordering_and_empty_text_behavior() {
        let msgs = mixed_provider_conversation().to_openai_messages();

        assert_eq!(msgs.len(), 7);
        assert_eq!(
            msgs[0],
            json!({"role": "system", "content": "Follow policy."})
        );
        assert_eq!(
            msgs[1],
            json!({
                "role": "tool",
                "tool_call_id": "orphan",
                "content": "cached",
                "is_error": true,
            })
        );
        assert_eq!(
            msgs[2],
            json!({"role": "user", "content": "Need weather now"})
        );
        assert_eq!(msgs[3]["role"], "assistant");
        assert_eq!(msgs[3]["content"], "Checking.Done.");
        assert_eq!(msgs[3]["tool_calls"][0]["id"], "call_1");
        assert_eq!(msgs[3]["tool_calls"][0]["function"]["name"], "weather");
        assert_eq!(
            msgs[3]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":\"Paris\"}"
        );
        assert_eq!(
            msgs[4],
            json!({
                "role": "tool",
                "tool_call_id": "call_1",
                "content": "72F sunny",
                "is_error": false,
            })
        );
        assert_eq!(msgs[5], json!({"role": "user", "content": "Thanks"}));
        assert_eq!(
            msgs[6],
            json!({"role": "assistant", "content": "It is 72F and sunny."})
        );
    }

    // ── Anthropic serialization ───────────────────────────────────────────────

    #[test]
    fn to_anthropic_messages_separates_system() {
        let mut c = Conversation::new();
        c.push(Message::system("You are Claude."));
        c.push(Message::user("hello"));
        c.push(Message::assistant("hi there"));

        let (sys, msgs) = c.to_anthropic_messages();
        assert_eq!(sys, Some("You are Claude.".to_string()));
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[1]["role"], "assistant");
    }

    #[test]
    fn to_anthropic_messages_content_array() {
        let mut c = Conversation::new();
        c.push(Message::user("explain recursion"));

        let (sys, msgs) = c.to_anthropic_messages();
        assert!(sys.is_none());
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0]["content"].is_array());
        assert_eq!(msgs[0]["content"][0]["type"], "text");
        assert_eq!(msgs[0]["content"][0]["text"], "explain recursion");
    }

    #[test]
    fn to_anthropic_messages_tool_blocks() {
        let mut c = Conversation::new();
        c.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "tu_1".into(),
                name: "read_file".into(),
                input: json!({"path": "/tmp/x"}),
            }],
            metadata: None,
        });
        c.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "tu_1".into(),
                content: vec![ContentBlock::text("file contents")],
                is_error: false,
            }],
            metadata: None,
        });

        let (_sys, msgs) = c.to_anthropic_messages();
        assert_eq!(msgs[0]["content"][0]["type"], "tool_use");
        assert_eq!(msgs[0]["content"][0]["id"], "tu_1");
        assert_eq!(msgs[1]["content"][0]["type"], "tool_result");
        assert_eq!(msgs[1]["content"][0]["tool_use_id"], "tu_1");
    }

    #[test]
    fn to_anthropic_messages_preserve_roles_and_block_order() {
        let (system, msgs) = mixed_provider_conversation().to_anthropic_messages();

        assert_eq!(system, Some("Follow policy.".to_string()));
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(
            msgs[0]["content"][0],
            json!({"type": "text", "text": "Need weather"})
        );
        assert_eq!(msgs[0]["content"][1]["type"], "tool_result");
        assert_eq!(msgs[0]["content"][1]["tool_use_id"], "orphan");
        assert_eq!(
            msgs[0]["content"][2],
            json!({"type": "text", "text": " now"})
        );
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(
            msgs[1]["content"][0],
            json!({"type": "text", "text": "Checking."})
        );
        assert_eq!(msgs[1]["content"][1]["type"], "tool_use");
        assert_eq!(msgs[1]["content"][1]["name"], "weather");
        assert_eq!(
            msgs[1]["content"][2],
            json!({"type": "text", "text": "Done."})
        );
        assert_eq!(msgs[2]["role"], "user");
        assert_eq!(msgs[2]["content"][0]["type"], "tool_result");
        assert_eq!(
            msgs[2]["content"][1],
            json!({"type": "text", "text": "Thanks"})
        );
        assert_eq!(msgs[2]["content"][2], json!({"type": "text", "text": ""}));
        assert_eq!(msgs[3]["role"], "assistant");
        assert_eq!(
            msgs[3]["content"][0],
            json!({"type": "text", "text": "It is 72F and sunny."})
        );
    }

    // ── Google serialization ──────────────────────────────────────────────────

    #[test]
    fn to_google_contents_maps_roles_and_parts() {
        let (system, contents) = mixed_provider_conversation().to_google_contents();

        assert_eq!(system, Some("Follow policy.".to_string()));
        assert_eq!(contents.len(), 4);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(
            contents[0]["parts"],
            json!([
                {"text": "Need weather"},
                {"text": "cached"},
                {"text": " now"}
            ])
        );
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(
            contents[1]["parts"],
            json!([
                {"text": "Checking."},
                {"functionCall": {"name": "weather", "args": {"city": "Paris"}}},
                {"text": "Done."}
            ])
        );
        assert_eq!(contents[2]["role"], "user");
        assert_eq!(
            contents[2]["parts"],
            json!([
                {"text": "72F"},
                {"text": " sunny"},
                {"text": "Thanks"},
                {"text": ""}
            ])
        );
        assert_eq!(contents[3]["role"], "model");
        assert_eq!(
            contents[3]["parts"],
            json!([{"text": "It is 72F and sunny."}])
        );
    }

    // ── OpenAI Responses serialization ────────────────────────────────────────

    #[test]
    fn to_openai_responses_input_maps_mixed_conversation() {
        let (instructions, input) = mixed_provider_conversation().to_openai_responses_input();

        assert_eq!(instructions, Some("Follow policy.".to_string()));
        // The orphaned tool result (`call_id: "orphan"` — no matching call)
        // is dropped: the Responses API rejects an output for a call it never
        // saw. With it gone, the two user texts that surrounded it collapse
        // into a single input_text group.
        assert_eq!(input.len(), 7);
        assert_eq!(
            input[0],
            json!({
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Need weather"},
                    {"type": "input_text", "text": " now"}
                ]
            })
        );
        assert_eq!(
            input[1],
            json!({
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Checking."}]
            })
        );
        assert_eq!(
            input[2],
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "weather",
                "arguments": "{\"city\":\"Paris\"}"
            })
        );
        assert_eq!(
            input[3],
            json!({
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Done."}]
            })
        );
        assert_eq!(
            input[4],
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "72F\n sunny"
            })
        );
        assert_eq!(
            input[5],
            json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "Thanks"}]
            })
        );
        assert_eq!(
            input[6],
            json!({
                "role": "assistant",
                "content": [{"type": "output_text", "text": "It is 72F and sunny."}]
            })
        );
        assert!(
            !input.iter().any(|i| i["call_id"] == "orphan"),
            "orphaned function_call_output must not be emitted"
        );
    }

    #[test]
    fn to_openai_responses_input_drops_orphaned_function_call() {
        // Reproduces the "No tool output found for function call …" 400: an
        // assistant turn made a tool call but its result was never recorded
        // (interrupted stream, or history persisted before tool results
        // were stored), then the conversation moved on. The orphaned
        // `function_call` must not be emitted or the request is rejected.
        let mut conversation = Conversation::new();
        conversation.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::text("Let me check."),
                ContentBlock::ToolUse {
                    id: "call_orphan".into(),
                    name: "search".into(),
                    input: json!({"q": "x"}),
                },
            ],
            metadata: None,
        });
        conversation.push(Message::user("never mind, what's 2+2?"));

        let (_, input) = conversation.to_openai_responses_input();

        // Assistant text survives; the dangling call is gone.
        assert!(input.iter().any(|i| {
            i["content"]
                .as_array()
                .is_some_and(|c| c.iter().any(|b| b["text"] == "Let me check."))
        }));
        assert!(
            !input.iter().any(|i| i["type"] == "function_call"),
            "orphaned function_call must be dropped"
        );
        assert!(!input.iter().any(|i| i["call_id"] == "call_orphan"));
    }

    #[test]
    fn to_openai_responses_input_keeps_paired_tool_call() {
        // The normal case: a tool call WITH its matching result round-trips
        // intact — the invariant only drops genuine orphans.
        let mut conversation = Conversation::new();
        conversation.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_ok".into(),
                name: "search".into(),
                input: json!({"q": "x"}),
            }],
            metadata: None,
        });
        conversation.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_ok".into(),
                content: vec![ContentBlock::text("result")],
                is_error: false,
            }],
            metadata: None,
        });

        let (_, input) = conversation.to_openai_responses_input();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "call_ok");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call_ok");
    }

    #[test]
    fn to_openai_responses_input_clamps_oversized_tool_output() {
        // Backstop for the `string_above_max_length` 400: a legacy/oversized
        // tool result must be clamped under the provider's per-string limit at
        // serialization time so it can't wedge the session on replay.
        let huge = "z".repeat(MAX_RESPONSES_STRING_BYTES + 4096);
        let mut conversation = Conversation::new();
        conversation.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "call_big".into(),
                name: "code_graph".into(),
                input: json!({}),
            }],
            metadata: None,
        });
        conversation.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_big".into(),
                content: vec![ContentBlock::text(huge.clone())],
                is_error: false,
            }],
            metadata: None,
        });

        let (_, input) = conversation.to_openai_responses_input();
        let output = input[1]["output"].as_str().unwrap();
        assert!(output.len() <= MAX_RESPONSES_STRING_BYTES + 256); // clamp + marker
        assert!(output.len() < huge.len());
        assert!(output.contains("bytes truncated"));
    }

    #[test]
    fn to_openai_responses_input_leaves_normal_output_untouched() {
        // The common case must be byte-for-byte identical — the clamp only
        // fires on pathological sizes.
        let mut conversation = Conversation::new();
        conversation.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: "c".into(),
                name: "shell".into(),
                input: json!({}),
            }],
            metadata: None,
        });
        conversation.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "c".into(),
                content: vec![ContentBlock::text("ordinary result")],
                is_error: false,
            }],
            metadata: None,
        });

        let (_, input) = conversation.to_openai_responses_input();
        assert_eq!(input[1]["output"], "ordinary result");
    }

    #[test]
    fn to_openai_responses_input_preserves_openai_reasoning_items() {
        let mut conversation = Conversation::new();
        conversation.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::OpenAIReasoning {
                    id: Some("rs_1".into()),
                    encrypted_content: "encrypted".into(),
                    summary: Some(json!([])),
                    status: Some("completed".into()),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "shell".into(),
                    input: json!({"cmd": "pwd"}),
                },
            ],
            metadata: None,
        });
        conversation.push(Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: "call_1".into(),
                content: vec![ContentBlock::text("/repo")],
                is_error: false,
            }],
            metadata: None,
        });

        let (_, input) = conversation.to_openai_responses_input();
        assert_eq!(input.len(), 3);
        assert_eq!(
            input[0],
            json!({
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "encrypted",
                "summary": [],
                "status": "completed"
            })
        );
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[2]["type"], "function_call_output");
    }

    #[test]
    fn to_openai_responses_input_merges_multiple_system_messages() {
        let conversation = Conversation {
            messages: vec![
                Message::system("First rule."),
                Message::system("Second rule."),
                Message::user("Hello"),
            ],
        };

        let (instructions, input) = conversation.to_openai_responses_input();

        assert_eq!(
            instructions,
            Some(
                "First rule.

Second rule."
                    .to_string()
            )
        );
        assert_eq!(
            input,
            vec![json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "Hello"}]
            })]
        );
    }

    // ── token_estimate ────────────────────────────────────────────────────────

    #[test]
    fn token_estimate_counts_text_tool_inputs_and_results() {
        let conversation = Conversation {
            messages: vec![
                Message::system("skip role but count text"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::text("abcd"),
                        ContentBlock::ToolUse {
                            id: "call_1".into(),
                            name: "weather".into(),
                            input: json!({"city": "Paris"}),
                        },
                    ],
                    metadata: None,
                },
                Message {
                    role: Role::User,
                    content: vec![ContentBlock::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: vec![ContentBlock::text("1234"), ContentBlock::text("5678")],
                        is_error: false,
                    }],
                    metadata: None,
                },
            ],
        };

        let expected_chars = "skip role but count text".len()
            + "abcd".len()
            + "weather".len()
            + json!({"city": "Paris"}).to_string().len()
            + "1234".len()
            + "5678".len();
        assert_eq!(conversation.token_estimate(), expected_chars / 4);
    }

    #[test]
    fn token_estimate_nonzero_for_nonempty() {
        let mut c = Conversation::new();
        c.push(Message::user("This is a test message."));
        assert!(c.token_estimate() > 0);
    }

    // ── Dangling tool-call sanitization ───────────────────────────────────────

    fn assistant_tool_use(id: &str, name: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::ToolUse {
                id: id.into(),
                name: name.into(),
                input: json!({}),
            }],
            metadata: None,
        }
    }

    fn user_tool_result(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                content: vec![ContentBlock::text("ok")],
                is_error: false,
            }],
            metadata: None,
        }
    }

    /// Collect `(tool_use_id, is_synthesized)` for every `ToolResult` block.
    fn tool_results(c: &Conversation) -> Vec<(String, bool)> {
        c.messages
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    ..
                } => {
                    let synth = content
                        .iter()
                        .filter_map(|c| c.as_text())
                        .any(|t| t.contains("tool execution interrupted"));
                    Some((tool_use_id.clone(), synth))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn sanitize_dangling_tail_synthesizes_result() {
        // Transcript ending in an unanswered tool call (session killed mid-tool).
        let convo = Conversation {
            messages: vec![
                Message::user("do it"),
                assistant_tool_use("read:37", "read"),
            ],
        };

        let repaired = convo.with_synthesized_tool_results();
        assert!(matches!(repaired, std::borrow::Cow::Owned(_)));

        // A synthesized result now answers the dangling id...
        let results = tool_results(&repaired);
        assert_eq!(results, vec![("read:37".to_string(), true)]);

        // ...carried by a user turn immediately after the assistant tool call,
        // so the pairing invariant holds for every wire format.
        let msgs = &repaired.messages;
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1].role, Role::Assistant);
        assert_eq!(msgs[2].role, Role::User);
        assert!(matches!(
            msgs[2].content[0],
            ContentBlock::ToolResult { .. }
        ));

        // The OpenAI chat serialization is now well-formed: the tool_calls
        // message is followed by a tool message for its id.
        let openai = repaired.to_openai_messages();
        let tool_msg = openai
            .iter()
            .find(|m| m["role"] == "tool")
            .expect("a tool message answers the call");
        assert_eq!(tool_msg["tool_call_id"], "read:37");
    }

    #[test]
    fn sanitize_leaves_well_formed_history_untouched() {
        let convo = Conversation {
            messages: vec![
                Message::user("do it"),
                assistant_tool_use("call_1", "read"),
                user_tool_result("call_1"),
                Message::assistant("done"),
            ],
        };

        let repaired = convo.with_synthesized_tool_results();
        // Borrowed (no allocation) — nothing to repair.
        assert!(matches!(repaired, std::borrow::Cow::Borrowed(_)));
        // No synthesized results were added.
        assert!(tool_results(&repaired).iter().all(|(_, synth)| !synth));
    }

    #[test]
    fn sanitize_multiple_dangling_ids_in_one_message() {
        let convo = Conversation {
            messages: vec![
                Message::user("do them"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::text("running two tools"),
                        ContentBlock::ToolUse {
                            id: "code_search:24".into(),
                            name: "code_search".into(),
                            input: json!({}),
                        },
                        ContentBlock::ToolUse {
                            id: "read:37".into(),
                            name: "read".into(),
                            input: json!({}),
                        },
                    ],
                    metadata: None,
                },
            ],
        };

        let repaired = convo.with_synthesized_tool_results();
        let mut results = tool_results(&repaired);
        results.sort();
        assert_eq!(
            results,
            vec![
                ("code_search:24".to_string(), true),
                ("read:37".to_string(), true),
            ]
        );
    }

    #[test]
    fn sanitize_partial_results_only_fills_missing() {
        // Assistant issued two calls; only one result was persisted before the
        // kill. The following user turn already carries the answered result.
        let convo = Conversation {
            messages: vec![
                Message::user("do them"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentBlock::ToolUse {
                            id: "answered".into(),
                            name: "read".into(),
                            input: json!({}),
                        },
                        ContentBlock::ToolUse {
                            id: "dangling".into(),
                            name: "code_search".into(),
                            input: json!({}),
                        },
                    ],
                    metadata: None,
                },
                user_tool_result("answered"),
            ],
        };

        let repaired = convo.with_synthesized_tool_results();
        let mut results = tool_results(&repaired);
        results.sort();
        assert_eq!(
            results,
            vec![
                ("answered".to_string(), false),
                ("dangling".to_string(), true),
            ]
        );
        // Merged into the existing user turn — no extra message inserted, so
        // Anthropic's user/assistant alternation is preserved.
        assert_eq!(repaired.messages.len(), 3);
        assert_eq!(repaired.messages[2].role, Role::User);
    }

    #[test]
    fn sanitize_merges_into_following_user_turn_for_anthropic() {
        // Dangling call followed by a resume nudge (plain user text). The synth
        // result must fold into that same user turn, not create a second
        // consecutive user message (which Anthropic rejects).
        let convo = Conversation {
            messages: vec![
                assistant_tool_use("read:37", "read"),
                Message::user("Continue with the task."),
            ],
        };

        let repaired = convo.with_synthesized_tool_results();
        assert_eq!(repaired.messages.len(), 2);
        assert_eq!(repaired.messages[0].role, Role::Assistant);
        assert_eq!(repaired.messages[1].role, Role::User);
        // Tool result leads the user turn (Anthropic requirement), text follows.
        assert!(matches!(
            repaired.messages[1].content[0],
            ContentBlock::ToolResult { .. }
        ));

        // Anthropic serialization: exactly one user message, tool_result first.
        let (_system, msgs) = repaired.to_anthropic_messages();
        let user_turns: Vec<_> = msgs.iter().filter(|m| m["role"] == "user").collect();
        assert_eq!(user_turns.len(), 1);
        assert_eq!(user_turns[0]["content"][0]["type"], "tool_result");
    }

    // ── Thinking signature / redacted / passthrough serde ─────────────────────

    #[test]
    fn old_unsigned_thinking_deserializes_with_none_signature() {
        let json = json!({"type": "thinking", "thinking": "hello world"});
        let block: ContentBlock = serde_json::from_value(json).unwrap();
        match block {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "hello world");
                assert_eq!(signature, None);
            }
            _ => panic!("expected Thinking variant"),
        }
    }

    #[test]
    fn signed_thinking_round_trips_through_serde() {
        let block = ContentBlock::Thinking {
            thinking: "secret reasoning".into(),
            signature: Some("sig_abc123".into()),
        };
        let serialized = serde_json::to_value(&block).unwrap();
        assert_eq!(
            serialized,
            json!({
                "type": "thinking",
                "thinking": "secret reasoning",
                "signature": "sig_abc123"
            })
        );
        let deserialized: ContentBlock = serde_json::from_value(serialized).unwrap();
        assert_eq!(block, deserialized);
    }

    #[test]
    fn unsigned_thinking_omits_signature_on_serialize() {
        let block = ContentBlock::Thinking {
            thinking: "no sig".into(),
            signature: None,
        };
        let serialized = serde_json::to_value(&block).unwrap();
        assert!(serialized.get("signature").is_none());
        assert_eq!(serialized["thinking"], "no sig");
    }

    #[test]
    fn redacted_thinking_round_trips_preserving_data() {
        let block = ContentBlock::RedactedThinking {
            data: "opaque_base64_blob==".into(),
        };
        let serialized = serde_json::to_value(&block).unwrap();
        assert_eq!(
            serialized,
            json!({
                "type": "redacted_thinking",
                "data": "opaque_base64_blob=="
            })
        );
        let deserialized: ContentBlock = serde_json::from_value(serialized).unwrap();
        assert_eq!(block, deserialized);
    }

    #[test]
    fn unknown_passthrough_round_trips_preserving_extra_fields() {
        let mut extra = serde_json::Map::new();
        extra.insert("foo".into(), json!("bar"));
        extra.insert("num".into(), json!(42));
        let block = ContentBlock::Unknown {
            content_type: "custom_provider_block".into(),
            extra,
        };
        let serialized = serde_json::to_value(&block).unwrap();
        assert_eq!(serialized["type"], "unknown");
        assert_eq!(serialized["foo"], "bar");
        assert_eq!(serialized["num"], 42);
        let deserialized: ContentBlock = serde_json::from_value(serialized).unwrap();
        assert_eq!(block, deserialized);
    }

    #[test]
    fn unknown_passthrough_preserves_nested_json_in_extra() {
        let mut extra = serde_json::Map::new();
        extra.insert("nested".into(), json!({"a": [1, 2, {"b": true}]}));
        let block = ContentBlock::Unknown {
            content_type: "complex".into(),
            extra,
        };
        let serialized = serde_json::to_value(&block).unwrap();
        let deserialized: ContentBlock = serde_json::from_value(serialized).unwrap();
        assert_eq!(block, deserialized);
    }

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
        assert_eq!(
            content[0],
            json!({"type": "text", "text": "visible output"})
        );
        // Must not contain an empty-text placeholder.
        assert!(
            !content
                .iter()
                .any(|b| b["type"] == "text" && b["text"] == "")
        );
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
        assert_eq!(
            content[0],
            json!({"type": "text", "text": "visible output"})
        );
        assert!(
            !content
                .iter()
                .any(|b| b["type"] == "text" && b["text"] == "")
        );
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
        assert_eq!(
            content[0],
            json!({"type": "text", "text": "visible output"})
        );
        assert!(
            !content
                .iter()
                .any(|b| b["type"] == "text" && b["text"] == "")
        );
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
        assert_eq!(
            content[0],
            json!({"type": "text", "text": "visible output"})
        );
        assert!(
            !content
                .iter()
                .any(|b| b["type"] == "text" && b["text"] == "")
        );
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

    // ── OpenAI Responses: skip Anthropic-oriented provider-internal blocks ──

    /// OpenAI Responses serialization (`to_openai_responses_input`) must skip
    /// the shared Anthropic-oriented variants (Thinking, RedactedThinking,
    /// Unknown) and OpenAIReasoning rather than emitting empty `output_text` /
    /// `input_text` items. This guards the shared `ContentBlock` expansion from
    /// regressing non-Anthropic request content: the newly shared variants must
    /// not become empty text blocks or otherwise alter the established
    /// Responses input shape.
    #[test]
    fn openai_responses_serialization_skips_thinking_redacted_unknown_blocks() {
        let mut c = Conversation::new();
        c.push(Message {
            role: Role::Assistant,
            content: vec![
                ContentBlock::Thinking {
                    thinking: "internal reasoning".into(),
                    signature: Some("sig_abc".into()),
                },
                ContentBlock::RedactedThinking {
                    data: "opaque_data_blob".into(),
                },
                ContentBlock::Unknown {
                    content_type: "custom_provider_block".into(),
                    extra: {
                        let mut m = serde_json::Map::new();
                        m.insert("foo".into(), json!("bar"));
                        m
                    },
                },
                ContentBlock::text("visible output"),
            ],
            metadata: None,
        });

        let (_, input_items) = c.to_openai_responses_input();
        assert_eq!(input_items.len(), 1);
        assert_eq!(input_items[0]["role"], "assistant");
        let content = input_items[0]["content"].as_array().unwrap();
        // Only the single visible text block should survive as output_text.
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "output_text");
        assert_eq!(content[0]["text"], "visible output");
    }

    /// When all content blocks are provider-internal, the Responses input must
    /// not synthesize an empty-text item for the shared Anthropic variants.
    /// Guards against the empty-text fallback being reintroduced for the
    /// Responses path.
    #[test]
    fn openai_responses_serialization_drops_all_internal_blocks_without_empty_text() {
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
                ContentBlock::Unknown {
                    content_type: "custom".into(),
                    extra: serde_json::Map::new(),
                },
            ],
            metadata: None,
        });

        let (_, input_items) = c.to_openai_responses_input();
        // No items emitted at all — no empty-text placeholder.
        assert_eq!(input_items.len(), 0);
        // Explicitly assert none of the emitted items are empty output_text.
        assert!(input_items.iter().all(|item| {
            item.get("content")
                .and_then(|c| c.as_array())
                .map(|arr| {
                    !arr.iter().any(|b| {
                        b.get("type") == Some(&json!("output_text"))
                            && b.get("text") == Some(&json!(""))
                    })
                })
                .unwrap_or(true)
        }));
    }

    /// Unsigned thinking (no signature) is also skipped by the Responses path.
    /// This covers the historical-JSON deserialization shape once round-tripped
    /// through storage and presented to the Responses serializer.
    #[test]
    fn openai_responses_serialization_skips_unsigned_thinking_block() {
        // Simulate a historical unsigned thinking block coming back from DB.
        let raw = json!({"type": "thinking", "thinking": "legacy reasoning"});
        let block: ContentBlock = serde_json::from_value(raw).unwrap();
        let mut c = Conversation::new();
        c.push(Message {
            role: Role::Assistant,
            content: vec![block, ContentBlock::text("visible output")],
            metadata: None,
        });

        let (_, input_items) = c.to_openai_responses_input();
        assert_eq!(input_items.len(), 1);
        let content = input_items[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "output_text");
        assert_eq!(content[0]["text"], "visible output");
    }

    // ── Shared-schema round-trip through full JSON strings ──────────────────

    /// All shared Anthropic-oriented variants round-trip through full JSON
    /// string serialization/deserialization (simulating DB storage) and preserve
    /// their shapes. This strengthens the individual serde tests by exercising
    /// the complete set together via `serde_json::to_string` / `from_str`.
    #[test]
    fn all_shared_thinking_variants_round_trip_through_json_string() {
        let blocks = vec![
            ContentBlock::Thinking {
                thinking: "signed reasoning".into(),
                signature: Some("sig_xyz".into()),
            },
            ContentBlock::Thinking {
                thinking: "unsigned reasoning".into(),
                signature: None,
            },
            ContentBlock::RedactedThinking {
                data: "opaque_blob==".into(),
            },
            ContentBlock::Unknown {
                content_type: "future_block".into(),
                extra: {
                    let mut m = serde_json::Map::new();
                    m.insert("key".into(), json!({"nested": [1, 2, 3]}));
                    m
                },
            },
        ];

        for original in &blocks {
            let json_str = serde_json::to_string(original).unwrap();
            let deserialized: ContentBlock = serde_json::from_str(&json_str).unwrap();
            assert_eq!(original, &deserialized, "round-trip failed for: {json_str}");
        }
    }

    /// Historical Thinking JSON that lacks `signature` (produced before the
    /// schema expansion) deserializes as `ContentBlock::Thinking` with
    /// `signature: None`, survives a full string round-trip, and remains the
    /// `Thinking` variant — never falling back to `Unknown`.
    #[test]
    fn legacy_unsigned_thinking_json_round_trips_and_stays_thinking_variant() {
        let legacy_json = r#"{"type":"thinking","thinking":"old stored reasoning"}"#;
        let block: ContentBlock = serde_json::from_str(legacy_json).unwrap();
        match &block {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "old stored reasoning");
                assert_eq!(signature, &None);
            }
            other => panic!("expected Thinking variant, got {other:?}"),
        }
        // Round-trip back through a JSON string.
        let re = serde_json::to_string(&block).unwrap();
        let again: ContentBlock = serde_json::from_str(&re).unwrap();
        assert_eq!(block, again);
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
}
