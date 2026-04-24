use std::convert::Infallible;
use std::time::Instant;

use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use super::{
    ChatCompletionRequest, ChatContent, ChatContentBlock, DJINN_CHAT_SYSTEM_PROMPT, DeltaPayload,
    ErrorPayload, ToolCallPayload, ToolResultPayload, apply_chat_skills,
};
use crate::server::AppState;
use djinn_agent::actors::slot::{
    ProviderCredential, auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, load_provider_credential, parse_model_id,
};
use djinn_agent::mcp_client::{McpToolRegistry, connect_and_discover};
use djinn_provider::message::{ContentBlock, Conversation, Message, Role};
use djinn_provider::provider::{StreamEvent, TelemetryMeta, create_provider};
use djinn_agent::verification::settings::{load_mcp_server_registry, resolve_mcp_servers};
use djinn_db::ProjectRepository;
use djinn_control_plane::server::DjinnMcpServer;

const MAX_TOOL_ITERATIONS: usize = 20;

pub(super) fn sse_json_event<T: serde::Serialize>(event: &str, payload: &T) -> Event {
    Event::default()
        .event(event)
        .json_data(payload)
        .unwrap_or_else(|_| {
            Event::default()
                .event("error")
                .data("{\"message\":\"serialization error\"}")
        })
}

