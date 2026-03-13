use std::convert::Infallible;

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
use crate::agent::message::{ContentBlock, Conversation, Message, Role};
use crate::agent::provider::{create_provider, StreamEvent};
use crate::db::{EpicCountQuery, EpicRepository, NoteRepository, ProjectRepository, TaskRepository};
use crate::mcp::server::DjinnMcpServer;
use crate::server::AppState;

const DJINN_CHAT_SYSTEM_PROMPT: &str = include_str!("../agent/prompts/chat.md");
const MAX_TOOL_ITERATIONS: usize = 20;

fn compose_system_prompt(base_prompt: &str, project_context: Option<&str>, client_system: Option<&str>) -> String {
    let mut system_prompt = base_prompt.trim().to_string();
    if let Some(project_context) = project_context.filter(|s| !s.trim().is_empty()) {
        system_prompt = format!("{system_prompt}\n\n{project_context}");
    }
    if let Some(client_system) = client_system.filter(|s| !s.trim().is_empty()) {
        system_prompt = format!("{system_prompt}\n\n{client_system}");
    }
    system_prompt
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

fn normalize_brief_excerpt(content: &str, max_chars: usize) -> String {
    let compact = content.split_whitespace().collect::<Vec<_>>().join(" ");
    if compact.chars().count() <= max_chars {
        return compact;
    }
    compact.chars().take(max_chars).collect::<String>()
}

async fn build_project_context_block(state: &AppState, project_ref: &str) -> Option<String> {
    let project_repo = ProjectRepository::new(state.db().clone(), state.events().clone());
    let project_id = match project_repo.resolve(project_ref).await {
        Ok(Some(id)) => id,
        Ok(None) => return None,
        Err(_) => return None,
    };

    let project = match project_repo.get(&project_id).await {
        Ok(Some(project)) => project,
        Ok(None) => return None,
        Err(_) => return None,
    };

    let epic_repo = EpicRepository::new(state.db().clone(), state.events().clone());
    let task_repo = TaskRepository::new(state.db().clone(), state.events().clone());
    let note_repo = NoteRepository::new(state.db().clone(), state.events().clone());

    let open_epics = epic_repo
        .count_grouped(EpicCountQuery {
            project_id: Some(project_id.clone()),
            status: Some("open".to_string()),
            group_by: None,
        })
        .await
        .ok()
        .and_then(|v| v.get("total_count").and_then(|n| n.as_i64()).map(|n| n.to_string()))
        .unwrap_or_else(|| "unknown".to_string());

    let open_tasks = task_repo
        .count_grouped(crate::db::CountQuery {
            project_id: Some(project_id.clone()),
            status: Some("open".to_string()),
            issue_type: None,
            priority: None,
            label: None,
            text: None,
            parent: None,
            group_by: None,
        })
        .await
        .ok()
        .and_then(|v| v.get("total_count").and_then(|n| n.as_i64()).map(|n| n.to_string()))
        .unwrap_or_else(|| "unknown".to_string());

    let brief = note_repo
        .get_by_permalink(&project_id, "brief")
        .await
        .ok()
        .flatten()
        .map(|note| normalize_brief_excerpt(&note.content, 200))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "No brief yet — suggest /init-project".to_string());

    Some(format!(
        "## Current Project\n**Name**: {}  **Path**: {}\n**Open epics**: {}  **Open tasks**: {}\n**Brief**: {}",
        project.name, project.path, open_epics, open_tasks, brief
    ))
}
fn sse_json_event<T: Serialize>(event: &str, payload: &T) -> Event {
    Event::default().event(event).json_data(payload).unwrap_or_else(|_| {
        Event::default()
            .event("error")
            .data("{\"message\":\"serialization error\"}")
    })
}

pub(super) async fn completions_handler(
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
    let project_context = if let Some(project_ref) = req.project.as_deref() {
        build_project_context_block(&state, project_ref).await
    } else {
        None
    };
    let system_prompt = compose_system_prompt(
        DJINN_CHAT_SYSTEM_PROMPT,
        project_context.as_deref(),
        req.system.as_deref(),
    );
    conversation.push(Message::system(system_prompt));

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

    let mcp = DjinnMcpServer::new(state.clone());
    let tool_schemas = mcp.all_tool_schemas();

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

                let args = serde_json::Value::Object(input.as_object().cloned().unwrap_or_default());
                match mcp.dispatch_tool(&name, args).await {
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

#[cfg(test)]
mod tests {
    use super::{compose_system_prompt, DJINN_CHAT_SYSTEM_PROMPT};
    use crate::mcp::server::DjinnMcpServer;
    use crate::server::AppState;
    use crate::test_helpers;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn test_mcp() -> DjinnMcpServer {
        let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
        DjinnMcpServer::new(state)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_tool_routes_task_family() {
        let mcp = test_mcp();
        let result = mcp.dispatch_tool("task_list", json!({"project": "/tmp/nonexistent", "issue_type": "task", "status": "open", "label": "", "text": "", "sort": "updated_at", "offset": 0, "limit": 10})).await;
        assert!(result.is_ok(), "dispatch_tool task_list returned error: {result:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_tool_routes_epic_family() {
        let mcp = test_mcp();
        let result = mcp.dispatch_tool("epic_list", json!({"project": "/tmp/nonexistent", "limit": 1})).await;
        assert!(result.is_ok(), "dispatch_tool epic_list returned error: {result:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_tool_routes_memory_family() {
        let mcp = test_mcp();
        let result = mcp
            .dispatch_tool("memory_search", json!({"project":"/tmp/nonexistent", "query":"x", "limit": 1}))
            .await;
        assert!(result.is_ok(), "dispatch_tool memory_search returned error: {result:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_tool_routes_settings_family() {
        let mcp = test_mcp();
        let result = mcp.dispatch_tool("settings_get", json!({})).await;
        assert!(result.is_ok(), "dispatch_tool settings_get returned error: {result:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_tool_routes_provider_family() {
        let mcp = test_mcp();
        let result = mcp.dispatch_tool("provider_catalog", json!({})).await;
        assert!(result.is_ok(), "dispatch_tool provider_catalog returned error: {result:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_tool_rejects_unknown_tool() {
        let mcp = test_mcp();
        let err = mcp
            .dispatch_tool("tool_that_does_not_exist", json!({}))
            .await
            .expect_err("unknown tool should fail");
        assert!(err.contains("unknown MCP tool"));
    }

    #[test]
    fn system_prompt_contains_base_prompt_first_and_project_block_before_client_system() {
        let project_context = "## Current Project\n**Name**: Demo  **Path**: /tmp/demo\n**Open epics**: 1  **Open tasks**: 2\n**Brief**: hello";
        let client_system = "client system message";
        let prompt = compose_system_prompt(
            DJINN_CHAT_SYSTEM_PROMPT,
            Some(project_context),
            Some(client_system),
        );

        let base = DJINN_CHAT_SYSTEM_PROMPT.trim();
        assert!(prompt.starts_with(base));
        let base_pos = prompt.find(base).unwrap();
        let project_pos = prompt.find("## Current Project").unwrap();
        let client_pos = prompt.find(client_system).unwrap();
        assert!(base_pos <= project_pos);
        assert!(project_pos < client_pos);
    }

    #[test]
    fn system_prompt_appends_client_system_after_internal_content() {
        let client_system = "be concise";
        let prompt = compose_system_prompt(DJINN_CHAT_SYSTEM_PROMPT, None, Some(client_system));
        assert!(prompt.starts_with(DJINN_CHAT_SYSTEM_PROMPT.trim()));
        assert!(prompt.ends_with(client_system));
    }
}
