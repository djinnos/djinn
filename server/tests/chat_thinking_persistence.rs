//! Exact thinking-persistence behavior tests for the server chat handler.
//!
//! These tests exercise the *production* `drain_provider_turn` path through
//! the HTTP chat-completions endpoint, using registered test providers that
//! emit explicit `StreamEvent` sequences. They assert the exact persisted
//! `ContentBlock` arrays and SSE side effects for the Done and stream-error
//! paths.
//!
//! Coverage:
//! - Signed thinking completion persists once on Done.
//! - Signed A + partial B + stream error persists as: signed A, unsigned B
//!   partial, then partial text — with buffered tools absent (orphan-tool
//!   suppression).
//! - SSE text/error behavior is unchanged while attributed thinking does not
//!   produce a duplicate SSE text delta.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use futures::Stream;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use djinn_db::{SessionMessageRepository, SessionRepository};
use djinn_provider::message::ContentBlock;
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};
use djinn_provider::repos::CredentialRepository;
use djinn_server::events::EventBus;
use djinn_server::server::chat::handler::register_test_provider;
use djinn_server::test_helpers;

// ─── test provider ────────────────────────────────────────────────────────────

/// A test provider that emits a scripted sequence of stream events.
struct ScriptedProvider {
    events: Arc<Mutex<Vec<Result<StreamEvent, anyhow::Error>>>>,
}

impl LlmProvider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted-chat-test-provider"
    }

    fn stream<'a>(
        &'a self,
        _conversation: &'a djinn_provider::message::Conversation,
        _: &'a [Value],
        _: Option<ToolChoice>,
    ) -> Pin<
        Box<
            dyn futures::Future<
                    Output = anyhow::Result<
                        Pin<Box<dyn Stream<Item = anyhow::Result<StreamEvent>> + Send>>,
                    >,
                > + Send
                + 'a,
        >,
    > {
        let events = std::mem::take(&mut *self.events.lock().unwrap());
        Box::pin(async move {
            Ok(Box::pin(futures::stream::iter(events))
                as Pin<
                    Box<dyn Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                >)
        })
    }
}

/// Build a `ScriptedProvider` from an event list and register it for the
/// given session id.
fn install(session_id: &str, events: Vec<Result<StreamEvent, anyhow::Error>>) {
    let events = Arc::new(Mutex::new(events));
    register_test_provider(session_id, move || {
        Box::new(ScriptedProvider {
            events: events.clone(),
        })
    });
}

// ─── HTTP helpers ─────────────────────────────────────────────────────────────

async fn setup() -> (djinn_db::Database, String) {
    let db = test_helpers::create_test_db();
    CredentialRepository::new(db.clone(), EventBus::noop())
        .set("openai", "OPENAI_API_KEY", "test")
        .await
        .unwrap();
    let session_id = uuid::Uuid::now_v7().to_string();
    let sessions = SessionRepository::new(db.clone(), EventBus::noop());
    sessions
        .upsert_chat_session(&session_id, "openai/gpt-4o-mini")
        .await
        .unwrap();
    sessions
        .update_chat_title(&session_id, "existing")
        .await
        .unwrap();
    (db, session_id)
}

async fn post_chat(
    db: djinn_db::Database,
    session_id: &str,
    text: &str,
) -> (axum::http::StatusCode, String) {
    let app = test_helpers::create_test_app_with_db(db);
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/chat/completions")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(
            json!({
                "model": "openai/gpt-4o-mini",
                "session_id": session_id,
                "messages": [{"role": "user", "content": text}],
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

/// Read the persisted assistant message's content blocks from the raw
/// conversation (bypasses the API's thinking-block redaction).
async fn assistant_content_blocks(db: &djinn_db::Database, session_id: &str) -> Vec<ContentBlock> {
    let msg_repo = SessionMessageRepository::new(db.clone(), EventBus::noop());
    let conversation = msg_repo
        .load_raw_conversation(session_id)
        .await
        .expect("load_raw_conversation");
    conversation
        .messages
        .iter()
        .find(|m| m.role == djinn_provider::message::Role::Assistant)
        .expect("assistant message must be persisted")
        .content
        .clone()
}

/// Assert that a `ContentBlock` is `Thinking` with the given text and signature.
fn assert_thinking(block: &ContentBlock, expected_text: &str, expected_sig: Option<&str>) {
    match block {
        ContentBlock::Thinking {
            thinking,
            signature,
        } => {
            assert_eq!(thinking, expected_text, "thinking text mismatch: {block:?}");
            assert_eq!(
                signature.as_deref(),
                expected_sig,
                "sig mismatch: {block:?}"
            );
        }
        other => panic!("expected Thinking block, got {other:?}"),
    }
}

/// Parse SSE body into a list of (event_type, json_payload) pairs.
fn parse_sse_events(body: &str) -> Vec<(String, Value)> {
    let mut events = Vec::new();
    let mut event_type = String::new();
    let mut data_lines: Vec<String> = Vec::new();

    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event_type = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start().to_string());
        } else if line.is_empty() && !data_lines.is_empty() {
            let payload = data_lines.join("\n").trim().to_string();
            if let Ok(value) = serde_json::from_str::<Value>(&payload) {
                events.push((event_type.clone(), value));
            }
            data_lines.clear();
            event_type.clear();
        }
    }

    if !data_lines.is_empty() {
        let payload = data_lines.join("\n").trim().to_string();
        if let Ok(value) = serde_json::from_str::<Value>(&payload) {
            events.push((event_type.clone(), value));
        }
    }

    events
}