pub(super) async fn completions_handler_impl(
    state: AppState,
    headers: HeaderMap,
    req: ChatCompletionRequest,
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

    let telemetry_meta = TelemetryMeta {
        task_id: None,
        agent_type: Some("chat".to_owned()),
        session_id: Some(session_id.clone()),
    };

    let provider_config = match provider_credential {
        ProviderCredential::OAuthConfig(mut cfg) => {
            cfg.model_id = resolved_model.clone();
            cfg.context_window = context_window.max(0) as u32;
            cfg.telemetry = Some(telemetry_meta);
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
            djinn_provider::provider::ProviderConfig {
                base_url,
                auth: auth_method_for_provider(&provider_id, &api_key),
                format_family: format_family_for_provider(&provider_id, &resolved_model),
                model_id: resolved_model,
                context_window: context_window.max(0) as u32,
                telemetry: Some(telemetry_meta),
                session_affinity_key: Some(session_id.clone()),
                provider_headers: Default::default(),
                capabilities: capabilities_for_provider(&provider_id),
            }
        }
    };

    let project_resolution: Option<(std::path::PathBuf, String)> = if let Some(project_ref) =
        req.project.as_deref()
    {
        let project_repo = ProjectRepository::new(state.db().clone(), state.event_bus());
        match project_repo.resolve(project_ref).await {
            Ok(Some(id)) => match project_repo.get(&id).await {
                Ok(Some(project)) => Some((
                    djinn_core::paths::project_dir(&project.github_owner, &project.github_repo),
                    project.id,
                )),
                _ => None,
            },
            _ => None,
        }
    } else {
        None
    };
    let project_path = project_resolution.as_ref().map(|(path, _)| path.clone());
    let project_id_for_chat = project_resolution.as_ref().map(|(_, id)| id.clone());

    let provider = create_provider(provider_config);

    let mut conversation = Conversation::new();
    let chat_context =
        super::context::build_project_chat_context(&state, req.project.as_deref()).await;
    let system_message = super::prompt::system_message::build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        chat_context.project_context.as_deref(),
        req.system.as_deref(),
        &req.model,
    );
    let (system_message, chat_config) =
        apply_chat_skills(system_message, project_path.as_deref(), state.db()).await;
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
        let content_blocks = match m.content {
            ChatContent::Text(text) => vec![ContentBlock::Text { text }],
            ChatContent::Blocks(blocks) => blocks
                .into_iter()
                .map(|b| match b {
                    ChatContentBlock::Text { text } => ContentBlock::Text { text },
                    ChatContentBlock::Image { media_type, data } => {
                        ContentBlock::Image { media_type, data }
                    }
                    ChatContentBlock::Document {
                        media_type,
                        data,
                        filename,
                    } => ContentBlock::Document {
                        media_type,
                        data,
                        filename,
                    },
                })
                .collect(),
        };
        conversation.push(Message {
            role,
            content: content_blocks,
            metadata: None,
        });
    }

    let mcp = DjinnMcpServer::new(state.mcp_state());
    let mut tool_schemas = mcp.all_tool_schemas();
    let chat_mcp_registry: Option<McpToolRegistry> =
        if let Some(project_path) = project_path.as_deref() {
            let registry = load_mcp_server_registry(project_path);
            let resolved =
                resolve_mcp_servers(&session_id, "chat", &chat_config.mcp_servers, &registry)
                    .into_iter()
                    .map(|(name, cfg)| (name, cfg.clone()))
                    .collect::<Vec<_>>();
            if resolved.is_empty() {
                None
            } else {
                connect_and_discover(&session_id, "chat", &resolved, &state.agent_context()).await
            }
        } else {
            None
        };
    if let Some(registry) = &chat_mcp_registry {
        tool_schemas.extend(registry.tool_schemas().iter().cloned());
    }

    let agent_ctx = if let Some((project_path_buf, project_id)) = project_resolution.as_ref() {
        tool_schemas.extend(djinn_agent::chat_tools::chat_extension_tool_schemas());
        let mut ctx = state.agent_context();
        if let Some(cached_root) = state.chat_session_warmed_root(&session_id, project_id) {
            tracing::debug!(
                session_id = %session_id,
                project_id = %project_id,
                index_tree = %cached_root.display(),
                "chat session: reusing canonical working_root from first-use cache"
            );
            ctx.working_root = Some(cached_root);
        } else {
            // TODO(architect-only): the previous implementation called
            // `djinn_graph::canonical_graph::ensure_canonical_graph` here on
            // first chat message to pin `working_root` to the canonical
            // index tree.  That broke the architect-only warm invariant
            // (`djinn_graph::architect::ArchitectWarmToken`) — chat sessions
            // carry no role context and must not trigger a SCIP rebuild.
            //
            // If the index tree already exists on disk (because the
            // architect dispatch path or K8s warmer has run at least once
            // for this project), pin to it; otherwise fall back to the
            // project root and let the architect path create it on its
            // own schedule.
            let index_tree_path =
                djinn_core::index_tree::index_tree_path(project_path_buf);
            if index_tree_path.exists() {
                tracing::debug!(
                    session_id = %session_id,
                    project_id = %project_id,
                    index_tree = %index_tree_path.display(),
                    "chat session: pinned working_root to existing canonical index tree (no warm)"
                );
                state.chat_session_record_warmed(
                    &session_id,
                    project_id,
                    index_tree_path.clone(),
                );
                ctx.working_root = Some(index_tree_path);
            } else {
                tracing::debug!(
                    session_id = %session_id,
                    project_id = %project_id,
                    project_root = %project_path_buf.display(),
                    "chat session: canonical index tree absent; skipping warm (architect-only) \
                     and falling back to project root"
                );
            }
        }
        Some(ctx)
    } else {
        None
    };

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
                    Some(djinn_provider::provider::ToolChoice::Auto),
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

                let dispatch_result = if djinn_agent::chat_tools::is_chat_extension_tool(&name) {
                    if let (Some(ctx), Some(root), Some(pid)) =
                        (&agent_ctx, &project_path, project_id_for_chat.as_deref())
                    {
                        djinn_agent::chat_tools::dispatch_chat_tool(ctx, &name, args, root, pid)
                            .await
                    } else {
                        Err(format!("tool '{name}' requires a project context"))
                    }
                } else if let Some(registry) = &chat_mcp_registry
                    && registry.has_tool(&name)
                {
                    registry.call_tool(&name, input.as_object().cloned()).await
                } else {
                    mcp.dispatch_tool(&name, args).await
                };
                match dispatch_result {
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
