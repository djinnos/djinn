use std::convert::Infallible;
use std::path::PathBuf;

use axum::extract::State;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::Json;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;

use crate::actors::slot::{
    auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, load_provider_credential, parse_model_id, ProviderCredential,
};
use crate::agent::extension::{call_tool, chat_tool_schemas};
use crate::agent::message::{ContentBlock, Conversation, Message, Role};
use crate::agent::provider::{create_provider, StreamEvent};
use crate::server::AppState;

const MAX_TOOL_ITERATIONS: usize = 20;

#[derive(Debug, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub project: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: String,
}

#[derive(Serialize)]
struct ErrorPayload {
    message: String,
}

#[derive(Serialize)]
struct DeltaPayload {
    text: String,
}

#[derive(Serialize)]
struct ToolCallPayload {
    name: String,
    id: String,
}

#[derive(Serialize)]
struct ToolResultPayload {
    id: String,
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

fn sse_json_event<T: Serialize>(event: &str, payload: &T) -> Event {
    Event::default().event(event).json_data(payload).unwrap_or_else(|_| {
        Event::default()
            .event("error")
            .data("{\"message\":\"serialization error\"}")
    })
}

pub async fn completions_handler(
    State(state): State<AppState>,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<Sse<impl futures::Stream<Item = Result<Event, Infallible>>>, (axum::http::StatusCode, String)> {
    if req.model.trim().is_empty() {
        return Err((axum::http::StatusCode::BAD_REQUEST, "model is required".to_string()));
    }
    if req.messages.is_empty() {
        return Err((axum::http::StatusCode::BAD_REQUEST, "messages must not be empty".to_string()));
    }

    let (provider_id, model_name) = parse_model_id(&req.model)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, format!("invalid model: {e}")))?;

