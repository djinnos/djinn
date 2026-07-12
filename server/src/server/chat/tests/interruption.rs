//! End-to-end interruption regressions: HTTP handler, durable notices, and API projection.

use std::pin::Pin;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::header::CONTENT_TYPE;
use futures::Stream;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::events::EventBus;
use crate::server::chat::handler::register_test_provider;
use crate::test_helpers;
use djinn_db::{
    ChatInterruptionNoticeRepository, CreateChatInterruptionNotice, SessionMessageRepository,
    SessionRepository,
};
use djinn_provider::message::{ContentBlock, Conversation};
use djinn_provider::provider::{LlmProvider, StreamEvent, ToolChoice};
use djinn_provider::repos::CredentialRepository;

#[derive(Clone, Copy)]
enum Behavior {
    Complete,
    InitFails,
    Interrupt,
}

struct RecordingProvider {
    seen: Arc<Mutex<Vec<Conversation>>>,
    behavior: Behavior,
}
impl LlmProvider for RecordingProvider {
    fn name(&self) -> &str {
        "recording-chat-test-provider"
    }
    fn stream<'a>(
        &'a self,
        conversation: &'a Conversation,
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
        self.seen.lock().unwrap().push(conversation.clone());
        Box::pin(async move {
            if matches!(self.behavior, Behavior::InitFails) {
                return Err(anyhow::anyhow!("deliberate provider-start failure"));
            }
            let events = match self.behavior {
                Behavior::Complete => vec![
                    Ok(StreamEvent::Delta(ContentBlock::text("completed"))),
                    Ok(StreamEvent::Done),
                ],
                Behavior::Interrupt => vec![
                    Ok(StreamEvent::Delta(ContentBlock::text("saved partial text"))),
                    Ok(StreamEvent::Delta(ContentBlock::ToolUse {
                        id: "discarded-call".into(),
                        name: "memory_search".into(),
                        input: json!({"q":"discard me"}),
                    })),
                    Err(anyhow::anyhow!("deliberate stream interruption")),
                ],
                Behavior::InitFails => unreachable!(),
            };
            Ok(Box::pin(futures::stream::iter(events))
                as Pin<
                    Box<dyn Stream<Item = anyhow::Result<StreamEvent>> + Send>,
                >)
        })
    }
}

async fn post(app: axum::Router, session_id: &str, text: &str) -> (axum::http::StatusCode, String) {
    let req = axum::http::Request::builder().method("POST").uri("/api/chat/completions")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(json!({"model":"openai/gpt-4o-mini","session_id":session_id,"messages":[{"role":"user","content":text}]}).to_string())).unwrap();
    let response = app.oneshot(req).await.unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8(body.to_vec()).unwrap())
}

