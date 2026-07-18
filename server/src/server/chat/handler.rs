// djinn:allow-oversize — legacy module over size-guard threshold; split when touched substantively.
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(test)]
use std::collections::HashMap;
#[cfg(test)]
use std::sync::LazyLock;

use djinn_core::clock::{Clock, SystemClock as SystemClockTrait};

/// Default outer timeout for per-tool dispatch in the chat loop.
/// Defense-in-depth on top of op-specific inner timeouts (e.g. the 60s
/// `code_graph` dispatcher cap). Override with
/// `DJINN_CHAT_TOOL_DISPATCH_TIMEOUT_SECS`. 120s leaves the inner cap
/// comfortable headroom while still bounding worst-case stream stalls.
const CHAT_TOOL_DISPATCH_TIMEOUT_DEFAULT_SECS: u64 = 120;

fn chat_tool_dispatch_timeout() -> Duration {
    let secs = std::env::var("DJINN_CHAT_TOOL_DISPATCH_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(CHAT_TOOL_DISPATCH_TIMEOUT_DEFAULT_SECS);
    Duration::from_secs(secs)
}

fn should_persist_interruption_notice(
    partial: &[ContentBlock],
    discarded_tool_calls_count: i32,
) -> bool {
    !partial.is_empty() || discarded_tool_calls_count > 0
}

/// Persist an aborted assistant turn and its model-internal interruption notice.
/// The notice is deliberately in a side table: the partial assistant body remains
/// provider state/text only, and buffered ToolUse blocks are never written.
async fn persist_interrupted_assistant_turn(
    state: &AppState,
    session_id: &str,
    partial: &[ContentBlock],
    discarded_tool_calls_count: i32,
) {
    if !should_persist_interruption_notice(partial, discarded_tool_calls_count) {
        return;
    }

    let message_repo = SessionMessageRepository::new(state.db().clone(), state.event_bus());
    let saved_message_id = if partial.is_empty() {
        None
    } else {
        let content_json = serde_json::to_string(partial).unwrap_or_else(|_| "[]".to_string());
        match message_repo
            .insert_message(session_id, "", "assistant", &content_json, None)
            .await
        {
            Ok(message) => Some(message.id),
            Err(error) => {
                tracing::warn!(session_id=%session_id, error=%error, "failed to persist partial chat turn");
                None
            }
        }
    };

    // A tools-only interruption has no assistant message by design, but still
    // needs a durable notice. When partial content could not be saved, avoid a
    // dangling notice that claims a saved turn exists.
    if partial.is_empty() || saved_message_id.is_some() {
        let notice_repo = ChatInterruptionNoticeRepository::new(state.db().clone());
        if let Err(error) = notice_repo
            .create(CreateChatInterruptionNotice {
                session_id,
                session_message_id: saved_message_id.as_deref(),
                discarded_tool_calls_count,
            })
            .await
        {
            tracing::warn!(session_id=%session_id, error=%error, "failed to persist chat interruption notice");
        }
    }
}

use axum::http::HeaderMap;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::StreamExt;
use tokio_stream::wrappers::ReceiverStream;

use super::{
    ChatCompletionRequest, ChatContent, ChatContentBlock, DJINN_CHAT_SYSTEM_PROMPT, DeltaPayload,
    ErrorPayload, PROPOSAL_ADDRESS_SYSTEM_PROMPT, ProjectResolver, ProjectResolverError,
    SessionTitlePayload, ToolCallPayload, ToolResultPayload, apply_chat_skills,
    complete_chat_compaction_boundary, record_chat_compaction_started,
};
use crate::server::AppState;
use crate::server::auth::authenticate;
use djinn_agent::actors::slot::{
    ProviderCredential, auth_method_for_provider, capabilities_for_provider, default_base_url,
    format_family_for_provider, load_provider_credential, parse_model_id,
};
use djinn_agent::chat_tools::ChatResolvedProject;
use djinn_compaction::COMPACTION_SUMMARY_END_MARKER;
use djinn_control_plane::server::DjinnMcpServer;
use djinn_core::auth_context::{
    REVISION_CALLER_CONTEXT, SESSION_USER_ID, SESSION_USER_TOKEN, TrustedRevisionCallerContext,
};
use djinn_db::{
    ChatInterruptionNotice, ChatInterruptionNoticeRepository, CreateChatInterruptionNotice,
    ProposalRepository, SessionCompactionBoundaryRepository, SessionMessageRepository,
    SessionRepository,
};
use djinn_provider::message::{ContentBlock, Conversation, Message, Role};
use djinn_provider::provider::{LlmProvider, StreamEvent, TelemetryMeta, create_provider};

#[cfg(test)]
type TestProviderFactory = Arc<dyn Fn() -> Box<dyn LlmProvider> + Send + Sync>;

/// Test-only seam for HTTP handler regressions. The session-affinity key is
/// already unique per request, so concurrent tests cannot replace each other's
/// provider behavior.
#[cfg(test)]
static TEST_PROVIDERS: LazyLock<Mutex<HashMap<String, TestProviderFactory>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(test)]
pub(super) fn register_test_provider(
    session_id: &str,
    factory: impl Fn() -> Box<dyn LlmProvider> + Send + Sync + 'static,
) {
    let factory: TestProviderFactory = Arc::new(factory);
    let mut providers = TEST_PROVIDERS.lock().expect("test provider registry lock");
    providers.insert(session_id.to_owned(), factory.clone());
    providers.insert(format!("{session_id}:title"), factory);
}

fn create_chat_provider(config: djinn_provider::provider::ProviderConfig) -> Box<dyn LlmProvider> {
    #[cfg(test)]
    if let Some(factory) = config.session_affinity_key.as_deref().and_then(|key| {
        TEST_PROVIDERS
            .lock()
            .ok()
            .and_then(|providers| providers.get(key).cloned())
    }) {
        return factory();
    }
    create_provider(config)
}

const MAX_TOOL_ITERATIONS: usize = 20;

/// Metadata marker kind emitted by `SessionMessageRepository::summary_message`
/// when `load_conversation` projects a completed compaction boundary.
const PROJECTED_COMPACTION_MARKER_KIND: &str = "compaction_summary";

/// The durable ids and model-only message for notices included in one chat turn.
/// IDs, rather than rendered text or message positions, are the deduplication key.
struct InterruptionReminder {
    notice_ids: Vec<String>,
    message: Message,
}

/// Collapse all pending interruptions into one model-only reminder. Notice timestamps
/// are normalized UTC strings in the durable contract, so lexical maximum is latest.
fn collapse_interruption_notices(
    notices: &[ChatInterruptionNotice],
) -> Option<InterruptionReminder> {
    if notices.is_empty() {
        return None;
    }

    let discarded_tool_calls_count = notices
        .iter()
        .map(|notice| notice.discarded_tool_calls_count)
        .sum::<i32>();
    let latest_interruption = notices
        .iter()
        .map(|notice| notice.interrupted_at.as_str())
        .max()
        .expect("non-empty notices have an interruption timestamp");
    let message = Message::system(format!(
        "A previous assistant turn was interrupted at {latest_interruption}. \
         Its saved output may be partial. {discarded_tool_calls_count} pending tool call(s) \
         were discarded and did not run. Treat that turn as incomplete."
    ));

    Some(InterruptionReminder {
        notice_ids: notices
            .iter()
            .map(|notice| notice.interruption_notice_id.clone())
            .collect(),
        message,
    })
}

/// Consume the exact stable ids included in a provider prompt after its stream
/// has been created. Keeping this at the provider-start boundary makes the
/// durable one-shot behavior directly testable.
async fn consume_started_interruption_notices(
    notice_repo: &ChatInterruptionNoticeRepository,
    interruption_notice_ids: &mut Vec<String>,
) -> djinn_db::Result<()> {
    if interruption_notice_ids.is_empty() {
        return Ok(());
    }

    notice_repo.mark_consumed(interruption_notice_ids).await?;
    interruption_notice_ids.clear();
    Ok(())
}

/// Whether `msg` is a projected compaction summary produced by
/// `load_conversation` when a completed durable boundary exists.
///
/// Projected summaries are `Role::System` messages with `marker_kind` metadata;
/// raw historical system messages never carry this metadata.
fn is_projected_compaction_summary(msg: &Message) -> bool {
    msg.role == Role::System
        && msg
            .metadata
            .as_ref()
            .and_then(|m| m.provider_data.as_ref())
            .and_then(|pd| pd.get("marker_kind"))
            .and_then(|v| v.as_str())
            == Some(PROJECTED_COMPACTION_MARKER_KIND)
}

