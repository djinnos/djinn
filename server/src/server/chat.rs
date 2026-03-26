use std::collections::BTreeSet;
use std::convert::Infallible;
use std::path::Path;
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_stream::wrappers::ReceiverStream;

use crate::server::AppState;
use djinn_agent::actors::slot::{
    ProviderCredential, auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, load_provider_credential, parse_model_id,
};
use djinn_agent::message::{
    CacheBreakpoint, ContentBlock, Conversation, Message, MessageMeta, Role,
};
use djinn_agent::provider::{StreamEvent, create_provider};
use djinn_db::{
    EpicCountQuery, EpicRepository, NoteRepository, ProjectRepository, RepoMapCacheKey,
    RepoMapCacheRepository, TaskRepository,
};
use djinn_mcp::server::DjinnMcpServer;

use crate::repo_map::persist_repo_map_note;

const DJINN_CHAT_SYSTEM_PROMPT: &str = include_str!("../../crates/djinn-agent/src/prompts/chat.md");
const MAX_TOOL_ITERATIONS: usize = 20;
const REPO_MAP_SYSTEM_HEADER: &str = "## Repository Map";
const ANTHROPIC_CACHE_BREAKPOINT_KEY: &str = "anthropic_cache_breakpoint";

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptSegment {
    text: String,
    cache_breakpoint: bool,
}

fn prompt_segment(text: impl Into<String>) -> PromptSegment {
    PromptSegment {
        text: text.into(),
        cache_breakpoint: false,
    }
}

fn cached_prompt_segment(text: impl Into<String>) -> PromptSegment {
    PromptSegment {
        text: text.into(),
        cache_breakpoint: true,
    }
}

fn compose_system_prompt_segments(
    base_prompt: &str,
    project_context: Option<&str>,
    repo_map_context: Option<&str>,
    client_system: Option<&str>,
) -> Vec<PromptSegment> {
    let mut segments = vec![cached_prompt_segment(base_prompt.trim())];
    if let Some(project_context) = project_context.filter(|s| !s.trim().is_empty()) {
        segments.push(cached_prompt_segment(project_context));
    }
    if let Some(repo_map_context) = repo_map_context.filter(|s| !s.trim().is_empty()) {
        segments.push(cached_prompt_segment(repo_map_context));
    }
    if let Some(client_system) = client_system.filter(|s| !s.trim().is_empty()) {
        segments.push(prompt_segment(client_system));
    }
    segments
}

#[cfg(test)]
fn compose_system_prompt(
    base_prompt: &str,
    project_context: Option<&str>,
    repo_map_context: Option<&str>,
    client_system: Option<&str>,
) -> String {
    compose_system_prompt_segments(
        base_prompt,
        project_context,
        repo_map_context,
        client_system,
    )
    .into_iter()
    .map(|segment| segment.text)
    .collect::<Vec<_>>()
    .join("\n\n")
}

