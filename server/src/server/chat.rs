use std::convert::Infallible;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, Sse};
use serde::{Deserialize, Serialize};

use crate::server::AppState;
mod context;
mod handler;
mod prompt;

pub(super) const DJINN_CHAT_SYSTEM_PROMPT: &str =
    include_str!("../../crates/djinn-agent/src/prompts/chat.md");

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
    Sse<impl futures::Stream<Item = Result<Event, Infallible>>>,
    (axum::http::StatusCode, String),
> {
    handler::completions_handler_impl(state, headers, req).await
}

#[cfg(test)]
mod tests;