/// Whether `msg` is an old compaction marker/continuation pair message that
/// should be excluded from ordinary conversation input when a projected
/// boundary summary is already present.
fn is_compaction_marker_pair(msg: &Message) -> bool {
    if msg.role == Role::User && msg.text_content().contains(COMPACTION_SUMMARY_END_MARKER) {
        return true;
    }
    if msg.role == Role::Assistant {
        let text = msg.text_content();
        if text
            == "Your context was compacted. The previous message contains a summary of the conversation so far. Continue calling tools as necessary to complete the task."
            || text.starts_with("Part of your context was compacted.")
        {
            return true;
        }
    }
    false
}

/// Reactive compact-and-retry attempts on a context-overflow stream failure in
/// the chat loop (C3). Matches the worker reply loop's `MAX_COMPACTION_RETRIES`.
const MAX_CHAT_COMPACTION_RETRIES: u32 = 2;

/// Outcome of draining a single provider stream turn.
enum TurnResult {
    /// Normal turn with the fully assembled assistant content (provider state,
    /// reconciled thinking, text, tool calls) and the separate tool-call list
    /// for dispatch.
    Ok {
        assistant_content: Vec<ContentBlock>,
        tool_calls: Vec<ContentBlock>,
    },
    /// Provider stream emitted an error event; partial assistant content
    /// was already persisted and the caller should abort the loop.
    StreamError,
    /// Provider returned an empty turn (no text and no tool calls).
    Empty,
}

/// Outcome of `init_provider_stream` — distinguishes the two error paths
/// that the caller must handle with different control flow (`continue` vs
/// `break`).
enum StreamInitOutcome {
    /// A recoverable compaction succeeded; the caller should retry stream
    /// creation (i.e. `continue` the chat loop).
    CompactedAndContinue,
    /// An unrecoverable failure: either a non-compaction error or compaction
    /// exhaustion. An SSE error event has already been sent; the caller
    /// should `break` the chat loop.
    UnrecoverableBreak,
}

/// Proactively compact the conversation if it exceeds the compaction threshold.
async fn maybe_compact_proactively(
    state: &AppState,
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    session_id: &str,
    context_window: i64,
) {
    if djinn_agent::compaction::needs_compaction(
        conversation.token_estimate() as u32,
        context_window,
    ) {
        let boundary_repo = SessionCompactionBoundaryRepository::new(state.db().clone());
        let boundary_id =
            record_chat_compaction_started(&boundary_repo, session_id, conversation).await;
        let compacted = djinn_agent::compaction::compact_conversation(
            provider,
            conversation,
            session_id,
            "",
            djinn_agent::compaction::CompactionContext::ChatSession,
            context_window,
        )
        .await;
        if compacted {
            complete_chat_compaction_boundary(&boundary_repo, boundary_id.as_deref(), conversation)
                .await;
        }
    }
}

/// Attempt to initialize a provider stream, with bounded recoverable compaction retry.
///
/// Returns `Ok(stream)` on success. On recoverable compaction success, returns
/// `Err(StreamInitOutcome::CompactedAndContinue)` so the caller retries. On
/// unrecoverable failure, sends an SSE error event and returns
/// `Err(StreamInitOutcome::UnrecoverableBreak)` so the caller breaks the loop.
#[allow(clippy::too_many_arguments)]
async fn init_provider_stream(
    state: &AppState,
    provider: &dyn LlmProvider,
    conversation: &mut Conversation,
    tool_schemas: &[serde_json::Value],
    tx: &tokio::sync::mpsc::Sender<Event>,
    session_id: &str,
    context_window: i64,
    compaction_attempts: &mut u32,
) -> Result<
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamEvent, anyhow::Error>> + Send>>,
    StreamInitOutcome,
> {
    match provider
        .stream(
            conversation,
            tool_schemas,
            Some(djinn_provider::provider::ToolChoice::Auto),
        )
        .await
    {
        Ok(s) => Ok(s),
        Err(e) => {
            // C3 reactive net: a context-overflow (or orphaned-tool) failure
            // on stream init is recoverable — summarise and retry rather than
            // dropping the turn. Bounded by MAX_CHAT_COMPACTION_RETRIES.
            if djinn_agent::compaction::is_compaction_recoverable_error(&e)
                && *compaction_attempts < MAX_CHAT_COMPACTION_RETRIES
            {
                *compaction_attempts += 1;
                tracing::warn!(
                    error = %e,
                    attempt = *compaction_attempts,
                    "chat: recoverable stream-init failure; compacting and retrying"
                );
                let boundary_repo = SessionCompactionBoundaryRepository::new(state.db().clone());
                let boundary_id =
                    record_chat_compaction_started(&boundary_repo, session_id, conversation).await;
                let compacted = djinn_agent::compaction::compact_conversation(
                    provider,
                    conversation,
                    session_id,
                    "",
                    djinn_agent::compaction::CompactionContext::ChatSession,
                    context_window,
                )
                .await;
                if compacted {
                    complete_chat_compaction_boundary(
                        &boundary_repo,
                        boundary_id.as_deref(),
                        conversation,
                    )
                    .await;
                }
                if compacted {
                    return Err(StreamInitOutcome::CompactedAndContinue);
                }
            }
            tracing::warn!(error=%e, "provider stream init failed");
            let _ = tx
                .send(sse_json_event(
                    "error",
                    &ErrorPayload {
                        message: format!("provider stream failed: {e}"),
                    },
                ))
                .await;
            Err(StreamInitOutcome::UnrecoverableBreak)
        }
    }
}

/// Assemble the canonical persisted assistant content for a chat turn.
///
/// Canonical order: provider-state blocks, one unsigned thinking block from
/// unresolved fragments (reconciled by exact block ID), assistant text, then
/// tool calls. Side-effect-free.
fn assemble_chat_assistant_content(
    provider_state: &[ContentBlock],
    unresolved_thinking: &[(u64, String)],
    completed_thinking_ids: &std::collections::HashSet<u64>,
    unattributed_thinking: &str,
    text: &str,
    tool_calls: &[ContentBlock],
) -> Vec<ContentBlock> {
    let mut content: Vec<ContentBlock> = Vec::new();

    // 1. Provider-state blocks.
    content.extend(provider_state.iter().cloned());

    // 2. One unsigned thinking block from non-empty unresolved fragments.
    let mut unsigned = String::new();
    for (id, fragment) in unresolved_thinking {
        if !completed_thinking_ids.contains(id) {
            unsigned.push_str(fragment);
        }
    }
    if !unattributed_thinking.is_empty() {
        unsigned.push_str(unattributed_thinking);
    }
    if !unsigned.is_empty() {
        content.push(ContentBlock::Thinking {
            thinking: unsigned,
            signature: None,
        });
    }

    // 3. Assistant text.
    if !text.is_empty() {
        content.push(ContentBlock::Text {
            text: text.to_string(),
        });
    }

    // 4. Tool calls.
    content.extend(tool_calls.iter().cloned());

    content
}