async fn get_messages(app: axum::Router, session_id: &str) -> Value {
    let req = axum::http::Request::builder()
        .method("GET")
        .uri(format!("/api/chat/sessions/{session_id}/messages"))
        .body(Body::empty())
        .unwrap();
    let response = app.oneshot(req).await.unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

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

fn install(session_id: &str, seen: Arc<Mutex<Vec<Conversation>>>, behavior: Behavior) {
    register_test_provider(session_id, move || {
        Box::new(RecordingProvider {
            seen: seen.clone(),
            behavior,
        })
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_completion_aggregates_consumes_and_does_not_reinject_notices() {
    let (db, session_id) = setup().await;
    let messages = SessionMessageRepository::new(db.clone(), EventBus::noop());
    messages
        .insert_message(
            &session_id,
            "",
            "assistant",
            &json!([{"type":"text","text":"older partial"}]).to_string(),
            None,
        )
        .await
        .unwrap();
    let notices = ChatInterruptionNoticeRepository::new(db.clone());
    notices
        .create(CreateChatInterruptionNotice {
            session_id: &session_id,
            session_message_id: None,
            discarded_tool_calls_count: 1,
        })
        .await
        .unwrap();
    notices
        .create(CreateChatInterruptionNotice {
            session_id: &session_id,
            session_message_id: None,
            discarded_tool_calls_count: 2,
        })
        .await
        .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    install(&session_id, seen.clone(), Behavior::Complete);
    let (status, _) = post(
        test_helpers::create_test_app_with_db(db.clone()),
        &session_id,
        "continue",
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(
        notices
            .list_unconsumed(&session_id)
            .await
            .unwrap()
            .is_empty(),
        "provider start consumes all included ids"
    );
    let first = seen.lock().unwrap()[0].clone();
    let reminders: Vec<_> = first
        .messages
        .iter()
        .enumerate()
        .filter(|(_, m)| {
            m.text_content()
                .contains("previous assistant turn was interrupted")
        })
        .collect();
    assert_eq!(reminders.len(), 1);
    assert_eq!(reminders[0].0, 1, "reminder follows normal system preamble");
    assert!(
        reminders[0]
            .1
            .text_content()
            .contains("3 pending tool call(s)")
    );
    assert_eq!(first.messages[2].text_content(), "older partial");
    post(
        test_helpers::create_test_app_with_db(db),
        &session_id,
        "later turn",
    )
    .await;
    assert_eq!(
        seen.lock()
            .unwrap()
            .iter()
            .filter(|c| c.messages.iter().any(|m| m
                .text_content()
                .contains("previous assistant turn was interrupted")))
            .count(),
        1
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_provider_start_keeps_notices_retryable_and_api_hides_interruption_artifacts() {
    let (db, session_id) = setup().await;
    let notices = ChatInterruptionNoticeRepository::new(db.clone());
    notices
        .create(CreateChatInterruptionNotice {
            session_id: &session_id,
            session_message_id: None,
            discarded_tool_calls_count: 2,
        })
        .await
        .unwrap();
    let failed_seen = Arc::new(Mutex::new(Vec::new()));
    install(&session_id, failed_seen, Behavior::InitFails);
    let (_, body) = post(
        test_helpers::create_test_app_with_db(db.clone()),
        &session_id,
        "retry me",
    )
    .await;
    assert!(body.contains("provider stream failed"));
    assert_eq!(
        notices.list_unconsumed(&session_id).await.unwrap().len(),
        1,
        "failed start must not consume notice"
    );
    let retry_seen = Arc::new(Mutex::new(Vec::new()));
    install(&session_id, retry_seen.clone(), Behavior::Complete);
    post(
        test_helpers::create_test_app_with_db(db.clone()),
        &session_id,
        "retry succeeds",
    )
    .await;
    assert!(
        notices
            .list_unconsumed(&session_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(retry_seen.lock().unwrap()[0].messages.iter().any(|m| {
        m.text_content()
            .contains("previous assistant turn was interrupted")
    }));

    let interrupted_id = uuid::Uuid::now_v7().to_string();
    SessionRepository::new(db.clone(), EventBus::noop())
        .upsert_chat_session(&interrupted_id, "openai/gpt-4o-mini")
        .await
        .unwrap();
    SessionRepository::new(db.clone(), EventBus::noop())
        .update_chat_title(&interrupted_id, "existing")
        .await
        .unwrap();
    let interrupted_seen = Arc::new(Mutex::new(Vec::new()));
    install(&interrupted_id, interrupted_seen, Behavior::Interrupt);
    post(
        test_helpers::create_test_app_with_db(db.clone()),
        &interrupted_id,
        "start interrupted turn",
    )
    .await;
    let api = get_messages(test_helpers::create_test_app_with_db(db), &interrupted_id).await;
    let encoded = api.to_string();
    assert!(encoded.contains("saved partial text"));
    assert!(!encoded.contains("previous assistant turn was interrupted"));
    assert!(!encoded.contains("memory_search"));
    assert!(!encoded.contains("discarded-call"));
    assert_eq!(
        api["messages"].as_array().unwrap().len(),
        2,
        "only user plus saved partial assistant row are visible"
    );
}
