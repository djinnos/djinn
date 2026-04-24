use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use super::{
    ChatCompletionRequest, ChatContent, ChatContentBlock, DJINN_CHAT_SYSTEM_PROMPT, DeltaPayload,
    ErrorPayload, ProjectResolver, ProjectResolverError, ToolCallPayload, ToolResultPayload,
    apply_chat_skills,
};
use crate::server::AppState;
use djinn_agent::actors::slot::{
    ProviderCredential, auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, load_provider_credential, parse_model_id,
};
use djinn_agent::chat_tools::ChatResolvedProject;
use djinn_provider::message::{ContentBlock, Conversation, Message, Role};
use djinn_provider::provider::{LlmProvider, StreamEvent, TelemetryMeta, create_provider};
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

    // Freshly-minted session ids are UUIDv7.  The persistent
    // `WorkspaceStore` is not session-scoped so the session_id is no
    // longer load-bearing for on-disk layout, but we keep it for
    // telemetry + session-affinity routing.
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

    let provider = create_provider(provider_config);

    // User-scoped system message: base prompt + optional client-supplied
    // system string, NO per-project repo map, NO per-project brief.  The
    // orientation plan (§2) forbids project-named templating here.
    let mut conversation = Conversation::new();
    let system_message = super::prompt::system_message::build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        None,
        req.system.as_deref(),
        &req.model,
    );
    let (system_message, _chat_config) = apply_chat_skills(system_message).await;
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
    // Chat only gets a curated slice of the server-wide MCP tool surface.
    // Dumping `all_tool_schemas()` exposes admin/write tools that chat
    // has no business invoking (credential_set, project_environment_config_set,
    // task_update, settings_set, provider_*, agent_*, etc.) and also trips
    // OpenAI's strict validator on schemas that accept arbitrary JSON objects.
    let mut tool_schemas =
        djinn_agent::chat_tools::filter_chat_allowed_mcp_schemas(mcp.all_tool_schemas());
    tool_schemas.extend(djinn_agent::chat_tools::chat_extension_tool_schemas());

    // Construct the per-request ProjectResolver: shared
    // `WorkspaceStore` across sessions, per-request `lookup_cache`
    // for slug→id memoization.
    let resolver = Arc::new(ProjectResolver::new(
        state.db().clone(),
        state.event_bus(),
        state.workspace_store(),
    ));

    let (tx, rx) = tokio::sync::mpsc::channel::<Event>(64);
    let spawn_state = state.clone();
    tokio::spawn(async move {
        run_chat_loop(
            spawn_state,
            provider,
            conversation,
            tool_schemas,
            resolver,
            mcp,
            tx,
        )
        .await;
    });

    Ok(Sse::new(ReceiverStream::new(rx).map(Ok)).keep_alive(KeepAlive::default()))
}

#[allow(clippy::too_many_arguments)]
async fn run_chat_loop(
    state: AppState,
    provider: Box<dyn LlmProvider>,
    mut conversation: Conversation,
    tool_schemas: Vec<serde_json::Value>,
    resolver: Arc<ProjectResolver>,
    mcp: DjinnMcpServer,
    tx: tokio::sync::mpsc::Sender<Event>,
) {
    let agent_ctx = state.agent_context();
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

            let args = serde_json::Value::Object(input.as_object().cloned().unwrap_or_default());
            let started_at = Instant::now();

            let dispatch_result = if djinn_agent::chat_tools::is_chat_extension_tool(&name) {
                let resolver_for_dispatch = resolver.clone();
                let resolve_fn = move |project_ref: String| {
                    let resolver = resolver_for_dispatch.clone();
                    Box::pin(async move {
                        resolver
                            .resolve(&project_ref)
                            .await
                            .map(|resolved| ChatResolvedProject {
                                id: resolved.id,
                                clone_path: resolved.clone_path,
                            })
                            .map_err(|e| match e {
                                ProjectResolverError::NotFound(r) => {
                                    format!("project '{r}' not found")
                                }
                                ProjectResolverError::InvalidId => {
                                    "project id invalid (must be UUID-shaped)".to_owned()
                                }
                                ProjectResolverError::Workspace(inner) => {
                                    format!("workspace failed: {inner}")
                                }
                                ProjectResolverError::Database(inner) => {
                                    format!("project lookup failed: {inner}")
                                }
                            })
                    })
                        as std::pin::Pin<
                            Box<
                                dyn std::future::Future<
                                        Output = Result<ChatResolvedProject, String>,
                                    > + Send,
                            >,
                        >
                };
                djinn_agent::chat_tools::dispatch_chat_tool(
                    &agent_ctx,
                    &name,
                    args,
                    &resolve_fn,
                )
                .await
            } else if djinn_agent::chat_tools::is_chat_allowed_mcp_tool(&name) {
                mcp.dispatch_tool(&name, args).await
            } else {
                Err(format!(
                    "tool '{name}' is not available from chat (admin or write tools are gated)"
                ))
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
}