/// Drain a single provider stream turn, forwarding deltas via SSE and assembling
/// the turn result. On stream errors, persists partial assistant content and returns
/// `TurnResult::StreamError` so the caller can abort.
async fn drain_provider_turn(
    stream: &mut (impl futures::Stream<Item = Result<StreamEvent, anyhow::Error>> + Unpin),
    tx: &tokio::sync::mpsc::Sender<Event>,
    state: &AppState,
    session_id: &str,
) -> TurnResult {
    let mut turn_text = String::new();
    let mut turn_provider_state: Vec<ContentBlock> = Vec::new();
    let mut tool_calls: Vec<ContentBlock> = Vec::new();
    // Arrival-ordered unresolved thinking-delta fragments keyed by exact
    // content-block ID, plus the set of IDs that completed.
    let mut unresolved_thinking: Vec<(u64, String)> = Vec::new();
    let mut completed_thinking_ids: std::collections::HashSet<u64> =
        std::collections::HashSet::new();

    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamEvent::Delta(ContentBlock::Text { text })) => {
                turn_text.push_str(&text);
                let _ = tx
                    .send(sse_json_event("delta", &DeltaPayload { text }))
                    .await;
            }
            Ok(StreamEvent::Delta(tool @ ContentBlock::ToolUse { .. })) => tool_calls.push(tool),
            Ok(StreamEvent::Delta(state @ ContentBlock::OpenAIReasoning { .. }))
            | Ok(StreamEvent::Delta(state @ ContentBlock::Thinking { .. }))
            | Ok(StreamEvent::Delta(state @ ContentBlock::RedactedThinking { .. }))
            | Ok(StreamEvent::Delta(state @ ContentBlock::Unknown { .. })) => {
                turn_provider_state.push(state);
            }
            Ok(StreamEvent::Delta(ContentBlock::ToolResult { .. }))
            | Ok(StreamEvent::Delta(ContentBlock::Image { .. }))
            | Ok(StreamEvent::Delta(ContentBlock::Document { .. })) => {}
            Ok(StreamEvent::Thinking(_)) => {
                // Unattributed thinking (OpenAI reasoning) — no block ID to
                // reconcile against, always included via the turn_thinking
                // aggregate. Chat does not display thinking deltas live.
            }
            Ok(StreamEvent::ThinkingDelta { id, text }) => {
                unresolved_thinking.push((id, text));
            }
            Ok(StreamEvent::ThinkingBlockComplete { id, .. }) => {
                // The signed block already arrived as Delta(Thinking) in
                // turn_provider_state. Record the ID for reconciliation.
                completed_thinking_ids.insert(id);
            }
            Ok(StreamEvent::Done) => break,
            Ok(StreamEvent::Usage(_)) => {}
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
                // Prior turns were already persisted incrementally and
                // are well-formed. Persist this turn's partial content
                // using canonical assembly: completed provider state,
                // unresolved thinking (reconciled by exact ID), then
                // partial text. DROP any buffered tool calls: their results
                // will never be produced, and persisting a `function_call`
                // with no `function_call_output` is exactly the orphan that
                // wedges the next turn.
                let partial = assemble_chat_assistant_content(
                    &turn_provider_state,
                    &unresolved_thinking,
                    &completed_thinking_ids,
                    "",
                    &turn_text,
                    &[],
                );
                persist_interrupted_assistant_turn(
                    state,
                    session_id,
                    &partial,
                    tool_calls.len() as i32,
                )
                .await;
                return TurnResult::StreamError;
            }
        }
    }

    // Normal completion: assemble the canonical assistant content for the
    // caller to persist.
    let assistant_content = assemble_chat_assistant_content(
        &turn_provider_state,
        &unresolved_thinking,
        &completed_thinking_ids,
        "",
        &turn_text,
        &tool_calls,
    );

    if turn_text.is_empty() && tool_calls.is_empty() {
        tracing::warn!(
            provider_state_items = turn_provider_state.len(),
            "chat provider returned an empty assistant turn"
        );
        let _ = tx
            .send(sse_json_event(
                "error",
                &ErrorPayload {
                    message: "provider returned an empty response; this usually means the upstream Codex backend refused or throttled the request".to_string(),
                },
            ))
            .await;
        // Prior turns already persisted incrementally; this turn has no
        // content worth storing.
        return TurnResult::Empty;
    }

    TurnResult::Ok {
        assistant_content,
        tool_calls,
    }
}

/// The initial title stamped on a freshly-upserted chat session.  The
/// server-side auto-title path in [`run_chat_loop`] only fires when it
/// observes this exact value, so both the repository layer
/// (`SessionRepository::upsert_chat_session`) and the handler agree on
/// it by constant.
const DEFAULT_CHAT_TITLE: &str = "New Chat";

/// System prompt used for the out-of-band title generation pass.  Kept
/// terse and instruction-only so the non-streamed second call stays
/// well under 50 output tokens.
const TITLE_GEN_SYSTEM_PROMPT: &str = "Generate a concise 3-6 word title for this conversation. Return only the title text, nothing else.";

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

// Deliberate signature: the Ok side carries (turns, last-user-content) and the
// Err side is an axum (status, message) pair — naming each would obscure more
// than it clarifies for a single private helper.
#[allow(clippy::type_complexity)]
fn latest_user_turn_from_incoming(
    incoming: Vec<super::ChatMessage>,
) -> Result<(Vec<Message>, Option<Vec<ContentBlock>>), (axum::http::StatusCode, String)> {
    let last_incoming_index = incoming.len().saturating_sub(1);
    let mut turns = Vec::with_capacity(incoming.len());
    let mut last_user_content_for_persist = None;

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
        let content = incoming_to_content_blocks(m.content);
        if idx == last_incoming_index && matches!(role, Role::User) {
            last_user_content_for_persist = Some(content.clone());
        }
        turns.push(Message {
            role,
            content,
            metadata: None,
        });
    }

    Ok((turns, last_user_content_for_persist))
}

pub(super) async fn completions_handler_impl(
    state: AppState,
    headers: HeaderMap,
    req: ChatCompletionRequest,
) -> Result<
    Sse<impl futures::Stream<Item = Result<Event, Infallible>>>,
    (axum::http::StatusCode, String),
