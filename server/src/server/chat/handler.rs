use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;

use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use super::{
    ChatCompletionRequest, ChatContent, ChatContentBlock, DJINN_CHAT_SYSTEM_PROMPT, DeltaPayload,
    ErrorPayload, ProjectResolver, ProjectResolverError, SessionTitlePayload, ToolCallPayload,
    ToolResultPayload, apply_chat_skills,
};
use crate::server::AppState;
use djinn_agent::actors::slot::{
    ProviderCredential, auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, load_provider_credential, parse_model_id,
};
use djinn_agent::chat_tools::ChatResolvedProject;
use djinn_db::{SessionMessageRepository, SessionRepository};
use djinn_provider::message::{ContentBlock, Conversation, Message, Role};
use djinn_provider::provider::{LlmProvider, StreamEvent, TelemetryMeta, create_provider};
use djinn_control_plane::server::DjinnMcpServer;

const MAX_TOOL_ITERATIONS: usize = 20;

/// The initial title stamped on a freshly-upserted chat session.  The
/// server-side auto-title path in [`run_chat_loop`] only fires when it
/// observes this exact value, so both the repository layer
/// (`SessionRepository::upsert_chat_session`) and the handler agree on
/// it by constant.
const DEFAULT_CHAT_TITLE: &str = "New Chat";

/// System prompt used for the out-of-band title generation pass.  Kept
/// terse and instruction-only so the non-streamed second call stays
/// well under 50 output tokens.
const TITLE_GEN_SYSTEM_PROMPT: &str =
    "Generate a concise 3-6 word title for this conversation. Return only the title text, nothing else.";

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

/// Convert an incoming `ChatContentBlock` array into provider-native
/// `ContentBlock`s.  Used for both conversation construction and
/// DB persistence.
fn incoming_to_content_blocks(content: ChatContent) -> Vec<ContentBlock> {
    match content {
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
    }
}

