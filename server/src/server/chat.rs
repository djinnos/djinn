use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::Sse;
use serde::{Deserialize, Serialize};

use crate::server::AppState;
mod compaction_boundary;
#[cfg(any(test, feature = "test-support"))]
pub mod handler;
#[cfg(not(any(test, feature = "test-support")))]
mod handler;
mod project_resolver;
mod prompt;

pub(super) use compaction_boundary::{
    complete_chat_compaction_boundary, record_chat_compaction_started,
};
pub(super) mod sessions;

use djinn_provider::message::Message;

pub(super) use project_resolver::{ProjectResolver, ProjectResolverError};

pub(super) const DJINN_CHAT_SYSTEM_PROMPT: &str =
    include_str!("../../crates/djinn-roles/src/prompts/chat.md");

/// System prompt overlay for a proposal-scoped chat ("Address with djinn").
/// `{{PROPOSAL_CONTEXT}}` is replaced with the rendered spec + unresolved
/// feedback before it's merged into the chat system message.
pub(super) const PROPOSAL_ADDRESS_SYSTEM_PROMPT: &str =
    include_str!("../../crates/djinn-roles/src/prompts/proposal_address.md");

/// Apply globally-configured chat skills to the base system message.
///
/// Chat is user-scoped and globally multi-project (the chat-user-global
/// refactor) — skills no longer resolve against a per-project environment
/// config.  Until a user-scoped `environment_config` surface lands we
/// pass through the base message untouched and return an empty resolved
/// config.  The per-project MCP-server inheritance was dropped at the
/// same cut-over; chat tool dispatch runs only the in-process chat
/// extension tools.
// TODO(multiuser): resolve `global_skills` + `agent_mcp_defaults`
// against a user/installation-level `environment_config` once the
// user-scoped env surface exists.  For now chat operates without
// skills or per-project MCP stdio stacks.
async fn apply_chat_skills(base_message: Message) -> (Message, ResolvedChatConfig) {
    (base_message, ResolvedChatConfig::default())
}

#[derive(Debug, Clone, Default)]
struct ResolvedChatConfig {
    #[allow(dead_code)]
    mcp_servers: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub system: Option<String>,
    /// Client-minted UUID identifying the chat session.  Required since
    /// cut-over to DB-backed chat sessions: each request upserts this
    /// id into `sessions(agent_type='chat', project_id=NULL)` before
    /// streaming, and persists both the incoming user turn and the
    /// assistant reply against it.
    pub session_id: String,
    /// Optional active project (slug or UUID).  When supplied AND the
    /// `DJINN_CHAT_AUTO_CODEBASE_HEADER` flag is on, the handler appends
    /// a `📦 CURRENT CODEBASE` block to the chat system prompt summarizing
    /// the warmed canonical graph + a depth-2 folder tree.  Skipped
    /// silently when omitted; never required.
    #[serde(default)]
    pub project: Option<String>,
    /// Optional proposal (UUID or short_id) this chat is scoped to — the
    /// "Address with djinn" flow. When present, the handler seeds the system
    /// prompt with the proposal spec + its unresolved feedback and grants the
    /// proposal-editing tool subset, so djinn can rewrite the spec (appending a
    /// revision) and resolve feedback. Requires an authenticated user.
    #[serde(default)]
    pub proposal_id: Option<String>,
    /// Optional feedback entry the chat is centered on (highlighted in the
    /// seeded context). Only meaningful alongside `proposal_id`.
    #[serde(default)]
    pub feedback_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub(super) struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: ChatContent,
}

/// Accepts either a plain string or an array of typed content blocks.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub(super) enum ChatContent {
    Blocks(Vec<ChatContentBlock>),
    Text(String),
}

impl Default for ChatContent {
    fn default() -> Self {
        ChatContent::Text(String::new())
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(super) enum ChatContentBlock {
    Text {
        text: String,
    },
    Image {
        media_type: String,
        data: String,
    },
    Document {
        media_type: String,
        data: String,
        #[serde(default)]
        filename: Option<String>,
    },
}

#[derive(Serialize)]
pub(super) struct ErrorPayload {
    message: String,
}

#[derive(Serialize)]
pub(super) struct DeltaPayload {
    text: String,
}

#[derive(Serialize)]
pub(super) struct ToolCallPayload {
    name: String,
    id: String,
    input: serde_json::Value,
}

#[derive(Serialize)]
pub(super) struct ToolResultPayload {
    id: String,
    output: String,
    elapsed_ms: u64,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

/// Emitted once, after the first assistant reply lands on a chat session
/// that still has the default "New Chat" title.  Precedes the `done`
/// event on the same SSE stream.
#[derive(Serialize)]
pub(super) struct SessionTitlePayload {
    pub session_id: String,
    pub title: String,
}

pub(super) async fn completions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<
    Sse<impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>>,
    (axum::http::StatusCode, String),
> {
    handler::completions_handler_impl(state, headers, req).await
}

#[cfg(test)]
mod tests;