> {
    // Resolve the authenticated user's GitHub access token and stable
    // `users.id` (if any) so MCP tools spawned by the chat loop can stamp
    // `created_by_user_id` on new epics/tasks/sessions via
    // `current_user_id()`. Mirrors the wiring in `mcp_handler.rs`.
    //
    // Unauthenticated chat requests are not rejected (chat is a generic
    // assistant endpoint and not all flows mutate the database); they
    // simply leave the task-locals as None, which yields NULL
    // attribution columns — same as background-agent-initiated writes.
    let (user_token, user_id): (Option<String>, Option<String>) = match authenticate(
        &state, &headers,
    )
    .await
    {
        Ok(Some(user)) => (Some(user.github_access_token), Some(user.id)),
        Ok(None) => (None, None),
        Err(err) => {
            tracing::warn!(error = %err, "chat completions: authenticate failed; proceeding unauth");
            (None, None)
        }
    };
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

    // Proposal-scoped chat ("Address with djinn"): seed the system prompt with
    // the spec + unresolved feedback and grant the proposal-editing tools.
    // This path edits the proposal as the requesting user, so — unlike generic
    // chat — it MUST be authenticated: an anonymous caller hits the proposal
    // edit gate's trusted-system bypass (`None ⇒ Ok`) and would slip past it.
    let proposal_ref = req.proposal_id.as_deref().filter(|s| !s.trim().is_empty());
    if proposal_ref.is_some() && user_id.is_none() {
        return Err((
            axum::http::StatusCode::UNAUTHORIZED,
            "addressing a proposal requires an authenticated user".to_string(),
        ));
    }
    let proposal_system = match proposal_ref {
        Some(p) => match build_proposal_address_prompt(&state, p, req.feedback_id.as_deref()).await
        {
            Some(prompt) => Some(prompt),
            None => {
                return Err((
                    axum::http::StatusCode::NOT_FOUND,
                    format!("proposal not found: {p}"),
                ));
            }
        },
        None => None,
    };

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

    // Resolve the provider credential UNDER the authenticated user's scope so
    // per-user credentials (migration 28) prefer this user's own connected
    // provider, falling back to the org-shared one. Unauthenticated chat
    // resolves org-shared (user_id is None). Mirrors the worker dispatch path.
    let provider_credential = SESSION_USER_ID
        .scope(
            user_id.clone(),
            load_provider_credential(&provider_id, &state.agent_context()),
        )
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
    //
    // Scope under SESSION_USER_ID so `upsert_chat_session` stamps
    // `created_by_user_id` via `current_user_id()`. Without this scope the
    // row is owned by no one and `list_chat_for_user` filters it out — the
    // user never sees their own new chats.
    let session_repo = SessionRepository::new(state.db().clone(), state.event_bus());
    let session_row = SESSION_USER_ID
        .scope(
            user_id.clone(),
            session_repo.upsert_chat_session(&session_id, &req.model),
        )
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
        ..Default::default()
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
                reasoning_effort: None,
                tool_schema_compat: None,
            }
        }
    };

    let provider = create_chat_provider(provider_config);

    // User-scoped system message: base prompt + optional client-supplied
    // system string, NO per-project repo map, NO per-project brief.  The
    // orientation plan (§2) forbids project-named templating here.
    //
    // PR E1 (Epic E — RAG plumbing): when `req.project` is supplied AND
    // `DJINN_CHAT_AUTO_CODEBASE_HEADER` is on, we resolve the project
    // and inject a compact `📦 CURRENT CODEBASE` block as the
    // `project_context` slot. The block is cached in-process for 60s
    // keyed by `(project_id, pinned_commit)` so we don't re-run the
    // status/ranked queries on every chat turn.
    let codebase_header = if super::prompt::codebase_header::is_enabled() {
        match req.project.as_ref().filter(|p| !p.trim().is_empty()) {
            Some(project_ref) => {
                // Build a one-shot resolver locally — the per-tool-call
                // resolver is constructed below for tool dispatch but we
                // need the path now.  We share its workspace store so
                // both layers hit the same persistent clone.
                let header_resolver = ProjectResolver::new(
                    state.db().clone(),
                    state.event_bus(),
                    state.workspace_store(),
                );
                match header_resolver.resolve(project_ref).await {
                    Ok(resolved) => {
                        let agent_ctx = state.agent_context();
                        match agent_ctx.repo_graph_ops.clone() {
                            Some(ops) => {
                                super::prompt::codebase_header::build_codebase_header(
                                    ops,
                                    &resolved.id,
                                    &resolved.clone_path,
                                )
                                .await
                            }
                            None => None,
                        }
                    }
                    Err(err) => {
                        tracing::warn!(project=%project_ref, error=%err, "codebase_header: project resolution failed; skipping");
                        None
                    }
                }
            }
            None => None,
        }
    } else {
        None
    };

    let (incoming_turns, last_user_content_for_persist) =
        latest_user_turn_from_incoming(req.messages)?;

    let message_repo = SessionMessageRepository::new(state.db().clone(), state.event_bus());
    let persisted_history = message_repo
        .load_conversation(&session_id)
        .await
        .map_err(|e| {
            tracing::warn!(session_id=%session_id, error=%e, "failed to load persisted chat history");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load persisted chat history: {e}"),
            )
        })?;
    // Read notices before constructing the next prompt, but do not consume
    // them yet: any prompt/history/provider-start failure must leave the same
    // stable ids available for a retry.
    let interruption_reminder = ChatInterruptionNoticeRepository::new(state.db().clone())
        .list_unconsumed(&session_id)
        .await
        .map_err(|e| {
            tracing::warn!(session_id=%session_id, error=%e, "failed to load chat interruption notices");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                format!("failed to load chat interruption notices: {e}"),
            )
        })
        .map(|notices| collapse_interruption_notices(&notices))?;
    let latest_user_already_persisted =
        last_user_content_for_persist
            .as_ref()
            .is_some_and(|content| {
                persisted_history.messages.last().is_some_and(|last| {
                    matches!(last.role, Role::User) && last.content.as_slice() == content.as_slice()
                })
            });

    let mut conversation = Conversation::new();
    // For a proposal-scoped chat the proposal overlay takes the client-system
    // slot (the client doesn't send one); it merges into the single chat system
    // message alongside the base prompt.
    let effective_system = proposal_system.as_deref().or(req.system.as_deref());
    let system_message = super::prompt::system_message::build_system_message(
        DJINN_CHAT_SYSTEM_PROMPT,
        codebase_header.as_deref(),
        effective_system,
        &req.model,
    );
    let (system_message, _chat_config) = apply_chat_skills(system_message).await;
    conversation.push(system_message);
    let interruption_notice_ids = if let Some(reminder) = interruption_reminder {
        // This must remain between the normal preamble and every persisted
        // message, including a projected compaction summary.
        conversation.push(reminder.message);
        reminder.notice_ids
    } else {
        Vec::new()
    };

    if persisted_history.messages.is_empty() {
        conversation.messages.extend(
            incoming_turns
                .into_iter()
                .filter(|m| !matches!(m.role, Role::System)),
        );
    } else {
        // Check whether load_conversation projected a completed boundary
        // summary. When present, the projected summary is a Role::System
        // message with marker metadata that must be preserved — the generic
        // Role::System filter would otherwise discard it.
        let has_projected_summary = persisted_history
            .messages
            .first()
            .is_some_and(is_projected_compaction_summary);
        conversation.messages.extend(
            persisted_history
                .messages
                .into_iter()
                .filter(|m| {
                    // Keep the projected summary; drop raw historical system messages.
                    !matches!(m.role, Role::System) || is_projected_compaction_summary(m)
                })
                .filter(|m| {
                    // When a projected boundary summary is present, exclude old
                    // summary-marker/continuation pairs so the summarizer sees
                    // prior summary through the projected path rather than as
                    // duplicate raw turns.
                    !has_projected_summary || !is_compaction_marker_pair(m)
                }),
        );
        if !latest_user_already_persisted
            && let Some(content) = last_user_content_for_persist.clone()
        {
            conversation.push(Message {
                role: Role::User,
                content,
                metadata: None,
            });
        }
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
    if !latest_user_already_persisted && let Some(ref content) = last_user_content_for_persist {
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
    let all_mcp_schemas = mcp.all_tool_schemas();
    let mut tool_schemas =
        djinn_agent::chat_tools::filter_chat_allowed_mcp_schemas(all_mcp_schemas.clone());
    tool_schemas.extend(djinn_agent::chat_tools::chat_extension_tool_schemas());
    // A proposal-scoped chat additionally gets the proposal-editing tools so
    // djinn can rewrite the spec and resolve feedback. Off the global allowlist
    // by design — added here only when this request resolved a proposal.
    let extra_allowed_mcp: Vec<String> = if proposal_system.is_some() {
        tool_schemas.extend(djinn_agent::chat_tools::filter_proposal_scoped_mcp_schemas(
            all_mcp_schemas,
        ));
        djinn_agent::chat_tools::PROPOSAL_SCOPED_MCP_TOOLS
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        Vec::new()
    };

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
    let revision_caller = user_id.clone().and_then(|user_id| {
        TrustedRevisionCallerContext::authenticated_human(user_id)
            .map(|context| context.with_execution_provenance(Some(session_id.clone()), None, None))
    });
    let model_for_title = req.model.clone();
    tokio::spawn(async move {
        // Scope the task-locals across the entire chat loop so any MCP
        // tool dispatch inside it sees the user's identity via
        // `current_user_id()` / `current_user_token()`.
        SESSION_USER_TOKEN
            .scope(
                user_token,
                SESSION_USER_ID.scope(
                    user_id,
                    REVISION_CALLER_CONTEXT.scope(
                        revision_caller,
                        run_chat_loop(ChatLoopContext {
                            state: spawn_state,
                            provider,
                            conversation,
                            tool_schemas,
                            resolver,
                            mcp,
                            tx,
                            session_id: session_id_for_loop,
                            needs_title,
                            user_turn_for_title,
                            model_id: model_for_title,
                            context_window,
                            extra_allowed_mcp,
                            interruption_notice_ids,
                        }),
                    ),
                ),
            )
            .await;
    });

    Ok(Sse::new(ReceiverStream::new(rx).map(Ok)).keep_alive(KeepAlive::default()))
}

/// Internal context bundle passed into `run_chat_loop` so the spawn
/// call and the loop body don't carry a 15-parameter positional list.
struct ChatLoopContext {
    state: AppState,
    provider: Box<dyn LlmProvider>,
    conversation: Conversation,
    tool_schemas: Vec<serde_json::Value>,
    resolver: Arc<ProjectResolver>,
    mcp: DjinnMcpServer,
    tx: tokio::sync::mpsc::Sender<Event>,
    session_id: String,
    needs_title: bool,
    user_turn_for_title: Option<Vec<ContentBlock>>,
    model_id: String,
    context_window: i64,
    /// Extra MCP tools allowed for this loop on top of the global chat allowlist
    /// (the proposal-editing subset for a proposal-scoped chat; empty otherwise).
    extra_allowed_mcp: Vec<String>,
    /// Stable notice ids included in the assembled first provider prompt.
    interruption_notice_ids: Vec<String>,
}

/// Dispatch a single tool call and return its `ToolResult` content block.
///
/// Sends `tool_call` and `tool_result` SSE events.  Handles stash tools,
/// chat-extension dispatch, allowed MCP dispatch (global + proposal-scoped),
/// gated unavailable tools, timeout wrapping, and result rendering/stashing.
#[allow(clippy::too_many_arguments)]
async fn dispatch_tool_call(
    id: String,
    name: String,
    input: serde_json::Value,
    agent_ctx: &djinn_agent::context::AgentContext,
    resolver: &Arc<ProjectResolver>,
    mcp: &DjinnMcpServer,
    extra_allowed_mcp: &[String],
    output_stash: &Arc<Mutex<djinn_agent::output_stash::OutputStash>>,
    tx: &tokio::sync::mpsc::Sender<Event>,
) -> ContentBlock {
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
    let started_at = SystemClockTrait::new().now_instant();

    // `output_view` / `output_grep` are served in-process against the
    // per-request stash — they never hit tool dispatch, and their
    // results are already size-bounded by the stash. Intercept before
    // the extension/MCP tiers (and before the timeout wrapper, since
    // there's nothing here that can wedge).
    if djinn_agent::chat_tools::is_chat_stash_tool(&name) {
        let (output, success) = match djinn_agent::output_stash::handle_stash_tool(
            output_stash,
            &name,
            args.as_object(),
        ) {
            Ok(text) => (text, true),
            Err(e) => (e, false),
        };
        let elapsed_ms = started_at.elapsed().as_millis() as u64;
        let result = ContentBlock::ToolResult {
            tool_use_id: id.clone(),
            content: vec![ContentBlock::text(output.clone())],
            is_error: !success,
        };
        let _ = tx
            .send(sse_json_event(
                "tool_result",
                &ToolResultPayload {
                    id,
                    output,
                    elapsed_ms,
                    success,
                    message: None,
                },
            ))
            .await;
        return result;
    }

    // Outer per-tool timeout. Defense-in-depth on top of the
    // op-specific timeouts inside `code_graph` dispatchers — any
    // tool that wedges (an LSP server hung on a 100k-line file,
    // a pathological SQL plan, an external API call without its
    // own timeout) returns a structured `is_error` tool_result
    // here instead of stalling the SSE stream forever. The
    // model gets a turn to recover. Override with
    // `DJINN_CHAT_TOOL_DISPATCH_TIMEOUT_SECS`; default 120s
    // gives the inner code_graph dispatcher's 60s comfortable
    // headroom plus margin for other tools.
    let tool_timeout = chat_tool_dispatch_timeout();
    let dispatch_future = async {
        if djinn_agent::chat_tools::is_chat_extension_tool(&name) {
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
                            dyn std::future::Future<Output = Result<ChatResolvedProject, String>>
                                + Send,
                        >,
                    >
            };
            djinn_agent::chat_tools::dispatch_chat_tool(agent_ctx, &name, args, &resolve_fn).await
        } else if djinn_agent::chat_tools::is_chat_allowed_mcp_tool(&name)
            || extra_allowed_mcp.iter().any(|t| t == &name)
        {
            mcp.dispatch_tool(&name, args).await
        } else {
            Err(format!(
                "tool '{name}' is not available from chat (admin or write tools are gated)"
            ))
        }
    };
    let dispatch_result = match tokio::time::timeout(tool_timeout, dispatch_future).await {
        Ok(r) => r,
        Err(_) => Err(format!(
            "tool '{name}' exceeded {}s — aborting so the chat can continue. \
             If this is expected for the input, narrow the call \
             (smaller scope, smaller limit, etc.)",
            tool_timeout.as_secs()
        )),
    };
    match dispatch_result {
        Ok(value) => {
            // Truncate-and-stash oversized results so neither the
            // persisted history nor the next provider request can carry
            // an unbounded string. The model browses the full output via
            // `output_view` / `output_grep` against `output_stash`.
            // Chat remains per-result-only for rdx6: it deliberately calls the
            // existing render_tool_result chokepoint and does not run the
            // turn-budget group post-pass planned for the worker reply loop in
            // sibling epic v9ie.
            let output =
                djinn_agent::output_stash::render_tool_result(output_stash, &id, &name, &value);
            let elapsed_ms = started_at.elapsed().as_millis() as u64;
            let result = ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: vec![ContentBlock::text(output.clone())],
                is_error: false,
            };
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
            result
        }
        Err(e) => {
            let elapsed_ms = started_at.elapsed().as_millis() as u64;
            tracing::warn!(tool=%name, error=%e, "tool dispatch failed");
            let result = ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: vec![ContentBlock::text(e.clone())],
                is_error: true,
            };
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
            result
        }
    }
}