    let provider_known = state
        .catalog()
        .list_providers()
        .iter()
        .any(|p| p.id == provider_id);
    if !provider_known {
        tracing::warn!(provider=%provider_id, "unknown provider");
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            format!("unknown provider: {provider_id}"),
        ));
    }

    let resolved_model = state
        .catalog()
        .list_models(&provider_id)
        .iter()
        .find(|m| {
            let bare = m.id.rsplit('/').next().unwrap_or(&m.id);
            m.id == model_name || m.name == model_name || bare == model_name
        })
        .map(|m| m.id.clone())
        .unwrap_or(model_name);

    let context_window = state
        .catalog()
        .find_model(&req.model)
        .map(|m| m.context_window)
        .unwrap_or(0);

    let provider_credential = load_provider_credential(&provider_id, &state)
        .await
        .map_err(|e| {
            tracing::warn!(provider=%provider_id, error=%e, "provider credential resolution failed");
            (axum::http::StatusCode::BAD_REQUEST, format!("provider credential resolution failed: {e}"))
        })?;

    let provider_config = match provider_credential {
        ProviderCredential::OAuthConfig(mut cfg) => {
            cfg.model_id = resolved_model.clone();
            cfg.context_window = context_window.max(0) as u32;
            cfg.telemetry = None;
            cfg
        }
        ProviderCredential::ApiKey(_name, api_key) => {
            let base_url = state
                .catalog()
                .list_providers()
                .iter()
                .find(|p| p.id == provider_id)
                .map(|p| p.base_url.clone())
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| default_base_url(&provider_id));
            crate::agent::provider::ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&provider_id, &api_key),
                format_family: format_family_for_provider(&provider_id, &resolved_model),
                model_id: resolved_model,
                context_window: context_window.max(0) as u32,
                telemetry: None,
                provider_headers: Default::default(),
                capabilities: capabilities_for_provider(&provider_id),
            }
        }
    };

    let provider = create_provider(provider_config);

    let mut conversation = Conversation::new();
    if let Some(system) = req.system.filter(|s| !s.trim().is_empty()) {
        conversation.push(Message::system(system));
    }

    for m in req.messages {
        let role = match m.role.as_str() {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::User,
            _ => return Err((axum::http::StatusCode::BAD_REQUEST, format!("unsupported role: {}", m.role))),
        };
        conversation.push(Message {
            role,
            content: vec![ContentBlock::Text { text: m.content }],
            metadata: None,
        });
    }

    let tool_schemas = chat_tool_schemas()
        .into_iter()
        .filter_map(|t| serde_json::to_value(t).ok())
        .collect::<Vec<_>>();

    let worktree_path = req.project.map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));

    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    tokio::spawn(async move {
        let mut loop_count = 0usize;
        loop {
            if loop_count >= MAX_TOOL_ITERATIONS {
                tracing::warn!(max_iterations=MAX_TOOL_ITERATIONS, "chat tool loop cap reached");
                let _ = tx
                    .send(sse_json_event(
                        "error",
                        &ErrorPayload {
                            message: format!("tool loop iteration cap reached ({MAX_TOOL_ITERATIONS})"),
                        },
                    ))
                    .await;
                break;
            }

            let stream = match provider.stream(&conversation, &tool_schemas).await {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error=%e, "provider stream init failed");
                    let _ = tx
                        .send(sse_json_event(
                            "error",
                            &ErrorPayload {
                                message: format!("provider stream failed: {e}"),
                            },
                        ))
                        .await;
                    break;
                }
            };

            tokio::pin!(stream);
            let mut turn_text = String::new();
            let mut tool_calls: Vec<ContentBlock> = Vec::new();

            while let Some(item) = stream.next().await {
                match item {
                    Ok(StreamEvent::Delta(ContentBlock::Text { text })) => {
                        turn_text.push_str(&text);
                        let _ = tx.send(sse_json_event("delta", &DeltaPayload { text })).await;
                    }
                    Ok(StreamEvent::Delta(tool @ ContentBlock::ToolUse { .. })) => tool_calls.push(tool),
                    Ok(StreamEvent::Done) => break,
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error=%e, "provider stream event failed");
                        let _ = tx
                            .send(sse_json_event(
                                "error",
                                &ErrorPayload {
                                    message: format!("provider stream error: {e}"),
                                },
                            ))
                            .await;
                        return;
                    }
                }
            }

            let mut assistant_content = Vec::new();
            if !turn_text.is_empty() {
                assistant_content.push(ContentBlock::Text { text: turn_text });
            }
            assistant_content.extend(tool_calls.clone());
            if !assistant_content.is_empty() {
                conversation.push(Message {
                    role: Role::Assistant,
                    content: assistant_content,
                    metadata: None,
                });
            }

            if tool_calls.is_empty() {
                let _ = tx.send(Event::default().event("done").data("{}")).await;
                break;
            }

            loop_count += 1;
            let mut tool_results = Vec::new();
            for tool_call in tool_calls {
                let ContentBlock::ToolUse { id, name, input } = tool_call else { continue; };
                let _ = tx
                    .send(sse_json_event(
                        "tool_call",
                        &ToolCallPayload {
                            name: name.clone(),
                            id: id.clone(),
                        },
                    ))
                    .await;

                let args = input.as_object().cloned();
                match call_tool(&state, &name, args, &worktree_path).await {
                    Ok(value) => {
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: vec![ContentBlock::text(value.to_string())],
                            is_error: false,
                        });
                        let _ = tx
                            .send(sse_json_event(
                                "tool_result",
                                &ToolResultPayload {
                                    id,
                                    success: true,
                                    message: None,
                                },
                            ))
                            .await;
                    }
                    Err(e) => {
                        tracing::warn!(tool=%name, error=%e, "tool dispatch failed");
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: vec![ContentBlock::text(e.clone())],
                            is_error: true,
                        });
                        let _ = tx
                            .send(sse_json_event(
                                "tool_result",
                                &ToolResultPayload {
                                    id,
                                    success: false,
                                    message: Some(e),
                                },
                            ))
                            .await;
                    }
                }
            }
            if !tool_results.is_empty() {
                conversation.push(Message {
                    role: Role::User,
                    content: tool_results,
                    metadata: None,
                });
            }
        }
    });

    Ok(Sse::new(ReceiverStream::new(rx).map(Ok)).keep_alive(KeepAlive::default()))
}
