//! Persistence-backed Anthropic replay fixtures using the real provider wire path.

use super::{ReplyLoopHarness, count_persisted_assistant_messages};
use crate::reply_loop::persistence::persist_session_message;
use axum::{
    Json, Router,
    extract::State,
    http::{HeaderValue, header},
    response::Response,
    routing::post,
};
use djinn_db::SessionMessageRepository;
use djinn_provider::{
    message::{ContentBlock, Message, Role},
    provider::format::anthropic::AnthropicProvider,
    provider::{AuthMethod, FormatFamily, ProviderCapabilities, ProviderConfig},
};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const FIRST_RESPONSE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":11}}}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"inspect the workspace\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig_replay_123\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"tool_replay_1\",\"name\":\"shell\"}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"command\\\":\\\"pwd\\\"}\"}}\n\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":7}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n"
);

const FINAL_RESPONSE: &str = concat!(
    "data: {\"type\":\"message_start\",\"message\":{\"usage\":{\"input_tokens\":17}}}\n\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
    "data: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":2}}\n\n",
    "data: {\"type\":\"message_stop\"}\n\n"
);

#[derive(Clone, Default)]
struct RecordedServer(Arc<Mutex<Vec<Value>>>);

impl RecordedServer {
    async fn spawn() -> (Self, String) {
        let state = Self::default();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind local recorded Anthropic server");
        let addr = listener.local_addr().expect("local server address");
        let app = Router::new()
            .route("/v1/messages", post(record_response))
            .with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("serve local Anthropic server");
        });
        (state, format!("http://{addr}"))
    }

    fn request(&self, index: usize) -> Value {
        self.0
            .lock()
            .expect("recorded request lock")
            .get(index)
            .cloned()
            .expect("recorded Anthropic request")
    }
}

async fn record_response(
    State(server): State<RecordedServer>,
    Json(request): Json<Value>,
) -> Response {
    let request_number = {
        let mut requests = server.0.lock().expect("recorded request lock");
        requests.push(request);
        requests.len()
    };
    let body = if request_number == 1 {
        FIRST_RESPONSE
    } else {
        FINAL_RESPONSE
    };
    Response::builder()
        .header(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/event-stream"),
        )
        .body(body.into())
        .expect("SSE response")
}

fn provider(base_url: String) -> AnthropicProvider {
    AnthropicProvider::new(ProviderConfig {
        base_url,
        auth: AuthMethod::NoAuth,
        format_family: FormatFamily::Anthropic,
        model_id: "claude-test-thinking".to_string(),
        context_window: 200_000,
        telemetry: None,
        session_affinity_key: None,
        provider_headers: Default::default(),
        capabilities: ProviderCapabilities {
            streaming: true,
            max_tokens_default: Some(1_024),
        },
        reasoning_effort: None,
        tool_schema_compat: None,
    })
}

fn assistant_content(request: &Value) -> &[Value] {
    request["messages"]
        .as_array()
        .expect("Anthropic messages")
        .iter()
        .find(|message| message["role"] == "assistant")
        .and_then(|message| message["content"].as_array())
        .expect("assistant replay content")
}

#[tokio::test]
async fn signed_thinking_tool_continuation_reloads_persisted_history_on_wire() {
    let (server, base_url) = RecordedServer::spawn().await;
    let provider = provider(base_url);
    let mut harness = ReplyLoopHarness::new().await;

    let result = harness.run(&provider, &[]).await;
    assert!(
        result.0.is_ok(),
        "reply loop should finish after the local final response"
    );
    assert_eq!(
        count_persisted_assistant_messages(&harness.slot_ctx, &harness.session_id).await,
        2
    );

    let repo = SessionMessageRepository::new(
        harness.slot_ctx.db.clone(),
        harness.slot_ctx.event_bus.clone(),
    );
    let loaded = repo
        .load_conversation(&harness.session_id)
        .await
        .expect("reload reply-loop history from Postgres");
    let stream = djinn_provider::provider::LlmProvider::stream(&provider, &loaded, &[], None)
        .await
        .expect("start replay from loaded history");
    futures::TryStreamExt::try_collect::<Vec<_>>(stream)
        .await
        .expect("consume local replay response");

    let continuation = server.request(2);
    let assistant = assistant_content(&continuation);
    assert_eq!(assistant.len(), 2, "thinking must remain before tool_use");
    assert_eq!(
        assistant[0],
        json!({
            "type": "thinking", "thinking": "inspect the workspace", "signature": "sig_replay_123"
        })
    );
    assert_eq!(
        assistant[1],
        json!({
            "type": "tool_use", "id": "tool_replay_1", "name": "shell", "input": {"command": "pwd"}
        })
    );
    assert!(
        !assistant
            .iter()
            .any(|block| block == &json!({"type": "text", "text": ""})),
        "thinking must never become an empty text substitute"
    );
    let tool_result = continuation["messages"]
        .as_array()
        .expect("Anthropic messages")
        .iter()
        .find(|message| message["role"] == "user" && message["content"][0]["type"] == "tool_result")
        .expect("matching user tool_result");
    assert_eq!(tool_result["content"][0]["tool_use_id"], "tool_replay_1");
    assert_eq!(tool_result["content"][0]["is_error"], false);
}

#[tokio::test]
async fn redacted_and_unknown_blocks_survive_repository_load_and_anthropic_wire_replay() {
    let (server, base_url) = RecordedServer::spawn().await;
    let provider = provider(base_url);
    let harness = ReplyLoopHarness::new().await;
    let mut extra = serde_json::Map::new();
    extra.insert("vendor_id".into(), json!("opaque-42"));
    extra.insert("nested".into(), json!({"keep": [true, 7]}));
    let message = Message {
        role: Role::Assistant,
        content: vec![
            ContentBlock::RedactedThinking {
                data: "redacted_blob".into(),
            },
            ContentBlock::Unknown {
                content_type: "vendor_trace".into(),
                extra,
            },
        ],
        metadata: None,
    };
    let repo = SessionMessageRepository::new(
        harness.slot_ctx.db.clone(),
        harness.slot_ctx.event_bus.clone(),
    );
    persist_session_message(&repo, &harness.session_id, &harness.task_id, &message).await;
    let mut loaded = repo
        .load_conversation(&harness.session_id)
        .await
        .expect("load persisted history");
    loaded.push(Message::user("continue"));

    let stream = djinn_provider::provider::LlmProvider::stream(&provider, &loaded, &[], None)
        .await
        .expect("start local Anthropic stream");
    futures::TryStreamExt::try_collect::<Vec<_>>(stream)
        .await
        .expect("consume local Anthropic response");

    let request = server.request(0);
    let assistant = assistant_content(&request);
    assert_eq!(
        assistant[0],
        json!({"type": "redacted_thinking", "data": "redacted_blob"})
    );
    assert_eq!(assistant[1]["type"], "vendor_trace");
    assert_eq!(assistant[1]["vendor_id"], "opaque-42");
    assert_eq!(assistant[1]["nested"], json!({"keep": [true, 7]}));
}