/// Persist tool results as a paired user row and append to conversation.
///
/// The tool-result turn is written as a `user` row immediately after the
/// paired assistant turn so the persisted history always carries matching
/// `function_call` / `function_call_output` items.
async fn persist_and_append_tool_results(
    tool_results: Vec<ContentBlock>,
    state: &AppState,
    session_id: &str,
    conversation: &mut Conversation,
) {
    if tool_results.is_empty() {
        return;
    }
    // Persist the tool results as their own user row, paired with
    // the assistant turn above. Previously these were dropped,
    // leaving the persisted `function_call` orphaned and breaking
    // the next turn's request.
    persist_turn(state, session_id, "user", &tool_results).await;
    conversation.push(Message {
        role: Role::User,
        content: tool_results,
        metadata: None,
    });
}

/// Finalize a completed chat loop: generate/update session title and emit
/// the final `done` SSE event.
///
/// Title generation only runs on successful completion (`completed_ok`)
/// when the session was still on the default placeholder title.  Any
/// failure logs and falls through — the UI keeps rendering "New Chat"
/// until the next turn.
#[allow(clippy::too_many_arguments)]
async fn finalize_chat_completion(
    completed_ok: bool,
    needs_title: bool,
    user_turn_for_title: Option<Vec<ContentBlock>>,
    assistant_content_for_title: &[ContentBlock],
    model_id: &str,
    session_id: &str,
    state: &AppState,
    tx: &tokio::sync::mpsc::Sender<Event>,
) {
    if completed_ok && needs_title {
        let title = generate_chat_title(
            state,
            user_turn_for_title.as_deref(),
            assistant_content_for_title,
            model_id,
            session_id,
        )
        .await;
        if let Some(title) = title {
            let repo = SessionRepository::new(state.db().clone(), state.event_bus());
            if let Err(e) = repo.update_chat_title(session_id, &title).await {
                tracing::warn!(session_id=%session_id, error=%e, "failed to persist chat title");
            } else {
                let _ = tx
                    .send(sse_json_event(
                        "session_title",
                        &SessionTitlePayload {
                            session_id: session_id.to_string(),
                            title,
                        },
                    ))
                    .await;
            }
        }
    }

    let _ = tx.send(Event::default().event("done").data("{}")).await;
}

