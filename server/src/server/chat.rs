use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::Sse;
use serde::{Deserialize, Serialize};

use crate::server::AppState;
mod context;
mod handler;
mod prompt;

use djinn_provider::message::{ContentBlock, Message, Role};
use djinn_agent::verification::settings::{
    effective_mcp_server_names, effective_skill_names, load_settings,
};

pub(super) const DJINN_CHAT_SYSTEM_PROMPT: &str =
    include_str!("../../crates/djinn-agent/src/prompts/chat.md");

fn apply_chat_skills(
    base_message: Message,
    project_path: Option<&std::path::Path>,
) -> (Message, ResolvedChatConfig) {
    let Some(project_path) = project_path else {
        return (base_message, ResolvedChatConfig::default());
    };

    let settings = load_settings(project_path).unwrap_or_default();
    let effective_skills = effective_skill_names(&settings, &[]);
    let effective_mcp_servers = effective_mcp_server_names(&settings, "chat", None);
    let resolved_skills = djinn_agent::skills::load_skills(project_path, &effective_skills);

    let text = djinn_agent::prompts::apply_skills(&base_message.text_content(), &resolved_skills);
    (
        Message {
            role: Role::System,
            content: vec![ContentBlock::text(text)],
            metadata: base_message.metadata.clone(),
        },
        ResolvedChatConfig {
            mcp_servers: effective_mcp_servers,
        },
    )
}

#[derive(Debug, Clone, Default)]
struct ResolvedChatConfig {
    mcp_servers: Vec<String>,
}

#[cfg(test)]
fn chat_effective_config(project_path: &std::path::Path) -> ResolvedChatConfig {
    let settings = load_settings(project_path).unwrap_or_default();
    ResolvedChatConfig {
        mcp_servers: effective_mcp_server_names(&settings, "chat", None),
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
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