pub(super) async fn completions_handler_impl(
    state: AppState,
    _headers: HeaderMap,
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
    if req.session_id.trim().is_empty() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "session_id is required".to_string(),
        ));
    }
    // Validate UUID shape up front — the column is VARCHAR(36) and the
    // client is expected to mint a UUIDv7.  Accept any UUID format.
    if uuid::Uuid::parse_str(req.session_id.trim()).is_err() {
        return Err((
            axum::http::StatusCode::BAD_REQUEST,
            "session_id must be a UUID".to_string(),
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

    // Chat session id now comes from the client — UUIDv7 minted by the
    // UI and re-used across requests so messages keep accumulating
    // against one row.  It also doubles as the SSE session-affinity
    // key for the upstream provider.
    let session_id = req.session_id.trim().to_string();

    // Upsert the chat session row before we spawn any provider work so
    // that the FK on `session_messages` holds when we persist the
    // incoming user turn below.  Idempotent: subsequent requests with
    // the same id re-fetch the existing row.
    let session_repo = SessionRepository::new(state.db().clone(), state.event_bus());
    let session_row = session_repo
        .upsert_chat_session(&session_id, &req.model)
        .await
        .map_err(|e| {
            tracing::warn!(session_id=%session_id, error=%e, "chat session upsert failed");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("chat session upsert failed: {e}"),
            )
        })?;

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

    // Extract the final user turn so we can persist it verbatim before
    // streaming starts (persist-user-message-before-stream invariant).
    // Earlier turns in `req.messages` are assumed already persisted by
    // prior /completions calls on the same session_id.
    let mut last_user_content_for_persist: Option<Vec<ContentBlock>> = None;

    let incoming = req.messages;
    let last_incoming_index = incoming.len().saturating_sub(1);
    for (idx, m) in incoming.into_iter().enumerate() {
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
        let content_blocks = incoming_to_content_blocks(m.content);
        if idx == last_incoming_index && matches!(role, Role::User) {
            last_user_content_for_persist = Some(content_blocks.clone());
        }
        conversation.push(Message {
            role,
            content: content_blocks,
            metadata: None,
        });
    }

    // Persist the incoming user turn BEFORE we spawn the streaming task.
    // Schema for user messages stored in `session_messages.content_json`:
    //
    //   [ContentBlock, …]
    //
    // where each `ContentBlock` is the provider-native `djinn_provider::
    // message::ContentBlock` JSON (adjacently-tagged on `type`, see
    // `djinn_core::message`).  The UI can reconstruct text + image +
    // document attachments without a separate `attachments` sidecar.
    if let Some(ref content) = last_user_content_for_persist {
        let message_repo = SessionMessageRepository::new(state.db().clone(), state.event_bus());
        let content_json = serde_json::to_string(content).unwrap_or_else(|_| "[]".to_string());
        // `task_id` is unused for chat — pass empty string (the repo
        // only consults it for the emitted event payload).
        if let Err(e) = message_repo
            .insert_message(&session_id, "", "user", &content_json, None)
            .await
        {
            tracing::warn!(session_id=%session_id, error=%e, "failed to persist user chat turn");
        }
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
    let needs_title = session_row.title.as_deref() == Some(DEFAULT_CHAT_TITLE);
    let user_turn_for_title = last_user_content_for_persist;
    let session_id_for_loop = session_id.clone();
    let model_for_title = req.model.clone();
    tokio::spawn(async move {
        run_chat_loop(
            spawn_state,
            provider,
            conversation,
            tool_schemas,
            resolver,
            mcp,
            tx,
            session_id_for_loop,
            needs_title,
            user_turn_for_title,
            model_for_title,
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
    session_id: String,
    needs_title: bool,
    user_turn_for_title: Option<Vec<ContentBlock>>,
    model_id: String,
) {
    let agent_ctx = state.agent_context();
    let mut loop_count = 0usize;
    // Accumulated assistant content across every provider turn of the
    // tool loop.  Persisted to `session_messages` once the loop exits
    // (successful `done` or `error`).  Also fed to the auto-title pass.
    let mut persisted_assistant_content: Vec<ContentBlock> = Vec::new();
    let mut completed_ok = false;

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
                    // Persist whatever assistant content we accumulated
                    // on the way down so the UI can reconstruct a
                    // partial conversation on refresh.
                    persist_assistant_turn(&state, &session_id, &persisted_assistant_content)
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
        persisted_assistant_content.extend(assistant_content.clone());
        if !assistant_content.is_empty() {
            conversation.push(Message {
                role: Role::Assistant,
                content: assistant_content,
                metadata: None,
            });
        }

        if tool_calls.is_empty() {
            completed_ok = true;
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

    // Persist the full accumulated assistant turn before we fire the
    // title generation / done events so that a refresh mid-title-pass
    // still sees the assistant message.
    persist_assistant_turn(&state, &session_id, &persisted_assistant_content).await;

    // Server-side auto-title pass.  Only runs on successful completion
    // (completed_ok) AND when the session was still on the default
    // placeholder title at the start of this request.  Any failure
    // (provider down, malformed reply) logs and falls through without
    // emitting `session_title` — the UI keeps rendering "New Chat"
    // until the next turn.
    if completed_ok && needs_title {
        let title = generate_chat_title(
            &state,
            user_turn_for_title.as_deref(),
            &persisted_assistant_content,
            &model_id,
            &session_id,
        )
        .await;
        if let Some(title) = title {
            let repo = SessionRepository::new(state.db().clone(), state.event_bus());
            if let Err(e) = repo.update_chat_title(&session_id, &title).await {
                tracing::warn!(session_id=%session_id, error=%e, "failed to persist chat title");
            } else {
                let _ = tx
                    .send(sse_json_event(
                        "session_title",
                        &SessionTitlePayload {
                            session_id: session_id.clone(),
                            title,
                        },
                    ))
                    .await;
            }
        }
    }

    let _ = tx.send(Event::default().event("done").data("{}")).await;
}

/// Persist the accumulated assistant turn to `session_messages`.
///
/// Schema stored in `content_json` for assistant messages:
///
///   [ContentBlock, …]
///
/// where each element is a provider-native `djinn_provider::message::
/// ContentBlock`.  In particular, every `ToolUse` block keeps its full
/// `input` JSON so the UI can reconstruct the tool-call args on
/// reload.  Empty turns (no text, no tool calls) are skipped.
async fn persist_assistant_turn(state: &AppState, session_id: &str, content: &[ContentBlock]) {
    if content.is_empty() {
        return;
    }
    let repo = SessionMessageRepository::new(state.db().clone(), state.event_bus());
    let content_json = serde_json::to_string(content).unwrap_or_else(|_| "[]".to_string());
    if let Err(e) = repo
        .insert_message(session_id, "", "assistant", &content_json, None)
        .await
    {
        tracing::warn!(session_id=%session_id, error=%e, "failed to persist assistant chat turn");
    }
}

/// Fire a second, non-streamed LLM call to generate a 3-6 word title
/// for the conversation.
///
/// Implementation note: the `LlmProvider` trait only exposes `stream()`
/// — there is no dedicated non-streaming entrypoint — so we drive the
/// same streaming API and accumulate text until the provider emits
/// `Done`.  "Non-streamed" here means "we don't forward deltas to the
/// client", not "don't use the streaming API".
async fn generate_chat_title(
    state: &AppState,
    user_content: Option<&[ContentBlock]>,
    assistant_content: &[ContentBlock],
    model_id: &str,
    session_id: &str,
) -> Option<String> {
    // Both sides of the conversation need at least one text block or
    // the title pass is pointless.
    let user_text = flatten_text(user_content.unwrap_or(&[]));
    let assistant_text = flatten_text(assistant_content);
    if user_text.trim().is_empty() && assistant_text.trim().is_empty() {
        return None;
    }

    let (provider_id, model_name) = match parse_model_id(model_id) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!(model=%model_id, error=%e, "title-gen: invalid model id");
            return None;
        }
    };
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

    let provider_credential = match load_provider_credential(&provider_id, &state.agent_context())
        .await
    {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(session_id=%session_id, error=%e, "title-gen: credential load failed");
            return None;
        }
    };

    let telemetry_meta = TelemetryMeta {
        task_id: None,
        agent_type: Some("chat_title".to_owned()),
        session_id: Some(session_id.to_owned()),
    };
    let provider_config = match provider_credential {
        ProviderCredential::OAuthConfig(mut cfg) => {
            cfg.model_id = resolved_model.clone();
            cfg.telemetry = Some(telemetry_meta);
            cfg.session_affinity_key = Some(format!("{session_id}:title"));
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
                context_window: 0,
                telemetry: Some(telemetry_meta),
                session_affinity_key: Some(format!("{session_id}:title")),
                provider_headers: Default::default(),
                capabilities: capabilities_for_provider(&provider_id),
            }
        }
    };

    let provider = create_provider(provider_config);
    let mut conversation = Conversation::new();
    conversation.push(Message {
        role: Role::System,
        content: vec![ContentBlock::text(TITLE_GEN_SYSTEM_PROMPT)],
        metadata: None,
    });
    conversation.push(Message {
        role: Role::User,
        content: vec![ContentBlock::text(format!(
            "User: {user_text}\n\nAssistant: {assistant_text}"
        ))],
        metadata: None,
    });

    let stream = match provider.stream(&conversation, &[], None).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(session_id=%session_id, error=%e, "title-gen: stream init failed");
            return None;
        }
    };

    tokio::pin!(stream);
    let mut text = String::new();
    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamEvent::Delta(ContentBlock::Text { text: chunk })) => text.push_str(&chunk),
            Ok(StreamEvent::Done) => break,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(session_id=%session_id, error=%e, "title-gen: stream event failed");
                return None;
            }
        }
    }

    let cleaned = clean_generated_title(&text);
    if cleaned.is_empty() { None } else { Some(cleaned) }
}

/// Concatenate all `Text` blocks in a content array into a single string.
/// Used to build the title-gen prompt input.
fn flatten_text(content: &[ContentBlock]) -> String {
    let mut out = String::new();
    for block in content {
        if let ContentBlock::Text { text } = block {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(text);
        }
    }
    out
}

/// Trim wrap characters a model might emit around the title.  Bounded
/// to 120 chars so a runaway model doesn't balloon the column.
fn clean_generated_title(raw: &str) -> String {
    let trimmed = raw.trim();
    // Strip a single surrounding quote pair if present.
    let trimmed = trimmed
        .trim_start_matches(['"', '\'', '`'])
        .trim_end_matches(['"', '\'', '`', '.', '!', '?'])
        .trim();
    // Collapse runs of whitespace (incl. newlines) to single spaces.
    let collapsed: String = trimmed.split_whitespace().collect::<Vec<_>>().join(" ");
    // Clamp length.
    if collapsed.chars().count() > 120 {
        collapsed.chars().take(120).collect()
    } else {
        collapsed
    }
}