async fn run_chat_loop(ctx: ChatLoopContext) {
    let ChatLoopContext {
        state,
        provider,
        mut conversation,
        tool_schemas,
        resolver,
        mcp,
        tx,
        session_id,
        needs_title,
        user_turn_for_title,
        model_id,
        context_window,
        extra_allowed_mcp,
        mut interruption_notice_ids,
    } = ctx;
    let agent_ctx = state.agent_context();
    let mut loop_count = 0usize;
    // C3: bound the reactive compact-and-retry on a context-overflow stream
    // failure, mirroring the worker reply loop's MAX_COMPACTION_RETRIES.
    let mut compaction_attempts = 0u32;
    // Assistant content accumulated across every provider turn of the tool
    // loop, kept in memory only to seed the auto-title pass. Persistence is
    // incremental (see below): each assistant turn and its paired
    // tool-result user row are written to `session_messages` in
    // conversation order the moment they finalize, so a reload never yields
    // a `function_call` without its `function_call_output`.
    let mut assistant_content_for_title: Vec<ContentBlock> = Vec::new();
    let mut completed_ok = false;

    // Per-request stash for oversized tool results. Mirrors the worker reply
    // loop: a result over `MAX_TOOL_RESULT_CHARS` is stashed in full and the
    // model gets a truncated view plus an `output_view`/`output_grep` hint.
    // This is what keeps a 12 MB `code_graph`/`pr_review_context`/MCP result
    // from being persisted and replayed verbatim into the provider's `input`
    // array (the `string_above_max_length` 400). Lives for the whole tool loop
    // so `output_view`/`output_grep` resolve within the same user turn; across
    // turns the persisted text is already truncated and the serializer clamp in
    // `Conversation::to_openai_responses_input` is the final backstop.
    let output_stash = Arc::new(Mutex::new(djinn_agent::output_stash::OutputStash::new()));

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
                        message: format!("tool loop iteration cap reached ({MAX_TOOL_ITERATIONS})"),
                    },
                ))
                .await;
            break;
        }

        // C3: proactively compact before streaming. Chat reloads the full
        // persisted history every request, so a long conversation would
        // otherwise 400 on cumulative input. Reuses the worker reply loop's
        // compaction machinery (same 80%-of-window trigger). After a compaction
        // the in-memory estimate falls back below threshold, so this no-ops on
        // subsequent iterations within the same request.
        maybe_compact_proactively(
            &state,
            provider.as_ref(),
            &mut conversation,
            &session_id,
            context_window,
        )
        .await;

        let stream = match init_provider_stream(
            &state,
            provider.as_ref(),
            &mut conversation,
            &tool_schemas,
            &tx,
            &session_id,
            context_window,
            &mut compaction_attempts,
        )
        .await
        {
            Ok(s) => s,
            Err(StreamInitOutcome::CompactedAndContinue) => continue,
            Err(StreamInitOutcome::UnrecoverableBreak) => break,
        };

        // Provider stream creation is the model-call start boundary. Only now
        // consume exactly the durable ids which were included in the reminder.
        // A failed initialization above leaves this vector and the rows intact.
        let notice_repo = ChatInterruptionNoticeRepository::new(state.db().clone());
        if let Err(error) =
            consume_started_interruption_notices(&notice_repo, &mut interruption_notice_ids).await
        {
            tracing::warn!(
                session_id=%session_id,
                error=%error,
                "failed to consume started chat interruption notices"
            );
        }

        tokio::pin!(stream);
        let turn = drain_provider_turn(&mut stream, &tx, &state, &session_id).await;
        let (assistant_content, tool_calls) = match turn {
            TurnResult::Ok {
                assistant_content,
                tool_calls,
            } => (assistant_content, tool_calls),
            TurnResult::StreamError => return,
            TurnResult::Empty => return,
        };

        // Persist this assistant turn immediately, in the exact shape it
        // will be replayed. Its tool-call results are written as a paired
        // user row right after they're computed (below), so the persisted
        // history always carries matching function_call /
        // function_call_output items — the fix for the "No tool output
        // found for function call" 400 on follow-up turns.
        persist_turn(&state, &session_id, "assistant", &assistant_content).await;
        assistant_content_for_title.extend(assistant_content.clone());
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
            let result = dispatch_tool_call(
                id,
                name,
                input,
                &agent_ctx,
                &resolver,
                &mcp,
                &extra_allowed_mcp,
                &output_stash,
                &tx,
            )
            .await;
            tool_results.push(result);
        }
        persist_and_append_tool_results(tool_results, &state, &session_id, &mut conversation).await;
    }

    // Every turn was persisted incrementally as it finalized, so there is
    // nothing left to flush here before the title / done events.
    finalize_chat_completion(
        completed_ok,
        needs_title,
        user_turn_for_title,
        &assistant_content_for_title,
        &model_id,
        &session_id,
        &state,
        &tx,
    )
    .await;
}

/// Build the proposal-scoped system overlay: the rendered spec + the list of
/// unresolved feedback, substituted into [`PROPOSAL_ADDRESS_SYSTEM_PROMPT`].
/// Returns `None` if the proposal can't be resolved (handled as a 404 upstream).
async fn build_proposal_address_prompt(
    state: &AppState,
    proposal_ref: &str,
    feedback_id: Option<&str>,
) -> Option<String> {
    let repo = ProposalRepository::new(state.db().clone(), state.event_bus());
    let proposal = repo.resolve(proposal_ref).await.ok().flatten()?;
    let feedback = repo.feedback(&proposal.id).await.unwrap_or_default();

    let mut ctx = String::new();
    ctx.push_str(&format!(
        "**{}** (`{}`) — status: {}, head revision: {}\n\n## Current spec\n\n{}\n",
        proposal.title,
        proposal.short_id,
        proposal.status,
        proposal.latest_revision_seq,
        if proposal.body.trim().is_empty() {
            "_(empty)_"
        } else {
            proposal.body.trim()
        },
    ));

    let ac = djinn_core::models::parse_json_array(&proposal.acceptance_criteria);
    if !ac.is_empty() {
        ctx.push_str("\n## Acceptance criteria\n\n");
        for c in &ac {
            ctx.push_str(&format!("- {c}\n"));
        }
    }

    let unresolved: Vec<_> = feedback
        .iter()
        .filter(|f| f.resolved_at.is_none())
        .collect();
    ctx.push_str("\n## Unresolved feedback\n\n");
    if unresolved.is_empty() {
        ctx.push_str("_(none)_\n");
    } else {
        for f in &unresolved {
            let who = if f.author_kind == "ai" {
                format!("AI ({})", f.author_model.as_deref().unwrap_or("model"))
            } else {
                "reviewer".to_string()
            };
            let marker = if Some(f.id.as_str()) == feedback_id {
                " ← the one the user opened this chat to address"
            } else {
                ""
            };
            ctx.push_str(&format!(
                "### Feedback `{}` — from {}{}\n\n{}\n\n",
                f.id,
                who,
                marker,
                f.body.trim()
            ));
        }
    }

    Some(PROPOSAL_ADDRESS_SYSTEM_PROMPT.replace("{{PROPOSAL_CONTEXT}}", &ctx))
}