// ─── tests ────────────────────────────────────────────────────────────────────

/// Signed thinking completion persists once on Done.
///
/// Event stream: ThinkingDelta(id=0, "thinking-A"), ThinkingBlockComplete(id=0,
/// "thinking-A", Some("sig-a")), Delta(Text("response")), Done.
///
/// Persisted assistant content must be exactly:
/// `[Thinking("thinking-A", Some("sig-a")), Text("response")]`.
///
/// The ThinkingDelta fragment is suppressed by exact ID because its completion
/// landed. The completion appears once in provider state. No unsigned fallback
/// block is produced.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signed_completion_persists_once_on_done() {
    let (db, session_id) = setup().await;
    install(
        &session_id,
        vec![
            Ok(StreamEvent::ThinkingDelta {
                id: 0,
                text: "thinking-A".to_string(),
            }),
            Ok(StreamEvent::ThinkingBlockComplete {
                id: 0,
                thinking: "thinking-A".to_string(),
                signature: Some("sig-a".to_string()),
            }),
            Ok(StreamEvent::Delta(ContentBlock::text("response"))),
            Ok(StreamEvent::Done),
        ],
    );

    let (status, _) = post_chat(db.clone(), &session_id, "hello").await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let content = assistant_content_blocks(&db, &session_id).await;

    // [Thinking(thinking-A, sig-a), Text(response)]
    assert_eq!(content.len(), 2, "exact content: {content:?}");
    assert_thinking(&content[0], "thinking-A", Some("sig-a"));
    assert!(matches!(&content[1], ContentBlock::Text { text } if text == "response"));
}

/// Signed A + partial B + stream error persists as: signed A, unsigned B
/// partial, then partial text — with buffered tools absent.
///
/// Event stream: ThinkingDelta(id=0, "A"), ThinkingBlockComplete(id=0, "A",
/// sig-a), ThinkingDelta(id=1, "Bpartial"), Delta(Text("partial response")),
/// Delta(ToolUse), Err(stream error).
///
/// Persisted assistant content must be exactly:
/// `[Thinking("A", Some("sig-a")), Thinking("Bpartial", None),
///  Text("partial response")]`.
///
/// The ToolUse is suppressed (orphan-tool suppression: a function_call with no
/// function_call_output wedges the next turn).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn signed_a_partial_b_stream_error_persists_canonical() {
    let (db, session_id) = setup().await;
    install(
        &session_id,
        vec![
            Ok(StreamEvent::ThinkingDelta {
                id: 0,
                text: "A".to_string(),
            }),
            Ok(StreamEvent::ThinkingBlockComplete {
                id: 0,
                thinking: "A".to_string(),
                signature: Some("sig-a".to_string()),
            }),
            Ok(StreamEvent::ThinkingDelta {
                id: 1,
                text: "Bpartial".to_string(),
            }),
            Ok(StreamEvent::Delta(ContentBlock::text("partial response"))),
            Ok(StreamEvent::Delta(ContentBlock::ToolUse {
                id: "call-1".to_string(),
                name: "memory_search".to_string(),
                input: json!({"q": "discard me"}),
            })),
            Err(anyhow::anyhow!("deliberate stream interruption")),
        ],
    );

    let (status, body) = post_chat(db.clone(), &session_id, "hello").await;
    assert_eq!(status, axum::http::StatusCode::OK);

    // The SSE stream must contain an error event.
    assert!(
        body.contains("error"),
        "SSE body must contain error event: {body}"
    );

    let api_content = assistant_content_blocks(&db, &session_id).await;

    // [Thinking(A, sig-a), Thinking(Bpartial, None), Text(partial response)]
    assert_eq!(api_content.len(), 3, "exact content: {api_content:?}");
    assert_thinking(&api_content[0], "A", Some("sig-a"));
    assert_thinking(&api_content[1], "Bpartial", None);
    assert!(matches!(&api_content[2], ContentBlock::Text { text } if text == "partial response"));

    // Orphan-tool suppression: no ToolUse in the persisted content.
    let has_tool = api_content
        .iter()
        .any(|b| matches!(b, ContentBlock::ToolUse { .. }));
    assert!(
        !has_tool,
        "buffered tool calls must be absent from persisted content: {api_content:?}"
    );
}

