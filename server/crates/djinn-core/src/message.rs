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
    /// Stored for display but stripped when serializing back to provider APIs
    /// (providers that need round-tripping, like Anthropic extended thinking
    /// with signatures, are not yet supported).
    Thinking { thinking: String },
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
                ContentBlock::Thinking { thinking } => thinking.len(),
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
                    // Thinking blocks are display-only; not sent back to OpenAI.

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
                        .filter(|b| !is_thinking(b))
                        .map(content_block_to_anthropic)
                        .collect();
                    msgs.push(json!({"role": "user", "content": content}));
                }
                Role::Assistant => {
                    let content: Vec<serde_json::Value> = msg
                        .content
                        .iter()
                        .filter(|b| !is_thinking(b))
                        .map(content_block_to_anthropic)
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
                            ContentBlock::Document { media_type, data, .. } => {
                                vec![json!({"inlineData": {"mimeType": media_type, "data": data}})]
                            }
                            ContentBlock::Thinking { .. } => {
                                // Thinking blocks are display-only; skip for Google.
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
        let mut input_items: Vec<serde_json::Value> = Vec::new();
        let mut instructions: Option<String> = None;

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
                                text_items.push(json!({"type": "input_text", "text": text}));
                            }
                            ContentBlock::Image { media_type, data } => {
                                text_items.push(json!({
                                    "type": "input_image",
                                    "image_url": format!("data:{media_type};base64,{data}")
                                }));
                            }
                            ContentBlock::Document { data, media_type, filename } => {
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
                                    "output": output
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
                                text_items.push(json!({"type": "output_text", "text": text}));
                            }
                            ContentBlock::ToolUse { id, name, input } => {
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
}

// ─── Anthropic content-block helpers ─────────────────────────────────────────

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
        ContentBlock::Document { media_type, data, filename } => {
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
        ContentBlock::Thinking { .. } => json!({"type": "text", "text": ""}),
    }
}

/// Returns `true` if the block is a `Thinking` variant (used to filter
/// before serializing for provider APIs).
fn is_thinking(block: &ContentBlock) -> bool {
    matches!(block, ContentBlock::Thinking { .. })
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
        assert_eq!(input.len(), 9);
        assert_eq!(
            input[0],
            json!({
                "role": "user",
                "content": [{"type": "input_text", "text": "Need weather"}]
            })
        );
        assert_eq!(
            input[1],
            json!({
                "type": "function_call_output",
                "call_id": "orphan",
                "output": "Error: cached"
            })
        );
        assert_eq!(
            input[2],
            json!({
                "role": "user",
                "content": [{"type": "input_text", "text": " now"}]
            })
        );
        assert_eq!(
            input[3],
            json!({
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Checking."}]
            })
        );
        assert_eq!(
            input[4],
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "weather",
                "arguments": "{\"city\":\"Paris\"}"
            })
        );
        assert_eq!(
            input[5],
            json!({
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Done."}]
            })
        );
        assert_eq!(
            input[6],
            json!({
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "72F\n sunny"
            })
        );
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
}