/// Persist one conversation turn to `session_messages`.
///
/// Schema stored in `content_json` is `[ContentBlock, …]`, where each
/// element is a provider-native `djinn_provider::message::ContentBlock`.
/// Assistant turns keep every `ToolUse` block (with its full `input` JSON)
/// so the UI can reconstruct the tool-call args on reload; the matching
/// tool-result turn is persisted as a `user` row holding the `ToolResult`
/// blocks. Writing both, in conversation order, is what keeps a reloaded
/// history's `function_call` / `function_call_output` items paired. Empty
/// turns (no content blocks) are skipped.
async fn persist_turn(state: &AppState, session_id: &str, role: &str, content: &[ContentBlock]) {
    if content.is_empty() {
        return;
    }
    let repo = SessionMessageRepository::new(state.db().clone(), state.event_bus());
    let content_json = serde_json::to_string(content).unwrap_or_else(|_| "[]".to_string());
    if let Err(e) = repo
        .insert_message(session_id, "", role, &content_json, None)
        .await
    {
        tracing::warn!(session_id=%session_id, role=%role, error=%e, "failed to persist chat turn");
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
        ..Default::default()
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
                reasoning_effort: None,
                tool_schema_compat: None,
            }
        }
    };

    // B5a: chat-title generation is a cheap, throwaway background call (3-6
    // words). Force the weakest reasoning tier so it doesn't burn deep-thinking
    // tokens/latency, regardless of what the main chat model is configured for.
    // The OAuth branch above may have inherited a `reasoning_effort` from the
    // reused config, so override unconditionally after the match.
    let mut provider_config = provider_config;
    provider_config.reasoning_effort = Some(djinn_provider::provider::ReasoningEffort::Minimal);

    let provider = create_chat_provider(provider_config);
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
            Ok(StreamEvent::Delta(_))
            | Ok(StreamEvent::Usage(_))
            | Ok(StreamEvent::Thinking(_))
            | Ok(StreamEvent::ThinkingDelta { .. })
            | Ok(StreamEvent::ThinkingBlockComplete { .. }) => {}
            Err(e) => {
                tracing::warn!(session_id=%session_id, error=%e, "title-gen: stream event failed");
                return None;
            }
        }
    }

    let cleaned = clean_generated_title(&text);
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;
    use djinn_core::message::MessageMeta;
    use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};
    use serde_json;
    use tokio_util::sync::CancellationToken;

    struct SuccessfulProvider;

    impl LlmProvider for SuccessfulProvider {
        fn name(&self) -> &str {
            "successful-test-provider"
        }

        fn stream<'a>(
            &'a self,
            _conversation: &'a Conversation,
            _tools: &'a [serde_json::Value],
            _tool_choice: Option<ToolChoice>,
        ) -> std::pin::Pin<
            Box<
                dyn futures::Future<
                        Output = anyhow::Result<
                            std::pin::Pin<
                                Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                            >,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async {
                Ok(Box::pin(futures::stream::iter(vec![Ok(StreamEvent::Done)]))
                    as std::pin::Pin<
                        Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                    >)
            })
        }
    }

    struct FailingProvider;

    impl LlmProvider for FailingProvider {
        fn name(&self) -> &str {
            "failing-test-provider"
        }

        fn stream<'a>(
            &'a self,
            _conversation: &'a Conversation,
            _tools: &'a [serde_json::Value],
            _tool_choice: Option<ToolChoice>,
        ) -> std::pin::Pin<
            Box<
                dyn futures::Future<
                        Output = anyhow::Result<
                            std::pin::Pin<
                                Box<dyn futures::Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                            >,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async { Err(anyhow::anyhow!("provider unavailable")) })
        }
    }

    fn compaction_marker_metadata() -> serde_json::Value {
        serde_json::json!({
            "marker_kind": PROJECTED_COMPACTION_MARKER_KIND,
            "end_marker": COMPACTION_SUMMARY_END_MARKER,
        })
    }

    fn projected_summary_message(text: &str) -> Message {
        let marker = compaction_marker_metadata();
        let metadata = MessageMeta {
            input_tokens: None,
            output_tokens: None,
            timestamp: None,
            provider_data: Some(marker),
        };
        Message::system_with_metadata(text, metadata)
    }

    fn interruption_notice(
        id: &str,
        discarded_tool_calls_count: i32,
        interrupted_at: &str,
    ) -> ChatInterruptionNotice {
        ChatInterruptionNotice {
            interruption_notice_id: id.to_owned(),
            session_id: "session".to_owned(),
            session_message_id: None,
            interrupted_turn: true,
            discarded_tool_calls_count,
            interrupted_at: interrupted_at.to_owned(),
            consumed_at: None,
        }
    }

    #[tokio::test]
    async fn successful_provider_start_consumes_included_notices_once() {
        let db = test_helpers::create_test_db();
        let session_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_chat_session_row(&db, &session_id).await;
        let repo = ChatInterruptionNoticeRepository::new(db.clone());
        let first = repo
            .create(CreateChatInterruptionNotice {
                session_id: &session_id,
                session_message_id: None,
                discarded_tool_calls_count: 1,
            })
            .await
            .unwrap();
        let second = repo
            .create(CreateChatInterruptionNotice {
                session_id: &session_id,
                session_message_id: None,
                discarded_tool_calls_count: 2,
            })
            .await
            .unwrap();
        let mut included_ids =
            collapse_interruption_notices(&repo.list_unconsumed(&session_id).await.unwrap())
                .expect("reminder for notices")
                .notice_ids;

        let state = AppState::new(db, CancellationToken::new());
        let provider = SuccessfulProvider;
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut conversation = Conversation::new();
        conversation.push(Message::system("preamble"));
        assert!(
            init_provider_stream(
                &state,
                &provider,
                &mut conversation,
                &[],
                &tx,
                &session_id,
                128_000,
                &mut 0,
            )
            .await
            .is_ok()
        );

        consume_started_interruption_notices(&repo, &mut included_ids)
            .await
            .expect("successful provider start consumes included ids");
        assert!(included_ids.is_empty());
        assert!(repo.list_unconsumed(&session_id).await.unwrap().is_empty());
        assert!(
            collapse_interruption_notices(&repo.list_unconsumed(&session_id).await.unwrap())
                .is_none()
        );
        assert_ne!(first.interruption_notice_id, second.interruption_notice_id);
    }

    #[tokio::test]
    async fn failed_provider_start_leaves_notices_retryable() {
        let db = test_helpers::create_test_db();
        let session_id = uuid::Uuid::now_v7().to_string();
        djinn_db::test_support::seed_chat_session_row(&db, &session_id).await;
        let repo = ChatInterruptionNoticeRepository::new(db.clone());
        let first = repo
            .create(CreateChatInterruptionNotice {
                session_id: &session_id,
                session_message_id: None,
                discarded_tool_calls_count: 2,
            })
            .await
            .unwrap();
        let second = repo
            .create(CreateChatInterruptionNotice {
                session_id: &session_id,
                session_message_id: None,
                discarded_tool_calls_count: 3,
            })
            .await
            .unwrap();
        let initial_reminder =
            collapse_interruption_notices(&repo.list_unconsumed(&session_id).await.unwrap())
                .expect("reminder for notices");

        let state = AppState::new(db, CancellationToken::new());
        let provider = FailingProvider;
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut conversation = Conversation::new();
        conversation.push(Message::system("preamble"));
        assert!(matches!(
            init_provider_stream(
                &state,
                &provider,
                &mut conversation,
                &[],
                &tx,
                &session_id,
                128_000,
                &mut 0,
            )
            .await,
            Err(StreamInitOutcome::UnrecoverableBreak)
        ));

        // The failed-init branch must not consume: retry includes the same ids.
        let retry_reminder =
            collapse_interruption_notices(&repo.list_unconsumed(&session_id).await.unwrap())
                .expect("failed start leaves reminder retryable");
        assert_eq!(retry_reminder.notice_ids, initial_reminder.notice_ids);
        assert!(
            retry_reminder
                .message
                .text_content()
                .contains("5 pending tool call(s)")
        );
        assert!(
            retry_reminder
                .notice_ids
                .contains(&first.interruption_notice_id)
        );
        assert!(
            retry_reminder
                .notice_ids
                .contains(&second.interruption_notice_id)
        );
    }

    #[test]
    fn interruption_reminder_aggregates_and_precedes_projected_transcript() {
        let notices = vec![
            interruption_notice("notice-1", 2, "2026-07-12T01:00:00.000Z"),
            interruption_notice("notice-2", 3, "2026-07-12T02:00:00.000Z"),
        ];
        let reminder = collapse_interruption_notices(&notices).expect("notices produce reminder");

        assert_eq!(reminder.notice_ids, vec!["notice-1", "notice-2"]);
        assert!(
            reminder
                .message
                .text_content()
                .contains("5 pending tool call(s)")
        );
        assert!(
            reminder
                .message
                .text_content()
                .contains("2026-07-12T02:00:00.000Z")
        );

        let mut conversation = Conversation::new();
        conversation.push(Message::system("normal system and developer preamble"));
        conversation.push(reminder.message);
        conversation.push(projected_summary_message("persisted compaction summary"));
        conversation.push(Message::user("persisted user turn"));

        assert_eq!(
            conversation.messages[0].text_content(),
            "normal system and developer preamble"
        );
        assert!(
            conversation.messages[1]
                .text_content()
                .contains("interrupted")
        );
        assert_eq!(
            conversation.messages[2].text_content(),
            "persisted compaction summary"
        );
        assert_eq!(
            conversation.messages[3].text_content(),
            "persisted user turn"
        );
    }

    #[test]
    fn interrupted_notice_predicate_requires_partial_content_or_discarded_tools() {
        assert!(should_persist_interruption_notice(
            &[ContentBlock::Text {
                text: "partial assistant text".to_owned(),
            }],
            0,
        ));
        assert!(should_persist_interruption_notice(&[], 2));
        assert!(!should_persist_interruption_notice(&[], 0));
    }

    #[test]
    fn is_projected_compaction_summary_recognises_marker_metadata() {
        let msg = projected_summary_message("earlier turns summary");
        assert!(is_projected_compaction_summary(&msg));
    }

    #[test]
    fn is_projected_compaction_summary_rejects_raw_system_message() {
        let msg = Message::system("You are a helpful assistant.");
        assert!(!is_projected_compaction_summary(&msg));
    }

    #[test]
    fn is_projected_compaction_summary_rejects_user_and_assistant() {
        let user_msg = Message::user("hello");
        let asst_msg = Message::assistant("hi");
        assert!(!is_projected_compaction_summary(&user_msg));
        assert!(!is_projected_compaction_summary(&asst_msg));
    }

    #[test]
    fn is_compaction_marker_pair_recognises_user_summary_marker() {
        let msg = Message::user(format!("summary{COMPACTION_SUMMARY_END_MARKER}"));
        assert!(is_compaction_marker_pair(&msg));
    }

    #[test]
    fn is_compaction_marker_pair_recognises_assistant_full_continuation() {
        let msg = Message::assistant(
            "Your context was compacted. The previous message contains a summary of the conversation so far. Continue calling tools as necessary to complete the task.",
        );
        assert!(is_compaction_marker_pair(&msg));
    }

    #[test]
    fn is_compaction_marker_pair_recognises_assistant_partial_continuation() {
        let msg = Message::assistant(
            "Part of your context was compacted. The messages above the summary are older context.",
        );
        assert!(is_compaction_marker_pair(&msg));
    }

    #[test]
    fn is_compaction_marker_pair_rejects_normal_user_and_assistant() {
        let user_msg = Message::user("hello");
        let asst_msg = Message::assistant("hi there");
        assert!(!is_compaction_marker_pair(&user_msg));
        assert!(!is_compaction_marker_pair(&asst_msg));
    }

    #[test]
    fn is_compaction_marker_pair_rejects_raw_system_message() {
        let msg = Message::system("some system text");
        assert!(!is_compaction_marker_pair(&msg));
    }

    #[test]
    fn projected_summary_survives_system_filter() {
        // Simulates the assembled persisted_history when a completed boundary
        // exists: projected summary, marker pair, retained tail.
        let persisted = vec![
            projected_summary_message("Compacted summary of earlier turns."),
            Message::user(format!("old summary{COMPACTION_SUMMARY_END_MARKER}")),
            Message::assistant(
                "Your context was compacted. The previous message contains a summary of the conversation so far. Continue calling tools as necessary to complete the task.",
            ),
            Message::user("What about Rust lifetimes?"),
            Message::assistant("Rust lifetimes ensure references are valid."),
        ];

        let has_projected_summary = persisted
            .first()
            .is_some_and(is_projected_compaction_summary);
        assert!(has_projected_summary);

        let assembled: Vec<Message> = persisted
            .into_iter()
            .filter(|m| !matches!(m.role, Role::System) || is_projected_compaction_summary(m))
            .filter(|m| !has_projected_summary || !is_compaction_marker_pair(m))
            .collect();

        // Projected summary survives.
        assert_eq!(assembled.len(), 3);
        assert_eq!(assembled[0].role, Role::System);
        assert_eq!(
            assembled[0].text_content(),
            "Compacted summary of earlier turns."
        );
        assert!(is_projected_compaction_summary(&assembled[0]));

        // Stale marker pair excluded.
        assert_eq!(assembled[1].role, Role::User);
        assert_eq!(assembled[1].text_content(), "What about Rust lifetimes?");
        assert_eq!(assembled[2].role, Role::Assistant);
        assert_eq!(
            assembled[2].text_content(),
            "Rust lifetimes ensure references are valid."
        );
    }

    #[test]
    fn no_boundary_still_filters_raw_system_messages() {
        // Simulates a raw-history conversation with no projected boundary.
        let persisted = vec![
            Message::system("You are a helpful assistant."),
            Message::user("hello"),
            Message::assistant("hi there"),
        ];

        let has_projected_summary = persisted
            .first()
            .is_some_and(is_projected_compaction_summary);
        assert!(!has_projected_summary);

        let assembled: Vec<Message> = persisted
            .into_iter()
            .filter(|m| !matches!(m.role, Role::System) || is_projected_compaction_summary(m))
            .filter(|m| !has_projected_summary || !is_compaction_marker_pair(m))
            .collect();

        // Raw system message is filtered.
        assert_eq!(assembled.len(), 2);
        assert_eq!(assembled[0].role, Role::User);
        assert_eq!(assembled[0].text_content(), "hello");
        assert_eq!(assembled[1].role, Role::Assistant);
        assert_eq!(assembled[1].text_content(), "hi there");
    }

    #[test]
    fn boundary_with_raw_system_in_tail_filters_stale_system_and_markers() {
        // Edge case: boundary projection falls back to full raw history
        // (e.g. tail identity mismatch). Both the stale system row and the
        // old marker pair must be excluded, but the projected summary survives.
        let persisted = vec![
            projected_summary_message("Compacted summary."),
            Message::system("You are a helpful assistant."), // stale raw system
            Message::user(format!("old summary{COMPACTION_SUMMARY_END_MARKER}")),
            Message::assistant(
                "Your context was compacted. The previous message contains a summary of the conversation so far. Continue calling tools as necessary to complete the task.",
            ),
            Message::user("latest user turn"),
            Message::assistant("latest assistant turn"),
        ];

        let has_projected_summary = persisted
            .first()
            .is_some_and(is_projected_compaction_summary);
        assert!(has_projected_summary);

        let assembled: Vec<Message> = persisted
            .into_iter()
            .filter(|m| !matches!(m.role, Role::System) || is_projected_compaction_summary(m))
            .filter(|m| !has_projected_summary || !is_compaction_marker_pair(m))
            .collect();

        assert_eq!(assembled.len(), 3);
        assert_eq!(assembled[0].text_content(), "Compacted summary.");
        assert_eq!(assembled[1].text_content(), "latest user turn");
        assert_eq!(assembled[2].text_content(), "latest assistant turn");
    }

    // ---- handler thinking/provider-state regression (nbky) ---------------
    //
    // Verify that assistant messages containing the new provider-state
    // ContentBlock variants (Thinking with signature, RedactedThinking,
    // Unknown/passthrough) survive the history assembly filtering unchanged.

    /// Assistant messages carrying signed Thinking, RedactedThinking, and
    /// Unknown passthrough blocks must survive the assembly filter as-is.
    /// The filter only removes system messages and compaction markers; it
    /// must not strip or transform provider-state content blocks.
    #[test]
    fn assembly_preserves_provider_state_content_blocks() {
        let mut extra = serde_json::Map::new();
        extra.insert("provider_flag".into(), serde_json::json!(true));

        let persisted = vec![
            Message::system("You are a helpful assistant."),
            Message::user("Explain quantum computing."),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "Let me reason about this...".into(),
                        signature: Some("sig_test_123".into()),
                    },
                    ContentBlock::RedactedThinking {
                        data: "b3BhcXVlX2Jsb2I=".into(),
                    },
                    ContentBlock::Text {
                        text: "Quantum computing uses qubits.".into(),
                    },
                    ContentBlock::Unknown {
                        content_type: "provider_custom".into(),
                        extra,
                    },
                ],
                metadata: None,
            },
        ];

        let has_projected_summary = persisted
            .first()
            .is_some_and(is_projected_compaction_summary);
        assert!(!has_projected_summary);

        let assembled: Vec<Message> = persisted
            .into_iter()
            .filter(|m| !matches!(m.role, Role::System) || is_projected_compaction_summary(m))
            .filter(|m| !has_projected_summary || !is_compaction_marker_pair(m))
            .collect();

        // System filtered out; user + assistant survive.
        assert_eq!(assembled.len(), 2);
        assert_eq!(assembled[0].role, Role::User);
        assert_eq!(assembled[1].role, Role::Assistant);

        // All four content blocks must be preserved in order.
        let blocks = &assembled[1].content;
        assert_eq!(blocks.len(), 4, "all content blocks must survive assembly");

        match &blocks[0] {
            ContentBlock::Thinking {
                thinking,
                signature,
            } => {
                assert_eq!(thinking, "Let me reason about this...");
                assert_eq!(signature.as_deref(), Some("sig_test_123"));
            }
            other => panic!("expected signed Thinking, got: {other:?}"),
        }
        match &blocks[1] {
            ContentBlock::RedactedThinking { data } => {
                assert_eq!(data, "b3BhcXVlX2Jsb2I=");
            }
            other => panic!("expected RedactedThinking, got: {other:?}"),
        }
        assert!(
            matches!(&blocks[2], ContentBlock::Text { text } if text == "Quantum computing uses qubits."),
            "text block must survive in order"
        );
        match &blocks[3] {
            ContentBlock::Unknown {
                content_type,
                extra,
            } => {
                assert_eq!(content_type, "provider_custom");
                assert_eq!(extra.get("provider_flag"), Some(&serde_json::json!(true)));
            }
            other => panic!("expected Unknown block, got: {other:?}"),
        }
    }

    /// The `incoming_to_content_blocks` conversion only produces Text, Image,
    /// and Document blocks. It must never produce Thinking, RedactedThinking,
    /// or Unknown blocks — those are provider-stream-only.
    #[test]
    fn incoming_content_blocks_never_produce_provider_state_variants() {
        use super::ChatContent;
        use super::ChatContentBlock;

        let content = ChatContent::Blocks(vec![
            ChatContentBlock::Text {
                text: "hello".into(),
            },
            ChatContentBlock::Image {
                media_type: "image/png".into(),
                data: "base64data".into(),
            },
            ChatContentBlock::Document {
                media_type: "application/pdf".into(),
                data: "pdfdata".into(),
                filename: Some("doc.pdf".into()),
            },
        ]);

        let blocks = incoming_to_content_blocks(content);
        for block in &blocks {
            match block {
                ContentBlock::Text { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::Document { .. } => {}
                ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::Unknown { .. }
                | ContentBlock::ToolUse { .. }
                | ContentBlock::ToolResult { .. }
                | ContentBlock::OpenAIReasoning { .. } => {
                    panic!("incoming_to_content_blocks produced provider-state variant: {block:?}");
                }
            }
        }
    }
}