/// SSE text behavior is unchanged: a plain text completion still emits
/// `delta` SSE events with the text, and a final `done` event.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_text_behavior_unchanged_on_plain_completion() {
    let (db, session_id) = setup().await;
    install(
        &session_id,
        vec![
            Ok(StreamEvent::Delta(ContentBlock::text("hello"))),
            Ok(StreamEvent::Delta(ContentBlock::text(" world"))),
            Ok(StreamEvent::Done),
        ],
    );

    let (_, body) = post_chat(db, &session_id, "test").await;
    let events = parse_sse_events(&body);

    // Must contain delta events with the text fragments.
    let delta_texts: Vec<&str> = events
        .iter()
        .filter(|(ev, _)| ev == "delta")
        .map(|(_, v)| v["text"].as_str().unwrap_or(""))
        .collect();
    assert!(
        delta_texts.contains(&"hello"),
        "SSE must emit delta with 'hello': {delta_texts:?}"
    );
    assert!(
        delta_texts.contains(&" world"),
        "SSE must emit delta with ' world': {delta_texts:?}"
    );
}

/// Attributed thinking does not produce a duplicate SSE text delta.
///
/// ThinkingDelta and Thinking events must NOT emit SSE `delta` text events —
/// only `Delta(Text)` does.  This proves that attributed thinking is not
/// confused with assistant text on the SSE stream.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn attributed_thinking_does_not_emit_duplicate_sse_text_delta() {
    let (db, session_id) = setup().await;
    install(
        &session_id,
        vec![
            Ok(StreamEvent::ThinkingDelta {
                id: 0,
                text: "thinking-delta-text".to_string(),
            }),
            Ok(StreamEvent::ThinkingBlockComplete {
                id: 0,
                thinking: "thinking-delta-text".to_string(),
                signature: Some("sig".to_string()),
            }),
            Ok(StreamEvent::Delta(ContentBlock::text("actual response"))),
            Ok(StreamEvent::Done),
        ],
    );

    let (_, body) = post_chat(db, &session_id, "test").await;
    let events = parse_sse_events(&body);

    // The SSE stream must NOT contain a delta with the thinking text.
    let delta_texts: Vec<&str> = events
        .iter()
        .filter(|(ev, _)| ev == "delta")
        .map(|(_, v)| v["text"].as_str().unwrap_or(""))
        .collect();
    assert!(
        !delta_texts.contains(&"thinking-delta-text"),
        "attributed thinking must NOT produce an SSE text delta: {delta_texts:?}"
    );

    // It must contain the actual response text.
    assert!(
        delta_texts.contains(&"actual response"),
        "SSE must emit delta with actual response: {delta_texts:?}"
    );
}

/// SSE error behavior is unchanged: a stream error still emits an `error` SSE
/// event with the provider error message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sse_error_behavior_unchanged_on_stream_error() {
    let (db, session_id) = setup().await;
    install(
        &session_id,
        vec![
            Ok(StreamEvent::Delta(ContentBlock::text("partial"))),
            Err(anyhow::anyhow!("provider stream error")),
        ],
    );

    let (_, body) = post_chat(db, &session_id, "test").await;
    let events = parse_sse_events(&body);

    // Must contain an error event.
    let has_error = events.iter().any(|(ev, v)| {
        ev == "error"
            && v["message"]
                .as_str()
                .is_some_and(|m| m.contains("provider stream error"))
    });
    assert!(
        has_error,
        "SSE must emit error event with provider stream error: {events:?}"
    );
}

/// Unattributed thinking (OpenAI reasoning) is retained on Done as unsigned
/// fallback thinking, with no signature.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unattributed_thinking_retained_unsigned_on_done() {
    let (db, session_id) = setup().await;
    install(
        &session_id,
        vec![
            Ok(StreamEvent::Thinking("openai-reasoning".to_string())),
            Ok(StreamEvent::Delta(ContentBlock::text("response"))),
            Ok(StreamEvent::Done),
        ],
    );

    let (status, _) = post_chat(db.clone(), &session_id, "hello").await;
    assert_eq!(status, axum::http::StatusCode::OK);

    let content = assistant_content_blocks(&db, &session_id).await;

    // [Thinking(openai-reasoning, None), Text(response)]
    assert_eq!(content.len(), 2, "exact content: {content:?}");
    assert_thinking(&content[0], "openai-reasoning", None);
    assert!(matches!(&content[1], ContentBlock::Text { text } if text == "response"));
}
