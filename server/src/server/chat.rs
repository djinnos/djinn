use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::Sse;
use serde::{Deserialize, Serialize};

use crate::server::AppState;
mod handler;
mod project_resolver;
mod prompt;

use djinn_provider::message::Message;

pub(super) use project_resolver::{ProjectResolver, ProjectResolverError};

pub(super) const DJINN_CHAT_SYSTEM_PROMPT: &str =
    include_str!("../../crates/djinn-agent/src/prompts/chat.md");

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