fn build_system_message(
    base_prompt: &str,
    project_context: Option<&str>,
    repo_map_context: Option<&str>,
    client_system: Option<&str>,
    model: &str,
) -> Message {
    let segments = compose_system_prompt_segments(
        base_prompt,
        project_context,
        repo_map_context,
        client_system,
    );
    let text = segments
        .iter()
        .map(|segment| segment.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");

    if model.starts_with("anthropic/") && segments.iter().any(|segment| segment.cache_breakpoint) {
        return Message::system_with_metadata(
            text,
            MessageMeta {
                input_tokens: None,
                output_tokens: None,
                timestamp: None,
                provider_data: Some(serde_json::json!({
                    ANTHROPIC_CACHE_BREAKPOINT_KEY: CacheBreakpoint {
                        kind: Some("stable_prefix".to_string()),
                    }
                })),
            },
        );
    }

    Message::system(text)
}

fn format_repo_map_block(rendered: &str, permalink: Option<&str>) -> String {
    match permalink {
        Some(permalink) => {
            format!("{REPO_MAP_SYSTEM_HEADER}\nSource note: memory://{permalink}\n{rendered}")
        }
        None => format!("{REPO_MAP_SYSTEM_HEADER}\n{rendered}"),
    }
}

async fn repo_commit_sha(state: &AppState, repo_path: &Path) -> Option<String> {
    let git = state.git_actor(repo_path).await.ok()?;
    let head = git.head_commit().await.ok()?;
    Some(head.sha)
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct RepoMapCompanionContext {
    companion_note_ids: Vec<String>,
}

fn unique_companion_note_ids<I>(companion_note_ids: I) -> Vec<String>
where
    I: IntoIterator,
    I::Item: AsRef<str>,
{
    companion_note_ids
        .into_iter()
        .map(|id| id.as_ref().trim().to_string())
        .filter(|id| !id.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

async fn reinforce_repo_map_companion_notes(
    note_repo: &NoteRepository,
    repo_map_note_id: Option<&str>,
    companion_note_ids: &[String],
) {
    let Some(repo_map_note_id) = repo_map_note_id else {
        return;
    };
    if companion_note_ids.is_empty() {
        return;
    }

    let _ = note_repo
        .record_repo_map_co_access(repo_map_note_id, companion_note_ids.iter().cloned())
        .await;
}

async fn repo_map_companion_context(state: &AppState, project_id: &str) -> RepoMapCompanionContext {
    let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());
    let companion_note_ids = note_repo
        .get_by_permalink(project_id, "brief")
        .await
        .ok()
        .flatten()
        .map(|note| vec![note.id])
        .unwrap_or_default();

    RepoMapCompanionContext {
        companion_note_ids: unique_companion_note_ids(companion_note_ids),
    }
}

async fn build_repo_map_context_block(
    state: &AppState,
    project_ref: &str,
    companion_note_ids: &[String],
) -> Option<String> {
    let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());
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

    let commit_sha = repo_commit_sha(state, Path::new(&project.path)).await?;
    let repo_map_repo = RepoMapCacheRepository::new(state.db().clone());
    let cached = repo_map_repo
        .get(RepoMapCacheKey {
            project_id: &project.id,
            project_path: &project.path,
            worktree_path: None,
            commit_sha: &commit_sha,
        })
        .await
        .ok()
        .flatten()?;

    let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());
    let note = persist_repo_map_note(
        &note_repo,
        &project.id,
        &commit_sha,
        &crate::repo_map::RenderedRepoMap {
            content: cached.rendered_map.clone(),
            token_estimate: cached.token_estimate as usize,
            included_entries: cached.included_entries as usize,
        },
    )
    .await
    .ok();

    reinforce_repo_map_companion_notes(
        &note_repo,
        note.as_ref().map(|note| note.id.as_str()),
        companion_note_ids,
    )
    .await;

    Some(format_repo_map_block(
        &cached.rendered_map,
        note.as_ref().map(|note| note.permalink.as_str()),
    ))
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
    input: serde_json::Value,
}

#[derive(Serialize)]
struct ToolResultPayload {
    id: String,
    output: String,
    elapsed_ms: u64,
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
    let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());
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

    let epic_repo = EpicRepository::new(state.db().clone(), state.event_bus());
    let task_repo = TaskRepository::new(state.db().clone(), state.event_bus());
    let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());

    let open_epics = epic_repo
        .count_grouped(EpicCountQuery {
            project_id: Some(project_id.clone()),
            status: Some("open".to_string()),
            group_by: None,
        })
        .await
        .ok()
        .and_then(|v| {
            v.get("total_count")
                .and_then(|n| n.as_i64())
                .map(|n| n.to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());

    let open_tasks = task_repo
        .count_grouped(djinn_db::CountQuery {
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
        .and_then(|v| {
            v.get("total_count")
                .and_then(|n| n.as_i64())
                .map(|n| n.to_string())
        })
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
    Event::default()
        .event(event)
        .json_data(payload)
        .unwrap_or_else(|_| {
            Event::default()
                .event("error")
                .data("{\"message\":\"serialization error\"}")
        })
}

pub(super) async fn completions_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<ChatCompletionRequest>,
) -> Result<
    Sse<impl futures::Stream<Item = Result<Event, Infallible>>>,
    (axum::http::StatusCode, String),
> {
    if req.model.trim().is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "model is required".to_string(),
        ));
    }
    if req.messages.is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "messages must not be empty".to_string(),
        ));
    }

    let (provider_id, model_name) = parse_model_id(&req.model).map_err(|e| {
        (
            axum::http::StatusCode::BAD_REQUEST,
            format!("invalid model: {e}"),
        )
    })?;

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

    let provider_credential = load_provider_credential(&provider_id, &state.agent_context())
        .await
        .map_err(|e| {
            tracing::warn!(provider=%provider_id, error=%e, "provider credential resolution failed");
            (axum::http::StatusCode::BAD_REQUEST, format!("provider credential resolution failed: {e}"))
        })?;

    let session_id = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());

    let provider_config = match provider_credential {
        ProviderCredential::OAuthConfig(mut cfg) => {
            cfg.model_id = resolved_model.clone();
            cfg.context_window = context_window.max(0) as u32;
            cfg.telemetry = None;
            cfg.session_affinity_key = Some(session_id.clone());
            *cfg
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
            djinn_agent::provider::ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&provider_id, &api_key),
                format_family: format_family_for_provider(&provider_id, &resolved_model),
                model_id: resolved_model,
                context_window: context_window.max(0) as u32,
                telemetry: None,
                session_affinity_key: Some(session_id.clone()),
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
    let repo_map_context = if let Some(project_ref) = req.project.as_deref() {
        let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());
        let companion_context = match project_repo.resolve(project_ref).await {
            Ok(Some(project_id)) => repo_map_companion_context(&state, &project_id).await,
            _ => RepoMapCompanionContext::default(),
        };
        build_repo_map_context_block(&state, project_ref, &companion_context.companion_note_ids)
            .await
    } else {
        None
    };
    let system_message = build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        project_context.as_deref(),
        repo_map_context.as_deref(),
        req.system.as_deref(),
        &req.model,
    );
    conversation.push(system_message);

    for m in req.messages {
        let role = match m.role.as_str() {
            "system" => Role::System,
            "user" => Role::User,
            "assistant" => Role::Assistant,
            "tool" => Role::User,
            _ => {
                return Err((
                    axum::http::StatusCode::BAD_REQUEST,
                    format!("unsupported role: {}", m.role),
                ));
            }
        };
        conversation.push(Message {
            role,
            content: vec![ContentBlock::Text { text: m.content }],
            metadata: None,
        });
    }

    let mcp = DjinnMcpServer::new(state.mcp_state());
    let tool_schemas = mcp.all_tool_schemas();

    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    tokio::spawn(async move {
        let mut loop_count = 0usize;
        loop {
            if loop_count >= MAX_TOOL_ITERATIONS {
                tracing::warn!(
                    max_iterations = MAX_TOOL_ITERATIONS,
                    "chat tool loop cap reached"
                );
                let _ = tx
                    .send(sse_json_event(
                        "error",
                        &ErrorPayload {
                            message: format!(
                                "tool loop iteration cap reached ({MAX_TOOL_ITERATIONS})"
                            ),
                        },
                    ))
                    .await;
                break;
            }

            let stream = match provider
                .stream(
                    &conversation,
                    &tool_schemas,
                    Some(djinn_provider::provider::ToolChoice::Required),
                )
                .await
            {
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
                        let _ = tx
                            .send(sse_json_event("delta", &DeltaPayload { text }))
                            .await;
                    }
                    Ok(StreamEvent::Delta(tool @ ContentBlock::ToolUse { .. })) => {
                        tool_calls.push(tool)
                    }
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
                let ContentBlock::ToolUse { id, name, input } = tool_call else {
                    continue;
                };
                let _ = tx
                    .send(sse_json_event(
                        "tool_call",
                        &ToolCallPayload {
                            name: name.clone(),
                            id: id.clone(),
                            input: input.clone(),
                        },
                    ))
                    .await;

                let args =
                    serde_json::Value::Object(input.as_object().cloned().unwrap_or_default());
                let started_at = Instant::now();
                match mcp.dispatch_tool(&name, args).await {
                    Ok(value) => {
                        let output = value.to_string();
                        let elapsed_ms = started_at.elapsed().as_millis() as u64;
                        tool_results.push(ContentBlock::ToolResult {
                            tool_use_id: id.clone(),
                            content: vec![ContentBlock::text(output.clone())],
                            is_error: false,
                        });
                        let _ = tx
                            .send(sse_json_event(
                                "tool_result",
                                &ToolResultPayload {
                                    id,
                                    output,
                                    elapsed_ms,
                                    success: true,
                                    message: None,
                                },
                            ))
                            .await;
                    }
                    Err(e) => {
                        let elapsed_ms = started_at.elapsed().as_millis() as u64;
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
                                    output: e.clone(),
                                    elapsed_ms,
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
    use super::{
        ANTHROPIC_CACHE_BREAKPOINT_KEY, DJINN_CHAT_SYSTEM_PROMPT, REPO_MAP_SYSTEM_HEADER,
        ToolCallPayload, build_system_message, compose_system_prompt,
        compose_system_prompt_segments, format_repo_map_block, reinforce_repo_map_companion_notes,
        repo_map_companion_context, sse_json_event, unique_companion_note_ids,
    };
    use crate::server::AppState;
    use crate::test_helpers;
    use djinn_db::NoteRepository;
    use djinn_mcp::server::DjinnMcpServer;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn test_mcp() -> DjinnMcpServer {
        let state = AppState::new(test_helpers::create_test_db(), CancellationToken::new());
        DjinnMcpServer::new(state.mcp_state())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_tool_routes_task_family() {
        let mcp = test_mcp();
        let result = mcp.dispatch_tool("task_list", json!({"project": "/tmp/nonexistent", "issue_type": "task", "status": "open", "label": "", "text": "", "sort": "updated_at", "offset": 0, "limit": 10})).await;
        assert!(
            result.is_ok(),
            "dispatch_tool task_list returned error: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_tool_routes_epic_family() {
        let mcp = test_mcp();
        let result = mcp
            .dispatch_tool(
                "epic_list",
                json!({"project": "/tmp/nonexistent", "limit": 1}),
            )
            .await;
        assert!(
            result.is_ok(),
            "dispatch_tool epic_list returned error: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_tool_routes_memory_family() {
        let mcp = test_mcp();
        let result = mcp
            .dispatch_tool(
                "memory_search",
                json!({"project":"/tmp/nonexistent", "query":"x", "limit": 1}),
            )
            .await;
        assert!(
            result.is_ok(),
            "dispatch_tool memory_search returned error: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_tool_routes_settings_family() {
        let mcp = test_mcp();
        let result = mcp.dispatch_tool("settings_get", json!({})).await;
        assert!(
            result.is_ok(),
            "dispatch_tool settings_get returned error: {result:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dispatch_tool_routes_provider_family() {
        let mcp = test_mcp();
        let result = mcp.dispatch_tool("provider_catalog", json!({})).await;
        assert!(
            result.is_ok(),
            "dispatch_tool provider_catalog returned error: {result:?}"
        );
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
    fn unique_companion_note_ids_deduplicates_and_drops_empty_values() {
        let ids = unique_companion_note_ids(["note-b", "", "note-a", "note-b", "  note-a  "]);
        assert_eq!(ids, vec!["note-a".to_string(), "note-b".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reinforce_repo_map_companion_notes_records_one_association_per_unique_pair() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), test_helpers::test_events());

        let repo_map_note = note_repo
            .upsert_db_note_by_permalink(
                &project.id,
                "reference/repo-maps/repository-map-deadbeef",
                "Repository Map deadbeef",
                "src/lib.rs",
                "repo_map",
                "[]",
            )
            .await
            .expect("repo map note persists");
        let companion = note_repo
            .upsert_db_note_by_permalink(
                &project.id,
                "references/companion-note",
                "companion note",
                "body",
                "reference",
                "[]",
            )
            .await
            .expect("companion note persists");

        reinforce_repo_map_companion_notes(
            &note_repo,
            Some(&repo_map_note.id),
            &[companion.id.clone(), companion.id.clone()],
        )
        .await;

        let associations = note_repo
            .get_associations_for_note(&repo_map_note.id)
            .await
            .expect("associations load");
        assert_eq!(associations.len(), 1);
        assert_eq!(associations[0].co_access_count, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reinforce_repo_map_companion_notes_is_noop_without_companions_or_repo_map_note() {
        let db = test_helpers::create_test_db();
        let project = test_helpers::create_test_project(&db).await;
        let note_repo = NoteRepository::new(db.clone(), test_helpers::test_events());

        let repo_map_note = note_repo
            .upsert_db_note_by_permalink(
                &project.id,
                "reference/repo-maps/repository-map-deadbeef",
                "Repository Map deadbeef",
                "src/lib.rs",
                "repo_map",
                "[]",
            )
            .await
            .expect("repo map note persists");

        reinforce_repo_map_companion_notes(&note_repo, Some(&repo_map_note.id), &[]).await;
        reinforce_repo_map_companion_notes(&note_repo, None, &["note-1".to_string()]).await;

        let associations = note_repo
            .get_associations_for_note(&repo_map_note.id)
            .await
            .expect("associations load");
        assert!(associations.is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repo_map_companion_context_uses_brief_note_when_present() {
        let state = test_helpers::test_app_state_in_memory().await;
        let project = test_helpers::create_test_project(state.db()).await;
        let note_repo = NoteRepository::new(state.db().clone(), state.event_bus());

        let brief = note_repo
            .upsert_db_note_by_permalink(
                &project.id,
                "brief",
                "brief",
                "project brief body",
                "reference",
                "[]",
            )
            .await
            .expect("brief note persists");

        let context = repo_map_companion_context(&state, &project.id).await;
        assert_eq!(context.companion_note_ids, vec![brief.id]);
    }

    #[test]
    fn system_prompt_repo_map_block_is_unchanged_when_reinforcement_is_unavailable() {
        let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);
        let prompt = compose_system_prompt(DJINN_CHAT_SYSTEM_PROMPT, None, Some(&repo_map), None);
        assert!(prompt.contains(REPO_MAP_SYSTEM_HEADER));
        assert!(prompt.contains("src/lib.rs"));
    }

    #[test]
    fn system_prompt_contains_base_prompt_first_and_project_block_before_repo_map_and_client_system()
     {
        let project_context = "## Current Project\n**Name**: Demo  **Path**: /tmp/demo\n**Open epics**: 1  **Open tasks**: 2\n**Brief**: hello";
        let repo_map = format_repo_map_block("src/main.rs\n  fn main()", None);
        let client_system = "client system message";
        let prompt = compose_system_prompt(
            DJINN_CHAT_SYSTEM_PROMPT,
            Some(project_context),
            Some(&repo_map),
            Some(client_system),
        );

        let base = DJINN_CHAT_SYSTEM_PROMPT.trim();
        assert!(prompt.starts_with(base));
        let base_pos = prompt.find(base).unwrap();
        let project_pos = prompt.find("## Current Project").unwrap();
        let repo_map_pos = prompt.find(REPO_MAP_SYSTEM_HEADER).unwrap();
        let client_pos = prompt.find(client_system).unwrap();
        assert!(base_pos <= project_pos);
        assert!(project_pos < repo_map_pos);
        assert!(repo_map_pos < client_pos);
    }

    #[test]
    fn system_prompt_segments_mark_stable_project_and_repo_map_context_for_caching() {
        let project_context = "## Current Project\nproject";
        let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);
        let client_system = "be concise";

        let segments = compose_system_prompt_segments(
            DJINN_CHAT_SYSTEM_PROMPT,
            Some(project_context),
            Some(&repo_map),
            Some(client_system),
        );

        assert_eq!(segments.len(), 4);
        assert!(segments[0].cache_breakpoint);
        assert_eq!(segments[1].text, project_context);
        assert!(segments[1].cache_breakpoint);
        assert_eq!(segments[2].text, repo_map);
        assert!(segments[2].cache_breakpoint);
        assert_eq!(segments[3].text, client_system);
        assert!(!segments[3].cache_breakpoint);
    }

    #[test]
    fn build_system_message_adds_anthropic_cache_breakpoint_for_stable_prefix() {
        let project_context = "## Current Project\nproject";
        let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);
        let message = build_system_message(
            DJINN_CHAT_SYSTEM_PROMPT,
            Some(project_context),
            Some(&repo_map),
            Some("volatile client system"),
            "anthropic/claude-3-5-sonnet",
        );

        let provider_data = message
            .metadata
            .as_ref()
            .and_then(|meta| meta.provider_data.as_ref())
            .expect("anthropic message should include provider metadata");
        assert!(provider_data.get(ANTHROPIC_CACHE_BREAKPOINT_KEY).is_some());
        assert!(message.text_content().contains(REPO_MAP_SYSTEM_HEADER));
        assert!(message.text_content().contains("volatile client system"));
    }

    #[test]
    fn build_system_message_skips_cache_breakpoint_for_non_anthropic_or_without_repo_map() {
        let openai_message = build_system_message(
            DJINN_CHAT_SYSTEM_PROMPT,
            Some("## Current Project\nproject"),
            Some(&format_repo_map_block("src/lib.rs", None)),
            None,
            "openai/gpt-4o",
        );
        assert!(openai_message.metadata.is_none());

        let anthropic_without_project_or_repo = build_system_message(
            DJINN_CHAT_SYSTEM_PROMPT,
            None,
            None,
            Some("volatile client system"),
            "anthropic/claude-3-5-sonnet",
        );
        assert!(anthropic_without_project_or_repo.metadata.is_some());
        assert!(
            anthropic_without_project_or_repo
                .text_content()
                .contains("volatile client system")
        );
    }

    #[test]
    fn system_prompt_includes_repo_map_block_when_available() {
        let repo_map = format_repo_map_block("src/lib.rs\n  pub fn run", None);
        let prompt = compose_system_prompt(DJINN_CHAT_SYSTEM_PROMPT, None, Some(&repo_map), None);
        assert!(prompt.contains(REPO_MAP_SYSTEM_HEADER));
        assert!(prompt.contains("src/lib.rs"));
    }

    #[test]
    fn tool_call_sse_payload_includes_id_input_and_name() {
        let payload = ToolCallPayload {
            name: "task_list".to_string(),
            id: "call-123".to_string(),
            input: json!({"project": "/tmp/demo", "limit": 10}),
        };

        let event = sse_json_event("tool_call", &payload);
        let serialized = format!("{event:?}");

        assert!(serialized.contains("event: tool_call"));

        let value = serde_json::to_value(payload).expect("payload serializes");
        assert_eq!(
            value.get("name").and_then(|v| v.as_str()),
            Some("task_list")
        );
        assert_eq!(value.get("id").and_then(|v| v.as_str()), Some("call-123"));
        assert_eq!(
            value.get("input"),
            Some(&json!({"project": "/tmp/demo", "limit": 10}))
        );
    }

    #[test]
    fn tool_call_payload_serialization_keeps_existing_keys_for_backward_compat() {
        let payload = ToolCallPayload {
            name: "memory_search".to_string(),
            id: "call-456".to_string(),
            input: json!({"query": "foo"}),
        };

        let value = serde_json::to_value(payload).expect("payload serializes");

        assert_eq!(
            value.get("name").and_then(|v| v.as_str()),
            Some("memory_search")
        );
        assert_eq!(value.get("id").and_then(|v| v.as_str()), Some("call-456"));
        assert_eq!(value.get("input"), Some(&json!({"query": "foo"})));
    }
}
